// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:util` and `node:util/types`. See <https://nodejs.org/api/util.html>.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{
    out_of_range_received, validate_function, validate_object, validate_plain_object,
    validate_string, ErrorKind, NodeError, Thrown,
};
use crate::inspect::{self, InspectOptions};

/// The symbol a function's promisified version is stored under,
/// `Symbol.for('nodejs.util.promisify.custom')`.
const PROMISIFY_CUSTOM: &str = "nodejs.util.promisify.custom";

js::instance_local! {
    /// The deprecation codes `deprecate` has emitted a warning for.
    static CODES_WARNED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// The body of `util.inspect(value, options)`. A boolean `options` is `showHidden`, and a third
/// and fourth argument other than `undefined` are `depth` and `colors`, as in the legacy
/// `inspect(value, showHidden, depth, colors)` form.
fn inspect_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let mut options = InspectOptions::current(scope)?;
    if args.len() > 2 && !args.get(2).is_undefined() {
        options.set(scope, c"depth", args.get(2))?;
    }
    if args.len() > 3 && !args.get(3).is_undefined() {
        options.set(scope, c"colors", args.get(3))?;
    }
    if args.get(1).is_boolean() {
        options.show_hidden = args.get(1).to_boolean();
    } else if let Ok(given) = Object::from_value(scope, *args.get(1)) {
        options = options.read(scope, given)?;
    }
    let text = inspect::inspect(scope, args.get(0), options)?;
    text.as_str().to_jsval_raw_throwing(scope)
}

/// The getter of `util.inspect.defaultOptions`, whose payload is the options object.
fn default_options_get(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(payload.get())
}

/// The setter of `util.inspect.defaultOptions`, whose payload is the options object: copy the
/// properties of the object assigned onto it.
fn default_options_set(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<(), Thrown> {
        let options = validate_object(scope, args.get(0), "options")?;
        let defaults = Object::from_value(scope, *payload)?;
        defaults.assign(scope, options)?;
        Ok(())
    };
    run().map_err(|error| error.throw(scope))?;
    Ok(js::value::undefined())
}

/// The body of a promisified function, whose payload is the original function: call it with the
/// arguments and a callback that settles the returned promise. A promise returned by the original
/// function emits the `DEP0174` warning.
fn promisified(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    let settle = Function::new_callback(scope, c"callback", 2, settle_promise, *promise)?;
    let mut call_args = crate::callback_args(&args);
    call_args.push((*settle).to_jsval_throwing(scope)?);
    match Function::call(scope, args.this(), payload, &call_args) {
        Ok(result) => {
            if Object::from_value(scope, *result)
                .is_ok_and(|result| js::Promise::is_promise(result.handle()))
            {
                emit_warning(
                    scope,
                    "Calling promisify on a function that returns a Promise is likely a mistake.",
                    "DEP0174",
                )?;
            }
        }
        Err(ExnThrown) => promise.reject_with_pending(scope)?,
    }
    promise.to_jsval_raw_throwing(scope)
}

/// Call `process.emitWarning(message, 'DeprecationWarning', code)`.
fn emit_warning(scope: &Scope<'_>, message: &str, code: &str) -> Result<(), ExnThrown> {
    let Ok(process) = Object::from_value(scope, *scope.global().get_property(scope, c"process")?)
    else {
        return Ok(());
    };
    Function::call_by_name(
        scope,
        process.handle(),
        c"emitWarning",
        &[message, "DeprecationWarning", code],
    )?;
    Ok(())
}

/// The callback a promisified function passes on, whose payload is the promise: reject it with a
/// truthy first argument, and resolve it with the second argument otherwise.
fn settle_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = js::function::cast_payload::<js::Promise>(scope, payload);
    let error = args.get(0);
    if bool::from_jsval(scope, error, ()).unwrap_or(false) {
        promise.reject(scope, error)?;
    } else {
        promise.resolve(scope, args.get(1))?;
    }
    Ok(js::value::undefined())
}

/// `Object.defineProperties(target, Object.getOwnPropertyDescriptors(source))`, with each
/// descriptor passed to `adjust` first.
fn copy_descriptors(
    scope: &Scope<'_>,
    target: Object<'_>,
    source: Object<'_>,
    adjust: impl Fn(&Scope<'_>, &str, Object<'_>) -> Result<(), ExnThrown>,
) -> Result<(), ExnThrown> {
    let object = Object::from_value(scope, *scope.global().get_property(scope, c"Object")?)
        .map_err(|e| e.throw(scope))?;
    let descriptors = Function::call_by_name(
        scope,
        object.handle(),
        c"getOwnPropertyDescriptors",
        &[source.to_jsval_throwing(scope)?],
    )?;
    let descriptors = Object::from_value(scope, *descriptors).map_err(|e| e.throw(scope))?;
    for key in descriptors.own_keys(scope)? {
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        let descriptor = descriptors.get_property_by_id(scope, id)?;
        let descriptor = Object::from_value(scope, *descriptor).map_err(|e| e.throw(scope))?;
        descriptor.set_prototype(scope, js::native::HandleObject::null())?;
        if !key.is_symbol() {
            let name = String::from_jsval(scope, key, ()).map_err(|_| ExnThrown)?;
            adjust(scope, &name, descriptor)?;
        }
    }
    Function::call_by_name(
        scope,
        object.handle(),
        c"defineProperties",
        &[
            target.to_jsval_throwing(scope)?,
            descriptors.to_jsval_throwing(scope)?,
        ],
    )?;
    Ok(())
}

/// Define `function[symbol]` as `value`, non-enumerable and non-writable.
fn define_custom(
    scope: &Scope<'_>,
    function: Object<'_>,
    symbol: HandleValue<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let id = scope.root_id(js::id::value_to_id(scope, symbol)?);
    function.define_value_by_id(scope, id, value, js::class_spec::JSPROP_READONLY as u32)
}

/// The body of `util.promisify(original)`.
fn promisify_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<Value, Thrown> {
        let original = validate_function(scope, args.get(0), "original")?;
        let custom = crate::symbol(scope, PROMISIFY_CUSTOM, true)?;
        let custom_id = scope.root_id(js::id::value_to_id(scope, custom)?);
        let existing = original.get_property_by_id(scope, custom_id)?;
        if bool::from_jsval(scope, existing, ()).unwrap_or(false) {
            let existing = validate_function(scope, existing, "util.promisify.custom")?;
            define_custom(scope, existing, custom, existing.to_jsval_throwing(scope)?)?;
            return Ok(existing.to_jsval_raw_throwing(scope)?);
        }
        let function = Function::new_callback(scope, c"fn", 0, promisified, original)?;
        if let Some(proto) = original.get_prototype(scope)? {
            function.set_prototype(scope, proto.handle())?;
        }
        define_custom(scope, *function, custom, function.to_jsval_throwing(scope)?)?;
        copy_descriptors(scope, *function, original, |_, _, _| Ok(()))?;
        Ok(function.to_jsval_raw_throwing(scope)?)
    };
    run().map_err(|error| error.throw(scope))
}

/// The body of a callbackified function, whose payload is the original async function: call it
/// with all arguments but the last, a callback called with the outcome.
fn callbackified(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<Value, Thrown> {
        let mut call_args = crate::callback_args(&args);
        let callback = call_args
            .pop()
            .unwrap_or(scope.root_value(js::value::undefined()));
        let callback = validate_function(scope, callback, "last argument")?;
        let result = Function::call(scope, args.this(), payload, &call_args)?;
        let promise_ctor =
            Object::from_value(scope, scope.global().get_property(scope, c"Promise")?)?;
        let promise = Function::call_by_name(scope, promise_ctor.handle(), c"resolve", &[result])?;
        let promise = Object::from_value(scope, promise)?;
        let on_fulfilled = Function::new_callback(scope, c"", 1, callbackify_fulfilled, callback)?;
        let on_rejected = Function::new_callback(scope, c"", 1, callbackify_rejected, callback)?;
        Function::call_by_name(
            scope,
            promise.handle(),
            c"then",
            &[*on_fulfilled, *on_rejected],
        )?;
        Ok(js::value::undefined())
    };
    run().map_err(|error| error.throw(scope))
}

fn callbackify_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let null = scope.root_value(js::value::null());
    Function::call(scope, js::value::undefined(), payload, &[null, args.get(0)])?;
    Ok(js::value::undefined())
}

fn callbackify_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let mut reason = args.get(0);
    // We wrap a falsy rejection reason in an error, because a falsy first callback argument
    // reports success.
    if !bool::from_jsval(scope, reason, ()).unwrap_or(false) {
        let error = NodeError::new(
            ErrorKind::Error,
            "ERR_FALSY_VALUE_REJECTION",
            "Promise was rejected with falsy value",
        )
        .throw(scope);
        let wrapped = js::exception::take_pending(scope).ok_or(error)?;
        if let Ok(object) = Object::from_value(scope, wrapped) {
            object.set_property(scope, c"reason", reason)?;
        }
        reason = wrapped;
    }
    Function::call(scope, js::value::undefined(), payload, &[reason])?;
    Ok(js::value::undefined())
}

/// The body of a deprecated function, whose payload is `[function, message, code, warned]`:
/// emit the deprecation warning the first time, unless `process.noDeprecation` is set or another
/// function deprecated with the same code has emitted it, then call the function.
fn deprecated(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, *payload).map_err(|e| e.throw(scope))?;
    let process = Object::from_value(scope, *scope.global().get_property(scope, c"process")?);
    let silenced = match &process {
        Ok(process) => bool::from_jsval(scope, process.get_property(scope, c"noDeprecation")?, ())
            .unwrap_or(false),
        Err(_) => true,
    };
    let warned = state.get_element(scope, 3)?;
    if !silenced && !(warned.is_boolean() && warned.to_boolean()) {
        state.set_element(scope, 3, true)?;
        let message = state.get_element(scope, 1)?;
        let kind =
            scope.root_value(js::JSString::from_str(scope, "DeprecationWarning")?.as_value());
        let code = state.get_element(scope, 2)?;
        let first = if code.is_undefined() {
            true
        } else {
            let code = String::from_jsval(scope, code, ()).map_err(|_| ExnThrown)?;
            CODES_WARNED.with(|codes| codes.borrow_mut().insert(code))
        };
        if let (true, Ok(process)) = (first, process) {
            if code.is_undefined() {
                Function::call_by_name(scope, process.handle(), c"emitWarning", &[message, kind])?;
            } else {
                Function::call_by_name(
                    scope,
                    process.handle(),
                    c"emitWarning",
                    &[message, kind, code],
                )?;
            }
        }
    }
    let function = state.get_element(scope, 0)?;
    let call_args = crate::callback_args(&args);
    if args.is_constructing() {
        return Function::construct(scope, function, &call_args)?.to_jsval_raw_throwing(scope);
    }
    Ok(Function::call(scope, args.this(), function, &call_args)?.get())
}

/// Whether the `NODE_DEBUG` environment variable enables `section`: it lists sections separated
/// by commas or spaces, case-insensitively, with `*` matching any characters.
fn debug_enabled(section: &str) -> bool {
    let Some(spec) = platform::process::env_vars()
        .into_iter()
        .find(|(key, _)| key == "NODE_DEBUG")
        .map(|(_, value)| value)
    else {
        return false;
    };
    let section = section.to_ascii_lowercase();
    spec.split([',', ' '])
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| glob_matches(&pattern.to_ascii_lowercase(), &section))
}

/// Whether `text` matches `pattern`, in which `*` matches any characters.
fn glob_matches(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((prefix, rest)) => {
            let Some(text) = text.strip_prefix(prefix) else {
                return false;
            };
            (0..=text.len())
                .any(|start| text.is_char_boundary(start) && glob_matches(rest, &text[start..]))
        }
    }
}

/// The body of a `debuglog` logger, whose payload is the section's name: print the formatted
/// arguments to stderr as `SECTION PID: message`.
fn debug_log(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let section = String::from_jsval(scope, payload, ()).unwrap_or_default();
    let message = inspect::format(
        scope,
        &crate::callback_args(&args),
        &InspectOptions::default(),
    )?;
    // A wasm instance has no process ID, so we print 1 there.
    let pid = if cfg!(target_arch = "wasm32") {
        1
    } else {
        std::process::id()
    };
    eprintln!("{} {pid}: {message}", section.to_ascii_uppercase());
    Ok(js::value::undefined())
}

/// A function that does nothing, the logger of a disabled `debuglog` section.
fn do_nothing(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// The style names `util.inspect.colors` holds, in its property order.
const STYLE_NAMES: &[&str] = &[
    "reset",
    "bold",
    "dim",
    "italic",
    "underline",
    "blink",
    "inverse",
    "hidden",
    "strikethrough",
    "doubleunderline",
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bgBlack",
    "bgRed",
    "bgGreen",
    "bgYellow",
    "bgBlue",
    "bgMagenta",
    "bgCyan",
    "bgWhite",
    "framed",
    "overlined",
    "gray",
    "redBright",
    "greenBright",
    "yellowBright",
    "blueBright",
    "magentaBright",
    "cyanBright",
    "whiteBright",
    "bgGray",
    "bgRedBright",
    "bgGreenBright",
    "bgYellowBright",
    "bgBlueBright",
    "bgMagentaBright",
    "bgCyanBright",
    "bgWhiteBright",
    "grey",
    "blackBright",
    "bgGrey",
    "bgBlackBright",
    "faint",
    "crossedout",
    "strikeThrough",
    "crossedOut",
    "conceal",
    "swapColors",
    "swapcolors",
    "doubleUnderline",
];

/// The ANSI escape codes `styleText` applies for `name`: the opening and closing code.
fn style_codes(name: &str) -> Option<(u8, u8)> {
    Some(match name {
        "reset" => (0, 0),
        "bold" => (1, 22),
        "dim" | "faint" => (2, 22),
        "italic" => (3, 23),
        "underline" => (4, 24),
        "blink" => (5, 25),
        "inverse" | "swapColors" | "swapcolors" => (7, 27),
        "hidden" | "conceal" => (8, 28),
        "strikethrough" | "crossedout" | "strikeThrough" | "crossedOut" => (9, 29),
        "doubleunderline" | "doubleUnderline" => (21, 24),
        "framed" => (51, 54),
        "overlined" => (53, 55),
        "black" => (30, 39),
        "red" => (31, 39),
        "green" => (32, 39),
        "yellow" => (33, 39),
        "blue" => (34, 39),
        "magenta" => (35, 39),
        "cyan" => (36, 39),
        "white" => (37, 39),
        "gray" | "grey" | "blackBright" => (90, 39),
        "redBright" => (91, 39),
        "greenBright" => (92, 39),
        "yellowBright" => (93, 39),
        "blueBright" => (94, 39),
        "magentaBright" => (95, 39),
        "cyanBright" => (96, 39),
        "whiteBright" => (97, 39),
        "bgBlack" => (40, 49),
        "bgRed" => (41, 49),
        "bgGreen" => (42, 49),
        "bgYellow" => (43, 49),
        "bgBlue" => (44, 49),
        "bgMagenta" => (45, 49),
        "bgCyan" => (46, 49),
        "bgWhite" => (47, 49),
        "bgGray" | "bgGrey" | "bgBlackBright" => (100, 49),
        "bgRedBright" => (101, 49),
        "bgGreenBright" => (102, 49),
        "bgYellowBright" => (103, 49),
        "bgBlueBright" => (104, 49),
        "bgMagentaBright" => (105, 49),
        "bgCyanBright" => (106, 49),
        "bgWhiteBright" => (107, 49),
        _ => return None,
    })
}

/// The length of the escape sequence Node.js's `ansi` pattern matches at `start` of `chars`, if
/// any: an OSC sequence (`ESC ]` up to BEL, `ESC \` or `0x9c`), or `ESC` or `0x9b` followed by
/// any of `[]()#;?`, optional numeric parameters and a final character.
fn ansi_sequence_at(chars: &[char], start: usize) -> Option<usize> {
    let introducer = chars[start];
    if introducer == '\u{1b}' && chars.get(start + 1) == Some(&']') {
        let mut index = start + 2;
        while index < chars.len() {
            match chars[index] {
                '\u{7}' | '\u{9c}' => return Some(index + 1),
                '\u{1b}' if chars.get(index + 1) == Some(&'\\') => return Some(index + 2),
                _ => index += 1,
            }
        }
    }
    if introducer != '\u{1b}' && introducer != '\u{9b}' {
        return None;
    }
    let is_final = |index: usize| -> Option<usize> {
        let c = *chars.get(index)?;
        let matches = c.is_ascii_digit()
            || ('A'..='P').contains(&c)
            || ('R'..='T').contains(&c)
            || c == 'Z'
            || c == 'c'
            || ('f'..='n').contains(&c)
            || ('q'..='u').contains(&c)
            || "y=><~".contains(c);
        matches.then_some(index + 1)
    };
    let mut prefix_end = start + 1;
    while prefix_end < chars.len() && "[]()#;?".contains(chars[prefix_end]) {
        prefix_end += 1;
    }
    for parameters in (start + 1..=prefix_end).rev() {
        if let Some(end) = ansi_parameters(chars, parameters, &is_final) {
            return Some(end);
        }
        if let Some(end) = is_final(parameters) {
            return Some(end);
        }
    }
    None
}

/// The end of `\d{1,4}(?:[;:]\d{0,4})*` at `start` followed by a character `is_final` matches,
/// at the end a backtracking regular expression engine finds, or `None` if `start` holds no
/// digit. `is_final` returns the index after the character it matches, and matches every digit.
fn ansi_parameters(
    chars: &[char],
    start: usize,
    is_final: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    let digits = |from: usize| {
        let mut count = 0;
        while count < 4 && chars.get(from + count).is_some_and(char::is_ascii_digit) {
            count += 1;
        }
        count
    };
    let first = digits(start);
    if first == 0 {
        return None;
    }
    let mut at = start + first;
    let mut last_digit_end = at;
    while matches!(chars.get(at), Some(';' | ':')) {
        let count = digits(at + 1);
        at += 1 + count;
        if count > 0 {
            last_digit_end = at;
        }
    }
    // If the character after the longest repetition isn't final, the match ends after the last
    // digit, which backtracking matches as the final character.
    Some(is_final(at).unwrap_or(last_digit_end))
}

/// `text` without ANSI escape sequences, as Node.js's `stripVTControlCharacters` removes them.
pub fn strip_vt_control_characters(text: &str) -> String {
    if !text.contains(['\u{1b}', '\u{9b}']) {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        match ansi_sequence_at(&chars, index) {
            Some(end) => index = end,
            None => {
                out.push(chars[index]);
                index += 1;
            }
        }
    }
    out
}

/// Node.js's `replaceCloseCode`: each `close` in `text` that doesn't end it replaced by `open`,
/// or by `close` and `open` if `keep_close`.
fn replace_close_code(text: &str, close: &str, open: &str, keep_close: bool) -> String {
    let replacement = if keep_close {
        format!("{close}{open}")
    } else {
        open.to_string()
    };
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    while let Some(offset) = text[last..].find(close) {
        let index = last + offset;
        let after = index + close.len();
        if after >= text.len() {
            break;
        }
        out.push_str(&text[last..index]);
        out.push_str(&replacement);
        last = after;
    }
    out.push_str(&text[last..]);
    out
}

/// The red, green and blue components of a `#RGB` or `#RRGGBB` color, if `text` is one.
fn hex_color(text: &str) -> Option<(u8, u8, u8)> {
    let digits = text.strip_prefix('#')?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let component = |hex: &str| u8::from_str_radix(hex, 16).ok();
    match digits.len() {
        3 => {
            let double = |index: usize| component(&digits[index..=index].repeat(2));
            Some((double(0)?, double(1)?, double(2)?))
        }
        6 => Some((
            component(&digits[0..2])?,
            component(&digits[2..4])?,
            component(&digits[4..6])?,
        )),
        _ => None,
    }
}

/// Whether `value` is an object that `check` accepts.
fn object_is(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    check: impl FnOnce(Object<'_>) -> Result<bool, ExnThrown>,
) -> Result<bool, ExnThrown> {
    match Object::from_value(scope, *value) {
        Ok(object) => check(object),
        Err(_) => Ok(false),
    }
}

/// Whether `value` is an object whose builtin tag is one of `tags`, with an own
/// `Symbol.toStringTag` disregarded.
fn has_tag(scope: &Scope<'_>, value: HandleValue<'_>, tags: &[&str]) -> Result<bool, ExnThrown> {
    object_is(scope, value, |object| {
        inspect::has_builtin_tag(scope, object, tags)
    })
}

/// Whether `value` is a boxed primitive of `kind`, or of any kind for `None`.
fn is_boxed(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    kind: Option<inspect::Boxed>,
) -> Result<bool, ExnThrown> {
    object_is(scope, value, |object| {
        Ok(match inspect::boxed_kind(scope, object)? {
            Some(found) => kind.is_none_or(|kind| kind == found),
            None => false,
        })
    })
}

/// The view `value` is, if it is one.
fn view_of<'s>(scope: &'s Scope<'_>, value: HandleValue<'_>) -> Option<js::ArrayBufferView<'s>> {
    Object::from_value(scope, *value)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
}

/// Whether `value` is a typed array of the kind named `name`.
fn is_typed_array_of(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> bool {
    view_of(scope, value)
        .is_some_and(|view| view.view_kind().is_typed_array() && view.view_kind().name() == name)
}

#[core_runtime::jsmodule(node_args, name = "node:util/types", default_namespace)]
pub mod types {
    use super::*;

    /// <https://nodejs.org/api/util.html#utiltypesisanyarraybuffervalue>
    pub fn is_any_array_buffer(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(inspect::is_array_buffer(object) || inspect::is_shared_array_buffer(scope, object)?)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisarraybuffervalue>
    pub fn is_array_buffer(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| Ok(inspect::is_array_buffer(object)))
    }
    /// <https://nodejs.org/api/util.html#utiltypesissharedarraybuffervalue>
    pub fn is_shared_array_buffer(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            inspect::is_shared_array_buffer(scope, object)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisarraybufferviewvalue>
    pub fn is_array_buffer_view(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        view_of(scope, value).is_some()
    }
    /// <https://nodejs.org/api/util.html#utiltypesistypedarrayvalue>
    pub fn is_typed_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        view_of(scope, value).is_some_and(|view| view.view_kind().is_typed_array())
    }
    /// <https://nodejs.org/api/util.html#utiltypesisdataviewvalue>
    pub fn is_data_view(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        view_of(scope, value).is_some_and(|view| !view.view_kind().is_typed_array())
    }
    /// <https://nodejs.org/api/util.html#utiltypesisuint8arrayvalue>
    pub fn is_uint8_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "Uint8Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisuint8clampedarrayvalue>
    pub fn is_uint8_clamped_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "Uint8ClampedArray")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisuint16arrayvalue>
    pub fn is_uint16_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "Uint16Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisuint32arrayvalue>
    pub fn is_uint32_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "Uint32Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisint8arrayvalue>
    pub fn is_int8_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "Int8Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisint16arrayvalue>
    pub fn is_int16_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "Int16Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisint32arrayvalue>
    pub fn is_int32_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "Int32Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisfloat32arrayvalue>
    pub fn is_float32_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "Float32Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisfloat64arrayvalue>
    pub fn is_float64_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "Float64Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisbigint64arrayvalue>
    pub fn is_big_int64_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "BigInt64Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisbiguint64arrayvalue>
    pub fn is_big_uint64_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, "BigUint64Array")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisargumentsobjectvalue>
    pub fn is_arguments_object(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_arguments(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisasyncfunctionvalue>
    pub fn is_async_function(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(object.is_callable()
                && inspect::has_builtin_tag(
                    scope,
                    object,
                    &["AsyncFunction", "AsyncGeneratorFunction"],
                )?)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisgeneratorfunctionvalue>
    pub fn is_generator_function(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(object.is_callable()
                && inspect::has_builtin_tag(
                    scope,
                    object,
                    &["GeneratorFunction", "AsyncGeneratorFunction"],
                )?)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisgeneratorobjectvalue>
    pub fn is_generator_object(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        has_tag(scope, value, &["Generator", "AsyncGenerator"])
    }
    /// <https://nodejs.org/api/util.html#utiltypesisdatevalue>
    pub fn is_date(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_date(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisregexpvalue>
    pub fn is_reg_exp(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_regexp(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesismapvalue>
    pub fn is_map(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_map(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesissetvalue>
    pub fn is_set(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_set(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisweakmapvalue>
    pub fn is_weak_map(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_weak_map(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisweaksetvalue>
    pub fn is_weak_set(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_weak_set(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesismapiteratorvalue>
    pub fn is_map_iterator(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        has_tag(scope, value, &["Map Iterator"])
    }
    /// <https://nodejs.org/api/util.html#utiltypesissetiteratorvalue>
    pub fn is_set_iterator(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        has_tag(scope, value, &["Set Iterator"])
    }
    /// <https://nodejs.org/api/util.html#utiltypesispromisevalue>
    pub fn is_promise(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        Object::from_value(scope, *value)
            .is_ok_and(|object| js::Promise::is_promise(object.handle()))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisnativeerrorvalue>
    pub fn is_native_error(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        value.is_object() && js::exception::is_error_object(scope, value)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisnumberobjectvalue>
    pub fn is_number_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::Number))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisstringobjectvalue>
    pub fn is_string_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::String))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisbooleanobjectvalue>
    pub fn is_boolean_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::Boolean))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisbigintobjectvalue>
    pub fn is_big_int_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::BigInt))
    }
    /// <https://nodejs.org/api/util.html#utiltypesissymbolobjectvalue>
    pub fn is_symbol_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::Symbol))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisboxedprimitivevalue>
    pub fn is_boxed_primitive(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, None)
    }
    /// <https://nodejs.org/api/util.html#utiltypesismodulenamespaceobjectvalue>
    pub fn is_module_namespace_object(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(object.get_prototype(scope)?.is_none()
                && !object.is_extensible(scope)?
                && inspect::has_builtin_tag(scope, object, &["Module"])?)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisproxyvalue>: always `false`, as the `js` crate
    /// has no proxy check.
    pub fn is_proxy() -> bool {
        false
    }
    /// <https://nodejs.org/api/util.html#utiltypesisexternalvalue>: there are no external values,
    /// so always `false`.
    pub fn is_external() -> bool {
        false
    }
    /// <https://nodejs.org/api/util.html#utiltypesiskeyobjectvalue>: `false`, as `node:crypto`
    /// key objects aren't implemented.
    pub fn is_key_object() -> bool {
        false
    }
    /// <https://nodejs.org/api/util.html#utiltypesiscryptokeyvalue>: `false`, as Web Crypto keys
    /// aren't implemented.
    pub fn is_crypto_key() -> bool {
        false
    }
}

#[core_runtime::jsmodule(node_args, name = "node:util", default_namespace)]
pub mod module {
    use super::*;

    /// <https://nodejs.org/api/util.html#utilformatformat-args>
    pub fn format(scope: &Scope<'_>, args: RestArgs<HandleValue<'_>>) -> Result<String, ExnThrown> {
        inspect::format(scope, &args, &InspectOptions::current(scope)?)
    }

    /// <https://nodejs.org/api/util.html#utilformatwithoptionsinspectoptions-format-args>
    pub fn format_with_options(
        scope: &Scope<'_>,
        options: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<String, Thrown> {
        let options = validate_plain_object(scope, options, "inspectOptions", true)?;
        let options = InspectOptions::current(scope)?.read(scope, options)?;
        Ok(inspect::format(scope, &args, &options)?)
    }

    /// <https://nodejs.org/api/util.html#utilisdeepstrictequalval1-val2-options>: a truthy
    /// `skipPrototype` leaves prototypes uncompared.
    pub fn is_deep_strict_equal(
        scope: &Scope<'_>,
        a: HandleValue<'_>,
        b: HandleValue<'_>,
        skip_prototype: Option<HandleValue<'_>>,
    ) -> Result<bool, ExnThrown> {
        let skip =
            skip_prototype.is_some_and(|skip| bool::from_jsval(scope, skip, ()).unwrap_or(false));
        let mode = if skip {
            inspect::Mode::StrictWithoutPrototypes
        } else {
            inspect::Mode::Strict
        };
        inspect::deep_equal(scope, a, b, mode)
    }

    /// <https://nodejs.org/api/util.html#utilinheritsconstructor-superconstructor>
    pub fn inherits(
        scope: &Scope<'_>,
        ctor: HandleValue<'_>,
        super_ctor: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let ctor = validate_function(scope, ctor, "ctor")?;
        let super_ctor = validate_function(scope, super_ctor, "superCtor")?;
        let super_proto = super_ctor.get_property(scope, c"prototype")?;
        let super_proto = Object::from_value(scope, super_proto).map_err(|_| {
            NodeError::invalid_arg_type(scope, "superCtor.prototype", "of type object", super_proto)
        })?;
        ctor.set_property(scope, c"super_", super_ctor)?;
        let proto = Object::from_value(scope, ctor.get_property(scope, c"prototype")?)?;
        proto.set_prototype(scope, super_proto.handle())?;
        Ok(())
    }

    /// <https://nodejs.org/api/util.html#utilcallbackifyoriginal>
    pub fn callbackify<'s>(
        scope: &'s Scope<'_>,
        original: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let original = validate_function(scope, original, "original")?;
        let function = Function::new_callback(scope, c"callbackified", 0, callbackified, original)?;
        copy_descriptors(scope, *function, original, |scope, name, descriptor| {
            let value = descriptor.get_property(scope, c"value")?;
            if name == "length" && value.is_number() {
                descriptor.set_property(scope, c"value", value.to_number() + 1.0)?;
            } else if name == "name" && value.is_string() {
                let name = String::from_jsval(scope, value, ()).map_err(|_| ExnThrown)?;
                descriptor.set_property(
                    scope,
                    c"value",
                    format!("{name}Callbackified").as_str(),
                )?;
            }
            Ok(())
        })?;
        Ok(*function)
    }

    /// <https://nodejs.org/api/util.html#utilgetcallsitesframecount-options>: the `frameCount`
    /// innermost frames of the calling code, 10 by default. `scriptId` is always `"0"`.
    pub fn get_call_sites<'s>(
        scope: &'s Scope<'_>,
        frame_count: Option<HandleValue<'_>>,
    ) -> Result<js::Array<'s>, Thrown> {
        let count = match frame_count.filter(|count| count.is_number()) {
            Some(count) => {
                let count = count.to_number();
                if !(1.0..=200.0).contains(&count) || count.fract() != 0.0 {
                    return Err(NodeError::out_of_range(
                        "frameCount",
                        ">= 1 && <= 200",
                        &count.to_string(),
                    )
                    .into());
                }
                count as usize
            }
            None => 10,
        };
        let error_class = scope.global().get_property(scope, c"Error")?;
        let error_class =
            Object::from_value(scope, error_class).map_err(|e| Thrown::from(e.throw(scope)))?;
        let error = Function::construct(scope, error_class, &[] as &[HandleValue<'_>])?;
        let stack = crate::stack_trace::without_hook(|| error.get_property(scope, c"stack"))?;
        let stack = String::from_jsval(scope, stack, ()).unwrap_or_default();
        let mut sites = Vec::new();
        for line in stack.lines().filter(|line| !line.is_empty()).take(count) {
            let (name, location) = line.split_once('@').unwrap_or(("", line));
            let mut parts = location.rsplitn(3, ':');
            let column = parts
                .next()
                .and_then(|column| column.parse::<f64>().ok())
                .unwrap_or(0.0);
            let line_number = parts
                .next()
                .and_then(|line| line.parse::<f64>().ok())
                .unwrap_or(0.0);
            let script = parts.next().unwrap_or_default();
            let site = Object::new_plain(scope)?;
            site.set_property(scope, c"functionName", name)?;
            site.set_property(scope, c"scriptId", "0")?;
            site.set_property(scope, c"scriptName", script)?;
            site.set_property(scope, c"lineNumber", line_number)?;
            site.set_property(scope, c"columnNumber", column)?;
            site.set_property(scope, c"column", column)?;
            sites.push(site.to_jsval_throwing(scope)?);
        }
        Ok(js::Array::with_contents(scope, &sites)?)
    }

    /// <https://nodejs.org/api/util.html#utildeprecatefn-msg-code-options>
    pub fn deprecate<'s>(
        scope: &'s Scope<'_>,
        function: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
        code: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let function = validate_function(scope, function, "fn")?;
        if let Some(code) = code.filter(|code| !code.is_undefined()) {
            validate_string(scope, code, "code")?;
        }
        let state = js::Array::with_contents(
            scope,
            &[
                function.to_jsval_raw_throwing(scope)?,
                message.map_or(js::value::undefined(), |message| message.get()),
                code.map_or(js::value::undefined(), |code| code.get()),
                js::value::from_bool(false),
            ],
        )?;
        let wrapper =
            Function::new_constructor_callback(scope, c"deprecated", 0, deprecated, *state)?;
        wrapper.set_prototype(scope, function.handle())?;
        let prototype = function.get_property(scope, c"prototype")?;
        if bool::from_jsval(scope, prototype, ()).unwrap_or(false) {
            wrapper.set_property(scope, c"prototype", prototype)?;
        }
        let length = function.get_property(scope, c"length")?;
        wrapper.define_property(
            scope,
            c"length",
            length,
            js::class_spec::JSPROP_READONLY as u32,
        )?;
        Ok(*wrapper)
    }

    /// <https://nodejs.org/api/util.html#utildebuglogsection-callback>: a logger that prints to
    /// stderr if `NODE_DEBUG` enables `section`, and does nothing otherwise. Its `enabled`
    /// property is `true` in the first case.
    pub fn debuglog<'s>(scope: &'s Scope<'_>, section: String) -> Result<Object<'s>, ExnThrown> {
        let enabled = debug_enabled(&section);
        let logger = if enabled {
            Function::new_callback(scope, c"debug", 0, debug_log, section.as_str())?
        } else {
            Function::new_callback(scope, c"debug", 0, do_nothing, ())?
        };
        logger.set_property(scope, c"enabled", enabled)?;
        Ok(*logger)
    }

    /// <https://nodejs.org/api/util.html#utildebugsection>: `debuglog`.
    pub fn debug<'s>(scope: &'s Scope<'_>, section: String) -> Result<Object<'s>, ExnThrown> {
        debuglog(scope, section)
    }

    /// <https://nodejs.org/api/util.html#utilstripvtcontrolcharactersstr>
    #[function(name = "stripVTControlCharacters")]
    pub fn strip_control_characters(
        scope: &Scope<'_>,
        text: HandleValue<'_>,
    ) -> Result<String, Thrown> {
        Ok(strip_vt_control_characters(&validate_string(
            scope, text, "str",
        )?))
    }

    /// <https://nodejs.org/api/util.html#utilstyletextformat-text-options>: always styled,
    /// whether or not the output is a terminal, so `options.stream` is not consulted. `format` is
    /// a style name, `'none'`, a `#RGB` or `#RRGGBB` color, or an array of these.
    pub fn style_text(
        scope: &Scope<'_>,
        format: HandleValue<'_>,
        text: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<String, Thrown> {
        let text = validate_string(scope, text, "text")?;
        if let Some(options) = options.filter(|options| !options.is_undefined()) {
            let options = validate_plain_object(scope, options, "options", false)?;
            let validate_stream = options.get_property(scope, c"validateStream")?;
            if !validate_stream.is_undefined() && !validate_stream.is_boolean() {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "options.validateStream",
                    "of type boolean",
                    validate_stream,
                )
                .into());
            }
        }
        let mut keys = Vec::new();
        match Object::from_value(scope, *format) {
            Ok(array) if js::Array::is_array(scope, array.handle())? => {
                let length = array
                    .cast::<js::Array>()
                    .map_err(|_| Thrown::Pending)?
                    .length(scope)?;
                for index in 0..length {
                    keys.push(array.get_element(scope, index)?);
                }
            }
            _ => keys.push(format),
        }
        let mut open_codes = String::new();
        let mut close_codes = String::new();
        let mut processed = text;
        for key in keys {
            let name = if key.is_string() {
                Some(String::from_jsval(scope, key, ())?)
            } else {
                None
            };
            if name.as_deref() == Some("none") {
                continue;
            }
            if let Some(color) = name.as_deref().filter(|name| name.starts_with('#')) {
                let (r, g, b) = hex_color(color).ok_or_else(|| {
                    NodeError::invalid_arg_value(
                        scope,
                        "format",
                        key,
                        "must be a valid hex color (#RGB or #RRGGBB)",
                    )
                })?;
                let open = format!("\u{1b}[38;2;{r};{g};{b}m");
                let close = "\u{1b}[39m";
                open_codes.push_str(&open);
                close_codes = format!("{close}{close_codes}");
                processed = replace_close_code(&processed, close, &open, false);
                continue;
            }
            let Some((open, close)) = name.as_deref().and_then(style_codes) else {
                let allowed: Vec<String> =
                    STYLE_NAMES.iter().map(|name| format!("'{name}'")).collect();
                return Err(NodeError::invalid_arg_value(
                    scope,
                    "format",
                    key,
                    &format!("must be one of: {}", allowed.join(", ")),
                )
                .into());
            };
            let open_seq = format!("\u{1b}[{open}m");
            let close_seq = format!("\u{1b}[{close}m");
            open_codes.push_str(&open_seq);
            close_codes = format!("{close_seq}{close_codes}");
            processed =
                replace_close_code(&processed, &close_seq, &open_seq, open == 1 || open == 2);
        }
        Ok(format!("{open_codes}{processed}{close_codes}"))
    }

    /// <https://nodejs.org/api/util.html#utiltousvstringstring>: lone surrogates become U+FFFD
    /// in the conversion to a Rust string.
    #[function(name = "toUSVString")]
    pub fn to_usv_string(text: String) -> String {
        text
    }

    /// <https://nodejs.org/api/util.html#utilgetsystemerrornameerr>
    pub fn get_system_error_name(
        scope: &Scope<'_>,
        err: HandleValue<'_>,
    ) -> Result<String, Thrown> {
        if !err.is_number() {
            return Err(NodeError::invalid_arg_type(scope, "err", "of type number", err).into());
        }
        let errno = err.to_number();
        if errno >= 0.0 || errno.fract() != 0.0 || errno.abs() > 9007199254740991.0 {
            return Err(NodeError::out_of_range(
                "err",
                "a negative integer",
                &out_of_range_received(errno),
            )
            .into());
        }
        let constants = crate::os::constants(scope)?;
        let table = Object::from_value(scope, constants.get_property(scope, c"errno")?)?;
        for key in table.own_keys(scope)? {
            let id = scope.root_id(js::id::value_to_id(scope, key)?);
            let value =
                f64::from_jsval(scope, table.get_property_by_id(scope, id)?, ()).unwrap_or(0.0);
            if value == -errno {
                return Ok(String::from_jsval(scope, key, ())?);
            }
        }
        Ok(format!(
            "Unknown system error {}",
            crate::errors::number_to_string(errno)
        ))
    }

    /// <https://nodejs.org/api/util.html#utilisarrayobject>: `Array.isArray`.
    pub fn is_array(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        match Object::from_value(scope, *value) {
            Ok(object) => js::Array::is_array(scope, object.handle()),
            Err(_) => Ok(false),
        }
    }

    /// `util._extend(target, source)`: copy `source`'s own enumerable properties onto `target`.
    #[function(name = "_extend")]
    pub fn extend<'s>(
        scope: &'s Scope<'_>,
        target: HandleValue<'_>,
        source: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let target_value = scope.root_value(target.get());
        if let (Ok(target), Ok(source)) = (
            Object::from_value(scope, *target),
            Object::from_value(scope, *source),
        ) {
            target.assign(scope, source)?;
        }
        Ok(target_value)
    }
}

/// `util.inspect.colors`: the ANSI codes that turn each style on and off.
const COLORS: &[(&CStr, f64, f64)] = &[
    (c"reset", 0.0, 0.0),
    (c"bold", 1.0, 22.0),
    (c"dim", 2.0, 22.0),
    (c"italic", 3.0, 23.0),
    (c"underline", 4.0, 24.0),
    (c"blink", 5.0, 25.0),
    (c"inverse", 7.0, 27.0),
    (c"hidden", 8.0, 28.0),
    (c"strikethrough", 9.0, 29.0),
    (c"doubleunderline", 21.0, 24.0),
    (c"black", 30.0, 39.0),
    (c"red", 31.0, 39.0),
    (c"green", 32.0, 39.0),
    (c"yellow", 33.0, 39.0),
    (c"blue", 34.0, 39.0),
    (c"magenta", 35.0, 39.0),
    (c"cyan", 36.0, 39.0),
    (c"white", 37.0, 39.0),
    (c"bgBlack", 40.0, 49.0),
    (c"bgRed", 41.0, 49.0),
    (c"bgGreen", 42.0, 49.0),
    (c"bgYellow", 43.0, 49.0),
    (c"bgBlue", 44.0, 49.0),
    (c"bgMagenta", 45.0, 49.0),
    (c"bgCyan", 46.0, 49.0),
    (c"bgWhite", 47.0, 49.0),
    (c"framed", 51.0, 54.0),
    (c"overlined", 53.0, 55.0),
    (c"gray", 90.0, 39.0),
    (c"redBright", 91.0, 39.0),
    (c"greenBright", 92.0, 39.0),
    (c"yellowBright", 93.0, 39.0),
    (c"blueBright", 94.0, 39.0),
    (c"magentaBright", 95.0, 39.0),
    (c"cyanBright", 96.0, 39.0),
    (c"whiteBright", 97.0, 39.0),
    (c"bgGray", 100.0, 49.0),
    (c"bgRedBright", 101.0, 49.0),
    (c"bgGreenBright", 102.0, 49.0),
    (c"bgYellowBright", 103.0, 49.0),
    (c"bgBlueBright", 104.0, 49.0),
    (c"bgMagentaBright", 105.0, 49.0),
    (c"bgCyanBright", 106.0, 49.0),
    (c"bgWhiteBright", 107.0, 49.0),
];

/// The non-enumerable aliases of `util.inspect.colors` entries: the alias, then the entry.
const COLOR_ALIASES: &[(&CStr, &CStr)] = &[
    (c"grey", c"gray"),
    (c"blackBright", c"gray"),
    (c"bgGrey", c"bgGray"),
    (c"bgBlackBright", c"bgGray"),
    (c"faint", c"dim"),
    (c"crossedout", c"strikethrough"),
    (c"strikeThrough", c"strikethrough"),
    (c"crossedOut", c"strikethrough"),
    (c"conceal", c"hidden"),
    (c"swapColors", c"inverse"),
    (c"swapcolors", c"inverse"),
    (c"doubleUnderline", c"doubleunderline"),
];

/// `util.inspect.styles`: the color of each kind of value.
const STYLES: &[(&CStr, &str)] = &[
    (c"special", "cyan"),
    (c"number", "yellow"),
    (c"bigint", "yellow"),
    (c"boolean", "yellow"),
    (c"undefined", "grey"),
    (c"null", "bold"),
    (c"string", "green"),
    (c"symbol", "green"),
    (c"date", "magenta"),
    (c"regexp", "red"),
    (c"module", "underline"),
];

/// Set `inspect.colors` and `inspect.styles`, objects with null prototypes.
fn define_color_tables(scope: &Scope<'_>, inspect: Object<'_>) -> Result<(), ExnThrown> {
    let colors = Object::new_plain(scope)?;
    colors.set_prototype(scope, js::native::HandleObject::null())?;
    for &(name, on, off) in COLORS {
        colors.set_property(scope, name, js::Array::with_contents(scope, &[on, off])?)?;
    }
    for &(alias, name) in COLOR_ALIASES {
        let codes = colors.get_property(scope, name)?;
        colors.define_property(scope, alias, codes, 0)?;
    }
    inspect.set_property(scope, c"colors", colors)?;
    let styles = Object::new_plain(scope)?;
    styles.set_prototype(scope, js::native::HandleObject::null())?;
    for &(name, color) in STYLES {
        styles.set_property(scope, name, color)?;
    }
    inspect.set_property(scope, c"styles", styles)
}

/// Register `node:util`. `inspect` and `promisify` have the `custom` symbols as properties,
/// `inspect` also the `defaultOptions` accessor and the `colors` and `styles` tables, and `types`, `TextEncoder` and `TextDecoder` are
/// `node:util/types`' `default` export and the global's classes.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let overrides = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let inspect = Function::new_callback(scope, c"inspect", 2, inspect_callback, ())?;
        inspect.set_property(
            scope,
            c"custom",
            crate::symbol(scope, inspect::INSPECT_CUSTOM, true)?,
        )?;
        let defaults = InspectOptions::default().to_object(scope)?;
        defaults.delete_property(scope, c"stylize")?;
        let getter = Function::new_callback(scope, c"get", 0, default_options_get, defaults)?;
        let setter = Function::new_callback(scope, c"set", 1, default_options_set, defaults)?;
        inspect.define_accessor(scope, c"defaultOptions", Some(*getter), Some(*setter), 0)?;
        define_color_tables(scope, *inspect)?;
        let promisify = Function::new_callback(scope, c"promisify", 1, promisify_callback, ())?;
        promisify.set_property(
            scope,
            c"custom",
            crate::symbol(scope, PROMISIFY_CUSTOM, true)?,
        )?;
        let types = core_runtime::module::builtin_namespace(scope, "node:util/types")?
            .ok_or(ExnThrown)?
            .get_property(scope, c"default")?;
        Ok(vec![
            ("inspect", (*inspect).to_jsval_throwing(scope)?),
            ("promisify", (*promisify).to_jsval_throwing(scope)?),
            ("types", types),
            (
                "TextEncoder",
                scope.global().get_property(scope, c"TextEncoder")?,
            ),
            (
                "TextDecoder",
                scope.global().get_property(scope, c"TextDecoder")?,
            ),
        ])
    };
    let Ok(overrides) = overrides() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &overrides)
}
