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
//! server that listened first, as a `request` event with an `IncomingMessage` that reads the
//! event's request and a `ServerResponse` that responds to the event with a web `Response` it
//! builds natively. When the runtime runs a script as a command without `--node-http-listen`,
//! nothing serves requests, so `listen()` emits an `ERR_SERVE_MODE_REQUIRED` error. In a
//! component without a `wasi:http/handler` export, `listen()` throws that error.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::incoming::{new_message, MessageHead};
use super::outgoing::{Served, Transport};
use crate::errors::{validate_integer, validate_plain_object, ErrorKind, NodeError, Thrown};
use crate::net::server::{ServerState, ServerStateImpl};
use crate::support::emit_later;

/// The state of an `http.Server`: the state of the `net.Server`, and the connections, timer and
/// options of the server.
#[core_runtime::jsclass(hidden, extends = ServerState)]
pub(crate) struct HttpServerState {
    parent: ServerStateImpl,
    /// The connections of a server that accepts them itself, as `ConnectionState`s.
    pub(super) connections: Vec<Heap<js::object::Object>>,
    /// The ID of the timer that checks the connections for timed-out requests.
    #[no_trace]
    pub(super) check_timer: Option<u64>,
    /// Whether `close` was called on a server that accepts connections itself.
    #[no_trace]
    pub(super) closing: bool,
    /// The port a server that serves the runtime's requests listens on.
    #[no_trace]
    port: Option<u16>,
    /// The class of the requests, from the `IncomingMessage` option.
    pub(super) incoming_class: Option<Heap<js::object::Object>>,
    /// The class of the responses, from the `ServerResponse` option.
    pub(super) response_class: Option<Heap<js::object::Object>>,
    /// The lowercase names of the `uniqueHeaders` option, which the responses get.
    #[no_trace]
    pub(super) unique_headers: Option<Vec<String>>,
}

#[core_runtime::jsmethods]
impl HttpServerState {}

/// The key of the shared array of listening servers.
static SERVERS_KEY: u8 = 0;

/// Whether `listen()` accepts connections through `net.Server`: when the runtime runs a script as
/// a command with `--node-http-listen`.
pub(super) fn listens_itself() -> bool {
    core_runtime::event_loop::running_as_command() && crate::runtime_flags::get().http_listen
}

/// The listening servers of the current global, in the order they started listening.
fn servers<'s>(scope: &'s Scope<'_>) -> Result<js::Array<'s>, ExnThrown> {
    let servers =
        js::class::get_or_init_shared_object(scope, &SERVERS_KEY as *const u8 as usize, |scope| {
            Ok(*js::Array::new(scope, 0)?)
        })?;
    servers
        .cast::<js::Array>()
        .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))
}

/// The listening server that serves a request to `port`.
fn server_for<'s>(
    scope: &'s Scope<'_>,
    port: Option<u16>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let mut first = None;
    for server in servers(scope)?.to_vec(scope)? {
        let Ok(server) = Object::from_value(scope, server) else {
            continue;
        };
        if first.is_none() {
            first = Some(server);
        }
        let listened =
            server_class::try_state_of(scope, server)?.and_then(|state| state.data().port);
        if port.is_some() && listened == port {
            return Ok(Some(server));
        }
    }
    Ok(first)
}

/// The authority of `url`: its host, followed by its port if it has one.
fn authority(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// The event `server` would emit with the request's socket for a request with `method` and the
/// header list `headers`, if it has listeners for it: `connect` for a `CONNECT` request, and
/// `upgrade` for a request with an `Upgrade` header.
fn socket_event(
    scope: &Scope<'_>,
    server: Object<'_>,
    method: &str,
    headers: &[(String, String)],
) -> Result<Option<&'static std::ffi::CStr>, ExnThrown> {
    let name = if method.eq_ignore_ascii_case("CONNECT") {
        c"connect"
    } else if headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("upgrade"))
    {
        c"upgrade"
    } else {
        return Ok(None);
    };
    let count =
        crate::events::listener_count(scope, server, js::class::get_or_init_atom(scope, name)?)?;
    Ok((count > 0.0).then_some(name))
}

/// Respond to `event` with `response`.
fn respond_with(
    scope: &Scope<'_>,
    event: Object<'_>,
    response: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    Function::call_by_name(scope, event.handle(), c"respondWith", &[response])?;
    Ok(())
}

/// Respond to `event` with a 501, since served requests have no socket to emit `event_name`
/// with.
fn respond_not_implemented(
    scope: &Scope<'_>,
    event: Object<'_>,
    event_name: &std::ffi::CStr,
) -> Result<(), ExnThrown> {
    let body = format!(
        "Served requests have no socket for a '{}' listener",
        event_name.to_string_lossy()
    );
    let length = body.len() as u64;
    let record = web_fetch::response::ResponseRecord {
        status: 501,
        ..Default::default()
    };
    let body = web_fetch::algorithms::Body {
        source: web_fetch::algorithms::BodySource::Bytes(bytes::Bytes::from(body.into_bytes())),
        length: Some(length),
        source_disturbed: false,
    };
    let response = web_fetch::algorithms::create_a_response_object(
        scope,
        record,
        vec![(
            "content-type".to_string(),
            "text/plain;charset=UTF-8".to_string(),
        )],
        web_fetch::headers::Guard::Immutable,
        Some(body),
        None,
    )?;
    respond_with(scope, event, response.to_jsval_throwing(scope)?)
}

/// The `fetch` event listener: respond to the event through the listening server for its request.
fn on_fetch(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let event = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    let fetch_event = event
        .cast::<fetch_event::fetch_event::FetchEvent>()
        .map_err(|_| js::error::TypeError("not a FetchEvent".into()).throw(scope))?;
    let request = fetch_event.request(scope);
    let url = request.current_url();
    let method = request.method().to_string();
    let port = url.port_or_known_default();
    let Some(server) = server_for(scope, port)? else {
        return Ok(js::value::undefined());
    };
    let mut headers = request.headers(scope).list();
    if let Some(name) = socket_event(scope, server, &method, &headers)? {
        respond_not_implemented(scope, event, name)?;
        return Ok(js::value::undefined());
    }

    let connection = super::host_socket::HostConnection {
        remote: fetch_event.client_address(),
        local_port: port,
        encrypted: url.scheme() == "https",
        owner: None,
    };
    let socket = super::host_socket::new_host_socket(scope, connection)?;
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    // A `wasi:http` host passes the host as the request's authority rather than as a header.
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        if let Some(authority) = authority(&url) {
            headers.insert(0, ("host".to_string(), authority));
        }
    }
    let head = MessageHead {
        version: Some((1, 1)),
        method: Some(&method),
        url: Some(&path),
        headers: Some(headers),
        ..MessageHead::default()
    };
    let req = new_message(scope, socket.to_jsval_throwing(scope)?, head)?;
    if let Some(body) = request.take_incoming_body() {
        super::incoming::set_host_source(scope, req, body, None)?;
    } else if let Some(stream) = request.body(scope)? {
        super::incoming::set_web_source(scope, req, stream)?;
    }

    let res = super::outgoing::new_response(
        scope,
        req,
        None,
        crate::stream::state::default_high_water_mark(false),
        false,
    )?;
    let req_value = req.to_jsval_throwing(scope)?;
    let promise = js::Promise::new_pending(scope)?;
    {
        let state = super::outgoing::state_of(scope, res)?;
        let mut data = state.data_mut();
        data.socket = Some(Heap::from(socket));
        data.transport = Transport::Served(Served::new(promise));
    }
    super::host_socket::set_owner(scope, socket, Some(res))?;
    respond_with(scope, event, promise.to_jsval_throwing(scope)?)?;
    let emitted = crate::events::emit(
        scope,
        server,
        c"request",
        &[req_value, res.to_jsval_throwing(scope)?],
    );
    if emitted.is_err() {
        // We reject the response so the request doesn't wait for a response forever, and leave
        // the listener's exception pending.
        let error = js::exception::take_pending_or_undefined(scope);
        super::outgoing::reject_served(scope, res, error)?;
        js::exception::set_pending(
            scope,
            error,
            js::native::ExceptionStackBehavior::DoNotCapture,
        );
        return Err(ExnThrown);
    }
    Ok(js::value::undefined())
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
        Some(options) => options.get_property(scope, name),
        None => Ok(HandleValue::undefined()),
    }
}

/// Validate the HTTP options `options` and store them on `server`, as Node.js's
/// `storeHTTPOptions` does. The `IncomingMessage` and `ServerResponse` classes are kept in the
/// server's state.
fn store_options(
    scope: &Scope<'_>,
    server: Object<'_>,
    options: Option<Object<'_>>,
) -> Result<(), Thrown> {
    let classes = [c"IncomingMessage", c"ServerResponse"].map(|name| {
        option(scope, options, name).map(|class| {
            Object::from_value(scope, *class)
                .ok()
                .filter(|_| js::conversion::to_boolean(class))
        })
    });
    let [incoming_class, response_class] = classes;
    let (incoming_class, response_class) = (incoming_class?, response_class?);
    let unique = option(scope, options, c"uniqueHeaders")?;
    let unique = super::outgoing::parse_unique_headers(scope, unique)?;
    {
        let state = server_class::state_of(scope, server)?;
        let mut data = state.data_mut();
        data.incoming_class = incoming_class.map(Heap::from);
        data.response_class = response_class.map(Heap::from);
        data.unique_headers = unique;
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
        crate::errors::validate_boolean(scope, optimize, "options.optimizeEmptyRequests")?;
    }
    let insecure = option(scope, options, c"insecureHTTPParser")?;
    let validation = option(scope, options, c"httpValidation")?;
    super::validate_parser_options(scope, insecure, validation)?;
    server.set_property(scope, c"insecureHTTPParser", insecure)?;
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
            crate::errors::validate_boolean(scope, value, label)?;
            server.set_property(scope, name, value)?;
        } else {
            match default {
                Some(default) => server.set_property(scope, name, default)?,
                None => server.set_property(scope, name, HandleValue::undefined())?,
            }
        }
    }
    let callback = option(scope, options, c"shouldUpgradeCallback")?;
    if !callback.is_undefined() {
        crate::errors::validate_function(scope, callback, "options.shouldUpgradeCallback")?;
        server.set_property(scope, c"shouldUpgradeCallback", callback)?;
    } else {
        let default =
            crate::support::shared_function(scope, super::connection::default_should_upgrade, 1)?;
        server.set_property(scope, c"shouldUpgradeCallback", default)?;
    }
    Ok(())
}

#[core_runtime::jsfunction_class(
    name = "Server",
    extends = "crate::net::server::server_class",
    state = "HttpServerState",
    invalid_this = "crate::errors::invalid_this",
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
    #[constructor(length = 2)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
        listener: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let (options, listener) = match options {
            Some(options) if js::function::is_callable(options) => (None, Some(options)),
            Some(options) if options.is_null_or_undefined() => (None, listener),
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
        if no_delay.is_null_or_undefined() {
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
        if let Some(listener) = listener.filter(|listener| js::function::is_callable(*listener)) {
            let listener =
                Object::from_value(scope, *listener).map_err(|e| Thrown::from(e.throw(scope)))?;
            crate::events::listen(scope, this, c"on", c"request", listener)?;
        }
        this.set_property(scope, c"httpAllowHalfOpen", false)?;
        let on_connection =
            crate::support::shared_function(scope, super::super::connection::on_connection, 1)?;
        crate::events::listen(scope, this, c"on", c"connection", on_connection)?;
        let on_listening =
            crate::support::shared_function(scope, super::super::connection::on_listening, 0)?;
        crate::events::listen(scope, this, c"on", c"listening", on_listening)?;
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
        let state = state_of(scope, this)?;
        if super::listens_itself() {
            state.data_mut().closing = false;
            return crate::net::server::server_class::listen(scope, this, args);
        }
        let args: Vec<HandleValue<'_>> = args.iter().copied().collect();
        let crate::net::server::ListenArgs { port, callback, .. } =
            crate::net::listen_args(scope, &args)?;
        if core_runtime::event_loop::without_request_handler() {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_SERVE_MODE_REQUIRED",
                "http.Server.listen() can't serve requests in a component without a wasi:http/handler export: componentize it with --serve",
            )
            .into());
        }
        if core_runtime::event_loop::running_as_command() {
            let error = NodeError::new(
                    ErrorKind::Error,
                    "ERR_SERVE_MODE_REQUIRED",
                    "Serving HTTP requests needs serve mode: run the script with --serve <port>, or serve the component with a wasi:http host, or pass --node-http-listen",
                ).to_value(scope)?;
            emit_later(scope, this, c"error", &[error])?;
            return Ok(this);
        }
        let listening = {
            let mut data = state.data_mut();
            data.port = Some(port);
            std::mem::replace(&mut data.parent.listening, true)
        };
        if !listening {
            servers(scope)?.push(scope, this)?;
        }
        this.set_property(scope, c"_handle", Object::new_plain(scope)?)?;
        let listener = crate::support::shared_function(scope, on_fetch, 1)?;
        scope
            .global()
            .cast::<web_globals::events::EventTarget>()
            .expect("the global is an event target")
            .add_event_listener(
                scope,
                "fetch".to_string(),
                listener.to_jsval_throwing(scope)?,
                None,
            )?;
        if let Some(callback) = callback {
            let callback =
                Object::from_value(scope, *callback).map_err(|e| Thrown::from(e.throw(scope)))?;
            crate::events::listen(scope, this, c"once", c"listening", callback)?;
        }
        emit_later(scope, this, c"listening", &[])?;
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
        let state = state_of(scope, this)?;
        if super::listens_itself() {
            super::super::connection::close_connections(scope, this, true)?;
            super::super::connection::stop_checking(scope, this)?;
            state.data_mut().closing = true;
            return crate::net::server::server_class::close(scope, this, callback);
        }
        let servers = servers(scope)?;
        let index = servers.index_of(scope, this.to_jsval_throwing(scope)?)?;
        if let Some(index) = index {
            servers.remove(scope, index, 1)?;
        }
        let was_listening = std::mem::replace(&mut state.data_mut().parent.listening, false);
        this.set_property(scope, c"_handle", js::value::null())?;
        crate::net::on_close(scope, this, callback, was_listening)?;
        emit_later(scope, this, c"close", &[])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#serveraddress>: the address `net.Server` listens on, or,
    /// when serving the runtime's requests, the port `listen` was given, on every address. `null`
    /// before `listen`.
    pub fn address<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        if super::listens_itself() {
            return crate::net::server::server_class::address(scope, this);
        }
        let port = {
            let state = state_of(scope, this)?;
            let data = state.data();
            data.port.filter(|_| data.parent.listening)
        };
        let Some(port) = port else {
            return Ok(HandleValue::null());
        };
        let address = Object::new_plain(scope)?;
        address.set_property(scope, c"address", "::")?;
        address.set_property(scope, c"family", "IPv6")?;
        address.set_property(scope, c"port", f64::from(port))?;
        address.to_jsval_throwing(scope)
    }

    /// <https://nodejs.org/api/http.html#serversettimeoutmsecs-callback>: the idle timeout of
    /// the connections the server accepts from now on. `callback` becomes a `timeout` listener.
    #[method(length = 2)]
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        msecs: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let msecs = msecs.unwrap_or(HandleValue::undefined());
        this.set_property(scope, c"timeout", msecs)?;
        if let Some(callback) = callback.filter(|callback| js::conversion::to_boolean(*callback)) {
            let callback = Object::from_value(scope, *callback).map_err(|e| e.throw(scope))?;
            crate::events::listen(scope, this, c"on", c"timeout", callback)?;
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
