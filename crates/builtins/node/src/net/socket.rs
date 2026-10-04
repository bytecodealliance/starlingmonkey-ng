// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `net.Socket`: a `Duplex` over a `platform::net` TCP connection.
//!
//! The connection is in a `SocketHandle` under `_starlingHandle`. Each read and write takes its
//! half of the connection out of the handle for as long as it runs. `destroy` cancels the connect,
//! read and write in progress and drops the connection.

use std::net::{IpAddr, SocketAddr};

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};
use platform::net::{Connection, ConnectionReader, ConnectionWriter, NetError};

use super::blocklist::Rules;
use crate::buffer::{encode_string, new_buffer, Encoding};
use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};
use crate::stream::{call_method, closure, is_function, string, truthy, Captures, Props};

/// The connection of a socket, without the halves that a read or write in progress holds.
#[core_runtime::jsclass(hidden)]
pub struct SocketHandle {
    #[no_trace]
    reader: Option<ConnectionReader>,
    #[no_trace]
    writer: Option<ConnectionWriter>,
    /// Cancels the read in progress.
    #[no_trace]
    cancel_read: Option<futures_channel::oneshot::Sender<()>>,
    /// Cancels the write or shutdown in progress.
    #[no_trace]
    cancel_write: Option<futures_channel::oneshot::Sender<()>>,
    /// Whether the socket was destroyed, so a read or write that finishes drops its half.
    #[no_trace]
    closed: bool,
    #[no_trace]
    local_addr: Option<SocketAddr>,
    #[no_trace]
    peer_addr: Option<SocketAddr>,
}

#[core_runtime::jsmethods]
impl SocketHandle {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// The cancellation of a connect in progress, which `destroy` triggers.
#[core_runtime::jsclass(hidden)]
pub struct PendingConnect {
    #[no_trace]
    cancel: Option<futures_channel::oneshot::Sender<()>>,
}

#[core_runtime::jsmethods]
impl PendingConnect {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// Throw the Node.js error for `error` in `syscall`, with the message `<syscall> <code> <detail>`
/// and `errno`, `code`, `syscall` and the extra properties `extra`.
pub(crate) fn throw_net_error(
    scope: &Scope<'_>,
    error: &NetError,
    syscall: &str,
    detail: Option<&str>,
    extra: &[(&'static std::ffi::CStr, HandleValue<'_>)],
) -> ExnThrown {
    let code = error.code();
    let errno = match code {
        "ENOTFOUND" => -3008,
        "EAI_AGAIN" => -3001,
        code => platform::fs::FsError::from_code(code).errno,
    };
    let message = match detail {
        Some(detail) => format!("{syscall} {code} {detail}"),
        None => format!("{syscall} {code}"),
    };
    let message = std::ffi::CString::new(message.replace('\0', "\\0")).expect("NULs were replaced");
    js::error::throw_plain_error(scope, &message);
    let error = crate::stream::take_exception(scope);
    let set = || -> Result<(), ExnThrown> {
        let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
        object.set_property(scope, c"errno", errno)?;
        object.set_property(scope, c"code", code)?;
        object.set_property(scope, c"syscall", syscall)?;
        for (name, value) in extra {
            object.set_property(scope, name, *value)?;
        }
        Ok(())
    };
    if set().is_err() {
        return ExnThrown;
    }
    js::exception::set_pending(scope, error, js::native::ExceptionStackBehavior::Capture);
    ExnThrown
}

/// The family name of `addr`, `IPv4` or `IPv6`.
fn family(addr: &SocketAddr) -> &'static str {
    if addr.is_ipv4() {
        "IPv4"
    } else {
        "IPv6"
    }
}

/// Make `connection` the connection of `socket`.
pub(super) fn attach(
    scope: &Scope<'_>,
    socket: Object<'_>,
    connection: Connection,
) -> Result<(), ExnThrown> {
    let Connection {
        reader,
        writer,
        local_addr,
        peer_addr,
    } = connection;
    let handle =
        js::class::create_instance_with::<SocketHandleImpl>(scope, |_| SocketHandleImpl {
            reader: Some(reader),
            writer: Some(writer),
            cancel_read: None,
            cancel_write: None,
            closed: false,
            local_addr: Some(local_addr),
            peer_addr: Some(peer_addr),
        })?;
    socket.define_property(
        scope,
        c"_starlingHandle",
        handle.to_jsval_throwing(scope)?,
        0,
    )?;
    socket.set_property(scope, c"connecting", false)?;
    Ok(())
}

/// Drop the connection of `socket` and cancel its read and write in progress, without emitting
/// anything.
pub(super) fn release(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    if let Some(handle) = handle_of(scope, socket)? {
        let mut state = handle.data_mut();
        if let Some(cancel) = state.cancel_read.take() {
            let _ = cancel.send(());
        }
        if let Some(cancel) = state.cancel_write.take() {
            let _ = cancel.send(());
        }
        state.reader = None;
        state.writer = None;
        state.closed = true;
    }
    socket.set_property(scope, c"_starlingHandle", js::value::undefined())
}

/// The handle of `socket`, if it is connected and not destroyed.
fn handle_of<'s>(
    scope: &'s Scope<'_>,
    socket: Object<'_>,
) -> Result<Option<SocketHandle<'s>>, ExnThrown> {
    Ok(socket
        .obj(scope, c"_starlingHandle")?
        .and_then(|handle| handle.cast::<SocketHandle>().ok()))
}

/// Restart the idle timer of `socket`, if it has one.
fn touch(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    let timeout = socket.num(scope, c"timeout")?;
    if timeout > 0.0 {
        arm_timer(scope, socket, timeout)?;
    }
    Ok(())
}

/// The reason a connect failed.
enum ConnectFailure {
    /// A network error, and the address it occurred for, if any.
    Net(NetError, Option<SocketAddr>),
    /// The socket's `BlockList` blocks the address.
    Blocked(IpAddr),
}

/// The body of the connect: resolve `host`, and connect to its first address of the address
/// family `family` (4, 6, or 0 for both) that `rules` doesn't block and that accepts.
async fn connect_to(
    host: String,
    port: u16,
    family: u8,
    rules: Option<Rules>,
) -> Result<Connection, ConnectFailure> {
    let addrs = platform::net::resolve(&host)
        .await
        .map_err(|error| ConnectFailure::Net(error, None))?;
    let mut last = None;
    let wanted = |addr: &IpAddr| match family {
        4 => addr.is_ipv4(),
        6 => addr.is_ipv6(),
        _ => true,
    };
    for addr in addrs.into_iter().filter(wanted) {
        if rules.as_ref().is_some_and(|rules| rules.blocks(addr)) {
            last = Some(ConnectFailure::Blocked(addr));
            continue;
        }
        let addr = SocketAddr::new(addr, port);
        match platform::net::connect_stream(addr, None).await {
            Ok(connection) => return Ok(connection),
            Err(error) => last = Some(ConnectFailure::Net(error, Some(addr))),
        }
    }
    Err(last.unwrap_or(ConnectFailure::Net(NetError::NoAddress(host), None)))
}

/// Start the first read of `socket` unless it is paused, so `end` is emitted even without a
/// consumer.
pub(super) fn read_zero(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    let paused = call_method(scope, socket, c"isPaused", &[])?;
    if !truthy(scope, paused) {
        let zero = scope.root_value(js::value::from_f64(0.0));
        call_method(scope, socket, c"read", &[zero])?;
    }
    Ok(())
}

/// The fulfillment reaction of a connect, whose captures are `[socket]`: emit `connect` and
/// `ready`, and start reading.
fn on_connect(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Captures::of(scope, payload)?.obj(0)?;
    if socket.flag(scope, c"destroyed")? || handle_of(scope, socket)?.is_none() {
        return Ok(js::value::undefined());
    }
    touch(scope, socket)?;
    call_method(scope, socket, c"emit", &[string(scope, "connect")?])?;
    call_method(scope, socket, c"emit", &[string(scope, "ready")?])?;
    if socket.flag(scope, c"_starlingReadPending")? {
        socket.set_property(scope, c"_starlingReadPending", false)?;
        start_read(scope, socket)?;
    }
    read_zero(scope, socket)?;
    Ok(js::value::undefined())
}

/// The rejection reaction of an operation on a socket, whose captures are `[socket]`: destroy
/// the socket with the error.
fn on_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Captures::of(scope, payload)?.obj(0)?;
    call_method(scope, socket, c"destroy", &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// Connect `socket` to `port` on `host`, keeping only addresses of the address family `family`
/// that `rules` doesn't block.
fn connect(
    scope: &Scope<'_>,
    socket: Object<'_>,
    host: String,
    port: u16,
    family: u8,
    rules: Option<Rules>,
) -> Result<(), ExnThrown> {
    socket.set_property(scope, c"connecting", true)?;
    let (sender, cancelled) = futures_channel::oneshot::channel();
    let pending =
        js::class::create_instance_with::<PendingConnectImpl>(scope, |_| PendingConnectImpl {
            cancel: Some(sender),
        })?;
    socket.define_property(
        scope,
        c"_starlingConnect",
        pending.to_jsval_throwing(scope)?,
        0,
    )?;
    let target = RootedHeap::<js::object::Object>::new(socket);
    let future = async move {
        let connect = async { Some(connect_to(host.clone(), port, family, rules).await) };
        let cancel = async {
            let _ = cancelled.await;
            None
        };
        let result = futures_lite::future::or(connect, cancel).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let socket = target.get(scope);
            let Some(result) = result else {
                return Ok(scope.root_value(js::value::undefined()));
            };
            if socket.flag(scope, c"destroyed")? {
                return Ok(scope.root_value(js::value::undefined()));
            }
            socket.set_property(scope, c"_starlingConnect", js::value::undefined())?;
            match result {
                Ok(connection) => {
                    attach(scope, socket, connection)?;
                    Ok(scope.root_value(js::value::undefined()))
                }
                Err(ConnectFailure::Blocked(ip)) => Err(NodeError::new(
                    ErrorKind::Error,
                    "ERR_IP_BLOCKED",
                    format!("IP({ip}) is blocked by net.BlockList"),
                )
                .throw(scope)),
                Err(ConnectFailure::Net(error, Some(addr))) => {
                    let address = js::JSString::from_str(scope, &addr.ip().to_string())?
                        .to_jsval_throwing(scope)?;
                    let port = scope.root_value(js::value::from_f64(f64::from(addr.port())));
                    Err(throw_net_error(
                        scope,
                        &error,
                        "connect",
                        Some(&addr.to_string()),
                        &[(c"address", address), (c"port", port)],
                    ))
                }
                Err(ConnectFailure::Net(error, None)) => {
                    let hostname =
                        js::JSString::from_str(scope, &host)?.to_jsval_throwing(scope)?;
                    Err(throw_net_error(
                        scope,
                        &error,
                        "getaddrinfo",
                        Some(&host),
                        &[(c"hostname", hostname)],
                    ))
                }
            }
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    let on_connect = closure(
        scope,
        0,
        on_connect,
        &[socket.to_jsval_raw_throwing(scope)?],
    )?;
    let on_error = closure(scope, 1, on_error, &[socket.to_jsval_raw_throwing(scope)?])?;
    promise.then(scope, Some(on_connect), Some(on_error))?;
    Ok(())
}

/// The fulfillment reaction of a read, whose captures are `[socket]`: push the `Buffer` read, or
/// `null` at the end. A cancelled read is fulfilled with `undefined`.
fn on_read(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Captures::of(scope, payload)?.obj(0)?;
    let chunk = args.get(0);
    if chunk.is_undefined() {
        return Ok(js::value::undefined());
    }
    if let Some(view) = Object::from_value(scope, chunk)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
    {
        let read = socket.num(scope, c"bytesRead")?;
        socket.set_property(scope, c"bytesRead", read + view.byte_length() as f64)?;
        touch(scope, socket)?;
    }
    call_method(scope, socket, c"push", &[chunk])?;
    if chunk.is_null() {
        // Reading after the end emits `end` even without a consumer.
        let zero = scope.root_value(js::value::from_f64(0.0));
        call_method(scope, socket, c"read", &[zero])?;
    }
    Ok(js::value::undefined())
}

/// Read the next bytes of `socket`'s connection, unless a read is in progress.
fn start_read(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    let Some(handle) = handle_of(scope, socket)? else {
        return Ok(());
    };
    let (sender, cancelled) = futures_channel::oneshot::channel();
    let Some(mut reader) = ({
        let mut state = handle.data_mut();
        let reader = state.reader.take();
        if reader.is_some() {
            state.cancel_read = Some(sender);
        }
        reader
    }) else {
        return Ok(());
    };
    let target = RootedHeap::<js::object::Object>::new(*handle);
    let future = async move {
        let read = async { Some(reader.read().await) };
        let cancel = async {
            let _ = cancelled.await;
            None
        };
        let result = futures_lite::future::or(read, cancel).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let Some(result) = result else {
                return Ok(scope.root_value(js::value::undefined()));
            };
            if let Ok(handle) = target.get(scope).cast::<SocketHandle>() {
                let mut state = handle.data_mut();
                if state.closed {
                    return Ok(scope.root_value(js::value::undefined()));
                }
                state.cancel_read = None;
                state.reader = Some(reader);
            }
            match result {
                Ok(Some(bytes)) => new_buffer(scope, &bytes)?.to_jsval_throwing(scope),
                Ok(None) => Ok(scope.root_value(js::value::null())),
                Err(error) => Err(throw_net_error(scope, &error, "read", None, &[])),
            }
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    let on_read = closure(scope, 1, on_read, &[socket.to_jsval_raw_throwing(scope)?])?;
    let on_error = closure(scope, 1, on_error, &[socket.to_jsval_raw_throwing(scope)?])?;
    promise.then(scope, Some(on_read), Some(on_error))?;
    Ok(())
}

/// The fulfillment reaction of a write, whose captures are `[socket, callback]`: restart the idle
/// timer and call the callback. A cancelled write is fulfilled with `undefined`.
fn on_written(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if args.get(0).is_undefined() {
        return Ok(js::value::undefined());
    }
    let captures = Captures::of(scope, payload)?;
    let socket = captures.obj(0)?;
    touch(scope, socket)?;
    Function::call(
        scope,
        js::value::undefined(),
        captures.get(1)?,
        js::function::EmptyArgs,
    )?;
    Ok(js::value::undefined())
}

/// The rejection reaction of a write or shutdown, whose captures are `[socket, callback]`: call
/// the callback with the error.
fn on_write_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    Function::call(
        scope,
        js::value::undefined(),
        captures.get(1)?,
        &[args.get(0)],
    )?;
    Ok(js::value::undefined())
}

/// The fulfillment reaction of a shutdown, whose captures are `[socket, callback]`: call the
/// callback. A cancelled shutdown is fulfilled with `undefined`.
fn on_shutdown(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if args.get(0).is_undefined() {
        return Ok(js::value::undefined());
    }
    let captures = Captures::of(scope, payload)?;
    Function::call(
        scope,
        js::value::undefined(),
        captures.get(1)?,
        js::function::EmptyArgs,
    )?;
    Ok(js::value::undefined())
}

/// Call `callback` with `ERR_SOCKET_CLOSED`.
fn socket_closed(scope: &Scope<'_>, callback: HandleValue<'_>) -> Result<(), ExnThrown> {
    let error = crate::stream::error_value(
        scope,
        NodeError::new(ErrorKind::Error, "ERR_SOCKET_CLOSED", "Socket is closed"),
    )?;
    Function::call(scope, js::value::undefined(), callback, &[error])?;
    Ok(())
}

/// Write `bytes` to `socket`'s connection, or shut its sending side down for `None`, and call
/// `callback` once done. The bytes the connection takes without waiting are written at once, and
/// `callback` is called before this returns if that is all of them.
fn write_or_shutdown(
    scope: &Scope<'_>,
    socket: Object<'_>,
    bytes: Option<Vec<u8>>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let Some(handle) = handle_of(scope, socket)? else {
        return socket_closed(scope, callback);
    };
    let bytes = match bytes {
        Some(bytes) if !bytes.is_empty() => {
            let unwritten = match handle.data_mut().writer.as_mut() {
                Some(writer) => writer.try_write(bytes),
                None => bytes,
            };
            if unwritten.is_empty() {
                touch(scope, socket)?;
                Function::call(
                    scope,
                    js::value::undefined(),
                    callback,
                    js::function::EmptyArgs,
                )?;
                return Ok(());
            }
            Some(unwritten)
        }
        bytes => bytes,
    };
    let (sender, cancelled) = futures_channel::oneshot::channel();
    let Some(mut writer) = ({
        let mut state = handle.data_mut();
        let writer = state.writer.take();
        if writer.is_some() {
            state.cancel_write = Some(sender);
        }
        writer
    }) else {
        return socket_closed(scope, callback);
    };
    let shutdown = bytes.is_none();
    let target = RootedHeap::<js::object::Object>::new(*handle);
    let future = async move {
        let operation = async {
            Some(match bytes {
                Some(bytes) => writer.write(bytes).await,
                None => writer.shutdown().await,
            })
        };
        let cancel = async {
            let _ = cancelled.await;
            None
        };
        let result = futures_lite::future::or(operation, cancel).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let Some(result) = result else {
                return Ok(scope.root_value(js::value::undefined()));
            };
            if let Ok(handle) = target.get(scope).cast::<SocketHandle>() {
                let mut state = handle.data_mut();
                if state.closed {
                    return Ok(scope.root_value(js::value::undefined()));
                }
                state.cancel_write = None;
                state.writer = Some(writer);
            }
            result.map_err(|error| {
                throw_net_error(
                    scope,
                    &error,
                    if shutdown { "shutdown" } else { "write" },
                    None,
                    &[],
                )
            })?;
            Ok(scope.root_value(js::value::from_bool(true)))
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    let done = if shutdown {
        closure(
            scope,
            1,
            on_shutdown,
            &[socket.to_jsval_raw_throwing(scope)?, callback.get()],
        )?
    } else {
        closure(
            scope,
            1,
            on_written,
            &[socket.to_jsval_raw_throwing(scope)?, callback.get()],
        )?
    };
    let failed = closure(
        scope,
        1,
        on_write_error,
        &[socket.to_jsval_raw_throwing(scope)?, callback.get()],
    )?;
    promise.then(scope, Some(done), Some(failed))?;
    Ok(())
}

/// The `connect` listener that retries a write or shutdown that came while connecting, whose
/// captures are `[socket, chunk, callback]`. A `null` chunk means shut down.
fn retry_after_connect(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let socket = captures.obj(0)?;
    let chunk = captures.get(1)?;
    let bytes = if chunk.is_null() {
        None
    } else {
        Some(
            Object::from_value(scope, *chunk)
                .ok()
                .and_then(js::ArrayBufferView::from_object)
                .map(|view| view.copy_bytes(scope))
                .transpose()?
                .unwrap_or_default(),
        )
    };
    write_or_shutdown(scope, socket, bytes, captures.get(2)?)?;
    Ok(js::value::undefined())
}

/// The bytes of the written chunk `chunk`: a `Buffer`, or a string in `encoding`.
fn chunk_bytes(
    scope: &Scope<'_>,
    chunk: HandleValue<'_>,
    encoding: HandleValue<'_>,
) -> Result<Vec<u8>, ExnThrown> {
    Ok(if chunk.is_string() {
        let encoding = String::from_jsval(scope, encoding, ())
            .ok()
            .and_then(|name| Encoding::parse(&name))
            .unwrap_or_default();
        encode_string(scope, js::JSString::from_value(scope, chunk)?, encoding)?
    } else {
        Object::from_value(scope, *chunk)
            .ok()
            .and_then(js::ArrayBufferView::from_object)
            .map(|view| view.copy_bytes(scope))
            .transpose()?
            .unwrap_or_default()
    })
}

/// Write `bytes` to `socket`, once it is connected, and call `callback` once done.
fn write_bytes(
    scope: &Scope<'_>,
    socket: Object<'_>,
    bytes: Vec<u8>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let dispatched = socket.num(scope, c"_starlingBytesDispatched")? + bytes.len() as f64;
    socket.set_property(scope, c"_starlingBytesDispatched", dispatched)?;
    if socket.flag(scope, c"connecting")? {
        let buffer = new_buffer(scope, &bytes)?.to_jsval_throwing(scope)?;
        when_connected(scope, socket, buffer, callback)?;
        return Ok(());
    }
    write_or_shutdown(scope, socket, Some(bytes), callback)
}

/// Run the write of `chunk`, a `Buffer`, or the shutdown for `null`, once `socket` is connected.
fn when_connected(
    scope: &Scope<'_>,
    socket: Object<'_>,
    chunk: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<bool, ExnThrown> {
    if !socket.flag(scope, c"connecting")? {
        return Ok(false);
    }
    let retry = closure(
        scope,
        0,
        retry_after_connect,
        &[
            socket.to_jsval_raw_throwing(scope)?,
            chunk.get(),
            callback.get(),
        ],
    )?;
    call_method(
        scope,
        socket,
        c"once",
        &[string(scope, "connect")?, retry.to_jsval_throwing(scope)?],
    )?;
    Ok(true)
}

/// The timer callback of the idle timer, whose captures are `[socket]`: emit `timeout`.
fn on_timeout(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Captures::of(scope, payload)?.obj(0)?;
    socket.set_property(scope, c"_starlingTimer", js::value::undefined())?;
    call_method(scope, socket, c"emit", &[string(scope, "timeout")?])?;
    Ok(js::value::undefined())
}

/// Clear the idle timer of `socket`, if it is pending.
fn clear_timer(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    let timer = socket.val(scope, c"_starlingTimer")?;
    if !timer.is_undefined() {
        let clear = scope.global().val(scope, c"clearTimeout")?;
        Function::call(scope, js::value::undefined(), clear, &[timer])?;
        socket.set_property(scope, c"_starlingTimer", js::value::undefined())?;
    }
    Ok(())
}

/// Start the idle timer of `socket`, which emits `timeout` after `timeout` milliseconds and does
/// not keep the event loop alive, replacing the pending one.
fn arm_timer(scope: &Scope<'_>, socket: Object<'_>, timeout: f64) -> Result<(), ExnThrown> {
    clear_timer(scope, socket)?;
    let fire = closure(
        scope,
        0,
        on_timeout,
        &[socket.to_jsval_raw_throwing(scope)?],
    )?;
    let set = scope.global().val(scope, c"setTimeout")?;
    let delay = scope.root_value(js::value::from_f64(timeout));
    let timer = Function::call(
        scope,
        js::value::undefined(),
        set,
        &[fire.to_jsval_throwing(scope)?, delay],
    )?;
    if let Ok(timer) = Object::from_value(scope, *timer) {
        if crate::stream::has_method(scope, timer, c"unref")? {
            call_method(scope, timer, c"unref", &[])?;
        }
    }
    socket.define_property(scope, c"_starlingTimer", timer, 0)
}

/// The tick callback that emits `close` on a destroyed socket, whose captures are `[socket,
/// hadError]`. `close` gets no argument if `hadError` is `undefined`.
fn emit_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let socket = captures.obj(0)?;
    let had_error = captures.get(1)?;
    let close = string(scope, "close")?;
    if had_error.is_undefined() {
        call_method(scope, socket, c"emit", &[close])?;
    } else {
        call_method(scope, socket, c"emit", &[close, had_error])?;
    }
    Ok(js::value::undefined())
}

/// The address `which` of `socket`'s connection, local or remote, as `{ address, family, port }`,
/// or `None` without a connection.
fn address_object<'s>(
    scope: &'s Scope<'_>,
    socket: Object<'_>,
    remote: bool,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let Some(handle) = handle_of(scope, socket)? else {
        return Ok(None);
    };
    let addr = if remote {
        handle.data().peer_addr
    } else {
        handle.data().local_addr
    };
    let Some(addr) = addr else {
        return Ok(None);
    };
    let object = Object::new_plain(scope)?;
    object.set_property(scope, c"address", addr.ip().to_string().as_str())?;
    object.set_property(scope, c"family", family(&addr))?;
    object.set_property(scope, c"port", f64::from(addr.port()))?;
    Ok(Some(object))
}

/// The names of the address getters, in the order of their payloads.
const ADDRESS_GETTERS: [(&std::ffi::CStr, bool, &std::ffi::CStr); 6] = [
    (c"remoteAddress", true, c"address"),
    (c"remoteFamily", true, c"family"),
    (c"remotePort", true, c"port"),
    (c"localAddress", false, c"address"),
    (c"localFamily", false, c"family"),
    (c"localPort", false, c"port"),
];

/// The getter of the address property whose index in [`ADDRESS_GETTERS`] is the payload. A
/// remote address read while connected stays readable after the socket closes, as in Node.js.
fn address_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let (_, remote, field) = ADDRESS_GETTERS[payload.to_number() as usize];
    let address = match address_object(scope, socket, remote)? {
        Some(address) => {
            if remote {
                socket.define_property(scope, c"_peername", address, 0)?;
            }
            Some(address)
        }
        None if remote => socket.obj(scope, c"_peername")?,
        None => None,
    };
    match address {
        Some(address) => Ok(address.get_property(scope, field)?.get()),
        None => Ok(js::value::undefined()),
    }
}

/// The getter of `bytesWritten`: the bytes handed to the connection, or waiting for it to connect,
/// and the bytes the stream buffers, as Node.js counts them.
fn bytes_written(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let mut bytes = socket.num(scope, c"_starlingBytesDispatched")?;
    if let Some(buffered) = socket.obj(scope, c"writableBuffer")? {
        let length = buffered
            .cast::<js::Array>()
            .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))?
            .length(scope)?;
        for index in 0..length {
            let Ok(entry) = Object::from_value(scope, buffered.get_element(scope, index)?) else {
                continue;
            };
            let chunk = entry.val(scope, c"chunk")?;
            let encoding = entry.val(scope, c"encoding")?;
            bytes += crate::buffer::buffer::byte_length(scope, chunk, Some(encoding))
                .map_err(|thrown| thrown.throw(scope))?;
        }
    }
    Ok(js::value::from_f64(bytes))
}

/// The getter of `pending`: whether the socket is not connected yet.
fn pending_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let pending = handle_of(scope, socket)?.is_none() || socket.flag(scope, c"connecting")?;
    Ok(js::value::from_bool(pending))
}

/// The getter of `readyState`.
fn ready_state(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let state = if socket.flag(scope, c"connecting")? {
        "opening"
    } else if socket.flag(scope, c"destroyed")? || handle_of(scope, socket)?.is_none() {
        "closed"
    } else {
        match (
            socket.flag(scope, c"readable")?,
            socket.flag(scope, c"writable")?,
        ) {
            (true, true) => "open",
            (true, false) => "readOnly",
            (false, true) => "writeOnly",
            (false, false) => "closed",
        }
    };
    js::JSString::from_str(scope, state)?.to_jsval_raw_throwing(scope)
}

/// The port, host and address family `connect`'s options give, as Node.js's `lookupAndConnect`
/// validates them.
fn connect_target(scope: &Scope<'_>, options: Object<'_>) -> Result<(u16, String, u8), Thrown> {
    let host = options.val(scope, c"host")?;
    let host = if truthy(scope, host) {
        let host = crate::errors::validate_string(scope, host, "options.host")?;
        if host.contains('\0') {
            return Err(NodeError::invalid_arg_value(
                scope,
                "options.host",
                options.val(scope, c"host")?,
                "must be a string without null bytes",
            )
            .into());
        }
        host
    } else {
        "localhost".to_string()
    };
    let port = options.val(scope, c"port")?;
    let port = if port.is_undefined() {
        0
    } else {
        if !port.is_number() && !port.is_string() {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options.port",
                "one of type number or string",
                port,
            )
            .into());
        }
        super::validate_port(scope, port, "Port")?
    };
    let family = options.val(scope, c"family")?;
    let family = if family.is_number() {
        family.to_number() as u8
    } else {
        let name = if family.is_string() {
            Some(String::from_jsval(scope, family, ())?)
        } else {
            None
        };
        match name.as_deref() {
            Some("IPv4") => 4,
            Some("IPv6") => 6,
            _ => 0,
        }
    };
    Ok((port, host, family))
}

#[core_runtime::jsfunction_class(
    name = "Socket",
    extends = "crate::stream::duplex_class",
    self_construct
)]
pub mod socket_class {
    use super::*;

    /// <https://nodejs.org/api/net.html#new-netsocketoptions>: the stream options `options`
    /// apply, `signal` destroys the socket when aborted, and `connect` skips the addresses that
    /// `blockList` blocks. `fd` and `handle` are not supported.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let options = options.filter(|options| options.is_object());
        let base = Object::new_plain(scope)?;
        if let Some(options) = options {
            let assign = scope.global().obj(scope, c"Object")?.ok_or(ExnThrown)?;
            call_method(
                scope,
                assign,
                c"assign",
                &[base.to_jsval_throwing(scope)?, options],
            )?;
        }
        let allow_half_open = truthy(scope, base.val(scope, c"allowHalfOpen")?);
        base.set_property(scope, c"allowHalfOpen", allow_half_open)?;
        base.set_property(scope, c"emitClose", false)?;
        base.set_property(scope, c"autoDestroy", true)?;
        crate::stream::init_duplex(scope, this, Some(base.to_jsval_throwing(scope)?))?;
        for (name, value) in [(c"connecting", false), (c"_starlingReadPending", false)] {
            this.set_property(scope, name, value)?;
        }
        this.define_property(scope, c"_starlingBytesDispatched", 0, 0)?;
        for name in [c"bytesRead", c"timeout"] {
            this.set_property(scope, name, 0)?;
        }
        let signal = base.val(scope, c"signal")?;
        if truthy(scope, signal) {
            crate::stream::add_abort_signal(scope, signal, this.to_jsval_throwing(scope)?)?;
        }
        let block_list = base.val(scope, c"blockList")?;
        if truthy(scope, block_list) {
            super::super::blocklist::validate_block_list(scope, block_list, "options.blockList")?;
            this.set_property(scope, c"blockList", block_list)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#socketconnect>: connect to `port` on `host`, which
    /// defaults to `localhost`, from `(port[, host][, listener])`, `(options[, listener])`, or an
    /// array of `[options, listener]` that `net._normalizeArgs` returned.
    /// The option `family` keeps only addresses of that family. IPC paths are not supported.
    pub fn connect<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        if let Some((options, listener)) = match args.first() {
            Some(first) => super::super::normalized_args(scope, *first)?,
            None => None,
        } {
            return connect(
                scope,
                this,
                js::class::RestArgs::new(vec![options, listener]),
            );
        }
        let first = args.first().copied();
        let options = match first {
            Some(first) if first.is_object() && !is_function(scope, first) => {
                Object::from_value(scope, *first).map_err(|e| Thrown::from(e.throw(scope)))?
            }
            Some(first) if super::super::is_pipe_name(scope, first)? => {
                return Err(NodeError::not_implemented("connect() to an IPC path").into());
            }
            Some(first) => {
                let options = Object::new_plain(scope)?;
                options.set_property(scope, c"port", first)?;
                if let Some(host) = args.get(1).filter(|host| host.is_string()) {
                    options.set_property(scope, c"host", *host)?;
                }
                options
            }
            None => Object::new_plain(scope)?,
        };
        if let Some(listener) = args
            .last()
            .copied()
            .filter(|last| is_function(scope, *last))
        {
            call_method(scope, this, c"once", &[string(scope, "connect")?, listener])?;
        }
        let path = options.val(scope, c"path")?;
        if options.val(scope, c"port")?.is_undefined() && (path.is_null() || path.is_undefined()) {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_MISSING_ARGS",
                "The \"options\" or \"port\" or \"path\" argument must be specified",
            )
            .into());
        }
        if truthy(scope, path) {
            return Err(NodeError::not_implemented("connect() to an IPC path").into());
        }
        let (port, host, family) = super::connect_target(scope, options)?;
        let rules = crate::net::blocklist::rules_of(scope, this.val(scope, c"blockList")?)?;
        super::connect(scope, this, host, port, family, rules)?;
        Ok(this)
    }

    #[method(name = "_read")]
    pub fn read_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if super::handle_of(scope, this)?.is_none() {
            return this.set_property(scope, c"_starlingReadPending", true);
        }
        super::start_read(scope, this)
    }

    /// Write `chunk`, a `Buffer`, or a string in `encoding`.
    #[method(name = "_write")]
    pub fn write_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let bytes = super::chunk_bytes(scope, chunk, encoding)?;
        super::write_bytes(scope, this, bytes, callback)
    }

    /// Write the buffered writes `chunks`, an array of `{ chunk, encoding }` entries, together.
    #[method(name = "_writev")]
    pub fn writev_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunks: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let mut bytes = Vec::new();
        if let Ok(chunks) = Object::from_value(scope, *chunks) {
            let length = chunks.num(scope, c"length")? as u32;
            for index in 0..length {
                let entry = chunks.get_element(scope, index)?;
                let Some(entry) = crate::stream::as_object(scope, entry) else {
                    continue;
                };
                let chunk = entry.val(scope, c"chunk")?;
                let encoding = entry.val(scope, c"encoding")?;
                bytes.extend(super::chunk_bytes(scope, chunk, encoding)?);
            }
        }
        super::write_bytes(scope, this, bytes, callback)
    }

    #[method(name = "_final")]
    pub fn final_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let null = scope.root_value(js::value::null());
        if super::when_connected(scope, this, null, callback)? {
            return Ok(());
        }
        if super::handle_of(scope, this)?.is_none() {
            Function::call(
                scope,
                js::value::undefined(),
                callback,
                js::function::EmptyArgs,
            )?;
            return Ok(());
        }
        super::write_or_shutdown(scope, this, None, callback)
    }

    /// Cancel the connect, read and write in progress, drop the connection, call `callback`
    /// with `error`, and then emit `close` with whether `error` is given in a later event loop
    /// turn. A socket that never connected emits `close` without an argument in the tick queue.
    /// The server that accepted the socket counts one connection less.
    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        super::clear_timer(scope, this)?;
        let connected =
            super::handle_of(scope, this)?.is_some() || this.flag(scope, c"connecting")?;
        if let Some(pending) = this.obj(scope, c"_starlingConnect")? {
            if let Ok(pending) = pending.cast::<PendingConnect>() {
                if let Some(cancel) = pending.data_mut().cancel.take() {
                    let _ = cancel.send(());
                }
            }
            this.set_property(scope, c"_starlingConnect", js::value::undefined())?;
        }
        super::release(scope, this)?;
        this.set_property(scope, c"connecting", false)?;
        Function::call(scope, js::value::undefined(), callback, &[error])?;
        let had_error = connected.then(|| truthy(scope, error));
        let this_value = this.to_jsval_throwing(scope)?;
        let emit_close = closure(
            scope,
            0,
            super::emit_close,
            &[
                this_value.get(),
                had_error.map_or(js::value::undefined(), js::value::from_bool),
            ],
        )?;
        let emit_close = emit_close.to_jsval_throwing(scope)?;
        if connected {
            // The connection closes in a later event loop turn, so `close` follows the `error`
            // event's handlers and the reactions they start.
            let set = scope.global().val(scope, c"setTimeout")?;
            Function::call(scope, js::value::undefined(), set, &[emit_close])?;
        } else {
            js::jobs::queue_tick(scope, emit_close, &[])?;
        }
        if let Some(server) = this.obj(scope, c"_server")? {
            let connections = server.num(scope, c"_connections")?;
            server.set_property(scope, c"_connections", connections - 1.0)?;
            if crate::stream::has_method(scope, server, c"_emitCloseIfDrained")? {
                call_method(scope, server, c"_emitCloseIfDrained", &[])?;
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#socketsettimeouttimeout-callback>: emit `timeout` once
    /// the socket is idle for `timeout` milliseconds, or never for 0.
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        timeout: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        if this.flag(scope, c"destroyed")? {
            return Ok(this);
        }
        if !timeout.is_number() {
            return Err(
                NodeError::invalid_arg_type(scope, "msecs", "of type number", timeout).into(),
            );
        }
        let msecs = timeout.to_number();
        if !(msecs >= 0.0 && msecs.is_finite()) {
            let received = String::from_jsval_throwing(scope, timeout, ())?;
            return Err(NodeError::out_of_range(
                "msecs",
                "a non-negative finite number",
                &received,
            )
            .into());
        }
        let callback = callback.filter(|callback| !callback.is_undefined());
        if let Some(callback) = callback {
            validate_function(scope, callback, "callback")?;
        }
        super::clear_timer(scope, this)?;
        this.set_property(scope, c"timeout", msecs)?;
        if msecs > 0.0 {
            super::arm_timer(scope, this, msecs)?;
        }
        if let Some(callback) = callback {
            let method = if msecs > 0.0 {
                c"once"
            } else {
                c"removeListener"
            };
            call_method(scope, this, method, &[string(scope, "timeout")?, callback])?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketaddress>: the local address, port and family, or
    /// an empty object without a connection.
    pub fn address<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<Object<'s>, ExnThrown> {
        match super::address_object(scope, this, false)? {
            Some(address) => Ok(address),
            None => Object::new_plain(scope),
        }
    }

    /// <https://nodejs.org/api/net.html#socketsetnodelaynodelay>: has no effect.
    pub fn set_no_delay<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketsetkeepaliveenable-initialdelay>: has no effect.
    pub fn set_keep_alive<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketref>: has no effect.
    #[method(name = "ref")]
    pub fn ref_impl<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketunref>: has no effect.
    pub fn unref<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// Destroy the socket once it wrote what it buffers, ending it first, as Node.js's
    /// `destroySoon` does.
    pub fn destroy_soon(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if this.flag(scope, c"writable")? {
            call_method(scope, this, c"end", &[])?;
        }
        if this.flag(scope, c"writableFinished")? {
            call_method(scope, this, c"destroy", &[])?;
        } else {
            let destroy = this.val(scope, c"destroy")?;
            call_method(scope, this, c"once", &[string(scope, "finish")?, destroy])?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#socketresetanddestroy>
    pub fn reset_and_destroy<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
    ) -> Result<Object<'s>, ExnThrown> {
        call_method(scope, this, c"destroy", &[])?;
        Ok(this)
    }

    /// Define `readyState`, `pending`, `bytesWritten`, and the address getters.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"readyState", 0, super::ready_state, ())?;
        proto.define_accessor(scope, c"readyState", Some(*getter), None, 0)?;
        let getter = Function::new_callback(scope, c"bytesWritten", 0, super::bytes_written, ())?;
        proto.define_accessor(scope, c"bytesWritten", Some(*getter), None, 0)?;
        let getter = Function::new_callback(scope, c"pending", 0, super::pending_getter, ())?;
        proto.define_accessor(scope, c"pending", Some(*getter), None, 0)?;
        for (index, (name, _, _)) in super::ADDRESS_GETTERS.iter().enumerate() {
            let getter =
                Function::new_callback(scope, name, 0, super::address_getter, index as f64)?;
            proto.define_accessor(scope, name, Some(*getter), None, 0)?;
        }
        Ok(())
    }
}
