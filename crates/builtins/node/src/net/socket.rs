// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `net.Socket`: a `Duplex` over a `platform::net` TCP connection, or over a `platform::child` pipe
//! to a child process.
//!
//! A socket's state is a [`SocketState`], which extends the state of the stream with the
//! socket's connection, its byte counts and its idle timer. Each read and write takes its half of
//! the connection out of the state for as long as it runs. `destroy` cancels the connect, read and
//! write in progress and drops the connection.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};
use platform::net::{Connection, ConnectionReader, ConnectionWriter, NetError, Received};

use super::blocklist::Rules;
use crate::buffer::buffer_from_vec;
use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};
use crate::fs::UvError;
use crate::stream::state::{StreamState, StreamStateImpl, WritableSide, W};
use crate::support::{chunk_bytes, spawn_cancellable_with_ref, until_cancelled, Canceller};

/// The halves of a TCP connection or a pipe that no read or write in progress holds, and the
/// cancellations of the read and the write in progress.
#[derive(Default)]
pub(crate) struct TcpLink {
    reader: Option<LinkReader>,
    writer: Option<LinkWriter>,
    cancel_read: Option<Canceller>,
    cancel_write: Option<Canceller>,
    /// The descriptor of a pipe.
    fd: Option<i32>,
    /// Whether the connection stopped delivering bytes, and the next read gets its outcome.
    stopped: bool,
}

/// The reading half of a TCP connection or a pipe.
pub(crate) enum LinkReader {
    Tcp(ConnectionReader),
    Pipe(platform::child::PipeReader),
}

impl LinkReader {
    /// The next bytes received, or how the connection ended, as [`ConnectionReader::receive`]
    /// returns them.
    async fn receive(&mut self) -> Result<Received, NetError> {
        match self {
            LinkReader::Tcp(reader) => reader.receive().await,
            LinkReader::Pipe(reader) => Ok(match reader.read().await? {
                Some(bytes) => Received::Bytes(bytes),
                None => Received::End,
            }),
        }
    }
}

/// The writing half of a TCP connection or a pipe.
pub(crate) enum LinkWriter {
    Tcp(ConnectionWriter),
    Pipe(platform::child::PipeWriter),
}

impl LinkWriter {
    /// Send all of `data`.
    async fn write(&mut self, data: Vec<u8>) -> Result<(), NetError> {
        match self {
            LinkWriter::Tcp(writer) => writer.write(data).await,
            LinkWriter::Pipe(writer) => writer.write(data).await,
        }
    }

    /// Send as much of `data` as the connection takes without waiting, and return the bytes not
    /// sent.
    fn try_write(&mut self, data: Vec<u8>) -> Vec<u8> {
        match self {
            LinkWriter::Tcp(writer) => writer.try_write(data),
            LinkWriter::Pipe(writer) => writer.try_write(data),
        }
    }

    /// End this side of the connection.
    async fn shutdown(&mut self) -> Result<(), NetError> {
        match self {
            LinkWriter::Tcp(writer) => writer.shutdown().await,
            LinkWriter::Pipe(writer) => writer.shutdown().await,
        }
    }
}

/// What a socket reads from and writes to.
#[derive(Default)]
pub(crate) enum Link {
    /// Nothing: the socket isn't connected, or was destroyed.
    #[default]
    None,
    /// A connect in progress, with its cancellation.
    Connecting(Canceller),
    /// A TCP connection.
    Tcp(TcpLink),
    /// A pipe to a child process.
    Pipe(TcpLink),
    /// The standard input of a `tty.ReadStream`.
    Stdin(crate::tty::StdinLink),
}

/// A write or shutdown that came while the socket was connecting: the bytes, or `None` for the
/// shutdown, the callback, and the `close` listener of a write.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct DeferredWrite {
    #[no_trace]
    bytes: Option<Vec<u8>>,
    callback: Heap<Value>,
    on_close: Option<Heap<js::object::Object>>,
}

/// The state of a `net.Socket`: the state of the stream, and the socket's own.
#[core_runtime::jsclass(hidden, extends = StreamState)]
pub(crate) struct SocketState {
    parent: StreamStateImpl,
    #[no_trace]
    pub(crate) link: Link,
    /// Whether `_read` was called while no read could start, so a read starts once one can.
    #[no_trace]
    pub(crate) read_pending: bool,
    #[no_trace]
    bytes_read: f64,
    /// The bytes handed to the connection, or waiting for the socket to connect.
    #[no_trace]
    bytes_dispatched: f64,
    /// The idle timeout in milliseconds, or 0 for none.
    #[no_trace]
    timeout: f64,
    /// The ID of the idle timer, while it is pending.
    #[no_trace]
    timer: Option<u64>,
    /// When the pending idle timer fires, in milliseconds of the monotonic clock.
    #[no_trace]
    timer_due: f64,
    /// When the socket was last active, in milliseconds of the monotonic clock.
    #[no_trace]
    active_at: f64,
    #[no_trace]
    local: Option<SocketAddr>,
    /// The peer's address, which stays known after the connection closes once `_peername` was
    /// defined.
    #[no_trace]
    peer: Option<SocketAddr>,
    #[no_trace]
    peername_defined: bool,
    /// The write or shutdown waiting for the connect in progress.
    deferred: Option<DeferredWrite>,
    /// The callback of the write or shutdown in progress.
    write_callback: Option<Heap<Value>>,
    /// The value of `_handle`.
    handle: Option<Heap<js::object::Object>>,
    /// The server that accepted the socket.
    pub(crate) server: Option<Heap<js::object::Object>>,
    /// Whether the socket's connect, reads and writes keep the event loop alive: unset by `unref`
    /// and set again by `ref`.
    #[no_trace]
    referenced: js::promise::FutureRef,
    /// The connect waiting for the `lookup` option's callback.
    #[no_trace]
    lookup: Option<PendingLookup>,
    /// Whether the socket destroys itself once it reads anything, which [`guard_reads`] sets.
    #[no_trace]
    read_guard: bool,
    /// Whether a child process got the socket's pipe as one of its descriptors, so the socket
    /// doesn't read from it.
    #[no_trace]
    used_as_stdio: bool,
}

/// A connect waiting for the addresses the `lookup` option's callback gives: the host and the
/// target, and whether the callback gets all addresses.
pub(crate) struct PendingLookup {
    host: String,
    target: ConnectTarget,
    all: bool,
}

/// Where a connect goes besides the host: the port, the address family of the addresses the
/// host resolves to (4, 6, or 0 for both) and the lookup's `getaddrinfo` flags, the `BlockList`
/// rules, and the local address and port to connect from.
pub(crate) struct ConnectTarget {
    port: u16,
    family: u8,
    hints: u32,
    rules: Option<Rules>,
    local_ip: Option<IpAddr>,
    local_port: u16,
}

#[core_runtime::jsmethods]
impl SocketState {}

impl SocketStateImpl {
    /// Whether both sides of the socket's stream are destroyed.
    pub(crate) fn destroyed(&self) -> bool {
        self.parent.duplex_destroyed()
    }

    /// The peer's IP address, while the socket is connected.
    pub(crate) fn peer_ip(&self) -> Option<IpAddr> {
        self.peer
            .filter(|_| matches!(self.link, Link::Tcp(_)))
            .map(|addr| addr.ip())
    }

    /// Remove the write or shutdown waiting for the connect, and return its bytes, callback and
    /// `close` listener.
    #[allow(clippy::type_complexity)]
    fn take_deferred<'s>(
        &mut self,
        scope: &'s Scope<'_>,
    ) -> Option<(Option<Vec<u8>>, HandleValue<'s>, Option<Object<'s>>)> {
        let deferred = self.deferred.as_mut()?;
        let taken = (
            deferred.bytes.take(),
            deferred.callback.get(scope),
            deferred.on_close.take_rooted(scope),
        );
        self.deferred = None;
        Some(taken)
    }
}

/// The state of `socket`, if it is a socket.
pub(crate) fn state_of<'s>(
    scope: &'s Scope<'_>,
    socket: Object<'s>,
) -> Result<Option<SocketState<'s>>, ExnThrown> {
    socket_class::try_state_of(scope, socket)
}

/// Whether `socket` is a socket connected over TCP.
pub(crate) fn is_tcp(scope: &Scope<'_>, socket: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(state_of(scope, socket)?.is_some_and(|state| matches!(state.data().link, Link::Tcp(_))))
}

/// The family name of `addr`, `IPv4` or `IPv6`.
pub(crate) fn family(addr: &SocketAddr) -> &'static str {
    if addr.is_ipv4() {
        "IPv4"
    } else {
        "IPv6"
    }
}

/// `dns.ADDRCONFIG`, the lookup hint a connect without an address family passes.
const ADDRCONFIG: f64 = 1024.0;

/// The properties of the address objects that `address()` returns and `_peername` holds, in
/// Node's order.
pub(crate) static ADDRESS_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"address"),
    TemplateKey::Name(c"family"),
    TemplateKey::Name(c"port"),
]);

/// `{ address, family, port }` for `addr`.
pub(crate) fn address_object<'s>(
    scope: &'s Scope<'_>,
    addr: SocketAddr,
) -> Result<Object<'s>, ExnThrown> {
    Object::new_plain_with_template(
        scope,
        &ADDRESS_PROPERTIES,
        &[
            addr.ip().to_string().as_str().to_jsval_throwing(scope)?,
            family(&addr).to_jsval_throwing(scope)?,
            f64::from(addr.port()).to_jsval_throwing(scope)?,
        ],
    )
}

/// The current monotonic clock reading in milliseconds.
fn now_ms() -> f64 {
    platform::clock::monotonic_ns() as f64 / 1e6
}

/// Make `connection` the connection of `socket`.
pub(super) fn attach(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: SocketState<'_>,
    connection: Connection,
) -> Result<(), ExnThrown> {
    let Connection {
        reader,
        writer,
        local_addr,
        peer_addr,
    } = connection;
    let handle = Object::new_plain(scope)?;
    {
        let mut data = state.data_mut();
        data.link = Link::Tcp(TcpLink {
            reader: Some(LinkReader::Tcp(reader)),
            writer: Some(LinkWriter::Tcp(writer)),
            cancel_read: None,
            cancel_write: None,
            fd: None,
            stopped: false,
        });
        data.local = Some(local_addr);
        data.peer = Some(peer_addr);
        data.handle = Some(Heap::from(handle));
    }
    socket.set_property(scope, c"connecting", false)
}

/// Make `pipe` the pipe of `socket`, a socket of a child process's standard stream.
pub(crate) fn attach_pipe(
    scope: &Scope<'_>,
    socket: Object<'_>,
    pipe: platform::child::Pipe,
) -> Result<(), ExnThrown> {
    let state = socket_class::state_of(scope, socket)?;
    let fd = pipe.as_raw_fd();
    let (reader, writer) = pipe
        .split()
        .map_err(|error| UvError::new(error, "pipe").throw(scope))?;
    let handle = Object::new_plain(scope)?;
    let mut data = state.data_mut();
    data.link = Link::Pipe(TcpLink {
        reader: Some(LinkReader::Pipe(reader)),
        writer: Some(LinkWriter::Pipe(writer)),
        cancel_read: None,
        cancel_write: None,
        fd: Some(fd),
        stopped: false,
    });
    data.handle = Some(Heap::from(handle));
    Ok(())
}

/// The descriptor of the pipe of `socket`, if it is a socket over a pipe to a child process.
pub(crate) fn pipe_fd(scope: &Scope<'_>, socket: Object<'_>) -> Result<Option<i32>, ExnThrown> {
    let Some(state) = state_of(scope, socket)? else {
        return Ok(None);
    };
    let fd = match &state.data().link {
        Link::Pipe(pipe) => pipe.fd,
        _ => None,
    };
    Ok(fd)
}

/// Stop reading from the pipe of `socket`, which a child process got as one of its descriptors,
/// and pause the socket, as Node.js does for a stream it passes to a child.
pub(crate) fn use_as_stdio(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    let Some(state) = state_of(scope, socket)? else {
        return Ok(());
    };
    let cancel = {
        let mut data = state.data_mut();
        data.used_as_stdio = true;
        match &mut data.link {
            Link::Pipe(pipe) => pipe.cancel_read.take(),
            _ => None,
        }
    };
    if let Some(cancel) = cancel {
        let _ = cancel.send(());
    }
    if let Some(side) = crate::stream::state::ReadableSide::try_of(scope, socket)? {
        side.get_mut()
            .flags
            .remove(crate::stream::state::R::READING);
    }
    Function::call_by_name(scope, socket.handle(), c"pause", js::function::EmptyArgs)?;
    socket.set_property(scope, c"readableFlowing", false)
}

/// Whether a child process got the pipe of `socket` as one of its descriptors.
pub(crate) fn used_as_stdio(scope: &Scope<'_>, socket: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(state_of(scope, socket)?.is_some_and(|state| state.data().used_as_stdio))
}

/// Drop the connection of `socket` and cancel its connect, read and write in progress, without
/// emitting anything.
pub(super) fn release(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: SocketState<'_>,
) -> Result<(), ExnThrown> {
    let (link, connecting) = {
        let mut data = state.data_mut();
        data.handle = None;
        data.lookup = None;
        let link = std::mem::take(&mut data.link);
        let connecting = matches!(link, Link::Connecting(_));
        (link, connecting)
    };
    match link {
        Link::Connecting(canceller) => {
            let _ = canceller.send(());
        }
        Link::Tcp(tcp) | Link::Pipe(tcp) => {
            for canceller in [tcp.cancel_read, tcp.cancel_write].into_iter().flatten() {
                let _ = canceller.send(());
            }
        }
        Link::Stdin(_) | Link::None => {}
    }
    if connecting {
        socket.set_property(scope, c"connecting", false)?;
    }
    Ok(())
}

/// Record activity on the socket of `state`, which restarts its idle timer. The timer starts if
/// the socket has a timeout and the timer isn't pending.
fn touch(scope: &Scope<'_>, socket: Object<'_>, state: &SocketState<'_>) -> Result<(), ExnThrown> {
    let (timeout, pending) = {
        let mut data = state.data_mut();
        data.active_at = now_ms();
        (data.timeout, data.timer.is_some())
    };
    if timeout > 0.0 && !pending {
        arm_timer(scope, socket, state, timeout)?;
    }
    Ok(())
}

/// Start the idle timer of `socket`, which fires after `delay` milliseconds and doesn't keep the
/// event loop alive.
fn arm_timer(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &SocketState<'_>,
    delay: f64,
) -> Result<(), ExnThrown> {
    let callback = crate::support::shared_function(scope, on_idle_timer, 0)?;
    let callable = js::Callable::from_jsval(scope, callback.to_jsval_throwing(scope)?, ())
        .map_err(|e| e.throw(scope))?;
    let id = core_runtime::event_loop::timer::start_timer(
        scope,
        callable,
        socket.to_jsval_throwing(scope)?,
        &[],
        Duration::from_millis(delay.ceil() as u64),
        false,
        None,
    )
    .ok_or_else(|| core_runtime::event_loop::throw_no_event_loop(scope, "socket.setTimeout"))?;
    core_runtime::event_loop::with_active_event_loop(|event_loop| {
        event_loop.set_js_timer_ref(id, false)
    });
    let mut data = state.data_mut();
    data.timer = Some(id);
    data.timer_due = now_ms() + delay.ceil();
    Ok(())
}

/// Cancel the idle timer of the socket of `state`, if it is pending.
fn clear_timer(state: &SocketState<'_>) {
    if let Some(id) = state.data_mut().timer.take() {
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.clear_js_timer(id)
        });
    }
}

/// The idle timer callback, whose `this` is the socket: emit `timeout` if the socket was idle for
/// its timeout, and otherwise wait for the rest of it.
fn on_idle_timer(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let Some(state) = state_of(scope, socket)? else {
        return Ok(js::value::undefined());
    };
    let (timeout, active_at) = {
        let mut data = state.data_mut();
        data.timer = None;
        (data.timeout, data.active_at)
    };
    if timeout <= 0.0 {
        return Ok(js::value::undefined());
    }
    let remaining = active_at + timeout - now_ms();
    if remaining >= 1.0 {
        arm_timer(scope, socket, &state, remaining)?;
        return Ok(js::value::undefined());
    }
    crate::events::emit(scope, socket, c"timeout", &[])?;
    Ok(js::value::undefined())
}

/// The reason a connect failed.
enum ConnectFailure {
    /// A network error, and the address it occurred for, if any.
    Net(NetError, Option<SocketAddr>),
    /// The socket's `BlockList` blocks the address.
    Blocked(IpAddr),
}

/// The body of the connect: resolve `host` to the addresses of the target's address family and
/// lookup flags, unless `addrs` gives its addresses, and connect to the first address that the
/// target's rules don't block and that accepts. The connect is from the target's local address
/// and port if it has either, with the unspecified address of the address's family in place of a
/// missing local address.
async fn connect_to(
    host: String,
    addrs: Option<Vec<IpAddr>>,
    target: ConnectTarget,
) -> Result<Connection, ConnectFailure> {
    let ConnectTarget {
        port,
        family,
        hints,
        rules,
        local_ip,
        local_port,
    } = target;
    let addrs = match addrs {
        Some(addrs) => addrs,
        None => {
            let addrs = platform::net::resolve(&host)
                .await
                .map_err(|error| ConnectFailure::Net(error, None))?;
            crate::dns::addresses_of_family(addrs, family, hints)
        }
    };
    let mut last = None;
    for addr in addrs {
        if rules.as_ref().is_some_and(|rules| rules.blocks(addr)) {
            last = Some(ConnectFailure::Blocked(addr));
            continue;
        }
        let local = (local_ip.is_some() || local_port != 0).then(|| {
            let ip = local_ip.unwrap_or(match addr {
                IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
            });
            SocketAddr::new(ip, local_port)
        });
        let addr = SocketAddr::new(addr, port);
        match platform::net::connect_stream_from(addr, local, None).await {
            Ok(connection) => return Ok(connection),
            Err(error) => last = Some(ConnectFailure::Net(error, Some(addr))),
        }
    }
    Err(last.unwrap_or(ConnectFailure::Net(NetError::NoAddress(host), None)))
}

/// Make `socket` destroy itself once it reads data or the end of the connection while `guard`,
/// without emitting them, and start reading now. The socket is destroyed at once if its
/// connection already stopped delivering bytes. Sockets of other classes are left alone.
pub(crate) fn guard_reads(
    scope: &Scope<'_>,
    socket: Object<'_>,
    guard: bool,
) -> Result<(), ExnThrown> {
    let Some(state) = state_of(scope, socket)? else {
        return Ok(());
    };
    state.data_mut().read_guard = guard;
    if !guard {
        return Ok(());
    }
    let stopped = match &state.data().link {
        Link::Tcp(tcp) => tcp.stopped,
        _ => return Ok(()),
    };
    if stopped {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
        return Ok(());
    }
    start_read(scope, socket, &state)
}

/// Start the first read of `socket` unless it is paused, so `end` is emitted even without a
/// consumer.
pub(crate) fn read_zero(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    let paused =
        Function::call_by_name(scope, socket.handle(), c"isPaused", js::function::EmptyArgs)?;
    if !js::conversion::to_boolean(paused) {
        let zero = scope.root_value(js::value::from_f64(0.0));
        Function::call_by_name(scope, socket.handle(), c"read", &[zero])?;
    }
    Ok(())
}

/// The completion of a connect: emit `connect` and `ready` and start reading, or destroy the
/// socket with the error.
fn connect_done(
    scope: &Scope<'_>,
    socket: Object<'_>,
    result: Result<HandleValue<'_>, HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    if let Err(error) = result {
        Function::call_by_name(scope, socket.handle(), c"destroy", &[error])?;
        return Ok(());
    }
    let Some(state) = state_of(scope, socket)? else {
        return Ok(());
    };
    if state.data().destroyed() || !matches!(state.data().link, Link::Tcp(_)) {
        return Ok(());
    }
    touch(scope, socket, &state)?;
    crate::events::emit(scope, socket, c"connect", &[])?;
    let deferred = state.data_mut().take_deferred(scope);
    if let Some((bytes, callback, on_close)) = deferred {
        if let Some(on_close) = on_close {
            crate::events::listen(scope, socket, c"removeListener", c"close", on_close)?;
        }
        write_or_shutdown(scope, socket, &state, bytes, callback)?;
    }
    crate::events::emit(scope, socket, c"ready", &[])?;
    let pending = std::mem::take(&mut state.data_mut().read_pending);
    if pending {
        start_read(scope, socket, &state)?;
    }
    read_zero(scope, socket)
}

/// Connect `socket` to `target` on `host`, or on `addrs` if they are given, as
/// [`connect_to`] does.
fn connect(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &SocketState<'_>,
    host: String,
    addrs: Option<Vec<IpAddr>>,
    target: ConnectTarget,
) -> Result<(), ExnThrown> {
    socket.set_property(scope, c"connecting", true)?;
    let name = host.clone();
    let referenced = state.data().referenced.clone();
    let canceller = spawn_cancellable_with_ref(
        scope,
        socket,
        |cancelled| until_cancelled(cancelled, connect_to(name, addrs, target)),
        move |scope, socket, result| {
            let Some(state) = state_of(scope, socket)? else {
                return Ok(HandleValue::undefined());
            };
            if !matches!(state.data().link, Link::Connecting(_)) {
                // The socket was destroyed, which drops the connection.
                return Ok(HandleValue::undefined());
            }
            match result {
                Ok(connection) => {
                    attach(scope, socket, state, connection)?;
                    Ok(HandleValue::undefined())
                }
                Err(ConnectFailure::Blocked(ip)) => Err(NodeError::new(
                    ErrorKind::Error,
                    "ERR_IP_BLOCKED",
                    format!("IP({ip}) is blocked by net.BlockList"),
                )
                .throw(scope)),
                Err(ConnectFailure::Net(error, Some(addr))) => Err(UvError::net(&error, "connect")
                    .suffix(format!(" {addr}"))
                    .addr(addr)
                    .throw(scope)),
                Err(ConnectFailure::Net(error, None)) => Err(UvError::net(&error, "getaddrinfo")
                    .suffix(format!(" {host}"))
                    .hostname(&host)
                    .throw(scope)),
            }
        },
        connect_done,
        referenced,
    )?;
    state.data_mut().link = Link::Connecting(canceller);
    Ok(())
}

/// Call the `lookup` option `lookup` of the connect of `socket` with the host of `pending` and the
/// `family` and `hints` of `options`, and wait for its callback, as Node.js's `lookupAndConnect`
/// does.
fn connect_after_lookup(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &SocketState<'_>,
    lookup: HandleValue<'_>,
    options: Object<'_>,
    pending: PendingLookup,
) -> Result<(), ExnThrown> {
    socket.set_property(scope, c"connecting", true)?;
    let family = options.get_property(scope, c"family")?;
    let hints = options.get_property(scope, c"hints")?;
    let host = pending.host.clone();
    let all = pending.all;
    let (canceller, _) = futures_channel::oneshot::channel();
    {
        let mut data = state.data_mut();
        data.link = Link::Connecting(canceller);
        data.lookup = Some(pending);
    }
    let options = Object::new_plain(scope)?;
    let family = if crate::support::is_str(scope, family, c"IPv4")? {
        scope.root_value(js::value::from_f64(4.0))
    } else if crate::support::is_str(scope, family, c"IPv6")? {
        scope.root_value(js::value::from_f64(6.0))
    } else {
        family
    };
    options.set_property(scope, c"family", family)?;
    let hints = if js::conversion::to_boolean(hints) {
        hints
    } else {
        scope.root_value(js::value::from_f64(0.0))
    };
    let any_family = !(family.is_number() && matches!(family.to_number(), 4.0 | 6.0));
    if any_family && hints.is_number() && hints.to_number() == 0.0 && !cfg!(windows) {
        options.set_property(scope, c"hints", ADDRCONFIG)?;
    } else {
        options.set_property(scope, c"hints", hints)?;
    }
    if all {
        options.set_property(scope, c"all", true)?;
    }
    let done = Function::new_callback(scope, c"emitLookup", 3, lookup_done, socket)?;
    let args = [
        host.as_str().to_jsval_throwing(scope)?,
        options.to_jsval_throwing(scope)?,
        (*done).to_jsval_throwing(scope)?,
    ];
    Function::call(scope, HandleValue::undefined(), lookup, &args)?;
    Ok(())
}

/// The tick callback whose payload is a socket and whose argument is an error: destroy the
/// socket with the error, as Node.js's `connectErrorNT` does.
fn connect_error_tick(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = js::function::cast_payload::<js::Object>(scope, payload);
    Function::call_by_name(scope, socket.handle(), c"destroy", &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// `ERR_INVALID_IP_ADDRESS` for `address`.
fn invalid_ip(scope: &Scope<'_>, address: HandleValue<'_>) -> NodeError {
    let shown = crate::errors::describe_value(scope, address);
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_INVALID_IP_ADDRESS",
        format!("Invalid IP address: {shown}"),
    )
}

/// The IP address `address` is, if it is a string of one, and its family `family` is 4 or 6.
fn lookup_address(
    scope: &Scope<'_>,
    address: HandleValue<'_>,
    family: HandleValue<'_>,
) -> Option<IpAddr> {
    let family = family.is_number().then(|| family.to_number())?;
    let address = crate::support::rust_string(scope, address)?;
    let ip = address.parse::<IpAddr>().ok()?;
    matches!(family, 4.0 | 6.0).then_some(ip)
}

/// The callback of the `lookup` option, whose payload is the socket and whose arguments are the
/// error and the address and its family, or all addresses: emit `lookup` for each address, and
/// connect to the addresses of family 4 or 6, alternating between the families, starting with the
/// family of the first address. The socket is destroyed with the error, or with
/// `ERR_INVALID_IP_ADDRESS` or `ERR_INVALID_ADDRESS_FAMILY` if no address is valid. A destroyed
/// socket ignores the callback.
fn lookup_done(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = js::function::cast_payload::<js::Object>(scope, payload);
    let Some(state) = state_of(scope, socket)? else {
        return Ok(js::value::undefined());
    };
    let Some(pending) = state.data_mut().lookup.take() else {
        return Ok(js::value::undefined());
    };
    let PendingLookup { host, target, all } = pending;
    let host_value = host.as_str().to_jsval_throwing(scope)?;
    let error = args.get(0);
    // Whether the socket still connects: a `lookup` listener can destroy it.
    let connecting = || matches!(state.data().link, Link::Connecting(_));
    let fail = |scope: &Scope<'_>, error: HandleValue<'_>| -> Result<Value, ExnThrown> {
        js::jobs::queue_native_tick(scope, connect_error_tick, socket, &[error])?;
        Ok(js::value::undefined())
    };
    let undefined = HandleValue::undefined();
    if !all {
        let (address, family) = (args.get(1), args.get(2));
        crate::events::emit(
            scope,
            socket,
            c"lookup",
            &[error, address, family, host_value],
        )?;
        if !connecting() {
            return Ok(js::value::undefined());
        }
        if js::conversion::to_boolean(error) {
            return fail(scope, error);
        }
        let Some(ip) = lookup_address(scope, address, family) else {
            let ip = crate::support::rust_string(scope, address)
                .and_then(|address| address.parse::<IpAddr>().ok());
            let error = match ip {
                None => invalid_ip(scope, address),
                Some(_) => invalid_family(scope, family, &host, target.port),
            };
            let error = error.to_value(scope)?;
            return fail(scope, error);
        };
        connect(scope, socket, &state, host, Some(vec![ip]), target)?;
        return Ok(js::value::undefined());
    }
    if !connecting() {
        return Ok(js::value::undefined());
    }
    if js::conversion::to_boolean(error) {
        crate::events::emit(
            scope,
            socket,
            c"lookup",
            &[error, undefined, undefined, host_value],
        )?;
        return fail(scope, error);
    }
    let addresses = crate::support::array_elements(scope, args.get(1))?.unwrap_or_default();
    let mut groups: [Vec<IpAddr>; 2] = [Vec::new(), Vec::new()];
    let mut first_family = None;
    let mut first = None;
    for entry in &addresses {
        let entry = Object::from_value(scope, **entry).map_err(|e| e.throw(scope))?;
        let address = entry.get_property(scope, c"address")?;
        let family = entry.get_property(scope, c"family")?;
        if first.is_none() {
            first = Some((address, family));
        }
        crate::events::emit(
            scope,
            socket,
            c"lookup",
            &[error, address, family, host_value],
        )?;
        if !connecting() {
            return Ok(js::value::undefined());
        }
        if let Some(ip) = lookup_address(scope, address, family) {
            let ipv6 = ip.is_ipv6();
            let first_ipv6 = *first_family.get_or_insert(ipv6);
            let group = &mut groups[usize::from(ipv6 != first_ipv6)];
            if !group.contains(&ip) {
                group.push(ip);
            }
        }
    }
    if groups[0].is_empty() && groups[1].is_empty() {
        let Some((address, family)) = first else {
            return Ok(js::value::undefined());
        };
        let valid_ip = crate::support::rust_string(scope, address)
            .is_some_and(|address| address.parse::<IpAddr>().is_ok());
        let error = if !valid_ip {
            invalid_ip(scope, address)
        } else {
            invalid_family(scope, family, &host, target.port)
        };
        let error = error.to_value(scope)?;
        return fail(scope, error);
    }
    let [preferred, other] = groups;
    let mut addrs = Vec::with_capacity(preferred.len() + other.len());
    for index in 0..preferred.len().max(other.len()) {
        addrs.extend(preferred.get(index));
        addrs.extend(other.get(index));
    }
    connect(scope, socket, &state, host, Some(addrs), target)?;
    Ok(js::value::undefined())
}

/// `ERR_INVALID_ADDRESS_FAMILY` for the family `family` of an address of `host` and `port`.
fn invalid_family(scope: &Scope<'_>, family: HandleValue<'_>, host: &str, port: u16) -> NodeError {
    let shown = crate::errors::describe_value(scope, family);
    NodeError::new(
        ErrorKind::RangeError,
        "ERR_INVALID_ADDRESS_FAMILY",
        format!("Invalid address family: {shown} {host}:{port}"),
    )
}

/// The completion of a read: push the `Buffer` read, or `null` at the end, or destroy the socket
/// with the error. A cancelled read, which gives `undefined`, pushes nothing. `false` means that
/// the connection stopped delivering bytes, and the next read is started to get its outcome.
fn read_done(
    scope: &Scope<'_>,
    socket: Object<'_>,
    result: Result<HandleValue<'_>, HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let chunk = match result {
        Ok(chunk) if chunk.is_undefined() => return Ok(()),
        Ok(chunk) => chunk,
        Err(error) => {
            Function::call_by_name(scope, socket.handle(), c"destroy", &[error])?;
            return Ok(());
        }
    };
    let state = state_of(scope, socket)?;
    if state.as_ref().is_some_and(|state| state.data().read_guard) {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
        return Ok(());
    }
    if chunk.is_boolean() {
        if let Some(state) = &state {
            start_read(scope, socket, state)?;
        }
        return Ok(());
    }
    if let Some(view) = Object::from_value(scope, chunk)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
    {
        if let Some(state) = &state {
            state.data_mut().bytes_read += view.byte_length() as f64;
            touch(scope, socket, state)?;
        }
    }
    Function::call_by_name(scope, socket.handle(), c"push", &[chunk])?;
    if chunk.is_null() {
        // Reading after the end emits `end` even without a consumer.
        let zero = scope.root_value(js::value::from_f64(0.0));
        Function::call_by_name(scope, socket.handle(), c"read", &[zero])?;
    }
    Ok(())
}

/// Read the next bytes of the connection of `socket`, unless a read is in progress.
fn start_read(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &SocketState<'_>,
) -> Result<(), ExnThrown> {
    let reader = match &mut state.data_mut().link {
        Link::Tcp(tcp) | Link::Pipe(tcp) => tcp.reader.take(),
        _ => None,
    };
    let Some(mut reader) = reader else {
        return Ok(());
    };
    let referenced = state.data().referenced.clone();
    let canceller = spawn_cancellable_with_ref(
        scope,
        socket,
        |cancelled| async move {
            let result = until_cancelled(cancelled, reader.receive()).await;
            Some((reader, result))
        },
        |scope, socket, (reader, result)| {
            let stopped = matches!(result, Some(Ok(Received::Stopped)));
            if let Some(state) = state_of(scope, socket)? {
                if let Link::Tcp(tcp) | Link::Pipe(tcp) = &mut state.data_mut().link {
                    tcp.reader = Some(reader);
                    tcp.cancel_read = None;
                    tcp.stopped |= stopped;
                }
            }
            match result {
                None => Ok(HandleValue::undefined()),
                Some(Ok(Received::Bytes(bytes))) => {
                    buffer_from_vec(scope, bytes)?.to_jsval_throwing(scope)
                }
                Some(Ok(Received::End)) => Ok(HandleValue::null()),
                Some(Ok(Received::Stopped)) => Ok(scope.root_value(js::value::from_bool(false))),
                Some(Err(error)) => Err(UvError::net(&error, "read").throw(scope)),
            }
        },
        read_done,
        referenced,
    )?;
    if let Link::Tcp(tcp) | Link::Pipe(tcp) = &mut state.data_mut().link {
        tcp.cancel_read = Some(canceller);
    }
    Ok(())
}

/// The completion of a write or shutdown: restart the idle timer after a write, and call the
/// callback, with the error if it failed.
fn write_done(
    scope: &Scope<'_>,
    socket: Object<'_>,
    result: Result<HandleValue<'_>, HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let Some(state) = state_of(scope, socket)? else {
        return Ok(());
    };
    let callback = {
        let mut data = state.data_mut();
        let callback = data
            .write_callback
            .as_ref()
            .map(|callback| callback.get(scope));
        data.write_callback = None;
        callback
    };
    let Some(callback) = callback else {
        return Ok(());
    };
    match result {
        Ok(wrote) => {
            if wrote.is_boolean() && wrote.to_boolean() {
                touch(scope, socket, &state)?;
            }
            Function::call(
                scope,
                HandleValue::undefined(),
                callback,
                js::function::EmptyArgs,
            )?;
        }
        Err(error) => {
            Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        }
    }
    Ok(())
}

/// Call `callback` with `ERR_SOCKET_CLOSED`.
fn socket_closed(scope: &Scope<'_>, callback: HandleValue<'_>) -> Result<(), ExnThrown> {
    let error = NodeError::new(ErrorKind::Error, "ERR_SOCKET_CLOSED", "Socket is closed")
        .to_value(scope)?;
    Function::call(scope, HandleValue::undefined(), callback, &[error])?;
    Ok(())
}

/// Write `bytes` to the connection of `socket`, or shut its sending side down for `None`, and call
/// `callback` once done. The bytes the connection takes without waiting are written at once, and
/// `callback` is called before this returns if that is all of them.
fn write_or_shutdown(
    scope: &Scope<'_>,
    socket: Object<'_>,
    state: &SocketState<'_>,
    bytes: Option<Vec<u8>>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let bytes = match bytes {
        Some(bytes) if !bytes.is_empty() => {
            let unwritten = match &mut state.data_mut().link {
                Link::Tcp(TcpLink {
                    writer: Some(writer),
                    ..
                })
                | Link::Pipe(TcpLink {
                    writer: Some(writer),
                    ..
                }) => Some(writer.try_write(bytes)),
                _ => None,
            };
            let Some(unwritten) = unwritten else {
                return socket_closed(scope, callback);
            };
            if unwritten.is_empty() {
                touch(scope, socket, state)?;
                Function::call(
                    scope,
                    HandleValue::undefined(),
                    callback,
                    js::function::EmptyArgs,
                )?;
                return Ok(());
            }
            Some(unwritten)
        }
        bytes => bytes,
    };
    let writer = match &mut state.data_mut().link {
        Link::Tcp(tcp) | Link::Pipe(tcp) => tcp.writer.take(),
        _ => None,
    };
    let Some(mut writer) = writer else {
        return socket_closed(scope, callback);
    };
    let shutdown = bytes.is_none();
    let referenced = {
        let mut data = state.data_mut();
        data.write_callback = Some(Heap::from(callback.get()));
        data.referenced.clone()
    };
    let canceller = spawn_cancellable_with_ref(
        scope,
        socket,
        |cancelled| async move {
            let operation = async {
                match bytes {
                    Some(bytes) => writer.write(bytes).await,
                    None => writer.shutdown().await,
                }
            };
            let result = until_cancelled(cancelled, operation).await?;
            Some((writer, result))
        },
        move |scope, socket, (writer, result)| {
            if let Some(state) = state_of(scope, socket)? {
                if let Link::Tcp(tcp) | Link::Pipe(tcp) = &mut state.data_mut().link {
                    tcp.writer = Some(writer);
                    tcp.cancel_write = None;
                }
            }
            let syscall = if shutdown { "shutdown" } else { "write" };
            result.map_err(|error| UvError::net(&error, syscall).throw(scope))?;
            Ok(scope.root_value(js::value::from_bool(!shutdown)))
        },
        write_done,
        referenced,
    )?;
    if let Link::Tcp(tcp) | Link::Pipe(tcp) = &mut state.data_mut().link {
        tcp.cancel_write = Some(canceller);
    }
    Ok(())
}

/// Write `bytes` to `socket`, once it is connected, and call `callback` once done.
fn write_bytes(
    scope: &Scope<'_>,
    socket: Object<'_>,
    bytes: Vec<u8>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = socket_class::state_of(scope, socket)?;
    state.data_mut().bytes_dispatched += bytes.len() as f64;
    match defer_until_connected(&state, Some(bytes), callback) {
        Ok(()) => {
            // As in Node.js's `_writeGeneric`, a `close` before the connect calls the callback.
            let on_close = Function::new_callback(scope, c"onClose", 0, closed_early, callback)?;
            crate::events::listen(scope, socket, c"once", c"close", *on_close)?;
            if let Some(deferred) = state.data_mut().deferred.as_mut() {
                deferred.on_close = Some(Heap::from(*on_close));
            }
            Ok(())
        }
        Err(bytes) => write_or_shutdown(scope, socket, &state, bytes, callback),
    }
}

/// Keep the write of `bytes`, or the shutdown for `None`, until the connect in progress
/// completes. Returns `bytes` as the error if the socket isn't connecting.
fn defer_until_connected(
    state: &SocketState<'_>,
    bytes: Option<Vec<u8>>,
    callback: HandleValue<'_>,
) -> Result<(), Option<Vec<u8>>> {
    let mut data = state.data_mut();
    if !matches!(data.link, Link::Connecting(_)) {
        return Err(bytes);
    }
    data.deferred = Some(DeferredWrite {
        bytes,
        callback: Heap::from(callback.get()),
        on_close: None,
    });
    Ok(())
}

/// The `close` listener of a socket with a write waiting for the connect, whose payload is the
/// write's callback: call it with `ERR_SOCKET_CLOSED_BEFORE_CONNECTION`.
fn closed_early(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    callback: HandleValue,
) -> Result<Value, ExnThrown> {
    closed_before_connection(scope, callback)?;
    Ok(js::value::undefined())
}

/// Call `callback` with `ERR_SOCKET_CLOSED_BEFORE_CONNECTION`.
fn closed_before_connection(scope: &Scope<'_>, callback: HandleValue<'_>) -> Result<(), ExnThrown> {
    let closed = NodeError::new(
        ErrorKind::Error,
        "ERR_SOCKET_CLOSED_BEFORE_CONNECTION",
        "Socket closed before the connection was established",
    )
    .to_value(scope)?;
    Function::call(scope, HandleValue::undefined(), callback, &[closed])?;
    Ok(())
}

/// The callback of the timer that emits `close` on a destroyed socket, whose `this` is the
/// socket and whose arguments are those of `close`.
fn emit_close(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let close: Vec<HandleValue<'_>> = args.rest(0);
    crate::events::emit(scope, socket, c"close", &close)?;
    Ok(js::value::undefined())
}

/// The `end` listener of a socket, whose `this` is the socket, as Node.js's
/// `onReadableStreamEnd`: unless the socket allows half-open connections, its `write` becomes
/// [`write_after_fin`].
fn on_readable_stream_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    if !js::conversion::to_boolean(socket.get_property(scope, c"allowHalfOpen")?) {
        let write =
            crate::support::shared_named_function(scope, write_after_fin, c"writeAfterFIN", 3)?;
        socket.set_property(scope, c"write", write)?;
    }
    Ok(js::value::undefined())
}

/// The `write` of a socket whose peer ended the connection, whose `this` is the socket, as
/// Node.js's `writeAfterFIN`: a write before the socket ended writes, and a later one destroys
/// the socket with an `EPIPE` error, which the callback gets in the tick queue.
fn write_after_fin(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    if !js::conversion::to_boolean(socket.get_property(scope, c"writableEnded")?) {
        let (chunk, encoding, callback) = (args.get(0), args.get(1), args.get(2));
        let wrote = crate::stream::writable_class::write(
            scope,
            socket,
            chunk,
            Some(encoding),
            Some(callback),
        )
        .map_err(|thrown| thrown.throw(scope))?;
        return Ok(js::value::from_bool(wrote));
    }
    let callback = if js::function::is_callable(args.get(1)) {
        args.get(1)
    } else {
        args.get(2)
    };
    let error = NodeError::new(
        ErrorKind::Error,
        "EPIPE",
        "This socket has been ended by the other party",
    )
    .to_value(scope)?;
    crate::support::next_tick_call(scope, callback, &[error])?;
    Function::call_by_name(scope, socket.handle(), c"destroy", &[error])?;
    Ok(js::value::from_bool(false))
}

/// The options of a connect, as Node.js's `lookupAndConnect` validates them before the lookup.
struct ConnectOptions {
    host: String,
    port: u16,
    local_ip: Option<IpAddr>,
    local_port: u16,
    auto_select: bool,
}

/// The options `connect`'s `options` give, validated in the order of Node.js's
/// `lookupAndConnect`: `host`, `localAddress`, `localPort`, `port`, `autoSelectFamily` and
/// `autoSelectFamilyAttemptTimeout`.
fn connect_options(scope: &Scope<'_>, options: Object<'_>) -> Result<ConnectOptions, Thrown> {
    let given = options.get_property(scope, c"host")?;
    let host = if js::conversion::to_boolean(given) {
        let host = crate::errors::validate_string(scope, given, "options.host")?;
        if host.contains('\0') {
            return Err(NodeError::invalid_arg_value(
                scope,
                "options.host",
                given,
                "must be a string without null bytes",
            )
            .into());
        }
        host
    } else {
        "localhost".to_string()
    };
    let local_address = options.get_property(scope, c"localAddress")?;
    let local_ip = if js::conversion::to_boolean(local_address) {
        let ip = crate::support::rust_string(scope, local_address)
            .and_then(|address| address.parse::<IpAddr>().ok());
        match ip {
            Some(ip) => Some(ip),
            None => return Err(invalid_ip(scope, local_address).into()),
        }
    } else {
        None
    };
    let local_port = options.get_property(scope, c"localPort")?;
    let local_port = if js::conversion::to_boolean(local_port) {
        crate::errors::validate_number(scope, local_port, "options.localPort", f64::NEG_INFINITY)?
            as u16
    } else {
        0
    };
    let port = options.get_property(scope, c"port")?;
    let port = if port.is_undefined() {
        0
    } else {
        if !port.is_number() && !port.is_string() {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options.port",
                "one of type number or string",
                port,
            )
            .into());
        }
        super::validate_port(scope, port, "Port")?
    };
    let auto_select = options.get_property(scope, c"autoSelectFamily")?;
    let auto_select = if auto_select.is_null_or_undefined() {
        super::auto_select().0
    } else {
        crate::errors::validate_boolean(scope, auto_select, "options.autoSelectFamily")?
    };
    let timeout = options.get_property(scope, c"autoSelectFamilyAttemptTimeout")?;
    if !timeout.is_null_or_undefined() {
        crate::errors::validate_integer(
            scope,
            timeout,
            "options.autoSelectFamilyAttemptTimeout",
            1.0,
            2147483647.0,
        )?;
    }
    Ok(ConnectOptions {
        host,
        port,
        local_ip,
        local_port,
        auto_select,
    })
}

/// The address family 4 or 6 that the `family` option `family` names as a number or as `IPv4` or
/// `IPv6`, or 0.
fn family_number(scope: &Scope<'_>, family: HandleValue<'_>) -> Result<u8, ExnThrown> {
    if family.is_number() {
        return Ok(match family.to_number() {
            4.0 => 4,
            6.0 => 6,
            _ => 0,
        });
    }
    Ok(if crate::support::is_str(scope, family, c"IPv4")? {
        4
    } else if crate::support::is_str(scope, family, c"IPv6")? {
        6
    } else {
        0
    })
}

/// The lookup options of a connect to a host that isn't an IP address with `dns.lookup`: the
/// options' `family`, and their `hints`, or 0 if they are falsy, as `dns.lookup` validates them.
fn default_lookup_options(
    scope: &Scope<'_>,
    options: Object<'_>,
) -> Result<crate::dns::LookupOptions, Thrown> {
    let dns_options = Object::new_plain(scope)?;
    let family = options.get_property(scope, c"family")?;
    dns_options.set_property(scope, c"family", family)?;
    let hints = options.get_property(scope, c"hints")?;
    if js::conversion::to_boolean(hints) {
        dns_options.set_property(scope, c"hints", hints)?;
    }
    let dns_options = dns_options.to_jsval_throwing(scope)?;
    crate::dns::lookup_options(scope, Some(dns_options), true)
}

/// The state of the `this` of an accessor, if it is a socket.
fn this_state<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
) -> Result<Option<(Object<'s>, SocketState<'s>)>, ExnThrown> {
    let Ok(socket) = Object::from_value(scope, args.this()) else {
        return Ok(None);
    };
    Ok(state_of(scope, socket)?.map(|state| (socket, state)))
}

/// The getter of `_handle`.
fn get_handle(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let handle = match this_state(scope, &args)? {
        Some((_, state)) => state.data().handle.get(scope),
        None => None,
    };
    Ok(handle.map_or(js::value::null(), |handle| handle.as_value()))
}

/// The getter of `_connecting`, Node.js's alias of `this.connecting`.
fn get_connecting(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    Ok(this.get_property(scope, c"connecting")?.get())
}

/// The setter of `_handle`.
fn set_handle(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some((_, state)) = this_state(scope, &args)? {
        let handle = Object::from_value(scope, args.get(0)).ok();
        state.data_mut().handle = handle.map(Heap::from);
    }
    Ok(js::value::undefined())
}

/// The getter of `bytesRead`.
fn get_bytes_read(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let bytes = match this_state(scope, &args)? {
        Some((_, state)) => state.data().bytes_read,
        None => 0.0,
    };
    Ok(js::value::from_f64(bytes))
}

/// The getter of `_bytesDispatched`: the bytes handed to the connection, or waiting for the
/// socket to connect.
fn get_bytes_dispatched(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let bytes = match this_state(scope, &args)? {
        Some((_, state)) => state.data().bytes_dispatched,
        None => 0.0,
    };
    Ok(js::value::from_f64(bytes))
}

/// The getter of `bytesWritten`: the bytes handed to the connection, or waiting for the socket to
/// connect, and the bytes the stream buffers, as Node.js counts them.
fn get_bytes_written(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some((socket, state)) = this_state(scope, &args)? else {
        return Ok(js::value::undefined());
    };
    let mut bytes = state.data().bytes_dispatched;
    if let Some(side) = WritableSide::try_of(scope, socket)? {
        let buffered: Vec<(HandleValue<'_>, HandleValue<'_>)> = side
            .get()
            .buffered
            .iter()
            .map(|write| (write.chunk.get(scope), write.encoding.get(scope)))
            .collect();
        for (chunk, encoding) in buffered {
            bytes += crate::buffer::buffer::byte_length(scope, chunk, Some(encoding))
                .map_err(|thrown| thrown.throw(scope))?;
        }
    }
    Ok(js::value::from_f64(bytes))
}

/// The getter of `pending`: whether the socket has no connection, or is connecting.
fn get_pending(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let pending = match this_state(scope, &args)? {
        Some((_, state)) => !matches!(
            state.data().link,
            Link::Tcp(_) | Link::Pipe(_) | Link::Stdin(_)
        ),
        None => true,
    };
    Ok(js::value::from_bool(pending))
}

/// The getter of `readyState`.
fn get_ready_state(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = match this_state(scope, &args)? {
        Some((_, state)) => {
            let data = state.data();
            if matches!(data.link, Link::Connecting(_)) {
                "opening"
            } else {
                let readable = data
                    .parent
                    .readable
                    .as_ref()
                    .is_some_and(|part| part.is_readable());
                let writable = data
                    .parent
                    .writable
                    .as_ref()
                    .is_some_and(|part| part.is_writable());
                match (readable, writable) {
                    (true, true) => "open",
                    (true, false) => "readOnly",
                    (false, true) => "writeOnly",
                    (false, false) => "closed",
                }
            }
        }
        None => "closed",
    };
    js::JSString::from_str(scope, state)?.to_jsval_raw_throwing(scope)
}

/// The names of the address getters, in the order of their payloads.
const ADDRESS_GETTERS: [(&std::ffi::CStr, bool); 6] = [
    (c"remoteAddress", true),
    (c"remoteFamily", true),
    (c"remotePort", true),
    (c"localAddress", false),
    (c"localFamily", false),
    (c"localPort", false),
];

/// The getter of the address property whose index in [`ADDRESS_GETTERS`] is the payload. Reading
/// a remote address while connected defines `_peername`, after which the remote address stays
/// readable once the socket closed, as in Node.js.
fn get_address(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some((socket, state)) = this_state(scope, &args)? else {
        return Ok(js::value::undefined());
    };
    let index = payload.to_number() as usize;
    let (_, remote) = ADDRESS_GETTERS[index];
    let (connected, addr, define_peername) = {
        let data = state.data();
        let connected = matches!(data.link, Link::Tcp(_));
        if remote {
            let known = connected || data.peername_defined;
            (
                connected,
                data.peer.filter(|_| known),
                connected && !data.peername_defined,
            )
        } else {
            (connected, data.local.filter(|_| connected), false)
        }
    };
    let Some(addr) = addr else {
        return Ok(js::value::undefined());
    };
    if define_peername && connected {
        state.data_mut().peername_defined = true;
        let peername = address_object(scope, addr)?;
        socket.define_property(
            scope,
            c"_peername",
            peername,
            js::class_spec::JSPROP_ENUMERATE as u32,
        )?;
    }
    Ok(match index % 3 {
        0 => js::JSString::from_str(scope, &addr.ip().to_string())?.to_jsval_raw_throwing(scope)?,
        1 => js::JSString::from_str(scope, family(&addr))?.to_jsval_raw_throwing(scope)?,
        _ => js::value::from_f64(f64::from(addr.port())),
    })
}

#[core_runtime::jsfunction_class(
    name = "Socket",
    extends = "crate::stream::duplex_class",
    state = "SocketState",
    invalid_this = "crate::errors::invalid_this",
    self_construct
)]
pub mod socket_class {
    use super::*;

    /// <https://nodejs.org/api/net.html#new-netsocketoptions>: the stream options `options`
    /// apply, except the object modes, which throw, and `decodeStrings`, which is `false`.
    /// `signal` destroys the socket when aborted, and `connect` skips the addresses that
    /// `blockList` blocks. `fd` and `handle` are not supported.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if let Some(options) = options.and_then(|options| Object::from_value(scope, *options).ok())
        {
            let mut unsupported = None;
            for name in [c"objectMode", c"readableObjectMode", c"writableObjectMode"] {
                let value = options.get_property(scope, name)?;
                if js::conversion::to_boolean(value) {
                    unsupported = Some((name, value));
                    break;
                }
            }
            if let Some((name, value)) = unsupported {
                let name = format!("options.{}", name.to_string_lossy());
                return Err(
                    NodeError::invalid_arg_value(scope, &name, value, "is not supported").into(),
                );
            }
        }
        this.set_property(scope, c"connecting", false)?;
        this.set_property(scope, c"_hadError", false)?;
        this.set_property(scope, c"_parent", HandleValue::null())?;
        this.set_property(scope, c"_host", HandleValue::null())?;
        this.set_property(scope, c"_closeAfterHandlingError", false)?;
        let base = Object::new_plain(scope)?;
        if let Some(options) = options.and_then(|options| Object::from_value(scope, *options).ok())
        {
            base.assign(scope, options)?;
        }
        let allow_half_open =
            js::conversion::to_boolean(base.get_property(scope, c"allowHalfOpen")?);
        base.set_property(scope, c"allowHalfOpen", allow_half_open)?;
        base.set_property(scope, c"emitClose", false)?;
        base.set_property(scope, c"autoDestroy", true)?;
        base.set_property(scope, c"decodeStrings", false)?;
        crate::stream::init_duplex(scope, this, Some(base.to_jsval_throwing(scope)?))?;
        let on_end = crate::support::shared_named_function(
            scope,
            on_readable_stream_end,
            c"onReadableStreamEnd",
            0,
        )?;
        crate::events::listen(scope, this, c"on", c"end", on_end)?;
        this.set_property(scope, c"_sockname", js::value::null())?;
        this.set_property(scope, c"_pendingData", js::value::null())?;
        this.set_property(scope, c"_pendingEncoding", "")?;
        this.set_property(scope, c"server", js::value::null())?;
        this.set_property(scope, c"_server", js::value::null())?;
        let signal = base.get_property(scope, c"signal")?;
        if js::conversion::to_boolean(signal) {
            crate::stream::add_abort_signal(scope, signal, this.to_jsval_throwing(scope)?)?;
        }
        let block_list = base.get_property(scope, c"blockList")?;
        if js::conversion::to_boolean(block_list) {
            super::super::blocklist::validate_block_list(scope, block_list, "options.blockList")?;
            this.set_property(scope, c"blockList", block_list)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#socketconnect>: connect to `port` on `host`, which
    /// defaults to `localhost`, from `(port[, host][, listener])`, `(options[, listener])`, or an
    /// array of `[options, listener]` that `net._normalizeArgs` returned.
    /// The options `family` and `hints` select the addresses of a host that isn't an IP address
    /// as `dns.lookup` does, `lookup` replaces the lookup of the host, and `localAddress` and
    /// `localPort` set the local address and port. IPC paths are not
    /// supported. A socket without a connection, including a destroyed one, is reset with
    /// `_undestroy()` before it connects.
    pub fn connect<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        if let Some((options, listener)) = match args.first() {
            Some(first) => super::super::normalized_args(scope, *first)?,
            None => None,
        } {
            return connect(
                scope,
                this,
                js::class::RestArgs::new(vec![options, listener]),
            );
        }
        let state = state_of(scope, this)?;
        let first = args.first().copied();
        let options = match first {
            Some(first) if first.is_object() && !js::function::is_callable(first) => {
                Object::from_value(scope, *first).map_err(|e| Thrown::from(e.throw(scope)))?
            }
            Some(first) if super::super::is_pipe_name(scope, first)? => {
                return Err(NodeError::not_implemented("connect() to an IPC path").into());
            }
            Some(first) => {
                let options = Object::new_plain(scope)?;
                options.set_property(scope, c"port", first)?;
                if let Some(host) = args.get(1).filter(|host| host.is_string()) {
                    options.set_property(scope, c"host", *host)?;
                }
                options
            }
            None => Object::new_plain(scope)?,
        };
        if let Some(listener) = args
            .last()
            .copied()
            .filter(|last| js::function::is_callable(*last))
        {
            Function::call_by_name(
                scope,
                this.handle(),
                c"once",
                &[js::class::get_or_init_atom(scope, c"connect")?, listener],
            )?;
        }
        let path = options.get_property(scope, c"path")?;
        if options.get_property(scope, c"port")?.is_undefined()
            && (path.is_null() || path.is_undefined())
        {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_MISSING_ARGS",
                "The \"options\" or \"port\" or \"path\" argument must be specified",
            )
            .into());
        }
        let (destroyed, unlinked) = {
            let data = state.data();
            (data.destroyed(), matches!(data.link, Link::None))
        };
        if destroyed && std::mem::take(&mut state.data_mut().peername_defined) {
            this.set_property(scope, c"_peername", HandleValue::null())?;
        }
        if unlinked {
            crate::stream::readable_class::undestroy(scope, this)?;
            this.set_property(scope, c"_sockname", HandleValue::null())?;
        }
        if js::conversion::to_boolean(path) {
            return Err(NodeError::not_implemented("connect() to an IPC path").into());
        }
        let ConnectOptions {
            host,
            port,
            local_ip,
            local_port,
            auto_select,
        } = super::connect_options(scope, options)?;
        this.set_property(scope, c"_host", host.as_str())?;
        let rules = crate::net::blocklist::rules_of(scope, this.get_property(scope, c"blockList")?);
        if host.parse::<IpAddr>().is_ok() {
            let target = ConnectTarget {
                port,
                family: 0,
                hints: 0,
                rules,
                local_ip,
                local_port,
            };
            super::connect(scope, this, &state, host, None, target)?;
            return Ok(this);
        }
        let lookup = options.get_property(scope, c"lookup")?;
        if !lookup.is_null_or_undefined() {
            validate_function(scope, lookup, "options.lookup")?;
        }
        let (family, hints) = if lookup.is_null_or_undefined() {
            let parsed = super::default_lookup_options(scope, options)?;
            (parsed.family, parsed.hints)
        } else {
            let family = options.get_property(scope, c"family")?;
            (super::family_number(scope, family)?, 0)
        };
        let target = ConnectTarget {
            port,
            family,
            hints,
            rules,
            local_ip,
            local_port,
        };
        if !lookup.is_null_or_undefined() {
            let all = !matches!(family, 4 | 6) && local_ip.is_none() && auto_select;
            let pending = PendingLookup {
                host: host.clone(),
                target,
                all,
            };
            super::connect_after_lookup(scope, this, &state, lookup, options, pending)?;
            return Ok(this);
        }
        super::connect(scope, this, &state, host, None, target)?;
        Ok(this)
    }

    #[method(name = "_read", length = 1)]
    pub fn read_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        if matches!(state.data().link, Link::Tcp(_) | Link::Pipe(_)) {
            return start_read(scope, this, &state);
        }
        state.data_mut().read_pending = true;
        Ok(())
    }

    /// Write `chunk`, a `Buffer`, or a string in `encoding`.
    #[method(name = "_write")]
    pub fn write_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let (bytes, _) = chunk_bytes(scope, chunk, Some(encoding))?.unwrap_or_default();
        Ok(write_bytes(scope, this, bytes, callback)?)
    }

    /// Write the buffered writes `chunks`, an array of `{ chunk, encoding }` entries, together.
    #[method(name = "_writev")]
    pub fn writev_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunks: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let mut bytes = Vec::new();
        if let Ok(chunks) = Object::from_value(scope, *chunks) {
            if let Ok(array) = chunks.cast::<js::Array>() {
                for entry in array.to_vec(scope)? {
                    let Ok(entry) = Object::from_value(scope, entry) else {
                        continue;
                    };
                    let chunk = entry.get_property(scope, c"chunk")?;
                    let encoding = entry.get_property(scope, c"encoding")?;
                    if let Some((chunk, _)) = chunk_bytes(scope, chunk, Some(encoding))? {
                        bytes.extend(chunk);
                    }
                }
            }
        }
        Ok(write_bytes(scope, this, bytes, callback)?)
    }

    #[method(name = "_final")]
    pub fn final_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        if defer_until_connected(&state, None, callback).is_ok() {
            return Ok(());
        }
        if !matches!(state.data().link, Link::Tcp(_) | Link::Pipe(_)) {
            Function::call(
                scope,
                HandleValue::undefined(),
                callback,
                js::function::EmptyArgs,
            )?;
            return Ok(());
        }
        write_or_shutdown(scope, this, &state, None, callback)
    }

    /// Cancel the connect, read and write in progress, drop the connection, call `callback`
    /// with `error`, and then emit `close` with whether `error` is given in a later event loop
    /// turn. A socket that never connected emits `close` without an argument in the tick queue.
    /// The server that accepted the socket counts one connection less. A shutdown waiting for
    /// the connect gets `ERR_SOCKET_CLOSED_BEFORE_CONNECTION`, and a write waiting for it gets
    /// the error from the `close` listener that the write added.
    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        clear_timer(&state);
        let (connected, deferred, server) = {
            let mut data = state.data_mut();
            let connected = !matches!(data.link, Link::None);
            (
                connected,
                data.take_deferred(scope),
                data.server.take_rooted(scope),
            )
        };
        release(scope, this, state)?;
        if let Some((_, deferred, None)) = deferred {
            closed_before_connection(scope, deferred)?;
        }
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        if connected {
            // The connection closes after a millisecond, as `setTimeout` without a delay runs a
            // callback, so `close` follows the `error` event's handlers, the reactions they
            // start, and the I/O already completed.
            let close = crate::support::shared_function(scope, emit_close, 0)?;
            let had_error =
                scope.root_value(js::value::from_bool(js::conversion::to_boolean(error)));
            let callable = js::Callable::from_jsval(scope, close.to_jsval_throwing(scope)?, ())
                .map_err(|e| e.throw(scope))?;
            core_runtime::event_loop::timer::start_timer(
                scope,
                callable,
                this.to_jsval_throwing(scope)?,
                &[had_error],
                Duration::from_millis(1),
                false,
                None,
            )
            .ok_or_else(|| {
                core_runtime::event_loop::throw_no_event_loop(scope, "socket.destroy")
            })?;
        } else {
            crate::support::emit_later(scope, this, c"close", &[])?;
        }
        if let Some(server) = server {
            super::super::server::connection_closed(scope, server)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#socketsettimeouttimeout-callback>: emit `timeout` once
    /// the socket is idle for `timeout` milliseconds, or never for 0.
    #[method(length = 2)]
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        timeout: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        if state.data().destroyed() {
            return Ok(this);
        }
        let msecs = crate::timers::get_timer_duration(scope, timeout, "msecs")?;
        let callback = callback.filter(|callback| !callback.is_undefined());
        if let Some(callback) = callback {
            validate_function(scope, callback, "callback")?;
        }
        let delay = if msecs > 0.0 { msecs.max(1.0) } else { 0.0 };
        let now = now_ms();
        // A pending timer that fires before the new timeout ends stays, and waits for the rest
        // of the timeout once it fires. A timeout of 0 lets it fire without effect.
        let rearm = {
            let mut data = state.data_mut();
            data.timeout = delay;
            data.active_at = now;
            delay > 0.0 && (data.timer.is_none() || data.timer_due > now + delay)
        };
        this.set_property(scope, c"timeout", msecs)?;
        if rearm {
            clear_timer(&state);
            arm_timer(scope, this, &state, delay)?;
        }
        if let Some(callback) = callback {
            let method = if msecs > 0.0 {
                c"once"
            } else {
                c"removeListener"
            };
            Function::call_by_name(
                scope,
                this.handle(),
                method,
                &[js::class::get_or_init_atom(scope, c"timeout")?, callback],
            )?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketaddress>: the local address, port and family, or
    /// an empty object without a connection.
    pub fn address<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<Object<'s>, ExnThrown> {
        let local = {
            let state = state_of(scope, this)?;
            let data = state.data();
            data.local.filter(|_| matches!(data.link, Link::Tcp(_)))
        };
        match local {
            Some(addr) => address_object(scope, addr),
            None => Object::new_plain(scope),
        }
    }

    /// <https://nodejs.org/api/net.html#socketsetnodelaynodelay>: has no effect.
    #[method(length = 1)]
    pub fn set_no_delay<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketsetkeepaliveenable-initialdelay>: has no effect.
    #[method(length = 4)]
    pub fn set_keep_alive<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketref>: the socket's connect, reads and writes keep
    /// the event loop alive again.
    #[method(name = "ref")]
    pub fn ref_impl<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        state_of(scope, this)?
            .data()
            .referenced
            .set_referenced(true);
        Ok(this)
    }

    /// <https://nodejs.org/api/net.html#socketunref>: the socket's connect, reads and writes
    /// don't keep the event loop alive. Its idle timer never does.
    pub fn unref<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        state_of(scope, this)?
            .data()
            .referenced
            .set_referenced(false);
        Ok(this)
    }

    /// Destroy the socket once it wrote what it buffers, ending it first, as Node.js's
    /// `destroySoon` does.
    pub fn destroy_soon(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        let writable = state
            .data()
            .parent
            .writable
            .as_ref()
            .is_some_and(|part| part.is_writable());
        if writable {
            Function::call_by_name(scope, this.handle(), c"end", js::function::EmptyArgs)?;
        }
        let finished = state
            .data()
            .parent
            .writable
            .as_ref()
            .is_some_and(|part| part.flags.contains(W::FINISHED));
        if finished {
            Function::call_by_name(scope, this.handle(), c"destroy", js::function::EmptyArgs)?;
        } else {
            let destroy = this.get_property(scope, c"destroy")?;
            Function::call_by_name(
                scope,
                this.handle(),
                c"once",
                &[js::class::get_or_init_atom(scope, c"finish")?, destroy],
            )?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#socketresetanddestroy>: a socket without a connection
    /// is destroyed with `ERR_SOCKET_CLOSED`, and a pipe or standard input throws
    /// `ERR_INVALID_HANDLE_TYPE`. A TCP connection, or a connect in progress, is destroyed
    /// without sending a reset.
    pub fn reset_and_destroy<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
    ) -> Result<Object<'s>, ExnThrown> {
        let state = state_of(scope, this)?;
        let (unconnected, other_handle) = {
            let data = state.data();
            (
                matches!(data.link, Link::None),
                matches!(data.link, Link::Pipe(_) | Link::Stdin(_)),
            )
        };
        if other_handle {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_HANDLE_TYPE",
                "This handle type cannot be sent",
            )
            .throw(scope));
        }
        if unconnected {
            let error = NodeError::new(ErrorKind::Error, "ERR_SOCKET_CLOSED", "Socket is closed")
                .to_value(scope)?;
            Function::call_by_name(scope, this.handle(), c"destroy", &[error])?;
        } else {
            Function::call_by_name(scope, this.handle(), c"destroy", js::function::EmptyArgs)?;
        }
        Ok(this)
    }

    /// Define `_handle`, `readyState`, `pending`, the byte counts, the address getters and
    /// `_connecting`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let enumerable = js::class_spec::JSPROP_ENUMERATE as u32;
        let getter = Function::new_callback(scope, c"_handle", 0, get_handle, ())?;
        let setter = Function::new_callback(scope, c"_handle", 1, set_handle, ())?;
        proto.define_accessor(scope, c"_handle", Some(*getter), Some(*setter), 0)?;
        for (name, body, flags) in [
            (c"readyState", get_ready_state as js::function::Callback, 0),
            (c"pending", get_pending, 0),
            (c"bytesRead", get_bytes_read, enumerable),
            (c"_bytesDispatched", get_bytes_dispatched, enumerable),
            (c"bytesWritten", get_bytes_written, enumerable),
        ] {
            let getter = Function::new_callback(scope, name, 0, body, ())?;
            proto.define_accessor(scope, name, Some(*getter), None, flags)?;
        }
        for (index, (name, _)) in ADDRESS_GETTERS.iter().enumerate() {
            let getter = Function::new_callback(scope, name, 0, get_address, index as f64)?;
            proto.define_accessor(scope, name, Some(*getter), None, enumerable)?;
        }
        let getter = Function::new_callback(scope, c"_connecting", 0, get_connecting, ())?;
        let permanent = js::class_spec::JSPROP_PERMANENT as u32;
        proto.define_accessor(scope, c"_connecting", Some(*getter), None, permanent)?;
        Ok(())
    }
}
