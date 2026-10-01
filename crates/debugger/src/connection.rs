// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The transport-independent side of a DAP connection.

use std::future::Future;
use std::pin::Pin;

/// A future borrowed from a connection or listener.
pub(crate) type LocalFuture<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// The result of taking a message without blocking.
pub(crate) enum Received {
    Message(String),
    Empty,
    Closed,
}

/// One DAP connection. Messages are JSON bodies without framing. Outgoing bodies get their `seq`
/// from the connection.
pub(crate) trait Connection {
    /// The next message, blocking until one arrives. `None` once the peer closed the connection.
    fn recv_blocking(&self) -> Option<String>;
    /// The next message if one has arrived.
    fn try_recv(&self) -> Received;
    /// Resolves once [`Connection::try_recv`] would return something other than `Empty`.
    fn ready(&self) -> LocalFuture<'_>;
    /// Queue an outgoing message.
    fn send(&self, body: String);
    /// Block until every queued message is written. Connections that write without being polled
    /// keep this default, which returns at once.
    fn flush(&self) {}
    /// Send `last`, wait until everything queued is written, and close the connection.
    fn close(&self, last: Vec<String>);
}

/// A source of connections from DAP clients.
pub(crate) trait Listener {
    /// The next connection, blocking until a client connects. `None` if the listener failed.
    fn accept_blocking(&self) -> Option<Box<dyn Connection>>;
    /// A connection if a client has connected.
    fn try_accept(&self) -> Option<Box<dyn Connection>>;
    /// Resolves once [`Listener::try_accept`] would return a connection.
    fn ready(&self) -> LocalFuture<'_>;
}
