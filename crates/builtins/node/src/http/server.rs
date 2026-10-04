// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `http.Server`, a `net.Server` that serves the runtime's incoming requests, or HTTP/1.1
//! connections it accepts itself.
//!
//! When the runtime runs a script as a command with `--node-http-listen`, `listen()` listens
//! through `net.Server`, and the server's `connection` listener handles the HTTP/1.1 requests of
//! each connection (see [`super::connection`]).
//!
//! Otherwise `listen()` adds a `fetch` event listener, and records the server with its port. Each
//! `fetch` event goes to the listening server whose port is the request URL's, or else to the
//! server that listened first, as a `request` event with an `IncomingMessage` for the web
//! `Request` and a `ServerResponse` whose web `Response` is the event's response. When the
//! runtime runs a script as a command without `--node-http-listen`, nothing serves requests, so
//! `listen()` emits an `ERR_SERVE_MODE_REQUIRED` error. In a component without a
//! `wasi:http/handler` export, `listen()` throws that error.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::incoming::{incoming_message_class, set_headers};
use super::outgoing::server_response_class;
use crate::errors::{validate_integer, validate_plain_object, ErrorKind, NodeError, Thrown};
use crate::stream::{call_method, is_function, next_tick_call, string, Props};

/// The key of the shared array of listening servers.
static SERVERS_KEY: u8 = 0;

/// Whether `listen()` accepts connections through `net.Server`: when the runtime runs a script as
/// a command with `--node-http-listen`.
pub(super) fn listens_itself() -> bool {
    core_runtime::event_loop::running_as_command() && crate::runtime_flags::get().http_listen
}

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

/// The socket a served request reports, with the request's local port, whether it was encrypted,
/// and the client's address, port and family where the transport reports them, which `wasi:http`
/// doesn't.
fn socket<'s>(
    scope: &'s Scope<'_>,
    event: Object<'_>,
    port: Option<u16>,
    encrypted: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let client_address = event
        .cast::<fetch_event::fetch_event::FetchEvent>()
        .ok()
        .and_then(|event| event.client_address());
    let remote = client_address.map(|client| {
        let family = if client.is_ipv4() { "IPv4" } else { "IPv6" };
        (client.ip().to_string(), client.port(), family)
    });
    let connection = super::host_socket::HostConnection {
        remote,
        local_port: port,
        encrypted,
        owner: None,
    };
    super::host_socket::new_host_socket(scope, connection)?.to_jsval_throwing(scope)
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
    let req = Function::construct(
        scope,
        incoming_message_class::class_constructor(scope)?,
        &[socket(scope, event, port, encrypted)?],
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
    res.set_property(scope, c"_starlingWire", false)?;
    let req_socket = req.val(scope, c"socket")?;
    res.set_property(scope, c"socket", req_socket)?;
    res.set_property(scope, c"connection", req_socket)?;
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

/// Call the `net.Server` method `name` on `server` with `args`.
fn call_net<'s>(
    scope: &'s Scope<'_>,
    server: Object<'_>,
    name: &'static std::ffi::CStr,
    args: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    let class = crate::net::server::server_class::class_constructor(scope)?;
    let proto = class.obj(scope, c"prototype")?.ok_or(ExnThrown)?;
    let method = proto.val(scope, name)?;
    crate::stream::call(scope, server.to_jsval_throwing(scope)?, method, args)
}

/// `validateBoolean(value, name)`.
fn validate_boolean(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<(), Thrown> {
    if !value.is_boolean() {
        return Err(NodeError::invalid_arg_type(scope, name, "of type boolean", value).into());
    }
    Ok(())
}

/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: f64 = 9007199254740991.0;

/// The integer option `name` of `options`, at least 0, or `default` if it is `undefined`.
fn integer_option(
    scope: &Scope<'_>,
    options: Option<Object<'_>>,
    name: &'static std::ffi::CStr,
    default: f64,
) -> Result<f64, Thrown> {
    let value = option(scope, options, name)?;
    if value.is_undefined() {
        return Ok(default);
    }
    let label = name.to_str().unwrap_or_default();
    validate_integer(scope, value, label, 0.0, MAX_SAFE_INTEGER)
}

/// The option `name` of `options`, or `undefined` without options.
fn option<'s>(
    scope: &'s Scope<'_>,
    options: Option<Object<'_>>,
    name: &'static std::ffi::CStr,
) -> Result<HandleValue<'s>, ExnThrown> {
    match options {
        Some(options) => options.val(scope, name),
        None => Ok(scope.root_value(js::value::undefined())),
    }
}

/// Validate the HTTP options `options` and store them on `server`, as Node.js's
/// `storeHTTPOptions` does.
fn store_options(
    scope: &Scope<'_>,
    server: Object<'_>,
    options: Option<Object<'_>>,
) -> Result<(), Thrown> {
    for (name, hidden) in [
        (c"IncomingMessage", c"_starlingIncomingMessage"),
        (c"ServerResponse", c"_starlingServerResponse"),
    ] {
        let class = option(scope, options, name)?;
        if crate::stream::truthy(scope, class) {
            server.define_property(scope, hidden, class, 0)?;
        }
    }
    let max_header_size = option(scope, options, c"maxHeaderSize")?;
    if !max_header_size.is_undefined() {
        validate_integer(
            scope,
            max_header_size,
            "maxHeaderSize",
            0.0,
            MAX_SAFE_INTEGER,
        )?;
    }
    server.set_property(scope, c"maxHeaderSize", max_header_size)?;
    let optimize = option(scope, options, c"optimizeEmptyRequests")?;
    if !optimize.is_undefined() {
        validate_boolean(scope, optimize, "options.optimizeEmptyRequests")?;
    }
    let insecure = option(scope, options, c"insecureHTTPParser")?;
    if !insecure.is_undefined() {
        validate_boolean(scope, insecure, "options.insecureHTTPParser")?;
    }
    server.set_property(scope, c"insecureHTTPParser", insecure)?;
    let validation = option(scope, options, c"httpValidation")?;
    if !validation.is_undefined() {
        let text = crate::stream::rust_string(scope, validation);
        if !matches!(text.as_deref(), Some("strict" | "relaxed" | "insecure")) {
            return Err(NodeError::invalid_arg_value(
                scope,
                "options.httpValidation",
                validation,
                "must be one of: 'strict', 'relaxed', 'insecure'",
            )
            .into());
        }
        if !insecure.is_undefined() {
            return Err(NodeError::invalid_arg_value(
                scope,
                "options.httpValidation",
                validation,
                "cannot be used together with options.insecureHTTPParser",
            )
            .into());
        }
    }
    server.set_property(scope, c"httpValidation", validation)?;
    let request_timeout = integer_option(scope, options, c"requestTimeout", 300_000.0)?;
    server.set_property(scope, c"requestTimeout", request_timeout)?;
    let given_headers_timeout = option(scope, options, c"headersTimeout")?;
    let headers_timeout = integer_option(
        scope,
        options,
        c"headersTimeout",
        request_timeout.min(60_000.0),
    )?;
    server.set_property(scope, c"headersTimeout", headers_timeout)?;
    if request_timeout > 0.0 && headers_timeout > 0.0 && headers_timeout > request_timeout {
        let received = if given_headers_timeout.is_number() {
            crate::errors::out_of_range_received(given_headers_timeout.to_number())
        } else {
            crate::errors::describe_value(scope, given_headers_timeout)
        };
        return Err(
            NodeError::out_of_range("headersTimeout", "<= requestTimeout", &received).into(),
        );
    }
    for (name, default) in [
        (c"keepAliveTimeout", 5_000.0),
        (c"keepAliveTimeoutBuffer", 1_000.0),
        (c"connectionsCheckingInterval", 30_000.0),
    ] {
        let value = integer_option(scope, options, name, default)?;
        server.set_property(scope, name, value)?;
    }
    for (name, label, default) in [
        (
            c"requireHostHeader",
            "options.requireHostHeader",
            Some(true),
        ),
        (
            c"joinDuplicateHeaders",
            "options.joinDuplicateHeaders",
            None,
        ),
        (
            c"rejectNonStandardBodyWrites",
            "options.rejectNonStandardBodyWrites",
            Some(false),
        ),
    ] {
        let value = option(scope, options, name)?;
        if !value.is_undefined() {
            validate_boolean(scope, value, label)?;
            server.set_property(scope, name, value)?;
        } else {
            match default {
                Some(default) => server.set_property(scope, name, default)?,
                None => server.set_property(scope, name, js::value::undefined())?,
            }
        }
    }
    let callback = option(scope, options, c"shouldUpgradeCallback")?;
    if !callback.is_undefined() {
        crate::errors::validate_function(scope, callback, "options.shouldUpgradeCallback")?;
        server.set_property(scope, c"shouldUpgradeCallback", callback)?;
    } else {
        let default = shared_function(scope, super::connection::default_should_upgrade, 1)?;
        server.set_property(scope, c"shouldUpgradeCallback", default)?;
    }
    Ok(())
}

/// The shared function of the current global for `body`.
fn shared_function<'s>(
    scope: &'s Scope<'_>,
    body: js::function::Callback,
    nargs: u32,
) -> Result<Object<'s>, ExnThrown> {
    let function =
        js::class::get_or_init_shared_function(scope, body as *const () as usize, |scope| {
            Function::new_callback(scope, c"", nargs, body, ())
        })?;
    Ok(*function)
}

#[core_runtime::jsfunction_class(
    name = "Server",
    extends = "crate::net::server::server_class",
    self_construct
)]
pub mod server_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpserver>: a server whose `request` events go to
    /// `listener`. The options are validated and stored as Node.js stores them. Of them,
    /// `IncomingMessage`, `ServerResponse`, `maxHeaderSize`, `requestTimeout`, `headersTimeout`,
    /// `keepAliveTimeout`, `keepAliveTimeoutBuffer`, `connectionsCheckingInterval`,
    /// `requireHostHeader`, `joinDuplicateHeaders`, `rejectNonStandardBodyWrites` and
    /// `shouldUpgradeCallback` apply to the connections a server accepts itself, and the others
    /// have no effect.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
        listener: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let (options, listener) = match options {
            Some(options) if is_function(scope, options) => (None, Some(options)),
            Some(options) if options.is_null() || options.is_undefined() => (None, listener),
            Some(options) => (
                Some(validate_plain_object(scope, options, "options", false)?),
                listener,
            ),
            None => (None, listener),
        };
        store_options(scope, this, options)?;
        let net_options = Object::new_plain(scope)?;
        net_options.set_property(scope, c"allowHalfOpen", true)?;
        let no_delay = option(scope, options, c"noDelay")?;
        if no_delay.is_null() || no_delay.is_undefined() {
            net_options.set_property(scope, c"noDelay", true)?;
        } else {
            net_options.set_property(scope, c"noDelay", no_delay)?;
        }
        for name in [c"keepAlive", c"keepAliveInitialDelay", c"highWaterMark"] {
            net_options.set_property(scope, name, option(scope, options, name)?)?;
        }
        crate::net::server::server_class::init(
            scope,
            this,
            Some(net_options.to_jsval_throwing(scope)?),
            None,
        )?;
        if let Some(listener) = listener.filter(|listener| is_function(scope, *listener)) {
            call_method(scope, this, c"on", &[string(scope, "request")?, listener])?;
        }
        this.set_property(scope, c"httpAllowHalfOpen", false)?;
        let on_connection = shared_function(scope, super::super::connection::on_connection, 1)?;
        call_method(
            scope,
            this,
            c"on",
            &[
                string(scope, "connection")?,
                on_connection.to_jsval_throwing(scope)?,
            ],
        )?;
        let on_listening = shared_function(scope, super::super::connection::on_listening, 0)?;
        call_method(
            scope,
            this,
            c"on",
            &[
                string(scope, "listening")?,
                on_listening.to_jsval_throwing(scope)?,
            ],
        )?;
        this.set_property(scope, c"timeout", 0)?;
        this.set_property(scope, c"maxHeadersCount", js::value::null())?;
        this.set_property(scope, c"maxRequestsPerSocket", 0)?;
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#serverlisten>: serve requests to `port`, or to any port.
    /// When the runtime runs a script as a command with `--node-http-listen`, this is
    /// `net.Server`'s `listen`.
    pub fn listen<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let args: Vec<HandleValue<'_>> =
            args.iter().map(|arg| scope.root_value(arg.get())).collect();
        if super::listens_itself() {
            this.set_property(scope, c"_starlingClosing", false)?;
            call_net(scope, this, c"listen", &args)?;
            return Ok(this);
        }
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
                    "Serving HTTP requests needs serve mode: run the script with --serve <port>, or serve the component with a wasi:http host, or pass --node-http-listen",
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
    /// not listening. A server that accepts connections itself destroys its idle connections,
    /// closes the others once their response is finished, and emits `close` once they are
    /// closed.
    pub fn close<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        if super::listens_itself() {
            super::super::connection::close_connections(scope, this, true)?;
            super::super::connection::stop_checking(scope, this)?;
            this.set_property(scope, c"_starlingClosing", true)?;
            let callback = callback.unwrap_or_else(|| scope.root_value(js::value::undefined()));
            call_net(scope, this, c"close", &[callback])?;
            return Ok(this);
        }
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

    /// <https://nodejs.org/api/net.html#serveraddress>: the address `net.Server` listens on, or,
    /// when serving the runtime's requests, the port `listen` was given, on every address. `null`
    /// before `listen`.
    pub fn address<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        if super::listens_itself() {
            return call_net(scope, this, c"address", &[]);
        }
        if !this.flag(scope, c"listening")? {
            return Ok(scope.root_value(js::value::null()));
        }
        let address = Object::new_plain(scope)?;
        address.set_property(scope, c"address", "::")?;
        address.set_property(scope, c"family", "IPv6")?;
        address.set_property(scope, c"port", this.val(scope, c"_starlingPort")?)?;
        address.to_jsval_throwing(scope)
    }

    /// <https://nodejs.org/api/http.html#serversettimeoutmsecs-callback>: the idle timeout of
    /// the connections the server accepts from now on. `callback` becomes a `timeout` listener.
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        msecs: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let msecs = msecs.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        this.set_property(scope, c"timeout", msecs)?;
        if let Some(callback) = callback.filter(|callback| crate::stream::truthy(scope, *callback))
        {
            call_method(scope, this, c"on", &[string(scope, "timeout")?, callback])?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#servercloseallconnections>: destroy the connections
    /// the server accepted itself.
    pub fn close_all_connections(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        super::super::connection::close_connections(scope, this, false)
    }

    /// <https://nodejs.org/api/http.html#servercloseidleconnections>: destroy the connections
    /// the server accepted itself that are between requests and have no response in progress.
    pub fn close_idle_connections(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        super::super::connection::close_connections(scope, this, true)
    }
}
