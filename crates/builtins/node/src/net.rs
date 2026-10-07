// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:net`: TCP sockets and servers over `platform::net`, `BlockList` and `SocketAddress`, and
//! `isIP`, `isIPv4` and `isIPv6`.
//! See <https://nodejs.org/api/net.html>.

pub(crate) mod blocklist;
pub(crate) mod server;
pub(crate) mod socket;

use std::net::{Ipv4Addr, Ipv6Addr};

use js::conversion::{FromJSVal, ToJSVal};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};

pub(crate) use server::{listen_args, on_close};

/// The port `value`, a number or a string, as Node.js's `validatePort` checks it, or
/// `ERR_SOCKET_BAD_PORT` naming the port `name`, such as `Port` or `options.port`.
pub(crate) fn validate_port(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<u16, Thrown> {
    validate_port_allowing(scope, value, name, true)
}

/// [`validate_port`], which rejects port 0 unless `allow_zero`, as Node.js's
/// `validatePort(value, name, allowZero)` does.
pub(crate) fn validate_port_allowing(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    allow_zero: bool,
) -> Result<u16, Thrown> {
    let number = if value.is_number() {
        value.to_number()
    } else if value.is_string() {
        let text = String::from_jsval(scope, value, ())?;
        if text
            .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
            .is_empty()
        {
            f64::NAN
        } else {
            f64::from_jsval(scope, value, ())?
        }
    } else {
        f64::NAN
    };
    let rejected_zero = !allow_zero && number == 0.0;
    if !(0.0..=65535.0).contains(&number) || number.fract() != 0.0 || rejected_zero {
        return Err(NodeError::new(
            ErrorKind::RangeError,
            "ERR_SOCKET_BAD_PORT",
            format!(
                "{name} should be {} 0 and < 65536. Received {}.",
                if allow_zero { ">=" } else { ">" },
                crate::errors::describe_value(scope, value)
            ),
        )
        .into());
    }
    Ok(number as u16)
}

/// Whether `value` is a string that `Number()` doesn't convert to a number of at least 0, which
/// Node.js takes for an IPC path.
fn is_pipe_name(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, Thrown> {
    if !value.is_string() {
        return Ok(false);
    }
    let number = f64::from_jsval(scope, value, ())?;
    // `NaN` is a pipe name too.
    Ok(number.is_nan() || number < 0.0)
}

/// The private symbol under which the arrays `net._normalizeArgs` returns are marked with `true`.
static NORMALIZED_ARGS: js::symbol::CachedSymbol = js::symbol::CachedSymbol::new(c"normalizedArgs");

/// The symbol that marks the arrays `net._normalizeArgs` returns, which `internal/net` exports.
pub(crate) fn normalized_args_symbol<'s>(
    scope: &'s Scope<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    NORMALIZED_ARGS.value(scope)
}

/// The options and the listener of `value`, if it is an array `net._normalizeArgs` returned.
pub(crate) fn normalized_args<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<(HandleValue<'s>, HandleValue<'s>)>, ExnThrown> {
    let Ok(array) = Object::from_value(scope, value) else {
        return Ok(None);
    };
    if !js::Array::is_array(scope, array.handle())? {
        return Ok(None);
    }
    let marked = array.get_property_by_id(scope, NORMALIZED_ARGS.id(scope)?)?;
    if !(marked.is_boolean() && marked.to_boolean()) {
        return Ok(None);
    }
    Ok(Some((
        array.get_element(scope, 0)?,
        array.get_element(scope, 1)?,
    )))
}

js::instance_local! {
    /// Whether auto-select-family is enabled, and its attempt timeout in milliseconds, once
    /// `net` set them. Until then they are the defaults the Node.js runtime flags give.
    static AUTO_SELECT: std::cell::Cell<Option<(bool, f64)>> = const { std::cell::Cell::new(None) };
}

/// The auto-select-family settings: whether it is enabled, `true` by default, and the attempt
/// timeout, 250 milliseconds by default. The Node.js runtime flags can change both defaults.
fn auto_select() -> (bool, f64) {
    AUTO_SELECT.get().unwrap_or_else(|| {
        let flags = crate::runtime_flags::get();
        (
            flags.network_family_autoselection,
            flags.network_family_autoselection_attempt_timeout,
        )
    })
}

/// Set the auto-select-family settings to what `update` makes of them.
fn set_auto_select(update: impl FnOnce(&mut (bool, f64))) {
    let mut settings = auto_select();
    update(&mut settings);
    AUTO_SELECT.set(Some(settings));
}

/// A new socket connected with the `connect` arguments `args`, as `net.connect` does.
pub(crate) fn connect<'s>(
    scope: &'s Scope<'_>,
    args: &[HandleValue<'_>],
) -> Result<Object<'s>, ExnThrown> {
    let class = socket::socket_class::class_constructor(scope)?;
    let args = js::Array::with_contents(scope, args)?;
    let normalized = module::normalize_args(scope, (*args).to_jsval_throwing(scope)?)
        .map_err(|thrown| js::error::ThrowException::throw(thrown, scope))?;
    let options = normalized.get_element(scope, 0)?;
    let socket = Function::construct(scope, class, &[options])?;
    if let Ok(options) = Object::from_value(scope, *options) {
        let timeout = options.get_property(scope, c"timeout")?;
        if js::conversion::to_boolean(timeout) {
            let timeout = scope.root_value(timeout.get());
            Function::call_by_name(scope, socket.handle(), c"setTimeout", &[timeout])?;
        }
    }
    let normalized = normalized.to_jsval_throwing(scope)?;
    Function::call_by_name(scope, socket.handle(), c"connect", &[normalized])?;
    Ok(socket)
}

/// Whether `input` is an IPv4 address in dotted-decimal form, without leading zeros.
pub(crate) fn is_ipv4(input: &str) -> bool {
    input.parse::<Ipv4Addr>().is_ok()
}

/// Whether `input` is an IPv6 address, optionally with a zone ID such as `%eth0`.
pub(crate) fn is_ipv6(input: &str) -> bool {
    let address = match input.split_once('%') {
        Some((address, zone)) => {
            let valid_zone = !zone.is_empty()
                && zone
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':'));
            if !valid_zone {
                return false;
            }
            address
        }
        None => input,
    };
    address.parse::<Ipv6Addr>().is_ok()
}

#[core_runtime::jsmodule(node_args, name = "node:net", default_namespace)]
pub mod module {
    use super::*;

    pub use super::blocklist::BlockList;
    pub use super::blocklist::SocketAddress;
    pub use super::server::server_class as Server;
    pub use super::socket::socket_class as Socket;
    pub use super::socket::socket_class as Stream;

    /// <https://nodejs.org/api/net.html#netgetdefaultautoselectfamily>: connecting tries the
    /// addresses a host resolves to in turn, whatever this setting is.
    pub fn get_default_auto_select_family() -> bool {
        super::auto_select().0
    }

    /// <https://nodejs.org/api/net.html#netsetdefaultautoselectfamilyvalue>: `value` must be a
    /// boolean.
    pub fn set_default_auto_select_family(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let value = crate::errors::validate_boolean(scope, value, "value")?;
        super::set_auto_select(|settings| settings.0 = value);
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#netgetdefaultautoselectfamilyattempttimeout>
    pub fn get_default_auto_select_family_attempt_timeout() -> f64 {
        super::auto_select().1
    }

    /// <https://nodejs.org/api/net.html#netsetdefaultautoselectfamilyattempttimeoutvalue>: `value`
    /// must be an integer from 1 to 2**31 - 1, and values below 10 become 10.
    pub fn set_default_auto_select_family_attempt_timeout(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let value = crate::errors::validate_integer(scope, value, "value", 1.0, 2147483647.0)?;
        super::set_auto_select(|settings| settings.1 = value.max(10.0));
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#netconnect>
    pub fn connect<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::connect(scope, &args)
    }

    /// <https://nodejs.org/api/net.html#netcreateconnection>
    pub fn create_connection<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::connect(scope, &args)
    }

    /// <https://nodejs.org/api/net.html#netcreateserveroptions-connectionlistener>
    #[function(length = 2)]
    pub fn create_server<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = super::server::server_class::class_constructor(scope)?;
        let args: Vec<HandleValue<'_>> =
            args.iter().map(|arg| scope.root_value(arg.get())).collect();
        Function::construct(scope, class, &args)
    }

    /// `net._normalizeArgs(args)`: the arguments `args` of `connect` or `listen` as
    /// `[options, listener]`, where `options` is the first argument if it is an object, and is
    /// made of a path, or of a port and a host, otherwise. `listener` is the last argument if it
    /// is a function, and `null` otherwise. `args` with a `length` of 0 gives `[{}, null]`. The
    /// array is marked as normalized, which `socket.connect` recognizes.
    #[function(name = "_normalizeArgs")]
    pub fn normalize_args<'s>(
        scope: &'s Scope<'_>,
        args: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let args = Object::from_value(scope, args)?;
        let length = args.get_property(scope, c"length")?;
        let (options, listener) = if length.is_number() && length.to_number() == 0.0 {
            (Object::new_plain(scope)?, HandleValue::null())
        } else {
            let length = f64::from_jsval_throwing(scope, length, ())?;
            let first = args.get_element(scope, 0)?;
            let options = if first.is_object() && !js::function::is_callable(first) {
                Object::from_value(scope, first)?
            } else {
                let options = Object::new_plain(scope)?;
                if super::is_pipe_name(scope, first)? {
                    options.set_property(scope, c"path", first)?;
                } else {
                    options.set_property(scope, c"port", first)?;
                    if length > 1.0 {
                        let host = args.get_element(scope, 1)?;
                        if host.is_string() {
                            options.set_property(scope, c"host", host)?;
                        }
                    }
                }
                options
            };
            let last_index = scope.root_value(js::value::from_f64(length - 1.0));
            let last_id = scope.root_id(js::id::value_to_id(scope, last_index)?);
            let last = args.get_property_by_id(scope, last_id)?;
            let listener = if Object::from_value(scope, last).is_ok_and(|last| last.is_callable()) {
                last
            } else {
                HandleValue::null()
            };
            (options, listener)
        };
        let normalized =
            js::Array::with_contents(scope, &[options.to_jsval_throwing(scope)?, listener])?;
        normalized.set_property_by_id(scope, super::NORMALIZED_ARGS.id(scope)?, true)?;
        Ok(*normalized)
    }

    /// <https://nodejs.org/api/net.html#netisipinput>: 4, 6, or 0 if `input` is neither.
    #[function(name = "isIP")]
    pub fn is_ip(input: String) -> u8 {
        if is_ipv4(&input) {
            4
        } else if is_ipv6(&input) {
            6
        } else {
            0
        }
    }

    /// <https://nodejs.org/api/net.html#netisipv4input>
    #[function(name = "isIPv4")]
    pub fn is_ip_v4(input: String) -> bool {
        is_ipv4(&input)
    }

    /// <https://nodejs.org/api/net.html#netisipv6input>
    #[function(name = "isIPv6")]
    pub fn is_ip_v6(input: String) -> bool {
        is_ipv6(&input)
    }
}

/// Register the classes of `node:net` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: js::Object<'_>) -> Result<(), ExnThrown> {
    blocklist::register_classes(scope, global)?;
    server::ServerState::add_to_global(scope, global);
    socket::SocketState::add_to_global(scope, global);
    server::server_class::register(scope)?;
    socket::socket_class::register(scope)?;
    Ok(())
}
