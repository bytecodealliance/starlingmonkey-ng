// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `finished`: Node's `lib/internal/streams/end-of-stream.js`.

use js::async_context::Snapshot;
use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{Callback, CallbackArgs};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::util::{
    as_object, call, closure, error_value, is_closed, is_function, is_node_stream, is_readable,
    is_readable_finished, is_readable_node_stream, is_writable, is_writable_finished,
    is_writable_node_stream, listen, next_tick, required_obj, side_errored, states, string, truthy,
    type_error, will_emit_close, Captures, Props,
};
use crate::errors::{
    validate_abort_signal, validate_function, validate_object, ErrorKind, NodeError, Thrown,
};

/// The captures of `finished`'s listeners: `[stream, callback, readable, writable,
/// willEmitClose, readableFinished, writableFinished, called, listeners, signal, abortListener]`,
/// where `callback` runs in the async context of the `finished()` call, `called` is set once the
/// callback ran or the cleanup made it a no-op, and `listeners` is the array of `[emitter, event,
/// listener]` to remove in cleanup.
mod captures {
    pub(super) const STREAM: u32 = 0;
    pub(super) const CALLBACK: u32 = 1;
    pub(super) const READABLE: u32 = 2;
    pub(super) const WRITABLE: u32 = 3;
    pub(super) const WILL_EMIT_CLOSE: u32 = 4;
    pub(super) const READABLE_FINISHED: u32 = 5;
    pub(super) const WRITABLE_FINISHED: u32 = 6;
    pub(super) const CALLED: u32 = 7;
    pub(super) const LISTENERS: u32 = 8;
    pub(super) const SIGNAL: u32 = 9;
    pub(super) const ABORT_LISTENER: u32 = 10;
    pub(super) const COUNT: usize = 11;
}

/// The state array of a listener whose captures are `[state]`.
fn state_of<'s>(scope: &'s Scope<'_>, payload: HandleValue<'_>) -> Result<Captures<'s>, ExnThrown> {
    Captures::of(scope, Captures::of(scope, payload)?.get(0)?)
}

/// Call the callback once with `args`, with `this` the stream, and detach the abort listener.
fn complete(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    use captures::*;
    if state.flag(CALLED)? {
        return Ok(());
    }
    state.set(CALLED, true)?;
    if let (Some(signal), Some(listener)) = (
        as_object(scope, state.get(SIGNAL)?),
        as_object(scope, state.get(ABORT_LISTENER)?),
    ) {
        listen(scope, signal, c"removeEventListener", "abort", listener)?;
    }
    let callback = state.get(CALLBACK)?;
    let stream = state.get(STREAM)?;
    call(scope, stream, callback, args)?;
    Ok(())
}

/// `ERR_STREAM_PREMATURE_CLOSE`.
pub(crate) fn premature_close() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_STREAM_PREMATURE_CLOSE",
        "Premature close",
    )
}

fn onfinish(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use captures::*;
    let state = state_of(scope, payload)?;
    state.set(WRITABLE_FINISHED, true)?;
    let stream = state.obj(STREAM)?;
    if truthy(scope, stream.get_property(scope, c"destroyed")?) {
        state.set(WILL_EMIT_CLOSE, false)?;
    }
    let readable = state.flag(READABLE)?;
    if state.flag(WILL_EMIT_CLOSE)?
        && (!truthy(scope, stream.get_property(scope, c"readable")?) || readable)
    {
        return Ok(js::value::undefined());
    }
    if !readable || state.flag(READABLE_FINISHED)? {
        complete(scope, &state, &[])?;
    }
    Ok(js::value::undefined())
}

fn onend(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use captures::*;
    let state = state_of(scope, payload)?;
    state.set(READABLE_FINISHED, true)?;
    let stream = state.obj(STREAM)?;
    if truthy(scope, stream.get_property(scope, c"destroyed")?) {
        state.set(WILL_EMIT_CLOSE, false)?;
    }
    let writable = state.flag(WRITABLE)?;
    if state.flag(WILL_EMIT_CLOSE)?
        && (!truthy(scope, stream.get_property(scope, c"writable")?) || writable)
    {
        return Ok(js::value::undefined());
    }
    if !writable || state.flag(WRITABLE_FINISHED)? {
        complete(scope, &state, &[])?;
    }
    Ok(js::value::undefined())
}

fn onlegacyfinish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    if !truthy(
        scope,
        state
            .obj(captures::STREAM)?
            .get_property(scope, c"writable")?,
    ) {
        return onfinish(scope, args, payload);
    }
    Ok(js::value::undefined())
}

fn onerror(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    complete(scope, &state_of(scope, payload)?, &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// The error either side of `stream` failed with, if any.
fn errored<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    if let Some(error) = side_errored(scope, stream, c"writableErrored", c"_writableState")? {
        return Ok(Some(error));
    }
    side_errored(scope, stream, c"readableErrored", c"_readableState")
}

/// Node's `getEosOnCloseError`: the error `finished` reports once the stream closed. That is
/// the error of either side, or `ERR_STREAM_PREMATURE_CLOSE` if a side `finished` checks didn't
/// finish, or `None`.
fn close_error<'s>(
    scope: &'s Scope<'_>,
    state: &Captures<'_>,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    use captures::*;
    let stream = state.obj(STREAM)?;
    if let Some(error) = eos_errored(scope, stream)? {
        return Ok(Some(error));
    }
    let stream_value = stream.to_jsval_throwing(scope)?;
    if state.flag(READABLE)?
        && !state.flag(READABLE_FINISHED)?
        && is_readable_node_stream(scope, stream_value, true)?
        && is_readable_finished(scope, stream_value, false)? != Some(true)
    {
        return Ok(Some(error_value(scope, premature_close())?));
    }
    if state.flag(WRITABLE)?
        && !state.flag(WRITABLE_FINISHED)?
        && is_writable_finished(scope, stream_value, false)? != Some(true)
    {
        return Ok(Some(error_value(scope, premature_close())?));
    }
    Ok(None)
}

/// Node's `getEosErrored`: the error of either side, unless it is a boolean.
fn eos_errored<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    Ok(errored(scope, stream)?.filter(|error| !error.is_boolean() && truthy(scope, *error)))
}

/// Call the callback with `error`, or with no arguments for `None`.
fn complete_with(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    match error {
        Some(error) => complete(scope, state, &[error]),
        None => complete(scope, state, &[]),
    }
}

fn onclose(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let error = close_error(scope, &state)?;
    complete_with(scope, &state, error)?;
    Ok(js::value::undefined())
}

/// The listener for `request` of an HTTP request, whose captures are `[state]`: listen for
/// `finish` on `stream.req`.
fn onrequest(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let request = required_obj(scope, state.obj(captures::STREAM)?, c"req")?;
    add_listener(scope, &state, request, "finish", onfinish)?;
    Ok(js::value::undefined())
}

/// The tick that reports the result `eos` computed when it was called, whose captures are
/// `[state, hasError, error]`.
fn report_immediate(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    let error = if captures.flag(1)? {
        Some(captures.get(2)?)
    } else {
        None
    };
    complete_with(scope, &state, error)?;
    Ok(js::value::undefined())
}

/// The abort listener, whose captures are `[state]`: remove the listeners and call the callback
/// with an `AbortError`, unless it ran or the cleanup ran.
fn onabort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    if state.flag(captures::CALLED)? {
        return Ok(js::value::undefined());
    }
    let signal = state.obj(captures::SIGNAL)?;
    let reason = signal.get_property(scope, c"reason")?;
    let error = crate::errors::abort_error(scope, Some(reason))?;
    cleanup_state(scope, &state)?;
    state.set(captures::CALLED, false)?;
    complete(scope, &state, &[error])?;
    Ok(js::value::undefined())
}

/// Add `body`, with captures `[state]`, as the listener for `event` on `emitter`, and record it
/// for the cleanup.
fn add_listener(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    emitter: Object<'_>,
    event: &str,
    body: Callback,
) -> Result<(), ExnThrown> {
    let listener = closure(scope, 1, body, &[state.array_value()?])?;
    record_listener(scope, state, emitter, event, listener)
}

/// Add `listener` for `event` on `emitter`, and record it for the cleanup.
fn record_listener(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    emitter: Object<'_>,
    event: &str,
    listener: Object<'_>,
) -> Result<(), ExnThrown> {
    listen(scope, emitter, c"on", event, listener)?;
    let event = string(scope, event)?;
    let entry = js::Array::with_contents(
        scope,
        &[
            emitter.to_jsval_raw_throwing(scope)?,
            event.get(),
            listener.to_jsval_raw_throwing(scope)?,
        ],
    )?;
    super::readable::array_push(
        scope,
        state.obj(captures::LISTENERS)?,
        (*entry).to_jsval_throwing(scope)?,
    )
}

fn nop(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// Remove `finished`'s listeners, and make the callback a no-op.
fn cleanup_state(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    state.set(captures::CALLED, true)?;
    let listeners = state.obj(captures::LISTENERS)?;
    for index in 0..listeners.num(scope, c"length")? as u32 {
        let entry = Captures::of(scope, listeners.get_element(scope, index)?)?;
        let emitter = entry.obj(0)?;
        let event = entry.get(1)?;
        let listener = entry.get(2)?;
        super::util::call_method(scope, emitter, c"removeListener", &[event, listener])?;
    }
    Ok(())
}

/// The cleanup function `finished` returns, whose captures are `[state]`.
fn cleanup(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    cleanup_state(scope, &state_of(scope, payload)?)?;
    Ok(js::value::undefined())
}

/// Run the captured callback with the async context of the `finished()` call, whose captures are
/// `[callback, context]`.
fn bound_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let callback = captures.get(0)?;
    let context = Snapshot::of(as_object(scope, captures.get(1)?));
    let previous = context.enter();
    let call_args: Vec<HandleValue<'_>> = (0..args.len()).map(|index| args.get(index)).collect();
    let this = scope.root_value(args.this());
    let result = Function::call(scope, this, callback, &call_args);
    previous.enter();
    result.map(|value| value.get())
}

/// Node's `eos`: call `callback` once `stream` finished, closed or failed. Returns the cleanup
/// function. If the stream already finished, closed or failed, or `options.signal` already
/// aborted, the callback runs in a tick and no listeners are added, except a no-op `error`
/// listener unless `options.error` is `false`.
pub(crate) fn eos<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
    callback: HandleValue<'_>,
) -> Result<HandleValue<'s>, Thrown> {
    eos_impl(scope, stream, options, callback, false)
}

/// [`eos`] with Node's `kEosNodeSynchronousCallback` option: a result known when it is called
/// is passed to the callback, with `this` the stream, before it returns instead of in a tick.
pub(crate) fn eos_synchronous<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
    callback: HandleValue<'_>,
) -> Result<HandleValue<'s>, Thrown> {
    eos_impl(scope, stream, options, callback, true)
}

/// The body of [`eos`] and [`eos_synchronous`].
fn eos_impl<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
    callback: HandleValue<'_>,
    synchronous: bool,
) -> Result<HandleValue<'s>, Thrown> {
    use captures::*;
    let options = match options.filter(|options| !options.is_null() && !options.is_undefined()) {
        Some(options) => Some(validate_object(scope, options, "options")?),
        None => None,
    };
    validate_function(scope, callback, "callback")?;
    let option = |name: &'static std::ffi::CStr| -> Result<HandleValue<'_>, ExnThrown> {
        match options {
            Some(options) => options.val(scope, name),
            None => Ok(HandleValue::undefined()),
        }
    };
    let signal = option(c"signal")?;
    validate_abort_signal(scope, signal, "options.signal")?;
    if !is_node_stream(scope, stream)? {
        return Err(NodeError::invalid_arg_type(
            scope,
            "stream",
            "an instance of ReadableStream, WritableStream, or Stream",
            stream,
        )
        .into());
    }
    let stream_object = as_object(scope, stream).expect("streams are objects");
    let readable_option = option(c"readable")?;
    let readable = if readable_option.is_null() || readable_option.is_undefined() {
        is_readable_node_stream(scope, stream, false)?
    } else {
        truthy(scope, readable_option)
    };
    let writable_option = option(c"writable")?;
    let writable = if writable_option.is_null() || writable_option.is_undefined() {
        is_writable_node_stream(scope, stream)?
    } else {
        truthy(scope, writable_option)
    };
    let will_emit_close = will_emit_close(scope, stream_object)?
        && is_readable_node_stream(scope, stream, false)? == readable
        && is_writable_node_stream(scope, stream)? == writable;
    let writable_finished = is_writable_finished(scope, stream, false)? == Some(true);
    let readable_finished = is_readable_finished(scope, stream, false)? == Some(true);
    let (r_state, w_state) = states(scope, stream_object)?;

    let state_array = js::Array::with_contents(scope, &[js::value::undefined(); COUNT])?;
    let state_value = scope.root_value((*state_array).to_jsval_raw_throwing(scope)?);
    let state = Captures::of(scope, (*state_array).to_jsval_throwing(scope)?)?;
    state.set(STREAM, stream)?;
    state.set(READABLE, readable)?;
    state.set(WRITABLE, writable)?;
    state.set(WILL_EMIT_CLOSE, will_emit_close)?;
    state.set(READABLE_FINISHED, readable_finished)?;
    state.set(WRITABLE_FINISHED, writable_finished)?;
    state.set(CALLED, false)?;
    state.set(
        LISTENERS,
        *js::Array::with_contents(scope, &[] as &[Value])?,
    )?;

    let error_emitted = |side: Option<Object<'_>>| -> Result<bool, ExnThrown> {
        Ok(match side {
            Some(side) => side.flag(scope, c"errorEmitted")?,
            None => false,
        })
    };
    let pending_writes = match w_state {
        Some(w) => {
            let pending = w.get_property(scope, c"pendingcb")?;
            !(pending.is_undefined() || (pending.is_number() && pending.to_number() == 0.0))
        }
        None => false,
    };
    // `Some(None)` is a result without an error.
    let mut immediate: Option<Option<HandleValue<'_>>> = None;
    if is_closed(scope, stream_object)? == Some(true) {
        immediate = Some(close_error(scope, &state)?);
    } else if error_emitted(w_state)? || error_emitted(r_state)? {
        if !will_emit_close {
            immediate = Some(eos_errored(scope, stream_object)?);
        }
    } else if (!readable
        && (!will_emit_close || is_readable(scope, stream)? == Some(true))
        && (writable_finished || is_writable(scope, stream)? == Some(false))
        && !pending_writes)
        || (!writable
            && (!will_emit_close || is_writable(scope, stream)? == Some(true))
            && (readable_finished || is_readable(scope, stream)? == Some(false)))
        || (r_state.is_some()
            && truthy(scope, stream_object.get_property(scope, c"req")?)
            && truthy(scope, stream_object.get_property(scope, c"aborted")?))
    {
        immediate = Some(eos_errored(scope, stream_object)?);
    }
    let error_option = option(c"error")?;
    let listen_errors = !(error_option.is_boolean() && !error_option.to_boolean());
    let signal_object = as_object(scope, signal);
    if immediate.is_some() {
        if listen_errors {
            let nop = js::class::get_or_init_shared_function(
                scope,
                nop as *const () as usize,
                |scope| Function::new_callback(scope, c"nop", 0, nop, ()),
            )?;
            record_listener(scope, &state, stream_object, "error", *nop)?;
        }
    } else if let Some(signal) = signal_object {
        if truthy(scope, signal.get_property(scope, c"aborted")?) {
            let reason = signal.get_property(scope, c"reason")?;
            immediate = Some(Some(crate::errors::abort_error(scope, Some(reason))?));
        }
    }

    let context = match js::async_context::current(scope) {
        Some(context) => context.to_jsval_throwing(scope)?,
        None => HandleValue::undefined(),
    };
    let cleanup = closure(scope, 0, cleanup, &[state_value.get()])?;
    if let Some(error) = immediate.filter(|_| synchronous) {
        state.set(CALLBACK, callback)?;
        complete_with(scope, &state, error)?;
        return Ok(cleanup.to_jsval_throwing(scope)?);
    }
    let callback = closure(scope, 0, bound_callback, &[callback.get(), context.get()])?;
    state.set(CALLBACK, callback)?;
    if let Some(error) = immediate {
        next_tick(
            scope,
            report_immediate,
            &[
                state_value.get(),
                js::value::from_bool(error.is_some()),
                error.map_or(js::value::undefined(), |error| error.get()),
            ],
        )?;
        return Ok(cleanup.to_jsval_throwing(scope)?);
    }

    if is_request(scope, stream_object)? {
        add_listener(scope, &state, stream_object, "complete", onfinish)?;
        if !will_emit_close {
            add_listener(scope, &state, stream_object, "abort", onclose)?;
        }
        if truthy(scope, stream_object.get_property(scope, c"req")?) {
            let request = required_obj(scope, stream_object, c"req")?;
            add_listener(scope, &state, request, "finish", onfinish)?;
        } else {
            add_listener(scope, &state, stream_object, "request", onrequest)?;
        }
    } else if writable && w_state.is_none() {
        add_listener(scope, &state, stream_object, "end", onlegacyfinish)?;
        add_listener(scope, &state, stream_object, "close", onlegacyfinish)?;
    }
    if !will_emit_close && stream_object.get_property(scope, c"aborted")?.is_boolean() {
        add_listener(scope, &state, stream_object, "aborted", onclose)?;
    }
    add_listener(scope, &state, stream_object, "end", onend)?;
    add_listener(scope, &state, stream_object, "finish", onfinish)?;
    if listen_errors {
        add_listener(scope, &state, stream_object, "error", onerror)?;
    }
    add_listener(scope, &state, stream_object, "close", onclose)?;

    if let Some(signal_object) = signal_object {
        state.set(SIGNAL, signal_object)?;
        let listener = closure(scope, 0, onabort, &[state_value.get()])?;
        state.set(ABORT_LISTENER, listener)?;
        listen(scope, signal_object, c"addEventListener", "abort", listener)?;
    }
    Ok(cleanup.to_jsval_throwing(scope)?)
}

/// Node's `isRequest`: whether `stream` has a truthy `setHeader` and an `abort` method.
fn is_request(scope: &Scope<'_>, stream: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(truthy(scope, stream.get_property(scope, c"setHeader")?)
        && is_function(scope, stream.get_property(scope, c"abort")?))
}

/// The callback of the promise `finished` returns, whose captures are `[promise, cleanup flag,
/// cleanup function]`.
fn settle(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let promise = captures
        .obj(0)?
        .cast::<js::Promise>()
        .map_err(|_| type_error(scope, "not a promise"))?;
    if captures.flag(1)? {
        let cleanup = captures.get(2)?;
        if is_function(scope, cleanup) {
            call(scope, HandleValue::undefined(), cleanup, &[])?;
        }
    }
    let error = args.get(0);
    if truthy(scope, error) {
        promise.reject(scope, error)?;
    } else {
        promise.resolve(scope, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

/// `stream/promises`' `finished(stream, options)`.
pub(crate) fn finished_promise<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let cleanup_option = match options.and_then(|options| as_object(scope, options)) {
        Some(options) => options.val(scope, c"cleanup")?,
        None => HandleValue::undefined(),
    };
    if truthy(scope, cleanup_option) && !cleanup_option.is_boolean() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "cleanup",
            "of type boolean",
            cleanup_option,
        )
        .throw(scope));
    }
    let auto_cleanup = truthy(scope, cleanup_option);
    let promise = js::Promise::new_pending(scope)?;
    let options = options.filter(|options| !options.is_null());
    let state = js::Array::with_contents(
        scope,
        &[
            promise.to_jsval_raw_throwing(scope)?,
            js::value::from_bool(auto_cleanup),
            js::value::undefined(),
        ],
    )?;
    let callback = Function::new_callback(scope, c"", 1, settle, *state)?;
    match eos(
        scope,
        stream,
        options,
        (*callback).to_jsval_throwing(scope)?,
    ) {
        Ok(cleanup) => state.set_element(scope, 2, cleanup)?,
        Err(error) => {
            error.throw(scope);
            promise.reject_with_pending(scope)?;
        }
    }
    Ok(promise)
}
