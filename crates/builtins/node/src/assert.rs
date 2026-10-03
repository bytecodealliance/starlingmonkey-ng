// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:assert` and `node:assert/strict`. See <https://nodejs.org/api/assert.html>.
//!
//! The assertions follow Node.js's `lib/assert.js`, and `AssertionError` its
//! `lib/internal/assert/assertion_error.js`, without colors.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{CallbackArgs, EmptyArgs};
use js::gc::scope::Scope;
use js::native::{CallArgs, Value};
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_function, validate_plain_object, ErrorKind, NodeError, Thrown};
use crate::inspect::{self, Compact, Getters, InspectOptions};

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
    options.sorted = true;
    options.getters = Getters::All;
    inspect::inspect(scope, value, options)
}

/// `String(value)`, which converts a symbol to its description.
fn js_string(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    if value.is_symbol() {
        return Ok(inspect::symbol_to_string(scope, value));
    }
    String::from_jsval(scope, value, ()).map_err(|_| ExnThrown)
}

/// Whether `value` is an object whose `typeof` is `'object'`.
fn is_typeof_object(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    Object::from_value(scope, *value).is_ok_and(|object| !object.is_callable())
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

/// Whether `value` is an `Error`.
fn is_error(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    match Object::from_value(scope, *value) {
        Ok(object) => inspect::is_error(scope, object),
        Err(_) => Ok(false),
    }
}

/// Whether `value` is a `RegExp`.
fn is_regexp(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    match Object::from_value(scope, *value) {
        Ok(object) => inspect::is_regexp(scope, object),
        Err(_) => Ok(false),
    }
}

/// `regexp.exec(text) !== null`.
fn regexp_matches(
    scope: &Scope<'_>,
    regexp: HandleValue<'_>,
    text: HandleValue<'_>,
) -> Result<bool, ExnThrown> {
    let regexp = Object::from_value(scope, *regexp).map_err(|e| e.throw(scope))?;
    Ok(!Function::call_by_name(scope, regexp.handle(), c"exec", &[text])?.is_null())
}

/// `value instanceof ctor`, `false` for a primitive `value`.
fn instance_of(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    ctor: Object<'_>,
) -> Result<bool, ExnThrown> {
    match Object::from_value(scope, *value) {
        Ok(object) => inspect::instance_of(scope, object, ctor),
        Err(_) => Ok(false),
    }
}

/// The global `Error` constructor.
fn error_constructor<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, *scope.global().get_property(scope, c"Error")?)
        .map_err(|e| e.throw(scope))
}

/// The UTF-16 length of `text`.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
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
enum DiffOp {
    Delete,
    Nop,
    Insert,
}

/// Node.js's `myersDiff` of the lines `actual` and `expected`, in reverse order.
fn myers_diff(actual: &[String], expected: &[String], comma: bool) -> Vec<(DiffOp, String)> {
    let equal =
        |a: &str, b: &str| a == b || (comma && (format!("{a},") == b || a == format!("{b},")));
    let max = actual.len() + expected.len();
    let mut v = vec![0isize; 2 * max + 1];
    let mut trace = Vec::new();
    for level in 0..=max as isize {
        trace.push(v.clone());
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
                return backtrack(&trace, actual, expected, comma);
            }
            diagonal += 2;
        }
    }
    Vec::new()
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
    let objects = is_typeof_object(scope, actual) && is_typeof_object(scope, expected);
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
        && (!is_typeof_object(scope, actual) || !is_typeof_object(scope, expected));
    let message = if simple {
        let (a, e) = (&split_actual[0], &split_expected[0]);
        let mut length = js_len(a) + js_len(e);
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
            if actual.is_string() && expected.is_string() && length <= 80 {
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
        let comma = is_typeof_object(scope, actual);
        let diff = myers_diff(&split_actual, &split_expected, comma);
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
            let separator = if js_len(&lines[0]) > 5 { "\n\n" } else { " " };
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
        if js_len(&result) > 1024 {
            result = truncate_units(&result, 1021) + "...";
        }
        return Ok(result);
    }
    if js_len(&result) > 512 {
        result = truncate_units(&result, 509) + "...";
    }
    if js_len(&other) > 512 {
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

/// Throw `message` if it is an `Error`, as Node.js's `innerFail` does, and the `AssertionError`
/// of `options` otherwise.
#[allow(clippy::too_many_arguments)]
fn fail_with(
    scope: &Scope<'_>,
    message: Option<HandleValue<'_>>,
    actual: HandleValue<'_>,
    expected: HandleValue<'_>,
    operator: &str,
    generated: Option<bool>,
) -> Thrown {
    let message = message.filter(|message| !message.is_undefined());
    if let Some(error) = message.filter(|message| is_error(scope, *message).unwrap_or(false)) {
        js::exception::set_pending(
            scope,
            error,
            js::native::ExceptionStackBehavior::DoNotCapture,
        );
        return Thrown::Pending;
    }
    let build = || -> Result<HandleValue<'_>, ExnThrown> {
        let options = Object::new_plain(scope)?;
        if let Some(message) = message {
            options.set_property(scope, c"message", message)?;
        }
        options.set_property(scope, c"actual", actual)?;
        options.set_property(scope, c"expected", expected)?;
        options.set_property(scope, c"operator", operator)?;
        let ctor = assertion_error::class_constructor(scope)?;
        let error = Function::construct(scope, ctor, &[options])?;
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
    let class = Function::new_callback(scope, c"Comparison", 0, comparison_constructor, ())?;
    let proto = Object::new_plain(scope)?;
    proto.define_property(scope, c"constructor", *class, 0)?;
    class.set_property(scope, c"prototype", proto)?;
    let result = Object::new_plain(scope)?;
    result.set_prototype(scope, proto.handle())?;
    for key in keys {
        let id = scope.root_id(js::id::value_to_id(scope, *key)?);
        if !object.has_property_by_id(scope, id)? {
            continue;
        }
        let value = object.get_property_by_id(scope, id)?;
        let mut shown = value;
        if let Some(actual) = actual {
            let have = actual.get_property_by_id(scope, id)?;
            if have.is_string() && is_regexp(scope, value)? && regexp_matches(scope, value, have)? {
                shown = have;
            }
        }
        result.set_property_by_id(scope, id, shown)?;
    }
    Ok(result)
}

/// The body of the `Comparison` constructor messages use.
fn comparison_constructor(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

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
            if regexp_matches(scope, expected, text_value)? {
                return Ok(());
            }
            generated_text = format!(
                "The input did not match the regular expression {}. Input:\n\n{}\n",
                show(scope, expected)?,
                show(scope, text_value)?
            );
        } else if !is_typeof_object(scope, actual) {
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
                let descriptor = Function::call_by_name(
                    scope,
                    expected_object.handle(),
                    c"propertyIsEnumerable",
                    &[key],
                );
                let enumerable = match descriptor {
                    Ok(enumerable) => enumerable.is_boolean() && enumerable.to_boolean(),
                    Err(ExnThrown) => return Err(Thrown::Pending),
                };
                if enumerable {
                    keys.push(key);
                }
            }
            if instance_of(scope, expected, error_constructor(scope)?)? {
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
                if have.is_string() && is_regexp(scope, want)? && regexp_matches(scope, want, have)?
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
        if !prototype.is_undefined() && instance_of(scope, actual, expected_object)? {
            return Ok(());
        }
        let error_ctor = error_constructor(scope)?;
        let derived_from_error = {
            let mut current = expected_object.get_prototype(scope)?;
            let mut found = false;
            while let Some(candidate) = current {
                if candidate.as_raw() == error_ctor.as_raw() {
                    found = true;
                    break;
                }
                current = candidate.get_prototype(scope)?;
            }
            found
        };
        let expected_name = js_string(scope, expected_object.get_property(scope, c"name")?)?;
        if derived_from_error {
            let mut text = format!(
                "The error is expected to be an instance of \"{expected_name}\". Received "
            );
            if is_error(scope, actual)? {
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
            if is_error(scope, actual)? {
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
            Some(actual) if is_typeof_object(scope, actual) => {
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
            return Ok(regexp_matches(scope, expected, text)?);
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
    if !prototype.is_undefined() && instance_of(scope, actual, expected_object)? {
        return Ok(true);
    }
    let error_ctor = error_constructor(scope)?;
    let mut current = expected_object.get_prototype(scope)?;
    while let Some(candidate) = current {
        if candidate.as_raw() == error_ctor.as_raw() {
            return Ok(false);
        }
        current = candidate.get_prototype(scope)?;
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
    js::exception::set_pending(
        scope,
        actual,
        js::native::ExceptionStackBehavior::DoNotCapture,
    );
    Err(Thrown::Pending)
}

/// The body of `rejects`' and `doesNotReject`'s reaction to the awaited promise. The payload is
/// `[result promise, expected, message, rejects, message given]`, and the reaction's first
/// argument the outcome.
fn settle_rejects(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
    rejected: bool,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, *payload).map_err(|e| e.throw(scope))?;
    let result = Object::from_value(scope, *state.get_element(scope, 0)?)
        .map_err(|e| e.throw(scope))?
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("the result is not a promise".into()).throw(scope))?;
    let expected = state.get_element(scope, 1)?;
    let message = state.get_element(scope, 2)?;
    let rejects = state.get_element(scope, 3)?.to_boolean();
    let message_given = state.get_element(scope, 4)?.to_boolean();
    let outcome = args.get(0);
    let expected = (!expected.is_undefined()).then_some(expected);
    let message = message_given.then_some(message);
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
    let promise_ctor = Object::from_value(scope, *scope.global().get_property(scope, c"Promise")?)
        .map_err(|e| e.throw(scope))?;
    let awaited = Function::call_by_name(scope, promise_ctor.handle(), c"resolve", &[awaited])?;
    let awaited = Object::from_value(scope, *awaited).map_err(|e| e.throw(scope))?;
    let state = js::Array::with_contents(
        scope,
        &[
            result.to_jsval_raw_throwing(scope)?,
            expected.map_or(js::value::undefined(), |expected| expected.get()),
            message.map_or(js::value::undefined(), |message| message.get()),
            js::value::from_bool(rejects),
            js::value::from_bool(message.is_some()),
        ],
    )?;
    let on_fulfilled = Function::new_callback(scope, c"", 1, rejects_fulfilled, *state)?;
    let on_rejected = Function::new_callback(scope, c"", 1, rejects_rejected, *state)?;
    Function::call_by_name(
        scope,
        awaited.handle(),
        c"then",
        &[*on_fulfilled, *on_rejected],
    )?;
    Ok(result)
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
        let operator_text = if operator.is_undefined() {
            "undefined".to_string()
        } else {
            js_string(scope, operator)?
        };
        let custom_diff = matches!(
            operator_text.as_str(),
            "deepStrictEqual" | "strictEqual" | "partialDeepStrictEqual"
        );
        let text = if !message.is_null() && !message.is_undefined() {
            let message = js_string(scope, message)?;
            if custom_diff {
                create_err_diff(scope, actual, expected, &operator_text, Some(&message))?
            } else {
                message
            }
        } else {
            generate_message(scope, actual, expected, &operator_text)?
        };
        let error = Function::construct(scope, error_constructor(scope)?, &[text.as_str()])?;
        let mut proto = None;
        if args.is_constructing() {
            let new_target = scope.root_value(args.new_target().get());
            if let Ok(new_target) = Object::from_value(scope, *new_target) {
                proto =
                    Object::from_value(scope, *new_target.get_property(scope, c"prototype")?).ok();
            }
        }
        let proto = match proto {
            Some(proto) => proto,
            None => Object::from_value(
                scope,
                *class_constructor(scope)?.get_property(scope, c"prototype")?,
            )?,
        };
        error.set_prototype(scope, proto.handle())?;
        let generated = !bool::from_jsval(scope, message, ()).unwrap_or(false);
        error.set_property(scope, c"generatedMessage", generated)?;
        error.set_property(scope, c"code", "ERR_ASSERTION")?;
        error.set_property(scope, c"actual", actual)?;
        error.set_property(scope, c"expected", expected)?;
        error.set_property(scope, c"operator", operator)?;
        error.set_property(scope, c"diff", "simple")?;
        Ok(error)
    }

    /// Make `AssertionError.prototype` inherit from `Error.prototype`, with `name`
    /// `AssertionError`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let error_ctor = error_constructor(scope)?;
        let error_proto = Object::from_value(scope, *error_ctor.get_property(scope, c"prototype")?)
            .map_err(|e| e.throw(scope))?;
        proto.set_prototype(scope, error_proto.handle())?;
        ctor.set_prototype(scope, error_ctor.handle())?;
        proto.set_property(scope, c"name", "AssertionError")
    }
}

/// `assert.fail(message)`: "Failed" as a generated message if there is none.
fn assert_fail(scope: &Scope<'_>, message: Option<HandleValue<'_>>) -> Result<(), Thrown> {
    let undefined = HandleValue::undefined();
    match message.filter(|message| !message.is_undefined() && !message.is_null()) {
        Some(message) => Err(fail_with(
            scope,
            Some(message),
            undefined,
            undefined,
            "fail",
            None,
        )),
        None => {
            let text = generated(scope, "Failed")?;
            Err(fail_with(
                scope,
                Some(text),
                undefined,
                undefined,
                "fail",
                Some(true),
            ))
        }
    }
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
    if is_string && regexp_matches(scope, regexp, string)? != negated {
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

/// The context of a function `CallTracker.calls` tracks: a plain object with the tracked calls in
/// `calls`, the expected count in `expected`, the function's name in `name` and an `Error` from the
/// `calls` call in `stack`.
fn tracker_context<'s>(
    scope: &'s Scope<'_>,
    expected: f64,
    name: &str,
) -> Result<Object<'s>, ExnThrown> {
    let context = Object::new_plain(scope)?;
    context.set_property(scope, c"calls", js::Array::new(scope, 0)?)?;
    context.set_property(scope, c"expected", expected)?;
    context.set_property(scope, c"name", name)?;
    let error_class = scope.global().get_property(scope, c"Error")?;
    let error_class = Object::from_value(scope, error_class).map_err(|e| e.throw(scope))?;
    let stack = Function::construct(scope, error_class, js::function::EmptyArgs)?;
    context.set_property(scope, c"stack", stack)?;
    Ok(context)
}

/// The `apply` trap of a tracked function's proxy, whose captures are `[context]`: record the
/// call's `this` and a frozen copy of its arguments, then call the function.
fn tracked_apply(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let context = crate::stream::Captures::of(scope, payload)?.obj(0)?;
    let target = args.get(0);
    let this = args.get(1);
    let list = Object::from_value(scope, *args.get(2)).map_err(|e| e.throw(scope))?;
    let object_ctor = scope.global().get_property(scope, c"Object")?;
    let object_ctor = Object::from_value(scope, object_ctor).map_err(|e| e.throw(scope))?;
    let copy = crate::stream::call_method(scope, list, c"slice", &[])?;
    crate::stream::call_method(scope, object_ctor, c"freeze", &[copy])?;
    let call = Object::new_plain(scope)?;
    call.set_property(scope, c"thisArg", this)?;
    call.set_property(scope, c"arguments", copy)?;
    crate::stream::call_method(
        scope,
        object_ctor,
        c"freeze",
        &[call.to_jsval_throwing(scope)?],
    )?;
    let calls = context.get_property(scope, c"calls")?;
    let calls = Object::from_value(scope, *calls).map_err(|e| e.throw(scope))?;
    crate::stream::call_method(scope, calls, c"push", &[call.to_jsval_throwing(scope)?])?;
    let reflect = scope.global().get_property(scope, c"Reflect")?;
    let reflect = Object::from_value(scope, reflect).map_err(|e| e.throw(scope))?;
    let result =
        crate::stream::call_method(scope, reflect, c"apply", &[target, this, args.get(2)])?;
    Ok(result.get())
}

/// The report of `context`: an object describing the mismatch, or `None` if the function was
/// called the expected number of times.
fn context_report<'s>(
    scope: &'s Scope<'_>,
    context: Object<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let calls = context.get_property(scope, c"calls")?;
    let calls = Object::from_value(scope, *calls).map_err(|e| e.throw(scope))?;
    let actual = crate::stream::Props::num(&calls, scope, c"length")?;
    let expected = crate::stream::Props::num(&context, scope, c"expected")?;
    if actual == expected {
        return Ok(None);
    }
    let name =
        String::from_jsval(scope, context.get_property(scope, c"name")?, ()).unwrap_or_default();
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
    report.set_property(scope, c"stack", context.get_property(scope, c"stack")?)?;
    Ok(Some(report))
}

/// <https://nodejs.org/api/assert.html#class-assertcalltracker>: a deprecated tracker of how
/// often functions are called. Its state is the array of contexts in `_starlingChecks` and the
/// `WeakMap` from tracked functions to contexts in `_starlingTracked`.
#[core_runtime::jsfunction_class(name = "CallTracker", class_syntax)]
pub mod call_tracker {
    use super::*;

    /// Emits DEP0173 the first time a tracker is created, as `util.deprecate` does.
    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if !DEPRECATION_WARNED.replace(true) {
            let process = scope.global().get_property(scope, c"process")?;
            if let Ok(process) = Object::from_value(scope, process) {
                crate::stream::call_method(
                    scope,
                    process,
                    c"emitWarning",
                    &[
                        crate::stream::string(scope, "assert.CallTracker is deprecated.")?,
                        crate::stream::string(scope, "DeprecationWarning")?,
                        crate::stream::string(scope, "DEP0173")?,
                    ],
                )?;
            }
        }
        this.define_property(scope, c"_starlingChecks", js::Array::new(scope, 0)?, 0)?;
        let weak_map = scope.global().get_property(scope, c"WeakMap")?;
        let weak_map = Object::from_value(scope, weak_map).map_err(|e| e.throw(scope))?;
        let tracked = Function::construct(scope, weak_map, js::function::EmptyArgs)?;
        this.define_property(scope, c"_starlingTracked", tracked, 0)
    }

    /// <https://nodejs.org/api/assert.html#trackercallsfn-exact>
    pub fn calls<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        function: Option<HandleValue<'s>>,
        expected: Option<HandleValue<'s>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let process = scope.global().get_property(scope, c"process")?;
        if Object::from_value(scope, process).is_ok_and(|process| {
            process
                .get_property(scope, c"_exiting")
                .is_ok_and(|exiting| exiting.is_boolean() && exiting.to_boolean())
        }) {
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
                crate::errors::validate_integer(scope, expected, "expected", 1.0, 4294967295.0)?;
                expected.to_number()
            }
            None => 1.0,
        };
        let function = match function {
            Some(function) => function,
            None => {
                let noop = Function::new_callback(scope, c"", 0, noop, ())?;
                noop.to_jsval_throwing(scope)?
            }
        };
        let name = Object::from_value(scope, *function)
            .ok()
            .and_then(|function| function.get_property(scope, c"name").ok())
            .and_then(|name| String::from_jsval(scope, name, ()).ok())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "calls".to_string());
        let context = tracker_context(scope, expected, &name)?;
        let handler = Object::new_plain(scope)?;
        handler.set_prototype(scope, js::native::HandleObject::null())?;
        let apply = crate::stream::closure(
            scope,
            3,
            tracked_apply,
            &[context.to_jsval_throwing(scope)?],
        )?;
        handler.set_property(scope, c"apply", apply)?;
        let proxy_ctor = scope.global().get_property(scope, c"Proxy")?;
        let proxy_ctor = Object::from_value(scope, proxy_ctor)?;
        let tracked = Function::construct(
            scope,
            proxy_ctor,
            &[function, handler.to_jsval_throwing(scope)?],
        )?;
        let checks = this.get_property(scope, c"_starlingChecks")?;
        let checks = Object::from_value(scope, *checks)?;
        crate::stream::call_method(scope, checks, c"push", &[context.to_jsval_throwing(scope)?])?;
        let map = this.get_property(scope, c"_starlingTracked")?;
        let map = Object::from_value(scope, *map)?;
        crate::stream::call_method(
            scope,
            map,
            c"set",
            &[
                tracked.to_jsval_throwing(scope)?,
                context.to_jsval_throwing(scope)?,
            ],
        )?;
        Ok(tracked.to_jsval_throwing(scope)?)
    }

    /// <https://nodejs.org/api/assert.html#trackergetcallsfn>
    pub fn get_calls<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        tracked: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let context = tracked_context(scope, this, tracked)?;
        let calls = context.get_property(scope, c"calls")?;
        let calls = Object::from_value(scope, *calls)?;
        let copy = crate::stream::call_method(scope, calls, c"slice", &[])?;
        let object_ctor = scope.global().get_property(scope, c"Object")?;
        let object_ctor = Object::from_value(scope, object_ctor)?;
        Ok(crate::stream::call_method(
            scope,
            object_ctor,
            c"freeze",
            &[copy],
        )?)
    }

    /// <https://nodejs.org/api/assert.html#trackerreport>
    pub fn report<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<Object<'s>, ExnThrown> {
        let checks = this.get_property(scope, c"_starlingChecks")?;
        let checks = Object::from_value(scope, *checks).map_err(|e| e.throw(scope))?;
        let length = crate::stream::Props::num(&checks, scope, c"length")? as u32;
        let mut reports = Vec::new();
        for index in 0..length {
            let context = Object::from_value(scope, *checks.get_element(scope, index)?)
                .map_err(|e| e.throw(scope))?;
            if let Some(report) = context_report(scope, context)? {
                reports.push(report.to_jsval_throwing(scope)?);
            }
        }
        Ok(*js::Array::with_contents(scope, &reports)?)
    }

    /// <https://nodejs.org/api/assert.html#trackerresetfn>
    pub fn reset(
        scope: &Scope<'_>,
        this: Object<'_>,
        tracked: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let reset = |context: Object<'_>| -> Result<(), ExnThrown> {
            context.set_property(scope, c"calls", js::Array::new(scope, 0)?)
        };
        match tracked.filter(|tracked| !tracked.is_undefined()) {
            Some(tracked) => reset(tracked_context(scope, this, tracked)?)?,
            None => {
                let checks = this.get_property(scope, c"_starlingChecks")?;
                let checks = Object::from_value(scope, *checks)?;
                let length = crate::stream::Props::num(&checks, scope, c"length")? as u32;
                for index in 0..length {
                    reset(Object::from_value(
                        scope,
                        *checks.get_element(scope, index)?,
                    )?)?;
                }
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/assert.html#trackerverify>
    pub fn verify(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let reports = report(scope, this)?;
        let length = crate::stream::Props::num(&reports, scope, c"length")? as u32;
        if length == 0 {
            return Ok(());
        }
        let message = if length == 1 {
            let first = Object::from_value(scope, *reports.get_element(scope, 0)?)?;
            first.get_property(scope, c"message")?
        } else {
            crate::stream::string(
                scope,
                "Functions were not called the expected number of times",
            )?
        };
        let options = Object::new_plain(scope)?;
        options.set_property(scope, c"message", message)?;
        let class = assertion_error::class_constructor(scope)?;
        let error = Function::construct(scope, class, &[options.to_jsval_throwing(scope)?])?;
        for name in [c"actual", c"expected", c"operator"] {
            error.set_property(scope, name, js::value::undefined())?;
        }
        for index in 0..length {
            let report = Object::from_value(scope, *reports.get_element(scope, index)?)?;
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
        js::exception::set_pending(
            scope,
            error.to_jsval_throwing(scope)?,
            js::native::ExceptionStackBehavior::DoNotCapture,
        );
        Err(Thrown::Pending)
    }
}

/// The context of the function `tracked` that `CallTracker` `this` tracks, or
/// `ERR_INVALID_ARG_VALUE` if it tracks no such function.
fn tracked_context<'s>(
    scope: &'s Scope<'_>,
    this: Object<'_>,
    tracked: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let map = this.get_property(scope, c"_starlingTracked")?;
    let map = Object::from_value(scope, *map)?;
    let context = crate::stream::call_method(scope, map, c"get", &[tracked])?;
    Object::from_value(scope, *context).map_err(|_| {
        NodeError::invalid_arg_value(scope, "tracked", tracked, "is not a tracked function").into()
    })
}

/// The function `CallTracker.calls` tracks when given none.
fn noop(
    _scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    Ok(js::value::undefined())
}

js::instance_local! {
    /// Set once a `CallTracker` was created, and DEP0173 emitted.
    static DEPRECATION_WARNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[core_runtime::jsmodule(node_args, name = "node:assert", default = "ok")]
pub mod module {
    use super::*;

    pub use super::assertion_error as AssertionError;
    pub use super::call_tracker as CallTracker;

    /// <https://nodejs.org/api/assert.html#assertokvalue-message>
    pub fn ok(
        scope: &Scope<'_>,
        value: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_ok(scope, value, message)
    }

    /// <https://nodejs.org/api/assert.html#assertequalactual-expected-message>
    pub fn equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, false, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotequalactual-expected-message>
    pub fn not_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, false, true)
    }

    /// <https://nodejs.org/api/assert.html#assertstrictequalactual-expected-message>
    pub fn strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotstrictequalactual-expected-message>
    pub fn not_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdeepequalactual-expected-message>
    pub fn deep_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, false, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotdeepequalactual-expected-message>
    pub fn not_deep_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, false, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdeepstrictequalactual-expected-message>
    pub fn deep_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotdeepstrictequalactual-expected-message>
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
    pub fn fail(scope: &Scope<'_>, message: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        assert_fail(scope, message)
    }

    /// <https://nodejs.org/api/assert.html#assertmatchstring-regexp-message>
    pub fn r#match(
        scope: &Scope<'_>,
        string: HandleValue<'_>,
        regexp: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_match(scope, string, regexp, message, false)
    }

    /// <https://nodejs.org/api/assert.html#assertdoesnotmatchstring-regexp-message>
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

#[core_runtime::jsmodule(node_args, name = "node:assert/strict", default = "ok")]
pub mod strict {
    use super::*;

    pub use super::assertion_error as AssertionError;
    pub use super::call_tracker as CallTracker;

    /// <https://nodejs.org/api/assert.html#assertokvalue-message>
    pub fn ok(
        scope: &Scope<'_>,
        value: Option<HandleValue<'_>>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_ok(scope, value, message)
    }

    /// <https://nodejs.org/api/assert.html#assertequalactual-expected-message>
    pub fn equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotequalactual-expected-message>
    pub fn not_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertstrictequalactual-expected-message>
    pub fn strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotstrictequalactual-expected-message>
    pub fn not_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdeepequalactual-expected-message>
    pub fn deep_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotdeepequalactual-expected-message>
    pub fn not_deep_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, true)
    }

    /// <https://nodejs.org/api/assert.html#assertdeepstrictequalactual-expected-message>
    pub fn deep_strict_equal(
        scope: &Scope<'_>,
        actual: HandleValue<'_>,
        expected: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_deep_equal(scope, actual, expected, message, true, false)
    }

    /// <https://nodejs.org/api/assert.html#assertnotdeepstrictequalactual-expected-message>
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
    pub fn fail(scope: &Scope<'_>, message: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        assert_fail(scope, message)
    }

    /// <https://nodejs.org/api/assert.html#assertmatchstring-regexp-message>
    pub fn r#match(
        scope: &Scope<'_>,
        string: HandleValue<'_>,
        regexp: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        check_match(scope, string, regexp, message, false)
    }

    /// <https://nodejs.org/api/assert.html#assertdoesnotmatchstring-regexp-message>
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

/// Register `node:assert`, whose `strict` is `node:assert/strict`'s `default` export, which gets
/// itself as its `strict` property.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let strict = || -> Result<HandleValue<'_>, ExnThrown> {
        let strict = core_runtime::module::builtin_namespace(scope, "node:assert/strict")?
            .ok_or(ExnThrown)?
            .get_property(scope, c"default")?;
        if let Ok(object) = Object::from_value(scope, *strict) {
            object.set_property(scope, c"strict", strict)?;
        }
        Ok(strict)
    };
    let Ok(strict) = strict() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &[("strict", strict)])
}
