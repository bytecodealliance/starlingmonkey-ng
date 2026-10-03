// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! TCP connections and host name lookup over tokio. Every future must run inside a tokio
//! runtime.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Why a socket operation failed.
#[derive(Debug)]
pub enum NetError {
    /// A socket operation failed.
    Io(std::io::Error),
    /// Looking up a host name failed, or it resolved to no address.
    NoAddress(String),
    /// Connecting did not finish within the given time.
    TimedOut(Duration),
}

impl NetError {
    /// The Node.js error code: the POSIX error name, `ENOTFOUND` for a failed lookup, or
    /// `ETIMEDOUT`.
    pub fn code(&self) -> &'static str {
        match self {
            NetError::Io(error) => crate::fs::FsError::from_io(error).code,
            NetError::NoAddress(_) => "ENOTFOUND",
            NetError::TimedOut(_) => "ETIMEDOUT",
        }
    }
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Io(error) => write!(f, "{error}"),
            NetError::NoAddress(host) => write!(f, "`{host}` resolves to no address"),
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
/// the host's lookup otherwise.
pub async fn resolve(host: &str) -> Result<Vec<IpAddr>, NetError> {
    if let Ok(addr) = host.parse::<IpAddr>() {
        return Ok(vec![addr]);
    }
    let addrs: Vec<IpAddr> = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|_| NetError::NoAddress(host.to_string()))?
        .map(|addr| addr.ip())
        .collect();
    if addrs.is_empty() {
        return Err(NetError::NoAddress(host.to_string()));
    }
    Ok(addrs)
}

/// The reading half of a [`Connection`], with the buffer each read fills.
pub struct ConnectionReader(OwnedReadHalf, Box<[u8]>);

impl ConnectionReader {
    /// The next bytes received, or `None` once the peer ended its side.
    pub async fn read(&mut self) -> Result<Option<Vec<u8>>, NetError> {
        let count = self.0.read(&mut self.1).await?;
        if count == 0 {
            return Ok(None);
        }
        Ok(Some(self.1[..count].to_vec()))
    }
}

/// The writing half of a [`Connection`].
pub struct ConnectionWriter(OwnedWriteHalf);

impl ConnectionWriter {
    /// Send all of `data`.
    pub async fn write(&mut self, data: Vec<u8>) -> Result<(), NetError> {
        Ok(self.0.write_all(&data).await?)
    }

    /// Send as much of `data` as the socket takes without waiting, and return the number of bytes
    /// sent. An error sends nothing, and the next [`ConnectionWriter::write`] reports it.
    pub fn try_write(&mut self, data: &[u8]) -> usize {
        self.0.try_write(data).unwrap_or(0)
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
            reader: ConnectionReader(reader, vec![0; 64 * 1024].into_boxed_slice()),
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

/// A TCP socket bound to `addr` and listening.
pub fn listen_stream(addr: SocketAddr) -> Result<StreamListener, NetError> {
    let bound = std::net::TcpListener::bind(addr)?;
    bound.set_nonblocking(true)?;
    let local_addr = bound.local_addr()?;
    Ok(StreamListener {
        bound: Some(bound),
        listener: None,
        local_addr,
    })
}
