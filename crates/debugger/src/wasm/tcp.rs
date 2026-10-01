// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The TCP transports on p3: a listener that DAP clients connect to, and a connection to a
//! listening client.

use std::cell::RefCell;
use std::future::{poll_fn, Future};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::pin::pin;
use std::task::{ready, Context, Poll};
use std::time::Duration;

use wasip3::sockets::ip_name_lookup::resolve_addresses;
use wasip3::sockets::types::{
    IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress, TcpSocket,
};
use wit_bindgen::block_on_detached;

use super::pump::{PumpConnection, ReadPump};
use crate::connection::{Connection, Listener, LocalFuture};

/// Bind a listener to `addr:port`, and return it with the address it is bound to.
pub(crate) fn listen(addr: IpAddr, port: u16) -> Result<(PumpListener, SocketAddr), String> {
    let socket = TcpSocket::create(family(addr)).map_err(|e| e.to_string())?;
    socket
        .bind(to_wasi(SocketAddr::new(addr, port)))
        .map_err(|e| e.to_string())?;
    let incoming = socket.listen().map_err(|e| e.to_string())?;
    let bound = socket.get_local_address().map_err(|e| e.to_string())?;
    let listener = PumpListener {
        incoming: RefCell::new(ReadPump::new(incoming, 1)),
        accepted: RefCell::new(None),
        _socket: socket,
    };
    Ok((listener, from_wasi(bound)))
}

/// Connect to `host:port`. Each address `host` resolves to is tried in turn, each attempt
/// bounded by `timeout`.
pub(crate) fn connect(host: &str, port: u16, timeout: Duration) -> Result<PumpConnection, String> {
    block_on_detached(connect_async(host, port, timeout))
}

async fn connect_async(host: &str, port: u16, timeout: Duration) -> Result<PumpConnection, String> {
    let addrs = match host.parse::<IpAddr>() {
        Ok(addr) => vec![addr],
        Err(_) => resolve_addresses(host.to_string())
            .await
            .map_err(|e| format!("resolving `{host}` failed: {e}"))?
            .into_iter()
            .map(from_wasi_ip)
            .collect(),
    };
    let mut last_error = None;
    for addr in addrs {
        match connect_to(SocketAddr::new(addr, port), timeout).await {
            Ok(socket) => return Ok(open(socket)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| format!("`{host}` resolves to no address")))
}

/// A socket connected to `addr`, or an error explaining why no connection was made within
/// `timeout`.
async fn connect_to(addr: SocketAddr, timeout: Duration) -> Result<TcpSocket, String> {
    let socket = TcpSocket::create(family(addr.ip())).map_err(|e| e.to_string())?;
    let connected = {
        let mut connect = pin!(socket.connect(to_wasi(addr)));
        let mut sleep = pin!(platform::clock::sleep(timeout));
        poll_fn(|cx| {
            if let Poll::Ready(result) = connect.as_mut().poll(cx) {
                return Poll::Ready(Some(result));
            }
            sleep.as_mut().poll(cx).map(|()| None)
        })
        .await
    };
    match connected {
        Some(Ok(())) => Ok(socket),
        Some(Err(error)) => Err(error.to_string()),
        None => Err(format!(
            "connecting timed out after {} ms",
            timeout.as_millis()
        )),
    }
}

/// A TCP listener whose accepted sockets are read from a [`ReadPump`].
pub(crate) struct PumpListener {
    /// Reads accepted sockets, one per read.
    incoming: RefCell<ReadPump<TcpSocket>>,
    /// A socket that was accepted and not taken yet.
    accepted: RefCell<Option<TcpSocket>>,
    _socket: TcpSocket,
}

impl PumpListener {
    /// Read from `incoming` until a socket was accepted. `Ready(false)` once accepting stopped.
    /// No read is started while an accepted socket is held.
    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<bool> {
        loop {
            if self.accepted.borrow().is_some() {
                return Poll::Ready(true);
            }
            if self.incoming.borrow().eof() {
                return Poll::Ready(false);
            }
            match ready!(self.incoming.borrow_mut().poll_read(cx)) {
                Some(sockets) => *self.accepted.borrow_mut() = sockets.into_iter().next(),
                None => {
                    eprintln!("debugger: accepting clients failed: the listener closed");
                    return Poll::Ready(false);
                }
            }
        }
    }

    fn take_accepted(&self) -> Option<Box<dyn Connection>> {
        let socket = self.accepted.borrow_mut().take()?;
        Some(Box::new(open(socket)))
    }
}

impl Listener for PumpListener {
    fn accept_blocking(&self) -> Option<Box<dyn Connection>> {
        block_on_detached(poll_fn(|cx| self.poll_accept(cx)));
        let driver = self.incoming.borrow_mut().driver.take();
        if let Some(driver) = driver {
            driver.wake();
        }
        self.take_accepted()
    }

    fn try_accept(&self) -> Option<Box<dyn Connection>> {
        self.take_accepted()
    }

    /// Records the polling task's waker in the accept pump. Stays pending once accepting
    /// stopped.
    fn ready(&self) -> LocalFuture<'_> {
        Box::pin(poll_fn(|cx| {
            self.incoming.borrow_mut().driver = Some(cx.waker().clone());
            if ready!(self.poll_accept(cx)) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }))
    }
}

/// A connection over `socket`'s receive and send streams.
fn open(socket: TcpSocket) -> PumpConnection {
    let (received, receive) = socket.receive();
    let (tx, rx) = wasip3::wit_stream::new::<u8>();
    let sent = socket.send(rx);
    PumpConnection::new(
        received,
        tx,
        async move {
            let _ = sent.await;
        },
        (socket, receive),
    )
}

fn family(addr: IpAddr) -> IpAddressFamily {
    match addr {
        IpAddr::V4(_) => IpAddressFamily::Ipv4,
        IpAddr::V6(_) => IpAddressFamily::Ipv6,
    }
}

fn to_wasi(addr: SocketAddr) -> IpSocketAddress {
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

fn from_wasi_ip(addr: IpAddress) -> IpAddr {
    match addr {
        IpAddress::Ipv4((a, b, c, d)) => Ipv4Addr::new(a, b, c, d).into(),
        IpAddress::Ipv6((a, b, c, d, e, f, g, h)) => Ipv6Addr::new(a, b, c, d, e, f, g, h).into(),
    }
}

fn from_wasi(addr: IpSocketAddress) -> SocketAddr {
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
