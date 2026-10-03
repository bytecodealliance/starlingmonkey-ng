// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! TCP sockets and host name lookup over `wasi:sockets`.

use std::fmt;
use std::future::{poll_fn, Future};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use wasip3::sockets::ip_name_lookup::{self, resolve_addresses};
pub use wasip3::sockets::types::TcpSocket;
use wasip3::sockets::types::{
    ErrorCode, IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress,
};
use wit_bindgen::StreamReader;

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

/// A TCP socket connected to `addr`. With a `timeout`, connecting fails with
/// [`NetError::TimedOut`] once it has not finished in that time.
pub async fn connect(addr: SocketAddr, timeout: Option<Duration>) -> Result<TcpSocket, NetError> {
    let socket = TcpSocket::create(family(addr.ip()))?;
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
                _ => "EIO",
            },
            NetError::Lookup(ip_name_lookup::ErrorCode::TemporaryResolverFailure) => "EAI_AGAIN",
            NetError::Lookup(_) | NetError::NoAddress(_) => "ENOTFOUND",
            NetError::TimedOut(_) => "ETIMEDOUT",
        }
    }
}

/// The reading half of a [`Connection`]: the socket's receive stream, and the future of its
/// outcome.
pub struct ConnectionReader {
    stream: Option<StreamReader<u8>>,
    outcome: Option<wit_bindgen::FutureReader<Result<(), ErrorCode>>>,
}

impl ConnectionReader {
    /// The next bytes received, or `None` once the peer ended its side.
    pub async fn read(&mut self) -> Result<Option<Vec<u8>>, NetError> {
        while let Some(mut stream) = self.stream.take() {
            let (result, bytes) = stream.read(Vec::with_capacity(64 * 1024)).await;
            if result != wit_bindgen::StreamResult::Dropped {
                self.stream = Some(stream);
            }
            if !bytes.is_empty() {
                return Ok(Some(bytes));
            }
        }
        if let Some(outcome) = self.outcome.take() {
            outcome.await?;
        }
        Ok(None)
    }
}

/// The writing half of a [`Connection`]: the stream the socket sends, and the outcome of sending
/// it, which a spawned task reports.
pub struct ConnectionWriter {
    stream: Option<wit_bindgen::StreamWriter<u8>>,
    sent: Option<futures_channel::oneshot::Receiver<Result<(), ErrorCode>>>,
}

impl ConnectionWriter {
    /// Send nothing, and return 0: a `wasi:sockets` stream only takes bytes through an
    /// asynchronous write, [`ConnectionWriter::write`].
    pub fn try_write(&mut self, _data: &[u8]) -> usize {
        0
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

    /// End this side of the connection.
    pub async fn shutdown(&mut self) -> Result<(), NetError> {
        self.stream = None;
        if let Some(sent) = self.sent.take() {
            if let Ok(result) = sent.await {
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
        // The host only sends the stream while the `send` call is polled, so a task of its own
        // polls it. The task owns the socket, which must outlive its streams.
        wit_bindgen::spawn_local(async move {
            let _ = sent_tx.send(socket.send(rx).await);
        });
        Ok(Connection {
            reader: ConnectionReader {
                stream: Some(received),
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
    /// Poll for the next connection with `cx`.
    pub fn poll_accept(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<Connection, NetError>> {
        loop {
            match std::task::ready!(self.incoming.poll_read(cx)) {
                Some(sockets) => {
                    if let Some(socket) = sockets.into_iter().next() {
                        return Poll::Ready(Connection::new(socket));
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
