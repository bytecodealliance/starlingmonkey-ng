// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The TCP transports on p3: a listener that DAP clients connect to, and a connection to a
//! listening client.

use std::cell::RefCell;
use std::future::poll_fn;
use std::net::{IpAddr, SocketAddr};
use std::task::{ready, Context, Poll};
use std::time::Duration;

use platform::block_on;
use platform::net::TcpSocket;
use platform::stream::ReadPump;

use super::pump::PumpConnection;
use crate::connection::{Connection, Listener, LocalFuture};

/// Bind a listener to `addr:port`, and return it with the address it is bound to.
pub(crate) fn listen(addr: IpAddr, port: u16) -> Result<(PumpListener, SocketAddr), String> {
    let listener = platform::net::listen(SocketAddr::new(addr, port)).map_err(|e| e.to_string())?;
    let bound = listener.local_addr;
    let listener = PumpListener {
        incoming: RefCell::new(ReadPump::new(listener.incoming, 1)),
        accepted: RefCell::new(None),
        _socket: listener.socket,
    };
    Ok((listener, bound))
}

/// Connect to `host:port`. Each address `host` resolves to is tried in turn, each attempt
/// bounded by `timeout`.
pub(crate) fn connect(host: &str, port: u16, timeout: Duration) -> Result<PumpConnection, String> {
    block_on(connect_async(host, port, timeout))
}

async fn connect_async(host: &str, port: u16, timeout: Duration) -> Result<PumpConnection, String> {
    let addrs = platform::net::resolve(host)
        .await
        .map_err(|e| format!("resolving `{host}` failed: {e}"))?;
    let mut last_error = None;
    for addr in addrs {
        match platform::net::connect(SocketAddr::new(addr, port), Some(timeout)).await {
            Ok(socket) => return Ok(open(socket)),
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(last_error.unwrap_or_else(|| format!("`{host}` resolves to no address")))
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
        block_on(poll_fn(|cx| self.poll_accept(cx)));
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
