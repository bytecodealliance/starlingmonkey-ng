// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `http.Server`, which serves the runtime's incoming requests.
//!
//! `listen()` adds a `fetch` event listener, and records the server with its port. Each `fetch`
//! event goes to the listening server whose port is the request URL's, or else to the server that
//! listened first, as a `request` event with an `IncomingMessage` for the web `Request` and a
//! `ServerResponse` whose web `Response` is the event's response. When the runtime runs a script as
//! a command, nothing serves requests, so `listen()` emits an `ERR_SERVE_MODE_REQUIRED` error. In a
//! component without a `wasi:http/handler` export, `listen()` throws that error.

use fetch_event::fetch_event::FetchEvent;
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::incoming::{incoming_message_class, set_headers};
use super::outgoing::server_response_class;
use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::stream::{call_method, is_function, next_tick_call, string, Props};

/// The key of the shared array of listening servers.
static SERVERS_KEY: u8 = 0;

/// The listening servers of the current global, in the order they started listening.
fn servers<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::get_or_init_shared_object(scope, &SERVERS_KEY as *const u8 as usize, |scope| {
        Ok(*js::Array::new(scope, 0)?)
    })
}

/// The listening server that serves a request to `port`.
fn server_for<'s>(
    scope: &'s Scope<'_>,
    port: Option<u16>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let servers = servers(scope)?;
    let length = servers
        .cast::<js::Array>()
        .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))?
        .length(scope)?;
    let mut first = None;
    for index in 0..length {
        let server = servers.get_element(scope, index)?;
        let Ok(server) = Object::from_value(scope, server) else {
            continue;
        };
        if first.is_none() {
            first = Some(server);
        }
        let listened = server.num(scope, c"_starlingPort")?;
        if port.is_some_and(|port| f64::from(port) == listened) {
            return Ok(Some(server));
        }
    }
    Ok(first)
}

/// The socket a served request reports: an `EventEmitter` with the request's local port, whether
/// it was encrypted, and the address, port and family of the client's address `client`. `client`
/// is `None` where the transport doesn't report it, as with `wasi:http`.
fn socket<'s>(
    scope: &'s Scope<'_>,
    port: Option<u16>,
    encrypted: bool,
    client: Option<std::net::SocketAddr>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let class = crate::events::emitter_constructor(scope)?;
    let socket = Function::construct(scope, class, js::function::EmptyArgs)?;
    match client {
        Some(client) => {
            socket.set_property(scope, c"remoteAddress", client.ip().to_string().as_str())?;
            socket.set_property(scope, c"remotePort", f64::from(client.port()))?;
            let family = if client.is_ipv4() { "IPv4" } else { "IPv6" };
            socket.set_property(scope, c"remoteFamily", family)?;
        }
        None => socket.set_property(scope, c"remoteAddress", js::value::undefined())?,
    }
    socket.set_property(
        scope,
        c"localPort",
        port.map_or(js::value::undefined(), |port| {
            js::value::from_f64(f64::from(port))
        }),
    )?;
    socket.set_property(scope, c"encrypted", encrypted)?;
    socket.set_property(scope, c"writable", true)?;
    socket.set_property(scope, c"readable", true)?;
    socket.to_jsval_throwing(scope)
}

/// The authority of `url`: its host, followed by its port if it has one.
fn authority(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// The event `server` would emit with the request's socket for `request`, if it has listeners
/// for it: `connect` for a `CONNECT` request, and `upgrade` for a request with an `Upgrade`
/// header.
fn socket_event(
    scope: &Scope<'_>,
    server: Object<'_>,
    request: Object<'_>,
) -> Result<Option<&'static str>, ExnThrown> {
    let method = String::from_jsval(scope, request.val(scope, c"method")?, ()).unwrap_or_default();
    let headers = request.obj(scope, c"headers")?.ok_or(ExnThrown)?;
    let upgrade = call_method(scope, headers, c"has", &[string(scope, "upgrade")?])?;
    let name = if method.eq_ignore_ascii_case("CONNECT") {
        "connect"
    } else if upgrade.is_boolean() && upgrade.to_boolean() {
        "upgrade"
    } else {
        return Ok(None);
    };
    let count = call_method(scope, server, c"listenerCount", &[string(scope, name)?])?;
    Ok((count.is_number() && count.to_number() > 0.0).then_some(name))
}

/// Respond to `event` with a 501, since served requests have no socket to emit `event_name`
/// with.
fn respond_not_implemented(
    scope: &Scope<'_>,
    event: Object<'_>,
    event_name: &str,
) -> Result<Value, ExnThrown> {
    let class = scope.global().obj(scope, c"Response")?.ok_or(ExnThrown)?;
    let init = Object::new_plain(scope)?;
    init.set_property(scope, c"status", 501)?;
    let body = format!("Served requests have no socket for a '{event_name}' listener");
    let response = Function::construct(
        scope,
        class,
        &[string(scope, &body)?, init.to_jsval_throwing(scope)?],
    )?;
    call_method(
        scope,
        event,
        c"respondWith",
        &[response.to_jsval_throwing(scope)?],
    )?;
    Ok(js::value::undefined())
}

/// The `fetch` event listener: respond to the event through the listening server for its request.
fn on_fetch(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let event = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    let request = event.obj(scope, c"request")?.ok_or(ExnThrown)?;
    let href = String::from_jsval(scope, request.val(scope, c"url")?, ()).unwrap_or_default();
    let url = url::Url::parse(&href).ok();
    let port = url.as_ref().and_then(|url| url.port_or_known_default());
    let Some(server) = server_for(scope, port)? else {
        return Ok(js::value::undefined());
    };

    if let Some(event_name) = socket_event(scope, server, request)? {
        return respond_not_implemented(scope, event, event_name);
    }

    let encrypted = url.as_ref().is_some_and(|url| url.scheme() == "https");
    let client = event
        .cast::<FetchEvent>()
        .ok()
        .and_then(|event| event.client_address());
    let req = Function::construct(
        scope,
        incoming_message_class::class_constructor(scope)?,
        &[socket(scope, port, encrypted, client)?],
    )?;
    req.set_property(scope, c"method", request.val(scope, c"method")?)?;
    let path = url.as_ref().map_or_else(
        || "/".to_string(),
        |url| match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        },
    );
    req.set_property(scope, c"url", path.as_str())?;
    let headers = request.obj(scope, c"headers")?.ok_or(ExnThrown)?;
    let mut list = headers
        .cast::<web_fetch::headers::Headers>()
        .map(|headers| headers.list())
        .unwrap_or_default();
    // A `wasi:http` host passes the host as the request's authority rather than as a header.
    if !list
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        if let Some(authority) = url.as_ref().and_then(authority) {
            list.insert(0, ("host".to_string(), authority));
        }
    }
    set_headers(scope, req, &list)?;
    if let Some(body) = request.obj(scope, c"body")? {
        let reader = call_method(scope, body, c"getReader", &[])?;
        req.define_property(scope, c"_starlingReader", reader, 0)?;
    }

    let req_value = req.to_jsval_throwing(scope)?;
    let res = Function::construct(
        scope,
        server_response_class::class_constructor(scope)?,
        &[req_value],
    )?;
    let promise = js::Promise::new_pending(scope)?;
    res.define_property(scope, c"_starlingRespond", *promise, 0)?;
    call_method(
        scope,
        event,
        c"respondWith",
        &[promise.to_jsval_throwing(scope)?],
    )?;
    let emitted = call_method(
        scope,
        server,
        c"emit",
        &[
            string(scope, "request")?,
            req_value,
            res.to_jsval_throwing(scope)?,
        ],
    );
    if emitted.is_err() {
        // We reject the response so the request doesn't wait for a response forever, and leave
        // the listener's exception pending.
        let error = crate::stream::take_exception(scope);
        if let Some(respond) = res.obj(scope, c"_starlingRespond")? {
            if let Ok(respond) = respond.cast::<js::Promise>() {
                res.set_property(scope, c"_starlingRespond", true)?;
                respond.reject(scope, error)?;
            }
        }
        js::exception::set_pending(
            scope,
            error,
            js::native::ExceptionStackBehavior::DoNotCapture,
        );
        return Err(ExnThrown);
    }
    Ok(js::value::undefined())
}

/// Emit the event and arguments `args` on `emitter` in a microtask.
pub(super) fn emit_later(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    let emit = emitter.obj(scope, c"emit")?.ok_or(ExnThrown)?;
    let bound = call_method(scope, emit, c"bind", &[emitter.to_jsval_throwing(scope)?])?;
    next_tick_call(scope, bound, args)
}

#[core_runtime::jsfunction_class(
    name = "Server",
    extends = "crate::events::event_emitter",
    self_construct
)]
pub mod server_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpserver>: a server whose `request` events go to
    /// `listener`. The options are ignored.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
        listener: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        crate::events::event_emitter::init(scope, this, None)
            .map_err(|thrown| thrown.throw(scope))?;
        let listener = match options {
            Some(options) if is_function(scope, options) => Some(options),
            _ => listener,
        };
        if let Some(listener) = listener.filter(|listener| is_function(scope, *listener)) {
            call_method(scope, this, c"on", &[string(scope, "request")?, listener])?;
        }
        this.set_property(scope, c"listening", false)?;
        for (name, value) in [
            (c"timeout", 0.0),
            (c"keepAliveTimeout", 5000.0),
            (c"headersTimeout", 60000.0),
            (c"requestTimeout", 300000.0),
            (c"maxHeadersCount", f64::NAN),
        ] {
            this.set_property(scope, name, value)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#serverlisten>: serve requests to `port`, or to any port.
    pub fn listen<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let args: Vec<HandleValue<'_>> =
            args.iter().map(|arg| scope.root_value(arg.get())).collect();
        let (port, _, callback) = crate::net::listen_args(scope, &args)?;
        if core_runtime::event_loop::without_request_handler() {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_SERVE_MODE_REQUIRED",
                "http.Server.listen() can't serve requests in a component without a wasi:http/handler export: componentize it with --serve",
            )
            .into());
        }
        if core_runtime::event_loop::running_as_command() {
            let error = crate::stream::error_value(
                scope,
                NodeError::new(
                    ErrorKind::Error,
                    "ERR_SERVE_MODE_REQUIRED",
                    "Serving HTTP requests needs serve mode: run the script with --serve <port>, or serve the component with a wasi:http host",
                ),
            )?;
            emit_later(scope, this, &[string(scope, "error")?, error])?;
            return Ok(this);
        }
        this.set_property(scope, c"_starlingPort", f64::from(port))?;
        if !this.flag(scope, c"listening")? {
            call_method(
                scope,
                servers(scope)?,
                c"push",
                &[this.to_jsval_throwing(scope)?],
            )?;
        }
        this.set_property(scope, c"listening", true)?;
        let listener = js::class::get_or_init_shared_function(
            scope,
            on_fetch as *const () as usize,
            |scope| Function::new_callback(scope, c"", 1, on_fetch, ()),
        )?;
        let add = scope.global().val(scope, c"addEventListener")?;
        js::Function::call(
            scope,
            js::value::undefined(),
            add,
            &[string(scope, "fetch")?, listener.to_jsval_throwing(scope)?],
        )?;
        if let Some(callback) = callback {
            call_method(
                scope,
                this,
                c"once",
                &[string(scope, "listening")?, callback],
            )?;
        }
        emit_later(scope, this, &[string(scope, "listening")?])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#serverclosecallback>: stop serving requests, and emit
    /// `close`. `callback` is called on `close`, with `ERR_SERVER_NOT_RUNNING` if the server was
    /// not listening.
    pub fn close<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let servers = servers(scope)?;
        let index = call_method(
            scope,
            servers,
            c"indexOf",
            &[this.to_jsval_throwing(scope)?],
        )?
        .to_number();
        if index >= 0.0 {
            let one = scope.root_value(js::value::from_f64(1.0));
            call_method(
                scope,
                servers,
                c"splice",
                &[scope.root_value(js::value::from_f64(index)), one],
            )?;
        }
        let was_listening = this.flag(scope, c"listening")?;
        this.set_property(scope, c"listening", false)?;
        crate::net::on_close(scope, this, callback, was_listening)?;
        emit_later(scope, this, &[string(scope, "close")?])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#serveraddress>: the port `listen` was given, on every
    /// address, or `null` before `listen`.
    pub fn address<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        if !this.flag(scope, c"listening")? {
            return Ok(scope.root_value(js::value::null()));
        }
        let address = Object::new_plain(scope)?;
        address.set_property(scope, c"address", "::")?;
        address.set_property(scope, c"family", "IPv6")?;
        address.set_property(scope, c"port", this.val(scope, c"_starlingPort")?)?;
        address.to_jsval_throwing(scope)
    }

    /// <https://nodejs.org/api/http.html#serversettimeoutmsecs-callback>: has no effect.
    pub fn set_timeout<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#servercloseallconnections>: has no effect.
    pub fn close_all_connections() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#servercloseidleconnections>: has no effect.
    pub fn close_idle_connections() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#serverref>: has no effect.
    #[method(name = "ref")]
    pub fn ref_impl<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#serverunref>: has no effect.
    pub fn unref<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }
}
