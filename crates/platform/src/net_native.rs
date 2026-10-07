// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! TCP connections, UDP sockets and host name lookup over tokio. Every future must run inside a
//! tokio runtime.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::net::Received;

/// Why a socket operation failed.
#[derive(Debug)]
pub enum NetError {
    /// A socket operation failed.
    Io(std::io::Error),
    /// Looking up a host name failed, or it resolved to no address.
    NoAddress(String),
    /// Looking up a host name failed temporarily, with `EAI_AGAIN`.
    TemporaryFailure(String),
    /// Connecting did not finish within the given time.
    TimedOut(Duration),
}

impl NetError {
    /// The Node.js error code: the POSIX error name, `ENOTFOUND` or `EAI_AGAIN` for a failed
    /// lookup, or `ETIMEDOUT`.
    pub fn code(&self) -> &'static str {
        match self {
            NetError::Io(error) => crate::fs::FsError::from_io(error).code,
            NetError::NoAddress(_) => "ENOTFOUND",
            NetError::TemporaryFailure(_) => "EAI_AGAIN",
            NetError::TimedOut(_) => "ETIMEDOUT",
        }
    }
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Io(error) => write!(f, "{error}"),
            NetError::NoAddress(host) => write!(f, "`{host}` resolves to no address"),
            NetError::TemporaryFailure(host) => write!(f, "looking up `{host}` failed temporarily"),
            NetError::TimedOut(timeout) => {
                write!(f, "connecting timed out after {} ms", timeout.as_millis())
            }
        }
    }
}

impl std::error::Error for NetError {}

impl From<std::io::Error> for NetError {
    fn from(error: std::io::Error) -> Self {
        NetError::Io(error)
    }
}

/// The addresses `host` resolves to: `host` itself if it is an IP address literal, the result of
/// the host's lookup otherwise. A failed lookup is [`NetError::TemporaryFailure`] for `EAI_AGAIN`
/// and [`NetError::NoAddress`] otherwise.
pub async fn resolve(host: &str) -> Result<Vec<IpAddr>, NetError> {
    if let Ok(addr) = host.parse::<IpAddr>() {
        return Ok(vec![addr]);
    }
    let name = host.to_string();
    let addrs = tokio::task::spawn_blocking(move || addr_info(&name))
        .await
        .map_err(|error| NetError::Io(std::io::Error::other(error)))??;
    if addrs.is_empty() {
        return Err(NetError::NoAddress(host.to_string()));
    }
    Ok(addrs)
}

/// The body of [`resolve`] for a host name, which blocks.
#[cfg(unix)]
fn addr_info(host: &str) -> Result<Vec<IpAddr>, NetError> {
    let Ok(name) = std::ffi::CString::new(host) else {
        return Err(NetError::NoAddress(host.to_string()));
    };
    // SAFETY: an all-zero `addrinfo` is a valid value, with no flags and no pointers.
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut list: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: `name` and `hints` outlive the call, and `list` receives a list freed below.
    let status = unsafe { libc::getaddrinfo(name.as_ptr(), std::ptr::null(), &hints, &mut list) };
    match status {
        0 => {}
        libc::EAI_AGAIN => return Err(NetError::TemporaryFailure(host.to_string())),
        _ => return Err(NetError::NoAddress(host.to_string())),
    }
    let mut addrs = Vec::new();
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: `entry` is an entry of the list `getaddrinfo` returned, which is still alive,
        // and its `ai_addr` points to an address of its `ai_family`.
        let info = unsafe { &*entry };
        match info.ai_family {
            libc::AF_INET => {
                let addr = unsafe { &*info.ai_addr.cast::<libc::sockaddr_in>() };
                addrs.push(IpAddr::from(
                    u32::from_be(addr.sin_addr.s_addr).to_be_bytes(),
                ));
            }
            libc::AF_INET6 => {
                let addr = unsafe { &*info.ai_addr.cast::<libc::sockaddr_in6>() };
                addrs.push(IpAddr::from(addr.sin6_addr.s6_addr));
            }
            _ => {}
        }
        entry = info.ai_next;
    }
    // SAFETY: `list` is the list `getaddrinfo` returned, and no reference into it remains.
    unsafe { libc::freeaddrinfo(list) };
    Ok(addrs)
}

/// The body of [`resolve`] for a host name, which blocks.
#[cfg(not(unix))]
fn addr_info(host: &str) -> Result<Vec<IpAddr>, NetError> {
    let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(host, 0))
        .map_err(|_| NetError::NoAddress(host.to_string()))?;
    Ok(addrs.map(|addr| addr.ip()).collect())
}

/// The host name and the service name of `addr`, as `getnameinfo` with `NI_NAMEREQD` finds them.
/// A failed lookup is [`NetError::NoAddress`] with the address. Only supported on Unix.
pub async fn lookup_service(addr: SocketAddr) -> Result<(String, String), NetError> {
    tokio::task::spawn_blocking(move || name_info(addr))
        .await
        .map_err(|error| NetError::Io(std::io::Error::other(error)))?
}

/// The body of [`lookup_service`], which blocks.
#[cfg(unix)]
fn name_info(addr: SocketAddr) -> Result<(String, String), NetError> {
    let sockaddr = socket2::SockAddr::from(addr);
    let mut host = [0 as libc::c_char; 1025];
    let mut service = [0 as libc::c_char; 32];
    // SAFETY: `sockaddr` is a valid address of its length, and the buffers outlive the call and
    // have the lengths passed.
    let status = unsafe {
        libc::getnameinfo(
            sockaddr.as_ptr().cast(),
            sockaddr.len(),
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            service.as_mut_ptr(),
            service.len() as libc::socklen_t,
            libc::NI_NAMEREQD,
        )
    };
    if status != 0 {
        return Err(NetError::NoAddress(addr.ip().to_string()));
    }
    // SAFETY: `getnameinfo` succeeded, so both buffers hold NUL-terminated strings.
    let text = |buffer: &[libc::c_char]| unsafe {
        std::ffi::CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    Ok((text(&host), text(&service)))
}

/// The body of [`lookup_service`], which isn't supported here.
#[cfg(not(unix))]
fn name_info(_addr: SocketAddr) -> Result<(String, String), NetError> {
    Err(NetError::Io(std::io::ErrorKind::Unsupported.into()))
}

/// The name servers of `/etc/resolv.conf`, which is read on every call. Empty if there is no such
/// file or it names no server.
pub fn name_servers() -> Vec<SocketAddr> {
    std::fs::read_to_string("/etc/resolv.conf")
        .map(|text| crate::net::parse_resolv_conf(&text))
        .unwrap_or_default()
}

/// The size of each read of a [`Connection`].
const READ_SIZE: usize = 64 * 1024;

/// The reading half of a [`Connection`].
pub struct ConnectionReader(OwnedReadHalf);

impl ConnectionReader {
    /// The next bytes received, or `None` once the peer ended its side. The bytes are returned in
    /// a buffer of capacity [`READ_SIZE`], which is allocated once the socket is readable.
    pub async fn read(&mut self) -> Result<Option<Vec<u8>>, NetError> {
        loop {
            self.0.readable().await?;
            let mut buffer = Vec::with_capacity(READ_SIZE);
            match self.0.try_read_buf(&mut buffer) {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(buffer)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// The next bytes received, or [`Received::End`] once the peer ended its side.
    pub async fn receive(&mut self) -> Result<Received, NetError> {
        Ok(match self.read().await? {
            Some(bytes) => Received::Bytes(bytes),
            None => Received::End,
        })
    }
}

/// The writing half of a [`Connection`].
pub struct ConnectionWriter(OwnedWriteHalf);

impl ConnectionWriter {
    /// Send all of `data`.
    pub async fn write(&mut self, data: Vec<u8>) -> Result<(), NetError> {
        Ok(self.0.write_all(&data).await?)
    }

    /// Send as much of `data` as the socket takes without waiting, and return the bytes not sent.
    /// An error sends nothing, and the next [`ConnectionWriter::write`] reports it.
    ///
    /// The bytes go to the socket directly. Tokio's `try_write` refuses them until its reactor has
    /// polled the socket, which it hasn't yet right after a connect or an accept.
    pub fn try_write(&mut self, mut data: Vec<u8>) -> Vec<u8> {
        let stream: &tokio::net::TcpStream = self.0.as_ref();
        let written = socket2::SockRef::from(stream).send(&data).unwrap_or(0);
        data.drain(..written);
        data
    }

    /// End this side of the connection.
    pub async fn shutdown(&mut self) -> Result<(), NetError> {
        Ok(self.0.shutdown().await?)
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
    fn new(stream: tokio::net::TcpStream) -> Result<Self, NetError> {
        let local_addr = stream.local_addr()?;
        let peer_addr = stream.peer_addr()?;
        let (reader, writer) = stream.into_split();
        Ok(Connection {
            reader: ConnectionReader(reader),
            writer: ConnectionWriter(writer),
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
    let connect = tokio::net::TcpStream::connect(addr);
    let stream = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, connect)
            .await
            .map_err(|_| NetError::TimedOut(timeout))??,
        None => connect.await?,
    };
    Connection::new(stream)
}

/// [`connect_stream`] from the local address `local`, which the socket is bound to before it
/// connects, or from an address the host picks for `None`.
pub async fn connect_stream_from(
    addr: SocketAddr,
    local: Option<SocketAddr>,
    timeout: Option<Duration>,
) -> Result<Connection, NetError> {
    let Some(local) = local else {
        return connect_stream(addr, timeout).await;
    };
    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    socket.bind(local)?;
    let connect = socket.connect(addr);
    let stream = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, connect)
            .await
            .map_err(|_| NetError::TimedOut(timeout))??,
        None => connect.await?,
    };
    Connection::new(stream)
}

/// A TCP socket listening for connections.
pub struct StreamListener {
    /// The socket as bound, until the first `accept` converts it into a tokio listener.
    bound: Option<std::net::TcpListener>,
    listener: Option<tokio::net::TcpListener>,
    /// The address the socket is bound to.
    pub local_addr: SocketAddr,
}

impl StreamListener {
    /// Poll for the next connection with `cx`. A connection whose addresses can't be read, such
    /// as one the peer reset before it was accepted, is skipped. Once converting the socket into
    /// a tokio listener failed, the first poll fails with that error and later polls stay
    /// pending.
    pub fn poll_accept(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Connection, NetError>> {
        if let Some(bound) = self.bound.take() {
            match tokio::net::TcpListener::from_std(bound) {
                Ok(listener) => self.listener = Some(listener),
                Err(error) => return std::task::Poll::Ready(Err(error.into())),
            }
        }
        let Some(listener) = self.listener.as_ref() else {
            return std::task::Poll::Pending;
        };
        loop {
            let (stream, _) = std::task::ready!(listener.poll_accept(cx))?;
            if let Ok(connection) = Connection::new(stream) {
                return std::task::Poll::Ready(Ok(connection));
            }
        }
    }
}

/// A TCP socket bound to `addr` and listening with a backlog of 511, Node.js's default.
pub fn listen_stream(addr: SocketAddr) -> Result<StreamListener, NetError> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        None,
    )?;
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(511)?;
    let bound = std::net::TcpListener::from(socket);
    bound.set_nonblocking(true)?;
    let local_addr = bound.local_addr()?;
    Ok(StreamListener {
        bound: Some(bound),
        listener: None,
        local_addr,
    })
}

/// The OS socket of a [`UdpSocket`]: a `std` socket until the first poll registers it with tokio.
enum UdpHandle {
    Std(std::net::UdpSocket),
    Tokio(tokio::net::UdpSocket),
    /// Registering the socket with tokio failed, which closed it.
    Closed,
}

/// A bound UDP socket.
pub struct UdpSocket {
    handle: UdpHandle,
    /// Whether the socket is an IPv6 socket.
    ipv6: bool,
    /// The buffer each receive fills, which holds the largest datagram.
    buffer: Box<[u8]>,
}

/// The error of an operation the host doesn't support.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn unsupported() -> NetError {
    #[cfg(unix)]
    return NetError::Io(std::io::Error::from_raw_os_error(libc::ENOTSUP));
    #[cfg(not(unix))]
    return NetError::Io(std::io::ErrorKind::Unsupported.into());
}

/// The error of an operation on a socket that registering with tokio closed.
fn closed() -> NetError {
    #[cfg(unix)]
    return NetError::Io(std::io::Error::from_raw_os_error(libc::EBADF));
    #[cfg(not(unix))]
    return NetError::Io(std::io::ErrorKind::NotConnected.into());
}

impl UdpSocket {
    /// A UDP socket of the address family of `addr`, bound to `addr` with `options`. On macOS and
    /// the BSDs, `reuse_addr` sets `SO_REUSEPORT`, which lets sockets with the option bind the
    /// same address and port there, and elsewhere `SO_REUSEADDR`. `reuse_port` is only supported
    /// on Linux and Android.
    pub fn bind(addr: SocketAddr, options: crate::net::UdpBindOptions) -> Result<Self, NetError> {
        use socket2::{Domain, Protocol, Socket, Type};
        let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        if options.reuse_addr {
            #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
            socket.set_reuse_port(true)?;
            #[cfg(not(all(unix, not(any(target_os = "linux", target_os = "android")))))]
            socket.set_reuse_address(true)?;
        }
        if options.reuse_port {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            socket.set_reuse_port(true)?;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            return Err(unsupported());
        }
        if options.ipv6_only && addr.is_ipv6() {
            socket.set_only_v6(true)?;
        }
        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;
        Ok(UdpSocket {
            handle: UdpHandle::Std(socket.into()),
            ipv6: addr.is_ipv6(),
            buffer: vec![0; 64 * 1024].into_boxed_slice(),
        })
    }

    /// The socket, for reading and setting its options through `socket2`.
    fn options(&self) -> Result<socket2::SockRef<'_>, NetError> {
        match &self.handle {
            UdpHandle::Std(socket) => Ok(socket.into()),
            UdpHandle::Tokio(socket) => Ok(socket.into()),
            UdpHandle::Closed => Err(closed()),
        }
    }

    /// Register the socket with tokio unless it is registered. This must run inside a tokio
    /// runtime.
    fn register(&mut self) -> Result<(), NetError> {
        if let UdpHandle::Std(_) = self.handle {
            let UdpHandle::Std(socket) = std::mem::replace(&mut self.handle, UdpHandle::Closed)
            else {
                unreachable!("the handle is a std socket");
            };
            self.handle = UdpHandle::Tokio(tokio::net::UdpSocket::from_std(socket)?);
        }
        Ok(())
    }

    /// The address the socket is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        let addr = self.options()?.local_addr()?;
        addr.as_socket().ok_or_else(closed)
    }

    /// The address the socket is connected to.
    pub fn peer_addr(&self) -> Result<SocketAddr, NetError> {
        let addr = self.options()?.peer_addr()?;
        addr.as_socket().ok_or_else(closed)
    }

    /// Send to and receive from `addr` only.
    pub fn connect(&mut self, addr: SocketAddr) -> Result<(), NetError> {
        Ok(self.options()?.connect(&addr.into())?)
    }

    /// Undo [`UdpSocket::connect`] by connecting to an address of the family `AF_UNSPEC`. The
    /// `EAFNOSUPPORT` and `EINVAL` errors of that connect are ignored.
    pub fn disconnect(&mut self) -> Result<(), NetError> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = self.options()?.as_raw_fd();
            // SAFETY: an all-zero `sockaddr` is a valid value.
            let mut addr: libc::sockaddr = unsafe { std::mem::zeroed() };
            addr.sa_family = libc::AF_UNSPEC as libc::sa_family_t;
            let size = std::mem::size_of::<libc::sockaddr>() as libc::socklen_t;
            // SAFETY: `fd` is the socket's open descriptor, and `addr` outlives the call.
            if unsafe { libc::connect(fd, &addr, size) } != 0 {
                let error = std::io::Error::last_os_error();
                if !matches!(
                    error.raw_os_error(),
                    Some(libc::EAFNOSUPPORT | libc::EINVAL)
                ) {
                    return Err(error.into());
                }
            }
            Ok(())
        }
        #[cfg(not(unix))]
        Err(unsupported())
    }

    /// Send `data` to `addr`, or to the connected address for `None`, if the socket takes it
    /// without waiting. Returns the number of bytes sent, or `None` if sending would wait.
    pub fn try_send(
        &mut self,
        data: &[u8],
        addr: Option<SocketAddr>,
    ) -> Option<Result<usize, NetError>> {
        let result = match (&self.handle, addr) {
            (UdpHandle::Std(socket), Some(addr)) => socket.send_to(data, addr),
            (UdpHandle::Std(socket), None) => socket.send(data),
            (UdpHandle::Tokio(socket), Some(addr)) => socket.try_send_to(data, addr),
            (UdpHandle::Tokio(socket), None) => socket.try_send(data),
            (UdpHandle::Closed, _) => return Some(Err(closed())),
        };
        match result {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
            result => Some(result.map_err(NetError::from)),
        }
    }

    /// Poll sending `data` to `addr`, or to the connected address for `None`, with `cx`. Returns
    /// the number of bytes sent. Every poll of a send passes the same `data` and `addr`, and
    /// `data` is left unchanged.
    ///
    /// `data` is a `Vec` because the WASIp3 backend's `poll_send` takes the bytes out of it.
    #[allow(clippy::ptr_arg)]
    pub fn poll_send(
        &mut self,
        cx: &mut std::task::Context<'_>,
        data: &mut Vec<u8>,
        addr: Option<SocketAddr>,
    ) -> std::task::Poll<Result<usize, NetError>> {
        if let Err(error) = self.register() {
            return std::task::Poll::Ready(Err(error));
        }
        let UdpHandle::Tokio(socket) = &self.handle else {
            return std::task::Poll::Ready(Err(closed()));
        };
        let sent = match addr {
            Some(addr) => std::task::ready!(socket.poll_send_to(cx, data, addr)),
            None => std::task::ready!(socket.poll_send(cx, data)),
        };
        std::task::Poll::Ready(sent.map_err(NetError::from))
    }

    /// Poll for the next datagram with `cx`. Returns its bytes and the address it came from.
    pub fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(Vec<u8>, SocketAddr), NetError>> {
        if let Err(error) = self.register() {
            return std::task::Poll::Ready(Err(error));
        }
        let UdpHandle::Tokio(socket) = &self.handle else {
            return std::task::Poll::Ready(Err(closed()));
        };
        let mut buffer = tokio::io::ReadBuf::new(&mut self.buffer);
        let from = std::task::ready!(socket.poll_recv_from(cx, &mut buffer))?;
        std::task::Poll::Ready(Ok((buffer.filled().to_vec(), from)))
    }

    /// Set whether the socket can send to broadcast addresses.
    pub fn set_broadcast(&self, on: bool) -> Result<(), NetError> {
        Ok(self.options()?.set_broadcast(on)?)
    }

    /// Set the time to live, or the hop limit for IPv6, of the unicast datagrams sent.
    pub fn set_ttl(&self, ttl: u8) -> Result<(), NetError> {
        let options = self.options()?;
        match self.ipv6 {
            true => options.set_unicast_hops_v6(u32::from(ttl))?,
            false => options.set_ttl_v4(u32::from(ttl))?,
        }
        Ok(())
    }

    /// Set the time to live, or the hop limit for IPv6, of the multicast datagrams sent.
    pub fn set_multicast_ttl(&self, ttl: u8) -> Result<(), NetError> {
        let options = self.options()?;
        match self.ipv6 {
            true => options.set_multicast_hops_v6(u32::from(ttl))?,
            false => options.set_multicast_ttl_v4(u32::from(ttl))?,
        }
        Ok(())
    }

    /// Set whether the multicast datagrams sent are delivered to the sending host.
    pub fn set_multicast_loop(&self, on: bool) -> Result<(), NetError> {
        let options = self.options()?;
        match self.ipv6 {
            true => options.set_multicast_loop_v6(on)?,
            false => options.set_multicast_loop_v4(on)?,
        }
        Ok(())
    }

    /// Set the interface multicast datagrams are sent from.
    pub fn set_multicast_interface(
        &self,
        interface: crate::net::MulticastInterface,
    ) -> Result<(), NetError> {
        let options = self.options()?;
        match interface {
            crate::net::MulticastInterface::V4(addr) => options.set_multicast_if_v4(&addr)?,
            crate::net::MulticastInterface::V6(index) => options.set_multicast_if_v6(index)?,
        }
        Ok(())
    }

    /// Join the multicast group `group` on `interface`, or leave it unless `join`. A group and an
    /// interface of different address families are an invalid argument.
    pub fn set_membership(
        &self,
        group: IpAddr,
        interface: crate::net::MulticastInterface,
        join: bool,
    ) -> Result<(), NetError> {
        use crate::net::MulticastInterface::{V4, V6};
        let options = self.options()?;
        match (group, interface, join) {
            (IpAddr::V4(group), V4(addr), true) => options.join_multicast_v4(&group, &addr)?,
            (IpAddr::V4(group), V4(addr), false) => options.leave_multicast_v4(&group, &addr)?,
            (IpAddr::V6(group), V6(index), true) => options.join_multicast_v6(&group, index)?,
            (IpAddr::V6(group), V6(index), false) => options.leave_multicast_v6(&group, index)?,
            _ => return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput).into()),
        }
        Ok(())
    }

    /// Join the IPv4 multicast group `group` for the datagrams from `source` on the interface with
    /// the address `interface`, or leave it unless `join`.
    pub fn set_source_membership(
        &self,
        source: Ipv4Addr,
        group: Ipv4Addr,
        interface: Ipv4Addr,
        join: bool,
    ) -> Result<(), NetError> {
        let options = self.options()?;
        match join {
            true => options.join_ssm_v4(&source, &group, &interface)?,
            false => options.leave_ssm_v4(&source, &group, &interface)?,
        }
        Ok(())
    }

    /// The size of the receive buffer, or of the send buffer unless `receive`, after setting it to
    /// `size` if given.
    pub fn buffer_size(&self, receive: bool, size: Option<usize>) -> Result<usize, NetError> {
        let options = self.options()?;
        match (receive, size) {
            (true, Some(size)) => options.set_recv_buffer_size(size)?,
            (false, Some(size)) => options.set_send_buffer_size(size)?,
            _ => {}
        }
        Ok(match receive {
            true => options.recv_buffer_size()?,
            false => options.send_buffer_size()?,
        })
    }
}

/// The index of the network interface named `name`, if there is one.
pub fn interface_index(name: &str) -> Option<u32> {
    #[cfg(unix)]
    {
        let name = std::ffi::CString::new(name).ok()?;
        // SAFETY: `name` is a NUL-terminated string that outlives the call.
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        (index != 0).then_some(index)
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        None
    }
}
