// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The IPC channel between a parent process and a child process, `fork`, and `_forkChild`.
//! Ported from Node.js's `setupChannel` with its `json` serialization, `fork` and `_forkChild`.
//!
//! The channel is a `net.Socket` over a pipe, which no property exposes. Its state is a
//! [`ChannelState`], the state of the `Control` object that the `channel` property of the
//! process at either end holds. A message is the JSON text of a value, followed by a newline.
//! Messages are emitted in the tick queue, and kept until the process has a `message` listener.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};
use platform::fs::FsError;

use super::options;
use crate::errors::{ErrorKind, NodeError, Thrown};

/// The number of bytes waiting to be sent from which `send` returns `false`.
const SEND_QUEUE_LIMIT: f64 = 65536.0 * 2.0;

/// The state of a `Control`, the `channel` of a process connected over IPC.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ChannelState {
    /// The `ChildProcess` or `process` at this end of the channel.
    target: Option<Heap<js::object::Object>>,
    socket: Option<Heap<js::object::Object>>,
    /// The descriptor of the pipe.
    #[no_trace]
    fd: Option<i32>,
    /// The number of `refCounted` calls without an `unrefCounted` call.
    #[no_trace]
    refs: u32,
    /// Whether `ref` or `unref` was called, after which `refCounted` and `unrefCounted` have no
    /// effect.
    #[no_trace]
    explicit: bool,
    /// The events and messages waiting for a `message` listener, in pairs.
    pending: Vec<Heap<Value>>,
    /// The text after the last newline received, a string, or `undefined` for an empty one.
    text: Heap<Value>,
    /// The UTF-8 decoder of the received bytes, which holds an incomplete character until the next
    /// chunk.
    #[no_trace]
    decoder: crate::string_decoder::NativeDecoderImpl,
    /// Whether `send` can send: unset by `disconnect`.
    #[no_trace]
    connected: bool,
    /// Whether the channel was closed, which `channel` reports as `null`.
    #[no_trace]
    closed: bool,
    /// Whether the target is a `ChildProcess`, whose `close` waits for the channel to end.
    #[no_trace]
    parent: bool,
}

#[core_runtime::jsmethods]
impl ChannelState {}

/// The target and the socket of the channel `control`.
fn parts<'s>(
    scope: &'s Scope<'_>,
    control: &ChannelState<'_>,
) -> (Option<Object<'s>>, Option<Object<'s>>) {
    let data = control.data();
    (
        data.target.as_ref().map(|target| target.get(scope)),
        data.socket.as_ref().map(|socket| socket.get(scope)),
    )
}

/// The state of the `Control` `control`.
fn state_of<'s>(scope: &'s Scope<'_>, control: Object<'s>) -> Result<ChannelState<'s>, ExnThrown> {
    control_class::state_of(scope, control)
}

/// Connect `target`, a `ChildProcess` if `parent` and `process` otherwise, to the process at the
/// other end of `pipe`, as Node.js's `setupChannel`: define its `channel`, `_channel`,
/// `_handleQueue`, `_pendingMessage`, `send`, `_send`, `connected`, `disconnect` and
/// `_disconnect`, and start reading. Without a pipe, the channel ends in the tick queue.
pub(super) fn setup<'s>(
    scope: &'s Scope<'_>,
    target: Object<'s>,
    pipe: Option<platform::child::Pipe>,
    parent: bool,
) -> Result<Object<'s>, Thrown> {
    let class = control_class::class_constructor(scope)?;
    let control = Function::construct(scope, class, &[] as &[HandleValue<'_>])?;
    let fd = pipe.as_ref().map(platform::child::Pipe::as_raw_fd);
    let socket = match pipe {
        Some(pipe) => {
            let options = Object::new_plain(scope)?;
            let class = crate::net::socket::socket_class::class_constructor(scope)?;
            let socket = Function::construct(scope, class, &[options.to_jsval_throwing(scope)?])?;
            crate::net::socket::attach_pipe(scope, socket, pipe)?;
            for (event, body) in [
                (c"data", on_data as js::function::Callback),
                (c"end", on_end),
                (c"error", on_error),
            ] {
                let listener = Function::new_callback(scope, c"", 1, body, control)?;
                crate::events::listen(scope, socket, c"on", event, *listener)?;
            }
            Some(socket)
        }
        None => None,
    };
    {
        let state = state_of(scope, control)?;
        let mut data = state.data_mut();
        data.target = Some(Heap::from(target));
        data.socket = socket.map(Heap::from);
        data.fd = fd;
        data.connected = true;
        data.parent = parent;
    }
    target.set_property(scope, c"channel", control)?;
    let getter = Function::new_callback(scope, c"get", 0, get_deprecated_channel, target)?;
    let setter = Function::new_callback(scope, c"set", 1, set_deprecated_channel, target)?;
    target.define_accessor(scope, c"_channel", Some(*getter), Some(*setter), 0)?;
    target.set_property(scope, c"_handleQueue", HandleValue::null())?;
    target.set_property(scope, c"_pendingMessage", HandleValue::null())?;
    let send = Function::new_callback(scope, c"", 4, send_method, control)?;
    target.set_property(scope, c"send", send)?;
    let send_now = Function::new_callback(scope, c"", 4, send_internal_method, control)?;
    target.set_property(scope, c"_send", send_now)?;
    target.set_property(scope, c"connected", true)?;
    let disconnect = Function::new_callback(scope, c"", 0, disconnect_method, control)?;
    target.set_property(scope, c"disconnect", disconnect)?;
    let disconnect_now =
        Function::new_callback(scope, c"", 0, disconnect_internal_method, control)?;
    target.set_property(scope, c"_disconnect", disconnect_now)?;
    let flush = Function::new_callback(scope, c"", 0, on_new_listener, control)?;
    crate::events::listen(scope, target, c"on", c"newListener", *flush)?;
    match socket {
        Some(socket) => {
            if !parent {
                crate::net::socket::socket_class::unref(scope, socket)?;
            }
            crate::net::socket::read_zero(scope, socket)?;
        }
        None => js::jobs::queue_native_tick(scope, end_tick, control, &[])?,
    }
    Ok(control)
}

/// The getter of the deprecated `_channel`, whose payload is the process: its `channel`.
fn get_deprecated_channel(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    warn_channel(scope)?;
    let target = js::function::cast_payload::<js::Object>(scope, payload);
    Ok(target.get_property(scope, c"channel")?.get())
}

/// The setter of the deprecated `_channel`, whose payload is the process: set its `channel`.
fn set_deprecated_channel(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    warn_channel(scope)?;
    let target = js::function::cast_payload::<js::Object>(scope, payload);
    target.set_property(scope, c"channel", args.get(0))?;
    Ok(js::value::undefined())
}

/// Emit the deprecation warning of `_channel`, once.
fn warn_channel(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    crate::process::deprecate_once(
        scope,
        "_channel is deprecated. Use ChildProcess.channel instead.",
        "DEP0129",
    )
}

/// The `data` listener of the channel's socket, whose payload is the `Control`: decode the
/// bytes as UTF-8, and parse and emit each complete line as a message, as Node.js's
/// `parseChannelMessages` does.
fn on_data(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    let state = state_of(scope, control)?;
    let chunk = crate::string_decoder::write_chunk(scope, args.get(0), |bytes| {
        state.data_mut().decoder.decode_chunk(bytes)
    })
    .map_err(|thrown| thrown.throw(scope))?;
    // SAFETY: the closure makes no call into SpiderMonkey.
    let newlines = unsafe {
        chunk.with_chars(scope, |chars| match chars {
            js::string::StrChars::Latin1(bytes) => {
                crate::support::try_collect(memchr::memchr_iter(b'\n', bytes))
            }
            js::string::StrChars::TwoByte(units) => crate::support::try_collect(
                (0..units.len()).filter(|&index| units[index] == u16::from(b'\n')),
            ),
        })
    }?
    .ok_or_else(|| {
        js::exception::report_out_of_memory(scope);
        ExnThrown
    })?;
    let text = state.data().text.get(scope);
    let text = if text.is_undefined() {
        js::JSString::empty(scope)
    } else {
        js::JSString::from_value(scope, text)?
    };
    let Some(&last) = newlines.last() else {
        state
            .data_mut()
            .text
            .set(text.concat(scope, chunk)?.as_value());
        return Ok(js::value::undefined());
    };
    let rest = chunk.substring(scope, last + 1, chunk.len() - last - 1)?;
    state.data_mut().text.set(rest.as_value());
    let mut start = 0;
    for end in newlines {
        let mut line = chunk.substring(scope, start, end - start)?;
        if start == 0 {
            line = text.concat(scope, line)?;
        }
        start = end + 1;
        let message = js::json::parse_js_string(scope, line.handle())?;
        let event = if is_internal(scope, message)? {
            c"internalMessage"
        } else {
            c"message"
        };
        handle_message(scope, control, event, message)?;
    }
    Ok(js::value::undefined())
}

/// Whether `message` is one of Node.js's internal messages: an object whose `cmd` is a string
/// longer than `NODE_` that starts with it.
fn is_internal(scope: &Scope<'_>, message: HandleValue<'_>) -> Result<bool, ExnThrown> {
    let Ok(object) = Object::from_value(scope, *message) else {
        return Ok(false);
    };
    let cmd = object.get_property(scope, c"cmd")?;
    Ok(crate::support::rust_string(scope, cmd)
        .filter(|_| cmd.is_string())
        .is_some_and(|cmd| cmd.len() > 5 && cmd.starts_with("NODE_")))
}

/// Emit `message` as the event `event` of the target of `control` in the tick queue, unless the
/// channel was closed.
fn handle_message(
    scope: &Scope<'_>,
    control: Object<'_>,
    event: &'static std::ffi::CStr,
    message: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if state_of(scope, control)?.data().closed {
        return Ok(());
    }
    let event = js::class::get_or_init_atom(scope, event)?;
    js::jobs::queue_native_tick(scope, emit_tick, control, &[event, message])
}

/// The tick callback whose payload is a `Control` and whose arguments are an event and a
/// message: emit the event on the target if it is `internalMessage` or the target has
/// `message` listeners, and keep it for the first listener otherwise.
fn emit_tick(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    let state = state_of(scope, control)?;
    let (target, _) = parts(scope, &state);
    let Some(target) = target else {
        return Ok(js::value::undefined());
    };
    let (event, message) = (args.get(0), args.get(1));
    let internal = crate::support::is_str(scope, event, c"internalMessage")?;
    let listened = {
        let name = js::class::get_or_init_atom(scope, c"message")?;
        crate::events::listener_count(scope, target, name)? > 0.0
    };
    if internal || listened {
        Function::call_by_name(
            scope,
            target.handle(),
            c"emit",
            &[event, message, HandleValue::undefined()],
        )?;
    } else if !state.data().closed {
        let mut data = state.data_mut();
        data.pending.push(Heap::from(event.get()));
        data.pending.push(Heap::from(message.get()));
    }
    Ok(js::value::undefined())
}

/// The `newListener` listener of the target, whose payload is the `Control`: emit the kept
/// messages in the tick queue if the target has `message` listeners then.
fn on_new_listener(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    js::jobs::queue_native_tick(scope, flush_tick, control, &[])?;
    Ok(js::value::undefined())
}

/// The tick callback whose payload is a `Control`: emit the kept messages on the target if it
/// has `message` listeners and the channel is open.
fn flush_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    let state = state_of(scope, control)?;
    let (target, _) = parts(scope, &state);
    let Some(target) = target else {
        return Ok(js::value::undefined());
    };
    let name = js::class::get_or_init_atom(scope, c"message")?;
    if state.data().closed || crate::events::listener_count(scope, target, name)? == 0.0 {
        return Ok(js::value::undefined());
    }
    let pending: Vec<HandleValue<'_>> = {
        let mut data = state.data_mut();
        let pending = data.pending.iter().map(|value| value.get(scope)).collect();
        data.pending.clear();
        pending
    };
    for pair in pending.chunks(2) {
        Function::call_by_name(
            scope,
            target.handle(),
            c"emit",
            &[pair[0], pair[1], HandleValue::undefined()],
        )?;
    }
    Ok(js::value::undefined())
}

/// The `end` listener of the channel's socket, whose payload is the `Control`.
fn on_end(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    channel_ended(scope, control)?;
    Ok(js::value::undefined())
}

/// The `error` listener of the channel's socket, whose payload is the `Control`: the channel
/// ends, as it does when the other end closes it.
fn on_error(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    channel_ended(scope, control)?;
    Ok(js::value::undefined())
}

/// The tick callback whose payload is a `Control` without a socket: the channel ends.
fn end_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    channel_ended(scope, control)?;
    Ok(js::value::undefined())
}

/// The end of the channel `control` from the other side, as Node.js's `onread` handles it:
/// disconnect, close the channel, and count the channel's close for the child's `close`.
fn channel_ended(scope: &Scope<'_>, control: Object<'_>) -> Result<(), ExnThrown> {
    let state = state_of(scope, control)?;
    if state.data().closed {
        return Ok(());
    }
    state.data_mut().text.set(js::value::undefined());
    disconnect(scope, control).map_err(|thrown| thrown.throw(scope))?;
    let (target, socket) = parts(scope, &state);
    if let Some(socket) = socket {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
    }
    let parent = state.data().parent;
    if let Some(target) = target {
        target.set_property(scope, c"channel", HandleValue::null())?;
        if parent {
            super::child::maybe_close(scope, target)?;
        }
    }
    Ok(())
}

/// `subprocess.disconnect()` and `process.disconnect()`, as Node.js's: emit
/// `ERR_IPC_DISCONNECTED` as an `error` if the channel is disconnected, and otherwise stop
/// sending, close the channel, and emit `disconnect` in the tick queue.
fn disconnect(scope: &Scope<'_>, control: Object<'_>) -> Result<(), Thrown> {
    let state = state_of(scope, control)?;
    let (target, _) = parts(scope, &state);
    let Some(target) = target else {
        return Ok(());
    };
    if !state.data().connected {
        let error = NodeError::new(
            ErrorKind::Error,
            "ERR_IPC_DISCONNECTED",
            "IPC channel is already disconnected",
        )
        .to_value(scope)?;
        crate::events::emit(scope, target, c"error", &[error])?;
        return Ok(());
    }
    state.data_mut().connected = false;
    target.set_property(scope, c"connected", false)?;
    disconnect_now(scope, control)
}

/// Node.js's `_disconnect`: mark the channel closed, and close it and emit `disconnect` in the
/// tick queue.
fn disconnect_now(scope: &Scope<'_>, control: Object<'_>) -> Result<(), Thrown> {
    let state = state_of(scope, control)?;
    let (target, _) = parts(scope, &state);
    if state.data().closed {
        return Ok(());
    }
    state.data_mut().closed = true;
    if let Some(target) = target {
        target.set_property(scope, c"channel", HandleValue::null())?;
    }
    js::jobs::queue_native_tick(scope, finish_disconnect, control, &[])?;
    Ok(())
}

/// The tick callback whose payload is a `Control` that disconnected: close the socket and emit
/// `disconnect` on the target.
fn finish_disconnect(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    let state = state_of(scope, control)?;
    let (target, socket) = parts(scope, &state);
    if let Some(socket) = socket {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
    }
    if let Some(target) = target {
        crate::events::emit(scope, target, c"disconnect", &[])?;
    }
    Ok(js::value::undefined())
}

/// `disconnect()` of a process, whose payload is its `Control`.
fn disconnect_method(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    disconnect(scope, control).map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::undefined())
}

/// `_disconnect()` of a process, whose payload is its `Control`.
fn disconnect_internal_method(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    disconnect_now(scope, control).map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::undefined())
}

/// `send(message, handle, options, callback)` of a process, whose payload is its `Control`:
/// send `message` while connected, and otherwise pass `ERR_IPC_CHANNEL_CLOSED` to `callback`, or
/// emit it as an `error`, in the tick queue.
fn send_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    let run = || -> Result<bool, Thrown> {
        let (message, mut handle, mut options, mut callback) =
            (args.get(0), args.get(1), args.get(2), args.get(3));
        if js::function::is_callable(handle) {
            callback = handle;
            handle = HandleValue::undefined();
            options = HandleValue::undefined();
        } else if js::function::is_callable(options) {
            callback = options;
            options = HandleValue::undefined();
        } else if !options.is_undefined() {
            crate::errors::validate_plain_object(scope, options, "options", false)?;
        }
        let state = state_of(scope, control)?;
        if state.data().connected {
            return send(scope, control, message, handle, options, callback);
        }
        let error = NodeError::new(ErrorKind::Error, "ERR_IPC_CHANNEL_CLOSED", "Channel closed")
            .to_value(scope)?;
        if js::function::is_callable(callback) {
            js::jobs::queue_tick(scope, callback, &[error])?;
        } else if let (Some(target), _) = parts(scope, &state) {
            crate::support::emit_later(scope, target, c"error", &[error])?;
        }
        Ok(false)
    };
    let sent = run().map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::from_bool(sent))
}

/// `_send(message, handle, options, callback)` of a process, whose payload is its `Control`.
fn send_internal_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let control = js::function::cast_payload::<js::Object>(scope, payload);
    let sent = send(
        scope,
        control,
        args.get(0),
        args.get(1),
        args.get(2),
        args.get(3),
    )
    .map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::from_bool(sent))
}

/// The state of a message being sent: the channel, the callback, and whether errors are only
/// passed to the callback.
#[core_runtime::jsclass(hidden)]
pub(crate) struct SendRequest {
    control: Option<Heap<js::object::Object>>,
    callback: Option<Heap<js::object::Object>>,
    #[no_trace]
    swallow_errors: bool,
}

#[core_runtime::jsmethods]
impl SendRequest {}

/// Node.js's `_send`: send the JSON text of `message` and a newline. Returns whether fewer bytes
/// than [`SEND_QUEUE_LIMIT`] wait to be sent. `callback` gets `null` once the message is sent,
/// or the error, which is an `EPIPE` error in the tick queue for a channel without a socket. Without a callback, the error is emitted as an `error` unless
/// `options.swallowErrors`, which `options` gives as a boolean too. Sending a handle, a
/// `net.Socket`, `net.Server` or `dgram.Socket`, throws `ERR_METHOD_NOT_IMPLEMENTED`, and other
/// handles throw `ERR_INVALID_HANDLE_TYPE`.
fn send(
    scope: &Scope<'_>,
    control: Object<'_>,
    message: HandleValue<'_>,
    handle: HandleValue<'_>,
    options: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<bool, Thrown> {
    if message.is_undefined() {
        return Err(NodeError::missing_args(&["message"]).into());
    }
    let valid = message.is_string()
        || message.is_number()
        || message.is_boolean()
        || (message.is_object() && !js::function::is_callable(message))
        || message.is_null();
    if !valid {
        return Err(NodeError::invalid_arg_type(
            scope,
            "message",
            "one of type string, object, number, or boolean",
            message,
        )
        .into());
    }
    if js::conversion::to_boolean(handle) {
        let Ok(object) = Object::from_value(scope, *handle) else {
            return Err(invalid_handle());
        };
        let sendable = crate::net::socket::socket_class::try_state_of(scope, object)?.is_some()
            || crate::net::server::server_class::try_state_of(scope, object)?.is_some()
            || crate::dgram::socket_class::try_state_of(scope, object)?.is_some();
        if !sendable {
            return Err(invalid_handle());
        }
        return Err(NodeError::not_implemented("send() with a handle").into());
    }
    let swallow_errors = if options.is_boolean() {
        options.to_boolean()
    } else {
        match Object::from_value(scope, *options) {
            Ok(options) => {
                js::conversion::to_boolean(options.get_property(scope, c"swallowErrors")?)
            }
            Err(_) => false,
        }
    };
    let mut text = js::json::stringify(scope, message, None, None)?.unwrap_or_default();
    text.push('\n');
    let state = state_of(scope, control)?;
    let (_, socket) = parts(scope, &state);
    ref_counted(scope, control, true)?;
    let request = js::class::create_instance_with::<SendRequestImpl>(scope, |_| SendRequestImpl {
        control: Some(Heap::from(control)),
        callback: Object::from_value(scope, *callback)
            .ok()
            .filter(|callback| callback.is_callable())
            .map(Heap::from),
        swallow_errors,
    })?;
    let done = Function::new_callback(scope, c"", 1, sent, request)?;
    let done = (*done).to_jsval_throwing(scope)?;
    let Some(socket) = socket else {
        let error = super::child::errno_exception(scope, FsError::from_code("EPIPE"), "write")?;
        crate::support::next_tick_call(scope, done, &[error.to_jsval_throwing(scope)?])?;
        return Ok(true);
    };
    let chunk = text.as_str().to_jsval_throwing(scope)?;
    crate::stream::writable_class::write(scope, socket, chunk, None, Some(done))?;
    let queued = socket.get_property(scope, c"writableLength")?;
    Ok(!(queued.is_number() && queued.to_number() >= SEND_QUEUE_LIMIT))
}

/// `ERR_INVALID_HANDLE_TYPE`, for a handle `send` can't send.
fn invalid_handle() -> Thrown {
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_INVALID_HANDLE_TYPE",
        "This handle type cannot be sent",
    )
    .into()
}

/// The write callback of a message, whose payload is the [`SendRequest`]: release the reference
/// the write held, and call the callback with `null` or the error, or emit the error.
fn sent(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<SendRequest>(scope, payload);
    let (control, callback, swallow_errors) = {
        let data = request.data();
        (
            data.control.as_ref().map(|control| control.get(scope)),
            data.callback.as_ref().map(|callback| callback.get(scope)),
            data.swallow_errors,
        )
    };
    let Some(control) = control else {
        return Ok(js::value::undefined());
    };
    ref_counted(scope, control, false)?;
    let error = args.get(0);
    if js::conversion::to_boolean(error) {
        if swallow_errors {
            return Ok(js::value::undefined());
        }
        match callback {
            Some(callback) => {
                Function::call(scope, HandleValue::undefined(), callback, &[error])?;
            }
            None => {
                let state = state_of(scope, control)?;
                if let (Some(target), _) = parts(scope, &state) {
                    crate::events::emit(scope, target, c"error", &[error])?;
                }
            }
        }
    } else if let Some(callback) = callback {
        Function::call(
            scope,
            HandleValue::undefined(),
            callback,
            &[HandleValue::null()],
        )?;
    }
    Ok(js::value::undefined())
}

/// `refCounted()` for `add`, and `unrefCounted()` otherwise: reference the channel's socket when
/// the count becomes 1, and unreference it when it becomes 0, unless `ref` or `unref` was
/// called.
fn ref_counted(scope: &Scope<'_>, control: Object<'_>, add: bool) -> Result<(), ExnThrown> {
    let state = state_of(scope, control)?;
    let change = {
        let mut data = state.data_mut();
        if add {
            data.refs += 1;
            (data.refs == 1 && !data.explicit).then_some(true)
        } else {
            data.refs = data.refs.saturating_sub(1);
            (data.refs == 0 && !data.explicit).then_some(false)
        }
    };
    if let Some(referenced) = change {
        set_referenced(scope, &state, referenced)?;
    }
    Ok(())
}

/// Reference the channel's socket of `state`, or unreference it.
fn set_referenced(
    scope: &Scope<'_>,
    state: &ChannelState<'_>,
    referenced: bool,
) -> Result<(), ExnThrown> {
    let (_, socket) = parts(scope, state);
    let Some(socket) = socket else {
        return Ok(());
    };
    if referenced {
        crate::net::socket::socket_class::ref_impl(scope, socket)?;
    } else {
        crate::net::socket::socket_class::unref(scope, socket)?;
    }
    Ok(())
}

/// The `newListener` and `removeListener` listeners of `process` in a child, whose payload is
/// `true` for `newListener`: count the `message` and `disconnect` listeners with
/// `refCounted` and `unrefCounted`.
fn on_listener_change(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let name = args.get(0);
    if !(crate::support::is_str(scope, name, c"message")?
        || crate::support::is_str(scope, name, c"disconnect")?)
    {
        return Ok(js::value::undefined());
    }
    let process = crate::support::this_object(scope, &args)?;
    let control = process.get_property(scope, c"channel")?;
    let Ok(control) = Object::from_value(scope, *control) else {
        return Ok(js::value::undefined());
    };
    if control_class::try_state_of(scope, control)?.is_some() {
        ref_counted(scope, control, payload.to_boolean())?;
    }
    Ok(js::value::undefined())
}

/// Node.js's `_forkChild(fd)`: connect `process` to its parent over the pipe on the descriptor
/// `fd`, which doesn't keep the event loop alive unless `process` has `message` or `disconnect`
/// listeners.
pub(super) fn fork_child(scope: &Scope<'_>, fd: HandleValue<'_>) -> Result<(), Thrown> {
    let fd = crate::errors::validate_integer(scope, fd, "fd", 0.0, 2147483647.0)? as i32;
    let pipe = platform::child::Pipe::open(fd)
        .map_err(|error| crate::fs::UvError::new(error, "open").throw(scope))?;
    let process = crate::process::process_value(scope)?;
    let process = Object::from_value(scope, *process)?;
    setup(scope, process, Some(pipe), false)?;
    for (event, added) in [(c"newListener", true), (c"removeListener", false)] {
        let name = if added {
            c"onNewListener"
        } else {
            c"onRemoveListener"
        };
        let listener = Function::new_callback(scope, name, 1, on_listener_change, added)?;
        crate::events::listen(scope, process, c"on", event, *listener)?;
    }
    Ok(())
}

/// Connect `process` to the parent that forked this process, if the environment names the
/// channel's descriptor in `NODE_CHANNEL_FD`, and remove `NODE_CHANNEL_FD` and
/// `NODE_CHANNEL_SERIALIZATION_MODE` from `process.env`, as Node.js does before running the
/// entry script. Does nothing where processes can't be spawned, and after its first call. An
/// error, such as one for a descriptor that isn't a socket, is reported, and `process` stays
/// unconnected.
pub(crate) fn connect_to_parent(scope: &Scope<'_>) {
    if connect(scope).is_err() {
        js::exception::report_and_clear(scope, "connecting to the parent process");
    }
}

/// The body of [`connect_to_parent`].
fn connect(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static CONNECTED: AtomicBool = AtomicBool::new(false);
    if !platform::child::SUPPORTED || CONNECTED.load(Ordering::Relaxed) {
        return Ok(());
    }
    let Some(fd) = platform::process::env_vars()
        .into_iter()
        .find(|(key, _)| key == "NODE_CHANNEL_FD")
        .map(|(_, value)| value)
    else {
        return Ok(());
    };
    CONNECTED.store(true, Ordering::Relaxed);
    let process = crate::process::process_value(scope)?;
    let Ok(process) = Object::from_value(scope, *process) else {
        return Ok(());
    };
    if let Ok(env) = Object::from_value(scope, *process.get_property(scope, c"env")?) {
        env.delete_property(scope, c"NODE_CHANNEL_FD")?;
        env.delete_property(scope, c"NODE_CHANNEL_SERIALIZATION_MODE")?;
    }
    let Ok(fd) = fd.trim().parse::<i32>() else {
        return Ok(());
    };
    let fd = scope.root_value(js::value::from_f64(f64::from(fd)));
    fork_child(scope, fd).map_err(|thrown| thrown.throw(scope))
}

/// `child_process.fork(modulePath, args, options)`, as Node.js's `fork`: spawn `options.execPath`
/// with `options.execArgv` or `process.execArgv`, the module and `args`, with an IPC channel on
/// the descriptor of `ipc` in `options.stdio`, and the standard streams inherited, or piped with
/// `options.silent`.
pub(super) fn fork<'s>(
    scope: &'s Scope<'_>,
    module_path: HandleValue<'_>,
    args: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let module_path = crate::fs::path_arg(scope, module_path, "modulePath")?;
    let mut options = scope.root_value(options.get());
    let args: Vec<HandleValue<'_>> = if args.is_null_or_undefined() {
        Vec::new()
    } else if args.is_object()
        && !js::function::is_callable(args)
        && !js::Array::is_array_value(scope, args)?
    {
        options = scope.root_value(args.get());
        Vec::new()
    } else {
        options::validate_array(scope, args, "args")?
    };
    let copy = Object::new_with_null_prototype(scope)?;
    if !options.is_null_or_undefined() {
        let given = crate::errors::validate_plain_object(scope, options, "options", false)?;
        copy.assign(scope, given)?;
    }
    copy.set_property(scope, c"shell", false)?;
    let process = crate::process::process_value(scope)?;
    let process = Object::from_value(scope, *process)?;
    let exec_path = copy.get_property(scope, c"execPath")?;
    if !js::conversion::to_boolean(exec_path) {
        copy.set_property(
            scope,
            c"execPath",
            process.get_property(scope, c"execPath")?,
        )?;
    }
    let exec_path = copy.get_property(scope, c"execPath")?;
    options::null_check(scope, exec_path, "options.execPath")?;
    let mut exec_argv = copy.get_property(scope, c"execArgv")?;
    if !js::conversion::to_boolean(exec_argv) {
        exec_argv = process.get_property(scope, c"execArgv")?;
    }
    let exec_argv = crate::support::list_from_array_like(scope, exec_argv)?;
    for (index, arg) in exec_argv.iter().enumerate() {
        options::null_check(scope, *arg, &format!("options.execArgv[{index}]"))?;
    }
    let mut argv = exec_argv;
    argv.push(module_path.as_str().to_jsval_throwing(scope)?);
    argv.extend(args);
    let stdio = copy.get_property(scope, c"stdio")?;
    let stdio = if stdio.is_string() || !js::Array::is_array_value(scope, stdio)? {
        let name = if stdio.is_string() {
            String::from_jsval(scope, stdio, ())?
        } else if js::conversion::to_boolean(copy.get_property(scope, c"silent")?) {
            "pipe".to_string()
        } else {
            "inherit".to_string()
        };
        let mut entries: Vec<HandleValue<'_>> = match name.as_str() {
            "ignore" | "overlapped" | "pipe" => {
                let name = name.as_str().to_jsval_throwing(scope)?;
                vec![name, name, name]
            }
            "inherit" => (0..3)
                .map(|fd| scope.root_value(js::value::from_f64(f64::from(fd))))
                .collect(),
            _ => {
                let name = name.as_str().to_jsval_throwing(scope)?;
                return Err(
                    NodeError::invalid_arg_value(scope, "stdio", name, "is invalid").into(),
                );
            }
        };
        entries.push("ipc".to_jsval_throwing(scope)?);
        js::Array::with_contents(scope, &entries)?.to_jsval_throwing(scope)?
    } else {
        let entries = crate::support::list_from_array_like(scope, stdio)?;
        let mut has_ipc = false;
        for entry in &entries {
            has_ipc |= crate::support::is_str(scope, *entry, c"ipc")?;
        }
        if !has_ipc {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_CHILD_PROCESS_IPC_REQUIRED",
                "Forked processes must have an IPC channel, missing value 'ipc' in options.stdio",
            )
            .into());
        }
        stdio
    };
    copy.set_property(scope, c"stdio", stdio)?;
    let argv = js::Array::with_contents(scope, &argv)?.to_jsval_throwing(scope)?;
    super::spawn(scope, exec_path, argv, copy.to_jsval_throwing(scope)?)
}

#[core_runtime::jsfunction_class(
    name = "Control",
    extends = "crate::events::event_emitter",
    state = "ChannelState",
    invalid_this = "crate::errors::invalid_this",
    class_syntax
)]
pub mod control_class {
    use super::*;

    /// Node.js's internal `Control`, the `channel` of a process connected over IPC.
    #[constructor(length = 1)]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)
    }

    /// Count a reason to keep the channel referenced.
    pub fn ref_counted(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        super::ref_counted(scope, this, true)
    }

    /// Release a reason counted by `refCounted`.
    pub fn unref_counted(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        super::ref_counted(scope, this, false)
    }

    /// <https://nodejs.org/api/child_process.html#subprocesschannelref>: the channel keeps the
    /// event loop alive, whatever its listeners.
    #[method(name = "ref")]
    pub fn ref_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        state.data_mut().explicit = true;
        super::set_referenced(scope, &state, true)
    }

    /// <https://nodejs.org/api/child_process.html#subprocesschannelunref>: the channel doesn't
    /// keep the event loop alive, whatever its listeners.
    pub fn unref(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        state.data_mut().explicit = true;
        super::set_referenced(scope, &state, false)
    }

    /// Define the `fd` getter.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"fd", 0, super::get_fd, ())?;
        proto.define_accessor(scope, c"fd", Some(*getter), None, 0)
    }
}

/// The getter of `Control.prototype.fd`: the descriptor of the channel's pipe.
fn get_fd(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Ok(control) = Object::from_value(scope, args.this()) else {
        return Ok(js::value::undefined());
    };
    let fd = match control_class::try_state_of(scope, control)? {
        Some(state) => state.data().fd,
        None => None,
    };
    Ok(fd.map_or(js::value::undefined(), |fd| {
        js::value::from_f64(f64::from(fd))
    }))
}

/// Register the classes of the IPC channel on `global`.
pub(super) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    ChannelState::add_to_global(scope, global);
    SendRequest::add_to_global(scope, global);
    control_class::register(scope)?;
    Ok(())
}
