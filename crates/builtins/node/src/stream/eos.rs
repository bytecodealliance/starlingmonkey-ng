// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `finished`: Node's `lib/internal/streams/end-of-stream.js`.

use std::ffi::CStr;

use js::async_context::Snapshot;
use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, Callback, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::util::{
    is_closed, is_node_stream, is_readable, is_readable_finished, is_readable_node_stream,
    is_writable, is_writable_finished, is_writable_node_stream, side_errored, sides,
    will_emit_close, Field,
};
use crate::errors::{
    validate_abort_signal, validate_function, validate_plain_object, ErrorKind, NodeError, Thrown,
};
use crate::events::listen;

/// The options of [`eos_native`]: Node's `eos` options, and its internal
/// `kEosNodeSynchronousCallback`.
pub(crate) struct EosOptions<'a> {
    /// Whether to wait for the readable side, or `None` to wait for it if the stream is readable.
    pub(crate) readable: Option<bool>,
    /// Whether to wait for the writable side, or `None` to wait for it if the stream is writable.
    pub(crate) writable: Option<bool>,
    /// Whether an `error` event finishes the stream.
    pub(crate) error: bool,
    pub(crate) signal: Option<Object<'a>>,
    /// Whether a result known when `eos` is called is passed to the callback before it returns,
    /// instead of in a tick.
    pub(crate) synchronous: bool,
}

impl Default for EosOptions<'_> {
    fn default() -> Self {
        EosOptions {
            readable: None,
            writable: None,
            error: true,
            signal: None,
            synchronous: false,
        }
    }
}

/// The callback of [`eos_native`]: a JS function, which runs with the stream as `this` in the
/// async context of the `eos` call, or a native callback with its payload.
#[derive(Clone, Copy)]
pub(crate) enum EosCallback<'a> {
    Js(HandleValue<'a>),
    Native(Callback, HandleValue<'a>),
}

/// A listener `eos` added, which its cleanup removes.
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct EosListener {
    emitter: Heap<js::object::Object>,
    #[no_trace]
    event: &'static CStr,
    listener: Heap<js::object::Object>,
}

/// The state of an `eos` call.
#[core_runtime::jsclass(hidden)]
pub(crate) struct EosState {
    stream: Heap<js::object::Object>,
    /// The JS callback, or the payload of `native`.
    callback: Heap<Value>,
    #[no_trace]
    native: Option<Callback>,
    /// The async context of the `eos` call, in which a JS callback runs.
    #[no_trace]
    context: Snapshot,
    #[no_trace]
    readable: bool,
    #[no_trace]
    writable: bool,
    #[no_trace]
    will_emit_close: bool,
    #[no_trace]
    readable_finished: bool,
    #[no_trace]
    writable_finished: bool,
    /// Set once the callback ran, or the cleanup made it a no-op.
    #[no_trace]
    called: bool,
    listeners: Vec<EosListener>,
    signal: Option<Heap<js::object::Object>>,
    abort_listener: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl EosState {}

/// Call the callback once with `error`, with `this` the stream, and detach the abort listener.
fn complete(
    scope: &Scope<'_>,
    state: EosState<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    if state.data().called {
        return Ok(());
    }
    state.data_mut().called = true;
    let (signal, abort_listener) = {
        let data = state.data();
        (data.signal.get(scope), data.abort_listener.get(scope))
    };
    if let (Some(signal), Some(listener)) = (signal, abort_listener) {
        listen(scope, signal, c"removeEventListener", c"abort", listener)?;
    }
    let (stream, callback, native, context) = {
        let data = state.data();
        (
            data.stream.get(scope),
            data.callback.get(scope),
            data.native,
            data.context.clone(),
        )
    };
    let args: Vec<HandleValue<'_>> = error.into_iter().collect();
    match native {
        Some(native) => {
            native(scope, CallbackArgs::from_values(&args), callback)?;
        }
        None => {
            let previous = context.enter();
            let result = Function::call(scope, stream, callback, &args);
            previous.enter();
            result?;
        }
    }
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
    let state = cast_payload::<EosState>(scope, payload);
    state.data_mut().writable_finished = true;
    let stream = state.data().stream.get(scope);
    if js::conversion::to_boolean(stream.get_property(scope, c"destroyed")?) {
        state.data_mut().will_emit_close = false;
    }
    let (readable, will_emit_close, readable_finished) = {
        let data = state.data();
        (data.readable, data.will_emit_close, data.readable_finished)
    };
    if will_emit_close
        && (!js::conversion::to_boolean(stream.get_property(scope, c"readable")?) || readable)
    {
        return Ok(js::value::undefined());
    }
    if !readable || readable_finished {
        complete(scope, state, None)?;
    }
    Ok(js::value::undefined())
}

fn onend(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<EosState>(scope, payload);
    state.data_mut().readable_finished = true;
    let stream = state.data().stream.get(scope);
    if js::conversion::to_boolean(stream.get_property(scope, c"destroyed")?) {
        state.data_mut().will_emit_close = false;
    }
    let (writable, will_emit_close, writable_finished) = {
        let data = state.data();
        (data.writable, data.will_emit_close, data.writable_finished)
    };
    if will_emit_close
        && (!js::conversion::to_boolean(stream.get_property(scope, c"writable")?) || writable)
    {
        return Ok(js::value::undefined());
    }
    if !writable || writable_finished {
        complete(scope, state, None)?;
    }
    Ok(js::value::undefined())
}

fn onlegacyfinish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<EosState>(scope, payload)
        .data()
        .stream
        .get(scope);
    if !js::conversion::to_boolean(stream.get_property(scope, c"writable")?) {
        return onfinish(scope, args, payload);
    }
    Ok(js::value::undefined())
}

fn onerror(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    complete(
        scope,
        cast_payload::<EosState>(scope, payload),
        Some(args.get(0)),
    )?;
    Ok(js::value::undefined())
}

/// Node's `getEosErrored`: the error of either side, unless it is a boolean.
fn eos_errored<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'s>,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let sides = sides(scope, stream)?;
    let errored = match side_errored(scope, stream, c"writableErrored", sides.writable)? {
        Some(error) => Some(error),
        None => side_errored(scope, stream, c"readableErrored", sides.readable)?,
    };
    Ok(errored.filter(|error| !error.is_boolean() && js::conversion::to_boolean(*error)))
}

/// Node's `getEosOnCloseError`: the error `finished` reports once the stream closed. That is
/// the error of either side, or `ERR_STREAM_PREMATURE_CLOSE` if a side `finished` checks didn't
/// finish, or `None`.
fn close_error<'s>(
    scope: &'s Scope<'_>,
    state: EosState<'_>,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let stream = state.data().stream.get(scope);
    if let Some(error) = eos_errored(scope, stream)? {
        return Ok(Some(error));
    }
    let (readable, readable_finished, writable, writable_finished) = {
        let data = state.data();
        (
            data.readable,
            data.readable_finished,
            data.writable,
            data.writable_finished,
        )
    };
    let stream_value = stream.to_jsval_throwing(scope)?;
    if readable
        && !readable_finished
        && is_readable_node_stream(scope, stream_value, true)?
        && is_readable_finished(scope, stream_value, false)? != Some(true)
    {
        return Ok(Some(premature_close().to_value(scope)?));
    }
    if writable
        && !writable_finished
        && is_writable_finished(scope, stream_value, false)? != Some(true)
    {
        return Ok(Some(premature_close().to_value(scope)?));
    }
    Ok(None)
}

fn onclose(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<EosState>(scope, payload);
    let error = close_error(scope, state)?;
    complete(scope, state, error)?;
    Ok(js::value::undefined())
}

/// The listener for `request` of an HTTP request: listen for `finish` on `stream.req`.
fn onrequest(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<EosState>(scope, payload);
    let stream = state.data().stream.get(scope);
    let request = stream.get_property(scope, c"req")?;
    let request = Object::from_value(scope, request)
        .map_err(|_| js::error::TypeError("req is not an object".into()).throw(scope))?;
    let finish = listener_function(scope, state, onfinish)?;
    add_listener(scope, state, request, c"finish", finish)?;
    Ok(js::value::undefined())
}

/// The tick that reports the result `eos` computed when it was called, whose payload is the
/// [`EosState`] and whose argument, if any, is the error.
fn report_immediate(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let error = (!args.is_empty()).then(|| args.get(0));
    complete(scope, cast_payload::<EosState>(scope, payload), error)?;
    Ok(js::value::undefined())
}

/// The abort listener: remove the listeners and call the callback with an `AbortError`, unless
/// it ran or the cleanup ran.
fn onabort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<EosState>(scope, payload);
    if state.data().called {
        return Ok(js::value::undefined());
    }
    let signal = state.data().signal.get(scope);
    let reason = match signal {
        Some(signal) => Some(signal.get_property(scope, c"reason")?),
        None => None,
    };
    let error = crate::errors::abort_error(scope, reason)?;
    cleanup(scope, state)?;
    state.data_mut().called = false;
    complete(scope, state, Some(error))?;
    Ok(js::value::undefined())
}

/// A listener function running `body` for `state`.
fn listener_function<'s>(
    scope: &'s Scope<'_>,
    state: EosState<'s>,
    body: Callback,
) -> Result<Object<'s>, ExnThrown> {
    Ok(*Function::new_callback(scope, c"", 1, body, state)?)
}

/// Add `listener` for `event` on `emitter`, and record it for the cleanup.
fn add_listener(
    scope: &Scope<'_>,
    state: EosState<'_>,
    emitter: Object<'_>,
    event: &'static CStr,
    listener: Object<'_>,
) -> Result<(), ExnThrown> {
    listen(scope, emitter, c"on", event, listener)?;
    state.data_mut().listeners.push(EosListener {
        emitter: Heap::from(emitter),
        event,
        listener: Heap::from(listener),
    });
    Ok(())
}

fn nop(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// `eos`'s cleanup: remove its listeners, and make the callback a no-op.
pub(crate) fn cleanup(scope: &Scope<'_>, state: EosState<'_>) -> Result<(), ExnThrown> {
    state.data_mut().called = true;
    let listeners: Vec<(Object<'_>, &'static CStr, Object<'_>)> = state
        .data()
        .listeners
        .iter()
        .map(|entry| {
            (
                entry.emitter.get(scope),
                entry.event,
                entry.listener.get(scope),
            )
        })
        .collect();
    for (emitter, event, listener) in listeners {
        listen(scope, emitter, c"removeListener", event, listener)?;
    }
    Ok(())
}

/// The cleanup function `finished` returns, whose payload is the [`EosState`].
fn cleanup_callback(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    cleanup(scope, cast_payload::<EosState>(scope, payload))?;
    Ok(js::value::undefined())
}

/// Node's `eos` for a stream `is_node_stream` accepts: call `callback` once `stream` finished,
/// closed or failed. Returns the state, which [`cleanup`] takes. If the stream already finished,
/// closed or failed, or the signal already aborted, the callback runs in a tick, or before this
/// returns with `options.synchronous`, and no listeners are added, except a no-op `error`
/// listener unless `options.error` is `false`.
pub(crate) fn eos_native<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    options: EosOptions<'_>,
    callback: EosCallback<'_>,
) -> Result<EosState<'s>, Thrown> {
    let stream_value = stream.to_jsval_throwing(scope)?;
    let readable = match options.readable {
        Some(readable) => readable,
        None => is_readable_node_stream(scope, stream_value, false)?,
    };
    let writable = match options.writable {
        Some(writable) => writable,
        None => is_writable_node_stream(scope, stream_value)?,
    };
    let will_emit_close = will_emit_close(scope, stream)?
        && is_readable_node_stream(scope, stream_value, false)? == readable
        && is_writable_node_stream(scope, stream_value)? == writable;
    let writable_finished = is_writable_finished(scope, stream_value, false)? == Some(true);
    let readable_finished = is_readable_finished(scope, stream_value, false)? == Some(true);
    let (callback, native) = match callback {
        EosCallback::Js(callback) => (callback, None),
        EosCallback::Native(native, payload) => (payload, Some(native)),
    };
    let state = js::class::create_instance_with::<EosStateImpl>(scope, |_| EosStateImpl {
        stream: Heap::from(stream),
        callback: Heap::from(callback.get()),
        native,
        context: Snapshot::capture(),
        readable,
        writable,
        will_emit_close,
        readable_finished,
        writable_finished,
        called: false,
        listeners: Vec::new(),
        signal: None,
        abort_listener: None,
    })?;

    let sides = sides(scope, stream)?;
    let error_emitted = |side: Option<super::util::Side<'_>>| -> Result<bool, ExnThrown> {
        match side {
            Some(side) => side.error_emitted(scope),
            None => Ok(false),
        }
    };
    let pending_writes = match sides.writable {
        Some(w) => {
            let pending = w.get(scope, Field::Pendingcb)?;
            !(pending.is_undefined() || (pending.is_number() && pending.to_number() == 0.0))
        }
        None => false,
    };
    // `Some(None)` is a result without an error.
    let mut immediate: Option<Option<HandleValue<'_>>> = None;
    if is_closed(scope, stream)? == Some(true) {
        immediate = Some(close_error(scope, state)?);
    } else if error_emitted(sides.writable)? || error_emitted(sides.readable)? {
        if !will_emit_close {
            immediate = Some(eos_errored(scope, stream)?);
        }
    } else if (!readable
        && (!will_emit_close || is_readable(scope, stream_value)? == Some(true))
        && (writable_finished || is_writable(scope, stream_value)? == Some(false))
        && !pending_writes)
        || (!writable
            && (!will_emit_close || is_writable(scope, stream_value)? == Some(true))
            && (readable_finished || is_readable(scope, stream_value)? == Some(false)))
        || (sides.readable.is_some()
            && js::conversion::to_boolean(stream.get_property(scope, c"req")?)
            && js::conversion::to_boolean(stream.get_property(scope, c"aborted")?))
    {
        immediate = Some(eos_errored(scope, stream)?);
    }
    if immediate.is_some() {
        if options.error {
            let nop = js::class::get_or_init_shared_function(
                scope,
                nop as *const () as usize,
                |scope| Function::new_callback(scope, c"nop", 0, nop, ()),
            )?;
            add_listener(scope, state, stream, c"error", *nop)?;
        }
    } else if let Some(signal) = options.signal {
        if js::conversion::to_boolean(signal.get_property(scope, c"aborted")?) {
            let reason = signal.get_property(scope, c"reason")?;
            immediate = Some(Some(crate::errors::abort_error(scope, Some(reason))?));
        }
    }

    if let Some(error) = immediate {
        if options.synchronous {
            complete(scope, state, error)?;
        } else {
            let args: Vec<HandleValue<'_>> = error.into_iter().collect();
            js::jobs::queue_native_tick(scope, report_immediate, state, &args)?;
        }
        return Ok(state);
    }

    let onfinish_fn = listener_function(scope, state, onfinish)?;
    let onclose_fn = listener_function(scope, state, onclose)?;
    if super::destroy::is_request(scope, stream)? {
        add_listener(scope, state, stream, c"complete", onfinish_fn)?;
        if !will_emit_close {
            add_listener(scope, state, stream, c"abort", onclose_fn)?;
        }
        let request = stream.get_property(scope, c"req")?;
        if js::conversion::to_boolean(request) {
            let request = Object::from_value(scope, request)
                .map_err(|_| js::error::TypeError("req is not an object".into()).throw(scope))?;
            add_listener(scope, state, request, c"finish", onfinish_fn)?;
        } else {
            let onrequest_fn = listener_function(scope, state, onrequest)?;
            add_listener(scope, state, stream, c"request", onrequest_fn)?;
        }
    } else if writable && sides.writable.is_none() {
        let legacy = listener_function(scope, state, onlegacyfinish)?;
        add_listener(scope, state, stream, c"end", legacy)?;
        add_listener(scope, state, stream, c"close", legacy)?;
    }
    if !will_emit_close && stream.get_property(scope, c"aborted")?.is_boolean() {
        add_listener(scope, state, stream, c"aborted", onclose_fn)?;
    }
    let onend_fn = listener_function(scope, state, onend)?;
    add_listener(scope, state, stream, c"end", onend_fn)?;
    add_listener(scope, state, stream, c"finish", onfinish_fn)?;
    if options.error {
        let onerror_fn = listener_function(scope, state, onerror)?;
        add_listener(scope, state, stream, c"error", onerror_fn)?;
    }
    add_listener(scope, state, stream, c"close", onclose_fn)?;

    if let Some(signal) = options.signal {
        let listener = listener_function(scope, state, onabort)?;
        {
            let mut data = state.data_mut();
            data.signal = Some(Heap::from(signal));
            data.abort_listener = Some(Heap::from(listener));
        }
        listen(scope, signal, c"addEventListener", c"abort", listener)?;
    }
    Ok(state)
}

/// The cleanup function `eos` returns for `state`.
pub(crate) fn cleanup_function<'s>(
    scope: &'s Scope<'_>,
    state: EosState<'s>,
) -> Result<Object<'s>, ExnThrown> {
    Ok(*Function::new_callback(
        scope,
        c"cleanup",
        0,
        cleanup_callback,
        state,
    )?)
}

/// Node's `eos`, `stream.finished(stream, options, callback)` with `options` given: returns the
/// cleanup function.
pub(crate) fn eos<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
    callback: HandleValue<'_>,
) -> Result<HandleValue<'s>, Thrown> {
    let state = eos_checked(scope, stream, options, EosCallback::Js(callback))?;
    Ok(cleanup_function(scope, state)?.to_jsval_throwing(scope)?)
}

/// [`eos_native`] for JS arguments, which it validates as Node's `eos` does.
fn eos_checked<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
    callback: EosCallback<'_>,
) -> Result<EosState<'s>, Thrown> {
    let options = match options.filter(|options| !options.is_null_or_undefined()) {
        Some(options) => Some(validate_plain_object(scope, options, "options", false)?),
        None => None,
    };
    if let EosCallback::Js(callback) = callback {
        validate_function(scope, callback, "callback")?;
    }
    let option = |name: &'static CStr| -> Result<HandleValue<'_>, ExnThrown> {
        match options {
            Some(options) => options.get_property(scope, name),
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
    let stream = Object::from_value(scope, stream).expect("streams are objects");
    let side_option = |name: &'static CStr| -> Result<Option<bool>, ExnThrown> {
        let value = option(name)?;
        Ok((!value.is_null_or_undefined()).then(|| js::conversion::to_boolean(value)))
    };
    let readable = side_option(c"readable")?;
    let writable = side_option(c"writable")?;
    let error = option(c"error")?;
    let options = EosOptions {
        readable,
        writable,
        error: !(error.is_boolean() && !error.to_boolean()),
        signal: Object::from_value(scope, signal).ok(),
        synchronous: false,
    };
    eos_native(scope, stream, options, callback)
}

/// The state of the promise `stream/promises`' `finished` returns: the promise, and the `eos`
/// state to clean up once it settles if `cleanup` was requested.
#[core_runtime::jsclass(hidden)]
pub(crate) struct FinishedState {
    promise: Heap<js::promise::Promise>,
    cleanup: Option<Heap<EosStateImpl>>,
}

#[core_runtime::jsmethods]
impl FinishedState {}

/// The `eos` callback of `stream/promises`' `finished`, whose payload is a [`FinishedState`].
fn settle(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FinishedState>(scope, payload);
    let (promise, eos_state) = {
        let data = state.data();
        (data.promise.get(scope), data.cleanup.get(scope))
    };
    if let Some(eos_state) = eos_state {
        cleanup(scope, eos_state)?;
    }
    let error = args.get(0);
    if !args.is_empty() && js::conversion::to_boolean(error) {
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
    let cleanup_option = match options.and_then(|options| Object::from_value(scope, options).ok()) {
        Some(options) => options.get_property(scope, c"cleanup")?,
        None => HandleValue::undefined(),
    };
    if js::conversion::to_boolean(cleanup_option) && !cleanup_option.is_boolean() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "cleanup",
            "of type boolean",
            cleanup_option,
        )
        .throw(scope));
    }
    let auto_cleanup = js::conversion::to_boolean(cleanup_option);
    let promise = js::Promise::new_pending(scope)?;
    let state =
        js::class::create_instance_with::<FinishedStateImpl>(scope, |_| FinishedStateImpl {
            promise: Heap::from(promise),
            cleanup: None,
        })?;
    let options = options.filter(|options| !options.is_null());
    let payload = state.to_jsval_throwing(scope)?;
    match eos_checked(scope, stream, options, EosCallback::Native(settle, payload)) {
        Ok(eos_state) => {
            if auto_cleanup {
                state.data_mut().cleanup = Some(Heap::from(eos_state));
            }
        }
        Err(error) => {
            error.throw(scope);
            promise.reject_with_pending(scope)?;
        }
    }
    Ok(promise)
}
