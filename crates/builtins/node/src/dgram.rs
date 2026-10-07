// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:dgram`: UDP sockets over `platform::net::UdpSocket`.
//! See <https://nodejs.org/api/dgram.html>.
//!
//! A socket's state is a [`DgramState`]: its OS socket once bound, its bind and connect states,
//! the operations waiting for the bind in progress, the sends the OS socket hasn't taken yet, and
//! the receive in progress. The OS socket is created when the socket binds, explicitly or for the
//! first `send` or `connect`. Every address goes through the socket's `lookup` function, which
//! is `dns.lookup` unless the `lookup` option replaces it.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::rc::Rc;
use std::task::Poll;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};
use platform::net::{MulticastInterface, NetError, UdpBindOptions, UdpSocket};

use crate::errors::{validate_function, validate_string, ErrorKind, NodeError, Thrown};
use crate::fs::UvError;
use crate::net::socket::address_object;
use crate::support::{spawn_cancellable, spawn_cancellable_with_ref, until_cancelled, Canceller};

/// The size of the largest datagram, which bounds the bytes a send allocates.
const MAX_DATAGRAM: usize = 65535;

/// Where a socket is in binding, as Node.js's `bindState` tracks it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum BindState {
    #[default]
    Unbound,
    Binding,
    Bound,
}

/// Where a socket is in connecting, as Node.js's `connectState` tracks it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum ConnectState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
}

/// A range of the bytes of an `ArrayBufferView`, which a send sends.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct Chunk {
    view: Heap<js::object::Object>,
    #[no_trace]
    start: usize,
    #[no_trace]
    length: usize,
}

/// A send whose arguments were validated: its chunks, the port for a socket that isn't connected,
/// the address as given, which is `undefined` or a string, and the callback.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct SendArgs {
    chunks: Vec<Chunk>,
    #[no_trace]
    port: Option<u16>,
    address: Heap<Value>,
    callback: Option<Heap<Value>>,
}

/// A [`Chunk`] rooted in a scope.
#[derive(Clone, Copy)]
struct ChunkRef<'s> {
    view: Object<'s>,
    start: usize,
    length: usize,
}

/// [`SendArgs`] rooted in a scope.
struct SendRef<'s> {
    chunks: Vec<ChunkRef<'s>>,
    port: Option<u16>,
    address: HandleValue<'s>,
    callback: Option<HandleValue<'s>>,
}

impl SendArgs {
    /// These arguments, rooted in `scope`.
    fn root<'s>(&self, scope: &'s Scope<'_>) -> SendRef<'s> {
        SendRef {
            chunks: self
                .chunks
                .iter()
                .map(|chunk| ChunkRef {
                    view: chunk.view.get(scope),
                    start: chunk.start,
                    length: chunk.length,
                })
                .collect(),
            port: self.port,
            address: self.address.get(scope),
            callback: rooted(scope, &self.callback),
        }
    }
}

/// The [`SendArgs`] of the [`SendRef`] `$send`, for a traced container to hold.
macro_rules! traced_send {
    ($send:expr) => {
        SendArgs {
            chunks: $send
                .chunks
                .iter()
                .map(|chunk| Chunk {
                    view: Heap::from(chunk.view),
                    start: chunk.start,
                    length: chunk.length,
                })
                .collect(),
            port: $send.port,
            address: Heap::from($send.address.get()),
            callback: $send.callback.map(|callback| Heap::from(callback.get())),
        }
    };
}

/// An operation waiting for the bind in progress, as Node.js's `enqueue` keeps it.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) enum Queued {
    Send(SendArgs),
    Connect {
        #[no_trace]
        port: u16,
        address: Heap<Value>,
        callback: Option<Heap<Value>>,
    },
    Close,
}

/// A [`Queued`] operation rooted in a scope.
enum QueuedRef<'s> {
    Send(SendRef<'s>),
    Connect {
        port: u16,
        address: HandleValue<'s>,
        callback: Option<HandleValue<'s>>,
    },
    Close,
}

impl Queued {
    /// The operation, rooted in `scope`.
    fn root<'s>(&self, scope: &'s Scope<'_>) -> QueuedRef<'s> {
        match self {
            Queued::Send(send) => QueuedRef::Send(send.root(scope)),
            Queued::Connect {
                port,
                address,
                callback,
            } => QueuedRef::Connect {
                port: *port,
                address: address.get(scope),
                callback: rooted(scope, callback),
            },
            Queued::Close => QueuedRef::Close,
        }
    }
}

/// A send the OS socket hasn't taken yet: its bytes, which the send in progress holds, their
/// number, the destination, the address and port as given, and the callback.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct OutgoingSend {
    #[no_trace]
    bytes: Option<Vec<u8>>,
    #[no_trace]
    length: usize,
    #[no_trace]
    to: Option<SocketAddr>,
    #[no_trace]
    address: Option<String>,
    #[no_trace]
    port: u16,
    callback: Option<Heap<Value>>,
}

/// The state of a `dgram.Socket`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DgramState {
    /// Whether the socket is a `udp6` socket.
    #[no_trace]
    ipv6: bool,
    /// Whether the socket wasn't closed, which Node.js tracks as a handle.
    #[no_trace]
    open: bool,
    /// The OS socket, once bound, which the receive and send in progress poll too. Closing the
    /// socket drops it.
    #[no_trace]
    link: Rc<RefCell<Option<UdpSocket>>>,
    #[no_trace]
    receiving: bool,
    #[no_trace]
    bind_state: BindState,
    #[no_trace]
    connect_state: ConnectState,
    /// The operations waiting for the bind in progress, while there are any.
    queue: Option<Vec<Queued>>,
    #[no_trace]
    bind_options: UdpBindOptions,
    #[no_trace]
    recv_buffer_size: u32,
    #[no_trace]
    send_buffer_size: u32,
    receive_block_list: Option<Heap<js::object::Object>>,
    send_block_list: Option<Heap<js::object::Object>>,
    /// The `lookup` option, or `None` for `dns.lookup`.
    lookup: Option<Heap<js::object::Object>>,
    /// The value of the deprecated `_handle`: an object while the socket is open, unless replaced.
    handle: Option<Heap<js::object::Object>>,
    #[no_trace]
    cancel_receive: Option<Canceller>,
    /// Whether the receive in progress keeps the event loop alive, which `ref` and `unref` set.
    #[no_trace]
    referenced: js::promise::FutureRef,
    /// The sends the OS socket hasn't taken yet, in order. The first is in progress while
    /// `cancel_send` is set.
    sends: VecDeque<OutgoingSend>,
    #[no_trace]
    cancel_send: Option<Canceller>,
}

#[core_runtime::jsmethods]
impl DgramState {}

/// The state of a `bind` callback: the callback, and the `listening` and `errorMonitor`
/// listeners that call it and remove each other.
#[core_runtime::jsclass(hidden)]
pub(crate) struct BindCallback {
    callback: Heap<Value>,
    on_listening: Option<Heap<js::object::Object>>,
    remove: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl BindCallback {}

/// What an address lookup is for, and the arguments it continues with.
#[js::must_root]
#[derive(core_runtime::Traceable)]
enum LookupPurpose {
    Bind {
        port: Heap<Value>,
    },
    Connect {
        #[no_trace]
        port: u16,
        #[no_trace]
        address: String,
        callback: Option<Heap<Value>>,
    },
    Send(SendArgs),
}

/// A [`LookupPurpose`] rooted in a scope.
enum PurposeRef<'s> {
    Bind {
        port: HandleValue<'s>,
    },
    Connect {
        port: u16,
        address: String,
        callback: Option<HandleValue<'s>>,
    },
    Send(SendRef<'s>),
}

impl LookupPurpose {
    /// The purpose, rooted in `scope`.
    fn root<'s>(&self, scope: &'s Scope<'_>) -> PurposeRef<'s> {
        match self {
            LookupPurpose::Bind { port } => PurposeRef::Bind {
                port: port.get(scope),
            },
            LookupPurpose::Connect {
                port,
                address,
                callback,
            } => PurposeRef::Connect {
                port: *port,
                address: address.clone(),
                callback: rooted(scope, callback),
            },
            LookupPurpose::Send(send) => PurposeRef::Send(send.root(scope)),
        }
    }
}

/// The payload of the callback of an address lookup: the socket, and what the lookup is for.
#[core_runtime::jsclass(hidden)]
pub(crate) struct LookupOp {
    socket: Heap<js::object::Object>,
    purpose: Option<LookupPurpose>,
}

#[core_runtime::jsmethods]
impl LookupOp {}

/// The payload of the listener that removes the `signal` option's `abort` listener once the
/// socket or server closed.
#[core_runtime::jsclass(hidden)]
pub(crate) struct SignalLink {
    signal: Heap<js::object::Object>,
    listener: Heap<js::object::Object>,
}

#[core_runtime::jsmethods]
impl SignalLink {}

/// The value `heap` holds, if any, rooted.
fn rooted<'s>(scope: &'s Scope<'_>, heap: &Option<Heap<Value>>) -> Option<HandleValue<'s>> {
    heap.as_ref().map(|heap| heap.get(scope))
}

/// `ERR_SOCKET_DGRAM_NOT_RUNNING`.
fn not_running() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_SOCKET_DGRAM_NOT_RUNNING",
        "Not running",
    )
}

/// Throw the `TypeError` Node.js throws when it calls `method` on the handle of a closed socket.
fn null_handle(scope: &Scope<'_>, method: &str) -> Thrown {
    let message = format!("Cannot read properties of null (reading '{method}')");
    js::error::TypeError(message).throw(scope);
    Thrown::Pending
}

/// Throw Node.js's `ErrnoException` for `error` of `syscall`.
fn throw_net(scope: &Scope<'_>, error: &NetError, syscall: &'static str) -> Thrown {
    UvError::net(error, syscall).throw(scope);
    Thrown::Pending
}

/// Throw Node.js's `ErrnoException` for the POSIX error `code` of `syscall`.
fn throw_code(scope: &Scope<'_>, code: &'static str, syscall: &'static str) -> Thrown {
    UvError::net_code(code, syscall).throw(scope);
    Thrown::Pending
}

/// `ERR_SOCKET_DGRAM_NOT_RUNNING` unless the socket of `state` is open, as Node.js's
/// `healthCheck` checks it.
fn health_check(state: &DgramState<'_>) -> Result<(), NodeError> {
    match state.data().open {
        true => Ok(()),
        false => Err(not_running()),
    }
}

/// `value` as `>>> 0` converts it, as the C++ of Node.js converts a port.
pub(crate) fn to_uint32(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<u32, Thrown> {
    let number = f64::from_jsval(scope, value, ())?;
    Ok(if number.is_finite() {
        number.trunc().rem_euclid(4294967296.0) as u32
    } else {
        0
    })
}

/// The IP address `text` of the address family of a `udp6` socket if `ipv6`, and of `udp4`
/// otherwise, with the port `port`. An IPv6 address can have a zone, the name of a network
/// interface.
fn parse_addr(text: &str, ipv6: bool, port: u16) -> Option<SocketAddr> {
    if !ipv6 {
        return text
            .parse::<Ipv4Addr>()
            .ok()
            .map(|ip| SocketAddr::new(ip.into(), port));
    }
    let (address, scope_id) = parse_ipv6(text)?;
    Some(SocketAddrV6::new(address, port, 0, scope_id).into())
}

/// The IPv6 address `text`, with the index of the interface its zone names, or 0 without a zone
/// or for an unknown interface.
fn parse_ipv6(text: &str) -> Option<(Ipv6Addr, u32)> {
    let (address, zone) = match text.split_once('%') {
        Some((address, zone)) => (address, Some(zone)),
        None => (text, None),
    };
    let address = address.parse::<Ipv6Addr>().ok()?;
    let scope_id = zone.and_then(platform::net::interface_index).unwrap_or(0);
    Some((address, scope_id))
}

/// The string `value`, if it is one.
fn string_of(scope: &Scope<'_>, value: HandleValue<'_>) -> Option<String> {
    crate::support::rust_string(scope, value)
}

/// The address family `isIP` gives for `value`: 4, 6, or 0.
fn ip_family(scope: &Scope<'_>, value: HandleValue<'_>) -> u8 {
    match string_of(scope, value) {
        Some(text) if crate::net::is_ipv4(&text) => 4,
        Some(text) if crate::net::is_ipv6(&text) => 6,
        _ => 0,
    }
}

/// Whether `list`, a `BlockList` if given, blocks the IP address `ip` of the address family
/// `family`, such as `ipv4`, as its `check` method gives it.
fn blocked(
    scope: &Scope<'_>,
    list: Option<Object<'_>>,
    ip: &str,
    family: &str,
) -> Result<bool, ExnThrown> {
    let Some(list) = list else {
        return Ok(false);
    };
    let ip = js::JSString::from_str(scope, ip)?.to_jsval_throwing(scope)?;
    let family = js::JSString::from_str(scope, family)?.to_jsval_throwing(scope)?;
    let blocked = Function::call_by_name(scope, list.handle(), c"check", &[ip, family])?;
    Ok(js::conversion::to_boolean(blocked))
}

/// The family name of `ip` that Node.js passes to `BlockList.check`: `ipv4`, `ipv6`, or `ipv0`.
fn check_family(ip: &str) -> &'static str {
    if crate::net::is_ipv4(ip) {
        "ipv4"
    } else if crate::net::is_ipv6(ip) {
        "ipv6"
    } else {
        "ipv0"
    }
}

/// `ERR_IP_BLOCKED` for `ip`.
fn ip_blocked(ip: &str) -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_IP_BLOCKED",
        format!("IP({ip}) is blocked by net.BlockList"),
    )
}

/// Look `address` up with the socket's lookup function, and continue with `purpose` when the
/// lookup calls back. A falsy `address` is the loopback address of the socket's family, as
/// Node.js's `lookup4` and `lookup6` give it.
fn lookup(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    address: HandleValue<'_>,
    purpose: PurposeRef<'_>,
) -> Result<(), ExnThrown> {
    let (ipv6, custom) = {
        let data = state.data();
        (data.ipv6, data.lookup.get(scope))
    };
    let address = if js::conversion::to_boolean(address) {
        address
    } else if ipv6 {
        js::class::get_or_init_atom(scope, c"::1")?
    } else {
        js::class::get_or_init_atom(scope, c"127.0.0.1")?
    };
    let family = if ipv6 { 6 } else { 4 };
    let op = js::class::create_instance_with::<LookupOpImpl>(scope, |_| LookupOpImpl {
        socket: Heap::from(socket),
        purpose: Some(match purpose {
            PurposeRef::Bind { port } => LookupPurpose::Bind {
                port: Heap::from(port.get()),
            },
            PurposeRef::Connect {
                port,
                address,
                callback,
            } => LookupPurpose::Connect {
                port,
                address,
                callback: callback.map(|callback| Heap::from(callback.get())),
            },
            PurposeRef::Send(send) => LookupPurpose::Send(traced_send!(send)),
        }),
    })?;
    let callback = Function::new_callback(scope, c"", 3, on_lookup, op)?;
    let callback = callback.to_jsval_throwing(scope)?;
    let family_value = scope.root_value(js::value::from_f64(f64::from(family)));
    if let Some(custom) = custom {
        Function::call(
            scope,
            HandleValue::undefined(),
            custom,
            &[address, family_value, callback],
        )?;
        return Ok(());
    }
    // The default lookup gives an IP address of the socket's family itself, in the tick queue,
    // and reads `lookup` from `node:dns` for each other lookup.
    if ip_family(scope, address) == family {
        return js::jobs::queue_tick(
            scope,
            callback,
            &[HandleValue::null(), address, family_value],
        );
    }
    let dns = crate::internal_modules::module_default(scope, "node:dns")?;
    Function::call_by_name(
        scope,
        dns.handle(),
        c"lookup",
        &[address, family_value, callback],
    )?;
    Ok(())
}

/// The callback of a lookup, whose payload is the [`LookupOp`] and whose arguments are the error
/// and the address: continue the bind, connect or send. Calls after the first do nothing.
fn on_lookup(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let op = js::function::cast_payload::<LookupOp>(scope, payload);
    let socket = op.data().socket.get(scope);
    let purpose = match &op.data().purpose {
        Some(purpose) => purpose.root(scope),
        None => return Ok(js::value::undefined()),
    };
    op.data_mut().purpose = None;
    let error = args.get(0);
    let ip = args.get(1);
    let state = socket_class::state_of(scope, socket)?;
    let result = match purpose {
        PurposeRef::Bind { port } => after_bind_lookup(scope, socket, &state, error, ip, port),
        PurposeRef::Connect {
            port,
            address,
            callback,
        } => after_connect_lookup(scope, socket, &state, error, ip, port, &address, callback),
        PurposeRef::Send(send) => after_send_lookup(scope, socket, &state, error, ip, &send),
    };
    result.map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::undefined())
}

/// The continuation of `bind` once its address was looked up: bind the OS socket to `ip` and
/// `port`, and start listening, or emit `error`.
fn after_bind_lookup(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    error: HandleValue<'_>,
    ip: HandleValue<'_>,
    port: HandleValue<'_>,
) -> Result<(), Thrown> {
    if !state.data().open {
        return Ok(());
    }
    if js::conversion::to_boolean(error) {
        state.data_mut().bind_state = BindState::Unbound;
        crate::events::emit(scope, socket, c"error", &[error])?;
        return Ok(());
    }
    let port = if js::conversion::to_boolean(port) {
        (to_uint32(scope, port)? & 0xffff) as u16
    } else {
        0
    };
    let ip = crate::errors::js_string(scope, ip)?;
    if let Err(error) = bind_os_socket(state, &ip, port) {
        state.data_mut().bind_state = BindState::Unbound;
        let error = error.host_port(Some(&ip), port).into_value(scope)?;
        crate::events::emit(scope, socket, c"error", &[error])?;
        return Ok(());
    }
    start_receiving(scope, socket, state)?;
    crate::events::emit(scope, socket, c"listening", &[])?;
    Ok(())
}

/// Bind the OS socket of `state` to `ip` and `port`, with the socket's bind options.
fn bind_os_socket(state: &DgramState<'_>, ip: &str, port: u16) -> Result<(), UvError> {
    let (ipv6, options) = {
        let data = state.data();
        (data.ipv6, data.bind_options)
    };
    let addr = parse_addr(ip, ipv6, port).ok_or_else(|| UvError::net_code("EINVAL", "bind"))?;
    let socket = UdpSocket::bind(addr, options).map_err(|error| UvError::net(&error, "bind"))?;
    *state.data().link.borrow_mut() = Some(socket);
    Ok(())
}

/// Start receiving on the bound socket, and apply the buffer size options, as Node.js's
/// `startReceiving` does.
fn start_receiving(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
) -> Result<(), Thrown> {
    let (recv_size, send_size) = {
        let mut data = state.data_mut();
        data.receiving = true;
        data.bind_state = BindState::Bound;
        (data.recv_buffer_size, data.send_buffer_size)
    };
    receive(scope, socket, state)?;
    if recv_size > 0 {
        buffer_size(scope, state, f64::from(recv_size), true)?;
    }
    if send_size > 0 {
        buffer_size(scope, state, f64::from(send_size), false)?;
    }
    Ok(())
}

/// Stop the receive in progress, as Node.js's `stopReceiving` does.
fn stop_receiving(state: &DgramState<'_>) {
    let mut data = state.data_mut();
    if !data.receiving {
        return;
    }
    data.receiving = false;
    if let Some(cancel) = data.cancel_receive.take() {
        let _ = cancel.send(());
    }
}

/// Receive the next datagram on the socket of `state`, unless a receive is in progress.
fn receive(scope: &Scope<'_>, socket: Object<'_>, state: &DgramState<'_>) -> Result<(), ExnThrown> {
    let (link, referenced) = {
        let data = state.data();
        if !data.receiving || data.cancel_receive.is_some() || data.link.borrow().is_none() {
            return Ok(());
        }
        (Rc::clone(&data.link), data.referenced.clone())
    };
    let canceller = spawn_cancellable_with_ref(
        scope,
        socket,
        |cancelled| async move {
            let received = std::future::poll_fn(move |cx| match link.borrow_mut().as_mut() {
                Some(socket) => socket.poll_recv(cx).map(Some),
                None => Poll::Ready(None),
            });
            until_cancelled(cancelled, received).await.flatten()
        },
        |scope, _socket, result| {
            let (bytes, from) =
                result.map_err(|error| throw_net(scope, &error, "recvmsg").throw(scope))?;
            let size = bytes.len() as f64;
            let buffer = crate::buffer::buffer_from_vec(scope, bytes)?;
            let rinfo = address_object(scope, from)?;
            rinfo.set_property(scope, c"size", size)?;
            let pair = js::Array::with_contents(
                scope,
                &[
                    buffer.to_jsval_throwing(scope)?,
                    rinfo.to_jsval_throwing(scope)?,
                ],
            )?;
            pair.to_jsval_throwing(scope)
        },
        receive_done,
        referenced,
    )?;
    state.data_mut().cancel_receive = Some(canceller);
    Ok(())
}

/// The completion of a receive: emit `message` with the datagram and its `rinfo`, unless the
/// `receiveBlockList` option blocks the sender, or `error`, and receive the next datagram. A
/// socket that stopped receiving emits nothing.
fn receive_done(
    scope: &Scope<'_>,
    socket: Object<'_>,
    result: Result<HandleValue<'_>, HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let state = socket_class::state_of(scope, socket)?;
    let receiving = {
        let mut data = state.data_mut();
        data.cancel_receive = None;
        data.receiving
    };
    if !receiving {
        return Ok(());
    }
    match result {
        Ok(pair) => {
            let pair = Object::from_value(scope, *pair).map_err(|e| e.throw(scope))?;
            let buffer = pair.get_element(scope, 0)?;
            let rinfo = pair.get_element(scope, 1)?;
            let rinfo_object = Object::from_value(scope, *rinfo).map_err(|e| e.throw(scope))?;
            let address = rinfo_object.get_property(scope, c"address")?;
            let address = crate::errors::js_string(scope, address)?;
            let family = rinfo_object.get_property(scope, c"family")?;
            let family = crate::errors::js_string(scope, family)?.to_lowercase();
            let list = state.data().receive_block_list.get(scope);
            if !blocked(scope, list, &address, &family)? {
                crate::events::emit(scope, socket, c"message", &[buffer, rinfo])?;
            }
        }
        Err(error) => {
            crate::events::emit(scope, socket, c"error", &[error])?;
        }
    }
    let state = socket_class::state_of(scope, socket)?;
    receive(scope, socket, &state)
}

/// Node.js's `bufferSize`: set the size of the receive buffer, or of the send buffer unless
/// `receive`, to `size` unless it is 0, and return the size. A `size` that isn't an unsigned
/// 32-bit integer throws `ERR_SOCKET_BAD_BUFFER_SIZE`.
fn buffer_size(
    scope: &Scope<'_>,
    state: &DgramState<'_>,
    size: f64,
    receive: bool,
) -> Result<f64, Thrown> {
    if !(0.0..=4294967295.0).contains(&size) || size.fract() != 0.0 {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_SOCKET_BAD_BUFFER_SIZE",
            "Buffer size must be a positive integer",
        )
        .into());
    }
    if !state.data().open {
        return Err(null_handle(scope, "bufferSize"));
    }
    let syscall = if receive {
        "uv_recv_buffer_size"
    } else {
        "uv_send_buffer_size"
    };
    let requested = (size > 0.0).then_some(size as usize);
    let result = match state.data().link.borrow().as_ref() {
        Some(socket) => socket
            .buffer_size(receive, requested)
            .map_err(|error| error.code()),
        None => Err("EBADF"),
    };
    match result {
        Ok(actual) => Ok(if size > 0.0 { size } else { actual as f64 }),
        Err(code) => {
            let error = platform::fs::FsError::from_code(code);
            let message = format!(
                "Could not get or set buffer size: {syscall} returned {} ({})",
                error.code, error.description
            );
            let value =
                crate::errors::system_error(scope, "ERR_SOCKET_BUFFER_SIZE", message, |info| {
                    info.set_property(scope, c"errno", error.errno)?;
                    info.set_property(scope, c"code", error.code)?;
                    info.set_property(scope, c"message", error.description)?;
                    info.set_property(scope, c"syscall", syscall)
                })?;
            Err(js::exception::throw_value(scope, value).into())
        }
    }
}

/// The continuation of `connect` once its address was looked up, as Node.js's `doConnect`: connect
/// the OS socket to `ip` and `port`, and emit `connect` in the tick queue, or report the error
/// there, to `callback` if given and as an `error` event otherwise.
#[allow(clippy::too_many_arguments)]
fn after_connect_lookup(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    error: HandleValue<'_>,
    ip: HandleValue<'_>,
    port: u16,
    address: &str,
    callback: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    if !state.data().open {
        return Ok(());
    }
    let mut failure = js::conversion::to_boolean(error).then_some(error);
    if failure.is_none() {
        let ip_text = crate::errors::js_string(scope, ip)?;
        let list = state.data().send_block_list.get(scope);
        if blocked(scope, list, &ip_text, check_family(&ip_text))? {
            failure = Some(ip_blocked(&ip_text).to_value(scope)?);
        } else if let Err(error) = connect_os_socket(state, &ip_text, port) {
            let error = error.host_port(Some(address), port).into_value(scope)?;
            failure = Some(error);
        }
    }
    let callback = callback.unwrap_or(HandleValue::undefined());
    match failure {
        Some(error) => {
            state.data_mut().connect_state = ConnectState::Disconnected;
            js::jobs::queue_native_tick(scope, connect_failed, socket, &[error, callback])?;
        }
        None => {
            state.data_mut().connect_state = ConnectState::Connected;
            crate::support::emit_later(scope, socket, c"connect", &[])?;
        }
    }
    Ok(())
}

/// Connect the OS socket of `state` to `ip` and `port`.
fn connect_os_socket(state: &DgramState<'_>, ip: &str, port: u16) -> Result<(), UvError> {
    let ipv6 = state.data().ipv6;
    let addr = parse_addr(ip, ipv6, port).ok_or_else(|| UvError::net_code("EINVAL", "connect"))?;
    let data = state.data();
    let mut link = data.link.borrow_mut();
    let socket = link
        .as_mut()
        .ok_or_else(|| UvError::net_code("EBADF", "connect"))?;
    socket
        .connect(addr)
        .map_err(|error| UvError::net(&error, "connect"))
}

/// The tick callback of a failed connect, whose payload is the socket and whose arguments are the
/// error and the callback: remove the callback's `connect` listener and call it with the error,
/// or emit `error` without a callback.
fn connect_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = js::function::cast_payload::<js::Object>(scope, payload);
    let error = args.get(0);
    let callback = args.get(1);
    if js::function::is_callable(callback) {
        Function::call_by_name(
            scope,
            socket.handle(),
            c"removeListener",
            &[js::class::get_or_init_atom(scope, c"connect")?, callback],
        )?;
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
    } else {
        crate::events::emit(scope, socket, c"error", &[error])?;
    }
    Ok(js::value::undefined())
}

/// Node.js's `_connect`: add `callback` as a `connect` listener, and look `address` up to connect
/// to it.
fn start_connect(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    port: u16,
    address: HandleValue<'_>,
    callback: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    if let Some(callback) = callback {
        Function::call_by_name(
            scope,
            socket.handle(),
            c"once",
            &[js::class::get_or_init_atom(scope, c"connect")?, callback],
        )?;
    }
    let address_text = crate::errors::js_string(scope, address)?;
    let purpose = PurposeRef::Connect {
        port,
        address: address_text,
        callback,
    };
    Ok(lookup(scope, socket, state, address, purpose)?)
}

/// Add `operation` to the operations waiting for the bind in progress, and add the `listening`
/// and `errorMonitor` listeners that run or drop them, unless they were added, as Node.js's
/// `enqueue` does.
fn enqueue(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    operation: QueuedRef<'_>,
) -> Result<(), ExnThrown> {
    let first = {
        let mut data = state.data_mut();
        let first = data.queue.is_none();
        data.queue
            .get_or_insert_with(Vec::new)
            .push(match operation {
                QueuedRef::Send(send) => Queued::Send(traced_send!(send)),
                QueuedRef::Connect {
                    port,
                    address,
                    callback,
                } => Queued::Connect {
                    port,
                    address: Heap::from(address.get()),
                    callback: callback.map(|callback| Heap::from(callback.get())),
                },
                QueuedRef::Close => Queued::Close,
            });
        first
    };
    if first {
        let on_error = crate::support::shared_function(scope, on_listen_error, 1)?;
        let on_success = crate::support::shared_function(scope, on_listen_success, 0)?;
        Function::call_by_name(
            scope,
            socket.handle(),
            c"once",
            &[
                crate::events::ERROR_MONITOR.value(scope)?,
                on_error.to_jsval_throwing(scope)?,
            ],
        )?;
        Function::call_by_name(
            scope,
            socket.handle(),
            c"once",
            &[
                js::class::get_or_init_atom(scope, c"listening")?,
                on_success.to_jsval_throwing(scope)?,
            ],
        )?;
    }
    Ok(())
}

/// The `listening` listener of [`enqueue`], whose `this` is the socket: remove the
/// `errorMonitor` listener, and run the operations waiting for the bind.
fn on_listen_success(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let on_error = crate::support::shared_function(scope, on_listen_error, 1)?;
    Function::call_by_name(
        scope,
        socket.handle(),
        c"removeListener",
        &[
            crate::events::ERROR_MONITOR.value(scope)?,
            on_error.to_jsval_throwing(scope)?,
        ],
    )?;
    let Some(state) = socket_class::try_state_of(scope, socket)? else {
        return Ok(js::value::undefined());
    };
    let queue: Vec<QueuedRef<'_>> = {
        let data = state.data();
        data.queue
            .iter()
            .flatten()
            .map(|operation| operation.root(scope))
            .collect()
    };
    state.data_mut().queue = None;
    for operation in queue {
        let state = socket_class::state_of(scope, socket)?;
        let result = match operation {
            QueuedRef::Send(send) => send_validated(scope, socket, &state, send),
            QueuedRef::Connect {
                port,
                address,
                callback,
            } => start_connect(scope, socket, &state, port, address, callback),
            QueuedRef::Close => close(scope, socket, &state, None).map(|_| ()),
        };
        result.map_err(|thrown| thrown.throw(scope))?;
    }
    Ok(js::value::undefined())
}

/// The `errorMonitor` listener of [`enqueue`], whose `this` is the socket: remove the
/// `listening` listener, and drop the operations waiting for the bind.
fn on_listen_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let on_success = crate::support::shared_function(scope, on_listen_success, 0)?;
    Function::call_by_name(
        scope,
        socket.handle(),
        c"removeListener",
        &[
            js::class::get_or_init_atom(scope, c"listening")?,
            on_success.to_jsval_throwing(scope)?,
        ],
    )?;
    if let Some(state) = socket_class::try_state_of(scope, socket)? {
        state.data_mut().queue = None;
    }
    Ok(js::value::undefined())
}

/// The part of `send` after its arguments were validated: bind the socket unless it is bound,
/// wait for the bind in progress, and look the address up, unless the socket is connected.
fn send_validated(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    send: SendRef<'_>,
) -> Result<(), Thrown> {
    health_check(state)?;
    if state.data().bind_state == BindState::Unbound {
        implicit_bind(scope, socket, state)?;
    }
    if state.data().bind_state != BindState::Bound {
        enqueue(scope, socket, state, QueuedRef::Send(send))?;
        return Ok(());
    }
    if send.port.is_some() {
        let address = send.address;
        lookup(scope, socket, state, address, PurposeRef::Send(send))?;
        return Ok(());
    }
    do_send(scope, socket, state, HandleValue::null(), &send)
}

/// The continuation of a send once its address was looked up.
fn after_send_lookup(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    error: HandleValue<'_>,
    ip: HandleValue<'_>,
    send: &SendRef<'_>,
) -> Result<(), Thrown> {
    if js::conversion::to_boolean(error) {
        return report_send_error(scope, socket, error, send.callback);
    }
    do_send(scope, socket, state, ip, send)
}

/// Report the send error `error` to `callback` in the tick queue, or as an `error` event there
/// without a callback.
fn report_send_error(
    scope: &Scope<'_>,
    socket: Object<'_>,
    error: HandleValue<'_>,
    callback: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    match callback {
        Some(callback) => crate::support::next_tick_call(scope, callback, &[error])?,
        None => crate::support::emit_later(scope, socket, c"error", &[error])?,
    }
    Ok(())
}

/// The bytes of `chunks`, or `None` if there are more than [`MAX_DATAGRAM`]. A chunk whose view
/// shrank since gives the bytes it still has.
fn datagram_bytes(
    scope: &Scope<'_>,
    chunks: &[ChunkRef<'_>],
) -> Result<Option<Vec<u8>>, ExnThrown> {
    let mut total: usize = 0;
    for &ChunkRef { length, .. } in chunks {
        total = match total.checked_add(length) {
            Some(total) if total <= MAX_DATAGRAM => total,
            _ => return Ok(None),
        };
    }
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(total).is_err() {
        js::exception::report_out_of_memory(scope);
        return Err(ExnThrown);
    }
    for &ChunkRef {
        view,
        start,
        length,
    } in chunks
    {
        let Some(view) = js::ArrayBufferView::from_object(view) else {
            continue;
        };
        view.with_bytes(|data| {
            let start = start.min(data.len());
            let end = start.saturating_add(length).min(data.len());
            bytes.extend_from_slice(&data[start..end]);
        });
    }
    Ok(Some(bytes))
}

/// Node.js's `doSend`: send `send` to `ip`, or to the connected address for `null`, unless the
/// socket closed or the `sendBlockList` option blocks `ip`. The OS socket takes the datagram at
/// once if no send is waiting, and `callback` is called in the tick queue then. Otherwise the send
/// waits for the earlier ones, and `callback` is called once the OS socket took it.
fn do_send(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    ip: HandleValue<'_>,
    send: &SendRef<'_>,
) -> Result<(), Thrown> {
    let callback = send.callback;
    if !state.data().open {
        return Ok(());
    }
    let address = string_of(scope, send.address);
    let port = send.port.unwrap_or(0);
    let fail = |code: &'static str| -> Result<(), Thrown> {
        if let Some(callback) = callback {
            let error = UvError::net_code(code, "send")
                .host_port(address.as_deref(), port)
                .into_value(scope)?;
            crate::support::next_tick_call(scope, callback, &[error])?;
        }
        Ok(())
    };
    let to = match send.port {
        Some(port) => {
            let ip = crate::errors::js_string(scope, ip)?;
            let list = state.data().send_block_list.get(scope);
            if blocked(scope, list, &ip, check_family(&ip))? {
                if let Some(callback) = callback {
                    let error = ip_blocked(&ip).to_value(scope)?;
                    crate::support::next_tick_call(scope, callback, &[error])?;
                }
                return Ok(());
            }
            let ipv6 = state.data().ipv6;
            match parse_addr(&ip, ipv6, port) {
                Some(to) => Some(to),
                None => return fail("EINVAL"),
            }
        }
        None => None,
    };
    let Some(bytes) = datagram_bytes(scope, &send.chunks)? else {
        return fail("EMSGSIZE");
    };
    let try_send =
        state.data().sends.is_empty() && !crate::runtime_flags::get().test_udp_no_try_send;
    if try_send {
        let sent = {
            let data = state.data();
            let mut link = data.link.borrow_mut();
            link.as_mut().and_then(|socket| socket.try_send(&bytes, to))
        };
        match sent {
            Some(Ok(sent)) => {
                if let Some(callback) = callback {
                    let sent = scope.root_value(js::value::from_f64(sent as f64));
                    crate::support::next_tick_call(scope, callback, &[HandleValue::null(), sent])?;
                }
                return Ok(());
            }
            Some(Err(error)) => {
                if let Some(callback) = callback {
                    let error = UvError::net(&error, "send")
                        .host_port(address.as_deref(), port)
                        .into_value(scope)?;
                    crate::support::next_tick_call(scope, callback, &[error])?;
                }
                return Ok(());
            }
            None => {}
        }
    }
    state.data_mut().sends.push_back(OutgoingSend {
        length: bytes.len(),
        bytes: Some(bytes),
        to,
        address,
        port,
        callback: callback.map(|callback| Heap::from(callback.get())),
    });
    pump_sends(scope, socket, state)?;
    Ok(())
}

/// Start the first waiting send, unless one is in progress.
fn pump_sends(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
) -> Result<(), ExnThrown> {
    let (link, mut bytes, to, address, port) = {
        let mut data = state.data_mut();
        if data.cancel_send.is_some() {
            return Ok(());
        }
        let link = Rc::clone(&data.link);
        let Some(next) = data.sends.front_mut() else {
            return Ok(());
        };
        let Some(bytes) = next.bytes.take() else {
            return Ok(());
        };
        (link, bytes, next.to, next.address.clone(), next.port)
    };
    let canceller = spawn_cancellable(
        scope,
        socket,
        |cancelled| async move {
            let sending = std::future::poll_fn(|cx| match link.borrow_mut().as_mut() {
                Some(socket) => socket.poll_send(cx, &mut bytes, to).map(Some),
                None => Poll::Ready(None),
            });
            until_cancelled(cancelled, sending).await.flatten()
        },
        move |scope, _socket, result| match result {
            Ok(sent) => Ok(scope.root_value(js::value::from_f64(sent as f64))),
            Err(error) => {
                let error = UvError::net(&error, "send")
                    .host_port(address.as_deref(), port)
                    .into_value(scope)?;
                Err(js::exception::throw_value(scope, error))
            }
        },
        send_done,
    )?;
    state.data_mut().cancel_send = Some(canceller);
    Ok(())
}

/// The completion of a send: start the next one, and call the callback with `null` and the bytes
/// sent, or with the error.
fn send_done(
    scope: &Scope<'_>,
    socket: Object<'_>,
    result: Result<HandleValue<'_>, HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let state = socket_class::state_of(scope, socket)?;
    let callback = {
        let mut data = state.data_mut();
        data.cancel_send = None;
        let callback = data
            .sends
            .front()
            .and_then(|send| rooted(scope, &send.callback));
        data.sends.pop_front();
        callback
    };
    pump_sends(scope, socket, &state)?;
    if let Some(callback) = callback {
        let args = match result {
            Ok(sent) => vec![HandleValue::null(), sent],
            Err(error) => vec![error],
        };
        Function::call(scope, HandleValue::undefined(), callback, &args)?;
    }
    Ok(())
}

/// Bind the socket to a free port of every address, as `send` and `connect` do with
/// `bind({ port: 0, exclusive: true })`.
fn implicit_bind(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
) -> Result<(), Thrown> {
    let options = Object::new_plain(scope)?;
    options.set_property(scope, c"port", 0)?;
    options.set_property(scope, c"exclusive", true)?;
    let args = [options.to_jsval_throwing(scope)?, HandleValue::null()];
    bind(scope, socket, state, &args)
}

/// Node.js's `bind`: validate the arguments, add the callback, and look the address up to bind
/// to it.
fn bind(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), Thrown> {
    health_check(state)?;
    if state.data().bind_state != BindState::Unbound {
        return Err(already_bound().into());
    }
    state.data_mut().bind_state = BindState::Binding;
    if let Some(callback) = args
        .last()
        .copied()
        .filter(|last| js::function::is_callable(*last))
    {
        add_bind_callback(scope, socket, callback)?;
    }
    let port = args.first().copied().unwrap_or(HandleValue::undefined());
    let (port, address) = match Object::from_value(scope, *port) {
        Ok(options) => {
            if js::function::is_callable(options.get_property(scope, c"recvStart")?) {
                return Err(NodeError::not_implemented("bind() with a handle").into());
            }
            let fd = options.get_property(scope, c"fd")?;
            if fd.is_int32() && fd.to_int32() > 0 {
                return Err(NodeError::not_implemented("bind() with a file descriptor").into());
            }
            let address = options.get_property(scope, c"address")?;
            (options.get_property(scope, c"port")?, address)
        }
        Err(_) => {
            let address = args.get(1).copied().unwrap_or(HandleValue::undefined());
            let address = if js::function::is_callable(address) {
                HandleValue::undefined()
            } else {
                address
            };
            (port, address)
        }
    };
    let address = if js::conversion::to_boolean(address) {
        address
    } else if state.data().ipv6 {
        js::class::get_or_init_atom(scope, c"::")?
    } else {
        js::class::get_or_init_atom(scope, c"0.0.0.0")?
    };
    lookup(scope, socket, state, address, PurposeRef::Bind { port })?;
    Ok(())
}

/// `ERR_SOCKET_ALREADY_BOUND`.
fn already_bound() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_SOCKET_ALREADY_BOUND",
        "Socket is already bound",
    )
}

/// Add the listeners that call `callback` with the socket as `this` once it listens, and remove
/// each other on `listening` or an error, as Node.js's `bind` adds them.
fn add_bind_callback(
    scope: &Scope<'_>,
    socket: Object<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let link = js::class::create_instance_with::<BindCallbackImpl>(scope, |_| BindCallbackImpl {
        callback: Heap::from(callback.get()),
        on_listening: None,
        remove: None,
    })?;
    let on_listening = Function::new_callback(scope, c"onListening", 0, bind_on_listening, link)?;
    let remove = Function::new_callback(scope, c"removeListeners", 0, bind_remove_listeners, link)?;
    {
        let mut data = link.data_mut();
        data.on_listening = Some(Heap::from(*on_listening));
        data.remove = Some(Heap::from(*remove));
    }
    Function::call_by_name(
        scope,
        socket.handle(),
        c"on",
        &[
            crate::events::ERROR_MONITOR.value(scope)?,
            remove.to_jsval_throwing(scope)?,
        ],
    )?;
    Function::call_by_name(
        scope,
        socket.handle(),
        c"on",
        &[
            js::class::get_or_init_atom(scope, c"listening")?,
            on_listening.to_jsval_throwing(scope)?,
        ],
    )?;
    Ok(())
}

/// Remove the listeners of the [`BindCallback`] `link` from `socket`.
fn remove_bind_listeners(
    scope: &Scope<'_>,
    socket: HandleValue<'_>,
    link: BindCallback<'_>,
) -> Result<(), ExnThrown> {
    let (on_listening, remove) = {
        let data = link.data();
        (data.on_listening.get(scope), data.remove.get(scope))
    };
    let (Some(on_listening), Some(remove)) = (on_listening, remove) else {
        return Ok(());
    };
    let socket = Object::from_value(scope, *socket).map_err(|e| e.throw(scope))?;
    Function::call_by_name(
        scope,
        socket.handle(),
        c"removeListener",
        &[
            crate::events::ERROR_MONITOR.value(scope)?,
            remove.to_jsval_throwing(scope)?,
        ],
    )?;
    Function::call_by_name(
        scope,
        socket.handle(),
        c"removeListener",
        &[
            js::class::get_or_init_atom(scope, c"listening")?,
            on_listening.to_jsval_throwing(scope)?,
        ],
    )?;
    Ok(())
}

/// The `errorMonitor` listener of a `bind` callback, whose payload is the [`BindCallback`].
fn bind_remove_listeners(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let link = js::function::cast_payload::<BindCallback>(scope, payload);
    remove_bind_listeners(scope, scope.root_value(args.this()), link)?;
    Ok(js::value::undefined())
}

/// The `listening` listener of a `bind` callback, whose payload is the [`BindCallback`]: remove
/// both listeners, and call the callback with the socket as `this`.
fn bind_on_listening(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let link = js::function::cast_payload::<BindCallback>(scope, payload);
    let this = scope.root_value(args.this());
    remove_bind_listeners(scope, this, link)?;
    let callback = link.data().callback.get(scope);
    Function::call(scope, this, callback, js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// Node.js's `close`: add `callback` as a `close` listener, wait for the bind in progress, stop
/// receiving, close the OS socket, and emit `close` in the tick queue. The sends the OS socket
/// didn't take get `ECANCELED` after that.
fn close<'s>(
    scope: &Scope<'_>,
    socket: Object<'s>,
    state: &DgramState<'_>,
    callback: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    if let Some(callback) = callback.filter(|callback| js::function::is_callable(*callback)) {
        Function::call_by_name(
            scope,
            socket.handle(),
            c"on",
            &[js::class::get_or_init_atom(scope, c"close")?, callback],
        )?;
    }
    if state.data().queue.is_some() {
        enqueue(scope, socket, state, QueuedRef::Close)?;
        return Ok(socket);
    }
    health_check(state)?;
    stop_receiving(state);
    let (os_socket, cancel_send, cancelled) = {
        let mut data = state.data_mut();
        data.open = false;
        data.handle = None;
        let os_socket = data.link.borrow_mut().take();
        let cancelled: Vec<(HandleValue<'_>, Option<String>, u16)> = data
            .sends
            .iter()
            .filter_map(|send| {
                let callback = rooted(scope, &send.callback)?;
                Some((callback, send.address.clone(), send.port))
            })
            .collect();
        data.sends.clear();
        (os_socket, data.cancel_send.take(), cancelled)
    };
    drop(cancel_send);
    drop(os_socket);
    crate::support::emit_later(scope, socket, c"close", &[])?;
    for (callback, address, port) in cancelled {
        let error = UvError::net_code("ECANCELED", "send")
            .host_port(address.as_deref(), port)
            .into_value(scope)?;
        crate::support::next_tick_call(scope, callback, &[error])?;
    }
    Ok(socket)
}

/// The chunk of the whole of `buffer`, a string, which is encoded as UTF-8, or an
/// `ArrayBufferView`, or `None` for another value.
fn whole_chunk<'s>(
    scope: &'s Scope<'_>,
    buffer: HandleValue<'_>,
) -> Result<Option<ChunkRef<'s>>, Thrown> {
    let view = if buffer.is_string() {
        let (bytes, _) = crate::support::chunk_bytes(scope, buffer, None)?.unwrap_or_default();
        crate::buffer::new_buffer(scope, &bytes)?
    } else {
        match Object::from_value(scope, *buffer)
            .ok()
            .filter(|object| js::ArrayBufferView::from_object(*object).is_some())
        {
            Some(view) => view,
            None => return Ok(None),
        }
    };
    let length = js::ArrayBufferView::from_object(view).map_or(0, |view| view.byte_length());
    Ok(Some(ChunkRef {
        view,
        start: 0,
        length,
    }))
}

/// The expected types of a datagram's buffers, as Node.js's `ERR_INVALID_ARG_TYPE` names them.
const BUFFER_TYPES: &str = "of type string or an instance of Buffer, TypedArray, or DataView";

/// The chunk of `length` bytes from `offset` of `buffer`, as Node.js's `sliceBuffer` takes it.
fn slice_buffer<'s>(
    scope: &'s Scope<'_>,
    buffer: HandleValue<'_>,
    offset: HandleValue<'_>,
    length: HandleValue<'_>,
) -> Result<ChunkRef<'s>, Thrown> {
    let Some(mut chunk) = whole_chunk(scope, buffer)? else {
        return Err(NodeError::invalid_arg_type(scope, "buffer", BUFFER_TYPES, buffer).into());
    };
    let offset = to_uint32(scope, offset)? as usize;
    let length = to_uint32(scope, length)? as usize;
    let out_of_bounds = |name: &str| {
        NodeError::new(
            ErrorKind::RangeError,
            "ERR_BUFFER_OUT_OF_BOUNDS",
            format!("\"{name}\" is outside of buffer bounds"),
        )
    };
    if offset > chunk.length {
        return Err(out_of_bounds("offset").into());
    }
    if length > chunk.length - offset {
        return Err(out_of_bounds("length").into());
    }
    chunk.start = offset;
    chunk.length = length;
    Ok(chunk)
}

/// The chunks of `buffer`, a string, an `ArrayBufferView` or an array of them, as Node.js's
/// `send` takes them.
fn buffer_chunks<'s>(
    scope: &'s Scope<'_>,
    buffer: HandleValue<'_>,
) -> Result<Vec<ChunkRef<'s>>, Thrown> {
    let Some(elements) = crate::support::array_elements(scope, buffer)? else {
        return match whole_chunk(scope, buffer)? {
            Some(chunk) => Ok(vec![chunk]),
            None => Err(NodeError::invalid_arg_type(scope, "buffer", BUFFER_TYPES, buffer).into()),
        };
    };
    let mut chunks = Vec::new();
    for element in elements {
        match whole_chunk(scope, element)? {
            Some(chunk) => chunks.push(chunk),
            None => {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "buffer list arguments",
                    BUFFER_TYPES,
                    buffer,
                )
                .into())
            }
        }
    }
    Ok(chunks)
}

/// `ttl` as Node.js's `validateNumber(ttl, 'ttl')` checks it, converted to an integer as its C++
/// converts it.
fn ttl_value(scope: &Scope<'_>, ttl: HandleValue<'_>) -> Result<i32, Thrown> {
    if !ttl.is_number() {
        return Err(NodeError::invalid_arg_type(scope, "ttl", "of type number", ttl).into());
    }
    Ok(i32::from_jsval(
        scope,
        ttl,
        js::conversion::ConversionBehavior::Default,
    )?)
}

/// Run `operation` on the OS socket of `state`, and throw Node.js's `ErrnoException` for
/// `syscall` if it fails, with `EBADF` for an unbound socket. A closed socket throws the
/// `TypeError` Node.js throws for its missing handle, which names `method`.
fn with_os_socket<T>(
    scope: &Scope<'_>,
    state: &DgramState<'_>,
    method: &str,
    syscall: &'static str,
    operation: impl FnOnce(&mut UdpSocket) -> Result<T, NetError>,
) -> Result<T, Thrown> {
    if !state.data().open {
        return Err(null_handle(scope, method));
    }
    let result = {
        let data = state.data();
        let mut link = data.link.borrow_mut();
        link.as_mut().map(operation)
    };
    match result {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(throw_net(scope, &error, syscall)),
        None => Err(throw_code(scope, "EBADF", syscall)),
    }
}

/// The multicast interface `interface` for the group `group`: an IPv4 interface address for an
/// IPv4 group, and an IPv6 address whose zone names the interface for an IPv6 group. Without
/// `interface`, the default interface.
fn membership_interface(group: IpAddr, interface: Option<&str>) -> Option<MulticastInterface> {
    match (group, interface) {
        (IpAddr::V4(_), None) => Some(MulticastInterface::V4(Ipv4Addr::UNSPECIFIED)),
        (IpAddr::V4(_), Some(interface)) => interface.parse().ok().map(MulticastInterface::V4),
        (IpAddr::V6(_), None) => Some(MulticastInterface::V6(0)),
        (IpAddr::V6(_), Some(interface)) => {
            parse_ipv6(interface).map(|(_, index)| MulticastInterface::V6(index))
        }
    }
}

/// The multicast group `text`, an IPv4 or IPv6 address.
fn parse_group(text: &str) -> Option<IpAddr> {
    if let Ok(ip) = text.parse::<Ipv4Addr>() {
        return Some(ip.into());
    }
    parse_ipv6(text).map(|(ip, _)| ip.into())
}

/// The interface argument `value` of a membership method: `None` if it is `undefined` or
/// `null`, and its string otherwise.
fn interface_arg(
    scope: &Scope<'_>,
    value: Option<HandleValue<'_>>,
) -> Result<Option<String>, Thrown> {
    match value {
        Some(value) if !value.is_null_or_undefined() => {
            Ok(Some(crate::errors::js_string(scope, value)?))
        }
        _ => Ok(None),
    }
}

/// Join or leave the multicast group `group` on `interface`, as `addMembership` and
/// `dropMembership` do, naming `syscall` in errors.
fn set_membership(
    scope: &Scope<'_>,
    state: &DgramState<'_>,
    group: HandleValue<'_>,
    interface: Option<HandleValue<'_>>,
    join: bool,
    syscall: &'static str,
) -> Result<(), Thrown> {
    health_check(state)?;
    if !js::conversion::to_boolean(group) {
        return Err(NodeError::missing_args(&["multicastAddress"]).into());
    }
    let group = crate::errors::js_string(scope, group)?;
    let interface = interface_arg(scope, interface)?;
    let Some(group) = parse_group(&group) else {
        return Err(throw_code(scope, "EINVAL", syscall));
    };
    let Some(interface) = membership_interface(group, interface.as_deref()) else {
        return Err(throw_code(scope, "EINVAL", syscall));
    };
    with_os_socket(scope, state, syscall, syscall, |socket| {
        socket.set_membership(group, interface, join)
    })
}

/// Join or leave the source-specific multicast group `group` for `source` on `interface`, as
/// `addSourceSpecificMembership` and `dropSourceSpecificMembership` do, naming `syscall` in
/// errors. IPv6 groups are not supported: they throw `EINVAL` on a `udp4` socket, as in Node.js,
/// and `ENOSYS` on a `udp6` socket.
fn set_source_membership(
    scope: &Scope<'_>,
    state: &DgramState<'_>,
    source: HandleValue<'_>,
    group: HandleValue<'_>,
    interface: Option<HandleValue<'_>>,
    join: bool,
    syscall: &'static str,
) -> Result<(), Thrown> {
    health_check(state)?;
    let source = validate_string(scope, source, "sourceAddress")?;
    let group = validate_string(scope, group, "groupAddress")?;
    let interface = interface_arg(scope, interface)?;
    if let Ok(group) = group.parse::<Ipv4Addr>() {
        let Ok(source) = source.parse::<Ipv4Addr>() else {
            return Err(throw_code(scope, "EINVAL", syscall));
        };
        let interface = match interface.as_deref() {
            Some(interface) => match interface.parse::<Ipv4Addr>() {
                Ok(interface) => interface,
                Err(_) => return Err(throw_code(scope, "EINVAL", syscall)),
            },
            None => Ipv4Addr::UNSPECIFIED,
        };
        return with_os_socket(scope, state, syscall, syscall, |socket| {
            socket.set_source_membership(source, group, interface, join)
        });
    }
    if parse_ipv6(&group).is_none() || parse_ipv6(&source).is_none() || !state.data().ipv6 {
        return Err(throw_code(scope, "EINVAL", syscall));
    }
    Err(throw_code(scope, "ENOSYS", syscall))
}

/// The `abort` listener of the `signal` option, whose payload is the socket: close the socket if
/// it is open.
fn on_signal_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = js::function::cast_payload::<js::Object>(scope, payload);
    let state = socket_class::state_of(scope, socket)?;
    if state.data().open {
        close(scope, socket, &state, None).map_err(|thrown| thrown.throw(scope))?;
    }
    Ok(js::value::undefined())
}

/// The `close` listener that removes the `signal` option's `abort` listener, whose payload is the
/// [`SignalLink`].
fn remove_signal_listener(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let link = js::function::cast_payload::<SignalLink>(scope, payload);
    let (signal, listener) = {
        let data = link.data();
        (data.signal.get(scope), data.listener.get(scope))
    };
    crate::events::listen(scope, signal, c"removeEventListener", c"abort", listener)?;
    Ok(js::value::undefined())
}

/// Close the socket when `signal` aborts, or now if it aborted, as the `signal` option does.
fn watch_signal(
    scope: &Scope<'_>,
    socket: Object<'_>,
    signal: HandleValue<'_>,
) -> Result<(), Thrown> {
    crate::errors::validate_abort_signal(scope, signal, "options.signal")?;
    let signal = Object::from_value(scope, *signal).map_err(|e| Thrown::from(e.throw(scope)))?;
    let state = socket_class::state_of(scope, socket)?;
    if js::conversion::to_boolean(signal.get_property(scope, c"aborted")?) {
        if state.data().open {
            close(scope, socket, &state, None)?;
        }
        return Ok(());
    }
    let listener = Function::new_callback(scope, c"onAborted", 0, on_signal_abort, socket)?;
    listen_for_abort_until_close(scope, socket, signal, *listener)?;
    Ok(())
}

/// Add `listener` as an `abort` listener of `signal`, and remove it once `target`, a socket or a
/// server, emits `close`.
pub(crate) fn listen_for_abort_until_close(
    scope: &Scope<'_>,
    target: Object<'_>,
    signal: Object<'_>,
    listener: Object<'_>,
) -> Result<(), ExnThrown> {
    crate::events::listen(scope, signal, c"addEventListener", c"abort", listener)?;
    let link = js::class::create_instance_with::<SignalLinkImpl>(scope, |_| SignalLinkImpl {
        signal: Heap::from(signal),
        listener: Heap::from(listener),
    })?;
    let remove = Function::new_callback(scope, c"", 0, remove_signal_listener, link)?;
    crate::events::listen(scope, target, c"once", c"close", *remove)
}

/// `Socket.prototype[Symbol.asyncDispose]()`: close the socket, and return a promise fulfilled
/// once it emitted `close`, or at once if it is closed.
fn async_dispose(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let promise = js::Promise::new_pending(scope)?;
    let state = socket_class::state_of(scope, socket)?;
    if !state.data().open {
        promise.resolve(scope, HandleValue::undefined())?;
        return promise.to_jsval_raw_throwing(scope);
    }
    let resolve = Function::new_callback(scope, c"", 0, resolve_dispose, promise)?;
    let resolve = resolve.to_jsval_throwing(scope)?;
    if let Err(thrown) = close(scope, socket, &state, Some(resolve)) {
        thrown.throw(scope);
        promise.reject_with_pending(scope)?;
    }
    promise.to_jsval_raw_throwing(scope)
}

/// The `close` listener of [`async_dispose`], whose payload is its promise: fulfill it.
fn resolve_dispose(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = js::function::cast_payload::<js::Object>(scope, payload)
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
    promise.resolve(scope, HandleValue::undefined())?;
    Ok(js::value::undefined())
}

/// The deprecated accessors of `Socket.prototype`, in the order of their payloads.
const LEGACY_ACCESSORS: [&std::ffi::CStr; 5] = [
    c"_handle",
    c"_receiving",
    c"_bindState",
    c"_queue",
    c"_reuseAddr",
];

/// The state of the `this` of a deprecated accessor, after emitting the deprecation warning
/// DEP0112 for the accessor `name`.
fn legacy_state<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
    name: &str,
) -> Result<DgramState<'s>, ExnThrown> {
    crate::process::deprecate_once(
        scope,
        &format!("Socket.prototype.{name} is deprecated"),
        "DEP0112",
    )?;
    let socket = crate::support::this_object(scope, args)?;
    socket_class::state_of(scope, socket)
}

/// The getter of the deprecated accessor whose index in [`LEGACY_ACCESSORS`] is the payload.
fn get_legacy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let index = payload.to_number() as usize;
    let name = LEGACY_ACCESSORS[index].to_str().unwrap_or_default();
    let state = legacy_state(scope, &args, name)?;
    let data = state.data();
    Ok(match index {
        0 => data
            .handle
            .get(scope)
            .map_or(js::value::null(), |handle| handle.as_value()),
        1 => js::value::from_bool(data.receiving),
        2 => js::value::from_f64(match data.bind_state {
            BindState::Unbound => 0.0,
            BindState::Binding => 1.0,
            BindState::Bound => 2.0,
        }),
        3 => match &data.queue {
            Some(queue) => {
                let length = queue.len();
                drop(data);
                js::Array::new(scope, length)?.as_value()
            }
            None => js::value::undefined(),
        },
        _ => js::value::from_bool(data.bind_options.reuse_addr),
    })
}

/// The setter of the deprecated accessor whose index in [`LEGACY_ACCESSORS`] is the payload.
/// Setting `_queue` to `undefined` drops the waiting operations, and other values are ignored.
fn set_legacy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let index = payload.to_number() as usize;
    let name = LEGACY_ACCESSORS[index].to_str().unwrap_or_default();
    let state = legacy_state(scope, &args, name)?;
    let value = args.get(0);
    let mut data = state.data_mut();
    match index {
        0 => data.handle = Object::from_value(scope, *value).ok().map(Heap::from),
        1 => data.receiving = js::conversion::to_boolean(value),
        2 => {
            let number = if value.is_number() {
                value.to_number()
            } else {
                -1.0
            };
            data.bind_state = match number {
                0.0 => BindState::Unbound,
                1.0 => BindState::Binding,
                2.0 => BindState::Bound,
                _ => data.bind_state,
            };
        }
        3 => {
            if value.is_undefined() {
                data.queue = None;
            }
        }
        _ => data.bind_options.reuse_addr = js::conversion::to_boolean(value),
    }
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(
    name = "Socket",
    extends = "crate::events::event_emitter",
    state = "DgramState",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod socket_class {
    use super::*;

    /// <https://nodejs.org/api/dgram.html#dgramcreatesockettype-callback>: a socket of the type
    /// `kind`, `udp4` or `udp6`, or of the options object's `type`, whose `message` events go to
    /// `listener`. Of the options, `reuseAddr`, `reusePort`, `ipv6Only`, `recvBufferSize`,
    /// `sendBufferSize`, `lookup`, `signal`, `receiveBlockList` and `sendBlockList` are
    /// supported.
    #[constructor(length = 2)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        kind: Option<HandleValue<'_>>,
        listener: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)?;
        let mut kind = kind.unwrap_or(HandleValue::undefined());
        let options = Object::from_value(scope, *kind)
            .ok()
            .filter(|options| !options.is_callable());
        let mut lookup = HandleValue::undefined();
        let mut sizes = [0u32; 2];
        let mut block_lists = [None, None];
        if let Some(options) = options {
            kind = options.get_property(scope, c"type")?;
            lookup = options.get_property(scope, c"lookup")?;
            for (index, (name, label)) in [
                (c"recvBufferSize", "options.recvBufferSize"),
                (c"sendBufferSize", "options.sendBufferSize"),
            ]
            .into_iter()
            .enumerate()
            {
                let size = options.get_property(scope, name)?;
                if js::conversion::to_boolean(size) {
                    sizes[index] =
                        crate::errors::validate_integer(scope, size, label, 0.0, 4294967295.0)?
                            as u32;
                }
            }
            for (index, (name, label)) in [
                (c"receiveBlockList", "options.receiveBlockList"),
                (c"sendBlockList", "options.sendBlockList"),
            ]
            .into_iter()
            .enumerate()
            {
                let list = options.get_property(scope, name)?;
                if js::conversion::to_boolean(list) {
                    crate::net::blocklist::validate_block_list(scope, list, label)?;
                    block_lists[index] = Object::from_value(scope, *list).ok();
                }
            }
        }
        let lookup = if lookup.is_undefined() {
            None
        } else {
            Some(validate_function(scope, lookup, "lookup")?)
        };
        let ipv6 = match string_of(scope, kind).as_deref() {
            Some("udp4") => false,
            Some("udp6") => true,
            _ => {
                return Err(NodeError::new(
                    ErrorKind::TypeError,
                    "ERR_SOCKET_BAD_TYPE",
                    "Bad socket type specified. Valid types are: udp4, udp6",
                )
                .into())
            }
        };
        this.set_property(scope, c"type", kind)?;
        if let Some(listener) = listener.filter(|listener| js::function::is_callable(*listener)) {
            Function::call_by_name(
                scope,
                this.handle(),
                c"on",
                &[js::class::get_or_init_atom(scope, c"message")?, listener],
            )?;
        }
        let flag = |name| -> Result<bool, ExnThrown> {
            match options {
                Some(options) => Ok(js::conversion::to_boolean(
                    options.get_property(scope, name)?,
                )),
                None => Ok(false),
            }
        };
        let bind_options = UdpBindOptions {
            reuse_addr: flag(c"reuseAddr")?,
            reuse_port: flag(c"reusePort")?,
            ipv6_only: flag(c"ipv6Only")?,
        };
        let handle = Object::new_plain(scope)?;
        {
            let state = state_of(scope, this)?;
            let mut data = state.data_mut();
            data.ipv6 = ipv6;
            data.open = true;
            data.bind_options = bind_options;
            data.recv_buffer_size = sizes[0];
            data.send_buffer_size = sizes[1];
            data.receive_block_list = block_lists[0].map(Heap::from);
            data.send_block_list = block_lists[1].map(Heap::from);
            data.lookup = lookup.map(Heap::from);
            data.handle = Some(Heap::from(handle));
        }
        if let Some(options) = options {
            let signal = options.get_property(scope, c"signal")?;
            if !signal.is_undefined() {
                watch_signal(scope, this, signal)?;
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/dgram.html#socketbindport-address-callback>: bind to `port` of
    /// `address`, from `([port][, address][, callback])` or `(options[, callback])`, after looking
    /// the address up. Binding to a file descriptor or a handle is not supported.
    #[method(length = 2)]
    pub fn bind<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        super::bind(scope, this, &state, &args)?;
        Ok(this)
    }

    /// <https://nodejs.org/api/dgram.html#socketbindsyncoptions>: bind to the port and the IP
    /// address `options` gives at once, emit `listening` in the tick queue, and return the bound
    /// address.
    #[method(length = 0)]
    pub fn bind_sync<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        super::bind_sync(scope, this, &state, options)
    }

    /// <https://nodejs.org/api/dgram.html#socketconnectport-address-callback>: connect to `port`
    /// of `address`, the loopback address by default, after looking it up.
    #[method(length = 3)]
    pub fn connect(
        scope: &Scope<'_>,
        this: Object<'_>,
        port: Option<HandleValue<'_>>,
        address: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let undefined = HandleValue::undefined();
        let port = port.unwrap_or(undefined);
        let port = crate::net::validate_port_allowing(scope, port, "Port", false)?;
        let mut callback = callback.filter(|callback| js::conversion::to_boolean(*callback));
        let address = match address {
            Some(address) if js::function::is_callable(address) => {
                callback = Some(address);
                js::class::get_or_init_atom(scope, c"")?
            }
            Some(address) if !address.is_undefined() => address,
            _ => js::class::get_or_init_atom(scope, c"")?,
        };
        validate_string(scope, address, "address")?;
        let state = state_of(scope, this)?;
        if state.data().connect_state != ConnectState::Disconnected {
            return Err(is_connected().into());
        }
        state.data_mut().connect_state = ConnectState::Connecting;
        if state.data().bind_state == BindState::Unbound {
            implicit_bind(scope, this, &state)?;
        }
        if state.data().bind_state != BindState::Bound {
            let operation = QueuedRef::Connect {
                port,
                address,
                callback,
            };
            enqueue(scope, this, &state, operation)?;
            return Ok(());
        }
        start_connect(scope, this, &state, port, address, callback)
    }

    /// <https://nodejs.org/api/dgram.html#socketconnectsyncport-address>: connect to `port` of
    /// the IP address `address` at once, binding first if needed, and emit `connect` in the tick
    /// queue.
    #[method(length = 2)]
    pub fn connect_sync(
        scope: &Scope<'_>,
        this: Object<'_>,
        port: Option<HandleValue<'_>>,
        address: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        super::connect_sync(scope, this, &state, port, address)
    }

    /// <https://nodejs.org/api/dgram.html#socketdisconnect>
    pub fn disconnect(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        if state.data().connect_state != ConnectState::Connected {
            return Err(not_connected().into());
        }
        with_os_socket(scope, &state, "disconnect", "connect", |socket| {
            socket.disconnect()
        })?;
        state.data_mut().connect_state = ConnectState::Disconnected;
        Ok(())
    }

    /// `socket.sendto(buffer, offset, length, port, address[, callback])`, the form of `send`
    /// that Node.js keeps for compatibility.
    #[method(length = 6)]
    pub fn sendto(
        scope: &Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let arg = |index: usize| args.get(index).copied().unwrap_or(HandleValue::undefined());
        for (index, name) in [(1, "offset"), (2, "length"), (3, "port")] {
            if !arg(index).is_number() {
                return Err(
                    NodeError::invalid_arg_type(scope, name, "of type number", arg(index)).into(),
                );
            }
        }
        validate_string(scope, arg(4), "address")?;
        send(scope, this, args)
    }

    /// <https://nodejs.org/api/dgram.html#socketsendmsg-offset-length-port-address-callback>:
    /// send `msg`, a string, an `ArrayBufferView` or an array of them, or `length` bytes of it
    /// from `offset`, to `port` of `address`, or to the connected address. The socket binds first
    /// unless it is bound.
    #[method(length = 6)]
    pub fn send(
        scope: &Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        let send = super::parse_send(scope, &state, &args)?;
        send_validated(scope, this, &state, send)
    }

    /// <https://nodejs.org/api/dgram.html#socketclosecallback>
    #[method(length = 1)]
    pub fn close<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        super::close(scope, this, &state, callback)
    }

    /// <https://nodejs.org/api/dgram.html#socketaddress>
    pub fn address<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        health_check(&state)?;
        let addr = with_os_socket(scope, &state, "getsockname", "getsockname", |socket| {
            socket.local_addr()
        })?;
        Ok(address_object(scope, addr)?)
    }

    /// <https://nodejs.org/api/dgram.html#socketremoteaddress>
    pub fn remote_address<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        health_check(&state)?;
        if state.data().connect_state != ConnectState::Connected {
            return Err(not_connected().into());
        }
        let addr = with_os_socket(scope, &state, "getpeername", "getpeername", |socket| {
            socket.peer_addr()
        })?;
        Ok(address_object(scope, addr)?)
    }

    /// <https://nodejs.org/api/dgram.html#socketsetbroadcastflag>: not supported on wasm, where
    /// it throws `ENOTSUP`.
    #[method(length = 1)]
    pub fn set_broadcast(
        scope: &Scope<'_>,
        this: Object<'_>,
        flag: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let on = flag.is_some_and(js::conversion::to_boolean);
        let state = state_of(scope, this)?;
        with_os_socket(scope, &state, "setBroadcast", "setBroadcast", |socket| {
            socket.set_broadcast(on)
        })
    }

    /// <https://nodejs.org/api/dgram.html#socketsetttlttl>: returns `ttl`.
    #[method(name = "setTTL", length = 1)]
    pub fn set_ttl<'s>(
        scope: &Scope<'_>,
        this: Object<'_>,
        ttl: HandleValue<'s>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let value = ttl_value(scope, ttl)?;
        let state = state_of(scope, this)?;
        if !state.data().open {
            return Err(null_handle(scope, "setTTL"));
        }
        if !(1..=255).contains(&value) {
            return Err(throw_code(scope, "EINVAL", "setTTL"));
        }
        with_os_socket(scope, &state, "setTTL", "setTTL", |socket| {
            socket.set_ttl(value as u8)
        })?;
        Ok(ttl)
    }

    /// <https://nodejs.org/api/dgram.html#socketsetmulticastttlttl>: returns `ttl`. Not
    /// supported on wasm, where it throws `ENOTSUP`.
    #[method(name = "setMulticastTTL", length = 1)]
    pub fn set_multicast_ttl<'s>(
        scope: &Scope<'_>,
        this: Object<'_>,
        ttl: HandleValue<'s>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let value = ttl_value(scope, ttl)?;
        let state = state_of(scope, this)?;
        if !state.data().open {
            return Err(null_handle(scope, "setMulticastTTL"));
        }
        if !(0..=255).contains(&value) {
            return Err(throw_code(scope, "EINVAL", "setMulticastTTL"));
        }
        with_os_socket(
            scope,
            &state,
            "setMulticastTTL",
            "setMulticastTTL",
            |socket| socket.set_multicast_ttl(value as u8),
        )?;
        Ok(ttl)
    }

    /// <https://nodejs.org/api/dgram.html#socketsetmulticastloopbackflag>: returns `flag`. Not
    /// supported on wasm, where it throws `ENOTSUP`.
    #[method(length = 1)]
    pub fn set_multicast_loopback<'s>(
        scope: &Scope<'_>,
        this: Object<'_>,
        flag: HandleValue<'s>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let on = js::conversion::to_boolean(flag);
        let state = state_of(scope, this)?;
        with_os_socket(
            scope,
            &state,
            "setMulticastLoopback",
            "setMulticastLoopback",
            |socket| socket.set_multicast_loop(on),
        )?;
        Ok(flag)
    }

    /// <https://nodejs.org/api/dgram.html#socketsetmulticastinterfacemulticastinterface>: an
    /// IPv4 interface address, or an IPv6 address whose zone names the interface. Not supported
    /// on wasm, where it throws `ENOTSUP`.
    #[method(length = 1)]
    pub fn set_multicast_interface(
        scope: &Scope<'_>,
        this: Object<'_>,
        interface: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        health_check(&state)?;
        let interface = interface.unwrap_or(HandleValue::undefined());
        let interface = validate_string(scope, interface, "interfaceAddress")?;
        let syscall = "setMulticastInterface";
        let interface = match interface.parse::<Ipv4Addr>() {
            Ok(addr) => MulticastInterface::V4(addr),
            Err(_) => match parse_ipv6(&interface) {
                Some((_, index)) => MulticastInterface::V6(index),
                None => return Err(throw_code(scope, "EINVAL", syscall)),
            },
        };
        with_os_socket(scope, &state, syscall, syscall, |socket| {
            socket.set_multicast_interface(interface)
        })
    }

    /// <https://nodejs.org/api/dgram.html#socketaddmembershipmulticastaddress-multicastinterface>:
    /// an unbound socket throws `EBADF` where Node.js binds it. Not supported on wasm, where it
    /// throws `ENOTSUP`.
    #[method(length = 2)]
    pub fn add_membership(
        scope: &Scope<'_>,
        this: Object<'_>,
        group: Option<HandleValue<'_>>,
        interface: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        let group = group.unwrap_or(HandleValue::undefined());
        set_membership(scope, &state, group, interface, true, "addMembership")
    }

    /// <https://nodejs.org/api/dgram.html#socketdropmembershipmulticastaddress-multicastinterface>:
    /// an unbound socket throws `EBADF` where Node.js binds it. Not supported on wasm, where it
    /// throws `ENOTSUP`.
    #[method(length = 2)]
    pub fn drop_membership(
        scope: &Scope<'_>,
        this: Object<'_>,
        group: Option<HandleValue<'_>>,
        interface: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        let group = group.unwrap_or(HandleValue::undefined());
        set_membership(scope, &state, group, interface, false, "dropMembership")
    }

    /// <https://nodejs.org/api/dgram.html#socketaddsourcespecificmembershipsourceaddress-groupaddress-multicastinterface>:
    /// IPv4 groups only, natively. IPv6 groups throw `ENOSYS`, wasm throws `ENOTSUP`, and an
    /// unbound socket throws `EBADF` where Node.js binds it.
    #[method(length = 3)]
    pub fn add_source_specific_membership(
        scope: &Scope<'_>,
        this: Object<'_>,
        source: Option<HandleValue<'_>>,
        group: Option<HandleValue<'_>>,
        interface: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        let undefined = HandleValue::undefined();
        let (source, group) = (source.unwrap_or(undefined), group.unwrap_or(undefined));
        let syscall = "addSourceSpecificMembership";
        set_source_membership(scope, &state, source, group, interface, true, syscall)
    }

    /// <https://nodejs.org/api/dgram.html#socketdropsourcespecificmembershipsourceaddress-groupaddress-multicastinterface>:
    /// IPv4 groups only, natively. IPv6 groups throw `ENOSYS`, wasm throws `ENOTSUP`, and an
    /// unbound socket throws `EBADF`.
    #[method(length = 3)]
    pub fn drop_source_specific_membership(
        scope: &Scope<'_>,
        this: Object<'_>,
        source: Option<HandleValue<'_>>,
        group: Option<HandleValue<'_>>,
        interface: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        let undefined = HandleValue::undefined();
        let (source, group) = (source.unwrap_or(undefined), group.unwrap_or(undefined));
        let syscall = "dropSourceSpecificMembership";
        set_source_membership(scope, &state, source, group, interface, false, syscall)
    }

    /// <https://nodejs.org/api/dgram.html#socketref>
    #[method(name = "ref")]
    pub fn ref_impl<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        let state = state_of(scope, this)?;
        let data = state.data();
        if data.open {
            data.referenced.set_referenced(true);
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/dgram.html#socketunref>: the socket's receive stops keeping the
    /// event loop alive. Its sends and lookups still keep it alive until they complete.
    pub fn unref<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        let state = state_of(scope, this)?;
        let data = state.data();
        if data.open {
            data.referenced.set_referenced(false);
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/dgram.html#socketsetrecvbuffersizesize>
    #[method(length = 1)]
    pub fn set_recv_buffer_size(
        scope: &Scope<'_>,
        this: Object<'_>,
        size: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        buffer_size(scope, &state, size_number(size), true)?;
        Ok(())
    }

    /// <https://nodejs.org/api/dgram.html#socketsetsendbuffersizesize>
    #[method(length = 1)]
    pub fn set_send_buffer_size(
        scope: &Scope<'_>,
        this: Object<'_>,
        size: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        buffer_size(scope, &state, size_number(size), false)?;
        Ok(())
    }

    /// <https://nodejs.org/api/dgram.html#socketgetrecvbuffersize>
    pub fn get_recv_buffer_size(scope: &Scope<'_>, this: Object<'_>) -> Result<f64, Thrown> {
        let state = state_of(scope, this)?;
        buffer_size(scope, &state, 0.0, true)
    }

    /// <https://nodejs.org/api/dgram.html#socketgetsendbuffersize>
    pub fn get_send_buffer_size(scope: &Scope<'_>, this: Object<'_>) -> Result<f64, Thrown> {
        let state = state_of(scope, this)?;
        buffer_size(scope, &state, 0.0, false)
    }

    /// <https://nodejs.org/api/dgram.html#socketgetsendqueuesize>: the bytes of the sends the
    /// OS socket hasn't taken yet.
    pub fn get_send_queue_size(scope: &Scope<'_>, this: Object<'_>) -> Result<f64, Thrown> {
        let state = state_of(scope, this)?;
        let data = state.data();
        if !data.open {
            return Err(null_handle(scope, "getSendQueueSize"));
        }
        Ok(data
            .sends
            .iter()
            .fold(0.0, |total, send| total + send.length as f64))
    }

    /// <https://nodejs.org/api/dgram.html#socketgetsendqueuecount>: the number of sends the OS
    /// socket hasn't taken yet.
    pub fn get_send_queue_count(scope: &Scope<'_>, this: Object<'_>) -> Result<f64, Thrown> {
        let state = state_of(scope, this)?;
        let data = state.data();
        if !data.open {
            return Err(null_handle(scope, "getSendQueueCount"));
        }
        Ok(data.sends.len() as f64)
    }

    /// `socket._healthCheck()`, deprecated with DEP0112: throw unless the socket is open.
    #[method(name = "_healthCheck")]
    pub fn health_check_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        crate::process::deprecate_once(
            scope,
            "Socket.prototype._healthCheck() is deprecated",
            "DEP0112",
        )?;
        let state = state_of(scope, this)?;
        Ok(health_check(&state)?)
    }

    /// `socket._stopReceiving()`, deprecated with DEP0112: stop receiving.
    #[method(name = "_stopReceiving")]
    pub fn stop_receiving_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        crate::process::deprecate_once(
            scope,
            "Socket.prototype._stopReceiving() is deprecated",
            "DEP0112",
        )?;
        let state = state_of(scope, this)?;
        stop_receiving(&state);
        Ok(())
    }

    /// Define `Symbol.asyncDispose` and the deprecated accessors.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let dispose =
            Function::new_callback(scope, c"[Symbol.asyncDispose]", 0, async_dispose, ())?;
        js::class::define_well_known_symbol_property(
            scope,
            proto,
            js::native::SymbolCode::asyncDispose,
            *dispose,
        )?;
        for (index, name) in LEGACY_ACCESSORS.iter().enumerate() {
            let payload = index as f64;
            let getter = Function::new_callback(scope, name, 0, get_legacy, payload)?;
            let setter = Function::new_callback(scope, name, 1, set_legacy, payload)?;
            proto.define_accessor(scope, name, Some(*getter), Some(*setter), 0)?;
        }
        Ok(())
    }
}

/// `size` as `bufferSize` checks it: a number, and `NaN` for other values, which fail the check.
fn size_number(size: Option<HandleValue<'_>>) -> f64 {
    match size {
        Some(size) if size.is_number() => size.to_number(),
        _ => f64::NAN,
    }
}

/// `ERR_SOCKET_DGRAM_IS_CONNECTED`.
fn is_connected() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_SOCKET_DGRAM_IS_CONNECTED",
        "Already connected",
    )
}

/// `ERR_SOCKET_DGRAM_NOT_CONNECTED`.
fn not_connected() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_SOCKET_DGRAM_NOT_CONNECTED",
        "Not connected",
    )
}

/// The arguments of `send` as Node.js's `send` validates them, which differ for a connected
/// socket: `(msg[, offset, length][, port][, address][, callback])`.
fn parse_send<'s>(
    scope: &'s Scope<'_>,
    state: &DgramState<'_>,
    args: &[HandleValue<'s>],
) -> Result<SendRef<'s>, Thrown> {
    let arg = |index: usize| args.get(index).copied().unwrap_or(HandleValue::undefined());
    let (buffer, offset, length) = (arg(0), arg(1), arg(2));
    let (mut port, mut address, mut callback) = (arg(3), arg(4), arg(5));
    let truthy = js::conversion::to_boolean;
    let connected = state.data().connect_state == ConnectState::Connected;
    let mut sliced = None;
    if !connected {
        if truthy(address) || (truthy(port) && !js::function::is_callable(port)) {
            sliced = Some(slice_buffer(scope, buffer, offset, length)?);
        } else {
            callback = port;
            port = offset;
            address = length;
        }
    } else {
        if length.is_number() {
            sliced = Some(slice_buffer(scope, buffer, offset, length)?);
            if js::function::is_callable(port) {
                callback = port;
                port = HandleValue::null();
            }
        } else {
            callback = offset;
        }
        if truthy(port) || truthy(address) {
            return Err(is_connected().into());
        }
    }
    let chunks = match sliced {
        Some(chunk) => vec![chunk],
        None => buffer_chunks(scope, buffer)?,
    };
    let port = if connected {
        None
    } else {
        Some(crate::net::validate_port_allowing(
            scope, port, "Port", false,
        )?)
    };
    let mut callback = js::function::is_callable(callback).then_some(callback);
    if js::function::is_callable(address) {
        callback = Some(address);
        address = HandleValue::undefined();
    } else if !address.is_null_or_undefined() {
        validate_string(scope, address, "address")?;
    }
    Ok(SendRef {
        chunks,
        port,
        address,
        callback,
    })
}

/// Node.js's `bindSync`: bind to the port and IP address of `options` at once.
fn bind_sync<'s>(
    scope: &'s Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    health_check(state)?;
    let options = match options.filter(|options| !options.is_undefined()) {
        Some(options) => crate::errors::validate_plain_object(scope, options, "options", false)?,
        None => Object::new_plain(scope)?,
    };
    if state.data().bind_state != BindState::Unbound {
        return Err(already_bound().into());
    }
    let port = options.get_property(scope, c"port")?;
    let zero = scope.root_value(js::value::from_f64(0.0));
    let port = if port.is_null_or_undefined() {
        zero
    } else {
        port
    };
    let port = crate::net::validate_port(scope, port, "options.port")?;
    let address = options.get_property(scope, c"address")?;
    let address = if !js::conversion::to_boolean(address) {
        String::from(if state.data().ipv6 { "::" } else { "0.0.0.0" })
    } else {
        let text = validate_string(scope, address, "options.address")?;
        if ip_family(scope, address) == 0 {
            return Err(NodeError::invalid_arg_value(
                scope,
                "options.address",
                address,
                "must be a numeric IP address; bindSync does not perform DNS resolution",
            )
            .into());
        }
        text
    };
    state.data_mut().bind_state = BindState::Binding;
    if let Err(error) = bind_os_socket(state, &address, port) {
        state.data_mut().bind_state = BindState::Unbound;
        error.host_port(Some(&address), port).throw(scope);
        return Err(Thrown::Pending);
    }
    start_receiving(scope, socket, state)?;
    js::jobs::queue_native_tick(scope, emit_listening, socket, &[])?;
    socket_class::address(scope, socket)
}

/// The tick callback of `bindSync`, whose payload is the socket: emit `listening` unless the
/// socket closed.
fn emit_listening(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = js::function::cast_payload::<js::Object>(scope, payload);
    if socket_class::state_of(scope, socket)?.data().open {
        crate::events::emit(scope, socket, c"listening", &[])?;
    }
    Ok(js::value::undefined())
}

/// The tick callback of `connectSync`, whose payload is the socket: emit `connect` unless the
/// socket closed.
fn emit_connect(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = js::function::cast_payload::<js::Object>(scope, payload);
    if socket_class::state_of(scope, socket)?.data().open {
        crate::events::emit(scope, socket, c"connect", &[])?;
    }
    Ok(js::value::undefined())
}

/// Node.js's `connectSync`: connect to `port` of the IP address `address` at once.
fn connect_sync(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &DgramState<'_>,
    port: Option<HandleValue<'_>>,
    address: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    health_check(state)?;
    let port = port.unwrap_or(HandleValue::undefined());
    let port = crate::net::validate_port_allowing(scope, port, "Port", false)?;
    if state.data().connect_state != ConnectState::Disconnected {
        return Err(is_connected().into());
    }
    let address = address.unwrap_or(HandleValue::undefined());
    let empty = address.is_null_or_undefined() || crate::support::is_str(scope, address, c"")?;
    let address = if empty {
        String::from(if state.data().ipv6 {
            "::1"
        } else {
            "127.0.0.1"
        })
    } else {
        let text = validate_string(scope, address, "address")?;
        if ip_family(scope, address) == 0 {
            return Err(NodeError::invalid_arg_value(
                scope,
                "address",
                address,
                "must be a numeric IP address; connectSync does not perform DNS resolution",
            )
            .into());
        }
        text
    };
    let bind_state = state.data().bind_state;
    match bind_state {
        BindState::Unbound => {
            bind_sync(scope, socket, state, None)?;
        }
        BindState::Binding => return Err(already_bound().into()),
        BindState::Bound => {}
    }
    state.data_mut().connect_state = ConnectState::Connecting;
    let list = state.data().send_block_list.get(scope);
    if blocked(scope, list, &address, check_family(&address))? {
        state.data_mut().connect_state = ConnectState::Disconnected;
        return Err(ip_blocked(&address).into());
    }
    if let Err(error) = connect_os_socket(state, &address, port) {
        state.data_mut().connect_state = ConnectState::Disconnected;
        error.host_port(Some(&address), port).throw(scope);
        return Err(Thrown::Pending);
    }
    state.data_mut().connect_state = ConnectState::Connected;
    js::jobs::queue_native_tick(scope, emit_connect, socket, &[])?;
    Ok(())
}

#[core_runtime::jsmodule(node_args, name = "node:dgram", default_namespace)]
pub mod module {
    use super::*;

    pub use super::socket_class as Socket;

    /// <https://nodejs.org/api/dgram.html#dgramcreatesocketoptions-callback>
    #[function(length = 2)]
    pub fn create_socket<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = super::socket_class::class_constructor(scope)?;
        let args: Vec<HandleValue<'_>> =
            args.iter().map(|arg| scope.root_value(arg.get())).collect();
        Function::construct(scope, class, &args)
    }
}

/// Register the classes of `node:dgram` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: js::Object<'_>) -> Result<(), ExnThrown> {
    DgramState::add_to_global(scope, global);
    BindCallback::add_to_global(scope, global);
    LookupOp::add_to_global(scope, global);
    SignalLink::add_to_global(scope, global);
    socket_class::register(scope)?;
    Ok(())
}
