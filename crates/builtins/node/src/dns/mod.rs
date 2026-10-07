// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:dns` and `node:dns/promises`: `lookup` and `lookupService` over the host's name lookup in
//! `platform::net`, `setDefaultResultOrder` and `getDefaultResultOrder`, and the resolver of
//! [`resolver`], which queries name servers itself. See <https://nodejs.org/api/dns.html>.

mod channel;
mod resolver;
mod wire;

use std::cell::Cell;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};

use crate::errors::{validate_function, NodeError, Thrown};
use crate::support::Props;

/// The `getaddrinfo` flags `lookup`'s `hints` can have, the values of `dns.ADDRCONFIG`,
/// `dns.ALL` and `dns.V4MAPPED`.
const ADDRCONFIG: u32 = 1024;
const ALL: u32 = 256;
const V4MAPPED: u32 = 2048;

/// The properties of the result of `dnsPromises.lookupService`.
pub(crate) static SERVICE_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"hostname"),
    TemplateKey::Name(c"service"),
]);

/// The properties of an address a lookup gives.
static ENTRY_PROPERTIES: PropertyTemplate =
    PropertyTemplate::new(&[TemplateKey::Name(c"address"), TemplateKey::Name(c"family")]);

/// Intern the keys of the property templates of `node:dns`.
pub(crate) fn register_templates(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    SERVICE_PROPERTIES.register(scope)?;
    ENTRY_PROPERTIES.register(scope)?;
    resolver::register_templates(scope)
}

/// The order of the addresses a lookup gives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    /// The order the host's lookup gives.
    Verbatim,
    Ipv4First,
    Ipv6First,
}

impl Order {
    const NAMES: [(&'static str, Order); 3] = [
        ("verbatim", Order::Verbatim),
        ("ipv4first", Order::Ipv4First),
        ("ipv6first", Order::Ipv6First),
    ];

    fn name(self) -> &'static str {
        Self::NAMES
            .iter()
            .find(|(_, order)| *order == self)
            .map_or("verbatim", |(name, _)| name)
    }
}

js::instance_local! {
    /// The order `setDefaultResultOrder` set, if it was called.
    static DEFAULT_ORDER: Cell<Option<Order>> = const { Cell::new(None) };
}

/// The default lookup order: the one `setDefaultResultOrder` set, or else the one
/// `--dns-result-order` gave, or else `verbatim`.
fn default_order() -> Order {
    DEFAULT_ORDER.with(Cell::get).unwrap_or_else(|| {
        let flags = crate::runtime_flags::get();
        let name = flags.dns_result_order.as_deref();
        Order::NAMES
            .iter()
            .find(|(order_name, _)| Some(*order_name) == name)
            .map_or(Order::Verbatim, |(_, order)| *order)
    })
}

/// The options of a lookup: the address family to keep, 0 for both, its `getaddrinfo` flags,
/// whether to give every address, and their order.
pub(crate) struct LookupOptions {
    pub(crate) family: u8,
    pub(crate) hints: u32,
    all: bool,
    order: Order,
}

/// The order `value` names, or `ERR_INVALID_ARG_VALUE` naming the argument `name`.
fn validate_order(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<Order, Thrown> {
    let text = value
        .is_string()
        .then(|| String::from_jsval(scope, value, ()).ok())
        .flatten();
    Order::NAMES
        .iter()
        .find(|(order, _)| text.as_deref() == Some(*order))
        .map(|(_, order)| *order)
        .ok_or_else(|| {
            NodeError::invalid_arg_value(
                scope,
                name,
                value,
                "must be one of: 'verbatim', 'ipv4first', 'ipv6first'",
            )
            .into()
        })
}

/// The address family `value` is, the number 0, 4 or 6, or `ERR_INVALID_ARG_VALUE` naming the
/// argument `name`.
fn validate_family(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<u8, Thrown> {
    let number = if value.is_number() {
        value.to_number()
    } else {
        f64::NAN
    };
    match number {
        0.0 => Ok(0),
        4.0 => Ok(4),
        6.0 => Ok(6),
        _ => {
            Err(NodeError::invalid_arg_value(scope, name, value, "must be one of: 0, 4, 6").into())
        }
    }
}

/// `options[name]` as a boolean, if it is neither `null` nor `undefined`, or
/// `ERR_INVALID_ARG_TYPE`.
fn boolean_option(
    scope: &Scope<'_>,
    options: Object<'_>,
    name: &'static std::ffi::CStr,
    label: &str,
) -> Result<Option<bool>, Thrown> {
    let value = options.val(scope, name)?;
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    if !value.is_boolean() {
        return Err(NodeError::invalid_arg_type(scope, label, "of type boolean", value).into());
    }
    Ok(Some(value.to_boolean()))
}

/// The lookup options the argument `options` gives, as Node.js validates them: a family number,
/// or an object with `hints`, `family`, `all`, `verbatim` and `order`. The family names `IPv4`
/// and `IPv6` are accepted if `family_names`.
pub(crate) fn lookup_options(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
    family_names: bool,
) -> Result<LookupOptions, Thrown> {
    let mut parsed = LookupOptions {
        family: 0,
        hints: 0,
        all: false,
        order: default_order(),
    };
    let Some(options) = options.filter(|options| !options.is_undefined()) else {
        return Ok(parsed);
    };
    if options.is_number() {
        parsed.family = validate_family(scope, options, "family")?;
        return Ok(parsed);
    }
    if js::function::is_callable(options) || (!options.is_object() && !options.is_null()) {
        return Err(NodeError::invalid_arg_type(
            scope,
            "options",
            "of type object or integer",
            options,
        )
        .into());
    }
    let Ok(object) = Object::from_value(scope, *options) else {
        return Ok(parsed);
    };
    let hints = object.val(scope, c"hints")?;
    if !hints.is_null() && !hints.is_undefined() {
        if !hints.is_number() {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options.hints",
                "of type number",
                hints,
            )
            .into());
        }
        let flags = crate::dgram::to_uint32(scope, hints)?;
        if flags & !(ADDRCONFIG | ALL | V4MAPPED) != 0 {
            let flags = f64::from(flags).to_jsval_throwing(scope)?;
            return Err(NodeError::invalid_arg_value(scope, "hints", flags, "is invalid").into());
        }
        parsed.hints = flags;
    }
    let family = object.val(scope, c"family")?;
    if !family.is_null() && !family.is_undefined() {
        let name = if family.is_string() {
            Some(String::from_jsval(scope, family, ())?)
        } else {
            None
        };
        parsed.family = match name.as_deref() {
            Some("IPv4") if family_names => 4,
            Some("IPv6") if family_names => 6,
            _ => validate_family(scope, family, "options.family")?,
        };
    }
    if let Some(all) = boolean_option(scope, object, c"all", "options.all")? {
        parsed.all = all;
    }
    if let Some(verbatim) = boolean_option(scope, object, c"verbatim", "options.verbatim")? {
        parsed.order = if verbatim {
            Order::Verbatim
        } else {
            Order::Ipv4First
        };
    }
    let order = object.val(scope, c"order")?;
    if !order.is_null() && !order.is_undefined() {
        parsed.order = validate_order(scope, order, "options.order")?;
    }
    Ok(parsed)
}

/// The host name `hostname`, or `None` if it is falsy, or the error Node.js throws for it.
fn validate_hostname(
    scope: &Scope<'_>,
    hostname: HandleValue<'_>,
) -> Result<Option<String>, Thrown> {
    if !js::conversion::to_boolean(hostname) {
        return Ok(None);
    }
    let host = crate::errors::validate_string(scope, hostname, "hostname")?;
    if host.contains('\0') {
        return Err(NodeError::invalid_arg_value(
            scope,
            "hostname",
            hostname,
            "must be a string without null bytes",
        )
        .into());
    }
    Ok(Some(host))
}

/// The address family number of `addr`.
fn family_of(addr: &IpAddr) -> u8 {
    if addr.is_ipv4() {
        4
    } else {
        6
    }
}

/// The addresses of `addrs` that a lookup of the address family `family` with the `getaddrinfo`
/// flags `hints` gives, in their order: all of them for family 0, and those of the family
/// otherwise. For family 6 with `V4MAPPED`, the IPv4 addresses mapped to IPv6 follow the IPv6
/// addresses if there are none or if `hints` also has `ALL`. `ADDRCONFIG` has no effect.
pub(crate) fn addresses_of_family(addrs: Vec<IpAddr>, family: u8, hints: u32) -> Vec<IpAddr> {
    if family == 0 {
        return addrs;
    }
    let (wanted, others): (Vec<IpAddr>, Vec<IpAddr>) = addrs
        .into_iter()
        .partition(|addr| family_of(addr) == family);
    if family != 6 || hints & V4MAPPED == 0 || (!wanted.is_empty() && hints & ALL == 0) {
        return wanted;
    }
    let mapped = others.into_iter().filter_map(|addr| match addr {
        IpAddr::V4(v4) => Some(IpAddr::V6(v4.to_ipv6_mapped())),
        IpAddr::V6(_) => None,
    });
    wanted.into_iter().chain(mapped).collect()
}

/// A `{ address, family }` object.
fn entry<'s>(
    scope: &'s Scope<'_>,
    address: Option<&str>,
    family: u8,
) -> Result<HandleValue<'s>, ExnThrown> {
    let address = match address {
        Some(address) => address.to_jsval_throwing(scope)?,
        None => HandleValue::null(),
    };
    let family = scope.root_value(js::value::from_f64(f64::from(family)));
    Object::new_plain_with_template(scope, &ENTRY_PROPERTIES, &[address, family])?
        .to_jsval_throwing(scope)
}

/// The result of looking `host` up without the host's lookup, if it needs none: for no host
/// name, no address, and for an IP address, that address with its own family. The result is an
/// array of the `{ address, family }` objects if `options.all`, and the first one otherwise. No
/// host name emits the `DEP0118` deprecation warning for `hostname`, the argument `host` came
/// from, once per process.
fn immediate_result<'s>(
    scope: &'s Scope<'_>,
    hostname: HandleValue<'_>,
    host: Option<&str>,
    options: &LookupOptions,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let found = match host {
        None => {
            let message = format!(
                "The provided hostname \"{}\" is not a valid hostname, and is supported in the dns \
                 module solely for compatibility.",
                crate::errors::js_string(scope, hostname)?
            );
            crate::process::deprecate_once(scope, &message, "DEP0118")?;
            None
        }
        Some(host) if crate::net::is_ipv4(host) => Some((host, 4)),
        Some(host) if crate::net::is_ipv6(host) => Some((host, 6)),
        Some(_) => return Ok(None),
    };
    let result = match (found, options.all) {
        (Some((address, family)), true) => {
            let entry = entry(scope, Some(address), family)?;
            js::Array::with_contents(scope, &[entry])?.to_jsval_throwing(scope)?
        }
        (Some((address, family)), false) => entry(scope, Some(address), family)?,
        (None, true) => js::Array::new(scope, 0)?.to_jsval_throwing(scope)?,
        (None, false) => entry(scope, None, if options.family == 6 { 6 } else { 4 })?,
    };
    Ok(Some(result))
}

/// Start looking `host` up with `options`, and return a promise for an array of the `{ address,
/// family }` objects of the addresses found, or for the first one.
fn start_lookup<'s>(
    scope: &'s Scope<'_>,
    host: String,
    options: LookupOptions,
) -> Result<js::Promise<'s>, ExnThrown> {
    let future = async move {
        let result = platform::net::resolve(&host).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let fail = |scope: &Scope<'_>, error: &platform::net::NetError| -> ExnThrown {
                crate::fs::UvError::net(error, "getaddrinfo")
                    .suffix(format!(" {host}"))
                    .hostname(&host)
                    .throw(scope)
            };
            let mut addrs = addresses_of_family(
                result.map_err(|error| fail(scope, &error))?,
                options.family,
                options.hints,
            );
            match options.order {
                Order::Verbatim => {}
                Order::Ipv4First => addrs.sort_by_key(family_of),
                Order::Ipv6First => addrs.sort_by_key(|addr| std::cmp::Reverse(family_of(addr))),
            }
            if addrs.is_empty() {
                return Err(fail(
                    scope,
                    &platform::net::NetError::NoAddress(host.clone()),
                ));
            }
            let mut entries = Vec::new();
            for addr in &addrs {
                entries.push(entry(scope, Some(&addr.to_string()), family_of(addr))?);
            }
            if options.all {
                return js::Array::with_contents(scope, &entries)?.to_jsval_throwing(scope);
            }
            Ok(entries.swap_remove(0))
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    Ok(promise)
}

/// Call `callback` with `null` and `result`, an array if `all`, and otherwise the address and
/// family of the `{ address, family }` object `result`.
fn call_back(
    scope: &Scope<'_>,
    callback: HandleValue<'_>,
    result: HandleValue<'_>,
    all: bool,
) -> Result<(), ExnThrown> {
    let null = HandleValue::null();
    if all {
        return crate::support::call_reporting(scope, callback, &[null, result]).map(drop);
    }
    let result = Object::from_value(scope, *result).map_err(|e| e.throw(scope))?;
    let address = result.val(scope, c"address")?;
    let family = result.val(scope, c"family")?;
    crate::support::call_reporting(scope, callback, &[null, address, family])?;
    Ok(())
}

/// The payload of [`on_lookup`]: the callback of a callback lookup, and whether it gets all
/// addresses.
#[core_runtime::jsclass(hidden)]
pub(crate) struct LookupRequest {
    callback: Heap<Value>,
    #[no_trace]
    all: bool,
}

#[core_runtime::jsmethods]
impl LookupRequest {}

/// The fulfillment reaction of a callback lookup, and the tick callback of one that needed no
/// lookup, whose payload is a [`LookupRequest`] and whose argument is the result.
fn on_lookup(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = cast_payload::<LookupRequest>(scope, payload);
    let callback = request.data().callback.get(scope);
    let all = request.data().all;
    call_back(scope, callback, args.get(0), all)?;
    Ok(js::value::undefined())
}

/// The rejection reaction of a callback lookup, whose payload is the callback: call the callback
/// with the error.
fn on_lookup_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    callback: HandleValue,
) -> Result<Value, ExnThrown> {
    crate::support::call_reporting(scope, callback, &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// `dnsPromises.lookup(hostname[, options])`.
fn lookup_promise_impl<'s>(
    scope: &'s Scope<'_>,
    hostname: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<js::Promise<'s>, Thrown> {
    let host = validate_hostname(scope, hostname)?;
    let options = lookup_options(scope, options, false)?;
    if let Some(result) = immediate_result(scope, hostname, host.as_deref(), &options)? {
        let promise = js::Promise::new_pending(scope)?;
        promise.resolve(scope, result)?;
        return Ok(promise);
    }
    Ok(start_lookup(scope, host.unwrap_or_default(), options)?)
}

/// The body of `dnsPromises.lookup`.
fn lookup_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let values = args.rest(0);
    let hostname = values
        .first()
        .copied()
        .unwrap_or_else(|| scope.root_value(js::value::undefined()));
    let promise =
        lookup_promise_impl(scope, hostname, values.get(1).copied()).map_err(|e| e.throw(scope))?;
    promise.to_jsval_raw_throwing(scope)
}

/// Set the default lookup order to the order `value` names.
fn set_default_order(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<(), Thrown> {
    let order = validate_order(scope, value, "dnsOrder")?;
    DEFAULT_ORDER.with(|default| default.set(Some(order)));
    Ok(())
}

/// The body of `dnsPromises.setDefaultResultOrder`.
fn set_order_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    set_default_order(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    Ok(js::value::undefined())
}

/// The body of `dnsPromises.getDefaultResultOrder`.
fn get_order_callback(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let name = default_order().name();
    js::JSString::from_str(scope, name)?.to_jsval_raw_throwing(scope)
}

/// The address and port that `lookupService` looks up: `address`, an IP address whose IPv6 zone
/// is the name or number of an interface, and `port`.
fn service_addr(address: &str, port: u16) -> Option<SocketAddr> {
    let (ip, zone) = match address.split_once('%') {
        Some((ip, zone)) => (ip, Some(zone)),
        None => (address, None),
    };
    match ip.parse::<IpAddr>().ok()? {
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
}

/// The address of `lookupService` and its port, checked as Node.js checks them.
fn service_args(
    scope: &Scope<'_>,
    address: HandleValue<'_>,
    port: HandleValue<'_>,
) -> Result<(String, u16), Thrown> {
    let text = crate::errors::validate_string(scope, address, "address")?;
    if !crate::net::is_ipv4(&text) && !crate::net::is_ipv6(&text) {
        return Err(NodeError::invalid_arg_value(scope, "address", address, "is invalid").into());
    }
    let port = crate::net::validate_port(scope, port, "Port")?;
    Ok((text, port))
}

/// Start looking up the host and service names of `address` and `port`, and return a promise
/// for a `{ hostname, service }` object.
fn start_lookup_service<'s>(
    scope: &'s Scope<'_>,
    address: String,
    port: u16,
) -> Result<js::Promise<'s>, ExnThrown> {
    let addr = service_addr(&address, port);
    let future = async move {
        let result = match addr {
            Some(addr) => platform::net::lookup_service(addr).await,
            None => Err(platform::net::NetError::NoAddress(address.clone())),
        };
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| match result {
            Ok((hostname, service)) => {
                let values = [
                    hostname.as_str().to_jsval_throwing(scope)?,
                    service.as_str().to_jsval_throwing(scope)?,
                ];
                Object::new_plain_with_template(scope, &SERVICE_PROPERTIES, &values)?
                    .to_jsval_throwing(scope)
            }
            Err(error) => Err(crate::fs::UvError::net(&error, "getnameinfo")
                .suffix(format!(" {address}"))
                .hostname(&address)
                .throw(scope)),
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    Ok(promise)
}

/// The fulfillment reaction of a callback `lookupService`, whose payload is the callback: call it
/// with `null` and the `hostname` and `service` of the result.
fn on_lookup_service(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    callback: HandleValue,
) -> Result<Value, ExnThrown> {
    let result = Object::from_value(scope, *args.get(0)).map_err(|e| e.throw(scope))?;
    let hostname = result.val(scope, c"hostname")?;
    let service = result.val(scope, c"service")?;
    crate::support::call_reporting(scope, callback, &[HandleValue::null(), hostname, service])?;
    Ok(js::value::undefined())
}

/// The body of `dnsPromises.lookupService(address, port)`.
fn lookup_service_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<js::Promise<'_>, Thrown> {
        if args.len() != 2 {
            return Err(NodeError::missing_args(&["address", "port"]).into());
        }
        let (address, port) = service_args(scope, args.get(0), args.get(1))?;
        Ok(start_lookup_service(scope, address, port)?)
    };
    let promise = run().map_err(|e| e.throw(scope))?;
    promise.to_jsval_raw_throwing(scope)
}

/// The values of the exports of `node:dns/promises`: `lookup`, `lookupService`, `Resolver`,
/// `setDefaultResultOrder`, `getDefaultResultOrder`, `setServers`, the functions bound to the
/// default resolver, and the error codes.
fn promises_exports<'s>(
    scope: &'s Scope<'_>,
    name: &'static str,
) -> Result<Vec<(&'static str, HandleValue<'s>)>, ExnThrown> {
    let bound = resolver::bound_functions(scope, resolver::default_resolver(scope)?, true)?;
    let mut exports = Vec::new();
    for &(export, kind) in crate::stub::exports_of(name).unwrap_or_default() {
        let function: Option<(&std::ffi::CStr, u32, js::function::Callback)> = match export {
            "lookup" => Some((c"lookup", 2, lookup_promise)),
            "lookupService" => Some((c"lookupService", 2, lookup_service_promise)),
            "setDefaultResultOrder" => Some((c"setDefaultResultOrder", 1, set_order_callback)),
            "getDefaultResultOrder" => Some((c"getDefaultResultOrder", 0, get_order_callback)),
            "setServers" => Some((
                c"defaultResolverSetServers",
                1,
                resolver::set_servers_promise,
            )),
            _ => None,
        };
        let value = if let Some((function_name, nargs, body)) = function {
            Function::new_callback(scope, function_name, nargs, body, ())?
                .to_jsval_throwing(scope)?
        } else if export == "Resolver" {
            resolver::promises_resolver_class::class_constructor(scope)?.to_jsval_throwing(scope)?
        } else if let Some((_, value)) = bound.iter().find(|(key, _)| *key == export) {
            *value
        } else {
            crate::stub::stub_value(scope, name, export, kind)?
        };
        exports.push((export, value));
    }
    Ok(exports)
}

#[core_runtime::jsmodule(node_args, name = "node:dns", default_namespace)]
pub mod module {
    use super::*;

    pub use super::resolver::resolver_class as Resolver;

    /// <https://nodejs.org/api/dns.html#dnslookuphostname-options-callback>
    #[function(length = 3)]
    pub fn lookup<'s>(
        scope: &'s Scope<'_>,
        hostname: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let host = validate_hostname(scope, hostname)?;
        let undefined = scope.root_value(js::value::undefined());
        let (options, callback) = match options {
            Some(options) if js::function::is_callable(options) => (None, options),
            Some(options) if !options.is_undefined() && !options.is_object() => {
                if !options.is_number() {
                    validate_function(scope, callback.unwrap_or(options), "callback")?;
                }
                (Some(options), callback.unwrap_or(undefined))
            }
            options => (options, callback.unwrap_or(undefined)),
        };
        validate_function(scope, callback, "callback")?;
        let options = lookup_options(scope, options, true)?;
        let all = options.all;
        let request =
            js::class::create_instance_with::<LookupRequestImpl>(scope, |_| LookupRequestImpl {
                callback: Heap::from(callback.get()),
                all,
            })?;
        if let Some(result) = immediate_result(scope, hostname, host.as_deref(), &options)? {
            js::jobs::queue_native_tick(scope, on_lookup, request, &[result])?;
            return Ok(Object::new_plain(scope)?);
        }
        let promise = start_lookup(scope, host.unwrap_or_default(), options)?;
        let on_lookup = Function::new_callback(scope, c"", 1, on_lookup, request)?;
        let on_error = Function::new_callback(scope, c"", 1, on_lookup_error, callback)?;
        promise.add_reactions(scope, Some(*on_lookup), Some(*on_error))?;
        Ok(Object::new_plain(scope)?)
    }

    /// <https://nodejs.org/api/dns.html#dnslookupserviceaddress-port-callback>: natively with
    /// the host's `getnameinfo`. On wasm, the lookup fails with `ENOTSUP`, since `wasi:sockets`
    /// provides no such lookup.
    #[function(length = 3)]
    pub fn lookup_service<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        if args.len() != 3 {
            return Err(NodeError::missing_args(&["address", "port", "callback"]).into());
        }
        let (address, port) = service_args(scope, args[0], args[1])?;
        let callback = args[2];
        validate_function(scope, callback, "callback")?;
        let promise = start_lookup_service(scope, address, port)?;
        let on_service = Function::new_callback(scope, c"", 1, on_lookup_service, callback)?;
        let on_error = Function::new_callback(scope, c"", 1, on_lookup_error, callback)?;
        promise.add_reactions(scope, Some(*on_service), Some(*on_error))?;
        Ok(Object::new_plain(scope)?)
    }

    /// <https://nodejs.org/api/dns.html#dnssetdefaultresultorderorder>
    pub fn set_default_result_order(
        scope: &Scope<'_>,
        order: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        set_default_order(scope, order)
    }

    /// <https://nodejs.org/api/dns.html#dnsgetdefaultresultorder>
    pub fn get_default_result_order() -> &'static str {
        default_order().name()
    }
}

#[core_runtime::jsmodule(node_args, name = "node:dns/promises", default_namespace)]
pub mod promises_module {}

/// Register `node:dns`: its `promises` is the `default` export of `node:dns/promises`, and the
/// functions bound to the default resolver are those of [`resolver::bound_functions`]. Defines
/// Node's `customPromisifyArgs` on `lookup` and `lookupService`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let exports = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let namespace = core_runtime::module::builtin_namespace(scope, "node:dns/promises")?
            .ok_or_else(|| {
                js::error::TypeError("node:dns/promises is not a builtin module".into())
                    .throw(scope)
            })?;
        let promises = namespace.get_property(scope, c"default")?;
        let resolver = resolver::default_resolver(scope)?;
        let mut exports = resolver::bound_functions(scope, resolver, false)?;
        let set_servers = Function::new_callback(
            scope,
            c"defaultResolverSetServers",
            1,
            resolver::set_servers_callback,
            (),
        )?;
        exports.push(("setServers", set_servers.to_jsval_throwing(scope)?));
        exports.push(("promises", promises));
        Ok(exports)
    };
    let Ok(exports) = exports() else {
        return false;
    };
    if !crate::stub::register_implemented_with::<module::js_module>(scope, name, &exports) {
        return false;
    }
    let mark = || -> Result<(), ExnThrown> {
        let namespace = core_runtime::module::builtin_namespace(scope, name)?.ok_or_else(|| {
            js::error::TypeError(format!("{name} is not a builtin module")).throw(scope)
        })?;
        let lookup = namespace.get_property(scope, c"lookup")?;
        let lookup = Object::from_value(scope, *lookup).map_err(|e| e.throw(scope))?;
        crate::util::define_promisify_args(scope, lookup, &["address", "family"])?;
        let service = namespace.get_property(scope, c"lookupService")?;
        let service = Object::from_value(scope, *service).map_err(|e| e.throw(scope))?;
        crate::util::define_promisify_args(scope, service, &["hostname", "service"])
    };
    mark().is_ok()
}

/// Register `node:dns/promises` with the values of [`promises_exports`].
pub(crate) fn register_promises(scope: &Scope<'_>, name: &'static str) -> bool {
    let Ok(exports) = promises_exports(scope, name) else {
        return false;
    };
    crate::stub::register_implemented_with::<promises_module::js_module>(scope, name, &exports)
}

/// Register the classes of `node:dns` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: js::Object<'_>) -> Result<(), ExnThrown> {
    LookupRequest::add_to_global(scope, global);
    resolver::register_classes(scope, global)
}
