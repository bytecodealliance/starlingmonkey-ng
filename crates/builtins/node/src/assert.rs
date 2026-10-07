// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:assert` and `node:assert/strict`. See <https://nodejs.org/api/assert.html>.
//!
//! The assertions follow Node.js's `lib/assert.js`, and `AssertionError` its
//! `lib/internal/assert/assertion_error.js`, without colors.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs, EmptyArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::{CallArgs, Value};
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{
    js_string, validate_function, validate_plain_object, ErrorKind, NodeError, Thrown,
};
use crate::inspect::{self, Compact, Getters, InspectOptions};
use crate::support::{try_collect, try_filled_vec};

/// `value` as `util.inspect` shows it with the current default options.
fn show(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    inspect::inspect(scope, value, InspectOptions::current(scope)?)
}

/// Node.js's `inspectValue`: `value` as an `AssertionError` message shows it.
fn inspect_value(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    let mut options = InspectOptions::current(scope)?;
    options.compact = Compact::Bool(false);
    options.custom_inspect = false;
    options.depth = Some(1000.0);
    options.max_array_length = f64::INFINITY;
    options.show_hidden = false;
    options.show_proxy = false;
    options.sorted = inspect::Sorted::Yes;
    options.getters = Getters::All;
    inspect::inspect(scope, value, options)
}

/// Node.js's `copyError`: a plain object with the prototype and own enumerable properties of the
/// error `source`, its `message` as a read-only own property, and its own `cause`, copied in turn
/// if it is an error. The copy has no own `stack`.
fn copy_error<'s>(scope: &'s Scope<'_>, source: Object<'_>) -> Result<Object<'s>, ExnThrown> {
    let _guard = crate::errors::check_stack(scope)?;
    let target = match source.get_prototype(scope)? {
        Some(proto) => Object::new_plain_with_proto(scope, proto.handle())?,
        None => Object::new_plain(scope)?,
    };
    target.assign(scope, source)?;
    let readonly = (js::class_spec::JSPROP_READONLY | js::class_spec::JSPROP_PERMANENT) as u32;
    let message = source.get_property(scope, c"message")?;
    if target.has_own_property(scope, c"message")? {
        target.set_property(scope, c"message", message)?;
    } else {
        target.define_property(scope, c"message", message, readonly)?;
    }
    if source.has_own_property(scope, c"cause")? {
        let mut cause = source.get_property(scope, c"cause")?;
        if let Ok(object) = Object::from_value(scope, *cause) {
            if inspect::is_error(scope, object)? {
                cause = copy_error(scope, object)?.to_jsval_throwing(scope)?;
            }
        }
        if target.has_own_property(scope, c"cause")? {
            target.set_property(scope, c"cause", cause)?;
        } else {
            target.define_property(scope, c"cause", cause, readonly)?;
        }
    }
    Ok(target)
}

/// `value` as an object if it has a `stack` property and is an `instanceof Error`, the condition
/// under which an `AssertionError` without a message shows a [`copy_error`] of it.
fn error_to_copy<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    match Object::from_value(scope, *value) {
        Ok(object)
            if !object.is_callable()
                && object.has_property(scope, c"stack")?
                && inspect::is_error_instance(scope, object)? =>
        {
            Ok(Some(object))
        }
        _ => Ok(None),
    }
}

/// The `typeof` of `value`.
fn type_of(scope: &Scope<'_>, value: HandleValue<'_>) -> &'static str {
    match Object::from_value(scope, *value) {
        Ok(object) if object.is_callable() => "function",
        Ok(_) => "object",
        Err(_) if value.is_null() => "object",
        Err(_) if value.is_undefined() => "undefined",
        Err(_) if value.is_string() => "string",
        Err(_) if value.is_number() => "number",
        Err(_) if value.is_boolean() => "boolean",
        Err(_) if value.is_bigint() => "bigint",
        Err(_) => "symbol",
    }
}

/// Whether `value` is a `RegExp`.
fn is_regexp(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    match Object::from_value(scope, *value) {
        Ok(object) => inspect::is_regexp(scope, object),
        Err(_) => Ok(false),
    }
}

/// Whether the realm's `Error` constructor is in the prototype chain of `ctor`.
fn derives_from_error(scope: &Scope<'_>, ctor: Object<'_>) -> Result<bool, ExnThrown> {
    let error_ctor = crate::stack_trace::error_constructor(scope)?;
    let mut current = ctor.get_prototype(scope)?;
    while let Some(candidate) = current {
        if candidate.as_raw() == error_ctor.as_raw() {
            return Ok(true);
        }
        current = candidate.get_prototype(scope)?;
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// AssertionError messages
// ---------------------------------------------------------------------------

/// Node.js's `kReadableOperator`.
fn readable_operator(operator: &str) -> Option<&'static str> {
    Some(match operator {
        "deepStrictEqual" => "Expected values to be strictly deep-equal:",
        "partialDeepStrictEqual" => "Expected values to be partially and strictly deep-equal:",
        "strictEqual" => "Expected values to be strictly equal:",
        "strictEqualObject" => "Expected \"actual\" to be reference-equal to \"expected\":",
        "deepEqual" => "Expected values to be loosely deep-equal:",
        "notDeepStrictEqual" => "Expected \"actual\" not to be strictly deep-equal to:",
        "notStrictEqual" => "Expected \"actual\" to be strictly unequal to:",
        "notStrictEqualObject" => "Expected \"actual\" not to be reference-equal to \"expected\":",
        "notDeepEqual" => "Expected \"actual\" not to be loosely deep-equal to:",
        "notIdentical" => "Values have same structure but are not reference-equal:",
        "notDeepEqualUnequal" => "Expected values not to be loosely deep-equal:",
        _ => return None,
    })
}

/// A line of a diff: an inserted line of `actual`, a deleted line of `expected`, or a line both
/// share.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffOp {
    Delete,
    Nop,
    Insert,
}

impl DiffOp {
    /// The number Node.js's `myersDiff` represents the operation with: `-1` for `Delete`, `0` for
    /// `Nop` and `1` for `Insert`.
    pub(crate) fn sign(self) -> i32 {
        match self {
            DiffOp::Delete => -1,
            DiffOp::Nop => 0,
            DiffOp::Insert => 1,
        }
    }
}

/// Node.js's `myersDiff` of the lines `actual` and `expected`, in reverse order, or `None` if
/// allocating its state fails. The state takes `2 * (actual.len() + expected.len()) + 1` words
/// for each inserted or deleted line.
pub(crate) fn myers_diff(
    actual: &[String],
    expected: &[String],
    comma: bool,
) -> Option<Vec<(DiffOp, String)>> {
    let equal =
        |a: &str, b: &str| a == b || (comma && (format!("{a},") == b || a == format!("{b},")));
    let max = actual.len() + expected.len();
    let mut v = try_filled_vec::<isize>(max.checked_mul(2)?.checked_add(1)?)?;
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for level in 0..=max as isize {
        trace.try_reserve(1).ok()?;
        trace.push(try_collect(v.iter().copied())?);
        let mut diagonal = -level;
        while diagonal <= level {
            let offset = (diagonal + max as isize) as usize;
            let previous = if offset > 0 { v[offset - 1] } else { 0 };
            let next = v.get(offset + 1).copied().unwrap_or(0);
            let mut x = if diagonal == -level || (diagonal != level && previous < next) {
                next
            } else {
                previous + 1
            };
            let mut y = x - diagonal;
            while (x as usize) < actual.len()
                && (y as usize) < expected.len()
                && y >= 0
                && equal(&actual[x as usize], &expected[y as usize])
            {
                x += 1;
                y += 1;
            }
            v[offset] = x;
            if x as usize >= actual.len() && y >= 0 && y as usize >= expected.len() {
                return Some(backtrack(&trace, actual, expected, comma));
            }
            diagonal += 2;
        }
    }
    Some(Vec::new())
}

/// Node.js's `backtrack` for [`myers_diff`].
fn backtrack(
    trace: &[Vec<isize>],
    actual: &[String],
    expected: &[String],
    comma: bool,
) -> Vec<(DiffOp, String)> {
    let max = (actual.len() + expected.len()) as isize;
    let mut x = actual.len() as isize;
    let mut y = expected.len() as isize;
    let mut result = Vec::new();
    for level in (0..trace.len() as isize).rev() {
        let v = &trace[level as usize];
        let diagonal = x - y;
        let offset = (diagonal + max) as usize;
        let previous = if offset > 0 { v[offset - 1] } else { 0 };
        let next = v.get(offset + 1).copied().unwrap_or(0);
        let previous_diagonal = if diagonal == -level || (diagonal != level && previous < next) {
            diagonal + 1
        } else {
            diagonal - 1
        };
        let previous_x = v[(previous_diagonal + max) as usize];
        let previous_y = previous_x - previous_diagonal;
        while x > previous_x && y > previous_y {
            let item = &actual[x as usize - 1];
            let value = if comma && !item.ends_with(',') {
                expected[y as usize - 1].clone()
            } else {
                item.clone()
            };
            result.push((DiffOp::Nop, value));
            x -= 1;
            y -= 1;
        }
        if level > 0 {
            if x > previous_x {
                x -= 1;
                result.push((DiffOp::Insert, actual[x as usize].clone()));
            } else {
                y -= 1;
                result.push((DiffOp::Delete, expected[y as usize].clone()));
            }
        }
    }
    result
}

/// Node.js's `printMyersDiff` without colors: the message, and whether lines were skipped.
fn print_myers_diff(diff: &[(DiffOp, String)], operator: &str) -> (String, bool) {
    const COLLAPSE: usize = 5;
    let mut message = String::new();
    let mut skipped = false;
    let mut nop_count = 0;
    for index in (0..diff.len()).rev() {
        let (operation, value) = &diff[index];
        let previous = (index + 1 < diff.len()).then(|| diff[index + 1].0);
        if previous == Some(DiffOp::Nop) && *operation != DiffOp::Nop {
            if nop_count == COLLAPSE + 1 {
                message.push_str(&format!("  {}\n", diff[index + 1].1));
            } else if nop_count == COLLAPSE + 2 {
                message.push_str(&format!("  {}\n", diff[index + 2].1));
                message.push_str(&format!("  {}\n", diff[index + 1].1));
            } else if nop_count >= COLLAPSE + 3 {
                message.push_str("...\n");
                message.push_str(&format!("  {}\n", diff[index + 1].1));
                skipped = true;
            }
            nop_count = 0;
        }
        match operation {
            DiffOp::Insert if operator == "partialDeepStrictEqual" => {
                message.push_str(&format!("+ {value}\n"));
            }
            DiffOp::Insert => message.push_str(&format!("+ {value}\n")),
            DiffOp::Delete => message.push_str(&format!("- {value}\n")),
            DiffOp::Nop => {
                if nop_count < COLLAPSE {
                    message.push_str(&format!("  {value}\n"));
                }
                nop_count += 1;
            }
        }
    }
    (format!("\n{}", message.trim_end()), skipped)
}

/// Node.js's `createErrDiff` without colors.
fn create_err_diff(
    scope: &Scope<'_>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    operator: &str,
    custom: Option<&str>,
) -> Result<String, ExnThrown> {
    let mut operator = operator.to_string();
    let objects =
        inspect::is_typeof_object(scope, actual) && inspect::is_typeof_object(scope, expected);
    let functions = type_of(scope, actual) == "function" && type_of(scope, expected) == "function";
    if operator == "strictEqual" && (objects || functions) {
        operator = "strictEqualObject".into();
    }
    let inspected_actual = inspect_value(scope, actual)?;
    let inspected_expected = inspect_value(scope, expected)?;
    let split_actual: Vec<String> = inspected_actual.split('\n').map(String::from).collect();
    let split_expected: Vec<String> = inspected_expected.split('\n').map(String::from).collect();
    let mut header = "+ actual - expected".to_string();
    let mut skipped = false;
    let simple = split_actual.len() <= 1
        && split_expected.len() <= 1
        && (!inspect::is_typeof_object(scope, actual)
            || !inspect::is_typeof_object(scope, expected));
    let message = if simple {
        let (a, e) = (&split_actual[0], &split_expected[0]);
        let mut length = inspect::js_len(a) + inspect::js_len(e);
        if actual.is_string() {
            length -= 2;
        }
        if expected.is_string() {
            length -= 2;
        }
        let zero = |value: HandleValue<'_>| value.is_number() && value.to_number() == 0.0;
        if length <= 12 && (!zero(actual) || !zero(expected)) {
            header = String::new();
            format!("{a} !== {e}")
        } else {
            let mut message = format!("\n+ {a}\n- {e}");
            // As in Node.js, the indicator is shown for values of every type.
            if inspect::js_len(a) + inspect::js_len(e) <= 80 {
                let a_units: Vec<u16> = a.encode_utf16().collect();
                let e_units: Vec<u16> = e.encode_utf16().collect();
                if let Some(index) =
                    (0..a_units.len()).find(|&i| e_units.get(i) != Some(&a_units[i]))
                {
                    if index >= 3 {
                        message.push_str(&format!("\n{}^", " ".repeat(index + 2)));
                    }
                }
            }
            message
        }
    } else if inspected_actual == inspected_expected {
        operator = "notIdentical".into();
        header = String::new();
        if split_actual.len() > 50 {
            skipped = true;
            format!("{}\n...}}", split_actual[..50].join("\n"))
        } else {
            split_actual.join("\n")
        }
    } else {
        let comma = inspect::is_typeof_object(scope, actual);
        let diff = myers_diff(&split_actual, &split_expected, comma).ok_or_else(|| {
            js::exception::report_out_of_memory(scope);
            ExnThrown
        })?;
        let (message, diff_skipped) = print_myers_diff(&diff, &operator);
        if operator == "partialDeepStrictEqual" {
            header = "+ actual - expected".into();
        }
        skipped = diff_skipped;
        message
    };
    let title = custom
        .map(String::from)
        .or_else(|| readable_operator(&operator).map(String::from))
        .unwrap_or_default();
    let skipped = if skipped { "\n... Skipped lines" } else { "" };
    Ok(format!("{title}\n{header}{skipped}\n{message}\n"))
}

/// The message `new AssertionError(options)` gives an error without `options.message`.
fn generate_message(
    scope: &Scope<'_>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    operator: &str,
) -> Result<String, ExnThrown> {
    if matches!(
        operator,
        "deepStrictEqual" | "strictEqual" | "partialDeepStrictEqual"
    ) {
        return create_err_diff(scope, actual, expected, operator, None);
    }
    if operator == "notDeepStrictEqual" || operator == "notStrictEqual" {
        let mut base = readable_operator(operator).unwrap_or_default();
        let mut lines: Vec<String> = inspect_value(scope, actual)?
            .split('\n')
            .map(String::from)
            .collect();
        if operator == "notStrictEqual" && Object::from_value(scope, *actual).is_ok() {
            base = readable_operator("notStrictEqualObject").unwrap_or_default();
        }
        if lines.len() > 50 {
            lines[46] = "...".into();
            lines.truncate(47);
        }
        return Ok(if lines.len() == 1 {
            let separator = if inspect::js_len(&lines[0]) > 5 {
                "\n\n"
            } else {
                " "
            };
            format!("{base}{separator}{}", lines[0])
        } else {
            format!("{base}\n\n{}\n", lines.join("\n"))
        });
    }
    let mut result = inspect_value(scope, actual)?;
    let mut other = inspect_value(scope, expected)?;
    let known = readable_operator(operator);
    if operator == "notDeepEqual" && result == other {
        result = format!("{}\n\n{result}", known.unwrap_or_default());
        if inspect::js_len(&result) > 1024 {
            result = truncate_units(&result, 1021) + "...";
        }
        return Ok(result);
    }
    if inspect::js_len(&result) > 512 {
        result = truncate_units(&result, 509) + "...";
    }
    if inspect::js_len(&other) > 512 {
        other = truncate_units(&other, 509) + "...";
    }
    if operator == "deepEqual" {
        result = format!(
            "{}\n\n{result}\n\nshould loosely deep-equal\n\n",
            known.unwrap_or_default()
        );
    } else if let Some(unequal) = readable_operator(&format!("{operator}Unequal")) {
        result = format!("{unequal}\n\n{result}\n\nshould not loosely deep-equal\n\n");
    } else {
        other = format!(" {operator} {other}");
    }
    Ok(format!("{result}{other}"))
}

/// The first `count` UTF-16 code units of `text`.
fn truncate_units(text: &str, count: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().take(count).collect();
    String::from_utf16_lossy(&units)
}

/// Throw `message` if it is an `Error`, as Node.js's `innerFail` does, and a new `AssertionError`
/// otherwise. `generated` overrides the error's `generatedMessage`.
#[allow(clippy::too_many_arguments)]
fn fail_with(
    scope: &Scope<'_>,
    message: Option<HandleValue<'_>>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    operator: &str,
    generated: Option<bool>,
) -> Thrown {
    match js::JSString::from_str(scope, operator) {
        Ok(operator) => {
            let operator = scope.root_value(operator.as_value());
            fail_with_operator(scope, message, actual, expected, operator, generated)
        }
        Err(ExnThrown) => Thrown::Pending,
    }
}

/// [`fail_with`] with an `operator` of any value.
fn fail_with_operator(
    scope: &Scope<'_>,
    message: Option<HandleValue<'_>>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    operator: HandleValue<'_>,
    generated: Option<bool>,
) -> Thrown {
    let message = message.filter(|message| !message.is_undefined());
    if let Some(error) = message.filter(|message| js::exception::is_error_object(scope, *message)) {
        js::exception::throw_value(scope, error);
        return Thrown::Pending;
    }
    let build = || -> Result<HandleValue<'_>, ExnThrown> {
        let message = message.unwrap_or(HandleValue::undefined());
        let error = new_assertion_error(scope, message, actual, expected, operator, None)?;
        if let Some(generated) = generated {
            error.set_property(scope, c"generatedMessage", generated)?;
        }
        error.to_jsval_throwing(scope)
    };
    match build() {
        Ok(error) => {
            js::exception::set_pending(scope, error, js::native::ExceptionStackBehavior::Capture);
            Thrown::Pending
        }
        Err(ExnThrown) => Thrown::Pending,
    }
}

/// A string message, with `generatedMessage` set to `true`.
fn generated<'s>(scope: &'s Scope<'_>, text: &str) -> Result<HandleValue<'s>, ExnThrown> {
    Ok(scope.root_value(js::JSString::from_str(scope, text)?.as_value()))
}

/// `assert.ok(value, message)`. A missing `value` fails too.
fn check_ok(
    scope: &Scope<'_>,
    value: Option<HandleValue<'_>>,
    message: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let text = match value {
        Some(value) if bool::from_jsval(scope, value, ()).unwrap_or(false) => return Ok(()),
        Some(_) => "The expression evaluated to a falsy value",
        None => "No value argument passed to `assert.ok()`",
    };
    let value = value.unwrap_or(HandleValue::undefined());
    let truthy = scope.root_value(js::value::from_bool(true));
    let given = message.filter(|message| !message.is_null() && !message.is_undefined());
    let (message, is_generated) = match given {
        Some(message) => (message, false),
        None => (generated(scope, text)?, true),
    };
    Err(fail_with(
        scope,
        Some(message),
        value,
        truthy,
        "==",
        Some(is_generated),
    ))
}

/// The shared body of the equality assertions: `strict` selects `Object.is` over `==` and
/// `negated` selects the unequal variants.
fn check_equal(
    scope: &Scope<'_>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    message: Option<HandleValue<'_>>,
    strict: bool,
    negated: bool,
) -> Result<(), Thrown> {
    let equal = if strict {
        js::comparison::same_value(scope, actual, expected)?
    } else {
        js::comparison::loosely_equal(scope, actual, expected)?
            || (actual.is_number()
                && expected.is_number()
                && actual.to_number().is_nan()
                && expected.to_number().is_nan())
    };
    if equal != negated {
        return Ok(());
    }
    let operator = match (strict, negated) {
        (true, false) => "strictEqual",
        (true, true) => "notStrictEqual",
        (false, false) => "==",
        (false, true) => "!=",
    };
    Err(fail_with(scope, message, actual, expected, operator, None))
}

/// The shared body of the deep equality assertions.
fn check_deep_equal(
    scope: &Scope<'_>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    message: Option<HandleValue<'_>>,
    strict: bool,
    negated: bool,
) -> Result<(), Thrown> {
    let mode = if strict {
        inspect::Mode::Strict
    } else {
        inspect::Mode::Loose
    };
    if inspect::deep_equal(scope, actual, expected, mode)? != negated {
        return Ok(());
    }
    let operator = match (strict, negated) {
        (true, false) => "deepStrictEqual",
        (true, true) => "notDeepStrictEqual",
        (false, false) => "deepEqual",
        (false, true) => "notDeepEqual",
    };
    Err(fail_with(scope, message, actual, expected, operator, None))
}

/// An object holding the properties `keys` of `object` that it has, as Node.js's `Comparison`
/// class builds them for a message: a `RegExp` property that matches the string property of
/// `actual` is replaced by that string.
fn comparison<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    keys: &[HandleValue<'_>],
    actual: Option<Object<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let result = js::class::create_instance_with::<ComparisonImpl>(scope, |_| ComparisonImpl {})?;
    for key in keys {
        let id = scope.root_id(js::id::value_to_id(scope, *key)?);
        if !object.has_property_by_id(scope, id)? {
            continue;
        }
        let value = object.get_property_by_id(scope, id)?;
        let mut shown = value;
        if let Some(actual) = actual {
            let have = actual.get_property_by_id(scope, id)?;
            if have.is_string()
                && is_regexp(scope, value)?
                && inspect::regexp_exec_matches(scope, value, have)?
            {
                shown = have;
            }
        }
        result.set_property_by_id(scope, id, shown)?;
    }
    Ok(result.as_object())
}

/// The class of the objects [`comparison`] creates, which shows their class name in messages.
#[core_runtime::jsclass(name = "Comparison", hidden)]
pub(crate) struct Comparison {}

#[core_runtime::jsmethods]
impl Comparison {}

/// Node.js's `compareExceptionKey`.
#[allow(clippy::too_many_arguments)]
fn compare_exception_key(
    scope: &Scope<'_>,
    actual: Object<'_>,
    expected: Object<'_>,
    key: HandleValue<'_>,
    message: Option<HandleValue<'_>>,
    keys: &[HandleValue<'_>],
    operator: &str,
) -> Result<(), Thrown> {
    let id = scope.root_id(js::id::value_to_id(scope, key)?);
    let present = actual.has_property_by_id(scope, id)?;
    let equal = present
        && inspect::deep_equal(
            scope,
            actual.get_property_by_id(scope, id)?,
            expected.get_property_by_id(scope, id)?,
            inspect::Mode::Strict,
        )?;
    if equal {
        return Ok(());
    }
    let actual_value = scope.root_value(actual.to_jsval_raw_throwing(scope)?);
    let expected_value = scope.root_value(expected.to_jsval_raw_throwing(scope)?);
    if bool::from_jsval(scope, message.unwrap_or(HandleValue::undefined()), ()).unwrap_or(false) {
        return Err(fail_with(
            scope,
            message,
            actual_value,
            expected_value,
            operator,
            None,
        ));
    }
    let a = comparison(scope, actual, keys, None)?;
    let b = comparison(scope, expected, keys, Some(actual))?;
    let text = create_err_diff(
        scope,
        scope.root_value(a.to_jsval_raw_throwing(scope)?),
        scope.root_value(b.to_jsval_raw_throwing(scope)?),
        "deepStrictEqual",
        None,
    )?;
    let text = generated(scope, &text)?;
    Err(fail_with(
        scope,
        Some(text),
        actual_value,
        expected_value,
        operator,
        Some(true),
    ))
}

/// Node.js's `expectedException`: check the thrown `actual` against `expected`, a `RegExp`
/// tested against `String(actual)`, an error class, a validation function, or an object whose
/// properties `actual` must have.
fn expected_exception(
    scope: &Scope<'_>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    message: Option<HandleValue<'_>>,
    operator: &str,
) -> Result<(), Thrown> {
    let has_message =
        bool::from_jsval(scope, message.unwrap_or(HandleValue::undefined()), ()).unwrap_or(false);
    let expected_object = Object::from_value(scope, *expected).map_err(|e| e.throw(scope))?;
    let generated_text: String;
    if !expected_object.is_callable() {
        if is_regexp(scope, expected)? {
            let text = js_string(scope, actual)?;
            let text_value = generated(scope, &text)?;
            if inspect::regexp_exec_matches(scope, expected, text_value)? {
                return Ok(());
            }
            generated_text = format!(
                "The input did not match the regular expression {}. Input:\n\n{}\n",
                show(scope, expected)?,
                show(scope, text_value)?
            );
        } else if !inspect::is_typeof_object(scope, actual) {
            let text = if has_message {
                None
            } else {
                Some(generate_message(
                    scope,
                    actual,
                    expected,
                    "deepStrictEqual",
                )?)
            };
            let message = match text {
                Some(text) => Some(generated(scope, &text)?),
                None => message,
            };
            return Err(fail_with(
                scope,
                message,
                actual,
                expected,
                operator,
                Some(!has_message),
            ));
        } else {
            let actual_object = Object::from_value(scope, *actual).map_err(|e| e.throw(scope))?;
            let mut keys = Vec::new();
            for key in expected_object.own_keys(scope)? {
                if key.is_symbol() {
                    continue;
                }
                let id = scope.root_id(js::id::value_to_id(scope, key)?);
                if expected_object
                    .own_property_descriptor(scope, id)?
                    .is_some_and(|property| property.enumerable)
                {
                    keys.push(key);
                }
            }
            if crate::stack_trace::error_constructor(scope)?.instance_of(scope, expected)? {
                keys.push(generated(scope, "name")?);
                keys.push(generated(scope, "message")?);
            } else if keys.is_empty() {
                return Err(NodeError::invalid_arg_value(
                    scope,
                    "error",
                    expected,
                    "may not be an empty object",
                )
                .into());
            }
            for key in &keys {
                let id = scope.root_id(js::id::value_to_id(scope, *key)?);
                let have = actual_object.get_property_by_id(scope, id)?;
                let want = expected_object.get_property_by_id(scope, id)?;
                if have.is_string()
                    && is_regexp(scope, want)?
                    && inspect::regexp_exec_matches(scope, want, have)?
                {
                    continue;
                }
                compare_exception_key(
                    scope,
                    actual_object,
                    expected_object,
                    *key,
                    message,
                    &keys,
                    operator,
                )?;
            }
            return Ok(());
        }
    } else {
        let prototype = expected_object.get_property(scope, c"prototype")?;
        if !prototype.is_undefined() && expected_object.instance_of(scope, actual)? {
            return Ok(());
        }
        let derived_from_error = derives_from_error(scope, expected_object)?;
        let expected_name = js_string(scope, expected_object.get_property(scope, c"name")?)?;
        if derived_from_error {
            let mut text = format!(
                "The error is expected to be an instance of \"{expected_name}\". Received "
            );
            if js::exception::is_error_object(scope, actual) {
                let actual_object =
                    Object::from_value(scope, *actual).map_err(|e| e.throw(scope))?;
                let ctor = actual_object.get_property(scope, c"constructor")?;
                let ctor_name = match Object::from_value(scope, *ctor) {
                    Ok(ctor) => ctor.get_property(scope, c"name")?,
                    Err(_) => HandleValue::undefined(),
                };
                let name = if bool::from_jsval(scope, ctor_name, ()).unwrap_or(false) {
                    ctor_name
                } else {
                    actual_object.get_property(scope, c"name")?
                };
                let name = js_string(scope, name)?;
                if expected_name == name {
                    text.push_str("an error with identical name but a different prototype.");
                } else {
                    text.push_str(&format!("\"{name}\""));
                }
                let error_message = actual_object.get_property(scope, c"message")?;
                if bool::from_jsval(scope, error_message, ()).unwrap_or(false) {
                    text.push_str(&format!(
                        "\n\nError message:\n\n{}",
                        js_string(scope, error_message)?
                    ));
                }
            } else {
                let mut options = InspectOptions::current(scope)?;
                options.depth = Some(-1.0);
                text.push_str(&format!(
                    "\"{}\"",
                    inspect::inspect(scope, actual, options)?
                ));
            }
            generated_text = text;
        } else {
            let this = Object::new_plain(scope)?;
            let result = Function::call(scope, this, expected, &[actual])?;
            if result.is_boolean() && result.to_boolean() {
                return Ok(());
            }
            let name = if expected_name.is_empty() {
                String::new()
            } else {
                format!("\"{expected_name}\" ")
            };
            let mut text = format!(
                "The {name}validation function is expected to return \"true\". Received {}",
                show(scope, result)?
            );
            if js::exception::is_error_object(scope, actual) {
                text.push_str(&format!(
                    "\n\nCaught error:\n\n{}",
                    js_string(scope, actual)?
                ));
            }
            generated_text = text;
        }
    }
    let message = if has_message {
        message
    } else {
        Some(generated(scope, &generated_text)?)
    };
    Err(fail_with(
        scope,
        message,
        actual,
        expected,
        operator,
        Some(!has_message),
    ))
}

/// Node.js's `expectsError`: `actual` is the thrown error or rejection reason, or `None` if there
/// was none. `message_given` is whether the caller passed a message argument.
fn expects_error(
    scope: &Scope<'_>,
    actual: Option<HandleValue<'_>>,
    error: Option<HandleValue<'_>>,
    message: Option<HandleValue<'_>>,
    operator: &str,
) -> Result<(), Thrown> {
    let mut error = error.filter(|error| !error.is_undefined());
    let mut message = message;
    let invalid_type = |value: HandleValue<'_>| -> Thrown {
        NodeError::invalid_arg_type(
            scope,
            "error",
            "of type function or an instance of Error, RegExp, or Object",
            value,
        )
        .into()
    };
    if let Some(text) = error.filter(|error| error.is_string()) {
        if message.is_some() {
            return Err(invalid_type(text));
        }
        let text_string = js_string(scope, text)?;
        match actual {
            Some(actual) if inspect::is_typeof_object(scope, actual) => {
                let actual = Object::from_value(scope, *actual).map_err(|e| e.throw(scope))?;
                let actual_message = actual.get_property(scope, c"message")?;
                if actual_message.is_string() && js_string(scope, actual_message)? == text_string {
                    return Err(ambiguous(&format!(
                        "The error message \"{text_string}\" is identical to the message."
                    )));
                }
            }
            Some(actual) if js::comparison::strictly_equal(scope, actual, text)? => {
                return Err(ambiguous(&format!(
                    "The error \"{text_string}\" is identical to the message."
                )));
            }
            _ => {}
        }
        message = Some(text);
        error = None;
    } else if let Some(value) = error.filter(|error| !error.is_null() && !error.is_object()) {
        return Err(invalid_type(value));
    }
    let Some(actual) = actual else {
        let mut details = String::new();
        if let Some(error) = error.and_then(|error| Object::from_value(scope, *error).ok()) {
            let name = error.get_property(scope, c"name")?;
            if bool::from_jsval(scope, name, ()).unwrap_or(false) {
                details.push_str(&format!(" ({})", js_string(scope, name)?));
            }
        }
        let message_value = message.unwrap_or(HandleValue::undefined());
        if bool::from_jsval(scope, message_value, ()).unwrap_or(false) {
            details.push_str(&format!(": {}", js_string(scope, message_value)?));
        } else {
            details.push('.');
        }
        let noun = if operator == "rejects" {
            "rejection"
        } else {
            "exception"
        };
        let text = generated(scope, &format!("Missing expected {noun}{details}"))?;
        return Err(fail_with(
            scope,
            Some(text),
            HandleValue::undefined(),
            error.unwrap_or(HandleValue::undefined()),
            operator,
            None,
        ));
    };
    match error.filter(|error| !error.is_null()) {
        Some(error) => expected_exception(scope, actual, error, message, operator),
        None => Ok(()),
    }
}

/// `ERR_AMBIGUOUS_ARGUMENT` for the `error/message` argument of `throws` and `rejects`.
fn ambiguous(reason: &str) -> Thrown {
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_AMBIGUOUS_ARGUMENT",
        format!("The \"error/message\" argument is ambiguous. {reason}"),
    )
    .into()
}

/// Node.js's `hasMatchingError`.
fn has_matching_error(
    scope: &Scope<'_>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
) -> Result<bool, Thrown> {
    let expected_object = Object::from_value(scope, *expected)
        .ok()
        .filter(|expected| expected.is_callable());
    let Some(expected_object) = expected_object else {
        if is_regexp(scope, expected)? {
            let text = generated(scope, &js_string(scope, actual)?)?;
            return Ok(inspect::regexp_exec_matches(scope, expected, text)?);
        }
        return Err(NodeError::invalid_arg_type(
            scope,
            "expected",
            "of type function or an instance of RegExp",
            expected,
        )
        .into());
    };
    let prototype = expected_object.get_property(scope, c"prototype")?;
    if !prototype.is_undefined() && expected_object.instance_of(scope, actual)? {
        return Ok(true);
    }
    if derives_from_error(scope, expected_object)? {
        return Ok(false);
    }
    let this = Object::new_plain(scope)?;
    let result = Function::call(scope, this, expected, &[actual])?;
    Ok(result.is_boolean() && result.to_boolean())
}

/// Node.js's `expectsNoError`: fail if `actual` matches `error`, or there is no `error`, and
/// rethrow `actual` otherwise.
fn expects_no_error(
    scope: &Scope<'_>,
    actual: Option<HandleValue<'_>>,
    error: Option<HandleValue<'_>>,
    message: Option<HandleValue<'_>>,
    operator: &str,
) -> Result<(), Thrown> {
    let Some(actual) = actual else {
        return Ok(());
    };
    let (error, message) = match error {
        Some(error) if error.is_string() => (None, Some(error)),
        error => (error.filter(|error| !error.is_undefined()), message),
    };
    let matches = match error.filter(|error| bool::from_jsval(scope, *error, ()).unwrap_or(false)) {
        None => true,
        Some(error) => has_matching_error(scope, actual, error)?,
    };
    if matches {
        let message_value = message.unwrap_or(HandleValue::undefined());
        let details = if bool::from_jsval(scope, message_value, ()).unwrap_or(false) {
            format!(": {}", js_string(scope, message_value)?)
        } else {
            ".".into()
        };
        let noun = if operator == "doesNotReject" {
            "rejection"
        } else {
            "exception"
        };
        let actual_message = match Object::from_value(scope, *actual) {
            Ok(object) => object.get_property(scope, c"message")?,
            Err(_) => HandleValue::undefined(),
        };
        let text = format!(
            "Got unwanted {noun}{details}\nActual message: \"{}\"",
            js_string(scope, actual_message)?
        );
        let text = generated(scope, &text)?;
        return Err(fail_with(
            scope,
            Some(text),
            actual,
            error.unwrap_or(HandleValue::undefined()),
            operator,
            None,
        ));
    }
    js::exception::throw_value(scope, actual);
    Err(Thrown::Pending)
}

/// The state of a `rejects` or `doesNotReject` call that awaits its target.
#[core_runtime::jsclass(hidden)]
pub(crate) struct RejectsState {
    /// The promise the call returned.
    result: Option<Heap<js::promise::Promise>>,
    expected: Heap<Value>,
    message: Option<Heap<Value>>,
    /// Whether the call is `rejects`, not `doesNotReject`.
    #[no_trace]
    rejects: bool,
}

#[core_runtime::jsmethods]
impl RejectsState {}

/// The body of `rejects`' and `doesNotReject`'s reaction to the awaited promise, whose payload is
/// a [`RejectsState`] and whose first argument is the outcome.
fn settle_rejects(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
    rejected: bool,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<RejectsState>(scope, payload);
    let (result, expected, message, rejects) = {
        let data = state.data();
        let result = data.result.as_ref().map(|result| result.get(scope));
        let expected = data.expected.get(scope);
        let message = data.message.as_ref().map(|message| message.get(scope));
        (result, expected, message, data.rejects)
    };
    let result = result.ok_or(ExnThrown)?;
    let outcome = args.get(0);
    let expected = (!expected.is_undefined()).then_some(expected);
    let actual = rejected.then_some(outcome);
    let checked = if rejects {
        expects_error(scope, actual, expected, message, "rejects")
    } else {
        expects_no_error(scope, actual, expected, message, "doesNotReject")
    };
    match checked {
        Ok(()) => result.resolve(scope, HandleValue::undefined())?,
        Err(error) => {
            error.throw(scope);
            result.reject_with_pending(scope)?;
        }
    }
    Ok(js::value::undefined())
}

fn rejects_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    settle_rejects(scope, args, payload, false)
}

fn rejects_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    settle_rejects(scope, args, payload, true)
}

/// Node.js's `checkIsPromise`: a promise, or an object with `then` and `catch` methods.
fn is_promise_like(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    let Ok(object) = Object::from_value(scope, *value) else {
        return Ok(false);
    };
    if js::Promise::is_promise(object.handle()) {
        return Ok(true);
    }
    let callable = |value: HandleValue<'_>| {
        Object::from_value(scope, *value).is_ok_and(|value| value.is_callable())
    };
    Ok(!object.is_callable()
        && callable(object.get_property(scope, c"then")?)
        && callable(object.get_property(scope, c"catch")?))
}

/// The shared body of `rejects` and `doesNotReject`, Node.js's `waitForActual`: await `target`,
/// a promise or a function returning one, and settle the returned promise with the assertion's
/// outcome.
fn await_rejection<'s>(
    scope: &'s Scope<'_>,
    target: HandleValue<'_>,
    expected: Option<HandleValue<'_>>,
    message: Option<HandleValue<'_>>,
    rejects: bool,
) -> Result<js::Promise<'s>, ExnThrown> {
    let result = js::Promise::new_pending(scope)?;
    let reject_with = |error: NodeError| -> Result<js::Promise<'s>, ExnThrown> {
        error.throw(scope);
        result.reject_with_pending(scope)?;
        Ok(result)
    };
    let awaited = match Object::from_value(scope, *target) {
        Ok(function) if function.is_callable() => {
            match Function::call(scope, HandleValue::undefined(), target, EmptyArgs) {
                Ok(value) => {
                    if !is_promise_like(scope, value)? {
                        return reject_with(NodeError::new(
                            ErrorKind::TypeError,
                            "ERR_INVALID_RETURN_VALUE",
                            format!(
                                "Expected instance of Promise to be returned from the \"promiseFn\" function but got {}.",
                                crate::errors::describe_value(scope, value)
                            ),
                        ));
                    }
                    value
                }
                Err(ExnThrown) => {
                    result.reject_with_pending(scope)?;
                    return Ok(result);
                }
            }
        }
        _ if is_promise_like(scope, target)? => target,
        _ => {
            return reject_with(NodeError::invalid_arg_type(
                scope,
                "promiseFn",
                "of type function or an instance of Promise",
                target,
            ));
        }
    };
    let awaited = js::Promise::call_original_resolve(scope, awaited)?;
    let state = js::class::create_instance_with::<RejectsStateImpl>(scope, |_| RejectsStateImpl {
        result: Some(Heap::from(result)),
        expected: Heap::from(expected.unwrap_or(HandleValue::undefined()).get()),
        message: message.map(|message| Heap::from(message.get())),
        rejects,
    })?;
    let on_fulfilled = Function::new_callback(scope, c"", 1, rejects_fulfilled, state)?;
    let on_rejected = Function::new_callback(scope, c"", 1, rejects_rejected, state)?;
    awaited.add_reactions(scope, Some(*on_fulfilled), Some(*on_rejected))?;
    Ok(result)
}

/// A new `AssertionError` with the prototype `proto`, or `AssertionError.prototype` if `proto` is
/// `None`, without its `diff` property. Its message is `message` if that is neither `null` nor
/// `undefined`, followed by a diff for the operators `deepStrictEqual`, `strictEqual` and
/// `partialDeepStrictEqual`. Otherwise the message is generated from `actual`, `expected` and
/// `operator`, where both `actual` and `expected` are replaced by their [`copy_error`] if
/// [`error_to_copy`] accepts both, in the message and in the error's properties.
fn assertion_error_without_diff<'s>(
    scope: &'s Scope<'_>,
    message: HandleValue<'_>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    operator: HandleValue<'_>,
    proto: Option<Object<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let operator_text = if operator.is_undefined() {
        "undefined".to_string()
    } else {
        js_string(scope, operator)?
    };
    let custom_diff = matches!(
        operator_text.as_str(),
        "deepStrictEqual" | "strictEqual" | "partialDeepStrictEqual"
    );
    let mut copies = None;
    let text = if !message.is_null() && !message.is_undefined() {
        let message = js_string(scope, message)?;
        if custom_diff {
            create_err_diff(scope, actual, expected, &operator_text, Some(&message))?
        } else {
            message
        }
    } else {
        if let (Some(a), Some(b)) = (
            error_to_copy(scope, actual)?,
            error_to_copy(scope, expected)?,
        ) {
            copies = Some((
                copy_error(scope, a)?.to_jsval_throwing(scope)?,
                copy_error(scope, b)?.to_jsval_throwing(scope)?,
            ));
        }
        let (actual, expected) = copies.unwrap_or((actual, expected));
        generate_message(scope, actual, expected, &operator_text)?
    };
    let (actual, expected) = copies.unwrap_or((actual, expected));
    let error = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, &text)?;
    let error = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
    let proto = match proto {
        Some(proto) => proto,
        None => assertion_error::prototype_object(scope)?,
    };
    error.set_prototype(scope, proto.handle())?;
    error.set_property(
        scope,
        c"generatedMessage",
        !js::conversion::to_boolean(message),
    )?;
    error.define_property(scope, c"name", "AssertionError", 0)?;
    error.set_property(scope, c"code", "ERR_ASSERTION")?;
    error.set_property(scope, c"actual", actual)?;
    error.set_property(scope, c"expected", expected)?;
    error.set_property(scope, c"operator", operator)?;
    Ok(error)
}

/// [`assertion_error_without_diff`], with its `diff` property.
fn new_assertion_error<'s>(
    scope: &'s Scope<'_>,
    message: HandleValue<'_>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    operator: HandleValue<'_>,
    proto: Option<Object<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let error = assertion_error_without_diff(scope, message, actual, expected, operator, proto)?;
    error.set_property(scope, c"diff", "simple")?;
    Ok(error)
}

#[core_runtime::jsfunction_class(name = "AssertionError", class_syntax)]
pub mod assertion_error {
    use super::*;

    /// <https://nodejs.org/api/assert.html#class-assertassertionerror>: an `Error` with
    /// `options.message`, or a message generated from `actual`, `expected` and `operator`, and
    /// with the prototype of `new.target`.
    #[constructor]
    pub fn create<'s>(
        scope: &'s Scope<'_>,
        args: &CallArgs,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        if !args.is_constructing() {
            let message = "Class constructor AssertionError cannot be invoked without 'new'";
            return Err(js::error::TypeError(message.into()).throw(scope).into());
        }
        let options = validate_plain_object(
            scope,
            options.unwrap_or(HandleValue::undefined()),
            "options",
            false,
        )?;
        let message = options.get_property(scope, c"message")?;
        let actual = options.get_property(scope, c"actual")?;
        let expected = options.get_property(scope, c"expected")?;
        let operator = options.get_property(scope, c"operator")?;
        let new_target = scope.root_value(args.new_target().get());
        let proto = match Object::from_value(scope, *new_target) {
            Ok(new_target) => {
                Object::from_value(scope, *new_target.get_property(scope, c"prototype")?).ok()
            }
            Err(_) => None,
        };
        Ok(new_assertion_error(
            scope, message, actual, expected, operator, proto,
        )?)
    }

    /// `AssertionError.prototype.toString()`: `` `${name} [${code}]: ${message}` ``.
    #[method(name = "toString")]
    pub fn to_string(scope: &Scope<'_>, this: Object<'_>) -> Result<String, ExnThrown> {
        let name = js_string(scope, this.get_property(scope, c"name")?)?;
        let code = js_string(scope, this.get_property(scope, c"code")?)?;
        let message = js_string(scope, this.get_property(scope, c"message")?)?;
        Ok(format!("{name} [{code}]: {message}"))
    }

    /// Make `AssertionError` inherit from `Error`, and give it the `length` 1.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let error_ctor = crate::stack_trace::error_constructor(scope)?;
        let error_proto = Object::from_value(scope, *error_ctor.get_property(scope, c"prototype")?)
            .map_err(|e| e.throw(scope))?;
        proto.set_prototype(scope, error_proto.handle())?;
        ctor.set_prototype(scope, error_ctor.handle())?;
        let readonly = js::class_spec::JSPROP_READONLY as u32;
        ctor.define_property(scope, c"length", 1, readonly)
    }
}

/// `assert.fail(actual, expected, message, operator)`. Without arguments or with a nullish one,
/// the message is "Failed", and with one, the argument is the message. With more arguments,
/// DEP0094 is emitted once, and a message is generated from `actual`, `expected` and `operator`
/// unless `message` is given. `operator` is `'!='` with two arguments, and `'fail'` when it is
/// `undefined`.
fn assert_fail(scope: &Scope<'_>, args: &CallArgs) -> Result<(), Thrown> {
    let count = args.argc_;
    let arg = |index: u32| {
        if index < count {
            scope.root_value(args.get(index).get())
        } else {
            HandleValue::undefined()
        }
    };
    let undefined = HandleValue::undefined();
    let actual = arg(0);
    if count <= 1 && (actual.is_undefined() || actual.is_null()) {
        let text = generated(scope, "Failed")?;
        return Err(fail_with(
            scope,
            Some(text),
            undefined,
            undefined,
            "fail",
            Some(true),
        ));
    }
    if count == 1 {
        return Err(fail_with(
            scope,
            Some(actual),
            undefined,
            undefined,
            "fail",
            None,
        ));
    }
    crate::process::deprecate_once(
        scope,
        "assert.fail() with more than one argument is deprecated. Please use \
         assert.strictEqual() instead or only pass a message.",
        "DEP0094",
    )?;
    let operator = match (count, arg(3)) {
        (2, _) => "!=",
        (_, operator) if operator.is_undefined() => "fail",
        (_, operator) => {
            return Err(fail_with_operator(
                scope,
                Some(arg(2)),
                actual,
                arg(1),
                operator,
                None,
            ))
        }
    };
    Err(fail_with(
        scope,
        Some(arg(2)),
        actual,
        arg(1),
        operator,
        None,
    ))
}

/// `assert.throws(fn, expected, message)`.
fn assert_throws(
    scope: &Scope<'_>,
    function: HandleValue<'_>,
    expected: Option<HandleValue<'_>>,
    message: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    validate_function(scope, function, "fn")?;
    let error = match Function::call(scope, HandleValue::undefined(), function, EmptyArgs) {
        Ok(_) => None,
        Err(ExnThrown) => Some(js::exception::take_pending(scope).ok_or(Thrown::Pending)?),
    };
    let error = error.map(|error| scope.root_value(error.get()));
    expects_error(scope, error, expected, message, "throws")
}

/// `assert.doesNotThrow(fn, expected, message)`.
fn assert_does_not_throw(
    scope: &Scope<'_>,
    function: HandleValue<'_>,
    expected: Option<HandleValue<'_>>,
    message: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    validate_function(scope, function, "fn")?;
    let error = match Function::call(scope, HandleValue::undefined(), function, EmptyArgs) {
        Ok(_) => None,
        Err(ExnThrown) => Some(js::exception::take_pending(scope).ok_or(Thrown::Pending)?),
    };
    let error = error.map(|error| scope.root_value(error.get()));
    expects_no_error(scope, error, expected, message, "doesNotThrow")
}

/// `assert.ifError(value)`: an `AssertionError` for any value but `null` and `undefined`, whose
/// message holds the value's `message` if it is a string.
fn check_if_error(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<(), Thrown> {
    if value.is_undefined() || value.is_null() {
        return Ok(());
    }
    let mut text = "ifError got unwanted exception: ".to_string();
    let error_message = match Object::from_value(scope, *value) {
        Ok(object) if !object.is_callable() => {
            Some((object, object.get_property(scope, c"message")?))
        }
        _ => None,
    };
    match error_message {
        Some((object, message)) if message.is_string() => {
            let message = js_string(scope, message)?;
            let ctor = object.get_property(scope, c"constructor")?;
            if message.is_empty() && bool::from_jsval(scope, ctor, ()).unwrap_or(false) {
                let ctor = Object::from_value(scope, *ctor).map_err(|e| e.throw(scope))?;
                text.push_str(&js_string(scope, ctor.get_property(scope, c"name")?)?);
            } else {
                text.push_str(&message);
            }
        }
        _ => text.push_str(&show(scope, value)?),
    }
    let text = generated(scope, &text)?;
    Err(fail_with(
        scope,
        Some(text),
        value,
        HandleValue::null(),
        "ifError",
        None,
    ))
}

/// The shared body of `match` and `doesNotMatch`.
fn check_match(
    scope: &Scope<'_>,
    string: HandleValue<'_>,
    regexp: HandleValue<'_>,
    message: Option<HandleValue<'_>>,
    negated: bool,
) -> Result<(), Thrown> {
    if !is_regexp(scope, regexp)? {
        return Err(
            NodeError::invalid_arg_type(scope, "regexp", "an instance of RegExp", regexp).into(),
        );
    }
    let is_string = string.is_string();
    if is_string && inspect::regexp_exec_matches(scope, regexp, string)? != negated {
        return Ok(());
    }
    let operator = if negated { "doesNotMatch" } else { "match" };
    let message_value = message.unwrap_or(HandleValue::undefined());
    if bool::from_jsval(scope, message_value, ()).unwrap_or(false) {
        return Err(fail_with(
            scope,
            message,
            string,
            regexp,
            operator,
            Some(false),
        ));
    }
    let text = if !is_string {
        format!(
            "The \"string\" argument must be of type string. Received type {} ({})",
            type_of(scope, string),
            show(scope, string)?
        )
    } else {
        format!(
            "{} {}. Input:\n\n{}\n",
            if negated {
                "The input was expected to not match the regular expression"
            } else {
                "The input did not match the regular expression"
            },
            show(scope, regexp)?,
            show(scope, string)?
        )
    };
    let text = generated(scope, &text)?;
    Err(fail_with(
        scope,
        Some(text),
        string,
        regexp,
        operator,
        Some(true),
    ))
}

/// A function `CallTracker.calls` tracks: its calls as frozen `{ thisArg, arguments }` objects,
/// the expected number of calls, its name, and an `Error` created by the `calls` call.
#[core_runtime::jsclass(hidden)]
pub(crate) struct CallTrackerContext {
    calls: Vec<Heap<js::object::Object>>,
    #[no_trace]
    expected: f64,
    #[no_trace]
    name: String,
    stack: Heap<Value>,
}

#[core_runtime::jsmethods]
impl CallTrackerContext {}

impl<'s> CallTrackerContext<'s> {
    /// The calls as a frozen array.
    fn frozen_calls<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Array<'r>, ExnThrown> {
        let calls: Vec<Object<'_>> = self
            .data()
            .calls
            .iter()
            .map(|call| call.get(scope))
            .collect();
        let array = js::Array::with_contents(scope, &calls)?;
        array.freeze(scope)?;
        Ok(array)
    }

    /// An object describing how the number of calls differs from the expected one, or `None` if
    /// the function was called the expected number of times.
    fn report<'r>(&self, scope: &'r Scope<'_>) -> Result<Option<Object<'r>>, ExnThrown> {
        let (actual, expected, name, stack) = {
            let data = self.data();
            let stack = data.stack.get(scope);
            (
                data.calls.len() as f64,
                data.expected,
                data.name.clone(),
                stack,
            )
        };
        if actual == expected {
            return Ok(None);
        }
        let report = Object::new_plain(scope)?;
        let message = format!(
            "Expected the {name} function to be executed {} time(s) but was executed {} time(s).",
            crate::errors::number_to_string(expected),
            crate::errors::number_to_string(actual),
        );
        report.set_property(scope, c"message", message.as_str())?;
        report.set_property(scope, c"actual", actual)?;
        report.set_property(scope, c"expected", expected)?;
        report.set_property(scope, c"operator", name.as_str())?;
        report.set_property(scope, c"stack", stack)?;
        Ok(Some(report))
    }
}

/// The `apply` trap of a tracked function's proxy, whose payload is its [`CallTrackerContext`]:
/// record the call's `this` and a frozen copy of its arguments, then call the function.
fn tracked_apply(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let context = cast_payload::<CallTrackerContext>(scope, payload);
    let target = args.get(0);
    let this = args.get(1);
    let list = Object::from_value(scope, *args.get(2))
        .map_err(|e| e.throw(scope))?
        .cast::<js::Array>()
        .map_err(|_| js::error::TypeError("the arguments are not an array".into()).throw(scope))?;
    let arguments = list.to_vec(scope)?;
    let copy = js::Array::with_contents(scope, &arguments)?;
    copy.freeze(scope)?;
    let call = Object::new_plain(scope)?;
    call.set_property(scope, c"thisArg", this)?;
    call.set_property(scope, c"arguments", *copy)?;
    call.freeze(scope)?;
    context.data_mut().calls.push(Heap::from(call));
    let result = Function::call(scope, this, target, &arguments[..])?;
    Ok(result.get())
}

/// A `CallTracker`: the contexts of the functions it tracks, and a `WeakMap` from each tracked
/// function to its context. See <https://nodejs.org/api/assert.html#class-assertcalltracker>.
#[core_runtime::jsclass(name = "CallTracker", hidden)]
pub struct CallTracker {
    checks: Vec<Heap<CallTrackerContextImpl>>,
    tracked: Option<Heap<js::collections::weak_map::WeakMap>>,
}

#[core_runtime::jsmethods(node_args)]
impl CallTracker {
    /// Emits DEP0173 the first time a tracker is created.
    #[constructor]
    pub fn new(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        crate::process::deprecate_once(scope, "assert.CallTracker is deprecated.", "DEP0173")?;
        let tracked = js::WeakMap::new(scope)?;
        self.data_mut().tracked = Some(Heap::from(tracked));
        Ok(())
    }

    /// <https://nodejs.org/api/assert.html#trackercallsfn-exact>
    #[method(length = 1)]
    pub fn calls<'r>(
        &self,
        scope: &'r Scope<'_>,
        function: Option<HandleValue<'r>>,
        expected: Option<HandleValue<'r>>,
    ) -> Result<Object<'r>, Thrown> {
        if crate::process::exiting(scope)? {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_UNAVAILABLE_DURING_EXIT",
                "Cannot call function in process exit handler",
            )
            .into());
        }
        let (function, expected) = match function {
            Some(count) if count.is_number() => (None, Some(count)),
            Some(function) if function.is_undefined() => (None, expected),
            function => (function, expected),
        };
        let expected = match expected.filter(|expected| !expected.is_undefined()) {
            Some(expected) => {
                crate::errors::validate_integer(scope, expected, "expected", 1.0, 4294967295.0)?
            }
            None => 1.0,
        };
        // Without a function, Node.js tracks `Function.prototype`, whose name is empty.
        let function = match function {
            Some(function) => function,
            None => {
                let prototype = js::class::get_class_prototype(
                    scope,
                    js::class_spec::JSProtoKey::JSProto_Function,
                )?;
                let prototype = Object::from_handle(prototype).ok_or(ExnThrown)?;
                prototype.to_jsval_throwing(scope)?
            }
        };
        let name = Object::from_value(scope, *function)
            .ok()
            .and_then(|function| function.get_property(scope, c"name").ok())
            .and_then(|name| crate::support::rust_string(scope, name))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "calls".to_string());
        let stack = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, "")?;
        let context = js::class::create_instance_with::<CallTrackerContextImpl>(scope, |_| {
            CallTrackerContextImpl {
                calls: Vec::new(),
                expected,
                name,
                stack: Heap::from(stack.get()),
            }
        })?;
        let handler = Object::new_plain(scope)?;
        handler.set_prototype(scope, js::native::HandleObject::null())?;
        let apply = Function::new_callback(scope, c"", 3, tracked_apply, context)?;
        handler.set_property(scope, c"apply", *apply)?;
        let proxy = js::class::get_class_object(scope, js::class_spec::JSProtoKey::JSProto_Proxy)?;
        let proxy = Object::from_handle(proxy).ok_or(ExnThrown)?;
        let tracked = Function::construct(
            scope,
            proxy,
            &[function.get(), handler.to_jsval_raw_throwing(scope)?],
        )?;
        let map = {
            let mut data = self.data_mut();
            data.checks.push(Heap::from(context));
            data.tracked.as_ref().map(|map| map.get(scope))
        };
        let map = map.ok_or(ExnThrown)?;
        map.insert(
            scope,
            tracked.to_jsval_throwing(scope)?,
            context.to_jsval_throwing(scope)?,
        )?;
        Ok(tracked)
    }

    /// <https://nodejs.org/api/assert.html#trackergetcallsfn>
    #[method]
    pub fn get_calls<'r>(
        &self,
        scope: &'r Scope<'_>,
        tracked: HandleValue<'_>,
    ) -> Result<js::Array<'r>, Thrown> {
        Ok(self.context(scope, tracked)?.frozen_calls(scope)?)
    }

    /// <https://nodejs.org/api/assert.html#trackerreport>
    #[method]
    pub fn report<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Array<'r>, ExnThrown> {
        let mut reports: Vec<Object<'r>> = Vec::new();
        for context in self.contexts(scope) {
            if let Some(report) = context.report(scope)? {
                reports.push(report);
            }
        }
        js::Array::with_contents(scope, &reports)
    }

    /// <https://nodejs.org/api/assert.html#trackerresetfn>
    #[method(length = 1)]
    pub fn reset(&self, scope: &Scope<'_>, tracked: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        match tracked.filter(|tracked| !tracked.is_undefined()) {
            Some(tracked) => self.context(scope, tracked)?.data_mut().calls.clear(),
            None => {
                for context in self.contexts(scope) {
                    context.data_mut().calls.clear();
                }
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/assert.html#trackerverify>
    #[method]
    pub fn verify(&self, scope: &Scope<'_>) -> Result<(), Thrown> {
        let mut reports = Vec::new();
        for context in self.contexts(scope) {
            if let Some(report) = context.report(scope)? {
                reports.push(report);
            }
        }
        let message = match reports.as_slice() {
            [] => return Ok(()),
            [report] => report.get_property(scope, c"message")?,
            _ => js::class::get_or_init_atom(
                scope,
                c"Functions were not called the expected number of times",
            )?,
        };
        let undefined = HandleValue::undefined();
        let error =
            assertion_error_without_diff(scope, message, undefined, undefined, undefined, None)?;
        for (index, report) in reports.iter().enumerate() {
            for (field, key) in [
                ("message", c"message"),
                ("actual", c"actual"),
                ("expected", c"expected"),
                ("operator", c"operator"),
                ("stack trace", c"stack"),
            ] {
                let name =
                    std::ffi::CString::new(format!("{field} {index}")).map_err(|_| ExnThrown)?;
                error.set_property_by_name(scope, &name, report.get_property(scope, key)?)?;
            }
        }
        error.set_property(scope, c"diff", "simple")?;
        js::exception::set_pending(
            scope,
            error.to_jsval_throwing(scope)?,
            js::native::ExceptionStackBehavior::Capture,
        );
        Err(Thrown::Pending)
    }
}

impl<'s> CallTracker<'s> {
    /// The contexts of the tracked functions.
    fn contexts<'r>(&self, scope: &'r Scope<'_>) -> Vec<CallTrackerContext<'r>> {
        self.data()
            .checks
            .iter()
            .map(|context| context.get(scope))
            .collect()
    }

    /// The context of the tracked function `tracked`, or `ERR_INVALID_ARG_VALUE` if this tracker
    /// doesn't track it.
    fn context<'r>(
        &self,
        scope: &'r Scope<'_>,
        tracked: HandleValue<'_>,
    ) -> Result<CallTrackerContext<'r>, Thrown> {
        let map = self.data().tracked.as_ref().map(|map| map.get(scope));
        let map = map.ok_or(ExnThrown)?;
        let context = if tracked.is_object() {
            map.lookup(scope, scope.root_value(tracked.get()))?
        } else {
            HandleValue::undefined()
        };
        if context.is_undefined() {
            return Err(NodeError::invalid_arg_value(
                scope,
                "tracked",
                tracked,
                "is not a tracked function",
            )
            .into());
        }
        Ok(cast_payload::<CallTrackerContext>(scope, context))
    }
}

/// Register the classes of `node:assert`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    assertion_error::register(scope)?;
    Comparison::add_to_global(scope, global);
    RejectsState::add_to_global(scope, global);
    CallTrackerContext::add_to_global(scope, global);
    CallTracker::add_to_global(scope, global);
    Ok(())
}

#[core_runtime::jsmodule(node_args, name = "node:assert", default = "ok")]
pub mod module {
    use super::*;

    pub use super::assertion_error as AssertionError;
    pub use super::CallTracker;

    /// <https://nodejs.org/api/assert.html#assertokvalue-message>
    pub fn ok(
        scope: &Scope<'_>,
        value: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_ok(scope, value, message)
    }

    /// <https://nodejs.org/api/assert.html#assertequalactual-expected-message>
    #[function(length = 3)]
    pub fn equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, false, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotequalactual-expected-message>
    #[function(length = 3)]
    pub fn not_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, false, true)
    }

    /// <https://nodejs.org/api/assert.html#assertstrictequalactual-expected-message>
    #[function(length = 3)]
    pub fn strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotstrictequalactual-expected-message>
    #[function(length = 3)]
    pub fn not_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdeepequalactual-expected-message>
    #[function(length = 3)]
    pub fn deep_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, false, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotdeepequalactual-expected-message>
    #[function(length = 3)]
    pub fn not_deep_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, false, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdeepstrictequalactual-expected-message>
    #[function(length = 3)]
    pub fn deep_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotdeepstrictequalactual-expected-message>
    #[function(length = 3)]
    pub fn not_deep_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertthrowsfn-error-message>
    pub fn throws(
        scope: &Scope<'_>,
        function: HandleValue<'_>,
        expected: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        assert_throws(scope, function, expected, message)
    }

    /// <https://nodejs.org/api/assert.html#assertdoesnotthrowfn-error-message>
    pub fn does_not_throw(
        scope: &Scope<'_>,
        function: HandleValue<'_>,
        expected: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        assert_does_not_throw(scope, function, expected, message)
    }

    /// <https://nodejs.org/api/assert.html#assertrejectsasyncfn-error-message>
    pub fn rejects<'s>(
        scope: &'s Scope<'_>,
        target: HandleValue<'_>,
        expected: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        await_rejection(scope, target, expected, message, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdoesnotrejectasyncfn-error-message>
    pub fn does_not_reject<'s>(
        scope: &'s Scope<'_>,
        target: HandleValue<'_>,
        expected: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        await_rejection(scope, target, expected, message, false)
    }

    /// <https://nodejs.org/api/assert.html#assertfailmessage>
    #[function(length = 5)]
    pub fn fail(scope: &Scope<'_>, args: &CallArgs) -> Result<(), ExnThrown> {
        assert_fail(scope, args).map_err(|error| error.throw(scope))
    }

    /// <https://nodejs.org/api/assert.html#assertmatchstring-regexp-message>
    #[function(length = 3)]
    pub fn r#match(
        scope: &Scope<'_>,
        string: HandleValue<'_>,
        regexp: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_match(scope, string, regexp, message, false)
    }

    /// <https://nodejs.org/api/assert.html#assertdoesnotmatchstring-regexp-message>
    #[function(length = 3)]
    pub fn does_not_match(
        scope: &Scope<'_>,
        string: HandleValue<'_>,
        regexp: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_match(scope, string, regexp, message, true)
    }

    /// <https://nodejs.org/api/assert.html#assertiferrorvalue>
    pub fn if_error(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<(), Thrown> {
        check_if_error(scope, value)
    }
}

#[core_runtime::jsmodule(
    node_args,
    name = "node:assert/strict",
    default = "ok",
    self_export = "strict"
)]
pub mod strict {
    use super::*;

    pub use super::assertion_error as AssertionError;
    pub use super::CallTracker;

    /// <https://nodejs.org/api/assert.html#assertokvalue-message>
    pub fn ok(
        scope: &Scope<'_>,
        value: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_ok(scope, value, message)
    }

    /// <https://nodejs.org/api/assert.html#assertequalactual-expected-message>
    #[function(length = 3)]
    pub fn equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotequalactual-expected-message>
    #[function(length = 3)]
    pub fn not_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertstrictequalactual-expected-message>
    #[function(length = 3)]
    pub fn strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotstrictequalactual-expected-message>
    #[function(length = 3)]
    pub fn not_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdeepequalactual-expected-message>
    #[function(length = 3)]
    pub fn deep_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotdeepequalactual-expected-message>
    #[function(length = 3)]
    pub fn not_deep_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdeepstrictequalactual-expected-message>
    #[function(length = 3)]
    pub fn deep_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotdeepstrictequalactual-expected-message>
    #[function(length = 3)]
    pub fn not_deep_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertthrowsfn-error-message>
    pub fn throws(
        scope: &Scope<'_>,
        function: HandleValue<'_>,
        expected: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        assert_throws(scope, function, expected, message)
    }

    /// <https://nodejs.org/api/assert.html#assertdoesnotthrowfn-error-message>
    pub fn does_not_throw(
        scope: &Scope<'_>,
        function: HandleValue<'_>,
        expected: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        assert_does_not_throw(scope, function, expected, message)
    }

    /// <https://nodejs.org/api/assert.html#assertrejectsasyncfn-error-message>
    pub fn rejects<'s>(
        scope: &'s Scope<'_>,
        target: HandleValue<'_>,
        expected: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        await_rejection(scope, target, expected, message, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdoesnotrejectasyncfn-error-message>
    pub fn does_not_reject<'s>(
        scope: &'s Scope<'_>,
        target: HandleValue<'_>,
        expected: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        await_rejection(scope, target, expected, message, false)
    }

    /// <https://nodejs.org/api/assert.html#assertfailmessage>
    #[function(length = 5)]
    pub fn fail(scope: &Scope<'_>, args: &CallArgs) -> Result<(), ExnThrown> {
        assert_fail(scope, args).map_err(|error| error.throw(scope))
    }

    /// <https://nodejs.org/api/assert.html#assertmatchstring-regexp-message>
    #[function(length = 3)]
    pub fn r#match(
        scope: &Scope<'_>,
        string: HandleValue<'_>,
        regexp: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_match(scope, string, regexp, message, false)
    }

    /// <https://nodejs.org/api/assert.html#assertdoesnotmatchstring-regexp-message>
    #[function(length = 3)]
    pub fn does_not_match(
        scope: &Scope<'_>,
        string: HandleValue<'_>,
        regexp: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_match(scope, string, regexp, message, true)
    }

    /// <https://nodejs.org/api/assert.html#assertiferrorvalue>
    pub fn if_error(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<(), Thrown> {
        check_if_error(scope, value)
    }
}

/// Register `node:assert`, whose `strict` is `node:assert/strict`'s `default` export.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let strict = || -> Result<HandleValue<'_>, ExnThrown> {
        core_runtime::module::builtin_namespace(scope, "node:assert/strict")?
            .ok_or(ExnThrown)?
            .get_property(scope, c"default")
    };
    let Ok(strict) = strict() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &[("strict", strict)])
}
