// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `dns.Resolver` and `dnsPromises.Resolver`, which query name servers over the runtime's own
//! sockets, and the module-level functions bound to the default resolver. See
//! <https://nodejs.org/api/dns.html#class-dnsresolver>.
//!
//! A resolver's state is a [`ResolverState`]: its servers, its `timeout`, `tries` and
//! `maxTimeout`, its local addresses, and the state it shares with its queries in progress. The
//! servers are the host's (`/etc/resolv.conf` natively, none on wasm), or `127.0.0.1` if there are
//! none, as in c-ares, until `setServers` replaces them.

use std::ffi::CString;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::pin::Pin;
use std::rc::Rc;

use hickory_proto::op::Message;
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyInit, PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};

use super::channel::{self, Config, Shared};
use super::wire::{self, Answer, AnyRecord, QueryKind, Status};
use crate::errors::{validate_function, validate_string, ErrorKind, NodeError, Thrown};

/// The key of the default resolver, a per-global shared object.
static DEFAULT_RESOLVER_KEY: u8 = 0;

/// The properties of a DNS error, as Node.js's `DNSException` creates them.
pub(crate) static ERROR_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"errno"),
    TemplateKey::Name(c"code"),
    TemplateKey::Name(c"syscall"),
    TemplateKey::Name(c"hostname"),
]);

/// The properties of the request object a callback query returns.
pub(crate) static REQUEST_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"bindingName"),
    TemplateKey::Name(c"callback"),
    TemplateKey::Name(c"hostname"),
    TemplateKey::Name(c"oncomplete"),
    TemplateKey::Name(c"ttl"),
]);

/// The properties of an address of `resolve4` and `resolve6` with the `ttl` option.
pub(crate) static TTL_PROPERTIES: PropertyTemplate =
    PropertyTemplate::new(&[TemplateKey::Name(c"address"), TemplateKey::Name(c"ttl")]);

/// The properties of an A or AAAA record of `resolveAny`.
pub(crate) static ANY_ADDRESS_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"address"),
    TemplateKey::Name(c"ttl"),
    TemplateKey::Name(c"type"),
]);

/// The properties of a CNAME, NS or PTR record of `resolveAny`.
pub(crate) static ANY_NAME_PROPERTIES: PropertyTemplate =
    PropertyTemplate::new(&[TemplateKey::Name(c"value"), TemplateKey::Name(c"type")]);

/// The properties of a TXT record of `resolveAny`.
pub(crate) static ANY_TXT_PROPERTIES: PropertyTemplate =
    PropertyTemplate::new(&[TemplateKey::Name(c"entries"), TemplateKey::Name(c"type")]);

pub(crate) static MX_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"exchange"),
    TemplateKey::Name(c"priority"),
    TemplateKey::Name(c"type"),
]);

pub(crate) static SRV_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"name"),
    TemplateKey::Name(c"port"),
    TemplateKey::Name(c"priority"),
    TemplateKey::Name(c"weight"),
    TemplateKey::Name(c"type"),
]);

pub(crate) static NAPTR_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"flags"),
    TemplateKey::Name(c"service"),
    TemplateKey::Name(c"regexp"),
    TemplateKey::Name(c"replacement"),
    TemplateKey::Name(c"order"),
    TemplateKey::Name(c"preference"),
    TemplateKey::Name(c"type"),
]);

pub(crate) static SOA_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"nsname"),
    TemplateKey::Name(c"hostmaster"),
    TemplateKey::Name(c"serial"),
    TemplateKey::Name(c"refresh"),
    TemplateKey::Name(c"retry"),
    TemplateKey::Name(c"expire"),
    TemplateKey::Name(c"minttl"),
    TemplateKey::Name(c"type"),
]);

/// The properties of a CAA record, which also gets a property named by the record's tag.
pub(crate) static CAA_PROPERTIES: PropertyTemplate =
    PropertyTemplate::new(&[TemplateKey::Name(c"critical"), TemplateKey::Name(c"type")]);

pub(crate) static TLSA_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"certUsage"),
    TemplateKey::Name(c"selector"),
    TemplateKey::Name(c"match"),
    TemplateKey::Name(c"data"),
]);

/// Intern the keys of the property templates of `node:dns`.
pub(crate) fn register_templates(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    for template in [
        &ERROR_PROPERTIES,
        &REQUEST_PROPERTIES,
        &TTL_PROPERTIES,
        &ANY_ADDRESS_PROPERTIES,
        &ANY_NAME_PROPERTIES,
        &ANY_TXT_PROPERTIES,
        &MX_PROPERTIES,
        &SRV_PROPERTIES,
        &NAPTR_PROPERTIES,
        &SOA_PROPERTIES,
        &CAA_PROPERTIES,
        &TLSA_PROPERTIES,
    ] {
        template.register(scope)?;
    }
    Ok(())
}

/// The state of a resolver.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ResolverState {
    #[no_trace]
    config: Config,
    #[no_trace]
    shared: Rc<Shared>,
    /// Whether the servers are the host's, which `setServers` replaces.
    #[no_trace]
    default_servers: bool,
}

#[core_runtime::jsmethods]
impl ResolverState {}

/// The payload of a module-level function bound to a resolver.
#[core_runtime::jsclass(hidden)]
pub(crate) struct BoundMethod {
    resolver: Heap<js::object::Object>,
    #[no_trace]
    method: Method,
}

#[core_runtime::jsmethods]
impl BoundMethod {}

/// A method of a resolver that the module-level functions are bound to, of the callback resolver
/// or of the promise resolver.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Method {
    #[default]
    GetServers,
    Resolve {
        promise: bool,
    },
    Query {
        kind: QueryKind,
        promise: bool,
    },
}

/// The module-level functions of `node:dns` and `node:dns/promises` that are bound to the
/// default resolver, in the order Node.js binds them.
const BOUND_KEYS: [&str; 16] = [
    "getServers",
    "resolve",
    "resolve4",
    "resolve6",
    "resolveAny",
    "resolveCaa",
    "resolveCname",
    "resolveMx",
    "resolveNaptr",
    "resolveNs",
    "resolvePtr",
    "resolveSoa",
    "resolveSrv",
    "resolveTlsa",
    "resolveTxt",
    "reverse",
];

impl Method {
    /// The method of the module-level function `key`, of the promise resolver if `promise`.
    fn of_key(key: &str, promise: bool) -> Method {
        match key {
            "getServers" => Method::GetServers,
            "resolve" => Method::Resolve { promise },
            key => Method::Query {
                kind: QueryKind::ALL
                    .into_iter()
                    .find(|kind| kind.method_name() == key)
                    .unwrap_or(QueryKind::A),
                promise,
            },
        }
    }

    /// The function's `name` and `length`.
    fn name_and_length(self) -> (&'static str, u32) {
        match self {
            Method::GetServers => ("getServers", 0),
            Method::Resolve { promise } => ("resolve", if promise { 2 } else { 3 }),
            Method::Query { kind, .. } => (kind.binding_name(), 2),
        }
    }
}

/// The value `undefined`, rooted.
fn undefined<'s>(scope: &'s Scope<'_>) -> HandleValue<'s> {
    scope.root_value(js::value::undefined())
}

/// Make `value` the pending exception.
fn throw_value(scope: &Scope<'_>, value: HandleValue<'_>) -> ExnThrown {
    js::exception::set_pending(scope, value, js::native::ExceptionStackBehavior::Capture);
    ExnThrown
}

/// The error Node.js's `DNSException` creates for the error code `code` of `syscall` on
/// `hostname`, the UTF-16 code units of the name: the message `syscall code hostname`, and
/// `errno`, which is `undefined` for c-ares codes, `code`, `syscall` and `hostname`, which is left
/// out if empty.
pub(crate) fn dns_error<'s>(
    scope: &'s Scope<'_>,
    code: &str,
    errno: Option<i32>,
    syscall: &str,
    hostname: &[u16],
) -> Result<HandleValue<'s>, ExnThrown> {
    let mut message: Vec<u16> = format!("{syscall} {code}").encode_utf16().collect();
    if !hostname.is_empty() {
        message.push(u16::from(b' '));
        message.extend_from_slice(hostname);
    }
    let message = js::JSString::from_utf16(scope, &message)?;
    let error = js::error::new_error_with_message(scope, js::error::JSExnType::JSEXN_ERR, message)?;
    let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
    // SAFETY: `object` is a new error object that no JavaScript has seen.
    let mut writer =
        unsafe { object.property_writer(scope, &ERROR_PROPERTIES, PropertyInit::Define) }?;
    match errno {
        Some(errno) => writer.write(scope, c"errno", f64::from(errno))?,
        None => writer.write(scope, c"errno", undefined(scope))?,
    }
    writer.write(scope, c"code", code)?;
    writer.write(scope, c"syscall", syscall)?;
    match hostname {
        [] => writer.skip(c"hostname"),
        hostname => writer.write(
            scope,
            c"hostname",
            js::JSString::from_utf16(scope, hostname)?,
        )?,
    }
    writer.finish(scope)?;
    Ok(error)
}

/// `address` as libuv's `uv_inet_pton` reads it: a dotted-decimal IPv4 address, or an IPv6
/// address with an optional zone after `%`, which is ignored.
fn parse_ip(address: &str) -> Option<IpAddr> {
    if let Ok(ip) = address.parse::<Ipv4Addr>() {
        return Some(ip.into());
    }
    let address = address.split('%').next().unwrap_or_default();
    address.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
}

/// `ip` as libuv's `uv_inet_ntop` formats it.
fn ip_text(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => crate::net::blocklist::format_ipv6(ip),
    }
}

/// `servers` without the repeated addresses and ports, as c-ares keeps them.
fn deduplicated(servers: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut unique: Vec<SocketAddr> = Vec::new();
    for server in servers {
        if !unique
            .iter()
            .any(|other| other.ip() == server.ip() && other.port() == server.port())
        {
            unique.push(server);
        }
    }
    unique
}

/// The host's name servers, or `127.0.0.1` if it has none, as c-ares falls back to it.
fn system_servers() -> Vec<SocketAddr> {
    let servers = deduplicated(platform::net::name_servers());
    if servers.is_empty() {
        return vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 53))];
    }
    servers
}

/// The option `name` of `options`, as Node.js reads it from `{ ...options }`: an own property of
/// an object, and `undefined` otherwise.
fn own_option<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'_>>,
    name: &'static std::ffi::CStr,
) -> Result<HandleValue<'s>, ExnThrown> {
    let Some(object) = options.and_then(|options| Object::from_value(scope, *options).ok()) else {
        return Ok(undefined(scope));
    };
    if !object.has_own_property(scope, name)? {
        return Ok(undefined(scope));
    }
    object.get_property(scope, name)
}

/// The integer option `name` of `options` from `min` to `max`, or `default` if it is
/// `undefined`, as Node.js's `validateInt32` and `validateUint32` check it.
fn integer_option(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
    name: &'static std::ffi::CStr,
    label: &str,
    range: (f64, f64),
    default: f64,
) -> Result<f64, Thrown> {
    let value = own_option(scope, options, name)?;
    if value.is_undefined() {
        return Ok(default);
    }
    crate::errors::validate_integer(scope, value, label, range.0, range.1)
}

/// Initialize the resolver `this` with `options`, as Node.js's `ResolverBase` constructor does.
fn init_resolver(
    scope: &Scope<'_>,
    this: Object<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let int32 = (-2147483648.0, 2147483647.0);
    let timeout = integer_option(
        scope,
        options,
        c"timeout",
        "options.timeout",
        (-1.0, int32.1),
        -1.0,
    )?;
    let tries = integer_option(
        scope,
        options,
        c"tries",
        "options.tries",
        (1.0, int32.1),
        4.0,
    )?;
    let max_timeout = integer_option(
        scope,
        options,
        c"maxTimeout",
        "options.maxTimeout",
        (0.0, 4294967295.0),
        0.0,
    )?;
    let state = resolver_base_class::state_of(scope, this)?;
    {
        let mut data = state.data_mut();
        data.config = Config {
            servers: system_servers(),
            timeout: timeout as i32,
            tries: tries as u32,
            max_timeout: max_timeout as u32,
            local_v4: None,
            local_v6: None,
        };
        data.shared = Rc::default();
        data.default_servers = true;
    }
    let handle = Object::new_plain(scope)?;
    this.set_property(scope, c"_handle", handle)?;
    Ok(())
}

/// The servers of the resolver `this`, as `getServers` formats them.
fn get_servers<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<js::Array<'s>, ExnThrown> {
    let servers = resolver_base_class::state_of(scope, this)?
        .data()
        .config
        .servers
        .clone();
    let array = js::Array::new(scope, 0)?;
    for server in servers {
        let ip = ip_text(server.ip());
        let text = match (server.port(), server.ip()) {
            (53, _) => ip,
            (port, IpAddr::V6(_)) => format!("[{ip}]:{port}"),
            (port, IpAddr::V4(_)) => format!("{ip}:{port}"),
        };
        array.push(scope, text.as_str())?;
    }
    Ok(array)
}

/// The host and port of `server` as Node.js's `addrSplitRE`, `/(^.+?)(?::(\d+))?$/`, splits it,
/// or `None` if it doesn't match.
fn split_port(server: &str) -> Option<(&str, Option<&str>)> {
    let terminators = ['\n', '\r', '\u{2028}', '\u{2029}'];
    if server.is_empty() || server.contains(terminators) {
        return None;
    }
    for (index, _) in server.char_indices().skip(1) {
        let rest = &server[index..];
        if let Some(digits) = rest.strip_prefix(':') {
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Some((&server[..index], Some(digits)));
            }
        }
    }
    Some((server, None))
}

/// The port that the digits `digits` give, as `NumberParseInt(digits) || 53`, checked by Node.js's
/// `validatePort`.
fn server_port(scope: &Scope<'_>, digits: Option<&str>) -> Result<u16, Thrown> {
    let number = digits
        .map(|digits| digits.parse::<f64>().unwrap_or(f64::NAN))
        .filter(|number| *number != 0.0 && !number.is_nan())
        .unwrap_or(53.0);
    let value = number.to_jsval_throwing(scope)?;
    crate::net::validate_port(scope, value, "Port")
}

/// The address and port that the server string `server` names, as Node.js's `setServers` parses
/// it: an IP address, an IPv6 address in brackets with an optional port, or an IP address with a
/// port. `ERR_INVALID_IP_ADDRESS` for other strings.
fn parse_server(scope: &Scope<'_>, server: &str) -> Result<SocketAddr, Thrown> {
    let socket_addr = |text: &str, port: u16| -> Option<SocketAddr> {
        if !crate::net::is_ipv4(text) && !crate::net::is_ipv6(text) {
            return None;
        }
        let (address, zone) = match text.split_once('%') {
            Some((address, zone)) => (address, Some(zone)),
            None => (text, None),
        };
        match address.parse::<IpAddr>().ok()? {
            IpAddr::V4(ip) => Some(SocketAddr::new(ip.into(), port)),
            IpAddr::V6(ip) => {
                let scope_id = zone
                    .and_then(|zone| {
                        zone.parse()
                            .ok()
                            .or_else(|| platform::net::interface_index(zone))
                    })
                    .unwrap_or(0);
                Some(SocketAddrV6::new(ip, port, 0, scope_id).into())
            }
        }
    };
    if let Some(addr) = socket_addr(server, 53) {
        return Ok(addr);
    }
    if let Some(rest) = server.strip_prefix('[') {
        let inner = rest.split([']', '[']).next().unwrap_or_default();
        let closed = rest[inner.len()..].starts_with(']');
        if closed && (crate::net::is_ipv4(inner) || crate::net::is_ipv6(inner)) {
            let digits = split_port(server).and_then(|(_, digits)| digits);
            let port = server_port(scope, digits)?;
            if let Some(addr) = socket_addr(inner, port) {
                return Ok(addr);
            }
        }
    }
    if let Some((host, digits)) = split_port(server) {
        if crate::net::is_ipv4(host) || crate::net::is_ipv6(host) {
            let port = server_port(scope, digits)?;
            if let Some(addr) = socket_addr(host, port) {
                return Ok(addr);
            }
        }
    }
    Err(NodeError::new(
        ErrorKind::TypeError,
        "ERR_INVALID_IP_ADDRESS",
        format!("Invalid IP address: {server}"),
    )
    .into())
}

/// Replace the servers of the resolver `this` with `servers`, an array of server strings, as
/// Node.js's `setServers` does. Throws `ERR_DNS_SET_SERVERS_FAILED` while queries are in
/// progress.
fn set_servers(
    scope: &Scope<'_>,
    this: Object<'_>,
    servers: HandleValue<'_>,
) -> Result<(), Thrown> {
    let array = match Object::from_value(scope, *servers) {
        Ok(array) if js::Array::is_array(scope, array.handle())? => array,
        _ => {
            return Err(NodeError::invalid_arg_type(
                scope,
                "servers",
                "an instance of Array",
                servers,
            )
            .into())
        }
    };
    let length = array.get_property(scope, c"length")?;
    let length = f64::from_jsval(scope, length, ())? as u32;
    let mut parsed = Vec::new();
    for index in 0..length {
        if !array.has_element(scope, index)? {
            continue;
        }
        let server = array.get_element(scope, index)?;
        let server = validate_string(scope, server, &format!("servers[{index}]"))?;
        parsed.push(parse_server(scope, &server)?);
    }
    let state = resolver_base_class::state_of(scope, this)?;
    if state.data().shared.pending() > 0 {
        let template = scope.root_value(
            js::JSString::from_str(scope, "c-ares failed to set servers: \"%s\" [%s]")?
                .to_jsval_throwing(scope)?
                .get(),
        );
        let reason = "There are pending queries.".to_jsval_throwing(scope)?;
        let message = crate::inspect::format_current(scope, &[template, reason, servers])?;
        return Err(NodeError::new(ErrorKind::Error, "ERR_DNS_SET_SERVERS_FAILED", message).into());
    }
    let mut data = state.data_mut();
    data.config.servers = deduplicated(parsed);
    data.default_servers = false;
    data.shared.reset_servers();
    Ok(())
}

/// The error of `setLocalAddress` with `message`.
fn local_address_error(message: &str) -> NodeError {
    NodeError::new(ErrorKind::TypeError, "ERR_INVALID_ARG_VALUE", message)
}

/// Set the local addresses of the resolver `this`, as Node.js's `setLocalAddress` does: an IPv4
/// and an IPv6 address in either order, and the default for the family that isn't given.
fn set_local_address(
    scope: &Scope<'_>,
    this: Object<'_>,
    ipv4: HandleValue<'_>,
    ipv6: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let first = validate_string(scope, ipv4, "ipv4")?;
    let second = match ipv6.filter(|ipv6| !ipv6.is_undefined()) {
        Some(ipv6) => Some(validate_string(scope, ipv6, "ipv6")?),
        None => None,
    };
    let invalid = || local_address_error("Invalid IP address.");
    let first = parse_ip(&first).ok_or_else(invalid)?;
    let (mut v4, mut v6) = (None, None);
    match first {
        IpAddr::V4(ip) => v4 = Some(ip),
        IpAddr::V6(ip) => v6 = Some(ip),
    }
    if let Some(second) = second {
        match (parse_ip(&second).ok_or_else(invalid)?, first) {
            (IpAddr::V4(_), IpAddr::V4(_)) => {
                return Err(local_address_error("Cannot specify two IPv4 addresses.").into())
            }
            (IpAddr::V6(_), IpAddr::V6(_)) => {
                return Err(local_address_error("Cannot specify two IPv6 addresses.").into())
            }
            (IpAddr::V4(ip), _) => v4 = Some(ip),
            (IpAddr::V6(ip), _) => v6 = Some(ip),
        }
    }
    let state = resolver_base_class::state_of(scope, this)?;
    let mut data = state.data_mut();
    data.config.local_v4 = v4.filter(|ip| !ip.is_unspecified());
    data.config.local_v6 = v6.filter(|ip| !ip.is_unspecified());
    Ok(())
}

/// Read the host's servers again for a resolver whose only server is the loopback fallback and
/// whose last query was refused, as Node.js's `EnsureServers` does. A resolver with other
/// servers stops checking.
fn ensure_servers(state: &ResolverState<'_>) {
    let mut data = state.data_mut();
    if !data.default_servers || data.shared.last_ok() {
        return;
    }
    let loopback =
        |ip: IpAddr| ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(Ipv6Addr::LOCALHOST);
    match data.config.servers.as_slice() {
        [] => {}
        [server] if loopback(server.ip()) => {
            data.config.servers = system_servers();
            data.shared.reset_servers();
        }
        _ => data.default_servers = false,
    }
}

/// The JS value of the address `ip`.
fn address_value<'s>(scope: &'s Scope<'_>, ip: IpAddr) -> Result<HandleValue<'s>, ExnThrown> {
    ip_text(ip).as_str().to_jsval_throwing(scope)
}

/// The JS string of the bytes `text`, each a character.
fn latin1<'s>(scope: &'s Scope<'_>, text: &[u8]) -> Result<HandleValue<'s>, ExnThrown> {
    js::JSString::from_latin1(scope, text)?.to_jsval_throwing(scope)
}

/// A plain object with the properties of `template` and `values`.
fn record<'s>(
    scope: &'s Scope<'_>,
    template: &'static PropertyTemplate,
    values: &[HandleValue<'s>],
) -> Result<HandleValue<'s>, ExnThrown> {
    Object::new_plain_with_template(scope, template, values)?.to_jsval_throwing(scope)
}

/// An array of `strings`.
fn string_array<'s>(
    scope: &'s Scope<'_>,
    strings: &[String],
) -> Result<HandleValue<'s>, ExnThrown> {
    let values = strings
        .iter()
        .map(|text| text.as_str().to_jsval_throwing(scope))
        .collect::<Result<Vec<_>, _>>()?;
    js::Array::with_contents(scope, &values)?.to_jsval_throwing(scope)
}

/// The array of the strings of a TXT record.
fn txt_value<'s>(scope: &'s Scope<'_>, strings: &[Vec<u8>]) -> Result<HandleValue<'s>, ExnThrown> {
    let values = strings
        .iter()
        .map(|text| latin1(scope, text))
        .collect::<Result<Vec<_>, _>>()?;
    js::Array::with_contents(scope, &values)?.to_jsval_throwing(scope)
}

/// An array of `entries`, with holes for `None`.
fn sparse_array<'s, T>(
    scope: &'s Scope<'_>,
    entries: &[Option<T>],
    value: impl Fn(&'s Scope<'_>, &T) -> Result<HandleValue<'s>, ExnThrown>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let array = js::Array::new(scope, entries.len())?;
    for (index, entry) in entries.iter().enumerate() {
        if let Some(entry) = entry {
            array.set_element(scope, index as u32, value(scope, entry)?)?;
        }
    }
    array.to_jsval_throwing(scope)
}

fn mx_value<'s>(scope: &'s Scope<'_>, mx: &wire::Mx) -> Result<HandleValue<'s>, ExnThrown> {
    let values = [
        mx.exchange.as_str().to_jsval_throwing(scope)?,
        f64::from(mx.priority).to_jsval_throwing(scope)?,
        "MX".to_jsval_throwing(scope)?,
    ];
    record(scope, &MX_PROPERTIES, &values)
}

fn srv_value<'s>(scope: &'s Scope<'_>, srv: &wire::Srv) -> Result<HandleValue<'s>, ExnThrown> {
    let values = [
        srv.name.as_str().to_jsval_throwing(scope)?,
        f64::from(srv.port).to_jsval_throwing(scope)?,
        f64::from(srv.priority).to_jsval_throwing(scope)?,
        f64::from(srv.weight).to_jsval_throwing(scope)?,
        "SRV".to_jsval_throwing(scope)?,
    ];
    record(scope, &SRV_PROPERTIES, &values)
}

/// A NAPTR record, whose `type` is `NAPTR` if `typed` and `undefined` otherwise.
fn naptr_value<'s>(
    scope: &'s Scope<'_>,
    naptr: &wire::Naptr,
    typed: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let values = [
        latin1(scope, &naptr.flags)?,
        latin1(scope, &naptr.service)?,
        latin1(scope, &naptr.regexp)?,
        naptr.replacement.as_str().to_jsval_throwing(scope)?,
        f64::from(naptr.order).to_jsval_throwing(scope)?,
        f64::from(naptr.preference).to_jsval_throwing(scope)?,
        match typed {
            true => "NAPTR".to_jsval_throwing(scope)?,
            false => undefined(scope),
        },
    ];
    record(scope, &NAPTR_PROPERTIES, &values)
}

/// An SOA record, whose `type` is `SOA` if `typed` and `undefined` otherwise.
fn soa_value<'s>(
    scope: &'s Scope<'_>,
    soa: &wire::Soa,
    typed: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let values = [
        soa.nsname.as_str().to_jsval_throwing(scope)?,
        soa.hostmaster.as_str().to_jsval_throwing(scope)?,
        f64::from(soa.serial).to_jsval_throwing(scope)?,
        f64::from(soa.refresh).to_jsval_throwing(scope)?,
        f64::from(soa.retry).to_jsval_throwing(scope)?,
        f64::from(soa.expire).to_jsval_throwing(scope)?,
        f64::from(soa.minttl).to_jsval_throwing(scope)?,
        match typed {
            true => "SOA".to_jsval_throwing(scope)?,
            false => undefined(scope),
        },
    ];
    record(scope, &SOA_PROPERTIES, &values)
}

/// A CAA record: `critical`, `type`, and its value under its tag.
fn caa_value<'s>(scope: &'s Scope<'_>, caa: &wire::Caa) -> Result<HandleValue<'s>, ExnThrown> {
    let values = [
        f64::from(caa.critical).to_jsval_throwing(scope)?,
        "CAA".to_jsval_throwing(scope)?,
    ];
    let object = Object::new_plain_with_template(scope, &CAA_PROPERTIES, &values)?;
    if let Ok(tag) = CString::new(caa.tag.clone()) {
        object.set_property_by_name(scope, &tag, latin1(scope, &caa.value)?)?;
    }
    object.to_jsval_throwing(scope)
}

fn tlsa_value<'s>(scope: &'s Scope<'_>, tlsa: &wire::Tlsa) -> Result<HandleValue<'s>, ExnThrown> {
    let values = [
        f64::from(tlsa.cert_usage).to_jsval_throwing(scope)?,
        f64::from(tlsa.selector).to_jsval_throwing(scope)?,
        f64::from(tlsa.matching).to_jsval_throwing(scope)?,
        js::ArrayBuffer::with_data(scope, &tlsa.data)?.to_jsval_throwing(scope)?,
    ];
    record(scope, &TLSA_PROPERTIES, &values)
}

/// A record of a `resolveAny` result, with its `type`.
fn any_value<'s>(scope: &'s Scope<'_>, entry: &AnyRecord) -> Result<HandleValue<'s>, ExnThrown> {
    let address = |scope: &'s Scope<'_>, address: &wire::Address, kind: &str| {
        let values = [
            address_value(scope, address.address)?,
            f64::from(address.ttl).to_jsval_throwing(scope)?,
            kind.to_jsval_throwing(scope)?,
        ];
        record(scope, &ANY_ADDRESS_PROPERTIES, &values)
    };
    let name = |scope: &'s Scope<'_>, value: &str, kind: &str| {
        let values = [
            value.to_jsval_throwing(scope)?,
            kind.to_jsval_throwing(scope)?,
        ];
        record(scope, &ANY_NAME_PROPERTIES, &values)
    };
    match entry {
        AnyRecord::A(a) => address(scope, a, "A"),
        AnyRecord::Aaaa(aaaa) => address(scope, aaaa, "AAAA"),
        AnyRecord::Cname(target) => name(scope, target, "CNAME"),
        AnyRecord::Ns(target) => name(scope, target, "NS"),
        AnyRecord::Ptr(target) => name(scope, target, "PTR"),
        AnyRecord::Mx(mx) => mx_value(scope, mx),
        AnyRecord::Txt(strings) => {
            let values = [txt_value(scope, strings)?, "TXT".to_jsval_throwing(scope)?];
            record(scope, &ANY_TXT_PROPERTIES, &values)
        }
        AnyRecord::Srv(srv) => srv_value(scope, srv),
        AnyRecord::Naptr(naptr) => naptr_value(scope, naptr, true),
        AnyRecord::Soa(soa) => soa_value(scope, soa, true),
        AnyRecord::Tlsa(tlsa) => tlsa_value(scope, tlsa),
        AnyRecord::Caa(caa) => caa_value(scope, caa),
    }
}

/// The JS value of `answer`. Addresses are `{ address, ttl }` objects if `ttl`.
fn answer_value<'s>(
    scope: &'s Scope<'_>,
    answer: &Answer,
    ttl: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let array = |values: Vec<HandleValue<'s>>| -> Result<HandleValue<'s>, ExnThrown> {
        js::Array::with_contents(scope, &values)?.to_jsval_throwing(scope)
    };
    match answer {
        Answer::Addresses(addresses) => {
            let mut values = Vec::new();
            for address in addresses {
                let text = address_value(scope, address.address)?;
                values.push(match ttl {
                    true => {
                        let ttl = f64::from(address.ttl).to_jsval_throwing(scope)?;
                        record(scope, &TTL_PROPERTIES, &[text, ttl])?
                    }
                    false => text,
                });
            }
            array(values)
        }
        Answer::Names(names) => string_array(scope, names),
        Answer::Mx(records) => array(
            records
                .iter()
                .map(|mx| mx_value(scope, mx))
                .collect::<Result<_, _>>()?,
        ),
        Answer::Txt(records) => array(
            records
                .iter()
                .map(|txt| txt_value(scope, txt))
                .collect::<Result<_, _>>()?,
        ),
        Answer::Srv(records) => array(
            records
                .iter()
                .map(|srv| srv_value(scope, srv))
                .collect::<Result<_, _>>()?,
        ),
        Answer::Naptr(records) => array(
            records
                .iter()
                .map(|naptr| naptr_value(scope, naptr, false))
                .collect::<Result<_, _>>()?,
        ),
        Answer::Soa(soa) => soa_value(scope, soa, false),
        Answer::Caa(records) => array(
            records
                .iter()
                .map(|caa| caa_value(scope, caa))
                .collect::<Result<_, _>>()?,
        ),
        Answer::Tlsa(entries) => sparse_array(scope, entries, tlsa_value),
        Answer::Any(entries) => sparse_array(scope, entries, any_value),
    }
}

/// A query in progress, or the error value of one that couldn't start.
enum Started<'s> {
    Pending(js::Promise<'s>),
    Failed(HandleValue<'s>),
}

/// Start a query of `kind` for `hostname` with the resolver `this`, as Node.js's `cares_wrap`
/// queries do, and return the promise for its result. A `reverse` of a string that isn't an IP
/// address fails with `EINVAL` without a query. A name that [`wire::query_name`] rejects with
/// `EBADNAME` fails with `UNKNOWN_ARES_ERROR` instead if the resolver has no servers, as in
/// Node.js. The query's error is the `DNSException` of its status. `reverse`
/// fails with `ENOTFOUND` for every status but `ECANCELLED`, as c-ares's `ares_gethostbyaddr`
/// does.
fn start_query<'s>(
    scope: &'s Scope<'_>,
    this: Object<'_>,
    kind: QueryKind,
    name: js::JSString<'_>,
    ttl: bool,
) -> Result<Started<'s>, ExnThrown> {
    let state = resolver_base_class::state_of(scope, this)?;
    let text = name.to_utf8(scope)?;
    let hostname = name.to_utf16_vec(scope)?;
    let name = match kind {
        QueryKind::Reverse => match parse_ip(&text) {
            Some(ip) => Ok(wire::reverse_name(ip)),
            None => {
                let errno = platform::fs::FsError::from_code("EINVAL").errno;
                let error =
                    dns_error(scope, "EINVAL", Some(errno), kind.binding_name(), &hostname)?;
                return Ok(Started::Failed(error));
            }
        },
        _ => wire::query_name(&text),
    };
    ensure_servers(&state);
    let (config, shared) = {
        let data = state.data();
        (data.config.clone(), Rc::clone(&data.shared))
    };
    let query: Pin<Box<dyn Future<Output = Result<Message, Status>>>> = match name {
        Ok(name) => Box::pin(channel::query(config, shared, name, kind.record_type())),
        Err(Status::BadName) if config.servers.is_empty() => {
            Box::pin(std::future::ready(Err(Status::NoServer)))
        }
        Err(status) => Box::pin(std::future::ready(Err(status))),
    };
    let future = async move {
        let outcome = query.await.and_then(|message| wire::parse(kind, &message));
        let outcome = match (kind, outcome) {
            (QueryKind::Reverse, Err(status)) if status != Status::Cancelled => {
                Err(Status::NotFound)
            }
            (_, outcome) => outcome,
        };
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| match outcome {
            Ok(answer) => answer_value(scope, &answer, ttl),
            Err(status) => {
                let error = dns_error(scope, status.code(), None, kind.binding_name(), &hostname)?;
                Err(throw_value(scope, error))
            }
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    Ok(Started::Pending(promise))
}

/// Whether `options` has a truthy `ttl`, as `!!(options?.ttl)` reads it.
fn ttl_option(scope: &Scope<'_>, options: HandleValue<'_>) -> Result<bool, ExnThrown> {
    if options.is_null() || options.is_undefined() {
        return Ok(false);
    }
    match Object::from_value(scope, *options) {
        Ok(options) => Ok(js::conversion::to_boolean(
            options.get_property(scope, c"ttl")?,
        )),
        Err(_) => Ok(false),
    }
}

/// The `name` argument of a query, which must be a string.
fn name_argument<'s>(
    scope: &'s Scope<'_>,
    name: HandleValue<'s>,
) -> Result<js::JSString<'s>, Thrown> {
    if !name.is_string() {
        return Err(NodeError::invalid_arg_type(scope, "name", "of type string", name).into());
    }
    Ok(js::JSString::from_value(scope, name)?)
}

/// The receiver of a resolver method.
fn receiver<'s>(scope: &'s Scope<'_>, this: HandleValue<'_>) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, *this).map_err(|_| NodeError::invalid_this("Resolver").throw(scope))
}

/// The fulfillment reaction of a callback query, whose payload is the callback.
fn on_query(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    callback: HandleValue,
) -> Result<Value, ExnThrown> {
    crate::support::call_reporting(scope, callback, &[HandleValue::null(), args.get(0)])?;
    Ok(js::value::undefined())
}

/// The `oncomplete` of a query's request object, Node.js's `onresolve(err, result)` with `this`
/// the request: call `this.callback` with `err` if it is truthy, and with `null` and `result`
/// otherwise. The queries don't call it.
fn on_resolve(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = crate::support::this_object(scope, &args)?;
    let (error, result) = (args.get(0), args.get(1));
    if js::conversion::to_boolean(error) {
        Function::call_by_name(scope, request.handle(), c"callback", &[error])?;
    } else {
        Function::call_by_name(
            scope,
            request.handle(),
            c"callback",
            &[HandleValue::null(), result],
        )?;
    }
    Ok(js::value::undefined())
}

/// A query of `kind` of the callback resolver `this` with `args`, `(name[, options], callback)`.
/// Returns the request object.
fn query_with_callback(
    scope: &Scope<'_>,
    this: HandleValue<'_>,
    kind: QueryKind,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let run = || -> Result<Value, Thrown> {
        let arg = |index: usize| args.get(index).copied().unwrap_or_else(|| undefined(scope));
        let (options, callback) = match args.len() > 2 {
            true => (arg(1), arg(2)),
            false => (undefined(scope), arg(1)),
        };
        let name = name_argument(scope, arg(0))?;
        validate_function(scope, callback, "callback")?;
        let ttl = ttl_option(scope, options)?;
        let this = receiver(scope, this)?;
        let request = Object::new_plain_with_template(
            scope,
            &REQUEST_PROPERTIES,
            &[
                kind.binding_name().to_jsval_throwing(scope)?,
                callback,
                arg(0),
                crate::support::shared_named_function(scope, on_resolve, c"onresolve", 3)?
                    .to_jsval_throwing(scope)?,
                ttl.to_jsval_throwing(scope)?,
            ],
        )?;
        match start_query(scope, this, kind, name, ttl)? {
            Started::Pending(promise) => {
                let done = Function::new_callback(scope, c"", 1, on_query, callback)?;
                let failed =
                    Function::new_callback(scope, c"", 1, super::on_lookup_error, callback)?;
                promise.add_reactions(scope, Some(*done), Some(*failed))?;
            }
            Started::Failed(error) => return Err(throw_value(scope, error).into()),
        }
        Ok(request.to_jsval_raw_throwing(scope)?)
    };
    run().map_err(|e| e.throw(scope))
}

/// A query of `kind` of the promise resolver `this` for `name` with `options`.
fn query_promise(
    scope: &Scope<'_>,
    this: HandleValue<'_>,
    kind: QueryKind,
    name: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let name = name_argument(scope, name).map_err(|e| e.throw(scope))?;
    let ttl = ttl_option(scope, options)?;
    let this = receiver(scope, this)?;
    let promise = match start_query(scope, this, kind, name, ttl)? {
        Started::Pending(promise) => promise,
        Started::Failed(error) => js::Promise::new_rejected_with_error(scope, error)?,
    };
    promise.to_jsval_raw_throwing(scope)
}

/// `resolve(hostname, rrtype, callback)` of the callback resolver `this`.
fn resolve_with_callback(
    scope: &Scope<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let arg = |index: usize| args.get(index).copied().unwrap_or_else(|| undefined(scope));
    let rrtype = arg(1);
    let (kind, callback) = if rrtype.is_string() {
        let text = js::JSString::from_value(scope, rrtype)?.to_utf8(scope)?;
        (QueryKind::from_rrtype(&text), arg(2))
    } else if js::function::is_callable(rrtype) {
        (Some(QueryKind::A), rrtype)
    } else {
        return Err(
            NodeError::invalid_arg_type(scope, "rrtype", "of type string", rrtype).throw(scope),
        );
    };
    match kind {
        Some(kind) => query_with_callback(scope, this, kind, &[arg(0), callback]),
        None => {
            Err(NodeError::invalid_arg_value(scope, "rrtype", rrtype, "is invalid").throw(scope))
        }
    }
}

/// `resolve(hostname[, rrtype])` of the promise resolver `this`.
fn resolve_promise(
    scope: &Scope<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let arg = |index: usize| args.get(index).copied().unwrap_or_else(|| undefined(scope));
    let rrtype = arg(1);
    let kind = if rrtype.is_undefined() {
        QueryKind::A
    } else {
        let text = validate_string(scope, rrtype, "rrtype").map_err(|e| e.throw(scope))?;
        QueryKind::from_rrtype(&text).ok_or_else(|| {
            NodeError::invalid_arg_value(scope, "rrtype", rrtype, "is invalid").throw(scope)
        })?
    };
    query_promise(scope, this, kind, arg(0), undefined(scope))
}

/// Call `method` with the receiver `this` and `args`.
fn call_method(
    scope: &Scope<'_>,
    this: HandleValue<'_>,
    method: Method,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let arg = |index: usize| args.get(index).copied().unwrap_or_else(|| undefined(scope));
    match method {
        Method::GetServers => {
            let this = receiver(scope, this)?;
            get_servers(scope, this)?.to_jsval_raw_throwing(scope)
        }
        Method::Resolve { promise: false } => resolve_with_callback(scope, this, args),
        Method::Resolve { promise: true } => resolve_promise(scope, this, args),
        Method::Query {
            kind,
            promise: false,
        } => query_with_callback(scope, this, kind, args),
        Method::Query {
            kind,
            promise: true,
        } => query_promise(scope, this, kind, arg(0), arg(1)),
    }
}

/// A query method of `Resolver.prototype`, whose payload is the index of its kind.
fn prototype_query(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let kind = QueryKind::ALL[payload.to_number() as usize];
    let values = crate::callback_args(&args);
    query_with_callback(scope, scope.root_value(args.this()), kind, &values)
}

/// A query method of `dnsPromises.Resolver.prototype`, whose payload is the index of its kind.
fn prototype_query_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let kind = QueryKind::ALL[payload.to_number() as usize];
    query_promise(
        scope,
        scope.root_value(args.this()),
        kind,
        args.get(0),
        args.get(1),
    )
}

/// `resolve` of `Resolver.prototype`.
fn prototype_resolve(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let values = crate::callback_args(&args);
    resolve_with_callback(scope, scope.root_value(args.this()), &values)
}

/// `resolve` of `dnsPromises.Resolver.prototype`.
fn prototype_resolve_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let values = crate::callback_args(&args);
    resolve_promise(scope, scope.root_value(args.this()), &values)
}

/// Assign the query methods and `resolve` to `proto`, the prototype of a resolver class, as
/// Node.js's `createResolverClass` does, of the promise resolver if `promise`.
fn define_query_methods(
    scope: &Scope<'_>,
    proto: Object<'_>,
    promise: bool,
) -> Result<(), ExnThrown> {
    let body = match promise {
        true => prototype_query_promise as js::function::Callback,
        false => prototype_query,
    };
    for (index, kind) in QueryKind::ALL.into_iter().enumerate() {
        let name = CString::new(kind.binding_name()).expect("names have no NULs");
        let function = Function::new_callback(scope, &name, 2, body, index as f64)?;
        let key = CString::new(kind.method_name()).expect("names have no NULs");
        proto.set_property_by_name(scope, &key, function)?;
    }
    let resolve = match promise {
        true => Function::new_callback(scope, c"resolve", 2, prototype_resolve_promise, ())?,
        false => Function::new_callback(scope, c"resolve", 3, prototype_resolve, ())?,
    };
    proto.set_property(scope, c"resolve", resolve)
}

/// A module-level function bound to a resolver, whose payload is a [`BoundMethod`].
fn bound_call(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let bound = cast_payload::<BoundMethod>(scope, payload);
    let (resolver, method) = {
        let data = bound.data();
        (data.resolver.get(scope), data.method)
    };
    let this = resolver.to_jsval_throwing(scope)?;
    let values = crate::callback_args(&args);
    call_method(scope, this, method, &values)
}

/// The module-level functions bound to `resolver`, as Node.js's `bindDefaultResolver` binds the
/// methods of the callback resolver, or of the promise resolver if `promise`.
pub(crate) fn bound_functions<'s>(
    scope: &'s Scope<'_>,
    resolver: Object<'_>,
    promise: bool,
) -> Result<Vec<(&'static str, HandleValue<'s>)>, ExnThrown> {
    let mut functions = Vec::new();
    for key in BOUND_KEYS {
        let method = Method::of_key(key, promise);
        let (name, length) = method.name_and_length();
        let bound =
            js::class::create_instance_with::<BoundMethodImpl>(scope, |_| BoundMethodImpl {
                resolver: Heap::from(resolver),
                method,
            })?;
        let name = CString::new(format!("bound {name}")).expect("names have no NULs");
        let function = Function::new_callback(scope, &name, length, bound_call, bound)?;
        functions.push((key, function.to_jsval_throwing(scope)?));
    }
    Ok(functions)
}

/// Assign the module-level functions bound to `resolver` to `target`.
fn bind_resolver(
    scope: &Scope<'_>,
    target: Object<'_>,
    resolver: Object<'_>,
    promise: bool,
) -> Result<(), ExnThrown> {
    for (key, function) in bound_functions(scope, resolver, promise)? {
        let key = CString::new(key).expect("names have no NULs");
        target.set_property_by_name(scope, &key, function)?;
    }
    Ok(())
}

/// The default resolver of the current global, a `ResolverBase` created on first use, which the
/// module-level functions of both modules are bound to until `setServers` replaces it.
pub(crate) fn default_resolver<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::get_or_init_shared_object(
        scope,
        &DEFAULT_RESOLVER_KEY as *const u8 as usize,
        |scope| {
            let class = resolver_base_class::class_constructor(scope)?;
            Function::construct(scope, class, js::function::EmptyArgs)
        },
    )
}

/// The `default` export of the builtin module `name`.
fn default_export<'s>(scope: &'s Scope<'_>, name: &str) -> Result<Object<'s>, ExnThrown> {
    let namespace = core_runtime::module::builtin_namespace(scope, name)?.ok_or_else(|| {
        js::error::TypeError(format!("{name} is not a builtin module")).throw(scope)
    })?;
    let exports = namespace.get_property(scope, c"default")?;
    Object::from_value(scope, *exports).map_err(|e| e.throw(scope))
}

/// `dns.setServers(servers)`, or `dnsPromises.setServers(servers)` if `promise`, as Node.js's
/// `defaultResolverSetServers` does it: a new resolver with `servers` becomes the default
/// resolver, which the module's functions are bound to, and `dns.setServers` binds the functions
/// of `node:dns/promises` to it too.
fn set_default_servers(
    scope: &Scope<'_>,
    servers: HandleValue<'_>,
    promise: bool,
) -> Result<(), Thrown> {
    let class = match promise {
        true => promises_resolver_class::class_constructor(scope)?,
        false => resolver_class::class_constructor(scope)?,
    };
    let resolver = Function::construct(scope, class, js::function::EmptyArgs)?;
    set_servers(scope, resolver, servers)?;
    if !promise {
        bind_resolver(scope, default_export(scope, "node:dns")?, resolver, false)?;
    }
    bind_resolver(
        scope,
        default_export(scope, "node:dns/promises")?,
        resolver,
        true,
    )?;
    Ok(())
}

/// The body of `dns.setServers`.
pub(crate) fn set_servers_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    set_default_servers(scope, args.get(0), false).map_err(|e| e.throw(scope))?;
    Ok(js::value::undefined())
}

/// The body of `dnsPromises.setServers`.
pub(crate) fn set_servers_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    set_default_servers(scope, args.get(0), true).map_err(|e| e.throw(scope))?;
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(
    name = "ResolverBase",
    class_syntax,
    state = "ResolverState",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod resolver_base_class {
    use super::*;

    /// <https://nodejs.org/api/dns.html#resolveroptions>: the `timeout`, `tries` and
    /// `maxTimeout` options.
    #[constructor(length = 0)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_resolver(scope, this, options)
    }

    /// <https://nodejs.org/api/dns.html#resolvercancel>: complete the queries in progress with
    /// `ECANCELLED`.
    pub fn cancel(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let shared = Rc::clone(&state_of(scope, this)?.data().shared);
        shared.cancel();
        Ok(())
    }

    /// <https://nodejs.org/api/dns.html#dnsgetservers>
    pub fn get_servers<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<js::Array<'s>, ExnThrown> {
        super::get_servers(scope, this)
    }

    /// <https://nodejs.org/api/dns.html#dnssetserversservers>
    pub fn set_servers(
        scope: &Scope<'_>,
        this: Object<'_>,
        servers: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        super::set_servers(scope, this, servers)
    }

    /// <https://nodejs.org/api/dns.html#resolversetlocaladdressipv4-ipv6>: the addresses UDP
    /// queries are sent from. TCP queries don't use them.
    #[method(length = 2)]
    pub fn set_local_address(
        scope: &Scope<'_>,
        this: Object<'_>,
        ipv4: HandleValue<'_>,
        ipv6: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::set_local_address(scope, this, ipv4, ipv6)
    }
}

#[core_runtime::jsfunction_class(
    name = "Resolver",
    extends = "super::resolver_base_class",
    class_syntax,
    state = "ResolverState",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod resolver_class {
    use super::*;

    /// <https://nodejs.org/api/dns.html#class-dnsresolver>
    #[constructor(length = 0)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_resolver(scope, this, options)
    }

    /// Assign the query methods, as <https://nodejs.org/api/dns.html#dnsresolvehostname-rrtype-callback>
    /// and the following sections describe them.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        super::define_query_methods(scope, proto, false)
    }
}

#[core_runtime::jsfunction_class(
    name = "Resolver",
    extends = "super::resolver_base_class",
    class_syntax,
    state = "ResolverState",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod promises_resolver_class {
    use super::*;

    /// <https://nodejs.org/api/dns.html#class-dnspromisesresolver>
    #[constructor(length = 0)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_resolver(scope, this, options)
    }

    /// Assign the query methods, as <https://nodejs.org/api/dns.html#dnspromisesresolvehostname-rrtype>
    /// and the following sections describe them.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        super::define_query_methods(scope, proto, true)
    }
}

/// Register the resolver classes on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: js::Object<'_>) -> Result<(), ExnThrown> {
    ResolverState::add_to_global(scope, global);
    BoundMethod::add_to_global(scope, global);
    resolver_base_class::register(scope)?;
    resolver_class::register(scope)?;
    promises_resolver_class::register(scope)?;
    Ok(())
}
