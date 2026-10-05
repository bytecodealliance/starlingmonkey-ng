// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `net.Server`, which accepts TCP connections through `platform::net`.
//!
//! A server's state is a [`ServerState`]: its listening socket while it listens, the
//! cancellation of the accept in progress, and the number of open connections. Each accepted
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
use js::{Function, Object};
use platform::net::StreamListener;

use super::socket::{address_object, attach, read_zero, release, socket_class};
use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};
use crate::fs::UvError;
use crate::support::{emit_later, next_tick_call, spawn_cancellable, until_cancelled, Canceller};

/// The state of a `net.Server`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ServerState {
    /// The listening socket, which the accept in progress polls too.
    #[no_trace]
    listener: Option<Rc<RefCell<StreamListener>>>,
    #[no_trace]
    cancel_accept: Option<Canceller>,
    #[no_trace]
    local_addr: Option<SocketAddr>,
    /// The number of open connections, which `_connections` mirrors.
    #[no_trace]
    connections: f64,
    /// Whether the server listens, which `listening` returns.
    #[no_trace]
    pub(crate) listening: bool,
}

#[core_runtime::jsmethods]
impl ServerState {}

/// Emit `drop` on `server` with the addresses of the connection of `socket`, and drop the
/// connection.
fn drop_connection(
    scope: &Scope<'_>,
    server: Object<'_>,
    socket: Object<'_>,
    state: super::socket::SocketState<'_>,
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
        data.set_property(scope, name, socket.get_property(scope, name)?)?;
    }
    release(scope, socket, state)?;
    crate::events::emit(scope, server, c"drop", &[data.to_jsval_throwing(scope)?])?;
    Ok(())
}

/// The completion of an accept: emit `connection` with the connected socket, or `drop` once
/// `maxConnections` connections are open, and accept the next connection. A connection whose peer
/// `blockList` blocks is closed without an event. After an error, `error` is emitted, and the next
/// connection is accepted if the error belongs to the connection that failed, such as one the
/// peer reset before it was accepted.
fn accept_done(
    scope: &Scope<'_>,
    server: Object<'_>,
    result: Result<HandleValue<'_>, HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let state = server_class::state_of(scope, server)?;
    state.data_mut().cancel_accept = None;
    let socket = match result {
        Ok(socket) => Object::from_value(scope, socket).map_err(|e| e.throw(scope))?,
        Err(error) => {
            let code = match Object::from_value(scope, error) {
                Ok(error) => {
                    crate::support::rust_string(scope, error.get_property(scope, c"code")?)
                }
                Err(_) => None,
            };
            crate::events::emit(scope, server, c"error", &[error])?;
            if matches!(
                code.as_deref(),
                Some("ECONNABORTED" | "ECONNRESET" | "ENOTCONN")
            ) {
                accept(scope, server)?;
            }
            return Ok(());
        }
    };
    let socket_state = socket_class::state_of(scope, socket)?;
    let connections = state.data().connections;
    let max = server.get_property(scope, c"maxConnections")?;
    if max.is_number() && connections >= max.to_number() {
        drop_connection(scope, server, socket, socket_state)?;
        return accept(scope, server);
    }
    let peer = socket_state.data().peer_ip();
    let block_list = server.get_property(scope, c"blockList")?;
    if let Some(ip) = peer {
        if super::blocklist::blocks(scope, block_list, ip) {
            release(scope, socket, socket_state)?;
            return accept(scope, server);
        }
    }
    if js::conversion::to_boolean(server.get_property(scope, c"pauseOnConnect")?) {
        Function::call_by_name(scope, socket.handle(), c"pause", js::function::EmptyArgs)?;
    } else {
        read_zero(scope, socket)?;
    }
    state.data_mut().connections = connections + 1.0;
    server.set_property(scope, c"_connections", connections + 1.0)?;
    socket_state.data_mut().server = Some(js::gc::handle::Heap::from(server));
    socket.set_property(scope, c"server", server)?;
    socket.set_property(scope, c"_server", server)?;
    crate::events::emit(
        scope,
        server,
        c"connection",
        &[socket.to_jsval_throwing(scope)?],
    )?;
    accept(scope, server)
}

/// Accept the next connection of `server`, unless it stopped listening.
fn accept(scope: &Scope<'_>, server: Object<'_>) -> Result<(), ExnThrown> {
    let state = server_class::state_of(scope, server)?;
    let Some(listener) = state.data().listener.clone() else {
        return Ok(());
    };
    let canceller = spawn_cancellable(
        scope,
        server,
        |cancelled| {
            let accepted = std::future::poll_fn(move |cx| listener.borrow_mut().poll_accept(cx));
            until_cancelled(cancelled, accepted)
        },
        |scope, server, result| {
            let connection = result.map_err(|error| UvError::net(&error, "accept").throw(scope))?;
            let options = Object::new_plain(scope)?;
            let allow_half_open =
                js::conversion::to_boolean(server.get_property(scope, c"allowHalfOpen")?);
            options.set_property(scope, c"allowHalfOpen", allow_half_open)?;
            let socket = socket_class::new_instance(scope)?;
            socket_class::init(scope, socket, Some(options.to_jsval_throwing(scope)?))
                .map_err(|thrown| thrown.throw(scope))?;
            let state = socket_class::state_of(scope, socket)?;
            attach(scope, socket, state, connection)?;
            socket.to_jsval_throwing(scope)
        },
        accept_done,
    )?;
    state.data_mut().cancel_accept = Some(canceller);
    Ok(())
}

/// Count one connection of `server` less, after the socket it accepted was destroyed, and emit
/// `close` if it was the last and the server stopped listening.
pub(crate) fn connection_closed(scope: &Scope<'_>, server: Object<'_>) -> Result<(), ExnThrown> {
    let Some(state) = server_class::try_state_of(scope, server)? else {
        return Ok(());
    };
    let connections = {
        let mut data = state.data_mut();
        data.connections -= 1.0;
        data.connections
    };
    server.set_property(scope, c"_connections", connections)?;
    emit_close_if_drained(scope, server)
}

/// Emit `close` on `server` in the tick queue if it doesn't listen and has no open connection.
fn emit_close_if_drained(scope: &Scope<'_>, server: Object<'_>) -> Result<(), ExnThrown> {
    let drained = {
        let state = server_class::state_of(scope, server)?;
        let data = state.data();
        data.listener.is_none() && data.connections <= 0.0
    };
    if drained {
        emit_later(scope, server, c"close", &[])?;
    }
    Ok(())
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
        .filter(|last| js::function::is_callable(*last));
    let first = args.first().copied();
    let options = match first {
        Some(first) if first.is_object() && !js::function::is_callable(first) => {
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
    let port = options.get_property(scope, c"port")?;
    let free_port = first.is_none_or(|first| js::function::is_callable(first))
        || (port.is_undefined() && options.has_property(scope, c"port")?)
        || port.is_null();
    let host = options.get_property(scope, c"host")?;
    let host = if host.is_string() && js::conversion::to_boolean(host) {
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
    let path = options.get_property(scope, c"path")?;
    if js::conversion::to_boolean(path) && super::is_pipe_name(scope, path)? {
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

/// The `close` listener `close` adds for a server that was not listening, whose payload is the
/// callback: call the callback with `ERR_SERVER_NOT_RUNNING`.
fn call_not_running(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let error = NodeError::new(
        ErrorKind::Error,
        "ERR_SERVER_NOT_RUNNING",
        "Server is not running.",
    )
    .to_value(scope)?;
    Function::call(scope, HandleValue::undefined(), payload, &[error])?;
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
    let Some(callback) = callback.filter(|callback| js::function::is_callable(*callback)) else {
        return Ok(());
    };
    let listener = if listening {
        callback
    } else {
        let close = Function::new_callback(scope, c"close", 0, call_not_running, callback)?;
        (*close).to_jsval_throwing(scope)?
    };
    Function::call_by_name(
        scope,
        server.handle(),
        c"once",
        &[js::class::get_or_init_atom(scope, c"close")?, listener],
    )?;
    Ok(())
}

/// The getter of `listening`.
fn get_listening(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let listening = match Object::from_value(scope, args.this()) {
        Ok(server) => {
            server_class::try_state_of(scope, server)?.is_some_and(|state| state.data().listening)
        }
        Err(_) => false,
    };
    Ok(js::value::from_bool(listening))
}

#[core_runtime::jsfunction_class(
    name = "Server",
    extends = "crate::events::event_emitter",
    state = "ServerState",
    invalid_this = "crate::errors::invalid_this",
    self_construct
)]
pub mod server_class {
    use super::*;

    /// <https://nodejs.org/api/net.html#class-netserver>: a server whose `connection` events go
    /// to `listener`. Of the options, `allowHalfOpen` and `pauseOnConnect` apply to the accepted
    /// sockets, `blockList` closes connections from the peers it blocks, and `noDelay`,
    /// `keepAlive`, `keepAliveInitialDelay` and `highWaterMark` have no effect.
    #[constructor(length = 2)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
        listener: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)?;
        let (options, listener) = match options {
            Some(options) if js::function::is_callable(options) => (None, Some(options)),
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
        if let Some(listener) = listener.filter(|listener| js::function::is_callable(*listener)) {
            Function::call_by_name(
                scope,
                this.handle(),
                c"on",
                &[js::class::get_or_init_atom(scope, c"connection")?, listener],
            )?;
        }
        this.set_property(scope, c"_connections", 0)?;
        this.set_property(scope, c"_handle", js::value::null())?;
        this.set_property(scope, c"_usingWorkers", false)?;
        this.set_property(scope, c"_workers", *js::Array::new(scope, 0)?)?;
        this.set_property(scope, c"_unref", false)?;
        this.set_property(scope, c"_listeningId", 1)?;
        let option = |name| -> Result<HandleValue<'_>, ExnThrown> {
            match options {
                Some(options) => options.get_property(scope, name),
                None => Ok(HandleValue::undefined()),
            }
        };
        for name in [
            c"allowHalfOpen",
            c"pauseOnConnect",
            c"noDelay",
            c"keepAlive",
        ] {
            let value = js::conversion::to_boolean(option(name)?);
            this.set_property(scope, name, value)?;
        }
        let delay = option(c"keepAliveInitialDelay")?;
        let delay = if delay.is_number() {
            (delay.to_number() / 1000.0).trunc()
        } else {
            0.0
        };
        this.set_property(scope, c"keepAliveInitialDelay", delay)?;
        let high_water_mark = option(c"highWaterMark")?;
        if high_water_mark.is_null_or_undefined() {
            let mark = crate::stream::state::default_high_water_mark(false);
            this.set_property(scope, c"highWaterMark", mark)?;
        } else {
            this.set_property(scope, c"highWaterMark", high_water_mark)?;
        }
        let block_list = option(c"blockList")?;
        if js::conversion::to_boolean(block_list) {
            super::super::blocklist::validate_block_list(scope, block_list, "options.blockList")?;
            this.set_property(scope, c"blockList", block_list)?;
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
        let state = state_of(scope, this)?;
        if state.data().listening {
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
            Function::call_by_name(
                scope,
                this.handle(),
                c"once",
                &[js::class::get_or_init_atom(scope, c"listening")?, callback],
            )?;
        }
        let addr = SocketAddr::new(ip, port);
        match platform::net::listen_stream(addr) {
            Ok(listener) => {
                {
                    let mut data = state.data_mut();
                    data.local_addr = Some(listener.local_addr);
                    data.listener = Some(Rc::new(RefCell::new(listener)));
                    data.listening = true;
                }
                this.set_property(scope, c"_handle", Object::new_plain(scope)?)?;
                emit_later(scope, this, c"listening", &[])?;
                accept(scope, this)?;
            }
            Err(error) => {
                let description = platform::fs::FsError::from_code(error.code()).description;
                let error = UvError::net(&error, "listen")
                    .suffix(format!(": {description} {addr}"))
                    .addr(addr)
                    .into_value(scope)?;
                emit_later(scope, this, c"error", &[error])?;
            }
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#serverclosecallback>: stop accepting connections, and
    /// emit `close` once the connections already accepted have closed. `callback` is called on
    /// `close`, with `ERR_SERVER_NOT_RUNNING` if the server was not listening.
    #[method(length = 1)]
    pub fn close<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let state = state_of(scope, this)?;
        let (listener, cancel) = {
            let mut data = state.data_mut();
            data.listening = false;
            (data.listener.take(), data.cancel_accept.take())
        };
        on_close(scope, this, callback, listener.is_some())?;
        drop(cancel);
        drop(listener);
        this.set_property(scope, c"_handle", js::value::null())?;
        emit_close_if_drained(scope, this)?;
        Ok(this)
    }

    /// Emit `close` in the tick queue if the server is not listening and has no open
    /// connection.
    #[method(name = "_emitCloseIfDrained")]
    pub fn emit_close_if_drained_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
    ) -> Result<(), ExnThrown> {
        emit_close_if_drained(scope, this)
    }

    /// <https://nodejs.org/api/net.html#serveraddress>: the bound address, family and port, or
    /// `null` when not listening.
    pub fn address<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let addr = {
            let state = state_of(scope, this)?;
            let data = state.data();
            data.local_addr.filter(|_| data.listener.is_some())
        };
        match addr {
            Some(addr) => address_object(scope, addr)?.to_jsval_throwing(scope),
            None => Ok(HandleValue::null()),
        }
    }

    /// <https://nodejs.org/api/net.html#servergetconnectionscallback>: the number of open
    /// connections.
    pub fn get_connections<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        callback: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        validate_function(scope, callback, "callback")?;
        let count = state_of(scope, this)?.data().connections;
        let count = scope.root_value(js::value::from_f64(count));
        next_tick_call(scope, callback, &[HandleValue::null(), count])?;
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

    /// Define `listening`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"listening", 0, get_listening, ())?;
        proto.define_accessor(
            scope,
            c"listening",
            Some(*getter),
            None,
            js::class_spec::JSPROP_ENUMERATE as u32,
        )
    }
}
