// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:http` and `node:https`. See <https://nodejs.org/api/http.html>.
//!
//! Servers serve the runtime's incoming requests, or HTTP/1.1 connections they accept themselves
//! (see [`server`]). Clients write HTTP/1.1 to sockets their agents give them, or send requests
//! through `platform::http`, which handles connections and TLS (see [`client`]).

mod agent;
mod client;
mod client_socket;
mod connection;
mod host_socket;
mod incoming;
mod outgoing;
mod parser;
mod server;
mod wire;

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};

/// The methods Node's HTTP parser supports, in order.
const METHODS: [&str; 35] = [
    "ACL",
    "BIND",
    "CHECKOUT",
    "CONNECT",
    "COPY",
    "DELETE",
    "GET",
    "HEAD",
    "LINK",
    "LOCK",
    "M-SEARCH",
    "MERGE",
    "MKACTIVITY",
    "MKCALENDAR",
    "MKCOL",
    "MOVE",
    "NOTIFY",
    "OPTIONS",
    "PATCH",
    "POST",
    "PROPFIND",
    "PROPPATCH",
    "PURGE",
    "PUT",
    "QUERY",
    "REBIND",
    "REPORT",
    "SEARCH",
    "SOURCE",
    "SUBSCRIBE",
    "TRACE",
    "UNBIND",
    "UNLINK",
    "UNLOCK",
    "UNSUBSCRIBE",
];

/// The reason phrase of each status code in Node's table.
const STATUS_CODES: [(u16, &str); 63] = [
    (100, "Continue"),
    (101, "Switching Protocols"),
    (102, "Processing"),
    (103, "Early Hints"),
    (200, "OK"),
    (201, "Created"),
    (202, "Accepted"),
    (203, "Non-Authoritative Information"),
    (204, "No Content"),
    (205, "Reset Content"),
    (206, "Partial Content"),
    (207, "Multi-Status"),
    (208, "Already Reported"),
    (226, "IM Used"),
    (300, "Multiple Choices"),
    (301, "Moved Permanently"),
    (302, "Found"),
    (303, "See Other"),
    (304, "Not Modified"),
    (305, "Use Proxy"),
    (307, "Temporary Redirect"),
    (308, "Permanent Redirect"),
    (400, "Bad Request"),
    (401, "Unauthorized"),
    (402, "Payment Required"),
    (403, "Forbidden"),
    (404, "Not Found"),
    (405, "Method Not Allowed"),
    (406, "Not Acceptable"),
    (407, "Proxy Authentication Required"),
    (408, "Request Timeout"),
    (409, "Conflict"),
    (410, "Gone"),
    (411, "Length Required"),
    (412, "Precondition Failed"),
    (413, "Payload Too Large"),
    (414, "URI Too Long"),
    (415, "Unsupported Media Type"),
    (416, "Range Not Satisfiable"),
    (417, "Expectation Failed"),
    (418, "I'm a Teapot"),
    (421, "Misdirected Request"),
    (422, "Unprocessable Entity"),
    (423, "Locked"),
    (424, "Failed Dependency"),
    (425, "Too Early"),
    (426, "Upgrade Required"),
    (428, "Precondition Required"),
    (429, "Too Many Requests"),
    (431, "Request Header Fields Too Large"),
    (451, "Unavailable For Legal Reasons"),
    (500, "Internal Server Error"),
    (501, "Not Implemented"),
    (502, "Bad Gateway"),
    (503, "Service Unavailable"),
    (504, "Gateway Timeout"),
    (505, "HTTP Version Not Supported"),
    (506, "Variant Also Negotiates"),
    (507, "Insufficient Storage"),
    (508, "Loop Detected"),
    (509, "Bandwidth Limit Exceeded"),
    (510, "Not Extended"),
    (511, "Network Authentication Required"),
];

/// Whether `name` is a non-empty HTTP token.
pub(crate) fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// Whether `value` contains a character that a header value can't: one outside tab, `0x20` to
/// `0x7e`, and `0x80` to `0xff`.
pub(crate) fn has_invalid_header_char(value: &str) -> bool {
    value.chars().any(|c| {
        let c = c as u32;
        !(c == 0x09 || (0x20..=0x7e).contains(&c) || (0x80..=0xff).contains(&c))
    })
}

/// `number` converted to a 32-bit integer, as `number | 0` does.
fn to_int32(number: f64) -> i32 {
    if !number.is_finite() {
        return 0;
    }
    number.trunc().rem_euclid(4294967296.0) as u32 as i32
}

/// Whether `object[name]` is truthy.
fn flag(
    scope: &Scope<'_>,
    object: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<bool, ExnThrown> {
    Ok(js::conversion::to_boolean(
        object.get_property(scope, name)?,
    ))
}

/// `object[key]`.
pub(crate) fn get_by_key<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    key: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let id = scope.root_id(js::id::value_to_id(scope, key)?);
    object.get_property_by_id(scope, id)
}

/// Set `object[key]` to `value`.
pub(crate) fn set_by_key(
    scope: &Scope<'_>,
    object: Object<'_>,
    key: HandleValue<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let id = scope.root_id(js::id::value_to_id(scope, key)?);
    object.set_property_by_id(scope, id, value)
}

js::instance_local! {
    /// Whether the warning about `--insecure-http-parser` was emitted.
    static WARNED_LENIENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the HTTP parser of a server or request with the options `validation`, its
/// `httpValidation`, and `insecure`, its `insecureHTTPParser`, is lenient, as Node.js's
/// `calculateLenientFlags` computes it. `'strict'` is strict, `'relaxed'` and `'insecure'` are
/// lenient, and otherwise the parser is lenient if `insecure` is truthy, or, if it is
/// `undefined`, with `--insecure-http-parser`, whose first use emits a warning.
pub(crate) fn lenient_parser(
    scope: &Scope<'_>,
    validation: HandleValue<'_>,
    insecure: HandleValue<'_>,
) -> Result<bool, ExnThrown> {
    match crate::support::rust_string(scope, validation).as_deref() {
        Some("strict") => return Ok(false),
        Some("relaxed" | "insecure") => return Ok(true),
        _ => {}
    }
    if !insecure.is_undefined() {
        return Ok(js::conversion::to_boolean(insecure));
    }
    let lenient = crate::runtime_flags::get().insecure_http_parser;
    if lenient && !WARNED_LENIENT.replace(true) {
        crate::process::emit_warning(scope, "Using insecure HTTP parsing", "Warning")?;
    }
    Ok(lenient)
}

/// Check the `insecureHTTPParser` option `insecure` and the `httpValidation` option
/// `validation` of a server or request, as Node.js checks them.
pub(crate) fn validate_parser_options(
    scope: &Scope<'_>,
    insecure: HandleValue<'_>,
    validation: HandleValue<'_>,
) -> Result<(), Thrown> {
    if !insecure.is_undefined() {
        crate::errors::validate_boolean(scope, insecure, "options.insecureHTTPParser")?;
    }
    if validation.is_undefined() {
        return Ok(());
    }
    let text = crate::support::rust_string(scope, validation);
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
    Ok(())
}

/// The reason phrase of `status` in Node's table.
pub(crate) fn status_text(status: u16) -> Option<&'static str> {
    STATUS_CODES
        .iter()
        .find(|(code, _)| *code == status)
        .map(|(_, reason)| *reason)
}

/// The header name `name`, or `ERR_INVALID_HTTP_TOKEN` if it isn't a valid token.
fn validate_name(scope: &Scope<'_>, name: HandleValue<'_>) -> Result<String, Thrown> {
    module::validate_header_name(scope, name, None)?;
    Ok(js::conversion::FromJSVal::from_jsval(scope, name, ())?)
}

/// Check that `value` is a valid value for the header `name`, or each element of it is.
fn validate_value(scope: &Scope<'_>, name: &str, value: HandleValue<'_>) -> Result<(), Thrown> {
    if let Some(items) = crate::support::array_elements(scope, value)? {
        for item in items {
            module::validate_header_value(scope, name.to_string(), item)?;
        }
        return Ok(());
    }
    module::validate_header_value(scope, name.to_string(), value)
}

/// The keys of the shared `http.globalAgent` and `https.globalAgent`.
static HTTP_AGENT_KEY: u8 = 0;
static HTTPS_AGENT_KEY: u8 = 0;

/// `https.globalAgent` if `https`, and `http.globalAgent` otherwise.
pub(crate) fn global_agent<'s>(scope: &'s Scope<'_>, https: bool) -> Result<Object<'s>, ExnThrown> {
    let key = if https {
        &HTTPS_AGENT_KEY
    } else {
        &HTTP_AGENT_KEY
    };
    js::class::get_or_init_shared_object(scope, key as *const u8 as usize, |scope| {
        let class = if https {
            agent::https_agent_class::class_constructor(scope)?
        } else {
            agent::agent_class::class_constructor(scope)?
        };
        let options = Object::new_plain(scope)?;
        options.set_property(scope, c"keepAlive", true)?;
        options.set_property(scope, c"scheduling", "lifo")?;
        options.set_property(scope, c"timeout", 5000)?;
        options.set_property(scope, c"proxyEnv", HandleValue::undefined())?;
        Function::construct(scope, class, &[options.to_jsval_throwing(scope)?])
    })
}

/// The current `globalAgent` of `node:https` if `https`, and of `node:http` otherwise: the
/// agent the module's exports object holds, which code can replace, or the agent the module was
/// created with if it holds none.
pub(crate) fn current_global_agent<'s>(
    scope: &'s Scope<'_>,
    https: bool,
) -> Result<Object<'s>, ExnThrown> {
    let name = if https { "node:https" } else { "node:http" };
    if let Some(namespace) = core_runtime::module::builtin_namespace(scope, name)? {
        if let Ok(exports) = Object::from_value(scope, *namespace.get_property(scope, c"default")?)
        {
            let agent = exports.get_property(scope, c"globalAgent")?;
            if let Ok(agent) = Object::from_value(scope, *agent) {
                return Ok(agent);
            }
        }
    }
    global_agent(scope, https)
}

/// Create a client request with the arguments `args`, as `http.request` does. For `https`, the
/// options are made as Node.js's `https.request` makes them: from the URL that comes first, with
/// the options that follow assigned, and `https.globalAgent` as the default agent.
fn request<'s>(
    scope: &'s Scope<'_>,
    args: &[HandleValue<'_>],
    https: bool,
) -> Result<Object<'s>, ExnThrown> {
    if !https {
        return client::new_request(scope, args);
    }
    let mut args: Vec<HandleValue<'_>> =
        args.iter().map(|arg| scope.root_value(arg.get())).collect();
    let first = args.first().copied().unwrap_or(HandleValue::undefined());
    let options = match client::url_input(scope, first).map_err(|thrown| thrown.throw(scope))? {
        Some(options) => {
            args.remove(0);
            options
        }
        None => Object::new_plain(scope)?,
    };
    if let Some(first) = args.first().copied() {
        if js::conversion::to_boolean(first) && !js::function::is_callable(first) {
            if let Ok(given) = Object::from_value(scope, *first) {
                options.assign(scope, given)?;
            }
            args.remove(0);
        }
    }
    options.set_property(scope, c"_defaultAgent", current_global_agent(scope, true)?)?;
    args.insert(0, options.to_jsval_throwing(scope)?);
    client::new_request(scope, &args)
}

/// Create a client request with `args` and end it, as `http.get` does.
fn get<'s>(
    scope: &'s Scope<'_>,
    args: &[HandleValue<'_>],
    https: bool,
) -> Result<Object<'s>, ExnThrown> {
    let request = request(scope, args, https)?;
    Function::call_by_name(scope, request.handle(), c"end", js::function::EmptyArgs)?;
    Ok(request)
}

/// Create a server, as `http.createServer` does.
fn create_server<'s>(
    scope: &'s Scope<'_>,
    args: &[HandleValue<'_>],
) -> Result<Object<'s>, ExnThrown> {
    let class = server::server_class::class_constructor(scope)?;
    let args: Vec<HandleValue<'_>> = args.iter().map(|arg| scope.root_value(arg.get())).collect();
    Function::construct(scope, class, &args)
}

#[core_runtime::jsmodule(node_args, name = "node:http", default_namespace)]
pub mod module {
    use super::*;

    pub use super::agent::agent_class as Agent;
    pub use super::client::client_request_class as ClientRequest;
    pub use super::incoming::incoming_message_class as IncomingMessage;
    pub use super::outgoing::outgoing_message_class as OutgoingMessage;
    pub use super::outgoing::server_response_class as ServerResponse;
    pub use super::server::server_class as Server;

    /// <https://nodejs.org/api/http.html#httpcreateserveroptions-requestlistener>
    #[function(length = 2)]
    pub fn create_server<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::create_server(scope, &args)
    }

    /// <https://nodejs.org/api/http.html#httprequesturl-options-callback>
    #[function(length = 3)]
    pub fn request<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::request(scope, &args, false)
    }

    /// <https://nodejs.org/api/http.html#httpgeturl-options-callback>
    #[function(length = 3)]
    pub fn get<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::get(scope, &args, false)
    }

    /// <https://nodejs.org/api/http.html#httpvalidateheadernamename-label>
    #[function(length = 0)]
    pub fn validate_header_name(
        scope: &Scope<'_>,
        name: HandleValue<'_>,
        label: Option<String>,
    ) -> Result<(), Thrown> {
        let text: String = if name.is_symbol() {
            crate::inspect::symbol_to_string(scope, name)
        } else {
            js::conversion::FromJSVal::from_jsval(scope, name, ())?
        };
        if name.is_string() && is_token(&text) {
            return Ok(());
        }
        let label = label
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "Header name".into());
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_HTTP_TOKEN",
            format!("{label} must be a valid HTTP token [\"{text}\"]"),
        )
        .into())
    }

    /// <https://nodejs.org/api/http.html#httpvalidateheadervaluename-value>
    #[function(length = 0)]
    pub fn validate_header_value(
        scope: &Scope<'_>,
        name: String,
        value: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        if value.is_undefined() {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_HTTP_INVALID_HEADER_VALUE",
                format!("Invalid value \"undefined\" for header \"{name}\""),
            )
            .into());
        }
        let text: String = js::conversion::FromJSVal::from_jsval(scope, value, ())?;
        if has_invalid_header_char(&text) {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_CHAR",
                format!("Invalid character in header content [\"{name}\"]"),
            )
            .into());
        }
        Ok(())
    }
}

#[core_runtime::jsmodule(node_args, name = "node:https", default_namespace)]
pub mod https {
    use super::*;

    pub use super::agent::https_agent_class as Agent;
    pub use super::server::server_class as Server;

    /// <https://nodejs.org/api/https.html#httpscreateserveroptions-requestlistener>: the TLS
    /// options are ignored, since the host terminates TLS.
    #[function(length = 2)]
    pub fn create_server<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::create_server(scope, &args)
    }

    /// <https://nodejs.org/api/https.html#httpsrequesturl-options-callback>
    pub fn request<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::request(scope, &args, true)
    }

    /// <https://nodejs.org/api/https.html#httpsgeturl-options-callback>
    #[function(length = 3)]
    pub fn get<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::get(scope, &args, true)
    }
}

/// Register `node:https` with `globalAgent`.
pub(crate) fn register_https(scope: &Scope<'_>, name: &'static str) -> bool {
    let agent = || -> Result<HandleValue<'_>, ExnThrown> {
        global_agent(scope, true)?.to_jsval_throwing(scope)
    };
    let Ok(agent) = agent() else {
        return false;
    };
    crate::stub::register_implemented_with::<https::js_module>(
        scope,
        name,
        &[("globalAgent", agent)],
    )
}

/// Register `node:http` with `METHODS`, `STATUS_CODES` and `globalAgent`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let tables = || -> Result<[(&'static str, HandleValue<'_>); 3], ExnThrown> {
        let methods: Vec<HandleValue<'_>> = METHODS
            .iter()
            .map(|method| js::JSString::from_str(scope, method)?.to_jsval_throwing(scope))
            .collect::<Result<_, _>>()?;
        let methods = js::Array::with_contents(scope, &methods)?;
        let codes = Object::new_plain(scope)?;
        for (code, reason) in STATUS_CODES {
            let key = std::ffi::CString::new(code.to_string()).expect("no NUL");
            codes.set_property_by_name(scope, &key, reason)?;
        }
        let agent = global_agent(scope, false)?;
        Ok([
            ("METHODS", (*methods).to_jsval_throwing(scope)?),
            ("STATUS_CODES", codes.to_jsval_throwing(scope)?),
            ("globalAgent", agent.to_jsval_throwing(scope)?),
        ])
    };
    let Ok(tables) = tables() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &tables)
}

/// Intern the keys of the property templates of the messages.
pub(crate) fn register_templates(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    incoming::MESSAGE_PROPERTIES.register(scope)?;
    outgoing::OUTGOING_PROPERTIES.register(scope)?;
    outgoing::RESPONSE_PROPERTIES.register(scope)?;
    client::REQUEST_PROPERTIES.register(scope)?;
    client_socket::INFORMATION_PROPERTIES.register(scope)
}

/// Register the internal classes of `node:http` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: js::Object<'_>) -> Result<(), ExnThrown> {
    agent::AgentSocket::add_to_global(scope, global);
    agent::SocketRequest::add_to_global(scope, global);
    client_socket::DeferredCall::add_to_global(scope, global);
    client_socket::ParserPool::add_to_global(scope, global);
    connection::ConnectionState::add_to_global(scope, global);
    connection::ParserHandle::add_to_global(scope, global);
    host_socket::register_class(scope, global);
    incoming::IncomingState::add_to_global(scope, global);
    outgoing::OutgoingState::add_to_global(scope, global);
    server::HttpServerState::add_to_global(scope, global);
    outgoing::outgoing_message_class::register(scope)?;
    outgoing::server_response_class::register(scope)?;
    incoming::incoming_message_class::register(scope)?;
    client::client_request_class::register(scope)?;
    agent::agent_class::register(scope)?;
    agent::https_agent_class::register(scope)?;
    server::server_class::register(scope)?;
    Ok(())
}
