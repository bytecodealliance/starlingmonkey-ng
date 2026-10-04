// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The socket of a served request and of a client request, whose connection the host manages.
//!
//! It is an `EventEmitter` with the properties of a connected `net.Socket` that the host reports,
//! and `net.Socket`'s connection options as methods without effect.

use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use super::server::emit_later;
use crate::stream::{call_method, is_function, string, Props};

/// What the host reports about a connection.
pub(super) struct HostConnection<'a> {
    /// The peer's address, port and family (`IPv4` or `IPv6`), if known.
    pub remote: Option<(String, u16, &'static str)>,
    /// The local port, if known.
    pub local_port: Option<u16>,
    /// Whether the connection uses TLS.
    pub encrypted: bool,
    /// The request a client socket belongs to, which `destroy` destroys.
    pub owner: Option<Object<'a>>,
}

/// A new socket with the properties of `connection`.
pub(super) fn new_host_socket<'s>(
    scope: &'s Scope<'_>,
    connection: HostConnection<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let class = host_socket_class::class_constructor(scope)?;
    let socket = js::Function::construct(scope, class, js::function::EmptyArgs)?;
    match connection.remote {
        Some((address, port, family)) => {
            socket.set_property(scope, c"remoteAddress", address.as_str())?;
            socket.set_property(scope, c"remotePort", f64::from(port))?;
            socket.set_property(scope, c"remoteFamily", family)?;
        }
        None => socket.set_property(scope, c"remoteAddress", js::value::undefined())?,
    }
    socket.set_property(
        scope,
        c"localPort",
        connection
            .local_port
            .map_or(js::value::undefined(), |port| {
                js::value::from_f64(f64::from(port))
            }),
    )?;
    socket.set_property(scope, c"encrypted", connection.encrypted)?;
    if let Some(owner) = connection.owner {
        socket.define_property(scope, c"_starlingOwner", owner, 0)?;
    }
    Ok(socket)
}

#[core_runtime::jsfunction_class(name = "Socket", extends = "crate::events::event_emitter")]
pub mod host_socket_class {
    use super::*;

    /// A connected socket that reads and writes nothing itself.
    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        crate::events::event_emitter::init(scope, this, None)
            .map_err(|thrown| thrown.throw(scope))?;
        this.set_property(scope, c"connecting", false)?;
        this.set_property(scope, c"pending", false)?;
        this.set_property(scope, c"destroyed", false)?;
        this.set_property(scope, c"readable", true)?;
        this.set_property(scope, c"writable", true)?;
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#socketsettimeouttimeout-callback>: `callback` becomes a
    /// `timeout` listener. The timeout has no effect.
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        _timeout: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        if let Some(callback) = callback.filter(|callback| is_function(scope, *callback)) {
            call_method(scope, this, c"once", &[string(scope, "timeout")?, callback])?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketsetnodelaynodelay>: has no effect.
    pub fn set_no_delay<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketsetkeepaliveenable-initialdelay>: has no effect.
    pub fn set_keep_alive<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketref>: has no effect.
    #[method(name = "ref")]
    pub fn ref_socket<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketunref>: has no effect.
    pub fn unref<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketaddress>: the local port, if the host reports it.
    pub fn address<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<Object<'s>, ExnThrown> {
        let address = Object::new_plain(scope)?;
        let port = this.val(scope, c"localPort")?;
        if !port.is_undefined() {
            address.set_property(scope, c"port", port)?;
        }
        Ok(address)
    }

    /// <https://nodejs.org/api/net.html#socketdestroyerror>: marks the socket destroyed, destroys
    /// the client request it belongs to with `error`, and emits `close` in the tick queue.
    pub fn destroy<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        error: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        if this.flag(scope, c"destroyed")? {
            return Ok(this);
        }
        this.set_property(scope, c"destroyed", true)?;
        this.set_property(scope, c"readable", false)?;
        this.set_property(scope, c"writable", false)?;
        let error = error.filter(|error| !error.is_undefined() && !error.is_null());
        if let Some(owner) = this.obj(scope, c"_starlingOwner")? {
            let args: Vec<HandleValue<'_>> = error.into_iter().collect();
            call_method(scope, owner, c"destroy", &args)?;
        }
        let had_error = scope.root_value(js::value::from_bool(error.is_some()));
        emit_later(scope, this, &[string(scope, "close")?, had_error])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketenddata-encoding-callback>: destroys the socket.
    pub fn end<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        host_socket_class::destroy(scope, this, None)
    }
}
