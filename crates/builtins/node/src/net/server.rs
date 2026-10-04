// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `net.Server`, which accepts TCP connections through `platform::net`.
//!
//! The listening socket is in a `ListenerHandle` under `_starlingListener`. Each accepted
//! connection becomes a connected `Socket`, emitted as a `connection` event.

use std::cell::RefCell;
use std::net::{IpAddr, SocketAddr};
use std::rc::Rc;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};
use platform::net::StreamListener;

use super::socket::{attach, read_zero, release, socket_class, throw_net_error};
use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};
use crate::stream::{
    call_method, closure, is_function, next_tick_call, string, truthy, Captures, Props,
};

/// The listening socket of a server, shared with the accept in progress, which polls it.
#[core_runtime::jsclass(hidden)]
pub struct ListenerHandle {
    /// The listening socket, which `close` takes and drops, so it stops accepting at once.
    #[no_trace]
    listener: Rc<RefCell<Option<StreamListener>>>,
    /// Cancels the accept in progress.
    #[no_trace]
    cancel: Option<futures_channel::oneshot::Sender<()>>,
    #[no_trace]
    local_addr: Option<SocketAddr>,
}

#[core_runtime::jsmethods]
impl ListenerHandle {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// The listener handle of `server`, if it is listening.
fn handle_of<'s>(
    scope: &'s Scope<'_>,
    server: Object<'_>,
) -> Result<Option<ListenerHandle<'s>>, ExnThrown> {
    Ok(server
        .obj(scope, c"_starlingListener")?
        .and_then(|handle| handle.cast::<ListenerHandle>().ok()))
}

/// Emit the event and arguments `args` on `emitter` in the tick queue.
fn emit_later(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    let emit = emitter.obj(scope, c"emit")?.ok_or(ExnThrown)?;
    let bound = call_method(scope, emit, c"bind", &[emitter.to_jsval_throwing(scope)?])?;
    next_tick_call(scope, bound, args)
}

/// Emit `drop` on `server` with the addresses of the connection of `socket`, and drop the
/// connection.
fn drop_connection(
    scope: &Scope<'_>,
    server: Object<'_>,
    socket: Object<'_>,
) -> Result<(), ExnThrown> {
    let data = Object::new_plain(scope)?;
    for name in [
        c"localAddress",
        c"localPort",
        c"localFamily",
        c"remoteAddress",
        c"remotePort",
        c"remoteFamily",
    ] {
        data.set_property(scope, name, socket.val(scope, name)?)?;
    }
    release(scope, socket)?;
    call_method(
        scope,
        server,
        c"emit",
        &[string(scope, "drop")?, data.to_jsval_throwing(scope)?],
    )?;
    Ok(())
}

/// Whether the `check` method of the `blockList` of `server` returns a truthy value for the peer
/// address of `socket`.
fn blocked(scope: &Scope<'_>, server: Object<'_>, socket: Object<'_>) -> Result<bool, ExnThrown> {
    let Some(block_list) = server.obj(scope, c"blockList")? else {
        return Ok(false);
    };
    let address = socket.val(scope, c"remoteAddress")?;
    let text = match address.is_string() {
        true => String::from_jsval_throwing(scope, address, ())?,
        false => return Ok(false),
    };
    let family = if super::is_ipv4(&text) {
        "ipv4"
    } else if super::is_ipv6(&text) {
        "ipv6"
    } else {
        return Ok(false);
    };
    let blocked = call_method(
        scope,
        block_list,
        c"check",
        &[address, string(scope, family)?],
    )?;
    Ok(truthy(scope, blocked))
}

/// The fulfillment reaction of an accept, whose captures are `[server]`: emit `connection` with
/// the connected socket, or `drop` once `maxConnections` connections are open, and accept the
/// next connection. A connection whose peer `blockList` blocks is closed without an event. A
/// cancelled accept is fulfilled with `undefined`.
fn on_accept(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = Captures::of(scope, payload)?.obj(0)?;
    let Ok(socket) = Object::from_value(scope, args.get(0)) else {
        return Ok(js::value::undefined());
    };
    let connections = server.num(scope, c"_connections")?;
    let max = server.val(scope, c"maxConnections")?;
    if max.is_number() && connections >= max.to_number() {
        drop_connection(scope, server, socket)?;
        return accept(scope, server);
    }
    if blocked(scope, server, socket)? {
        release(scope, socket)?;
        return accept(scope, server);
    }
    if server.flag(scope, c"pauseOnConnect")? {
        call_method(scope, socket, c"pause", &[])?;
    } else {
        read_zero(scope, socket)?;
    }
    server.set_property(scope, c"_connections", connections + 1.0)?;
    socket.set_property(scope, c"server", server)?;
    socket.define_property(scope, c"_server", server, 0)?;
    call_method(
        scope,
        server,
        c"emit",
        &[string(scope, "connection")?, args.get(0)],
    )?;
    accept(scope, server)
}

/// The rejection reaction of an accept, whose captures are `[server]`: emit `error`. The server
/// accepts the next connection if the error belongs to the connection that failed, such as one
/// the peer reset before it was accepted.
fn on_accept_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = Captures::of(scope, payload)?.obj(0)?;
    let code = match Object::from_value(scope, args.get(0)) {
        Ok(error) => {
            let code = error.val(scope, c"code")?;
            code.is_string()
                .then(|| String::from_jsval_throwing(scope, code, ()))
                .transpose()?
        }
        Err(_) => None,
    };
    call_method(
        scope,
        server,
        c"emit",
        &[string(scope, "error")?, args.get(0)],
    )?;
    if matches!(
        code.as_deref(),
        Some("ECONNABORTED" | "ECONNRESET" | "ENOTCONN")
    ) {
        accept(scope, server)?;
    }
    Ok(js::value::undefined())
}

/// Accept the next connection of `server`, unless it stopped listening.
fn accept(scope: &Scope<'_>, server: Object<'_>) -> Result<Value, ExnThrown> {
    let Some(handle) = handle_of(scope, server)? else {
        return Ok(js::value::undefined());
    };
    let (sender, cancelled) = futures_channel::oneshot::channel();
    let listener = {
        let mut state = handle.data_mut();
        if state.listener.borrow().is_none() {
            return Ok(js::value::undefined());
        }
        state.cancel = Some(sender);
        state.listener.clone()
    };
    let allow_half_open = server.flag(scope, c"allowHalfOpen")?;
    let future = async move {
        let accepted = std::future::poll_fn(|cx| match listener.borrow_mut().as_mut() {
            Some(listener) => listener.poll_accept(cx).map(Some),
            None => std::task::Poll::Ready(None),
        });
        let cancel = async {
            let _ = cancelled.await;
            None
        };
        let result = futures_lite::future::or(accepted, cancel).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let Some(result) = result else {
                return Ok(scope.root_value(js::value::undefined()));
            };
            let connection =
                result.map_err(|error| throw_net_error(scope, &error, "accept", None, &[]))?;
            let class = socket_class::class_constructor(scope)?;
            let options = Object::new_plain(scope)?;
            options.set_property(scope, c"allowHalfOpen", allow_half_open)?;
            let socket = Function::construct(scope, class, &[options.to_jsval_throwing(scope)?])?;
            attach(scope, socket, connection)?;
            socket.to_jsval_throwing(scope)
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    let on_accept = closure(scope, 1, on_accept, &[server.to_jsval_raw_throwing(scope)?])?;
    let on_error = closure(
        scope,
        1,
        on_accept_error,
        &[server.to_jsval_raw_throwing(scope)?],
    )?;
    promise.then(scope, Some(on_accept), Some(on_error))?;
    Ok(js::value::undefined())
}

/// The port, host and callback `listen`'s arguments give, as Node.js's `Server.listen`
/// normalizes and validates them: `([port][, host][, backlog][, callback])` or `(options[,
/// callback])`. A port that is omitted, `undefined` or `null` is 0, for a free port.
pub(crate) fn listen_args<'s>(
    scope: &'s Scope<'_>,
    args: &[HandleValue<'s>],
) -> Result<(u16, Option<String>, Option<HandleValue<'s>>), Thrown> {
    let callback = args
        .last()
        .copied()
        .filter(|last| is_function(scope, *last));
    let first = args.first().copied();
    let options = match first {
        Some(first) if first.is_object() && !is_function(scope, first) => {
            Object::from_value(scope, *first).map_err(|e| Thrown::from(e.throw(scope)))?
        }
        Some(first) if super::is_pipe_name(scope, first)? => {
            return Err(NodeError::not_implemented("listen() on an IPC path").into());
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
    let port = options.val(scope, c"port")?;
    let free_port = first.is_none_or(|first| is_function(scope, first))
        || (port.is_undefined() && options.has_property(scope, c"port")?)
        || port.is_null();
    let host = options.val(scope, c"host")?;
    let host = if host.is_string() && truthy(scope, host) {
        Some(String::from_jsval(scope, host, ())?)
    } else {
        None
    };
    if free_port {
        return Ok((0, host, callback));
    }
    if port.is_number() || port.is_string() {
        let port = super::validate_port(scope, port, "options.port")?;
        return Ok((port, host, callback));
    }
    let path = options.val(scope, c"path")?;
    if truthy(scope, path) && super::is_pipe_name(scope, path)? {
        return Err(NodeError::not_implemented("listen() on an IPC path").into());
    }
    let options_value = options.to_jsval_throwing(scope)?;
    if !options.has_property(scope, c"port")? && !options.has_property(scope, c"path")? {
        return Err(NodeError::invalid_arg_value(
            scope,
            "options",
            options_value,
            "must have the property \"port\" or \"path\"",
        )
        .into());
    }
    Err(NodeError::invalid_arg_value(scope, "options", options_value, "is invalid").into())
}

/// The `close` listener `close` adds for a server that was not listening, whose captures are
/// `[callback]`: call the callback with `ERR_SERVER_NOT_RUNNING`.
pub(crate) fn call_not_running(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let callback = Captures::of(scope, payload)?.get(0)?;
    let error = crate::stream::error_value(
        scope,
        NodeError::new(
            ErrorKind::Error,
            "ERR_SERVER_NOT_RUNNING",
            "Server is not running.",
        ),
    )?;
    Function::call(scope, js::value::undefined(), callback, &[error])?;
    Ok(js::value::undefined())
}

/// Add `callback` as a `close` listener of `server`, which gets `ERR_SERVER_NOT_RUNNING` unless
/// `listening`.
pub(crate) fn on_close(
    scope: &Scope<'_>,
    server: Object<'_>,
    callback: Option<HandleValue<'_>>,
    listening: bool,
) -> Result<(), ExnThrown> {
    let Some(callback) = callback.filter(|callback| is_function(scope, *callback)) else {
        return Ok(());
    };
    let listener = if listening {
        scope.root_value(callback.get())
    } else {
        closure(scope, 0, call_not_running, &[callback.get()])?.to_jsval_throwing(scope)?
    };
    call_method(scope, server, c"once", &[string(scope, "close")?, listener])?;
    Ok(())
}

#[core_runtime::jsfunction_class(
    name = "Server",
    extends = "crate::events::event_emitter",
    self_construct
)]
pub mod server_class {
    use super::*;

    /// <https://nodejs.org/api/net.html#class-netserver>: a server whose `connection` events go
    /// to `listener`. Of the options, `allowHalfOpen` and `pauseOnConnect` apply to the accepted
    /// sockets, `blockList` closes connections from the peers it blocks, and `noDelay` and
    /// `keepAlive` have no effect.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
        listener: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)?;
        let (options, listener) = match options {
            Some(options) if is_function(scope, options) => (None, Some(options)),
            Some(options) if options.is_null() || options.is_undefined() => (None, listener),
            Some(options) if options.is_object() => (
                Some(
                    Object::from_value(scope, *options)
                        .map_err(|e| Thrown::from(e.throw(scope)))?,
                ),
                listener,
            ),
            Some(options) => {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "options",
                    "of type object",
                    options,
                )
                .into())
            }
            None => (None, listener),
        };
        if let Some(listener) = listener.filter(|listener| is_function(scope, *listener)) {
            call_method(
                scope,
                this,
                c"on",
                &[string(scope, "connection")?, listener],
            )?;
        }
        this.set_property(scope, c"listening", false)?;
        this.set_property(scope, c"_connections", 0)?;
        for name in [
            c"allowHalfOpen",
            c"pauseOnConnect",
            c"noDelay",
            c"keepAlive",
        ] {
            let value = match options {
                Some(options) => truthy(scope, options.val(scope, name)?),
                None => false,
            };
            this.set_property(scope, name, value)?;
        }
        if let Some(options) = options {
            let block_list = options.val(scope, c"blockList")?;
            if truthy(scope, block_list) {
                super::super::blocklist::validate_block_list(
                    scope,
                    block_list,
                    "options.blockList",
                )?;
                this.set_property(scope, c"blockList", block_list)?;
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#serverlisten>: listen on `port`, or a free port, of
    /// `host`, or of every IPv4 address. IPC paths are not supported.
    pub fn listen<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let args: Vec<HandleValue<'_>> =
            args.iter().map(|arg| scope.root_value(arg.get())).collect();
        if this.flag(scope, c"listening")? {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_SERVER_ALREADY_LISTEN",
                "Listen method has been called more than once without closing.",
            )
            .into());
        }
        let (port, host, callback) = listen_args(scope, &args)?;
        let ip = match host.as_deref() {
            None => IpAddr::from([0, 0, 0, 0]),
            Some("localhost") => IpAddr::from([127, 0, 0, 1]),
            Some(host) => host.parse::<IpAddr>().map_err(|_| {
                NodeError::not_implemented("listen() on a host name other than localhost")
            })?,
        };
        if let Some(callback) = callback {
            call_method(
                scope,
                this,
                c"once",
                &[string(scope, "listening")?, callback],
            )?;
        }
        let addr = SocketAddr::new(ip, port);
        match platform::net::listen_stream(addr) {
            Ok(listener) => {
                let local_addr = listener.local_addr;
                let handle = js::class::create_instance_with::<ListenerHandleImpl>(scope, |_| {
                    ListenerHandleImpl {
                        listener: Rc::new(RefCell::new(Some(listener))),
                        cancel: None,
                        local_addr: Some(local_addr),
                    }
                })?;
                this.define_property(
                    scope,
                    c"_starlingListener",
                    handle.to_jsval_throwing(scope)?,
                    0,
                )?;
                this.set_property(scope, c"listening", true)?;
                emit_later(scope, this, &[string(scope, "listening")?])?;
                accept(scope, this)?;
            }
            Err(error) => {
                let address =
                    js::JSString::from_str(scope, &ip.to_string())?.to_jsval_throwing(scope)?;
                let port_value = scope.root_value(js::value::from_f64(f64::from(port)));
                let detail = format!(
                    "{}: {addr}",
                    platform::fs::FsError::from_code(error.code()).description
                );
                throw_net_error(
                    scope,
                    &error,
                    "listen",
                    Some(&detail),
                    &[(c"address", address), (c"port", port_value)],
                );
                let error = crate::stream::take_exception(scope);
                emit_later(scope, this, &[string(scope, "error")?, error])?;
            }
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#serverclosecallback>: stop accepting connections, and
    /// emit `close` once the connections already accepted have closed. `callback` is called on
    /// `close`, with `ERR_SERVER_NOT_RUNNING` if the server was not listening.
    pub fn close<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let handle = handle_of(scope, this)?;
        super::on_close(scope, this, callback, handle.is_some())?;
        if let Some(handle) = handle {
            let mut state = handle.data_mut();
            state.listener.borrow_mut().take();
            if let Some(cancel) = state.cancel.take() {
                let _ = cancel.send(());
            }
        }
        this.set_property(scope, c"_starlingListener", js::value::undefined())?;
        this.set_property(scope, c"listening", false)?;
        server_class::emit_close_if_drained(scope, this)?;
        Ok(this)
    }

    /// Emit `close` in the tick queue if the server is not listening and has no open
    /// connection.
    #[method(name = "_emitCloseIfDrained")]
    pub fn emit_close_if_drained(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if handle_of(scope, this)?.is_some() || this.num(scope, c"_connections")? > 0.0 {
            return Ok(());
        }
        emit_later(scope, this, &[string(scope, "close")?])
    }

    /// <https://nodejs.org/api/net.html#serveraddress>: the bound address, family and port, or
    /// `null` when not listening.
    pub fn address<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let Some(addr) = handle_of(scope, this)?.and_then(|handle| handle.data().local_addr) else {
            return Ok(scope.root_value(js::value::null()));
        };
        let address = Object::new_plain(scope)?;
        address.set_property(scope, c"address", addr.ip().to_string().as_str())?;
        address.set_property(
            scope,
            c"family",
            if addr.is_ipv4() { "IPv4" } else { "IPv6" },
        )?;
        address.set_property(scope, c"port", f64::from(addr.port()))?;
        address.to_jsval_throwing(scope)
    }

    /// <https://nodejs.org/api/net.html#servergetconnectionscallback>: the number of open
    /// connections.
    pub fn get_connections<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        callback: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        validate_function(scope, callback, "callback")?;
        let null = scope.root_value(js::value::null());
        let count = this.val(scope, c"_connections")?;
        next_tick_call(scope, callback, &[null, count])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#serverref>: has no effect.
    #[method(name = "ref")]
    pub fn ref_impl<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#serverunref>: has no effect, so a listening server
    /// keeps the event loop alive.
    pub fn unref<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }
}
