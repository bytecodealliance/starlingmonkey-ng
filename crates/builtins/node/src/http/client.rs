// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `ClientRequest`, which sends a request through `platform::http`, and `Agent`.
//!
//! The request body is collected in the request's state until `end()` and then sent with the
//! request. The host handles connections, so an agent only keeps its options, and the sockets of
//! a `keepAlive` agent's finished requests in its `freeSockets`.

use std::time::Duration;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyInit, PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};
use platform::http::{OutgoingBody, Request};

use super::incoming::{new_message, set_host_source, MessageHead};
use super::outgoing::{
    header_args, header_pairs, outgoing_message_class, ClientPart, OutgoingState, Transport,
};
use super::status_text;
use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::support::{emit_later, spawn_cancellable, until_cancelled};

/// The headers the client leaves out besides those that frame the request, which the host sets:
/// none natively, and on wasm the ones the host refuses.
#[cfg(not(target_arch = "wasm32"))]
const REFUSED_HEADERS: [&str; 0] = [];
#[cfg(target_arch = "wasm32")]
const REFUSED_HEADERS: [&str; 5] = [
    "host",
    "upgrade",
    "proxy-connection",
    "proxy-authorization",
    "http2-settings",
];

/// The headers that framing and connection management own, which the host sets itself.
const HOST_HEADERS: [&str; 4] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
];

/// Throw the error for the transport failure `message` of a request to `url`, with the `code`
/// its cause has in Node.js where the message names one: `ECONNREFUSED`, `ECONNRESET`,
/// `ENOTFOUND` or `ETIMEDOUT`. The URL in the message is not taken for a cause.
pub(super) fn transport_error(scope: &Scope<'_>, message: &str, url: Option<&str>) -> ExnThrown {
    let mut lower = message.to_ascii_lowercase();
    if let Some(url) = url {
        lower = lower.replace(&url.to_ascii_lowercase(), "");
    }
    let has = |patterns: &[&str]| patterns.iter().any(|pattern| lower.contains(pattern));
    let code = if has(&["connection refused", "connectionrefused"]) {
        Some("ECONNREFUSED")
    } else if has(&[
        "connection reset",
        "connectionterminated",
        "connection closed before message completed",
    ]) {
        Some("ECONNRESET")
    } else if has(&["dns", "failed to lookup"]) {
        Some("ENOTFOUND")
    } else if has(&["timed out", "timeout"]) {
        Some("ETIMEDOUT")
    } else {
        None
    };
    match code {
        Some(code) => NodeError::new(ErrorKind::Error, code, message).throw(scope),
        None => {
            let message =
                std::ffi::CString::new(message.replace('\0', "\\0")).expect("NULs were replaced");
            js::error::throw_plain_error(scope, &message)
        }
    }
}

/// The options a URL gives a request: `protocol`, `hostname`, `port`, `path` and `auth`.
fn url_options<'s>(scope: &'s Scope<'_>, href: &str) -> Result<Object<'s>, Thrown> {
    let url = url::Url::parse(href)
        .map_err(|_| NodeError::new(ErrorKind::TypeError, "ERR_INVALID_URL", "Invalid URL"))?;
    let options = Object::new_plain(scope)?;
    options.set_property(scope, c"protocol", format!("{}:", url.scheme()).as_str())?;
    let host = url.host_str().unwrap_or_default();
    options.set_property(
        scope,
        c"hostname",
        host.trim_start_matches('[').trim_end_matches(']'),
    )?;
    if let Some(port) = url.port() {
        options.set_property(scope, c"port", f64::from(port))?;
    }
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    options.set_property(scope, c"path", path.as_str())?;
    let password = url.password().unwrap_or_default();
    if !url.username().is_empty() || !password.is_empty() {
        let (Some(user), Some(password)) = (
            crate::url::decode_uri_component(url.username()),
            crate::url::decode_uri_component(password),
        ) else {
            return Err(Thrown::from(js::error::throw_uri_error(
                scope,
                c"URI malformed",
            )));
        };
        options.set_property(scope, c"auth", format!("{user}:{password}").as_str())?;
    }
    Ok(options)
}

/// The URL `value` is, a string or a WHATWG `URL`.
pub(super) fn url_of(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<String>, ExnThrown> {
    if value.is_string() {
        return Ok(Some(String::from_jsval_throwing(scope, value, ())?));
    }
    let Ok(object) = Object::from_value(scope, *value) else {
        return Ok(None);
    };
    let href = object.get_property(scope, c"href")?;
    if href.is_string() && object.get_property(scope, c"searchParams")?.is_object() {
        return Ok(Some(String::from_jsval_throwing(scope, href, ())?));
    }
    Ok(None)
}

/// `options[name]` as a string, or `None` if it is `null` or `undefined`.
fn string_option(
    scope: &Scope<'_>,
    options: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Option<String>, ExnThrown> {
    let value = options.get_property(scope, name)?;
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    Ok(Some(String::from_jsval_throwing(scope, value, ())?))
}

/// `options[name]` as a host name, or `None` if it is `null` or `undefined`, or
/// `ERR_INVALID_ARG_TYPE` naming the option `label` if it is not a string.
fn host_option(
    scope: &Scope<'_>,
    options: Object<'_>,
    name: &'static std::ffi::CStr,
    label: &str,
) -> Result<Option<String>, Thrown> {
    let value = options.get_property(scope, name)?;
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    if !value.is_string() {
        return Err(NodeError::invalid_arg_type(scope, label, "of type string", value).into());
    }
    Ok(Some(String::from_jsval(scope, value, ())?).filter(|host| !host.is_empty()))
}

/// Run `f` on the client part of the state of a request, if it is a client request.
fn with_client<R>(state: &OutgoingState<'_>, f: impl FnOnce(&mut ClientPart) -> R) -> Option<R> {
    match &mut state.data_mut().transport {
        Transport::Client(client) => Some(f(client)),
        _ => None,
    }
}

/// The completion of a request's send: emit `response` with the response, consuming it if
/// nothing listens, or emit `error` and `close` after a failure, unless the request was
/// destroyed.
fn send_done(
    scope: &Scope<'_>,
    request: Object<'_>,
    result: Result<HandleValue<'_>, HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let state = super::outgoing::state_of(scope, request)?;
    clear_timer(&state);
    with_client(&state, |client| client.cancel = None);
    let destroyed = state.data().destroyed;
    let response = match result {
        Ok(response) => Object::from_value(scope, response).map_err(|e| e.throw(scope))?,
        Err(error) => {
            if destroyed {
                return Ok(());
            }
            state.data_mut().destroyed = true;
            request.set_property(scope, c"destroyed", true)?;
            crate::events::emit(scope, request, c"error", &[error])?;
            crate::events::emit(scope, request, c"close", &[])?;
            return Ok(());
        }
    };
    if destroyed {
        Function::call_by_name(
            scope,
            response.handle(),
            c"destroy",
            js::function::EmptyArgs,
        )?;
        return Ok(());
    }
    with_client(&state, |client| {
        client.response = Some(Heap::from(response))
    });
    request.set_property(scope, c"res", response)?;
    response.set_property(scope, c"req", request)?;
    let close = Function::new_callback(scope, c"", 0, emit_request_close, request)?;
    crate::events::listen(scope, response, c"once", c"end", *close)?;
    let listened = crate::events::emit(
        scope,
        request,
        c"response",
        &[response.to_jsval_throwing(scope)?],
    )?;
    if js::conversion::to_boolean(request.get_property(scope, c"aborted")?) || !listened {
        Function::call_by_name(scope, response.handle(), c"resume", js::function::EmptyArgs)?;
    }
    Ok(())
}

/// The `end` listener of a response, whose payload is its request: return the request's socket
/// to its agent, and emit `close` on the request.
fn emit_request_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<js::Object>(scope, payload);
    release_socket(scope, request)?;
    emit_later(scope, request, c"close", &[])?;
    Ok(js::value::undefined())
}

/// The timer callback of `setTimeout`, whose `this` is the request: emit `timeout`.
fn on_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = crate::support::this_object(scope, &args)?;
    let state = super::outgoing::state_of(scope, request)?;
    with_client(&state, |client| client.timer = None);
    crate::events::emit(scope, request, c"timeout", &[])?;
    Ok(js::value::undefined())
}

/// Cancel the timer `setTimeout` started for the request of `state`, if it is pending.
fn clear_timer(state: &OutgoingState<'_>) {
    if let Some(Some(id)) = with_client(state, |client| client.timer.take()) {
        core_runtime::event_loop::with_active_event_loop(|el| el.clear_js_timer(id));
    }
}

/// Send `request` with the bytes written to it as its body. `destroy` cancels the send. A request
/// whose host name can't be in a URL fails with `ENOTFOUND`.
fn start(
    scope: &Scope<'_>,
    request: Object<'_>,
    state: &OutgoingState<'_>,
) -> Result<(), ExnThrown> {
    let (url, body) = with_client(state, |client| {
        (client.url.clone(), std::mem::take(&mut client.body))
    })
    .unwrap_or_default();
    if url.is_empty() {
        let host = request.get_property(scope, c"host")?;
        let name = String::from_jsval_throwing(scope, host, ())?;
        let error = crate::fs::UvError::net(
            &platform::net::NetError::NoAddress(name.clone()),
            "getaddrinfo",
        )
        .suffix(format!(" {name}"))
        .hostname(&name)
        .into_value(scope)?;
        return send_done(scope, request, Err(error));
    }
    let headers = header_pairs(scope, state)?
        .into_iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            !HOST_HEADERS.contains(&name.as_str()) && !REFUSED_HEADERS.contains(&name.as_str())
        })
        .collect();
    let method = request.get_property(scope, c"method")?;
    let outgoing = Request {
        method: String::from_jsval(scope, method, ()).unwrap_or_else(|_| "GET".into()),
        url: url.clone(),
        headers,
        body: OutgoingBody::Bytes(body.into()),
    };
    let canceller = spawn_cancellable(
        scope,
        request,
        |cancelled| until_cancelled(cancelled, platform::http::send(outgoing)),
        move |scope, request, result| {
            let response = result.map_err(|error| transport_error(scope, &error.0, Some(&url)))?;
            let state = super::outgoing::state_of(scope, request)?;
            let socket = state.data().socket.get(scope);
            let socket = match socket {
                Some(socket) => scope.root_value(socket.as_value()),
                None => HandleValue::undefined(),
            };
            let head = MessageHead {
                version: Some((1, 1)),
                status: Some((response.status, status_text(response.status).unwrap_or(""))),
                headers: Some(response.headers),
                ..MessageHead::default()
            };
            let message = new_message(scope, socket, head)?;
            set_host_source(scope, message, response.body, Some(url))?;
            message.to_jsval_throwing(scope)
        },
        send_done,
    )?;
    with_client(state, |client| client.cancel = Some(canceller));
    Ok(())
}

/// Keep `bytes` of the body of the request `request`, and send the request with the body kept if
/// `last` and it wasn't destroyed.
pub(super) fn send(
    scope: &Scope<'_>,
    request: Object<'_>,
    state: &OutgoingState<'_>,
    bytes: Vec<u8>,
    last: bool,
) -> Result<bool, ExnThrown> {
    with_client(state, |client| client.body.extend_from_slice(&bytes));
    if last && !state.data().destroyed {
        start(scope, request, state)?;
    }
    Ok(true)
}

/// Cancel the send of the request `request`, destroy the response if it arrived, and emit
/// `error` and `close`. The error is `error`, or, if none is given and neither a response
/// arrived nor the request was aborted, an `ECONNRESET` "socket hang up" error.
pub(super) fn destroy(
    scope: &Scope<'_>,
    request: Object<'_>,
    state: &OutgoingState<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    clear_timer(state);
    let response = with_client(state, |client| {
        client.cancel = None;
        client.response.as_ref().map(|response| response.get(scope))
    })
    .flatten();
    if let Some(response) = response {
        Function::call_by_name(scope, response.handle(), c"destroy", &[error])?;
    }
    let aborted = js::conversion::to_boolean(request.get_property(scope, c"aborted")?);
    let error = if !error.is_null_or_undefined() {
        Some(error)
    } else if response.is_none() && !aborted {
        Some(NodeError::new(ErrorKind::Error, "ECONNRESET", "socket hang up").to_value(scope)?)
    } else {
        None
    };
    if let Some(error) = error {
        emit_later(scope, request, c"error", &[error])?;
    }
    emit_later(scope, request, c"close", &[])
}

/// The free sockets of `agent` for connections named `name`: the array its `freeSockets` holds
/// under the name, created if `create`.
fn free_sockets<'s>(
    scope: &'s Scope<'_>,
    agent: Object<'_>,
    name: &str,
    create: bool,
) -> Result<Option<js::Array<'s>>, ExnThrown> {
    let Ok(pool) = Object::from_value(scope, *agent.get_property(scope, c"freeSockets")?) else {
        return Ok(None);
    };
    let key = name.to_jsval_throwing(scope)?;
    let sockets = crate::http::get_by_key(scope, pool, key)?;
    if let Some(sockets) = super::outgoing::as_array(scope, sockets)? {
        return Ok(Some(sockets));
    }
    if !create {
        return Ok(None);
    }
    let sockets = js::Array::new(scope, 0)?;
    crate::http::set_by_key(scope, pool, key, sockets.to_jsval_throwing(scope)?)?;
    Ok(Some(sockets))
}

/// Whether `agent` keeps connections alive.
fn keeps_alive(scope: &Scope<'_>, agent: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(js::conversion::to_boolean(
        agent.get_property(scope, c"keepAlive")?,
    ))
}

/// Assign `request` a socket for a connection to `host` and `port` in the tick queue, as Node.js's
/// agent does once it has one: `socket` and `connection` become the socket, and `socket` is
/// emitted. A request destroyed before then gets no socket. A `keepAlive` agent reuses a socket
/// a response of the same host and port returned to its `freeSockets`, and sets `reusedSocket`.
/// The socket's remote address is `host` if it is an IP address.
fn assign_socket_later(
    scope: &Scope<'_>,
    request: Object<'_>,
    state: &OutgoingState<'_>,
    agent: Option<Object<'_>>,
    host: &str,
    port: u16,
    encrypted: bool,
) -> Result<(), ExnThrown> {
    let name = format!("{host}:{port}:");
    let mut reused = None;
    if let Some(agent) = agent {
        if keeps_alive(scope, agent)? {
            if let Some(free) = free_sockets(scope, agent, &name, false)? {
                let length = free.length(scope)?;
                if length > 0 {
                    let socket = free.get_element(scope, length - 1)?;
                    free.set_length(scope, length - 1)?;
                    if let Ok(socket) = Object::from_value(scope, socket) {
                        super::host_socket::set_owner(scope, socket, Some(request))?;
                        request.set_property(scope, c"reusedSocket", true)?;
                        reused = Some(socket);
                    }
                }
            }
        }
    }
    with_client(state, |client| client.socket_name = name);
    let socket = match reused {
        Some(socket) => socket,
        None => {
            let remote = host
                .parse::<std::net::IpAddr>()
                .ok()
                .map(|ip| std::net::SocketAddr::new(ip, port));
            let connection = super::host_socket::HostConnection {
                remote,
                local_port: None,
                encrypted,
                owner: Some(request),
            };
            super::host_socket::new_host_socket(scope, connection)?
        }
    };
    js::jobs::queue_native_tick(
        scope,
        assign_socket,
        request,
        &[socket.to_jsval_throwing(scope)?],
    )
}

/// The tick of [`assign_socket_later`], whose payload is the request and whose argument is the
/// socket.
fn assign_socket(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<js::Object>(scope, payload);
    let state = super::outgoing::state_of(scope, request)?;
    if state.data().destroyed {
        return Ok(js::value::undefined());
    }
    let socket = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    state.data_mut().socket = Some(Heap::from(socket));
    crate::events::emit(
        scope,
        request,
        c"socket",
        &[socket.to_jsval_throwing(scope)?],
    )?;
    Ok(js::value::undefined())
}

/// Return the socket of `request` to its agent's `freeSockets`, if the agent keeps connections
/// alive and the socket isn't destroyed.
fn release_socket(scope: &Scope<'_>, request: Object<'_>) -> Result<(), ExnThrown> {
    let state = super::outgoing::state_of(scope, request)?;
    let (agent, name) = with_client(&state, |client| {
        (
            client.agent.as_ref().map(|agent| agent.get(scope)),
            client.socket_name.clone(),
        )
    })
    .unwrap_or_default();
    let Some(agent) = agent else {
        return Ok(());
    };
    let Some(socket) = state.data().socket.get(scope) else {
        return Ok(());
    };
    if !keeps_alive(scope, agent)? || super::host_socket::is_destroyed(socket) {
        return Ok(());
    }
    super::host_socket::set_owner(scope, socket, None)?;
    if let Some(free) = free_sockets(scope, agent, &name, true)? {
        free.push(scope, socket)?;
    }
    Ok(())
}

/// Add the headers `headers` gives to `request`: from an object, a flat array of names and
/// values, or an array of `[name, value]` pairs.
fn add_headers(scope: &Scope<'_>, request: Object<'_>, headers: Object<'_>) -> Result<(), Thrown> {
    let value = headers.to_jsval_throwing(scope)?;
    let is_array = super::outgoing::as_array(scope, value)?.is_some();
    for (name, value) in header_args(scope, value, false)? {
        if is_array {
            outgoing_message_class::append_header(scope, request, name, value)?;
        } else {
            outgoing_message_class::set_header(scope, request, name, value)?;
        }
    }
    Ok(())
}

/// The URL of a request to `host` and `port` with `path`, whose request target always starts
/// with `/`, or an empty string if `host` can't be in a URL.
fn request_url(protocol: &str, host: &str, port: u16, default_port: f64, path: &str) -> String {
    let bracketed = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let Ok(host) = url::Host::parse(&bracketed) else {
        return String::new();
    };
    let slash = if path.starts_with('/') { "" } else { "/" };
    if f64::from(port) == default_port {
        format!("{protocol}//{host}{slash}{path}")
    } else {
        format!("{protocol}//{host}:{port}{slash}{path}")
    }
}

/// The getter of `path`.
fn get_path(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let Some(state) = super::outgoing::try_state_of(scope, this)? else {
        return Ok(js::value::undefined());
    };
    let path = with_client(&state, |client| {
        client.path.as_ref().map(|path| path.get(scope))
    })
    .flatten();
    Ok(path.map_or_else(js::value::undefined, |path| path.get()))
}

/// The setter of `path`.
fn set_path(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    if let Some(state) = super::outgoing::try_state_of(scope, this)? {
        let value = args.get(0);
        with_client(&state, |client| client.path = Some(Heap::from(value.get())));
    }
    Ok(js::value::undefined())
}

/// The own properties the `ClientRequest` constructor creates after those of `OutgoingMessage`, in
/// Node's order.
pub(crate) static REQUEST_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"agent"),
    TemplateKey::Name(c"method"),
    TemplateKey::Name(c"res"),
    TemplateKey::Name(c"aborted"),
    TemplateKey::Name(c"reusedSocket"),
    TemplateKey::Name(c"host"),
    TemplateKey::Name(c"protocol"),
]);

#[core_runtime::jsfunction_class(name = "ClientRequest", extends = "super::outgoing_message_class")]
pub mod client_request_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpclientrequest>: a request to the URL `input`
    /// or with the options `input`, with `options` added, whose response goes to `callback`. Of
    /// the options, `signal` destroys the request when aborted, and the TLS options and
    /// `createConnection` are ignored.
    #[constructor(length = 3)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        input: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::outgoing_message_class::init(scope, this, None)?;
        let state = super::super::outgoing::state_of(scope, this)?;
        state.data_mut().transport = Transport::Client(ClientPart::default());
        let merged = Object::new_plain(scope)?;
        let (options, callback) = match options {
            Some(options) if js::function::is_callable(options) => (None, Some(options)),
            options => (options, callback),
        };
        let input_options = match url_of(scope, input)? {
            Some(href) => Some(url_options(scope, &href)?),
            None => Object::from_value(scope, *input).ok(),
        };
        let options = options.and_then(|options| Object::from_value(scope, *options).ok());
        for source in [input_options, options].into_iter().flatten() {
            merged.assign(scope, source)?;
        }
        let default_agent =
            match Object::from_value(scope, *merged.get_property(scope, c"_defaultAgent")?) {
                Ok(agent) => agent,
                Err(_) => crate::http::global_agent(scope, false)?,
            };
        let agent = merged.get_property(scope, c"agent")?;
        let agent = if agent.is_boolean() && !agent.to_boolean() {
            let ctor = default_agent.get_property(scope, c"constructor")?;
            let ctor =
                Object::from_value(scope, *ctor).map_err(|e| Thrown::from(e.throw(scope)))?;
            Some(Function::construct(scope, ctor, js::function::EmptyArgs)?)
        } else if agent.is_null_or_undefined() {
            (!js::function::is_callable(merged.get_property(scope, c"createConnection")?))
                .then_some(default_agent)
        } else {
            Object::from_value(scope, *agent).ok()
        };
        let default_protocol =
            string_option(scope, default_agent, c"protocol")?.unwrap_or_default();
        let protocol = match string_option(scope, merged, c"protocol")? {
            Some(protocol) if !protocol.is_empty() => protocol,
            _ => default_protocol.clone(),
        };
        let expected = match agent {
            Some(agent) => string_option(scope, agent, c"protocol")?
                .filter(|protocol| !protocol.is_empty())
                .unwrap_or(default_protocol),
            None => default_protocol,
        };
        let path = merged.get_property(scope, c"path")?;
        let path = if js::conversion::to_boolean(path) {
            let path = String::from_jsval(scope, path, ())?;
            if path.chars().any(|c| !('\u{21}'..='\u{ff}').contains(&c)) {
                return Err(NodeError::new(
                    ErrorKind::TypeError,
                    "ERR_UNESCAPED_CHARACTERS",
                    "Request path contains unescaped characters",
                )
                .into());
            }
            path
        } else {
            "/".to_string()
        };
        if protocol != expected {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_PROTOCOL",
                format!("Protocol \"{protocol}\" not supported. Expected \"{expected}\""),
            )
            .into());
        }
        let default_port = match merged.get_property(scope, c"defaultPort")? {
            port if js::conversion::to_boolean(port) => port,
            _ => match agent {
                Some(agent) => agent.get_property(scope, c"defaultPort")?,
                None => HandleValue::undefined(),
            },
        };
        let port = match merged.get_property(scope, c"port")? {
            port if js::conversion::to_boolean(port) => port,
            _ if js::conversion::to_boolean(default_port) => default_port,
            _ => scope.root_value(js::value::from_f64(80.0)),
        };
        f64::from_jsval(scope, port, ())?;
        if !port.is_number() && !port.is_string() {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options.port",
                "one of type number or string",
                port,
            )
            .into());
        }
        let port = crate::net::validate_port(scope, port, "Port")?;
        let default_port = if default_port.is_number() {
            default_port.to_number()
        } else if protocol == "https:" {
            443.0
        } else {
            80.0
        };
        let host = host_option(scope, merged, c"hostname", "options.hostname")?
            .or(host_option(scope, merged, c"host", "options.host")?)
            .unwrap_or_else(|| "localhost".into());
        let method = string_option(scope, merged, c"method")?
            .unwrap_or_else(|| "GET".into())
            .to_ascii_uppercase();
        let url = request_url(&protocol, &host, port, default_port, &path);
        // SAFETY: `Assign` has no requirements.
        let mut properties =
            unsafe { this.property_writer(scope, &REQUEST_PROPERTIES, PropertyInit::Assign) }?;
        match agent {
            Some(agent) => properties.write(scope, c"agent", agent)?,
            None => properties.write(scope, c"agent", HandleValue::undefined())?,
        }
        properties.write(scope, c"method", method.as_str())?;
        properties.write(scope, c"res", HandleValue::null())?;
        properties.write(scope, c"aborted", false)?;
        properties.write(scope, c"reusedSocket", false)?;
        properties.write(scope, c"host", host.as_str())?;
        properties.write(scope, c"protocol", protocol.as_str())?;
        properties.finish(scope)?;
        let path_value = path.as_str().to_jsval_throwing(scope)?;
        with_client(&state, |client| {
            client.url = url;
            client.agent = agent.map(Heap::from);
            client.path = Some(Heap::from(path_value.get()));
        });
        if let Ok(headers) = Object::from_value(scope, *merged.get_property(scope, c"headers")?) {
            super::add_headers(scope, this, headers)?;
        }
        if let Some(auth) = string_option(scope, merged, c"auth")? {
            if super::super::outgoing::header_value(scope, &state, "authorization").is_none() {
                let Some(encoded) = web_globals::base64::encode(auth.as_bytes(), false) else {
                    js::exception::report_out_of_memory(scope);
                    return Err(ExnThrown.into());
                };
                let name = js::class::get_or_init_atom(scope, c"Authorization")?;
                let value = format!("Basic {encoded}")
                    .as_str()
                    .to_jsval_throwing(scope)?;
                super::outgoing_message_class::set_header(scope, this, name, value)?;
            }
        }
        if let Some(callback) =
            callback.and_then(|callback| Object::from_value(scope, *callback).ok())
        {
            if callback.is_callable() {
                crate::events::listen(scope, this, c"once", c"response", callback)?;
            }
        }
        let timeout = merged.get_property(scope, c"timeout")?;
        if timeout.is_number() {
            client_request_class::set_timeout(scope, this, timeout.to_number(), None)?;
        }
        let signal = merged.get_property(scope, c"signal")?;
        if js::conversion::to_boolean(signal) {
            crate::stream::add_abort_signal(scope, signal, this.to_jsval_throwing(scope)?)?;
        }
        super::assign_socket_later(
            scope,
            this,
            &state,
            agent,
            &host,
            port,
            protocol == "https:",
        )?;
        Ok(())
    }

    /// Mark the headers sent, which `headersSent` returns. The runtime's HTTP client writes the
    /// request head.
    #[method(name = "_implicitHeader")]
    pub fn implicit_header_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        super::super::outgoing::state_of(scope, this)?
            .data_mut()
            .header_stored = true;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#requestabort>
    pub fn abort(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if js::conversion::to_boolean(this.get_property(scope, c"aborted")?) {
            return Ok(());
        }
        this.set_property(scope, c"aborted", true)?;
        emit_later(scope, this, c"abort", &[])?;
        super::outgoing_message_class::destroy(scope, this, None)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#requestsettimeouttimeout-callback>: emit `timeout` if
    /// no response arrived within `timeout` milliseconds.
    #[method(length = 2)]
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        timeout: f64,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let state = super::super::outgoing::state_of(scope, this)?;
        clear_timer(&state);
        if let Some(callback) =
            callback.and_then(|callback| Object::from_value(scope, *callback).ok())
        {
            if callback.is_callable() {
                crate::events::listen(scope, this, c"once", c"timeout", callback)?;
            }
        }
        if timeout > 0.0 {
            let timeout = scope.root_value(js::value::from_f64(timeout));
            let timeout = crate::timers::timer_delay(scope, timeout)?;
            let fire = crate::support::shared_function(scope, on_timeout, 0)?;
            let fire = js::Callable::from_jsval(scope, fire.to_jsval_throwing(scope)?, ())
                .map_err(|e| e.throw(scope))?;
            let id = core_runtime::event_loop::timer::start_timer(
                scope,
                fire,
                this.to_jsval_throwing(scope)?,
                &[],
                Duration::from_millis(timeout as u64),
                false,
                None,
            )
            .ok_or_else(|| {
                core_runtime::event_loop::throw_no_event_loop(scope, "request.setTimeout")
            })?;
            with_client(&state, |client| client.timer = Some(id));
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#requestsetnodelaynodelay>: has no effect.
    #[method(length = 1)]
    pub fn set_no_delay() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#requestsetsocketkeepaliveenable-initialdelay>: has no
    /// effect.
    #[method(length = 2)]
    pub fn set_socket_keep_alive() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// Define `path`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"path", 0, get_path, ())?;
        let setter = Function::new_callback(scope, c"path", 1, set_path, ())?;
        proto.define_accessor(
            scope,
            c"path",
            Some(*getter),
            Some(*setter),
            js::class_spec::JSPROP_ENUMERATE as u32,
        )
    }
}

/// Initialize the agent `this` with `options` and the default port and protocol, with Node.js's
/// own data properties.
fn init_agent(
    scope: &Scope<'_>,
    this: Object<'_>,
    options: Option<HandleValue<'_>>,
    default_port: f64,
    protocol: &str,
) -> Result<(), ExnThrown> {
    crate::events::event_emitter::init(scope, this, None).map_err(|thrown| thrown.throw(scope))?;
    let options = match options.and_then(|options| Object::from_value(scope, *options).ok()) {
        Some(options) => options,
        None => Object::new_plain(scope)?,
    };
    this.set_property(scope, c"options", options)?;
    this.set_property(scope, c"defaultPort", default_port)?;
    this.set_property(scope, c"protocol", protocol)?;
    for name in [c"requests", c"sockets", c"freeSockets"] {
        this.set_property(scope, name, Object::new_with_null_prototype(scope)?)?;
    }
    this.set_property(scope, c"keepAliveMsecs", 1000)?;
    let keep_alive = js::conversion::to_boolean(options.get_property(scope, c"keepAlive")?);
    this.set_property(scope, c"keepAlive", keep_alive)?;
    this.set_property(scope, c"maxSockets", f64::INFINITY)?;
    this.set_property(scope, c"maxFreeSockets", 256)?;
    this.set_property(scope, c"scheduling", "lifo")?;
    this.set_property(scope, c"maxTotalSockets", f64::INFINITY)?;
    this.set_property(scope, c"totalSocketCount", 0)?;
    this.set_property(scope, c"agentKeepAliveTimeoutBuffer", 1000)?;
    Ok(())
}

#[core_runtime::jsfunction_class(
    name = "Agent",
    extends = "crate::events::event_emitter",
    self_construct
)]
pub mod agent_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpagent>: the options are kept, and have no
    /// effect besides `keepAlive`, which keeps the sockets of finished requests in
    /// `freeSockets`.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        super::init_agent(scope, this, options, 80.0, "http:")
    }

    /// <https://nodejs.org/api/http.html#agentdestroy>: has no effect.
    pub fn destroy() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#agentgetnameoptions>
    pub fn get_name(
        scope: &Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<String, ExnThrown> {
        let Some(options) = options.and_then(|options| Object::from_value(scope, *options).ok())
        else {
            return Ok("localhost::".into());
        };
        let host = string_option(scope, options, c"host")?.unwrap_or_else(|| "localhost".into());
        let port = string_option(scope, options, c"port")?.unwrap_or_default();
        let address = string_option(scope, options, c"localAddress")?.unwrap_or_default();
        Ok(format!("{host}:{port}:{address}"))
    }
}

#[core_runtime::jsfunction_class(name = "Agent", extends = "super::agent_class", self_construct)]
pub mod https_agent_class {
    use super::*;

    /// <https://nodejs.org/api/https.html#class-httpsagent>: the options are kept, and have no
    /// effect besides `keepAlive`.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        super::init_agent(scope, this, options, 443.0, "https:")
    }
}
