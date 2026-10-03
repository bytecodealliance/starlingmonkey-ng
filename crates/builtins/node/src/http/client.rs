// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `ClientRequest`, which sends a request through `platform::http`, and `Agent`.
//!
//! The request body is collected until `end()` and then sent with the request. The host handles
//! connections, so agents only hold their options.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};
use platform::http::{OutgoingBody, Request};

use super::incoming::{incoming_message_class, set_headers, set_response_body};
use super::outgoing::{as_array, elements, header_pairs, object_keys, outgoing_message_class};
use super::server::emit_later;
use super::status_text;
use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::stream::{call_method, closure, is_function, string, truthy, Captures, Props};

/// The headers the client leaves out besides [`super::outgoing::HOST_HEADERS`]: none natively,
/// and on wasm the ones the host refuses.
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

/// `text` decoded as `decodeURIComponent` does, or `None` if it has a malformed escape.
fn decode_uri_component(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut input = text.bytes();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = (input.next()? as char).to_digit(16)?;
        let low = (input.next()? as char).to_digit(16)?;
        bytes.push((high * 16 + low) as u8);
    }
    String::from_utf8(bytes).ok()
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
            decode_uri_component(url.username()),
            decode_uri_component(password),
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
    let href = object.val(scope, c"href")?;
    if href.is_string() && object.val(scope, c"searchParams")?.is_object() {
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
    let value = options.val(scope, name)?;
    if value.is_null() || value.is_undefined() {
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
    let value = options.val(scope, name)?;
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    if !value.is_string() {
        return Err(NodeError::invalid_arg_type(scope, label, "of type string", value).into());
    }
    Ok(Some(String::from_jsval(scope, value, ())?).filter(|host| !host.is_empty()))
}

/// The fulfillment reaction of a request's send, whose captures are `[request]`: emit
/// `response` with the response the promise was fulfilled with, consuming it if nothing listens.
fn on_response(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = Captures::of(scope, payload)?.obj(0)?;
    let Ok(response) = Object::from_value(scope, args.get(0)) else {
        // The send was cancelled.
        return Ok(js::value::undefined());
    };
    clear_timer(scope, request)?;
    if request.flag(scope, c"destroyed")? {
        call_method(scope, response, c"destroy", &[])?;
        return Ok(js::value::undefined());
    }
    request.set_property(scope, c"res", response)?;
    response.set_property(scope, c"req", request)?;
    let emit_close = closure(
        scope,
        0,
        emit_request_close,
        &[request.to_jsval_raw_throwing(scope)?],
    )?;
    call_method(
        scope,
        response,
        c"once",
        &[string(scope, "end")?, emit_close.to_jsval_throwing(scope)?],
    )?;
    let response_value = response.to_jsval_throwing(scope)?;
    let listened = call_method(
        scope,
        request,
        c"emit",
        &[string(scope, "response")?, response_value],
    )?;
    if request.flag(scope, c"aborted")? || !truthy(scope, listened) {
        call_method(scope, response, c"resume", &[])?;
    }
    Ok(js::value::undefined())
}

/// The `end` listener of a response, whose captures are `[request]`: emit `close` on the request.
fn emit_request_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = Captures::of(scope, payload)?.obj(0)?;
    emit_later(scope, request, &[string(scope, "close")?])?;
    Ok(js::value::undefined())
}

/// The rejection reaction of a request's send, whose captures are `[request]`: emit `error` and
/// `close`, unless the request was destroyed.
fn on_send_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = Captures::of(scope, payload)?.obj(0)?;
    clear_timer(scope, request)?;
    if request.flag(scope, c"destroyed")? {
        return Ok(js::value::undefined());
    }
    request.set_property(scope, c"destroyed", true)?;
    call_method(
        scope,
        request,
        c"emit",
        &[string(scope, "error")?, args.get(0)],
    )?;
    call_method(scope, request, c"emit", &[string(scope, "close")?])?;
    Ok(js::value::undefined())
}

/// The timer callback of `setTimeout`, whose captures are `[request]`: emit `timeout`.
fn on_timeout(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = Captures::of(scope, payload)?.obj(0)?;
    request.set_property(scope, c"_starlingTimer", js::value::undefined())?;
    call_method(scope, request, c"emit", &[string(scope, "timeout")?])?;
    Ok(js::value::undefined())
}

/// Clear the timer `setTimeout` started for `request`, if it is pending.
fn clear_timer(scope: &Scope<'_>, request: Object<'_>) -> Result<(), ExnThrown> {
    let timer = request.val(scope, c"_starlingTimer")?;
    if !timer.is_undefined() {
        let clear = scope.global().val(scope, c"clearTimeout")?;
        Function::call(scope, js::value::undefined(), clear, &[timer])?;
        request.set_property(scope, c"_starlingTimer", js::value::undefined())?;
    }
    Ok(())
}

/// The cancellation of a request's send, which `destroy` triggers.
#[core_runtime::jsclass(hidden)]
pub struct SendCancel {
    #[no_trace]
    sender: Option<futures_channel::oneshot::Sender<()>>,
}

#[core_runtime::jsmethods]
impl SendCancel {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// Cancel the send of `request`, if it is in progress.
fn cancel_send(scope: &Scope<'_>, request: Object<'_>) -> Result<(), ExnThrown> {
    let Some(handle) = request.obj(scope, c"_starlingCancel")? else {
        return Ok(());
    };
    if let Ok(handle) = handle.cast::<SendCancel>() {
        if let Some(sender) = handle.data_mut().sender.take() {
            let _ = sender.send(());
        }
    }
    Ok(())
}

/// Attach the reactions of a request's send to `promise`.
fn react(
    scope: &Scope<'_>,
    request: Object<'_>,
    promise: js::Promise<'_>,
) -> Result<(), ExnThrown> {
    let on_response = closure(
        scope,
        1,
        on_response,
        &[request.to_jsval_raw_throwing(scope)?],
    )?;
    let on_error = closure(
        scope,
        1,
        on_send_error,
        &[request.to_jsval_raw_throwing(scope)?],
    )?;
    promise.then(scope, Some(on_response), Some(on_error))?;
    Ok(())
}

/// Send `request` with the chunks written to it as its body. `destroy` cancels the send, which
/// fulfills the promise with `undefined`. A request whose host name can't be in a URL fails
/// with `ENOTFOUND`.
fn start(scope: &Scope<'_>, request: Object<'_>) -> Result<(), ExnThrown> {
    let url = String::from_jsval_throwing(scope, request.val(scope, c"_starlingUrl")?, ())?;
    if url.is_empty() {
        let host = request.val(scope, c"host")?;
        let name = String::from_jsval_throwing(scope, host, ())?;
        crate::net::throw_net_error(
            scope,
            &platform::net::NetError::NoAddress(name.clone()),
            "getaddrinfo",
            Some(&name),
            &[(c"hostname", host)],
        );
        let error = crate::stream::take_exception(scope);
        let promise = js::Promise::new_pending(scope)?;
        promise.reject(scope, error)?;
        return react(scope, request, promise);
    }
    let mut body = Vec::new();
    if let Some(chunks) = request.obj(scope, c"_starlingChunks")? {
        for chunk in elements(scope, chunks)? {
            if let Some(view) = Object::from_value(scope, chunk)
                .ok()
                .and_then(js::ArrayBufferView::from_object)
            {
                view.with_bytes(|bytes| body.extend_from_slice(bytes));
            }
        }
    }
    let headers = header_pairs(scope, request)?
        .into_iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            !super::outgoing::HOST_HEADERS.contains(&name.as_str())
                && !REFUSED_HEADERS.contains(&name.as_str())
        })
        .collect();
    let outgoing = Request {
        method: String::from_jsval(scope, request.val(scope, c"method")?, ())
            .unwrap_or_else(|_| "GET".into()),
        url: url.clone(),
        headers,
        body: OutgoingBody::Bytes(body.into()),
    };
    let (sender, cancelled) = futures_channel::oneshot::channel();
    let cancel = js::class::create_instance_with::<SendCancelImpl>(scope, |_| SendCancelImpl {
        sender: Some(sender),
    })?;
    request.define_property(
        scope,
        c"_starlingCancel",
        cancel.to_jsval_throwing(scope)?,
        0,
    )?;
    let future = async move {
        let send = async { Some(platform::http::send(outgoing).await) };
        let cancel = async {
            let _ = cancelled.await;
            None
        };
        let result = futures_lite::future::or(send, cancel).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let Some(result) = result else {
                return Ok(scope.root_value(js::value::undefined()));
            };
            let response = result.map_err(|error| transport_error(scope, &error.0, Some(&url)))?;
            let class = incoming_message_class::class_constructor(scope)?;
            let message = Function::construct(scope, class, js::function::EmptyArgs)?;
            message.set_property(scope, c"statusCode", f64::from(response.status))?;
            message.set_property(
                scope,
                c"statusMessage",
                status_text(response.status).unwrap_or(""),
            )?;
            set_headers(scope, message, &response.headers)?;
            set_response_body(scope, message, response.body)?;
            message.to_jsval_throwing(scope)
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    react(scope, request, promise)
}

/// Add the headers `headers` gives to `request`: from an object, a flat array of names and
/// values, or an array of `[name, value]` pairs.
fn add_headers(scope: &Scope<'_>, request: Object<'_>, headers: Object<'_>) -> Result<(), Thrown> {
    let Some(array) = as_array(scope, headers.to_jsval_throwing(scope)?)? else {
        for key in object_keys(scope, headers)? {
            let value = crate::http::get_by_key(scope, headers, key)?;
            outgoing_message_class::set_header(scope, request, key, value)?;
        }
        return Ok(());
    };
    let items = elements(scope, array)?;
    if let Some(first) = items.first() {
        if as_array(scope, *first)?.is_some() {
            for item in &items {
                let pair =
                    Object::from_value(scope, **item).map_err(|e| Thrown::from(e.throw(scope)))?;
                let name = pair.get_element(scope, 0)?;
                let value = pair.get_element(scope, 1)?;
                outgoing_message_class::append_header(scope, request, name, value)?;
            }
            return Ok(());
        }
    }
    if items.len() % 2 != 0 {
        let headers = headers.to_jsval_throwing(scope)?;
        return Err(NodeError::invalid_arg_value(scope, "headers", headers, "is invalid").into());
    }
    for pair in items.chunks(2) {
        outgoing_message_class::append_header(scope, request, pair[0], pair[1])?;
    }
    Ok(())
}

/// The URL of a request to `host` and `port` with `path`, whose request target always starts
/// with `/`, or an empty string if `host` can't be in a URL.
fn request_url(protocol: &str, host: &str, port: f64, default_port: f64, path: &str) -> String {
    let bracketed = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let Ok(host) = url::Host::parse(&bracketed) else {
        return String::new();
    };
    let slash = if path.starts_with('/') { "" } else { "/" };
    if port == default_port {
        format!("{protocol}//{host}{slash}{path}")
    } else {
        format!("{protocol}//{host}:{port}{slash}{path}")
    }
}

#[core_runtime::jsfunction_class(name = "ClientRequest", extends = "super::outgoing_message_class")]
pub mod client_request_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpclientrequest>: a request to the URL `input`
    /// or with the options `input`, with `options` added, whose response goes to `callback`. Of
    /// the options, `signal` destroys the request when aborted, and the TLS options and
    /// `createConnection` are ignored.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        input: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::outgoing_message_class::init(scope, this)?;
        let merged = Object::new_plain(scope)?;
        let assign = scope.global().obj(scope, c"Object")?.ok_or(ExnThrown)?;
        let (options, callback) = match options {
            Some(options) if is_function(scope, options) => (None, Some(options)),
            options => (options, callback),
        };
        let (input_options, options) = match url_of(scope, input)? {
            Some(href) => (
                Some(url_options(scope, &href)?.to_jsval_throwing(scope)?),
                options,
            ),
            None if input.is_object() => (Some(scope.root_value(input.get())), options),
            None => (None, options),
        };
        for source in [input_options, options].into_iter().flatten() {
            if source.is_object() {
                call_method(
                    scope,
                    assign,
                    c"assign",
                    &[merged.to_jsval_throwing(scope)?, source],
                )?;
            }
        }
        let default_agent = match merged.obj(scope, c"_defaultAgent")? {
            Some(agent) => agent,
            None => crate::http::global_agent(scope, false)?,
        };
        let agent = merged.val(scope, c"agent")?;
        let agent = if agent.is_boolean() && !agent.to_boolean() {
            let ctor = default_agent.obj(scope, c"constructor")?.ok_or(ExnThrown)?;
            Some(Function::construct(scope, ctor, js::function::EmptyArgs)?)
        } else if agent.is_null() || agent.is_undefined() {
            (!is_function(scope, merged.val(scope, c"createConnection")?)).then_some(default_agent)
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
        let path = merged.val(scope, c"path")?;
        let path = if truthy(scope, path) {
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
        let default_port = match merged.val(scope, c"defaultPort")? {
            port if truthy(scope, port) => port,
            _ => match agent {
                Some(agent) => agent.val(scope, c"defaultPort")?,
                None => scope.root_value(js::value::undefined()),
            },
        };
        let port = match merged.val(scope, c"port")? {
            port if truthy(scope, port) => port,
            _ if truthy(scope, default_port) => default_port,
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
        let port = f64::from(crate::net::validate_port(scope, port, "Port")?);
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
        this.set_property(scope, c"method", method.as_str())?;
        this.set_property(scope, c"path", path.as_str())?;
        this.set_property(scope, c"host", host.as_str())?;
        this.set_property(scope, c"protocol", protocol.as_str())?;
        this.set_property(scope, c"aborted", false)?;
        this.set_property(scope, c"reusedSocket", false)?;
        this.set_property(scope, c"res", js::value::null())?;
        this.define_property(scope, c"_starlingUrl", url.as_str(), 0)?;
        match agent {
            Some(agent) => this.set_property(scope, c"agent", agent)?,
            None => this.set_property(scope, c"agent", js::value::undefined())?,
        }
        if let Some(headers) = merged.obj(scope, c"headers")? {
            super::add_headers(scope, this, headers)?;
        }
        if let Some(auth) = string_option(scope, merged, c"auth")? {
            if super::outgoing_message_class::get_header(
                scope,
                this,
                string(scope, "authorization")?,
            )?
            .is_undefined()
            {
                let encoded = crate::buffer::decode_bytes(
                    scope,
                    auth.as_bytes(),
                    crate::buffer::Encoding::Base64,
                )?
                .to_utf8(scope)?;
                let name = string(scope, "Authorization")?;
                let value = string(scope, &format!("Basic {encoded}"))?;
                super::outgoing_message_class::set_header(scope, this, name, value)?;
            }
        }
        if let Some(callback) = callback.filter(|callback| is_function(scope, *callback)) {
            call_method(
                scope,
                this,
                c"once",
                &[string(scope, "response")?, callback],
            )?;
        }
        let timeout = merged.val(scope, c"timeout")?;
        if timeout.is_number() {
            client_request_class::set_timeout(scope, this, timeout.to_number(), None)?;
        }
        let signal = merged.val(scope, c"signal")?;
        if truthy(scope, signal) {
            crate::stream::add_abort_signal(scope, signal, this.to_jsval_throwing(scope)?)?;
        }
        Ok(())
    }

    /// Collect `chunk`, and send the request with the collected chunks if `last`.
    #[method(name = "_starlingSend")]
    pub fn send_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        last: bool,
    ) -> Result<bool, ExnThrown> {
        let chunks = match this.obj(scope, c"_starlingChunks")? {
            Some(chunks) => chunks,
            None => {
                let chunks = js::Array::new(scope, 0)?;
                this.define_property(scope, c"_starlingChunks", *chunks, 0)?;
                *chunks
            }
        };
        call_method(scope, chunks, c"push", &[chunk])?;
        if last && !this.flag(scope, c"destroyed")? {
            start(scope, this)?;
        }
        Ok(true)
    }

    /// Cancel the send, destroy the response if it arrived, and emit `error` and `close`. The
    /// error is `error`, or, if none is given and neither a response arrived nor the request
    /// was aborted, an `ECONNRESET` "socket hang up" error.
    #[method(name = "_starlingDestroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        clear_timer(scope, this)?;
        cancel_send(scope, this)?;
        let response = this.obj(scope, c"res")?;
        if let Some(response) = response {
            call_method(scope, response, c"destroy", &[error])?;
        }
        let error = if !error.is_undefined() && !error.is_null() {
            Some(scope.root_value(error.get()))
        } else if response.is_none() && !this.flag(scope, c"aborted")? {
            Some(crate::stream::error_value(
                scope,
                NodeError::new(ErrorKind::Error, "ECONNRESET", "socket hang up"),
            )?)
        } else {
            None
        };
        if let Some(error) = error {
            emit_later(scope, this, &[string(scope, "error")?, error])?;
        }
        emit_later(scope, this, &[string(scope, "close")?])
    }

    /// <https://nodejs.org/api/http.html#requestabort>
    pub fn abort(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if truthy(scope, this.val(scope, c"aborted")?) {
            return Ok(());
        }
        this.set_property(scope, c"aborted", true)?;
        emit_later(scope, this, &[string(scope, "abort")?])?;
        super::outgoing_message_class::destroy(scope, this, None)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#requestsettimeouttimeout-callback>: emit `timeout` if
    /// no response arrived within `timeout` milliseconds.
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        timeout: f64,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        clear_timer(scope, this)?;
        if let Some(callback) = callback.filter(|callback| is_function(scope, *callback)) {
            call_method(scope, this, c"once", &[string(scope, "timeout")?, callback])?;
        }
        if timeout > 0.0 {
            let fire = closure(scope, 0, on_timeout, &[this.to_jsval_raw_throwing(scope)?])?;
            let set = scope.global().val(scope, c"setTimeout")?;
            let delay = scope.root_value(js::value::from_f64(timeout));
            let timer = Function::call(
                scope,
                js::value::undefined(),
                set,
                &[fire.to_jsval_throwing(scope)?, delay],
            )?;
            this.define_property(scope, c"_starlingTimer", timer, 0)?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#requestsetnodelaynodelay>: has no effect.
    pub fn set_no_delay() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#requestsetsocketkeepaliveenable-initialdelay>: has no
    /// effect.
    pub fn set_socket_keep_alive() -> Result<(), ExnThrown> {
        Ok(())
    }
}

/// Initialize the agent `this` with `options` and the default port and protocol.
fn init_agent(
    scope: &Scope<'_>,
    this: Object<'_>,
    options: Option<HandleValue<'_>>,
    default_port: f64,
    protocol: &str,
) -> Result<(), ExnThrown> {
    crate::events::event_emitter::init(scope, this, None).map_err(|thrown| thrown.throw(scope))?;
    let options = match options.filter(|options| options.is_object()) {
        Some(options) => scope.root_value(options.get()),
        None => Object::new_plain(scope)?.to_jsval_throwing(scope)?,
    };
    let options_object = Object::from_value(scope, *options).map_err(|e| e.throw(scope))?;
    this.set_property(scope, c"options", options)?;
    this.set_property(scope, c"defaultPort", default_port)?;
    this.set_property(scope, c"protocol", protocol)?;
    this.set_property(
        scope,
        c"keepAlive",
        truthy(scope, options_object.val(scope, c"keepAlive")?),
    )?;
    this.set_property(scope, c"keepAliveMsecs", 1000)?;
    this.set_property(scope, c"maxSockets", f64::INFINITY)?;
    this.set_property(scope, c"maxFreeSockets", 256)?;
    this.set_property(scope, c"maxTotalSockets", f64::INFINITY)?;
    this.set_property(scope, c"totalSocketCount", 0)?;
    for name in [c"sockets", c"freeSockets", c"requests"] {
        this.set_property(scope, name, super::outgoing::null_object(scope)?)?;
    }
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
    /// effect.
    #[constructor]
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
    /// effect.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        super::init_agent(scope, this, options, 443.0, "https:")
    }
}
