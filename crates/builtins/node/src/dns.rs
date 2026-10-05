// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:dns` and `node:dns/promises`: `lookup` over the host's name lookup in `platform::net`,
//! and `setDefaultResultOrder` and `getDefaultResultOrder`. See <https://nodejs.org/api/dns.html>.

use std::cell::Cell;
use std::net::IpAddr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};

use crate::errors::{validate_function, NodeError, Thrown};
use crate::support::Props;

/// The key of the shared object `dns.promises` is.
static PROMISES_KEY: u8 = 0;

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
    /// The order `setDefaultResultOrder` set.
    static DEFAULT_ORDER: Cell<Order> = const { Cell::new(Order::Verbatim) };
}

/// The options of a lookup: the address family to keep, 0 for both, whether to give every
/// address, and their order.
struct LookupOptions {
    family: u8,
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
fn lookup_options(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
    family_names: bool,
) -> Result<LookupOptions, Thrown> {
    let mut parsed = LookupOptions {
        family: 0,
        all: false,
        order: DEFAULT_ORDER.with(Cell::get),
    };
    let Some(options) = options.filter(|options| !options.is_undefined()) else {
        return Ok(parsed);
    };
    if options.is_number() {
        parsed.family = validate_family(scope, options, "family")?;
        return Ok(parsed);
    }
    if !options.is_object() && !options.is_null() {
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
    if !hints.is_null() && !hints.is_undefined() && !hints.is_number() {
        return Err(
            NodeError::invalid_arg_type(scope, "options.hints", "of type number", hints).into(),
        );
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

/// A `{ address, family }` object.
fn entry<'s>(
    scope: &'s Scope<'_>,
    address: Option<&str>,
    family: u8,
) -> Result<HandleValue<'s>, ExnThrown> {
    let entry = Object::new_plain(scope)?;
    match address {
        Some(address) => entry.set_property(scope, c"address", address)?,
        None => entry.set_property(scope, c"address", js::value::null())?,
    }
    entry.set_property(scope, c"family", f64::from(family))?;
    entry.to_jsval_throwing(scope)
}

/// The result of looking `host` up without the host's lookup, if it needs none: for no host
/// name, no address, and for an IP address, that address with its own family. The result is an
/// array of the `{ address, family }` objects if `options.all`, and the first one otherwise.
fn immediate_result<'s>(
    scope: &'s Scope<'_>,
    host: Option<&str>,
    options: &LookupOptions,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let found = match host {
        None => None,
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
            let mut addrs: Vec<IpAddr> = result
                .map_err(|error| fail(scope, &error))?
                .into_iter()
                .filter(|addr| options.family == 0 || family_of(addr) == options.family)
                .collect();
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
    if let Some(result) = immediate_result(scope, host.as_deref(), &options)? {
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
    let values = crate::callback_args(&args);
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
    DEFAULT_ORDER.with(|default| default.set(order));
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
    let name = DEFAULT_ORDER.with(Cell::get).name();
    js::JSString::from_str(scope, name)?.to_jsval_raw_throwing(scope)
}

/// The object `dns.promises` and `node:dns/promises` export: `lookup`, `setDefaultResultOrder`,
/// `getDefaultResultOrder`, and stubs for the rest.
fn promises_object<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let object = js::class::get_or_init_shared_object(
        scope,
        &PROMISES_KEY as *const u8 as usize,
        |scope| {
            let object = Object::new_plain(scope)?;
            for &(name, kind) in crate::stub::exports_of("node:dns/promises").unwrap_or_default() {
                let key = std::ffi::CString::new(name).expect("names have no NULs");
                let implemented: Option<(u32, js::function::Callback)> = match name {
                    "lookup" => Some((2, lookup_promise)),
                    "setDefaultResultOrder" => Some((1, set_order_callback)),
                    "getDefaultResultOrder" => Some((0, get_order_callback)),
                    _ => None,
                };
                let value = match implemented {
                    Some((nargs, body)) => Function::new_callback(scope, &key, nargs, body, ())?
                        .to_jsval_throwing(scope)?,
                    None => crate::stub::stub_value(scope, "node:dns/promises", name, kind)?,
                };
                object.set_property_by_name(scope, &key, value)?;
            }
            Ok(object)
        },
    )?;
    object.to_jsval_throwing(scope)
}

#[core_runtime::jsmodule(node_args, name = "node:dns", default_namespace)]
pub mod module {
    use super::*;

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
        if let Some(result) = immediate_result(scope, host.as_deref(), &options)? {
            js::jobs::queue_native_tick(scope, on_lookup, request, &[result])?;
            return Ok(Object::new_plain(scope)?);
        }
        let promise = start_lookup(scope, host.unwrap_or_default(), options)?;
        let on_lookup = Function::new_callback(scope, c"", 1, on_lookup, request)?;
        let on_error = Function::new_callback(scope, c"", 1, on_lookup_error, callback)?;
        promise.add_reactions(scope, Some(*on_lookup), Some(*on_error))?;
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
        DEFAULT_ORDER.with(Cell::get).name()
    }
}

#[core_runtime::jsmodule(node_args, name = "node:dns/promises", default_namespace)]
pub mod promises_module {}

/// Register `node:dns` with `promises`, and define Node's `customPromisifyArgs` on `lookup`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let Ok(promises) = promises_object(scope) else {
        return false;
    };
    if !crate::stub::register_implemented_with::<module::js_module>(
        scope,
        name,
        &[("promises", promises)],
    ) {
        return false;
    }
    let mark_lookup = || -> Result<(), ExnThrown> {
        let namespace = core_runtime::module::builtin_namespace(scope, name)?.ok_or_else(|| {
            js::error::TypeError(format!("{name} is not a builtin module")).throw(scope)
        })?;
        let lookup = namespace.get_property(scope, c"lookup")?;
        let lookup = Object::from_value(scope, *lookup).map_err(|e| e.throw(scope))?;
        crate::util::define_promisify_args(scope, lookup, &["address", "family"])
    };
    mark_lookup().is_ok()
}

/// Register `node:dns/promises` with the properties of [`promises_object`].
pub(crate) fn register_promises(scope: &Scope<'_>, name: &'static str) -> bool {
    let exports = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let object = promises_object(scope)?;
        let object = Object::from_value(scope, *object).map_err(|e| e.throw(scope))?;
        let mut exports = Vec::new();
        for &(export, _) in crate::stub::exports_of(name).unwrap_or_default() {
            let key = std::ffi::CString::new(export).expect("names have no NULs");
            let value = object.get_property_by_name(scope, &key)?;
            exports.push((export, scope.root_value(value.get())));
        }
        Ok(exports)
    };
    let Ok(exports) = exports() else {
        return false;
    };
    crate::stub::register_implemented_with::<promises_module::js_module>(scope, name, &exports)
}
