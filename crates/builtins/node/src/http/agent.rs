// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Agent` and `https.Agent`, ported from Node.js's `lib/_http_agent.js`.
//!
//! An agent keeps the sockets its requests use in `sockets`, the sockets it can reuse in
//! `freeSockets`, and the requests waiting for a socket in `requests`, each under the name
//! `getName` gives the connection, and counts its sockets in `totalSocketCount`. These are the
//! own data properties Node.js defines, which the agent reads and changes as Node.js's does,
//! since libraries such as `agent-base` change them too. The agent's methods call each other by
//! name, so subclasses can replace them. The listeners an agent adds to a socket are native
//! functions whose payload is an [`AgentSocket`].
//!
//! Requests through `platform::http` don't call `addRequest` (see [`super::client`]).

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::client::with_client;
use crate::errors::{NodeError, Thrown};

/// The listeners an agent added to one of its sockets, and what they need.
#[core_runtime::jsclass(hidden)]
pub(crate) struct AgentSocket {
    agent: Heap<js::object::Object>,
    socket: Heap<js::object::Object>,
    /// The options the socket was created with.
    options: Heap<js::object::Object>,
    /// The `free`, `close`, `timeout` and `agentRemove` listeners, in the order of
    /// [`SOCKET_EVENTS`].
    listeners: Vec<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl AgentSocket {}

/// A socket the agent creates for a request, with the callback it reports the socket to.
#[core_runtime::jsclass(hidden)]
pub(crate) struct SocketRequest {
    agent: Heap<js::object::Object>,
    /// The request, for the callbacks of `addRequest` and `removeSocket`.
    request: Heap<js::object::Object>,
    /// The options of `createSocket`.
    options: Option<Heap<js::object::Object>>,
    /// The connection's name.
    #[no_trace]
    name: String,
    /// The callback of `createSocket`.
    callback: Option<Heap<Value>>,
    /// Whether the socket was reported to the callback.
    #[no_trace]
    created: bool,
}

#[core_runtime::jsmethods]
impl SocketRequest {}

/// The events the agent listens to on its sockets, with the listener bodies.
const SOCKET_EVENTS: [(&std::ffi::CStr, js::function::Callback); 4] = [
    (c"free", socket_on_free),
    (c"close", socket_on_close),
    (c"timeout", socket_on_timeout),
    (c"agentRemove", socket_on_remove),
];

/// `object[name]` as a number, or `default` if it isn't one.
fn number(
    scope: &Scope<'_>,
    object: Object<'_>,
    name: &'static std::ffi::CStr,
    default: f64,
) -> Result<f64, ExnThrown> {
    let value = object.get_property(scope, name)?;
    Ok(if value.is_number() {
        value.to_number()
    } else {
        default
    })
}

/// `object[name]` if it is an object.
fn object_property<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Option<Object<'s>>, ExnThrown> {
    Ok(Object::from_value(scope, *object.get_property(scope, name)?).ok())
}

/// A copy of `options`, without a prototype, with the agent's options assigned over it, and
/// `path` set to `socketPath` if it is given, as `addRequest` and `createSocket` make it.
fn merged_options<'s>(
    scope: &'s Scope<'_>,
    agent: Object<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let merged = Object::new_with_null_prototype(scope)?;
    if let Ok(options) = Object::from_value(scope, *options) {
        merged.assign(scope, options)?;
    }
    if let Some(own) = object_property(scope, agent, c"options")? {
        merged.assign(scope, own)?;
    }
    let socket_path = merged.get_property(scope, c"socketPath")?;
    if js::conversion::to_boolean(socket_path) {
        merged.set_property(scope, c"path", socket_path)?;
    }
    Ok(merged)
}

/// Set `options.servername` from the request's `Host` header or `options.host`, unless it is
/// set, as Node.js's `normalizeServerName` does. An IP address gives an empty name.
fn normalize_server_name(
    scope: &Scope<'_>,
    options: Object<'_>,
    request: Object<'_>,
) -> Result<(), Thrown> {
    let given = options.get_property(scope, c"servername")?;
    if js::conversion::to_boolean(given) || crate::support::is_str(scope, given, c"")? {
        return Ok(());
    }
    let mut name = options.get_property(scope, c"host")?;
    let host = Function::call_by_name(
        scope,
        request.handle(),
        c"getHeader",
        &[js::class::get_or_init_atom(scope, c"host")?],
    )?;
    if js::conversion::to_boolean(host) {
        let host = crate::errors::validate_string(scope, host, "options.headers.host")?;
        let server = match host.strip_prefix('[') {
            Some(rest) => match rest.find(']') {
                Some(end) => rest[..end].to_string(),
                None => host.clone(),
            },
            None => host.split(':').next().unwrap_or_default().to_string(),
        };
        name = server.as_str().to_jsval_throwing(scope)?;
    }
    if let Some(text) = crate::support::rust_string(scope, name) {
        if crate::net::is_ipv4(&text) || crate::net::is_ipv6(&text) {
            name = js::class::get_or_init_atom(scope, c"")?;
        }
    }
    options.set_property(scope, c"servername", name)?;
    Ok(())
}

/// The name of the connection with `options`, through the agent's `getName`.
fn name_of(scope: &Scope<'_>, agent: Object<'_>, options: Object<'_>) -> Result<String, ExnThrown> {
    let name = Function::call_by_name(
        scope,
        agent.handle(),
        c"getName",
        &[options.to_jsval_throwing(scope)?],
    )?;
    String::from_jsval_throwing(scope, name, ())
}

/// The array the pool `pool` of `agent`, such as `sockets`, holds under `name`.
fn pool_array<'s>(
    scope: &'s Scope<'_>,
    agent: Object<'_>,
    pool: &'static std::ffi::CStr,
    name: &str,
) -> Result<Option<js::Array<'s>>, ExnThrown> {
    let Some(pool) = object_property(scope, agent, pool)? else {
        return Ok(None);
    };
    let key = name.to_jsval_throwing(scope)?;
    let value = crate::http::get_by_key(scope, pool, key)?;
    super::outgoing::as_array(scope, value)
}

/// The array the pool `pool` of `agent` holds under `name`, created if it has none, as
/// `pool[name] ||= []` does.
fn pool_array_or_new<'s>(
    scope: &'s Scope<'_>,
    agent: Object<'_>,
    pool: &'static std::ffi::CStr,
    name: &str,
) -> Result<js::Array<'s>, ExnThrown> {
    let pool = object_property(scope, agent, pool)?
        .ok_or_else(|| js::error::TypeError("the agent has no socket pool".into()).throw(scope))?;
    let key = name.to_jsval_throwing(scope)?;
    let value = crate::http::get_by_key(scope, pool, key)?;
    if let Some(array) = super::outgoing::as_array(scope, value)? {
        return Ok(array);
    }
    let array = js::Array::new(scope, 0)?;
    crate::http::set_by_key(scope, pool, key, array.to_jsval_throwing(scope)?)?;
    Ok(array)
}

/// Delete `name` from the pool `pool` of `agent`.
fn delete_pool_entry(
    scope: &Scope<'_>,
    agent: Object<'_>,
    pool: &'static std::ffi::CStr,
    name: &str,
) -> Result<(), ExnThrown> {
    if let Some(pool) = object_property(scope, agent, pool)? {
        let key = name.to_jsval_throwing(scope)?;
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        pool.delete_property_by_id(scope, id)?;
    }
    Ok(())
}

/// Add `delta` to the `totalSocketCount` of `agent`.
fn count_sockets(scope: &Scope<'_>, agent: Object<'_>, delta: f64) -> Result<(), ExnThrown> {
    let count = agent.get_property(scope, c"totalSocketCount")?;
    let count = f64::from_jsval_throwing(scope, count, ())?;
    agent.set_property(scope, c"totalSocketCount", count + delta)
}

/// `options[name]` if it is truthy, and `default` otherwise, as `options[name] || default`.
fn option_or<'s>(
    scope: &'s Scope<'_>,
    options: Object<'_>,
    name: &'static std::ffi::CStr,
    default: HandleValue<'s>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let value = options.get_property(scope, name)?;
    Ok(if js::conversion::to_boolean(value) {
        value
    } else {
        default
    })
}

/// Initialize the agent `this` with `options`, `default_port` and `protocol`, as Node.js's `Agent`
/// constructor does.
fn init_agent(
    scope: &Scope<'_>,
    this: Object<'_>,
    options: Option<HandleValue<'_>>,
    default_port: f64,
    protocol: &str,
) -> Result<(), Thrown> {
    crate::events::event_emitter::init(scope, this, None)?;
    let own = Object::new_with_null_prototype(scope)?;
    if let Some(options) = options.and_then(|options| Object::from_value(scope, *options).ok()) {
        own.assign(scope, options)?;
    }
    this.set_property(scope, c"options", own)?;
    let port = scope.root_value(js::value::from_f64(default_port));
    this.set_property(
        scope,
        c"defaultPort",
        option_or(scope, own, c"defaultPort", port)?,
    )?;
    let protocol = protocol.to_jsval_throwing(scope)?;
    this.set_property(
        scope,
        c"protocol",
        option_or(scope, own, c"protocol", protocol)?,
    )?;
    if own.get_property(scope, c"noDelay")?.is_undefined() {
        own.set_property(scope, c"noDelay", true)?;
    }
    own.set_property(scope, c"path", HandleValue::null())?;
    for name in [c"requests", c"sockets", c"freeSockets"] {
        this.set_property(scope, name, Object::new_with_null_prototype(scope)?)?;
    }
    let keep_alive_msecs = scope.root_value(js::value::from_f64(1000.0));
    this.set_property(
        scope,
        c"keepAliveMsecs",
        option_or(scope, own, c"keepAliveMsecs", keep_alive_msecs)?,
    )?;
    let no = scope.root_value(js::value::from_bool(false));
    this.set_property(
        scope,
        c"keepAlive",
        option_or(scope, own, c"keepAlive", no)?,
    )?;
    let max_sockets =
        agent_class::class_constructor(scope)?.get_property(scope, c"defaultMaxSockets")?;
    this.set_property(
        scope,
        c"maxSockets",
        option_or(scope, own, c"maxSockets", max_sockets)?,
    )?;
    let max_free = scope.root_value(js::value::from_f64(256.0));
    this.set_property(
        scope,
        c"maxFreeSockets",
        option_or(scope, own, c"maxFreeSockets", max_free)?,
    )?;
    let lifo = js::class::get_or_init_atom(scope, c"lifo")?;
    let scheduling = option_or(scope, own, c"scheduling", lifo)?;
    this.set_property(scope, c"scheduling", scheduling)?;
    let max_total = own.get_property(scope, c"maxTotalSockets")?;
    this.set_property(scope, c"maxTotalSockets", max_total)?;
    this.set_property(scope, c"totalSocketCount", 0)?;
    let buffer = own.get_property(scope, c"agentKeepAliveTimeoutBuffer")?;
    let buffer =
        if buffer.is_number() && buffer.to_number() >= 0.0 && buffer.to_number().is_finite() {
            buffer.to_number()
        } else {
            1000.0
        };
    this.set_property(scope, c"agentKeepAliveTimeoutBuffer", buffer)?;
    let valid = matches!(
        crate::support::rust_string(scope, scheduling).as_deref(),
        Some("fifo" | "lifo")
    ) && scheduling.is_string();
    if !valid {
        return Err(NodeError::invalid_arg_value(
            scope,
            "scheduling",
            scheduling,
            "must be one of: 'fifo', 'lifo'",
        )
        .into());
    }
    if max_total.is_undefined() {
        this.set_property(scope, c"maxTotalSockets", f64::INFINITY)?;
    } else {
        crate::errors::validate_number(scope, max_total, "maxTotalSockets", 1.0)?;
    }
    let on_free = Function::new_callback(scope, c"", 2, agent_on_free, this)?;
    crate::events::listen(scope, this, c"on", c"free", *on_free)?;
    let on_listener =
        crate::support::shared_named_function(scope, maybe_enable_keylog, c"maybeEnableKeylog", 1)?;
    crate::events::listen(scope, this, c"on", c"newListener", on_listener)?;
    Ok(())
}

/// The `newListener` listener of an agent, whose `this` is the agent: remove itself once a
/// `keylog` listener is added. The agent's sockets don't use TLS, so they emit no `keylog`.
fn maybe_enable_keylog(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if crate::support::is_str(scope, args.get(0), c"keylog")? {
        let agent = crate::support::this_object(scope, &args)?;
        let function = crate::support::shared_named_function(
            scope,
            maybe_enable_keylog,
            c"maybeEnableKeylog",
            1,
        )?;
        crate::events::listen(scope, agent, c"removeListener", c"newListener", function)?;
    }
    Ok(js::value::undefined())
}

/// The `free` listener of an agent, whose payload is the agent and whose arguments are a socket
/// and its options: give the socket to the next request waiting for the connection, or keep it in
/// `freeSockets` if the request kept the connection alive and the limits allow, and destroy it
/// otherwise.
fn agent_on_free(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let agent = js::function::cast_payload::<js::Object>(scope, payload);
    let socket = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    let options = Object::from_value(scope, args.get(1)).map_err(|e| e.throw(scope))?;
    let name = name_of(scope, agent, options)?;
    if !super::wire::socket_writable(scope, socket)? {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
        return Ok(js::value::undefined());
    }
    if let Some(requests) = pool_array(scope, agent, c"requests", &name)? {
        if requests.length(scope)? > 0 {
            let request = requests.shift(scope)?;
            let request = Object::from_value(scope, request).map_err(|e| e.throw(scope))?;
            set_request_socket(scope, agent, request, socket)?;
            if requests.length(scope)? == 0 {
                delete_pool_entry(scope, agent, c"requests", &name)?;
            }
            return Ok(js::value::undefined());
        }
    }
    let request = Object::from_value(scope, *socket.get_property(scope, c"_httpMessage")?).ok();
    let reusable = match request {
        Some(request) => {
            crate::http::flag(scope, request, c"shouldKeepAlive")?
                && crate::http::flag(scope, agent, c"keepAlive")?
        }
        None => false,
    };
    if !reusable {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
        return Ok(js::value::undefined());
    }
    let free = pool_array(scope, agent, c"freeSockets", &name)?;
    let free_count = match free {
        Some(free) => free.length(scope)?,
        None => 0,
    };
    let mut count = f64::from(free_count);
    if let Some(sockets) = pool_array(scope, agent, c"sockets", &name)? {
        count += f64::from(sockets.length(scope)?);
    }
    let over = number(scope, agent, c"totalSocketCount", 0.0)?
        > number(scope, agent, c"maxTotalSockets", f64::INFINITY)?
        || count > number(scope, agent, c"maxSockets", f64::INFINITY)?
        || f64::from(free_count) >= number(scope, agent, c"maxFreeSockets", 256.0)?;
    let keep = !over
        && js::conversion::to_boolean(Function::call_by_name(
            scope,
            agent.handle(),
            c"keepSocketAlive",
            &[socket.to_jsval_throwing(scope)?],
        )?);
    if !keep {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
        return Ok(js::value::undefined());
    }
    let free = pool_array_or_new(scope, agent, c"freeSockets", &name)?;
    socket.set_property(scope, c"_httpMessage", HandleValue::null())?;
    Function::call_by_name(
        scope,
        agent.handle(),
        c"removeSocket",
        &[
            socket.to_jsval_throwing(scope)?,
            options.to_jsval_throwing(scope)?,
        ],
    )?;
    let on_error = crate::support::shared_named_function(
        scope,
        free_socket_error_listener,
        c"freeSocketErrorListener",
        1,
    )?;
    crate::events::listen(scope, socket, c"once", c"error", on_error)?;
    install_data_guard(scope, socket)?;
    free.push(scope, socket)?;
    Ok(js::value::undefined())
}

/// The `error` listener of a free socket, whose `this` is the socket: destroy the socket and
/// remove it from its agent.
fn free_socket_error_listener(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
    crate::events::emit(scope, socket, c"agentRemove", &[])?;
    Ok(js::value::undefined())
}

/// Destroy the free socket `socket` if it buffers data, and otherwise destroy it once it reads
/// data or the end of the connection, as Node.js's `installFreeSocketDataGuard` does. A
/// connecting socket gets the guard once it connected. Only `net.Socket`s get the guard.
fn install_data_guard(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    let buffered = socket.get_property(scope, c"readableLength")?;
    if buffered.is_number() && buffered.to_number() > 0.0 {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
        return Ok(());
    }
    if crate::http::flag(scope, socket, c"connecting")? {
        let on_connect =
            crate::support::shared_named_function(scope, guard_on_connect, c"onConnect", 0)?;
        return crate::events::listen(scope, socket, c"once", c"connect", on_connect);
    }
    crate::net::socket::guard_reads(scope, socket, true)
}

/// The `connect` listener of a free socket that was connecting, whose `this` is the socket.
fn guard_on_connect(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    install_data_guard(scope, crate::support::this_object(scope, &args)?)?;
    Ok(js::value::undefined())
}

/// Remove the guard [`install_data_guard`] gave `socket`.
fn remove_data_guard(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    let on_connect =
        crate::support::shared_named_function(scope, guard_on_connect, c"onConnect", 0)?;
    crate::events::listen(scope, socket, c"removeListener", c"connect", on_connect)?;
    crate::net::socket::guard_reads(scope, socket, false)
}

/// Give `request` `socket`, as Node.js's `setRequestSocket` does: through the request's
/// `onSocket`, with the request's own timeout on the socket if it differs from the agent's.
fn set_request_socket(
    scope: &Scope<'_>,
    agent: Object<'_>,
    request: Object<'_>,
    socket: Object<'_>,
) -> Result<(), ExnThrown> {
    Function::call_by_name(
        scope,
        request.handle(),
        c"onSocket",
        &[socket.to_jsval_throwing(scope)?],
    )?;
    let agent_timeout = match object_property(scope, agent, c"options")? {
        Some(options) => {
            let timeout = options.get_property(scope, c"timeout")?;
            if js::conversion::to_boolean(timeout) {
                timeout
            } else {
                scope.root_value(js::value::from_f64(0.0))
            }
        }
        None => scope.root_value(js::value::from_f64(0.0)),
    };
    let timeout = request.get_property(scope, c"timeout")?;
    if timeout.is_undefined() || js::comparison::same_value(scope, timeout, agent_timeout)? {
        return Ok(());
    }
    Function::call_by_name(scope, socket.handle(), c"setTimeout", &[timeout])?;
    Ok(())
}

/// Listen to `free`, `close`, `timeout` and `agentRemove` on the socket `socket` the agent created
/// with `options`, as Node.js's `installListeners` does.
fn install_listeners(
    scope: &Scope<'_>,
    agent: Object<'_>,
    socket: Object<'_>,
    options: Object<'_>,
) -> Result<(), ExnThrown> {
    let state = js::class::create_instance_with::<AgentSocketImpl>(scope, |_| AgentSocketImpl {
        agent: Heap::from(agent),
        socket: Heap::from(socket),
        options: Heap::from(options),
        listeners: Vec::new(),
    })?;
    for (event, body) in SOCKET_EVENTS {
        let listener = Function::new_callback(scope, c"", 0, body, state)?;
        crate::events::listen(scope, socket, c"on", event, *listener)?;
        state.data_mut().listeners.push(Heap::from(*listener));
    }
    Ok(())
}

/// The agent, socket and options of the payload `payload` of a socket listener.
fn agent_socket<'s>(
    scope: &'s Scope<'_>,
    payload: HandleValue<'_>,
) -> (AgentSocket<'s>, Object<'s>, Object<'s>, Object<'s>) {
    let state = js::function::cast_payload::<AgentSocket>(scope, payload);
    let data = state.data();
    let (agent, socket, options) = (
        data.agent.get(scope),
        data.socket.get(scope),
        data.options.get(scope),
    );
    drop(data);
    (state, agent, socket, options)
}

/// The `free` listener of a socket of an agent: emit `free` on the agent with the socket and its
/// options.
fn socket_on_free(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (_, agent, socket, options) = agent_socket(scope, payload);
    crate::events::emit(
        scope,
        agent,
        c"free",
        &[
            socket.to_jsval_throwing(scope)?,
            options.to_jsval_throwing(scope)?,
        ],
    )?;
    Ok(js::value::undefined())
}

/// The `close` listener of a socket of an agent: count one socket less and remove it.
fn socket_on_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (_, agent, socket, options) = agent_socket(scope, payload);
    count_sockets(scope, agent, -1.0)?;
    Function::call_by_name(
        scope,
        agent.handle(),
        c"removeSocket",
        &[
            socket.to_jsval_throwing(scope)?,
            options.to_jsval_throwing(scope)?,
        ],
    )?;
    Ok(js::value::undefined())
}

/// The `timeout` listener of a socket of an agent: destroy the socket if it is free.
fn socket_on_timeout(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (_, agent, socket, _) = agent_socket(scope, payload);
    let Some(pool) = object_property(scope, agent, c"freeSockets")? else {
        return Ok(js::value::undefined());
    };
    for key in pool.own_enumerable_keys(scope)? {
        let sockets = crate::http::get_by_key(scope, pool, key)?;
        let Some(sockets) = super::outgoing::as_array(scope, sockets)? else {
            continue;
        };
        for entry in sockets.to_vec(scope)? {
            if entry.is_object() && entry.get().to_object() == socket.handle().get() {
                Function::call_by_name(
                    scope,
                    socket.handle(),
                    c"destroy",
                    js::function::EmptyArgs,
                )?;
                return Ok(js::value::undefined());
            }
        }
    }
    Ok(js::value::undefined())
}

/// The `agentRemove` listener of a socket of an agent: count one socket less, remove it, and
/// remove the agent's listeners from it.
fn socket_on_remove(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (state, agent, socket, options) = agent_socket(scope, payload);
    count_sockets(scope, agent, -1.0)?;
    Function::call_by_name(
        scope,
        agent.handle(),
        c"removeSocket",
        &[
            socket.to_jsval_throwing(scope)?,
            options.to_jsval_throwing(scope)?,
        ],
    )?;
    let listeners: Vec<Object<'_>> = state
        .data()
        .listeners
        .iter()
        .map(|listener| listener.get(scope))
        .collect();
    // Node.js removes `close` first.
    for index in [1, 0, 2, 3] {
        if let Some(listener) = listeners.get(index) {
            let event = SOCKET_EVENTS[index].0;
            crate::events::listen(scope, socket, c"removeListener", event, *listener)?;
        }
    }
    Ok(js::value::undefined())
}

/// Create a socket for `request` with `options` through the agent's `createSocket`, which calls
/// `done` with the payload `request` and `(error, socket)`.
fn create_socket_for(
    scope: &Scope<'_>,
    agent: Object<'_>,
    request: Object<'_>,
    options: Object<'_>,
    done: js::function::Callback,
) -> Result<(), ExnThrown> {
    let callback = Function::new_callback(scope, c"", 2, done, request)?;
    Function::call_by_name(
        scope,
        agent.handle(),
        c"createSocket",
        &[
            request.to_jsval_throwing(scope)?,
            options.to_jsval_throwing(scope)?,
            (*callback).to_jsval_throwing(scope)?,
        ],
    )?;
    Ok(())
}

/// The callback of the `createSocket` of `addRequest`, whose payload is the request: fail the
/// request with the error, or give it the socket.
fn added_socket(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<js::Object>(scope, payload);
    let (error, socket) = (args.get(0), args.get(1));
    if js::conversion::to_boolean(error) {
        Function::call_by_name(scope, request.handle(), c"onSocket", &[socket, error])?;
        return Ok(js::value::undefined());
    }
    let agent = Object::from_value(scope, *request.get_property(scope, c"agent")?)
        .map_err(|e| e.throw(scope))?;
    let socket = Object::from_value(scope, *socket).map_err(|e| e.throw(scope))?;
    set_request_socket(scope, agent, request, socket)?;
    Ok(js::value::undefined())
}

/// The callback of the `createSocket` of `removeSocket`, whose payload is the request: fail the
/// request with the error, or emit `free` on the socket, which gives it to the next waiting
/// request.
fn replaced_socket(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<js::Object>(scope, payload);
    let (error, socket) = (args.get(0), args.get(1));
    if js::conversion::to_boolean(error) {
        Function::call_by_name(
            scope,
            request.handle(),
            c"onSocket",
            &[HandleValue::null(), error],
        )?;
        return Ok(js::value::undefined());
    }
    let socket = Object::from_value(scope, *socket).map_err(|e| e.throw(scope))?;
    crate::events::emit(scope, socket, c"free", &[])?;
    Ok(js::value::undefined())
}

/// The callback `createSocket` passes to `createConnection`, whose payload is the
/// [`SocketRequest`]: the first call adds the socket to the agent's `sockets`, listens to it and
/// reports it to the callback of `createSocket`, or reports the error.
fn socket_created(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let creation = js::function::cast_payload::<SocketRequest>(scope, payload);
    socket_created_with(scope, creation, args.get(0), args.get(1))?;
    Ok(js::value::undefined())
}

/// The body of [`socket_created`].
fn socket_created_with(
    scope: &Scope<'_>,
    creation: SocketRequest<'_>,
    error: HandleValue<'_>,
    socket: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let (agent, options, name, callback) = {
        let mut data = creation.data_mut();
        if std::mem::replace(&mut data.created, true) {
            return Ok(());
        }
        (
            data.agent.get(scope),
            data.options.as_ref().map(|options| options.get(scope)),
            data.name.clone(),
            data.callback.as_ref().map(|callback| callback.get(scope)),
        )
    };
    let callback = callback.unwrap_or(HandleValue::undefined());
    if js::conversion::to_boolean(error) {
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        return Ok(());
    }
    let socket_object = Object::from_value(scope, *socket).map_err(|e| e.throw(scope))?;
    pool_array_or_new(scope, agent, c"sockets", &name)?.push(scope, socket_object)?;
    count_sockets(scope, agent, 1.0)?;
    if let Some(options) = options {
        install_listeners(scope, agent, socket_object, options)?;
    }
    Function::call(
        scope,
        HandleValue::undefined(),
        callback,
        &[HandleValue::null(), socket],
    )?;
    Ok(())
}

#[core_runtime::jsfunction_class(
    name = "Agent",
    extends = "crate::events::event_emitter",
    self_construct
)]
pub mod agent_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpagent>: an agent with `options`, which keeps
    /// connections alive with `keepAlive` and limits them with `maxSockets`, `maxFreeSockets`
    /// and `maxTotalSockets`. `proxyEnv` has no effect.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_agent(scope, this, options, 80.0, "http:")
    }

    /// <https://nodejs.org/api/http.html#agentcreateconnectionoptions-callback>: a new
    /// `net.Socket` connected with `args`, as `net.createConnection` creates it.
    #[method(length = 0)]
    pub fn create_connection<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        crate::net::connect(scope, &args)
    }

    /// <https://nodejs.org/api/http.html#agentgetnameoptions>: the host, port and local address
    /// of `options`, and the address family and socket path if they are given, separated by
    /// `:`.
    #[method(length = 0)]
    pub fn get_name(
        scope: &Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<String, ExnThrown> {
        let Some(options) = options
            .filter(|options| !options.is_undefined())
            .and_then(|options| Object::from_value(scope, *options).ok())
        else {
            return Ok("localhost::".into());
        };
        let text = |name: &'static std::ffi::CStr| -> Result<Option<String>, ExnThrown> {
            let value = options.get_property(scope, name)?;
            if !js::conversion::to_boolean(value) {
                return Ok(None);
            }
            Ok(Some(String::from_jsval_throwing(scope, value, ())?))
        };
        let mut name = text(c"host")?.unwrap_or_else(|| "localhost".into());
        name.push(':');
        if let Some(port) = text(c"port")? {
            name.push_str(&port);
        }
        name.push(':');
        if let Some(address) = text(c"localAddress")? {
            name.push_str(&address);
        }
        let family = options.get_property(scope, c"family")?;
        if family.is_number() && matches!(family.to_number(), 4.0 | 6.0) {
            name.push_str(&format!(":{}", family.to_number()));
        }
        if let Some(path) = text(c"socketPath")? {
            name.push(':');
            name.push_str(&path);
        }
        Ok(name)
    }

    /// <https://nodejs.org/api/http.html#agentaddrequestreq-options>: give `request` a free
    /// socket for its connection, or a new one while the agent has fewer than `maxSockets` for
    /// the connection and fewer than `maxTotalSockets` in all, and queue it in `requests`
    /// otherwise. `options` can be a host followed by the port and the local address.
    #[method(length = 4)]
    pub fn add_request(
        scope: &Scope<'_>,
        this: Object<'_>,
        request: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
        port: Option<HandleValue<'_>>,
        local_address: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let request = Object::from_value(scope, *request)?;
        let options = options.unwrap_or(HandleValue::undefined());
        let options = if options.is_string() {
            let legacy = Object::new_with_null_prototype(scope)?;
            legacy.set_property(scope, c"host", options)?;
            legacy.set_property(scope, c"port", port.unwrap_or(HandleValue::undefined()))?;
            legacy.set_property(
                scope,
                c"localAddress",
                local_address.unwrap_or(HandleValue::undefined()),
            )?;
            legacy.to_jsval_throwing(scope)?
        } else {
            options
        };
        let options = merged_options(scope, this, options)?;
        normalize_server_name(scope, options, request)?;
        let name = name_of(scope, this, options)?;
        let sockets = pool_array_or_new(scope, this, c"sockets", &name)?;
        let free = pool_array(scope, this, c"freeSockets", &name)?;
        let mut socket = None;
        if let Some(free) = free {
            while free.length(scope)? > 0 {
                let first = Object::from_value(scope, *free.get_element(scope, 0)?)?;
                if !super::super::wire::socket_destroyed(scope, first)? {
                    break;
                }
                free.shift(scope)?;
            }
            let fifo =
                crate::support::is_str(scope, this.get_property(scope, c"scheduling")?, c"fifo")?;
            let taken = if fifo {
                free.shift(scope)?
            } else {
                free.pop(scope)?
            };
            socket = Object::from_value(scope, *taken).ok();
            if free.length(scope)? == 0 {
                delete_pool_entry(scope, this, c"freeSockets", &name)?;
            }
        }
        let free_count = match free {
            Some(free) => f64::from(free.length(scope)?),
            None => 0.0,
        };
        let count = free_count + f64::from(sockets.length(scope)?);
        let high_water_mark = options.get_property(scope, c"highWaterMark")?;
        if let Some(socket) = socket {
            if !high_water_mark.is_null_or_undefined() {
                let current = socket.get_property(scope, c"writableHighWaterMark")?;
                if !js::comparison::same_value(scope, current, high_water_mark)? {
                    if let Ok(state) =
                        Object::from_value(scope, *socket.get_property(scope, c"_writableState")?)
                    {
                        state.set_property(scope, c"highWaterMark", high_water_mark)?;
                    }
                }
            }
            Function::call_by_name(
                scope,
                this.handle(),
                c"reuseSocket",
                &[
                    socket.to_jsval_throwing(scope)?,
                    request.to_jsval_throwing(scope)?,
                ],
            )?;
            set_request_socket(scope, this, request, socket)?;
            sockets.push(scope, socket)?;
        } else if count < number(scope, this, c"maxSockets", f64::INFINITY)?
            && number(scope, this, c"totalSocketCount", 0.0)?
                < number(scope, this, c"maxTotalSockets", f64::INFINITY)?
        {
            create_socket_for(scope, this, request, options, added_socket)?;
        } else {
            if let Some(state) = super::super::outgoing::try_state_of(scope, request)? {
                with_client(&state, |client| {
                    client.agent_options = Some(Heap::from(options))
                });
            }
            pool_array_or_new(scope, this, c"requests", &name)?.push(scope, request)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#agentcreatesocketreq-options-callback>: a new socket
    /// for `request` from `createConnection`, with the request's timeout or the agent's, which
    /// the agent adds to `sockets` and listens to before it calls `callback` with it.
    #[method(length = 3)]
    pub fn create_socket(
        scope: &Scope<'_>,
        this: Object<'_>,
        request: HandleValue<'_>,
        options: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let request = Object::from_value(scope, *request)?;
        let options = merged_options(scope, this, options)?;
        normalize_server_name(scope, options, request)?;
        let timeout = request.get_property(scope, c"timeout")?;
        let timeout = if js::conversion::to_boolean(timeout) {
            timeout
        } else {
            match object_property(scope, this, c"options")? {
                Some(own) => own.get_property(scope, c"timeout")?,
                None => HandleValue::undefined(),
            }
        };
        if js::conversion::to_boolean(timeout) {
            options.set_property(scope, c"timeout", timeout)?;
        }
        let name = name_of(scope, this, options)?;
        options.set_property(scope, c"_agentKey", name.as_str())?;
        options.set_property(scope, c"encoding", HandleValue::null())?;
        let creation =
            js::class::create_instance_with::<SocketRequestImpl>(scope, |_| SocketRequestImpl {
                agent: Heap::from(this),
                request: Heap::from(request),
                options: Some(Heap::from(options)),
                name,
                callback: Some(Heap::from(callback.get())),
                created: false,
            })?;
        let created = Function::new_callback(scope, c"", 2, socket_created, creation)?;
        if crate::http::flag(scope, this, c"keepAlive")? {
            options.set_property(scope, c"keepAlive", this.get_property(scope, c"keepAlive")?)?;
            options.set_property(
                scope,
                c"keepAliveInitialDelay",
                this.get_property(scope, c"keepAliveMsecs")?,
            )?;
        }
        let socket = Function::call_by_name(
            scope,
            this.handle(),
            c"createConnection",
            &[
                options.to_jsval_throwing(scope)?,
                (*created).to_jsval_throwing(scope)?,
            ],
        )?;
        if js::conversion::to_boolean(socket) {
            socket_created_with(scope, creation, HandleValue::null(), socket)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#agentremovesocketsocket-options>: remove `socket` from
    /// `sockets`, and from `freeSockets` if it isn't writable, then create a socket for the first
    /// request waiting for the connection, or for another connection without sockets.
    #[method(length = 2)]
    pub fn remove_socket(
        scope: &Scope<'_>,
        this: Object<'_>,
        socket: HandleValue<'_>,
        options: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let socket = Object::from_value(scope, *socket)?;
        let options = Object::from_value(scope, *options)?;
        let name = name_of(scope, this, options)?;
        let mut pools = vec![c"sockets"];
        if !super::super::wire::socket_writable(scope, socket)? {
            pools.push(c"freeSockets");
        }
        for pool in pools {
            if let Some(sockets) = pool_array(scope, this, pool, &name)? {
                let entries = sockets.to_vec(scope)?;
                let index = entries.iter().position(|entry| {
                    entry.is_object() && entry.get().to_object() == socket.handle().get()
                });
                if let Some(index) = index {
                    sockets.remove(scope, index as u32, 1)?;
                    if sockets.length(scope)? == 0 {
                        delete_pool_entry(scope, this, pool, &name)?;
                    }
                }
            }
        }
        let mut waiting = None;
        let mut options = Some(options);
        if let Some(requests) = pool_array(scope, this, c"requests", &name)? {
            if requests.length(scope)? > 0 {
                waiting = Object::from_value(scope, *requests.get_element(scope, 0)?).ok();
            }
        }
        if waiting.is_none() {
            if let Some(requests) = object_property(scope, this, c"requests")? {
                if let Some(key) = requests.own_enumerable_keys(scope)?.first() {
                    let key = *key;
                    let name = String::from_jsval_throwing(scope, key, ())?;
                    let busy = match pool_array(scope, this, c"sockets", &name)? {
                        Some(sockets) => sockets.length(scope)? > 0,
                        None => false,
                    };
                    if !busy {
                        if let Some(queue) = super::super::outgoing::as_array(
                            scope,
                            crate::http::get_by_key(scope, requests, key)?,
                        )? {
                            waiting = Object::from_value(scope, *queue.get_element(scope, 0)?).ok();
                            options = match waiting {
                                Some(request) => {
                                    match super::super::outgoing::try_state_of(scope, request)? {
                                        Some(state) => with_client(&state, |client| {
                                            client
                                                .agent_options
                                                .as_ref()
                                                .map(|options| options.get(scope))
                                        })
                                        .flatten(),
                                        None => None,
                                    }
                                }
                                None => None,
                            };
                        }
                    }
                }
            }
        }
        if let (Some(request), Some(options)) = (waiting, options) {
            if let Some(state) = super::super::outgoing::try_state_of(scope, request)? {
                with_client(&state, |client| client.agent_options = None);
            }
            create_socket_for(scope, this, request, options, replaced_socket)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#agentkeepsocketalivesocket>: keep `socket` alive for
    /// the next request, without keeping the event loop alive, with the agent's timeout, or a
    /// second less than the `Keep-Alive` timeout of the response if that is shorter. Returns
    /// `false` if that leaves no time.
    #[method(length = 1)]
    pub fn keep_socket_alive(
        scope: &Scope<'_>,
        this: Object<'_>,
        socket: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let socket = Object::from_value(scope, *socket)?;
        let delay = this.get_property(scope, c"keepAliveMsecs")?;
        Function::call_by_name(
            scope,
            socket.handle(),
            c"setKeepAlive",
            &[scope.root_value(js::value::from_bool(true)), delay],
        )?;
        Function::call_by_name(scope, socket.handle(), c"unref", js::function::EmptyArgs)?;
        let zero = scope.root_value(js::value::from_f64(0.0));
        let mut timeout = match object_property(scope, this, c"options")? {
            Some(own) => {
                let timeout = own.get_property(scope, c"timeout")?;
                if js::conversion::to_boolean(timeout) {
                    timeout
                } else {
                    zero
                }
            }
            None => zero,
        };
        let mut keep = true;
        let hint = match Object::from_value(scope, *socket.get_property(scope, c"_httpMessage")?) {
            Ok(request) => match object_property(scope, request, c"res")? {
                Some(response) => match object_property(scope, response, c"headers")? {
                    Some(headers) => crate::support::rust_string(
                        scope,
                        crate::http::get_by_key(
                            scope,
                            headers,
                            js::class::get_or_init_atom(scope, c"keep-alive")?,
                        )?,
                    ),
                    None => None,
                },
                None => None,
            },
            Err(_) => None,
        };
        if let Some(hint) = hint.filter(|hint| !hint.is_empty()) {
            let digits: String = hint
                .strip_prefix("timeout=")
                .unwrap_or_default()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(seconds) = digits.parse::<f64>() {
                let buffer = number(scope, this, c"agentKeepAliveTimeoutBuffer", 1000.0)?;
                let server = (seconds * 1000.0 - buffer).max(0.0);
                if server == 0.0 {
                    keep = false;
                } else if server < f64::from_jsval_throwing(scope, timeout, ())? {
                    timeout = scope.root_value(js::value::from_f64(server));
                }
            }
        }
        let current = socket.get_property(scope, c"timeout")?;
        if !js::comparison::strictly_equal(scope, current, timeout)? {
            Function::call_by_name(scope, socket.handle(), c"setTimeout", &[timeout])?;
        }
        Ok(keep)
    }

    /// <https://nodejs.org/api/http.html#agentreusesocketsocket-request>: take `socket` out of
    /// the free state for `request`, which gets `reusedSocket`, and keep the event loop alive
    /// for it again.
    #[method(length = 2)]
    pub fn reuse_socket(
        scope: &Scope<'_>,
        socket: HandleValue<'_>,
        request: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let socket = Object::from_value(scope, *socket)?;
        let request = Object::from_value(scope, *request)?;
        let on_error = crate::support::shared_named_function(
            scope,
            free_socket_error_listener,
            c"freeSocketErrorListener",
            1,
        )?;
        crate::events::listen(scope, socket, c"removeListener", c"error", on_error)?;
        remove_data_guard(scope, socket)?;
        request.set_property(scope, c"reusedSocket", true)?;
        Function::call_by_name(scope, socket.handle(), c"ref", js::function::EmptyArgs)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#agentdestroy>: destroy the free sockets and the sockets
    /// in use.
    pub fn destroy(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        for pool in [c"freeSockets", c"sockets"] {
            let Some(pool) = object_property(scope, this, pool)? else {
                continue;
            };
            for key in pool.own_enumerable_keys(scope)? {
                let sockets = crate::http::get_by_key(scope, pool, key)?;
                let Some(sockets) = super::super::outgoing::as_array(scope, sockets)? else {
                    continue;
                };
                let mut index = 0;
                while index < sockets.length(scope)? {
                    let socket = sockets.get_element(scope, index)?;
                    if let Ok(socket) = Object::from_value(scope, *socket) {
                        Function::call_by_name(
                            scope,
                            socket.handle(),
                            c"destroy",
                            js::function::EmptyArgs,
                        )?;
                    }
                    index += 1;
                }
            }
        }
        Ok(())
    }

    /// Define `Agent.defaultMaxSockets`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, _proto: Object<'_>) -> Result<(), ExnThrown> {
        ctor.set_property(scope, c"defaultMaxSockets", f64::INFINITY)
    }
}

#[core_runtime::jsfunction_class(name = "Agent", extends = "super::agent_class", self_construct)]
pub mod https_agent_class {
    use super::*;

    /// <https://nodejs.org/api/https.html#class-httpsagent>: an `http.Agent` for `https:`,
    /// with port 443. Its requests go through `platform::http`, which manages TLS sessions, so
    /// `maxCachedSessions` has no effect.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_agent(scope, this, options, 80.0, "http:")?;
        this.set_property(scope, c"defaultPort", 443)?;
        this.set_property(scope, c"protocol", "https:")?;
        let own = object_property(scope, this, c"options")?;
        let cached = match own {
            Some(own) => own.get_property(scope, c"maxCachedSessions")?,
            None => HandleValue::undefined(),
        };
        if cached.is_undefined() {
            this.set_property(scope, c"maxCachedSessions", 100)?;
        } else {
            this.set_property(scope, c"maxCachedSessions", cached)?;
        }
        let cache = Object::new_plain(scope)?;
        cache.set_property(scope, c"map", Object::new_plain(scope)?)?;
        cache.set_property(scope, c"list", *js::Array::new(scope, 0)?)?;
        this.set_property(scope, c"_sessionCache", cache)?;
        Ok(())
    }
}
