// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The TCP transports: a listener that DAP clients connect to, and a connection to a listening
//! client.

use std::cell::Cell;
use std::io::{self, ErrorKind, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use super::threads::{notifying_channel, NotifyingReceiver, NotifyingSender, ThreadConnection};
use crate::connection::{Connection, Listener, LocalFuture};

/// How long a write to a client may block. A write that blocks longer fails, which closes the
/// connection.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind a listener to `addr:port`, and return it with the address it is bound to.
pub(crate) fn listen(addr: IpAddr, port: u16) -> io::Result<(ThreadListener, SocketAddr)> {
    let listener = TcpListener::bind((addr, port))?;
    let bound = listener.local_addr()?;
    let (streams, incoming) = notifying_channel();
    let (requests, requested) = mpsc::channel();
    std::thread::Builder::new()
        .name("dap-accept".into())
        .spawn(move || accept_loop(listener, requested, streams))?;
    Ok((
        ThreadListener {
            incoming,
            requests,
            requested: Cell::new(false),
        },
        bound,
    ))
}

/// Connect to `host:port`. Each address `host` resolves to is tried in turn, each attempt
/// bounded by `timeout`.
pub(crate) fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<ThreadConnection> {
    let mut last_error = None;
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return open(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            ErrorKind::NotFound,
            format!("`{host}` resolves to no address"),
        )
    }))
}

/// A TCP listener whose connections a thread accepts, one for each request. Connections that
/// arrive while no request is outstanding wait in the listen backlog.
pub(crate) struct ThreadListener {
    /// Accepted streams in arrival order. Closed once accepting failed.
    incoming: NotifyingReceiver<TcpStream>,
    /// Each value asks the accept thread for one stream.
    requests: Sender<()>,
    /// Set while a request is outstanding.
    requested: Cell<bool>,
}

impl ThreadListener {
    /// Ask the accept thread for a stream, unless a request is outstanding.
    fn request(&self) {
        if !self.requested.replace(true) {
            let _ = self.requests.send(());
        }
    }
}

impl Listener for ThreadListener {
    fn accept_blocking(&self) -> Option<Box<dyn Connection>> {
        loop {
            self.request();
            let stream = self.incoming.recv_blocking()?;
            self.requested.set(false);
            if let Some(connection) = connection(stream) {
                return Some(connection);
            }
        }
    }

    fn try_accept(&self) -> Option<Box<dyn Connection>> {
        let stream = self.incoming.try_recv().ok()?;
        self.requested.set(false);
        connection(stream)
    }

    /// Asks the accept thread for a stream. Stays pending once accepting failed.
    fn ready(&self) -> LocalFuture<'_> {
        Box::pin(async move {
            self.request();
            self.incoming.ready().await;
            if !self.incoming.has_value() {
                std::future::pending::<()>().await;
            }
        })
    }
}

/// A connection over `stream`, or `None` after printing why none could be set up.
fn connection(stream: TcpStream) -> Option<Box<dyn Connection>> {
    match open(stream) {
        Ok(connection) => Some(Box::new(connection)),
        Err(error) => {
            eprintln!("debugger: setting up a client connection failed: {error}");
            None
        }
    }
}

fn open(stream: TcpStream) -> io::Result<ThreadConnection> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    ThreadConnection::new(stream.try_clone()?, Writer(stream), None)
}

/// Accept one connection for each value `requests` receives and send it to `streams`, until
/// accepting fails or either side of the channels is dropped.
fn accept_loop(listener: TcpListener, requests: Receiver<()>, streams: NotifyingSender<TcpStream>) {
    for () in requests {
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionAborted | ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    eprintln!("debugger: accepting clients failed: {error}");
                    return;
                }
            }
        };
        if !streams.send(stream) {
            return;
        }
    }
}

/// The writing side of a connection's stream. Dropping it shuts the stream down in both
/// directions, which ends the reader thread's read.
struct Writer(TcpStream);

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Instant;

    use super::*;

    #[test]
    fn a_connection_is_accepted_only_once_one_is_asked_for() {
        let (listener, bound) = listen(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).unwrap();
        let _first = TcpStream::connect(bound).unwrap();
        let _second = TcpStream::connect(bound).unwrap();
        assert!(listener.accept_blocking().is_some());
        std::thread::sleep(Duration::from_millis(200));
        assert!(listener.try_accept().is_none());
        assert!(listener.accept_blocking().is_some());
    }

    #[test]
    fn closing_a_connection_to_a_client_that_does_not_read_ends_after_the_write_timeout() {
        let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        let (_peer, _) = server.accept().unwrap();
        let connection = open(client).unwrap();
        let chunk = format!("\"{}\"", "x".repeat(1024 * 1024));
        for _ in 0..64 {
            connection.send(chunk.clone());
        }
        let start = Instant::now();
        connection.close(Vec::new());
        assert!(start.elapsed() < WRITE_TIMEOUT * 4, "{:?}", start.elapsed());
    }
}
