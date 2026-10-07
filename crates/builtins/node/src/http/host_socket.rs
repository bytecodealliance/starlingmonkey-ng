// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The socket of a served request and of a client request through the runtime's HTTP client,
//! whose connection the host manages.
//!
//! It is an `EventEmitter` with the accessors of a connected `net.Socket` over the addresses the
//! host provides, and `net.Socket`'s connection options as methods without effect. `_httpMessage` is
//! the message it belongs to.

use js::error::ExnThrown;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Function, Object};

/// The addresses and TLS use of a connection the host manages, and the message using it.
pub(super) struct HostConnection<'a> {
    /// The peer's address and port, if known.
    pub remote: Option<std::net::SocketAddr>,
    /// The local port, if known.
    pub local_port: Option<u16>,
    /// Whether the connection uses TLS.
    pub encrypted: bool,
    /// The message the socket belongs to.
    pub owner: Option<Object<'a>>,
}

/// The socket of a connection the host manages.
#[core_runtime::jsclass(hidden, name = "Socket")]
pub struct HostSocket {
    #[no_trace]
    remote: Option<std::net::SocketAddr>,
    #[no_trace]
    local_port: Option<u16>,
    #[no_trace]
    destroyed: bool,
    /// The message the socket belongs to, a client request or the response to a served request,
    /// which `destroy` destroys.
    owner: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl HostSocket {
    #[getter]
    fn connecting(&self) -> bool {
        false
    }

    #[getter]
    fn pending(&self) -> bool {
        false
    }

    #[getter]
    fn destroyed(&self) -> bool {
        self.data().destroyed
    }

    #[getter]
    fn readable(&self) -> bool {
        !self.data().destroyed
    }

    #[getter]
    fn writable(&self) -> bool {
        !self.data().destroyed
    }

    /// The peer's address, if the host provides it.
    #[getter]
    fn remote_address(&self) -> Option<String> {
        self.data().remote.map(|addr| addr.ip().to_string())
    }

    /// The peer's port, if the host provides it.
    #[getter]
    fn remote_port(&self) -> Option<f64> {
        self.data().remote.map(|addr| f64::from(addr.port()))
    }

    /// The peer's address family, `IPv4` or `IPv6`, if the host provides the peer's address.
    #[getter]
    fn remote_family(&self) -> Option<&'static str> {
        self.data()
            .remote
            .map(|addr| crate::net::socket::family(&addr))
    }

    /// The local port, if the host provides it.
    #[getter]
    fn local_port(&self) -> Option<f64> {
        self.data().local_port.map(f64::from)
    }

    /// <https://nodejs.org/api/net.html#socketsettimeouttimeout-callback>: `callback` becomes a
    /// `timeout` listener. The timeout has no effect.
    #[method]
    fn set_timeout<'r>(
        &self,
        scope: &'r Scope<'_>,
        _timeout: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, ExnThrown> {
        let this = self_object(scope, self);
        if let Some(callback) =
            callback.and_then(|callback| Object::from_value(scope, *callback).ok())
        {
            if callback.is_callable() {
                crate::events::listen(scope, this, c"once", c"timeout", callback)?;
            }
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketsetnodelaynodelay>: has no effect.
    #[method]
    fn set_no_delay<'r>(&self, scope: &'r Scope<'_>) -> Object<'r> {
        self_object(scope, self)
    }

    /// <https://nodejs.org/api/net.html#socketsetkeepaliveenable-initialdelay>: has no effect.
    #[method]
    fn set_keep_alive<'r>(&self, scope: &'r Scope<'_>) -> Object<'r> {
        self_object(scope, self)
    }

    /// <https://nodejs.org/api/net.html#socketref>: has no effect.
    #[method(name = "ref")]
    fn ref_socket<'r>(&self, scope: &'r Scope<'_>) -> Object<'r> {
        self_object(scope, self)
    }

    /// <https://nodejs.org/api/net.html#socketunref>: has no effect.
    #[method]
    fn unref<'r>(&self, scope: &'r Scope<'_>) -> Object<'r> {
        self_object(scope, self)
    }

    /// <https://nodejs.org/api/net.html#socketaddress>: the local port, if the host provides it.
    #[method]
    fn address<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        let address = Object::new_plain(scope)?;
        let port = self.data().local_port;
        if let Some(port) = port {
            address.set_property(scope, c"port", f64::from(port))?;
        }
        Ok(address)
    }

    /// <https://nodejs.org/api/net.html#socketdestroyerror>: marks the socket destroyed, destroys
    /// the message it belongs to with `error`, and emits `close` in the tick queue.
    #[method]
    fn destroy<'r>(
        &self,
        scope: &'r Scope<'_>,
        error: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, ExnThrown> {
        destroy(scope, self_object(scope, self), error)
    }

    /// <https://nodejs.org/api/net.html#socketenddata-encoding-callback>: destroys the socket.
    #[method]
    fn end<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        destroy(scope, self_object(scope, self), None)
    }
}

/// The object of `socket`, rooted in `scope`.
fn self_object<'r>(scope: &'r Scope<'_>, socket: &HostSocket<'_>) -> Object<'r> {
    Object::from_value(scope, socket.as_object().as_value()).expect("a socket is an object")
}

/// Destroy the host socket `socket`: mark it destroyed, destroy the message it belongs to with
/// `error`, and emit `close` in the tick queue.
fn destroy<'s>(
    scope: &Scope<'_>,
    socket: Object<'s>,
    error: Option<HandleValue<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let state = socket
        .cast::<HostSocket>()
        .map_err(|_| js::error::TypeError("not a socket".into()).throw(scope))?;
    let owner = {
        let mut data = state.data_mut();
        if data.destroyed {
            return Ok(socket);
        }
        data.destroyed = true;
        data.owner.take_rooted(scope)
    };
    let error = error.filter(|error| !error.is_null_or_undefined());
    if let Some(owner) = owner {
        let args: Vec<HandleValue<'_>> = error.into_iter().collect();
        Function::call_by_name(scope, owner.handle(), c"destroy", &args)?;
    }
    let had_error = scope.root_value(js::value::from_bool(error.is_some()));
    crate::support::emit_later(scope, socket, c"close", &[had_error])?;
    Ok(socket)
}

/// A new socket for `connection`, with `_httpMessage` set to `connection.owner`.
pub(super) fn new_host_socket<'s>(
    scope: &'s Scope<'_>,
    connection: HostConnection<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let socket = js::class::create_instance_with::<HostSocketImpl>(scope, |_| HostSocketImpl {
        remote: connection.remote,
        local_port: connection.local_port,
        destroyed: false,
        owner: connection.owner.map(Heap::from),
    })?;
    let socket = socket.as_object();
    crate::events::event_emitter::init(scope, socket, None)
        .map_err(|thrown| js::error::ThrowException::throw(thrown, scope))?;
    socket.set_property(scope, c"encrypted", connection.encrypted)?;
    if let Some(owner) = connection.owner {
        socket.set_property(scope, c"_httpMessage", owner)?;
    }
    Ok(socket)
}

/// Make `owner` the message `socket` belongs to, or none.
pub(super) fn set_owner(
    scope: &Scope<'_>,
    socket: Object<'_>,
    owner: Option<Object<'_>>,
) -> Result<(), ExnThrown> {
    if let Ok(state) = socket.cast::<HostSocket>() {
        state.data_mut().owner = owner.map(Heap::from);
    }
    match owner {
        Some(owner) => socket.set_property(scope, c"_httpMessage", owner),
        None => socket.set_property(scope, c"_httpMessage", js::value::null()),
    }
}

/// Register the socket class on `global`, with `EventEmitter.prototype` as its prototype's
/// prototype.
pub(super) fn register_class(scope: &Scope<'_>, global: Object<'_>) {
    HostSocket::add_to_global(scope, global);
    let chain = || -> Result<(), ExnThrown> {
        let proto = js::class::get_prototype_object_for::<HostSocketImpl>(scope)
            .expect("registering the class creates its prototype");
        let emitter = crate::events::event_emitter::prototype_object(scope)?;
        proto.set_prototype(scope, emitter.handle())
    };
    chain().expect("chaining the socket class can only fail due to OOM");
}

/// Whether `socket` is a host socket that was destroyed.
pub(super) fn is_destroyed(socket: Object<'_>) -> bool {
    socket
        .cast::<HostSocket>()
        .is_ok_and(|state| state.data().destroyed)
}
