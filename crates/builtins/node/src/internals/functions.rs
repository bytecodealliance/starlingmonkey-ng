// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The functions of the `internal/` modules that aren't members of a public module.

use std::cell::RefCell;
use std::collections::HashSet;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_function, validate_integer, NodeError, Thrown};

/// The value `result` holds, or its error thrown.
fn finish(scope: &Scope<'_>, result: Result<HandleValue<'_>, Thrown>) -> Result<Value, ExnThrown> {
    match result {
        Ok(value) => Ok(value.get()),
        Err(error) => Err(error.throw(scope)),
    }
}

/// `internal/util`'s `sleep(msec)`: block the thread for `msec` milliseconds, an unsigned 32-bit
/// integer.
pub(super) fn sleep(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let msec = validate_integer(scope, args.get(0), "msec", 0.0, 4294967295.0)
        .map_err(|error| error.throw(scope))?;
    std::thread::sleep(std::time::Duration::from_millis(msec as u64));
    Ok(js::value::undefined())
}

js::instance_local! {
    /// The features `emitExperimentalWarning` has warned about.
    static EXPERIMENTAL_WARNED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// `internal/util`'s `emitExperimentalWarning(feature)`: emit an `ExperimentalWarning` for
/// `feature`, once per feature.
pub(super) fn emit_experimental_warning(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let feature = js::JSString::from_value(scope, args.get(0))?.to_utf8(scope)?;
    warn_experimental(scope, &feature)?;
    Ok(js::value::undefined())
}

/// Emit an `ExperimentalWarning` for `feature`, once per feature, as `emitExperimentalWarning`
/// does.
pub(crate) fn warn_experimental(scope: &Scope<'_>, feature: &str) -> Result<(), ExnThrown> {
    if EXPERIMENTAL_WARNED.with(|warned| warned.borrow_mut().insert(feature.to_string())) {
        let message = format!("{feature} is an experimental feature and might change at any time");
        crate::process::emit_warning(scope, &message, "ExperimentalWarning")?;
    }
    Ok(())
}

/// `internal/util`'s `pendingDeprecate(fn, msg, code)`: `util.deprecate(fn, msg, code)` with
/// `--pending-deprecation`, and `fn` itself otherwise.
pub(super) fn pending_deprecate(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    if !crate::runtime_flags::get().pending_deprecation {
        return Ok(args.get(0).get());
    }
    let deprecated = crate::util::module::deprecate(
        scope,
        args.get(0),
        Some(args.get(1)),
        Some(args.get(2)),
        None,
    )
    .map_err(|error| error.throw(scope))?;
    deprecated.to_jsval_raw_throwing(scope)
}

/// `internal/readline/utils`'s `CSI`: a template tag that prefixes its string with the control
/// sequence introducer `ESC [`.
fn csi(scope: &Scope<'_>, args: CallbackArgs<'_>, _: HandleValue) -> Result<Value, ExnThrown> {
    let strings = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    let length =
        f64::from_jsval_throwing(scope, strings.get_property(scope, c"length")?, ())? as u32;
    let mut text = String::from("\x1b[");
    for index in 0..length {
        let part = strings.get_element(scope, index)?;
        text.push_str(&js::JSString::from_value(scope, part)?.to_utf8(scope)?);
        if index + 1 < args.len() {
            let value = args.get(index + 1);
            text.push_str(&js::JSString::from_value(scope, value)?.to_utf8(scope)?);
        }
    }
    text.as_str().to_jsval_raw_throwing(scope)
}

/// `internal/readline/utils`'s `CSI` tag, with the sequences it has as properties.
pub(super) fn csi_tag<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let tag = Function::new_callback(scope, c"CSI", 1, csi, ())?;
    for (name, sequence) in [
        (c"kEscape", "\x1b"),
        (c"kClearToLineBeginning", "\x1b[1K"),
        (c"kClearToLineEnd", "\x1b[0K"),
        (c"kClearLine", "\x1b[2K"),
        (c"kClearScreenDown", "\x1b[0J"),
    ] {
        tag.set_property(scope, name, sequence)?;
    }
    Ok(*tag)
}

/// The property `name` of `object`, which must be an object.
fn property<'s>(
    scope: &'s Scope<'_>,
    object: HandleValue<'_>,
    name: &'static std::ffi::CStr,
) -> Result<HandleValue<'s>, ExnThrown> {
    let object = Object::from_value(scope, object).map_err(|e| e.throw(scope))?;
    object.get_property(scope, name)
}

/// Set the property `name` of `object`, which must be an object, to `value`.
fn set(
    scope: &Scope<'_>,
    object: HandleValue<'_>,
    name: &'static std::ffi::CStr,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let object = Object::from_value(scope, object).map_err(|e| e.throw(scope))?;
    object.set_property(scope, name, value)
}

/// `internal/linkedlist`'s `init(list)`: make `list` an empty circular list, and return it.
pub(super) fn list_init(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let list = args.get(0);
    set(scope, list, c"_idleNext", list)?;
    set(scope, list, c"_idlePrev", list)?;
    Ok(list.get())
}

/// `internal/linkedlist`'s `peek(list)`: the last item of `list`, or `null` if it is empty.
pub(super) fn list_peek(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let list = args.get(0);
    let last = property(scope, list, c"_idlePrev")?;
    if js::comparison::strictly_equal(scope, last, list)? {
        return Ok(js::value::null());
    }
    Ok(last.get())
}

/// Unlink `item` from the list it is in.
fn unlink(scope: &Scope<'_>, item: HandleValue<'_>) -> Result<(), ExnThrown> {
    let next = property(scope, item, c"_idleNext")?;
    let previous = property(scope, item, c"_idlePrev")?;
    if next.is_object() {
        set(scope, next, c"_idlePrev", previous)?;
    }
    if previous.is_object() {
        set(scope, previous, c"_idleNext", next)?;
    }
    let null = HandleValue::null();
    set(scope, item, c"_idleNext", null)?;
    set(scope, item, c"_idlePrev", null)
}

/// `internal/linkedlist`'s `remove(item)`: unlink `item` from the list it is in.
pub(super) fn list_remove(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    unlink(scope, args.get(0))?;
    Ok(js::value::undefined())
}

/// `internal/linkedlist`'s `append(list, item)`: move `item` to the front of `list`.
pub(super) fn list_append(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let list = args.get(0);
    let item = args.get(1);
    let linked = |name| -> Result<bool, ExnThrown> {
        Ok(bool::from_jsval(scope, property(scope, item, name)?, ()).unwrap_or(false))
    };
    if linked(c"_idleNext")? || linked(c"_idlePrev")? {
        unlink(scope, item)?;
    }
    let first = property(scope, list, c"_idleNext")?;
    set(scope, item, c"_idleNext", first)?;
    set(scope, first, c"_idlePrev", item)?;
    set(scope, item, c"_idlePrev", list)?;
    set(scope, list, c"_idleNext", item)?;
    Ok(js::value::undefined())
}

/// `internal/linkedlist`'s `isEmpty(list)`: whether `list` has no items.
pub(super) fn list_is_empty(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let list = args.get(0);
    let first = property(scope, list, c"_idleNext")?;
    Ok(js::value::from_bool(js::comparison::strictly_equal(
        scope, first, list,
    )?))
}

/// `internal/url`'s `isURL(value)`: whether `value` has the `href` and `protocol` of a `URL`, and
/// neither the `auth` nor the `path` of a legacy URL object.
pub(super) fn is_url(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::from_bool(crate::url::is_url(
        scope,
        args.get(0),
    )?))
}

/// `internal/options`' `getOptionValue(name)`: the value of the runtime option `name`, for the
/// options the Node.js runtime flags set, and `--max-http-header-size`. Other options are
/// `undefined`.
pub(super) fn get_option_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let name = String::from_jsval_throwing(scope, args.get(0), ())?;
    let flags = crate::runtime_flags::get();
    Ok(match name.as_str() {
        "--expose-internals" => flags.expose_internals.to_jsval_raw_throwing(scope)?,
        "--warnings" => flags.warnings.to_jsval_raw_throwing(scope)?,
        "--pending-deprecation" => flags.pending_deprecation.to_jsval_raw_throwing(scope)?,
        "--deprecation" => (!flags.no_deprecation).to_jsval_raw_throwing(scope)?,
        "--throw-deprecation" => flags.throw_deprecation.to_jsval_raw_throwing(scope)?,
        "--abort-on-uncaught-exception" => flags
            .abort_on_uncaught_exception
            .to_jsval_raw_throwing(scope)?,
        "--experimental-stream-iter" => flags
            .experimental_stream_iter
            .to_jsval_raw_throwing(scope)?,
        "--insecure-http-parser" => flags.insecure_http_parser.to_jsval_raw_throwing(scope)?,
        "--network-family-autoselection" => flags
            .network_family_autoselection
            .to_jsval_raw_throwing(scope)?,
        "--network-family-autoselection-attempt-timeout" => {
            js::value::from_f64(flags.network_family_autoselection_attempt_timeout)
        }
        "--title" => flags
            .title
            .as_deref()
            .unwrap_or("")
            .to_jsval_raw_throwing(scope)?,
        "--max-http-header-size" => js::value::from_f64(16384.0),
        _ => js::value::undefined(),
    })
}

/// `internal/timers`' `setUnrefTimeout(callback, after, ...args)`: `setTimeout` with the timer
/// unreferenced.
pub(super) fn set_unref_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let call_args = args.rest(0);
    let result = (|| -> Result<HandleValue<'_>, Thrown> {
        validate_function(scope, args.get(0), "callback")?;
        let rest = call_args.get(2..).unwrap_or_default();
        let timer =
            crate::timers::create_timeout(scope, args.get(0), args.get(1), rest, false, false)?;
        Ok(timer.to_jsval_throwing(scope)?)
    })();
    finish(scope, result)
}

/// `internal/streams/add-abort-signal`'s `addAbortSignalNoValidate(signal, stream)`:
/// `stream.addAbortSignal(signal, stream)` if `signal` is an object with an `aborted` property,
/// and `stream` unchanged otherwise.
pub(super) fn add_abort_signal_no_validate(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let signal = args.get(0);
    let stream = args.get(1);
    let is_signal = match Object::from_value(scope, signal) {
        Ok(signal) => signal.has_property(scope, c"aborted")?,
        Err(_) => false,
    };
    if !is_signal {
        return Ok(stream.get());
    }
    let module = crate::internal_modules::module_default(scope, "node:stream")?;
    Ok(Function::call_by_name(scope, module.handle(), c"addAbortSignal", &[signal, stream])?.get())
}

/// `internal/assert/myers_diff`'s `myersDiff(actual, expected, checkCommaDisparity)`: the diff of
/// the array-likes `actual` and `expected` as `[operation, value]` pairs in reverse order, where
/// the operation is `1` for an element of `actual`, `-1` for an element of `expected`, and `0` for
/// an element both have. Their lengths must add up to less than 2^31.
pub(super) fn myers_diff(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let result = (|| -> Result<HandleValue<'_>, Thrown> {
        let actual = Object::from_value(scope, args.get(0))?;
        let expected = Object::from_value(scope, args.get(1))?;
        let comma = bool::from_jsval(scope, args.get(2), ()).unwrap_or(false);
        let actual_length =
            f64::from_jsval_throwing(scope, actual.get_property(scope, c"length")?, ())?;
        let expected_length =
            f64::from_jsval_throwing(scope, expected.get_property(scope, c"length")?, ())?;
        let max = actual_length + expected_length;
        if max >= 2f64.powi(31) {
            return Err(NodeError::out_of_range(
                "myersDiff input size",
                "< 2^31",
                &crate::errors::out_of_range_received(max),
            )
            .into());
        }
        let out_of_memory = || {
            js::exception::report_out_of_memory(scope);
            Thrown::Pending
        };
        let lines = |array: Object<'_>, length: f64| -> Result<Vec<String>, Thrown> {
            let mut lines = Vec::new();
            lines
                .try_reserve_exact(length as usize)
                .map_err(|_| out_of_memory())?;
            for index in 0..length as u32 {
                let value = array.get_element(scope, index)?;
                lines.push(js::JSString::from_value(scope, value)?.to_utf8(scope)?);
            }
            Ok(lines)
        };
        let diff = crate::assert::myers_diff(
            &lines(actual, actual_length)?,
            &lines(expected, expected_length)?,
            comma,
        )
        .ok_or_else(out_of_memory)?;
        let mut pairs = Vec::new();
        pairs
            .try_reserve_exact(diff.len())
            .map_err(|_| out_of_memory())?;
        for (operation, value) in diff {
            let operation = scope.root_value(js::value::from_i32(operation.sign()));
            let pair = js::Array::with_contents(
                scope,
                &[
                    operation,
                    js::conversion::ToJSVal::to_jsval_throwing(&value, scope)?,
                ],
            )?;
            pairs.push(pair.to_jsval_throwing(scope)?);
        }
        Ok(js::Array::with_contents(scope, &pairs)?.to_jsval_throwing(scope)?)
    })();
    finish(scope, result)
}

/// `internal/util/inspect`'s `getStringWidth(text, removeControlChars = true)`: the columns
/// `text` takes on a terminal, without its escape sequences unless `removeControlChars` is
/// `false`.
pub(super) fn get_string_width(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let text = js::JSString::from_value(scope, args.get(0))?.to_utf8(scope)?;
    let strip = !(args.len() > 1 && args.get(1).is_boolean() && !args.get(1).to_boolean());
    let width = if strip {
        crate::inspect::string_width(&text)
    } else {
        crate::inspect::columns(&text)
    };
    Ok(js::value::from_f64(width as f64))
}
