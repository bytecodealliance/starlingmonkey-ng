// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! TCP and UDP sockets and host name lookup over `wasi:sockets`.

use std::fmt;
use std::future::{poll_fn, Future};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use wasip3::sockets::ip_name_lookup::{self, resolve_addresses};
pub use wasip3::sockets::types::TcpSocket;
use wasip3::sockets::types::{
    ErrorCode, IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress,
};
use wit_bindgen::StreamReader;

use crate::net::Received;

/// Why a socket operation failed.
#[derive(Debug)]
pub enum NetError {
    /// A `wasi:sockets` operation failed.
    Socket(ErrorCode),
    /// Looking up a host name failed.
    Lookup(ip_name_lookup::ErrorCode),
    /// A host name resolved to no address.
    NoAddress(String),
    /// Connecting did not finish within the given time.
    TimedOut(Duration),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Socket(code) => write!(f, "{code}"),
            NetError::Lookup(code) => write!(f, "{code}"),
            NetError::NoAddress(host) => write!(f, "`{host}` resolves to no address"),
            NetError::TimedOut(timeout) => {
                write!(f, "connecting timed out after {} ms", timeout.as_millis())
            }
        }
    }
}

impl std::error::Error for NetError {}

impl From<ErrorCode> for NetError {
    fn from(code: ErrorCode) -> Self {
        NetError::Socket(code)
    }
}

/// The addresses `host` resolves to: `host` itself if it is an IP address literal, the loopback
/// addresses for `localhost`, which RFC 6761 reserves for them and `wasi:sockets` hosts may not
/// resolve, and the result of a `wasi:sockets` lookup otherwise.
pub async fn resolve(host: &str) -> Result<Vec<IpAddr>, NetError> {
    if let Ok(addr) = host.parse::<IpAddr>() {
        return Ok(vec![addr]);
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()]);
    }
    let addrs = resolve_addresses(host.to_string())
        .await
        .map_err(NetError::Lookup)?;
    if addrs.is_empty() {
        return Err(NetError::NoAddress(host.to_string()));
    }
    Ok(addrs.into_iter().map(from_wasi_ip).collect())
}

/// Fail: `wasi:sockets` provides no lookup of the name of an address.
pub async fn lookup_service(_addr: SocketAddr) -> Result<(String, String), NetError> {
    Err(unsupported())
}

/// No name servers: WASI provides no resolver configuration.
pub fn name_servers() -> Vec<SocketAddr> {
    Vec::new()
}

/// A TCP socket connected to `addr`. With a `timeout`, connecting fails with
/// [`NetError::TimedOut`] once it has not finished in that time.
pub async fn connect(addr: SocketAddr, timeout: Option<Duration>) -> Result<TcpSocket, NetError> {
    connect_from(addr, None, timeout).await
}

/// [`connect`] from the local address `local`, which the socket is bound to before it connects,
/// or from an address the host picks for `None`.
pub async fn connect_from(
    addr: SocketAddr,
    local: Option<SocketAddr>,
    timeout: Option<Duration>,
) -> Result<TcpSocket, NetError> {
    let socket = TcpSocket::create(family(addr.ip()))?;
    if let Some(local) = local {
        socket.bind(to_wasi(local))?;
    }
    let connected = {
        let mut connect = pin!(socket.connect(to_wasi(addr)));
        match timeout {
            None => Some(connect.await),
            Some(timeout) => {
                let mut sleep = pin!(crate::clock::sleep(timeout));
                poll_fn(|cx| {
                    if let Poll::Ready(result) = connect.as_mut().poll(cx) {
                        return Poll::Ready(Some(result));
                    }
                    sleep.as_mut().poll(cx).map(|()| None)
                })
                .await
            }
        }
    };
    match connected {
        Some(Ok(())) => Ok(socket),
        Some(Err(error)) => Err(error.into()),
        None => Err(NetError::TimedOut(timeout.unwrap_or_default())),
    }
}

/// A listening TCP socket.
pub struct TcpListener {
    /// The listening socket. Accepting stops when it is dropped.
    pub socket: TcpSocket,
    /// The accepted sockets, in arrival order.
    pub incoming: StreamReader<TcpSocket>,
    /// The address the socket is bound to.
    pub local_addr: SocketAddr,
}

/// A TCP socket bound to `addr` and listening.
pub fn listen(addr: SocketAddr) -> Result<TcpListener, NetError> {
    let socket = TcpSocket::create(family(addr.ip()))?;
    socket.bind(to_wasi(addr))?;
    let incoming = socket.listen()?;
    let local_addr = from_wasi(socket.get_local_address()?);
    Ok(TcpListener {
        socket,
        incoming,
        local_addr,
    })
}

/// The `wasi:sockets` address family of `addr`.
pub fn family(addr: IpAddr) -> IpAddressFamily {
    match addr {
        IpAddr::V4(_) => IpAddressFamily::Ipv4,
        IpAddr::V6(_) => IpAddressFamily::Ipv6,
    }
}

/// `addr` as a `wasi:sockets` address.
pub fn to_wasi(addr: SocketAddr) -> IpSocketAddress {
    match addr {
        SocketAddr::V4(addr) => {
            let [a, b, c, d] = addr.ip().octets();
            IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port: addr.port(),
                address: (a, b, c, d),
            })
        }
        SocketAddr::V6(addr) => {
            let [a, b, c, d, e, f, g, h] = addr.ip().segments();
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port: addr.port(),
                flow_info: addr.flowinfo(),
                address: (a, b, c, d, e, f, g, h),
                scope_id: addr.scope_id(),
            })
        }
    }
}

/// A `wasi:sockets` IP address as a `std` one.
pub fn from_wasi_ip(addr: IpAddress) -> IpAddr {
    match addr {
        IpAddress::Ipv4((a, b, c, d)) => Ipv4Addr::new(a, b, c, d).into(),
        IpAddress::Ipv6((a, b, c, d, e, f, g, h)) => Ipv6Addr::new(a, b, c, d, e, f, g, h).into(),
    }
}

/// A `wasi:sockets` socket address as a `std` one.
pub fn from_wasi(addr: IpSocketAddress) -> SocketAddr {
    match addr {
        IpSocketAddress::Ipv4(addr) => {
            let (a, b, c, d) = addr.address;
            SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), addr.port).into()
        }
        IpSocketAddress::Ipv6(addr) => {
            let (a, b, c, d, e, f, g, h) = addr.address;
            let ip = Ipv6Addr::new(a, b, c, d, e, f, g, h);
            SocketAddrV6::new(ip, addr.port, addr.flow_info, addr.scope_id).into()
        }
    }
}

impl NetError {
    /// The Node.js error code: the POSIX error name of a socket error, `ENOTFOUND` for a failed
    /// lookup, or `ETIMEDOUT`.
    pub fn code(&self) -> &'static str {
        match self {
            NetError::Socket(code) => match code {
                ErrorCode::AccessDenied => "EACCES",
                ErrorCode::NotSupported => "ENOTSUP",
                ErrorCode::InvalidArgument | ErrorCode::InvalidState => "EINVAL",
                ErrorCode::OutOfMemory => "ENOMEM",
                ErrorCode::Timeout => "ETIMEDOUT",
                ErrorCode::AddressNotBindable => "EADDRNOTAVAIL",
                ErrorCode::AddressInUse => "EADDRINUSE",
                ErrorCode::RemoteUnreachable => "EHOSTUNREACH",
                ErrorCode::ConnectionRefused => "ECONNREFUSED",
                ErrorCode::ConnectionBroken => "EPIPE",
                ErrorCode::ConnectionReset => "ECONNRESET",
                ErrorCode::ConnectionAborted => "ECONNABORTED",
                ErrorCode::DatagramTooLarge => "EMSGSIZE",
                _ => "EIO",
            },
            NetError::Lookup(ip_name_lookup::ErrorCode::TemporaryResolverFailure) => "EAI_AGAIN",
            NetError::Lookup(_) | NetError::NoAddress(_) => "ENOTFOUND",
            NetError::TimedOut(_) => "ETIMEDOUT",
        }
    }
}

/// The reading half of a [`Connection`]: the reads of the socket's receive stream, and the future
/// of its outcome.
pub struct ConnectionReader {
    incoming: crate::stream::ReadPump<u8>,
    outcome: Option<wit_bindgen::FutureReader<Result<(), ErrorCode>>>,
}

impl ConnectionReader {
    /// The next bytes received, or `None` once the peer ended its side. The bytes of a read that
    /// was cancelled by dropping its future are returned by the next read.
    pub async fn read(&mut self) -> Result<Option<Vec<u8>>, NetError> {
        loop {
            match self.receive().await? {
                Received::Bytes(bytes) => return Ok(Some(bytes)),
                Received::End => return Ok(None),
                Received::Stopped => {}
            }
        }
    }

    /// The next bytes received, or [`Received::Stopped`] once the receive stream ended, and then
    /// the outcome of the connection: [`Received::End`] or the error. The outcome is usually not
    /// known yet when the stream ends. The bytes of a call that was cancelled by dropping its
    /// future are returned by the next call.
    pub async fn receive(&mut self) -> Result<Received, NetError> {
        if !self.incoming.eof() {
            while let Some(bytes) = poll_fn(|cx| self.incoming.poll_read(cx)).await {
                if !bytes.is_empty() {
                    return Ok(Received::Bytes(bytes));
                }
            }
            if self.outcome.is_some() {
                return Ok(Received::Stopped);
            }
        }
        if let Some(outcome) = self.outcome.take() {
            outcome.await?;
        }
        Ok(Received::End)
    }
}

/// The writing half of a [`Connection`]: the stream the socket sends, and the outcome of sending
/// it, which a spawned task reports.
pub struct ConnectionWriter {
    stream: Option<wit_bindgen::StreamWriter<u8>>,
    sent: Option<futures_channel::oneshot::Receiver<Result<(), ErrorCode>>>,
}

impl ConnectionWriter {
    /// Send as much of `data` as the host takes without waiting, and return the bytes not sent.
    /// The write is polled once, and cancelled if the host doesn't complete it in that poll. A
    /// failed write sends nothing, and the next [`ConnectionWriter::write`] reports the error.
    pub fn try_write(&mut self, data: Vec<u8>) -> Vec<u8> {
        let Some(stream) = self.stream.as_mut() else {
            return data;
        };
        let mut write = pin!(stream.write(data));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let (_, unwritten) = match write.as_mut().poll(&mut cx) {
            Poll::Ready(outcome) => outcome,
            Poll::Pending => write.cancel(),
        };
        unwritten.into_vec()
    }

    /// Send all of `data`.
    pub async fn write(&mut self, data: Vec<u8>) -> Result<(), NetError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(NetError::Socket(ErrorCode::InvalidState));
        };
        let unwritten = stream.write_all(data).await;
        if unwritten.is_empty() {
            return Ok(());
        }
        self.stream = None;
        let outcome = match self.sent.take() {
            Some(sent) => sent.await.ok(),
            None => None,
        };
        match outcome {
            Some(Err(code)) => Err(NetError::Socket(code)),
            _ => Err(NetError::Socket(ErrorCode::ConnectionBroken)),
        }
    }

    /// End this side of the connection, without waiting for the host. Fails with the error of
    /// the send if the send already failed.
    pub async fn shutdown(&mut self) -> Result<(), NetError> {
        self.stream = None;
        if let Some(mut sent) = self.sent.take() {
            if let Ok(Some(result)) = sent.try_recv() {
                result?;
            }
        }
        Ok(())
    }
}

/// A TCP connection, split into its halves, with its addresses.
pub struct Connection {
    pub reader: ConnectionReader,
    pub writer: ConnectionWriter,
    pub local_addr: SocketAddr,
    pub peer_addr: SocketAddr,
}

impl Connection {
    fn new(socket: TcpSocket) -> Result<Self, NetError> {
        let local_addr = from_wasi(socket.get_local_address()?);
        let peer_addr = from_wasi(socket.get_remote_address()?);
        let (received, outcome) = socket.receive();
        let (tx, rx) = wasip3::wit_stream::new::<u8>();
        let (sent_tx, sent) = futures_channel::oneshot::channel();
        // The stream is passed to `send` here, so a write can complete before the task below
        // first runs. The task awaits the outcome, and owns the socket, which must outlive its
        // streams.
        let sending = socket.send(rx);
        wit_bindgen::spawn_local(async move {
            let _ = sent_tx.send(sending.await);
            drop(socket);
        });
        Ok(Connection {
            reader: ConnectionReader {
                incoming: crate::stream::ReadPump::new(received, 64 * 1024),
                outcome: Some(outcome),
            },
            writer: ConnectionWriter {
                stream: Some(tx),
                sent: Some(sent),
            },
            local_addr,
            peer_addr,
        })
    }
}

/// A TCP connection to `addr`. With a `timeout`, connecting fails with [`NetError::TimedOut`]
/// once it has not finished in that time.
pub async fn connect_stream(
    addr: SocketAddr,
    timeout: Option<Duration>,
) -> Result<Connection, NetError> {
    Connection::new(connect(addr, timeout).await?)
}

/// [`connect_stream`] from the local address `local`, which the socket is bound to before it
/// connects, or from an address the host picks for `None`.
pub async fn connect_stream_from(
    addr: SocketAddr,
    local: Option<SocketAddr>,
    timeout: Option<Duration>,
) -> Result<Connection, NetError> {
    Connection::new(connect_from(addr, local, timeout).await?)
}

/// A TCP socket listening for connections.
pub struct StreamListener {
    /// Reads the accepted sockets.
    incoming: crate::stream::ReadPump<TcpSocket>,
    /// The listening socket. Accepting stops when it is dropped.
    _socket: TcpSocket,
    /// The address the socket is bound to.
    pub local_addr: SocketAddr,
}

impl StreamListener {
    /// Poll for the next connection with `cx`. A connection whose addresses can't be read, such
    /// as one the peer reset before it was accepted, is skipped.
    pub fn poll_accept(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<Connection, NetError>> {
        loop {
            match std::task::ready!(self.incoming.poll_read(cx)) {
                Some(sockets) => {
                    if let Some(Ok(connection)) = sockets.into_iter().next().map(Connection::new) {
                        return Poll::Ready(Ok(connection));
                    }
                }
                None => return Poll::Ready(Err(NetError::Socket(ErrorCode::InvalidState))),
            }
        }
    }
}

/// A TCP socket bound to `addr` and listening.
pub fn listen_stream(addr: SocketAddr) -> Result<StreamListener, NetError> {
    let listener = listen(addr)?;
    Ok(StreamListener {
        incoming: crate::stream::ReadPump::new(listener.incoming, 1),
        _socket: listener.socket,
        local_addr: listener.local_addr,
    })
}

/// A `wasi:sockets` receive in progress.
type ReceiveOp = Pin<Box<dyn Future<Output = Result<(Vec<u8>, IpSocketAddress), ErrorCode>>>>;

/// A `wasi:sockets` send in progress.
type SendOp = Pin<Box<dyn Future<Output = Result<(), ErrorCode>>>>;

/// A bound UDP socket, with its receive and send in progress. Dropping it cancels both before the
/// socket closes.
pub struct UdpSocket {
    receiving: Option<ReceiveOp>,
    /// The send in progress, and the number of bytes it sends.
    sending: Option<(SendOp, usize)>,
    socket: Rc<wasip3::sockets::types::UdpSocket>,
}

/// The error of an operation `wasi:sockets` doesn't provide.
fn unsupported() -> NetError {
    NetError::Socket(ErrorCode::NotSupported)
}

impl UdpSocket {
    /// A UDP socket of the address family of `addr`, bound to `addr`. `wasi:sockets` has no
    /// options for address reuse or IPv6-only sockets: `reuse_addr` and `ipv6_only` have no
    /// effect, and `reuse_port` is not supported.
    pub fn bind(addr: SocketAddr, options: crate::net::UdpBindOptions) -> Result<Self, NetError> {
        if options.reuse_port {
            return Err(unsupported());
        }
        let socket = wasip3::sockets::types::UdpSocket::create(family(addr.ip()))?;
        socket.bind(to_wasi(addr))?;
        Ok(UdpSocket {
            receiving: None,
            sending: None,
            socket: Rc::new(socket),
        })
    }

    /// The address the socket is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(from_wasi(self.socket.get_local_address()?))
    }

    /// The address the socket is connected to.
    pub fn peer_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(from_wasi(self.socket.get_remote_address()?))
    }

    /// Send to and receive from `addr` only.
    pub fn connect(&mut self, addr: SocketAddr) -> Result<(), NetError> {
        Ok(self.socket.connect(to_wasi(addr))?)
    }

    /// Undo [`UdpSocket::connect`].
    pub fn disconnect(&mut self) -> Result<(), NetError> {
        Ok(self.socket.disconnect()?)
    }

    /// Return `None`: a `wasi:sockets` socket only sends through an asynchronous send,
    /// [`UdpSocket::poll_send`].
    pub fn try_send(
        &mut self,
        _data: &[u8],
        _addr: Option<SocketAddr>,
    ) -> Option<Result<usize, NetError>> {
        None
    }

    /// Poll sending `data` to `addr`, or to the connected address for `None`, with `cx`. Returns
    /// the number of bytes sent. The first poll of a send starts it with the bytes it takes out of
    /// `data`, and later polls of the same send ignore `data` and `addr`.
    pub fn poll_send(
        &mut self,
        cx: &mut std::task::Context<'_>,
        data: &mut Vec<u8>,
        addr: Option<SocketAddr>,
    ) -> Poll<Result<usize, NetError>> {
        let (op, length) = self.sending.get_or_insert_with(|| {
            let socket = Rc::clone(&self.socket);
            let data = std::mem::take(data);
            let length = data.len();
            let op: SendOp = Box::pin(async move { socket.send(data, addr.map(to_wasi)).await });
            (op, length)
        });
        let result = std::task::ready!(op.as_mut().poll(cx));
        let length = *length;
        self.sending = None;
        Poll::Ready(result.map(|()| length).map_err(NetError::from))
    }

    /// Poll for the next datagram with `cx`. Returns its bytes and the address it came from.
    pub fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(Vec<u8>, SocketAddr), NetError>> {
        let op = self.receiving.get_or_insert_with(|| {
            let socket = Rc::clone(&self.socket);
            Box::pin(async move { socket.receive().await })
        });
        let result = std::task::ready!(op.as_mut().poll(cx));
        self.receiving = None;
        Poll::Ready(
            result
                .map(|(data, from)| (data, from_wasi(from)))
                .map_err(NetError::from),
        )
    }

    /// Fail: `wasi:sockets` provides no broadcast option.
    pub fn set_broadcast(&self, _on: bool) -> Result<(), NetError> {
        Err(unsupported())
    }

    /// Set the hop limit of the unicast datagrams sent.
    pub fn set_ttl(&self, ttl: u8) -> Result<(), NetError> {
        Ok(self.socket.set_unicast_hop_limit(ttl)?)
    }

    /// Fail: `wasi:sockets` provides no multicast.
    pub fn set_multicast_ttl(&self, _ttl: u8) -> Result<(), NetError> {
        Err(unsupported())
    }

    /// Fail: `wasi:sockets` provides no multicast.
    pub fn set_multicast_loop(&self, _on: bool) -> Result<(), NetError> {
        Err(unsupported())
    }

    /// Fail: `wasi:sockets` provides no multicast.
    pub fn set_multicast_interface(
        &self,
        _interface: crate::net::MulticastInterface,
    ) -> Result<(), NetError> {
        Err(unsupported())
    }

    /// Fail: `wasi:sockets` provides no multicast.
    pub fn set_membership(
        &self,
        _group: IpAddr,
        _interface: crate::net::MulticastInterface,
        _join: bool,
    ) -> Result<(), NetError> {
        Err(unsupported())
    }

    /// Fail: `wasi:sockets` provides no multicast.
    pub fn set_source_membership(
        &self,
        _source: Ipv4Addr,
        _group: Ipv4Addr,
        _interface: Ipv4Addr,
        _join: bool,
    ) -> Result<(), NetError> {
        Err(unsupported())
    }

    /// The size of the receive buffer, or of the send buffer unless `receive`, after setting it to
    /// `size` if given.
    pub fn buffer_size(&self, receive: bool, size: Option<usize>) -> Result<usize, NetError> {
        match (receive, size) {
            (true, Some(size)) => self.socket.set_receive_buffer_size(size as u64)?,
            (false, Some(size)) => self.socket.set_send_buffer_size(size as u64)?,
            _ => {}
        }
        let size = match receive {
            true => self.socket.get_receive_buffer_size()?,
            false => self.socket.get_send_buffer_size()?,
        };
        Ok(usize::try_from(size).unwrap_or(usize::MAX))
    }
}

/// Return `None`: `wasi:sockets` doesn't name network interfaces.
pub fn interface_index(_name: &str) -> Option<u32> {
    None
}
