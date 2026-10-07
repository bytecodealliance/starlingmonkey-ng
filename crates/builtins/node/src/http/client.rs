// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `ClientRequest`, ported from Node.js's `lib/_http_client.js`.
//!
//! A request to an `http:` URL in a script the runtime runs as a command writes HTTP/1.1 to a
//! socket: the one its agent's `addRequest` gives it, the one its `createConnection` option
//! creates, or a new `net.Socket` (see [`super::client_socket`]). Every other request, to an
//! `https:` URL, while the runtime serves requests, or in a component, is sent through
//! `platform::http`, which handles connections and TLS. Its body is collected in the request's
//! state until `end()` and then sent with the request, and it gets a host socket (see
//! [`super::host_socket`]). An agent only keeps its options for those, and the sockets of a
//! `keepAlive` agent's finished requests in its `freeSockets`.

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
    header_pairs, outgoing_message_class, ClientPart, OutgoingFields, OutgoingState, Transport,
    OUTGOING_PROPERTIES,
};
use super::status_text;
use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::events::EMITTER_PROPERTIES;
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

/// Whether a request with `protocol` writes HTTP/1.1 to a socket: an `http:` request in a script
/// the runtime runs as a command. Other requests go through `platform::http`.
fn uses_socket(protocol: &str) -> bool {
    protocol == "http:" && core_runtime::event_loop::running_as_command()
}

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

/// The request options of the URL `input`, a string or an object `isURL` accepts, as Node.js's
/// `urlToHttpOptions` creates them, or `None` if `input` is neither. A string that isn't a URL
/// throws `ERR_INVALID_URL`.
pub(super) fn url_input<'s>(
    scope: &'s Scope<'_>,
    input: HandleValue<'_>,
) -> Result<Option<Object<'s>>, Thrown> {
    let url = if input.is_string() {
        let href = String::from_jsval(scope, input, ())?;
        if url::Url::parse(&href).is_err() {
            return Err(
                NodeError::new(ErrorKind::TypeError, "ERR_INVALID_URL", "Invalid URL").into(),
            );
        }
        Function::construct(scope, crate::url::url_constructor(scope)?, &[input])?
    } else if crate::url::is_url(scope, input)? {
        Object::from_value(scope, *input)?
    } else {
        return Ok(None);
    };
    let url = url.to_jsval_throwing(scope)?;
    Ok(Some(crate::url::module::url_to_http_options(scope, url)?))
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

/// The host name `options[name]`, if it is a non-empty string, or `ERR_INVALID_ARG_TYPE` naming
/// the option `options.{label}` if it is neither a string, `null` nor `undefined`, as Node.js's
/// `validateHost` checks it.
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
        return Err(NodeError::invalid_arg_type(
            scope,
            label,
            "of type string or one of undefined or null",
            value,
        )
        .into());
    }
    Ok(Some(String::from_jsval(scope, value, ())?).filter(|host| !host.is_empty()))
}

/// Run `f` on the client part of the state of a request, if it is a client request.
pub(super) fn with_client<R>(
    state: &OutgoingState<'_>,
    f: impl FnOnce(&mut ClientPart) -> R,
) -> Option<R> {
    state.data_mut().client.as_mut().map(f)
}

/// Whether the request of `state` goes through `platform::http`.
pub(super) fn through_host(state: &OutgoingState<'_>) -> bool {
    matches!(state.data().transport, Transport::Host)
}

/// The completion of a request's send through `platform::http`: emit `response` with the
/// response, consuming it if nothing listens, or emit `error` and `close` after a failure, unless
/// the request was destroyed.
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

/// The `end` listener of a response through `platform::http`, whose payload is its request:
/// return the request's socket to its agent, and emit `close` on the request.
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

/// The timer callback of `setTimeout` of a request through `platform::http`, whose `this` is the
/// request: emit `timeout`.
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

/// Send `request` through `platform::http` with the bytes written to it as its body. `destroy`
/// cancels the send. A request whose host name can't be in a URL fails with `ENOTFOUND`.
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
    let method = match String::from_jsval_throwing(scope, method, ()) {
        Ok(method) => method,
        Err(ExnThrown) => {
            js::exception::clear(scope);
            "GET".into()
        }
    };
    let outgoing = Request {
        method,
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

/// Keep `bytes` of the body of the request `request` through `platform::http`, and send the
/// request with the body kept if `last` and it wasn't destroyed.
pub(super) fn send(
    scope: &Scope<'_>,
    request: Object<'_>,
    state: &OutgoingState<'_>,
    bytes: Vec<u8>,
    last: bool,
) -> Result<bool, ExnThrown> {
    let kept = with_client(state, |client| {
        if client.body.is_empty() {
            client.body = bytes;
            return true;
        }
        let reserved = client.body.try_reserve(bytes.len()).is_ok();
        if reserved {
            client.body.extend_from_slice(&bytes);
        }
        reserved
    });
    if kept == Some(false) {
        js::exception::report_out_of_memory(scope);
        return Err(ExnThrown);
    }
    if last && !state.data().destroyed {
        start(scope, request, state)?;
    }
    Ok(true)
}

/// Cancel the send of the request `request` through `platform::http`, destroy the response if it
/// arrived, and emit `error` and `close`. The error is `error`, or, if none is given and neither
/// a response arrived nor the request was aborted, an `ECONNRESET` "socket hang up" error.
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
    if let Some(sockets) = crate::support::as_array(scope, sockets) {
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
    crate::support::truthy(scope, agent, c"keepAlive")
}

/// Assign `request`, which goes through `platform::http`, a socket for a connection to `host` and
/// `port` in the tick queue, as Node.js's agent does once it has one: `socket` and `connection`
/// become the socket, and `socket` is emitted. A request destroyed before then gets no socket. A
/// `keepAlive` agent reuses a socket a response of the same host and port returned to its
/// `freeSockets`, and sets `reusedSocket`. The socket's remote address is `host` if it is an IP
/// address.
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

/// Return the socket of `request`, which goes through `platform::http`, to its agent's
/// `freeSockets`, if the agent keeps connections alive and the socket isn't destroyed.
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

/// The URL of a request through `platform::http` to `host` and `port` with `path`, whose request
/// target always starts with `/`, or an empty string if `host` can't be in a URL.
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

/// `path` converted to a string, or `ERR_UNESCAPED_CHARACTERS` if it has a character outside
/// `\u{21}` to `\u{ff}`, as Node.js's `INVALID_PATH_REGEX` checks it.
fn validate_path(scope: &Scope<'_>, path: HandleValue<'_>) -> Result<String, Thrown> {
    let path = String::from_jsval(scope, path, ())?;
    if path.chars().any(|c| !('\u{21}'..='\u{ff}').contains(&c)) {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_UNESCAPED_CHARACTERS",
            "Request path contains unescaped characters",
        )
        .into());
    }
    Ok(path)
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

/// The setter of `path`: `ERR_UNESCAPED_CHARACTERS` for a path with a character outside
/// `\u{21}` to `\u{ff}`.
fn set_path(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let path = validate_path(scope, args.get(0)).map_err(|thrown| thrown.throw(scope))?;
    let path = path.as_str().to_jsval_throwing(scope)?;
    if let Some(state) = super::outgoing::try_state_of(scope, this)? {
        with_client(&state, |client| client.path = Some(Heap::from(path.get())));
    }
    Ok(js::value::undefined())
}

/// The own properties the `ClientRequest` constructor creates after those of `OutgoingMessage`, in
/// Node's order. `timeout` is left out without the `timeout` option.
pub(crate) static REQUEST_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"agent"),
    TemplateKey::Name(c"socketPath"),
    TemplateKey::Name(c"timeout"),
    TemplateKey::Name(c"method"),
    TemplateKey::Name(c"maxHeaderSize"),
    TemplateKey::Name(c"insecureHTTPParser"),
    TemplateKey::Name(c"httpValidation"),
    TemplateKey::Name(c"joinDuplicateHeaders"),
    TemplateKey::Name(c"_ended"),
    TemplateKey::Name(c"res"),
    TemplateKey::Name(c"aborted"),
    TemplateKey::Name(c"timeoutCb"),
    TemplateKey::Name(c"upgradeOrConnect"),
    TemplateKey::Name(c"parser"),
    TemplateKey::Name(c"maxHeadersCount"),
    TemplateKey::Name(c"reusedSocket"),
    TemplateKey::Name(c"host"),
    TemplateKey::Name(c"protocol"),
]);

/// The arguments of the `ClientRequest` constructor as Node.js normalizes them: the options,
/// a copy without a prototype unless a URL is given with a callback, and the callback.
fn request_options<'s>(
    scope: &'s Scope<'_>,
    input: HandleValue<'s>,
    options: Option<HandleValue<'s>>,
    callback: Option<HandleValue<'s>>,
) -> Result<(Object<'s>, Option<Object<'s>>), Thrown> {
    let (input, options, callback) = match url_input(scope, input)? {
        Some(url) => (Some(url), options, callback),
        None => (None, Some(input), options),
    };
    let callback_of = |value: Option<HandleValue<'s>>| {
        value
            .and_then(|value| Object::from_value(scope, *value).ok())
            .filter(|value| value.is_callable())
    };
    if let Some(callback) = callback_of(options) {
        let options = match input {
            Some(input) => input,
            None => Object::new_with_null_prototype(scope)?,
        };
        return Ok((options, Some(callback)));
    }
    let merged = Object::new_with_null_prototype(scope)?;
    let options = options.and_then(|options| Object::from_value(scope, *options).ok());
    for source in [input, options].into_iter().flatten() {
        merged.assign(scope, source)?;
    }
    Ok((merged, callback_of(callback)))
}

/// The agent of a request with `options`, whose default agent is `default_agent`: a new agent of
/// the default agent's class for `false`, the default agent for `null` or `undefined` unless the
/// options give `createConnection`, and otherwise the agent given, which must have an
/// `addRequest` method.
fn request_agent<'s>(
    scope: &'s Scope<'_>,
    options: Object<'_>,
    default_agent: Object<'s>,
) -> Result<Option<Object<'s>>, Thrown> {
    let agent = options.get_property(scope, c"agent")?;
    if agent.is_boolean() && !agent.to_boolean() {
        let class = default_agent.get_property(scope, c"constructor")?;
        let class = Object::from_value(scope, *class)?;
        return Ok(Some(Function::construct(
            scope,
            class,
            js::function::EmptyArgs,
        )?));
    }
    if agent.is_null_or_undefined() {
        let create = options.get_property(scope, c"createConnection")?;
        return Ok((!js::function::is_callable(create)).then_some(default_agent));
    }
    let agent_object = Object::from_value(scope, *agent).ok();
    let add_request = match agent_object {
        Some(agent) => agent.get_property(scope, c"addRequest")?,
        None => HandleValue::undefined(),
    };
    match agent_object {
        Some(agent) if js::function::is_callable(add_request) => Ok(Some(agent)),
        _ => Err(NodeError::invalid_arg_type(
            scope,
            "options.agent",
            "one of Agent-like Object, undefined, or false",
            agent,
        )
        .into()),
    }
}

/// Initialize `this` as the `ClientRequest` constructor does for its arguments, creating its
/// properties as `init` creates them, and start connecting.
///
/// # Safety
///
/// With [`PropertyInit::Define`], `this` is an instance [`new_request`] created that nothing else
/// references.
unsafe fn init_request<'w>(
    scope: &'w Scope<'_>,
    this: Object<'w>,
    input: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
    init: PropertyInit,
) -> Result<(), Thrown> {
    let (options, callback) = request_options(scope, input, options, callback)?;
    let high_water_mark = options.get_property(scope, c"highWaterMark")?;
    let default_agent =
        match Object::from_value(scope, *options.get_property(scope, c"_defaultAgent")?) {
            Ok(agent) => agent,
            Err(_) => crate::http::current_global_agent(scope, false)?,
        };
    let agent = request_agent(scope, options, default_agent)?;
    let default_protocol = string_option(scope, default_agent, c"protocol")?.unwrap_or_default();
    let protocol = match options.get_property(scope, c"protocol")? {
        protocol if js::conversion::to_boolean(protocol) => {
            String::from_jsval_throwing(scope, protocol, ())?
        }
        _ => default_protocol.clone(),
    };
    let expected = match agent {
        Some(agent) => match agent.get_property(scope, c"protocol")? {
            protocol if js::conversion::to_boolean(protocol) => {
                String::from_jsval_throwing(scope, protocol, ())?
            }
            _ => default_protocol,
        },
        None => default_protocol,
    };
    let given_path = options.get_property(scope, c"path")?;
    if js::conversion::to_boolean(given_path) {
        validate_path(scope, given_path)?;
    }
    if protocol != expected {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_PROTOCOL",
            format!("Protocol \"{protocol}\" not supported. Expected \"{expected}\""),
        )
        .into());
    }
    let over_socket = uses_socket(&protocol);
    let default_port = match options.get_property(scope, c"defaultPort")? {
        port if js::conversion::to_boolean(port) => port,
        _ => match agent {
            Some(agent) => agent.get_property(scope, c"defaultPort")?,
            None => HandleValue::undefined(),
        },
    };
    let port = match options.get_property(scope, c"port")? {
        port if js::conversion::to_boolean(port) => port,
        _ if js::conversion::to_boolean(default_port) => default_port,
        _ => scope.root_value(js::value::from_f64(80.0)),
    };
    let host = match host_option(scope, options, c"hostname", "options.hostname")? {
        Some(host) => host,
        None => host_option(scope, options, c"host", "options.host")?
            .unwrap_or_else(|| "localhost".into()),
    };
    let set_default_headers = options.get_property(scope, c"setDefaultHeaders")?;
    let removed = set_default_headers.is_boolean() && !set_default_headers.to_boolean();
    let set_host = match options.get_property(scope, c"setHost")? {
        set_host if !set_host.is_undefined() => js::conversion::to_boolean(set_host),
        _ => !removed,
    };
    let given_socket_path = options.get_property(scope, c"socketPath")?;
    let timeout = options.get_property(scope, c"timeout")?;
    let timeout = if timeout.is_undefined() {
        None
    } else {
        Some(crate::timers::get_timer_duration(
            scope, timeout, "timeout",
        )?)
    };
    let signal = options.get_property(scope, c"signal")?;
    let method = options.get_property(scope, c"method")?;
    let method = if method.is_null_or_undefined() {
        String::new()
    } else {
        crate::errors::validate_string(scope, method, "options.method")?
    };
    let method = if method.is_empty() {
        "GET".to_string()
    } else {
        if !super::is_token(&method) {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_HTTP_TOKEN",
                format!("Method must be a valid HTTP token [\"{method}\"]"),
            )
            .into());
        }
        method.to_ascii_uppercase()
    };
    let max_header_size = options.get_property(scope, c"maxHeaderSize")?;
    if !max_header_size.is_undefined() {
        crate::errors::validate_integer(
            scope,
            max_header_size,
            "maxHeaderSize",
            0.0,
            9007199254740991.0,
        )?;
    }
    let insecure = options.get_property(scope, c"insecureHTTPParser")?;
    let validation = options.get_property(scope, c"httpValidation")?;
    super::validate_parser_options(scope, insecure, validation)?;
    let join_duplicates = options.get_property(scope, c"joinDuplicateHeaders")?;
    if !join_duplicates.is_undefined() {
        crate::errors::validate_boolean(scope, join_duplicates, "options.joinDuplicateHeaders")?;
    }
    let path = if js::conversion::to_boolean(given_path) {
        validate_path(scope, given_path)?
    } else {
        "/".to_string()
    };
    let chunked_default = !matches!(
        method.as_str(),
        "GET" | "HEAD" | "DELETE" | "OPTIONS" | "TRACE" | "CONNECT"
    );
    // A request without an agent closes its connection, which it records once its header block
    // is stored, as Node.js does.
    let keep_alive = match agent {
        Some(agent) => {
            let keep_alive = keeps_alive(scope, agent)?;
            let max_sockets = agent.get_property(scope, c"maxSockets")?;
            keep_alive || (max_sockets.is_number() && max_sockets.to_number().is_finite())
        }
        None => true,
    };

    let fields = OutgoingFields {
        should_keep_alive: keep_alive,
        use_chunked_encoding_by_default: chunked_default,
        removed,
        last: !keep_alive,
        ..OutgoingFields::default()
    };
    // SAFETY: the caller guarantees the requirements of `Define`. Only the writer's own reads run
    // JavaScript that can reach `this`, until it is flushed.
    let mut properties = unsafe { this.property_writer(scope, &EMITTER_PROPERTIES, init) }?;
    super::outgoing::init_outgoing(scope, this, &mut properties, &fields, init)?;
    properties.continue_with(scope, &REQUEST_PROPERTIES)?;
    match agent {
        Some(agent) => properties.write(scope, c"agent", agent)?,
        None => properties.write(scope, c"agent", HandleValue::undefined())?,
    }
    properties.write(scope, c"socketPath", given_socket_path)?;
    match timeout {
        Some(timeout) => properties.write(scope, c"timeout", timeout)?,
        None => properties.skip(c"timeout"),
    }
    properties.write(scope, c"method", method.as_str())?;
    properties.write(scope, c"maxHeaderSize", max_header_size)?;
    properties.write(scope, c"insecureHTTPParser", insecure)?;
    properties.write(scope, c"httpValidation", validation)?;
    properties.write(scope, c"joinDuplicateHeaders", join_duplicates)?;
    properties.write(scope, c"_ended", false)?;
    properties.write(scope, c"res", HandleValue::null())?;
    properties.write(scope, c"aborted", false)?;
    properties.write(scope, c"timeoutCb", HandleValue::null())?;
    properties.write(scope, c"upgradeOrConnect", false)?;
    properties.write(scope, c"parser", HandleValue::null())?;
    properties.write(scope, c"maxHeadersCount", HandleValue::null())?;
    properties.write(scope, c"reusedSocket", false)?;
    properties.write(scope, c"host", host.as_str())?;
    properties.write(scope, c"protocol", protocol.as_str())?;
    properties.finish(scope)?;

    let state = super::outgoing::state_of(scope, this)?;
    let path_value = path.as_str().to_jsval_throwing(scope)?;
    {
        let mut data = state.data_mut();
        data.high_water_mark = if high_water_mark.is_number() {
            high_water_mark.to_number()
        } else {
            crate::stream::state::default_high_water_mark(false)
        };
        if !over_socket {
            data.transport = Transport::Host;
        }
        data.client = Some(ClientPart {
            agent: agent.map(Heap::from),
            path: Some(Heap::from(path_value.get())),
            ..ClientPart::default()
        });
    }
    if js::conversion::to_boolean(signal) {
        crate::stream::add_abort_signal(scope, signal, this.to_jsval_throwing(scope)?)?;
    }
    if let Some(callback) = callback {
        crate::events::listen(scope, this, c"once", c"response", callback)?;
    }
    // The `Host` header names the port unless it is the default port.
    let mut host_header = host.clone();
    if host_header.matches(':').count() > 1 && !host_header.starts_with('[') {
        host_header = format!("[{host_header}]");
    }
    let port_number = if port.is_number() {
        port.to_number()
    } else {
        f64::from_jsval_throwing(scope, port, ())?
    };
    let default_number = if default_port.is_number() {
        Some(default_port.to_number())
    } else {
        None
    };
    if js::conversion::to_boolean(port) && Some(port_number) != default_number {
        let port_text = String::from_jsval_throwing(scope, port, ())?;
        host_header = format!("{host_header}:{port_text}");
    }
    let headers = options.get_property(scope, c"headers")?;
    let request_line = format!("{method} {path} HTTP/1.1\r\n");
    if crate::support::as_array(scope, headers).is_some() {
        super::outgoing::store_head_headers(scope, this, &state, headers)?;
        if over_socket {
            super::wire::store_header(scope, this, &state, &request_line)?;
        } else {
            state.data_mut().header_stored = true;
        }
    } else {
        if let Ok(headers) = Object::from_value(scope, *headers) {
            for key in headers.own_enumerable_keys(scope)? {
                let value = crate::http::get_by_key(scope, headers, key)?;
                outgoing_message_class::set_header(scope, this, key, value)?;
            }
        }
        if !host.is_empty()
            && super::outgoing::header_value(scope, &state, "host").is_none()
            && set_host
        {
            let name = js::class::get_or_init_atom(scope, c"Host")?;
            let value = host_header.as_str().to_jsval_throwing(scope)?;
            outgoing_message_class::set_header(scope, this, name, value)?;
        }
        if let Some(auth) = string_option(scope, options, c"auth")? {
            if !auth.is_empty()
                && super::outgoing::header_value(scope, &state, "authorization").is_none()
            {
                let Some(encoded) = web_globals::base64::encode(auth.as_bytes(), false) else {
                    js::exception::report_out_of_memory(scope);
                    return Err(ExnThrown.into());
                };
                let name = js::class::get_or_init_atom(scope, c"Authorization")?;
                let value = format!("Basic {encoded}")
                    .as_str()
                    .to_jsval_throwing(scope)?;
                outgoing_message_class::set_header(scope, this, name, value)?;
            }
        }
        if super::outgoing::header_value(scope, &state, "expect")
            .is_some_and(|expect| js::conversion::to_boolean(expect))
        {
            if state.data().header_stored {
                return Err(super::outgoing::headers_sent("render").into());
            }
            if over_socket {
                super::wire::store_header(scope, this, &state, &request_line)?;
            }
        }
    }
    let unique = options.get_property(scope, c"uniqueHeaders")?;
    state.data_mut().unique_headers = super::outgoing::parse_unique_headers(scope, unique)?;

    if !over_socket {
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
        let default_port = match default_number {
            Some(port) => port,
            None if protocol == "https:" => 443.0,
            None => 80.0,
        };
        let url = request_url(&protocol, &host, port, default_port, &path);
        with_client(&state, |client| client.url = url);
        if let Some(timeout) = timeout {
            client_request_class::set_timeout(
                scope,
                this,
                scope.root_value(js::value::from_f64(timeout)),
                None,
            )?;
        }
        assign_socket_later(
            scope,
            this,
            &state,
            agent,
            &host,
            port,
            protocol == "https:",
        )?;
        return Ok(());
    }
    if agent.is_none() {
        this.set_property(scope, c"_last", true)?;
        this.set_property(scope, c"shouldKeepAlive", false)?;
    }
    let connect_options = Object::new_with_null_prototype(scope)?;
    connect_options.assign(scope, options)?;
    connect_options.set_property(scope, c"port", port)?;
    connect_options.set_property(scope, c"host", host.as_str())?;
    if js::conversion::to_boolean(signal) {
        connect_options.delete_property(scope, c"signal")?;
    }
    super::client_socket::connect(scope, this, agent, connect_options)
}

/// A new `ClientRequest` for the arguments `args` of `http.request`, as `new ClientRequest(...)`
/// creates it with the module's own constructor.
pub(super) fn new_request<'s>(
    scope: &'s Scope<'_>,
    args: &[HandleValue<'_>],
) -> Result<Object<'s>, ExnThrown> {
    let request = client_request_class::new_instance(scope)?;
    let arg = |index: usize| args.get(index).copied();
    let input = arg(0).unwrap_or(HandleValue::undefined());
    // SAFETY: `request` was just created, and nothing else references it.
    unsafe { init_request(scope, request, input, arg(1), arg(2), PropertyInit::Define) }
        .map_err(|thrown| thrown.throw(scope))?;
    Ok(request)
}

#[core_runtime::jsfunction_class(
    name = "ClientRequest",
    extends = "super::outgoing_message_class",
    templates = "EMITTER_PROPERTIES, OUTGOING_PROPERTIES, REQUEST_PROPERTIES"
)]
pub mod client_request_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpclientrequest>: a request to the URL `input`
    /// or with the options `input`, with `options` added, whose response goes to `callback`. The
    /// TLS options are ignored, and so is `createConnection` for a request through
    /// `platform::http`.
    #[constructor(length = 3)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        input: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        // SAFETY: `Assign` has no requirements.
        unsafe { init_request(scope, this, input, options, callback, PropertyInit::Assign) }
    }

    /// Store the header block with the request line, as Node.js's `_implicitHeader` does, or
    /// `ERR_HTTP_HEADERS_SENT` if it was stored. A request through `platform::http` only marks
    /// the headers sent, which `headersSent` returns, since the host writes the request head.
    #[method(name = "_implicitHeader")]
    pub fn implicit_header_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let state = super::super::outgoing::state_of(scope, this)?;
        if through_host(&state) {
            state.data_mut().header_stored = true;
            return Ok(());
        }
        if js::conversion::to_boolean(this.get_property(scope, c"_header")?) {
            return Err(super::super::outgoing::headers_sent("render").into());
        }
        let method = this.get_property(scope, c"method")?;
        let method = String::from_jsval_throwing(scope, method, ())?;
        let path = this.get_property(scope, c"path")?;
        let path = String::from_jsval_throwing(scope, path, ())?;
        let line = format!("{method} {path} HTTP/1.1\r\n");
        super::super::wire::store_header(scope, this, &state, &line)
    }

    /// Emit `prefinish`, as Node.js's `_finish` does.
    #[method(name = "_finish")]
    pub fn finish_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        crate::events::emit(scope, this, c"prefinish", &[])?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#requestabort>
    pub fn abort(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if js::conversion::to_boolean(this.get_property(scope, c"aborted")?) {
            return Ok(());
        }
        this.set_property(scope, c"aborted", true)?;
        emit_later(scope, this, c"abort", &[])?;
        Function::call_by_name(scope, this.handle(), c"destroy", js::function::EmptyArgs)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#requestdestroyerror>: mark the request destroyed, dump
    /// its response, and destroy its socket with `error`. A request through `platform::http`
    /// cancels its send instead.
    #[method(length = 1)]
    pub fn destroy<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        error: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let state = super::super::outgoing::state_of(scope, this)?;
        if through_host(&state) {
            return super::outgoing_message_class::destroy(scope, this, error);
        }
        super::super::client_socket::destroy(scope, this, &state, error)?;
        Ok(this)
    }

    /// Assign the request `socket`, or fail it with `error`, in the tick queue, as Node.js's
    /// `onSocket` does. An `error` listener on the socket forwards its errors to the request
    /// until then.
    #[method(length = 2)]
    pub fn on_socket(
        scope: &Scope<'_>,
        this: Object<'_>,
        socket: Option<HandleValue<'_>>,
        error: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let socket = socket.unwrap_or(HandleValue::undefined());
        let error = error.unwrap_or(HandleValue::undefined());
        super::super::client_socket::on_socket(scope, this, socket, error)
    }

    /// Call the socket's method `method` with `args` once the request has a writable socket, as
    /// Node.js's `_deferToConnect` does.
    #[method(name = "_deferToConnect", length = 2)]
    pub fn defer_to_connect(
        scope: &Scope<'_>,
        this: Object<'_>,
        method: HandleValue<'_>,
        args: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let args = args.unwrap_or(HandleValue::undefined());
        super::super::client_socket::defer_to_connect(scope, this, method, args)
    }

    /// <https://nodejs.org/api/http.html#requestsettimeouttimeout-callback>: emit `timeout`
    /// once the socket was idle for `timeout` milliseconds. A request through `platform::http`
    /// emits `timeout` if no response arrived in time.
    #[method(length = 2)]
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        timeout: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = super::super::outgoing::state_of(scope, this)?;
        if !through_host(&state) {
            super::super::client_socket::set_timeout(scope, this, timeout, callback)?;
            return Ok(this);
        }
        let timeout = crate::timers::get_timer_duration(scope, timeout, "msecs")?;
        if let Some(callback) = callback.filter(|callback| js::conversion::to_boolean(*callback)) {
            let timeout = js::class::get_or_init_atom(scope, c"timeout")?;
            Function::call_by_name(scope, this.handle(), c"once", &[timeout, callback])?;
        }
        clear_timer(&state);
        if timeout > 0.0 {
            let fire = crate::support::shared_function(scope, on_timeout, 0)?;
            let fire = js::Callable::from_jsval(scope, fire.to_jsval_throwing(scope)?, ())
                .map_err(|e| e.throw(scope))?;
            let id = core_runtime::event_loop::timer::start_timer(
                scope,
                fire,
                this.to_jsval_throwing(scope)?,
                &[],
                Duration::from_millis(timeout.max(1.0) as u64),
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

    /// <https://nodejs.org/api/http.html#requestsetnodelaynodelay>: calls the socket's
    /// `setNoDelay` once the request has a socket.
    #[method(length = 1)]
    pub fn set_no_delay(
        scope: &Scope<'_>,
        this: Object<'_>,
        no_delay: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let no_delay = no_delay.unwrap_or(HandleValue::undefined());
        let args = js::Array::with_contents(scope, &[no_delay])?;
        let method = js::class::get_or_init_atom(scope, c"setNoDelay")?;
        defer_to_connect(scope, this, method, Some((*args).to_jsval_throwing(scope)?))
    }

    /// <https://nodejs.org/api/http.html#requestsetsocketkeepaliveenable-initialdelay>: calls
    /// the socket's `setKeepAlive` once the request has a socket.
    #[method(length = 2)]
    pub fn set_socket_keep_alive(
        scope: &Scope<'_>,
        this: Object<'_>,
        enable: Option<HandleValue<'_>>,
        initial_delay: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let enable = enable.unwrap_or(HandleValue::undefined());
        let initial_delay = initial_delay.unwrap_or(HandleValue::undefined());
        let args = js::Array::with_contents(scope, &[enable, initial_delay])?;
        let method = js::class::get_or_init_atom(scope, c"setKeepAlive")?;
        defer_to_connect(scope, this, method, Some((*args).to_jsval_throwing(scope)?))
    }

    /// <https://nodejs.org/api/http.html#requestcleartimeout>: `setTimeout(0, callback)`.
    #[method(length = 1)]
    pub fn clear_timeout(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let zero = scope.root_value(js::value::from_f64(0.0));
        Function::call_by_name(
            scope,
            this.handle(),
            c"setTimeout",
            &[zero, callback.unwrap_or(HandleValue::undefined())],
        )?;
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
