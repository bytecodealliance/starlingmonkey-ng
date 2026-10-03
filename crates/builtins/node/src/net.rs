// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:net`: TCP sockets and servers over `platform::net`, and `isIP`, `isIPv4` and `isIPv6`.
//! See <https://nodejs.org/api/net.html>.

mod server;
mod socket;

use std::net::{Ipv4Addr, Ipv6Addr};

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::stream::call_method;

pub(crate) use server::{listen_args, on_close};
pub(crate) use socket::throw_net_error;

/// Register the classes of `node:net` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    server::ListenerHandle::add_to_global(scope, global);
    socket::SocketHandle::add_to_global(scope, global);
    socket::PendingConnect::add_to_global(scope, global);
    server::server_class::register(scope)?;
    socket::socket_class::register(scope)?;
    Ok(())
}

/// `value` as Node.js's `determineSpecificType` describes primitives in error messages, such as
/// `type number (70000)` or `type string ('abc')`.
pub(crate) fn specific_type(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    if value.is_null() {
        return "null".into();
    }
    if value.is_undefined() {
        return "undefined".into();
    }
    if value.is_string() {
        let text = String::from_jsval(scope, value, ()).unwrap_or_default();
        let text = if text.encode_utf16().count() > 28 {
            format!("{}...", text.chars().take(25).collect::<String>())
        } else {
            text
        };
        if !text.contains('\'') {
            return format!("type string ('{text}')");
        }
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("type string (\"{escaped}\")");
    }
    if let Ok(object) = Object::from_value(scope, *value) {
        let name = object
            .get_property(scope, c"constructor")
            .ok()
            .and_then(|ctor| Object::from_value(scope, ctor).ok())
            .and_then(|ctor| ctor.get_property(scope, c"name").ok())
            .and_then(|name| String::from_jsval(scope, name, ()).ok())
            .unwrap_or_else(|| "Object".into());
        return format!("an instance of {name}");
    }
    if value.is_symbol() {
        return "type symbol (Symbol())".into();
    }
    if value.is_number() && value.to_number() == 0.0 && value.to_number().is_sign_negative() {
        return "type number (-0)".into();
    }
    let (kind, suffix) = if value.is_number() {
        ("number", "")
    } else if value.is_boolean() {
        ("boolean", "")
    } else {
        ("bigint", "n")
    };
    let text = js::JSString::from_value(scope, value)
        .and_then(|text| text.to_utf8(scope))
        .unwrap_or_default();
    format!("type {kind} ({text}{suffix})")
}

/// The port `value`, a number or a string, as Node.js's `validatePort` checks it, or
/// `ERR_SOCKET_BAD_PORT` naming the port `name`, such as `Port` or `options.port`.
pub(crate) fn validate_port(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
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
    if !(0.0..=65535.0).contains(&number) || number.fract() != 0.0 {
        return Err(NodeError::new(
            ErrorKind::RangeError,
            "ERR_SOCKET_BAD_PORT",
            format!(
                "{name} should be >= 0 and < 65536. Received {}.",
                specific_type(scope, value)
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

/// The key of the shared object that holds the auto-select-family settings.
static AUTO_SELECT_KEY: u8 = 0;

/// The auto-select-family settings: whether it is enabled, `true` by default, and the attempt
/// timeout, 250 milliseconds by default.
fn auto_select(scope: &Scope<'_>) -> Result<(bool, f64), ExnThrown> {
    let settings = auto_select_settings(scope)?;
    let family = settings.get_property(scope, c"family")?;
    let timeout = settings.get_property(scope, c"timeout")?;
    Ok((
        family.is_boolean() && family.to_boolean(),
        timeout.to_number(),
    ))
}

/// Set the auto-select-family setting `name` to `value`.
fn set_auto_select<'s>(
    scope: &'s Scope<'_>,
    name: &'static std::ffi::CStr,
    value: impl js::conversion::ToJSVal<'s>,
) -> Result<(), ExnThrown> {
    auto_select_settings(scope)?.set_property(scope, name, value)
}

/// The shared object of the auto-select-family settings.
fn auto_select_settings<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::get_or_init_shared_object(scope, &AUTO_SELECT_KEY as *const u8 as usize, |scope| {
        let settings = Object::new_plain(scope)?;
        settings.set_property(scope, c"family", true)?;
        settings.set_property(scope, c"timeout", 250)?;
        Ok(settings)
    })
}

/// A new socket connected with the `connect` arguments `args`, as `net.connect` does.
fn connect<'s>(scope: &'s Scope<'_>, args: &[HandleValue<'_>]) -> Result<Object<'s>, ExnThrown> {
    let class = socket::socket_class::class_constructor(scope)?;
    let options = args
        .first()
        .filter(|first| first.is_object() && !crate::stream::is_function(scope, **first))
        .map(|options| scope.root_value(options.get()));
    let socket = match options {
        Some(options) => Function::construct(scope, class, &[options])?,
        None => Function::construct(scope, class, js::function::EmptyArgs)?,
    };
    if let Some(options) = options.and_then(|options| Object::from_value(scope, *options).ok()) {
        let timeout = options.get_property(scope, c"timeout")?;
        if crate::stream::truthy(scope, timeout) {
            let timeout = scope.root_value(timeout.get());
            call_method(scope, socket, c"setTimeout", &[timeout])?;
        }
    }
    call_method(scope, socket, c"connect", args)?;
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

    pub use super::server::server_class as Server;
    pub use super::socket::socket_class as Socket;
    pub use super::socket::socket_class as Stream;

    /// <https://nodejs.org/api/net.html#netgetdefaultautoselectfamily>: connecting tries the
    /// addresses a host resolves to in turn, whatever this setting is.
    pub fn get_default_auto_select_family(scope: &Scope<'_>) -> Result<bool, ExnThrown> {
        Ok(super::auto_select(scope)?.0)
    }

    /// <https://nodejs.org/api/net.html#netsetdefaultautoselectfamilyvalue>
    pub fn set_default_auto_select_family(scope: &Scope<'_>, value: bool) -> Result<(), ExnThrown> {
        super::set_auto_select(scope, c"family", value)
    }

    /// <https://nodejs.org/api/net.html#netgetdefaultautoselectfamilyattempttimeout>
    pub fn get_default_auto_select_family_attempt_timeout(
        scope: &Scope<'_>,
    ) -> Result<f64, ExnThrown> {
        Ok(super::auto_select(scope)?.1)
    }

    /// <https://nodejs.org/api/net.html#netsetdefaultautoselectfamilyattempttimeoutvalue>: values
    /// below 10 become 10.
    pub fn set_default_auto_select_family_attempt_timeout(
        scope: &Scope<'_>,
        value: f64,
    ) -> Result<(), ExnThrown> {
        super::set_auto_select(scope, c"timeout", value.max(10.0))
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
    pub fn create_server<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = super::server::server_class::class_constructor(scope)?;
        let args: Vec<HandleValue<'_>> =
            args.iter().map(|arg| scope.root_value(arg.get())).collect();
        Function::construct(scope, class, &args)
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
