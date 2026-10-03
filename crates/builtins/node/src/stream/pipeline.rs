// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `pipeline`: Node's `lib/internal/streams/pipeline.js`, for chains of Node streams, iterables and
//! functions. An async iterable a function stage returns becomes a stream through
//! `Readable.from`. Web streams aren't accepted.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::eos::{eos, premature_close};
use super::readable::array_push;
use super::util::{
    as_object, call, call_method, closure, error_value, is_function, is_iterable, is_node_stream,
    is_readable, is_readable_finished, is_readable_node_stream, is_str, listen, next_tick,
    next_tick_call, rust_string, truthy, type_error, Captures, Props,
};
use crate::errors::{validate_abort_signal, validate_function, ErrorKind, NodeError, Thrown};

/// The captures of the pipeline's shared functions: `[callback, error, value, finishCount,
/// destroys, lastCleanups, called, controller, outerSignal, abortListener]`.
mod captures {
    pub(super) const CALLBACK: u32 = 0;
    pub(super) const ERROR: u32 = 1;
    pub(super) const VALUE: u32 = 2;
    pub(super) const FINISH_COUNT: u32 = 3;
    pub(super) const DESTROYS: u32 = 4;
    pub(super) const LAST_CLEANUPS: u32 = 5;
    pub(super) const CALLED: u32 = 6;
    pub(super) const CONTROLLER: u32 = 7;
    pub(super) const OUTER_SIGNAL: u32 = 8;
    pub(super) const ABORT_LISTENER: u32 = 9;
    pub(super) const COUNT: usize = 10;
}

/// The pipeline state of a function whose captures are `[state, ...]`.
fn state_of<'s>(scope: &'s Scope<'_>, payload: HandleValue<'_>) -> Result<Captures<'s>, ExnThrown> {
    Captures::of(scope, Captures::of(scope, payload)?.get(0)?)
}

/// Node's `finishImpl`: record the first error, destroy the streams once there is an error or the
/// pipeline is done, and call the callback once it is done.
fn finish_impl(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    error: HandleValue<'_>,
    done: bool,
) -> Result<(), ExnThrown> {
    use captures::*;
    let recorded = state.get(ERROR)?;
    if truthy(scope, error) {
        let replaceable = match as_object(scope, recorded) {
            _ if !truthy(scope, recorded) => true,
            Some(recorded) => {
                is_str(
                    scope,
                    recorded.get_property(scope, c"code")?,
                    c"ERR_STREAM_PREMATURE_CLOSE",
                )? || is_str(scope, recorded.get_property(scope, c"name")?, c"AbortError")?
            }
            None => false,
        };
        if replaceable {
            state.set(ERROR, error)?;
        }
    }
    let error = state.get(ERROR)?;
    if !truthy(scope, error) && !done {
        return Ok(());
    }
    let destroys = state.obj(DESTROYS)?;
    while destroys.num(scope, c"length")? > 0.0 {
        let destroy = call_method(scope, destroys, c"shift", &[])?;
        call(scope, HandleValue::undefined(), destroy, &[error])?;
    }
    if let (Some(signal), Some(listener)) = (
        as_object(scope, state.get(OUTER_SIGNAL)?),
        as_object(scope, state.get(ABORT_LISTENER)?),
    ) {
        listen(scope, signal, c"removeEventListener", "abort", listener)?;
    }
    if let Some(controller) = as_object(scope, state.get(CONTROLLER)?) {
        call_method(scope, controller, c"abort", &[])?;
    }
    if done && !state.flag(CALLED)? {
        state.set(CALLED, true)?;
        if !truthy(scope, error) {
            let cleanups = state.obj(LAST_CLEANUPS)?;
            for index in 0..cleanups.num(scope, c"length")? as u32 {
                call(
                    scope,
                    HandleValue::undefined(),
                    cleanups.get_element(scope, index)?,
                    &[],
                )?;
            }
        }
        let error = if truthy(scope, error) {
            error
        } else {
            HandleValue::undefined()
        };
        next_tick_call(scope, state.get(CALLBACK)?, &[error, state.get(VALUE)?])?;
    }
    Ok(())
}

/// Node's `finish`, whose captures are `[state]`.
fn finish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let count = state.get(captures::FINISH_COUNT)?.to_number() - 1.0;
    state.set(captures::FINISH_COUNT, count)?;
    finish_impl(scope, &state, args.get(0), count == 0.0)?;
    Ok(js::value::undefined())
}

/// The outer signal's abort listener, whose captures are `[state]`.
fn abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let reason = match as_object(scope, state.get(captures::OUTER_SIGNAL)?) {
        Some(signal) => Some(signal.val(scope, c"reason")?),
        None => None,
    };
    let error = crate::errors::abort_error(scope, reason)?;
    finish_impl(scope, &state, error, true)?;
    Ok(js::value::undefined())
}

/// The listener for `close` of [`destroyer`], whose captures are `[finished]`.
fn mark_finished(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Captures::of(scope, payload)?.set(0, true)?;
    Ok(js::value::undefined())
}

/// The `finished` callback of [`destroyer`], whose captures are `[finished]`.
fn mark_finished_unless_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Captures::of(scope, payload)?.set(0, !truthy(scope, args.get(0)))?;
    Ok(js::value::undefined())
}

/// The destroy function of [`destroyer`], whose captures are `[finished, stream]`.
fn destroy_stream(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    if captures.flag(0)? {
        return Ok(js::value::undefined());
    }
    captures.set(0, true)?;
    let error = if truthy(scope, args.get(0)) {
        scope.root_value(args.get(0).get())
    } else {
        error_value(scope, super::destroy::stream_destroyed("pipe"))?
    };
    super::destroy::destroyer(scope, captures.get(1)?, error)?;
    Ok(js::value::undefined())
}

/// Node's `destroyer`: a function destroying `stream` unless it finished, and the cleanup of the
/// `finished` it uses.
fn destroyer<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    reading: bool,
    writing: bool,
) -> Result<(HandleValue<'s>, HandleValue<'s>), ExnThrown> {
    let cell = js::Array::with_contents(
        scope,
        &[
            js::value::from_bool(false),
            stream.to_jsval_raw_throwing(scope)?,
        ],
    )?;
    let on_close = Function::new_callback(scope, c"", 0, mark_finished, *cell)?;
    listen(scope, stream, c"on", "close", *on_close)?;
    let options = Object::new_plain(scope)?;
    options.set_property(scope, c"readable", reading)?;
    options.set_property(scope, c"writable", writing)?;
    let on_finished = Function::new_callback(scope, c"", 1, mark_finished_unless_error, *cell)?;
    let cleanup = eos(
        scope,
        stream.to_jsval_throwing(scope)?,
        Some(options.to_jsval_throwing(scope)?),
        (*on_finished).to_jsval_throwing(scope)?,
    )
    .map_err(|error| error.throw(scope))?;
    let destroy = Function::new_callback(scope, c"", 1, destroy_stream, *cell)?;
    Ok(((*destroy).to_jsval_throwing(scope)?, cleanup))
}

/// The error listener the pipeline adds to each stream, whose captures are `[state]`: record
/// errors other than aborts and premature closes.
fn on_stream_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let error = args.get(0);
    if let Some(object) = as_object(scope, error) {
        let name = rust_string(scope, object.get_property(scope, c"name")?);
        let code = rust_string(scope, object.get_property(scope, c"code")?);
        if name.as_deref() == Some("AbortError")
            || code.as_deref() == Some("ERR_STREAM_PREMATURE_CLOSE")
        {
            return Ok(js::value::undefined());
        }
    } else if !truthy(scope, error) {
        return Ok(js::value::undefined());
    }
    finish_impl(scope, &state_of(scope, payload)?, error, false)?;
    Ok(js::value::undefined())
}

/// The `close` listener [`pipe`] adds to `dst`, whose captures are `[state, src, dst, ended]`:
/// record a premature close if `dst` closed before `src` ended.
fn pipe_dst_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    if !captures.flag(3)? {
        let error = error_value(scope, premature_close())?;
        finish_impl(scope, &Captures::of(scope, captures.get(0)?)?, error, false)?;
    }
    Ok(js::value::undefined())
}

/// The function that ends `dst` once `src` ended, whose captures are `[state, src, dst, ended]`.
fn pipe_end(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    captures.set(3, true)?;
    call_method(scope, captures.obj(2)?, c"end", &[])?;
    Ok(js::value::undefined())
}

/// The `finished` callback for the source of [`pipe`]: finish once the source ended, also after a
/// premature close of a source that ended cleanly.
fn pipe_src_finished(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let src = captures.obj(1)?;
    let finish_fn = closure(scope, 1, finish, &[captures.get(0)?.get()])?;
    let error = args.get(0);
    let premature = as_object(scope, error)
        .map(|error| error.get_property(scope, c"code"))
        .transpose()?
        .is_some_and(|code| {
            rust_string(scope, code).as_deref() == Some("ERR_STREAM_PREMATURE_CLOSE")
        });
    let clean_end = match src.obj(scope, c"_readableState")? {
        Some(r) => {
            r.flag(scope, c"ended")?
                && !truthy(scope, r.get_property(scope, c"errored")?)
                && !r.flag(scope, c"errorEmitted")?
        }
        None => false,
    };
    if truthy(scope, error) && premature && clean_end {
        listen(scope, src, c"once", "end", finish_fn)?;
        listen(scope, src, c"once", "error", finish_fn)?;
    } else {
        call(
            scope,
            HandleValue::undefined(),
            finish_fn.to_jsval_throwing(scope)?,
            &[error],
        )?;
    }
    Ok(js::value::undefined())
}

/// Node's `pipe`: pipe `src` into `dst` and finish the pipeline's two counts once both have
/// finished. Returns the cleanup of the `finished` on `dst`.
fn pipe<'s>(
    scope: &'s Scope<'_>,
    state: &Captures<'_>,
    src: Object<'_>,
    dst: Object<'_>,
    end: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let state_value = scope.root_value(state.array_value()?);
    let shared = js::Array::with_contents(
        scope,
        &[
            state_value.get(),
            src.to_jsval_raw_throwing(scope)?,
            dst.to_jsval_raw_throwing(scope)?,
            js::value::from_bool(false),
        ],
    )?;
    let on_close = Function::new_callback(scope, c"", 0, pipe_dst_close, *shared)?;
    listen(scope, dst, c"on", "close", *on_close)?;
    let options = Object::new_plain(scope)?;
    options.set_property(scope, c"end", false)?;
    call_method(
        scope,
        src,
        c"pipe",
        &[
            dst.to_jsval_throwing(scope)?,
            options.to_jsval_throwing(scope)?,
        ],
    )?;
    let finish_fn = closure(scope, 1, finish, &[state_value.get()])?;
    if end {
        let end_fn = Function::new_callback(scope, c"", 0, pipe_end, *shared)?;
        if is_readable_finished(scope, src.to_jsval_throwing(scope)?, true)? == Some(true) {
            next_tick_call(scope, (*end_fn).to_jsval_throwing(scope)?, &[])?;
        } else {
            listen(scope, src, c"once", "end", *end_fn)?;
        }
    } else {
        call(
            scope,
            HandleValue::undefined(),
            finish_fn.to_jsval_throwing(scope)?,
            &[],
        )?;
    }
    let src_options = Object::new_plain(scope)?;
    src_options.set_property(scope, c"readable", true)?;
    src_options.set_property(scope, c"writable", false)?;
    let on_src = Function::new_callback(scope, c"", 1, pipe_src_finished, *shared)?;
    eos(
        scope,
        src.to_jsval_throwing(scope)?,
        Some(src_options.to_jsval_throwing(scope)?),
        (*on_src).to_jsval_throwing(scope)?,
    )
    .map_err(|error| error.throw(scope))?;
    let dst_options = Object::new_plain(scope)?;
    dst_options.set_property(scope, c"readable", false)?;
    dst_options.set_property(scope, c"writable", true)?;
    eos(
        scope,
        dst.to_jsval_throwing(scope)?,
        Some(dst_options.to_jsval_throwing(scope)?),
        finish_fn.to_jsval_throwing(scope)?,
    )
    .map_err(|error| error.throw(scope))
}

/// The fulfillment of a final function stage's promise, whose captures are `[state, pt, end]`.
fn final_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    let pt = captures.obj(1)?;
    let value = args.get(0);
    state.set(captures::VALUE, value)?;
    if !value.is_null() && !value.is_undefined() {
        call_method(scope, pt, c"write", &[value])?;
    }
    if captures.flag(2)? {
        call_method(scope, pt, c"end", &[])?;
    }
    next_tick(scope, finish, &[state.array_value()?])?;
    Ok(js::value::undefined())
}

/// The rejection of a final function stage's promise, whose captures are `[state, pt, end]`.
fn final_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    call_method(scope, captures.obj(1)?, c"destroy", &[args.get(0)])?;
    let finish_fn = closure(scope, 1, finish, &[state.array_value()?])?;
    next_tick_call(scope, finish_fn.to_jsval_throwing(scope)?, &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// `ERR_INVALID_RETURN_VALUE`.
fn invalid_return(
    scope: &Scope<'_>,
    expected: &str,
    name: &str,
    value: HandleValue<'_>,
) -> Result<NodeError, ExnThrown> {
    let received = determine_specific_type(scope, value)?;
    Ok(NodeError::new(
        ErrorKind::TypeError,
        "ERR_INVALID_RETURN_VALUE",
        format!(
            "Expected {expected} to be returned from the \"{name}\" function but got {received}."
        ),
    ))
}

/// Node's `determineSpecificType`: a description of `value` for error messages, such as
/// `type number (5)` or `an instance of Object`.
fn determine_specific_type(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    let text = |value: HandleValue<'_>| -> Result<String, ExnThrown> {
        js::JSString::from_value(scope, value.get())?.to_utf8(scope)
    };
    let inspect = |depth: Option<f64>| -> Result<String, ExnThrown> {
        let options = crate::inspect::InspectOptions {
            depth,
            ..crate::inspect::InspectOptions::current(scope)?
        };
        crate::inspect::inspect(scope, value, options)
    };
    if value.is_null() {
        return Ok("null".into());
    }
    if value.is_undefined() {
        return Ok("undefined".into());
    }
    if let Some(object) = as_object(scope, value) {
        if object.is_callable() {
            return Ok(format!(
                "function {}",
                text(object.get_property(scope, c"name")?)?
            ));
        }
        if let Some(constructor) = object.obj(scope, c"constructor")? {
            if constructor.has_property(scope, c"name")? {
                let name = constructor.get_property(scope, c"name")?;
                return Ok(format!("an instance of {}", text(name)?));
            }
        }
        return inspect(Some(-1.0));
    }
    if value.is_number() {
        let number = value.to_number();
        if number == 0.0 && number.is_sign_negative() {
            return Ok("type number (-0)".into());
        }
        return Ok(format!("type number ({})", text(value)?));
    }
    if value.is_boolean() {
        return Ok(format!("type boolean ({})", value.to_boolean()));
    }
    if value.is_symbol() {
        return Ok(format!("type symbol ({})", inspect(Some(2.0))?));
    }
    if value.is_bigint() {
        return Ok(format!("type bigint ({}n)", text(value)?));
    }
    let units: Vec<u16> = text(value)?.encode_utf16().collect();
    let mut string = String::from_utf16_lossy(&units);
    if units.len() > 28 {
        string = format!("{}...", String::from_utf16_lossy(&units[..25]));
    }
    if !string.contains('\'') {
        return Ok(format!("type string ('{string}')"));
    }
    let quoted = js::JSString::from_str(scope, &string)?;
    let quoted = js::json::stringify(scope, quoted.to_jsval_throwing(scope)?, None, None)?;
    Ok(format!("type string ({})", quoted.unwrap_or_default()))
}

/// The listener cleanup the pipeline runs for a readable last stream, whose captures are `[stream,
/// listener]`: remove the pipeline's `error` listener.
fn remove_error_listener(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    listen(
        scope,
        captures.obj(0)?,
        c"removeListener",
        "error",
        captures.obj(1)?,
    )?;
    Ok(js::value::undefined())
}

/// Node's `makeAsyncIterable`: `value` if it is iterable, an async iterator over it if it is a
/// readable stream, and `ERR_INVALID_ARG_TYPE` otherwise.
fn make_async_iterable<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'s>,
) -> Result<HandleValue<'s>, Thrown> {
    if is_iterable(scope, value, true)? {
        return Ok(value);
    }
    if is_readable_node_stream(scope, value, false)? {
        let stream = as_object(scope, value).expect("streams are objects");
        let iterator = super::readable::async_iterator(scope, stream, None)?;
        return Ok(iterator.to_jsval_throwing(scope)?);
    }
    Err(NodeError::invalid_arg_type(
        scope,
        "val",
        "an instance of Readable, Iterable, or AsyncIterable",
        value,
    )
    .into())
}

/// `Readable.from(value)` as a stream.
fn readable_from<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    object_mode: bool,
) -> Result<Object<'s>, Thrown> {
    let options = Object::new_plain(scope)?;
    options.set_property(scope, c"objectMode", object_mode)?;
    super::from::from(scope, value, Some(options.to_jsval_throwing(scope)?))
}

/// Node's `pipelineImpl`. Returns the last stream.
pub(crate) fn pipeline_impl<'s>(
    scope: &'s Scope<'_>,
    mut streams: Vec<HandleValue<'s>>,
    callback: HandleValue<'_>,
    options: Option<Object<'_>>,
) -> Result<HandleValue<'s>, Thrown> {
    use captures::*;
    if streams.len() == 1 {
        if let Some(list) = as_object(scope, streams[0])
            .filter(|list| js::Array::is_array(scope, list.handle()).unwrap_or(false))
        {
            streams = (0..list.num(scope, c"length")? as u32)
                .map(|index| list.get_element(scope, index))
                .collect::<Result<_, _>>()?;
        }
    }
    if streams.len() < 2 {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_MISSING_ARGS",
            "The \"streams\" argument must be specified",
        )
        .into());
    }
    let outer_signal = match options {
        Some(options) => options.val(scope, c"signal")?,
        None => HandleValue::undefined(),
    };
    validate_abort_signal(scope, outer_signal, "options.signal")?;
    let end_option = match options {
        Some(options) => options.val(scope, c"end")?,
        None => HandleValue::undefined(),
    };

    let state_array = js::Array::with_contents(scope, &[js::value::undefined(); COUNT])?;
    let state_value = scope.root_value((*state_array).to_jsval_raw_throwing(scope)?);
    let state = Captures::of(scope, (*state_array).to_jsval_throwing(scope)?)?;
    state.set(CALLBACK, callback)?;
    state.set(VALUE, js::value::undefined())?;
    state.set(FINISH_COUNT, 0)?;
    state.set(DESTROYS, *js::Array::with_contents(scope, &[] as &[Value])?)?;
    state.set(
        LAST_CLEANUPS,
        *js::Array::with_contents(scope, &[] as &[Value])?,
    )?;
    state.set(CALLED, false)?;
    let controller_ctor = scope
        .global()
        .obj(scope, c"AbortController")?
        .ok_or_else(|| type_error(scope, "AbortController is not a constructor"))?;
    let controller = js::Function::construct(scope, controller_ctor, &[] as &[HandleValue<'_>])?;
    state.set(CONTROLLER, controller)?;
    let signal = controller.val(scope, c"signal")?;
    if let Some(outer) = as_object(scope, outer_signal) {
        state.set(OUTER_SIGNAL, outer)?;
        let listener = closure(scope, 0, abort, &[state_value.get()])?;
        state.set(ABORT_LISTENER, listener)?;
        listen(scope, outer, c"addEventListener", "abort", listener)?;
    }
    let destroys = state.obj(DESTROYS)?;
    let last_cleanups = state.obj(LAST_CLEANUPS)?;
    let increment = |by: f64| -> Result<(), ExnThrown> {
        let count = state.get(FINISH_COUNT)?.to_number() + by;
        state.set(FINISH_COUNT, count)
    };
    let count = streams.len();
    let mut ret: HandleValue<'s> = HandleValue::undefined();
    for (index, stream) in streams.iter().copied().enumerate() {
        let reading = index < count - 1;
        let writing = index > 0;
        let is_last = index == count - 1;
        let end = reading || !(end_option.is_boolean() && !end_option.to_boolean());
        if is_node_stream(scope, stream)? {
            let stream_object = as_object(scope, stream).expect("streams are objects");
            if let Some(next) = streams
                .get(index + 1)
                .and_then(|next| as_object(scope, *next))
            {
                if truthy(scope, next.get_property(scope, c"closed")?)
                    || truthy(scope, next.get_property(scope, c"destroyed")?)
                {
                    return Err(NodeError::new(
                        ErrorKind::Error,
                        "ERR_STREAM_UNABLE_TO_PIPE",
                        "Cannot pipe to a closed or destroyed stream",
                    )
                    .into());
                }
            }
            if end {
                let (destroy, cleanup) = destroyer(scope, stream_object, reading, writing)?;
                array_push(scope, destroys, destroy)?;
                if is_readable(scope, stream)? == Some(true) && is_last {
                    array_push(scope, last_cleanups, cleanup)?;
                }
            }
            let on_error = closure(scope, 1, on_stream_error, &[state_value.get()])?;
            listen(scope, stream_object, c"on", "error", on_error)?;
            if is_readable(scope, stream)? == Some(true) && is_last {
                let remove = closure(
                    scope,
                    0,
                    remove_error_listener,
                    &[
                        stream_object.to_jsval_raw_throwing(scope)?,
                        on_error.to_jsval_raw_throwing(scope)?,
                    ],
                )?;
                array_push(scope, last_cleanups, remove.to_jsval_throwing(scope)?)?;
            }
        }
        if index == 0 {
            if is_function(scope, stream) {
                let options = Object::new_plain(scope)?;
                options.set_property(scope, c"signal", signal)?;
                ret = call(
                    scope,
                    HandleValue::undefined(),
                    stream,
                    &[options.to_jsval_throwing(scope)?],
                )?;
                if !is_iterable(scope, ret, true)? {
                    return Err(invalid_return(
                        scope,
                        "Iterable, AsyncIterable or Stream",
                        "source",
                        ret,
                    )?
                    .into());
                }
            } else if is_iterable(scope, stream, true)?
                || is_readable_node_stream(scope, stream, false)?
            {
                ret = stream;
            } else {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "source",
                    "an instance of Stream, Iterable, or AsyncIterable",
                    stream,
                )
                .into());
            }
            continue;
        }
        if is_function(scope, stream) {
            let source = make_async_iterable(scope, ret)?;
            let options = Object::new_plain(scope)?;
            options.set_property(scope, c"signal", signal)?;
            let result = call(
                scope,
                HandleValue::undefined(),
                stream,
                &[source, options.to_jsval_throwing(scope)?],
            )?;
            if reading {
                if !is_iterable(scope, result, false)? {
                    return Err(invalid_return(
                        scope,
                        "AsyncIterable",
                        &format!("transform[{}]", index - 1),
                        result,
                    )?
                    .into());
                }
                ret = result;
                continue;
            }
            let pass_through = super::duplex::pass_through::class_constructor(scope)?;
            let pt_options = Object::new_plain(scope)?;
            pt_options.set_property(scope, c"objectMode", true)?;
            let pt = js::Function::construct(
                scope,
                pass_through,
                &[pt_options.to_jsval_throwing(scope)?],
            )?;
            let then = match as_object(scope, result) {
                Some(result) => result.val(scope, c"then")?,
                None => HandleValue::undefined(),
            };
            if is_function(scope, then) {
                increment(1.0)?;
                let shared = [
                    state_value,
                    scope.root_value(pt.to_jsval_raw_throwing(scope)?),
                    scope.root_value(js::value::from_bool(end)),
                ];
                let on_value = closure(scope, 1, final_value, &shared)?;
                let on_error = closure(scope, 1, final_error, &shared)?;
                call(
                    scope,
                    result,
                    then,
                    &[
                        on_value.to_jsval_throwing(scope)?,
                        on_error.to_jsval_throwing(scope)?,
                    ],
                )?;
            } else if is_iterable(scope, result, false)? {
                increment(2.0)?;
                let source = readable_from(scope, result, true)?;
                pipe(scope, &state, source, pt, end)?;
            } else {
                return Err(invalid_return(
                    scope,
                    "AsyncIterable or Promise",
                    "destination",
                    result,
                )?
                .into());
            }
            ret = pt.to_jsval_throwing(scope)?;
            let (destroy, cleanup) = destroyer(scope, pt, false, true)?;
            array_push(scope, destroys, destroy)?;
            if is_last {
                array_push(scope, last_cleanups, cleanup)?;
            }
            continue;
        }
        if is_node_stream(scope, stream)? {
            let stream_object = as_object(scope, stream).expect("streams are objects");
            if is_readable_node_stream(scope, ret, false)? {
                increment(2.0)?;
                let source = as_object(scope, ret).expect("streams are objects");
                let cleanup = pipe(scope, &state, source, stream_object, end)?;
                if is_readable(scope, stream)? == Some(true) && is_last {
                    array_push(scope, last_cleanups, cleanup)?;
                }
            } else if is_iterable(scope, ret, true)? {
                increment(2.0)?;
                let source = readable_from(scope, ret, true)?;
                pipe(scope, &state, source, stream_object, end)?;
            } else {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "val",
                    "an instance of Readable, Iterable, or AsyncIterable",
                    ret,
                )
                .into());
            }
            ret = stream;
            continue;
        }
        return Err(NodeError::invalid_arg_type(
            scope,
            "streams",
            "an instance of Stream or a function",
            stream,
        )
        .into());
    }
    let aborted = |value: HandleValue<'_>| -> Result<bool, ExnThrown> {
        Ok(match as_object(scope, value) {
            Some(signal) => truthy(scope, signal.get_property(scope, c"aborted")?),
            None => false,
        })
    };
    if aborted(signal)? || aborted(outer_signal)? {
        next_tick(scope, abort, &[state_value.get()])?;
    }
    Ok(ret)
}

/// `stream.pipeline(...streams, callback)`.
pub(crate) fn pipeline<'s>(
    scope: &'s Scope<'_>,
    mut streams: Vec<HandleValue<'s>>,
) -> Result<HandleValue<'s>, Thrown> {
    let callback = streams.pop().unwrap_or(HandleValue::undefined());
    validate_function(scope, callback, "streams[stream.length - 1]")?;
    pipeline_impl(scope, streams, callback, None)
}

/// The callback of `stream/promises`' `pipeline`, whose captures are `[promise]`.
fn settle(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = Captures::of(scope, payload)?
        .obj(0)?
        .cast::<js::Promise>()
        .map_err(|_| type_error(scope, "not a promise"))?;
    if truthy(scope, args.get(0)) {
        promise.reject(scope, args.get(0))?;
    } else {
        promise.resolve(scope, args.get(1))?;
    }
    Ok(js::value::undefined())
}

/// `stream/promises`' `pipeline(...streams, options)`.
pub(crate) fn pipeline_promise<'s>(
    scope: &'s Scope<'_>,
    mut streams: Vec<HandleValue<'s>>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    let mut options = None;
    if let Some(last) = streams.last().copied() {
        if let Some(object) = as_object(scope, last) {
            if !is_node_stream(scope, last)?
                && !is_iterable(scope, last, true)?
                && !is_function(scope, last)
            {
                options = Some(object);
                streams.pop();
            }
        }
    }
    let callback = closure(scope, 2, settle, &[promise.to_jsval_raw_throwing(scope)?])?;
    if let Err(error) = pipeline_impl(scope, streams, callback.to_jsval_throwing(scope)?, options) {
        error.throw(scope);
        promise.reject_with_pending(scope)?;
    }
    Ok(promise)
}
