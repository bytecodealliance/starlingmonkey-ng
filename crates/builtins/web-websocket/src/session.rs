// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The WebSocket protocol of one connection, over tungstenite and, for `wss:` URLs,
//! [`platform::tls`], without I/O of its own.
//!
//! The caller passes the bytes it reads from the connection to [`Session::receive`] and
//! [`Session::receive_end`], and writes the bytes [`Session::take_outgoing`] returns. tungstenite
//! reads from and writes to a [`Pipe`], which holds those bytes.
//!
//! Incoming frames and messages are bounded by [`MAX_MESSAGE_SIZE`]. tungstenite reserves the
//! declared length of a frame when its header arrives, and assembles fragmented messages, with
//! allocations that aren't fallible. The bound limits the size of those allocations.

use std::io::{self, Read, Write};

use tungstenite::handshake::client::{generate_key, ClientHandshake, Request, Response};
use tungstenite::handshake::{HandshakeError, MidHandshake};
use tungstenite::http::header::{HeaderName, HeaderValue};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::frame::{CloseFrame, Utf8Bytes};
use tungstenite::protocol::{WebSocket, WebSocketConfig};
use tungstenite::{Bytes, Error, Message};

use platform::tls::TlsClient;

/// The largest message and the largest frame a session receives, in bytes.
pub const MAX_MESSAGE_SIZE: usize = 64 << 20;

/// The bytes tungstenite reads from and writes to: what was received and not yet read, and what
/// was written and not yet taken.
#[derive(Default)]
struct Pipe {
    incoming: Vec<u8>,
    /// How much of `incoming` was read.
    read: usize,
    /// Whether the peer ended the connection, which makes reading past `incoming` return 0.
    ended: bool,
    outgoing: Vec<u8>,
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = &self.incoming[self.read..];
        if available.is_empty() {
            self.incoming.clear();
            self.read = 0;
            return match self.ended {
                true => Ok(0),
                false => Err(io::ErrorKind::WouldBlock.into()),
            };
        }
        let count = available.len().min(buf.len());
        buf[..count].copy_from_slice(&available[..count]);
        self.read += count;
        Ok(count)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.outgoing
            .try_reserve(buf.len())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        self.outgoing.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The state of the protocol.
enum Protocol {
    /// The opening handshake is in progress.
    Handshake(MidHandshake<ClientHandshake<Pipe>>),
    /// The connection is established.
    Open(WebSocket<Pipe>),
    /// The connection failed or ended, and only the bytes in `Pipe::outgoing` remain to be sent.
    Ended(Pipe),
}

/// What happened on a connection, in the order it happened.
pub enum Event {
    /// The opening handshake completed, with the subprotocol the server selected, or the empty
    /// string.
    Open { protocol: String },
    /// A text message.
    Text(Utf8Bytes),
    /// A binary message.
    Binary(Bytes),
    /// A close frame was received, with its status code (1005 for none) and reason. The closing
    /// handshake started then, if it hadn't before.
    CloseFrame { code: u16, reason: String },
    /// The connection ended. It ended cleanly if the closing handshake completed first.
    Closed { clean: bool },
    /// The connection failed for the reason given, and ended.
    Failed(String),
}

/// The WebSocket protocol of one client connection.
pub struct Session {
    /// The TLS session of a `wss:` connection.
    tls: Option<TlsClient>,
    /// The state of the protocol, which is `None` only while it changes.
    protocol: Option<Protocol>,
    /// Whether a close frame was received.
    close_received: bool,
    /// Why the TLS session of a `wss:` connection couldn't be created, if it couldn't.
    tls_failure: Option<String>,
}

/// What a session needs to start: the URL of the connection, and the headers of the opening
/// handshake besides the ones every handshake has.
pub struct Opening<'a> {
    pub url: &'a url::Url,
    /// `Host`'s value.
    pub host: String,
    /// The requested subprotocols, in order.
    pub protocols: &'a [String],
    /// `Origin`'s value, if there is one.
    pub origin: Option<String>,
    /// Further headers, as names and values. Those named like one of [`HANDSHAKE_HEADERS`] are
    /// left out, since the handshake sets its own.
    pub headers: &'a [(String, String)],
}

/// The headers whose values every opening handshake sets itself.
const HANDSHAKE_HEADERS: [&str; 5] = [
    "Host",
    "Connection",
    "Upgrade",
    "Sec-WebSocket-Version",
    "Sec-WebSocket-Key",
];

impl Session {
    /// A session that starts the opening handshake for `opening`, and the TLS handshake first
    /// for a `wss:` URL. Fails if the request can't be built, such as for a header value that
    /// isn't valid. A TLS session that can't be created is [`Session::tls_failure`].
    pub fn start(opening: Opening<'_>) -> Result<Session, String> {
        let tls = match opening.url.scheme() {
            "wss" => {
                let host = match opening.url.host() {
                    Some(url::Host::Ipv6(addr)) => addr.to_string(),
                    Some(host) => host.to_string(),
                    None => String::new(),
                };
                Some(TlsClient::new(&host).map_err(|error| error.to_string()))
            }
            _ => None,
        };
        let (tls, tls_failure) = match tls {
            Some(Ok(tls)) => (Some(tls), None),
            Some(Err(reason)) => (None, Some(reason)),
            None => (None, None),
        };
        let mut target = opening.url.path().to_string();
        if let Some(query) = opening.url.query() {
            target.push('?');
            target.push_str(query);
        }
        let uri = format!("{}://{}{}", opening.url.scheme(), opening.host, target);
        let mut request = Request::builder()
            .uri(uri)
            .header("Host", &opening.host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", generate_key());
        if !opening.protocols.is_empty() {
            request = request.header("Sec-WebSocket-Protocol", opening.protocols.join(", "));
        }
        if let Some(origin) = opening.origin {
            request = request.header("Origin", origin);
        }
        let further = opening.headers.iter().filter(|(name, _)| {
            !HANDSHAKE_HEADERS
                .iter()
                .any(|own| own.eq_ignore_ascii_case(name))
        });
        for (name, value) in further {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?;
            let value = HeaderValue::from_bytes(value.as_bytes()).map_err(|e| e.to_string())?;
            request = request.header(name, value);
        }
        let request = request.body(()).map_err(|error| error.to_string())?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_SIZE))
            .max_frame_size(Some(MAX_MESSAGE_SIZE))
            .write_buffer_size(0);
        let handshake = ClientHandshake::start(Pipe::default(), request, Some(config))
            .map_err(|error| error.to_string())?;
        let mut session = Session {
            tls,
            protocol: Some(Protocol::Handshake(handshake)),
            close_received: false,
            tls_failure,
        };
        let mut events = Vec::new();
        session.advance(&mut events);
        match events.pop() {
            Some(Event::Failed(reason)) => Err(reason),
            _ => Ok(session),
        }
    }

    /// Why the TLS session of a `wss:` connection couldn't be created, if it couldn't. Such a
    /// session's connection fails before it connects.
    pub fn tls_failure(&self) -> Option<&str> {
        self.tls_failure.as_deref()
    }

    /// Process `bytes`, the next bytes received, and append what happened to `events`.
    pub fn receive(&mut self, bytes: &[u8], events: &mut Vec<Event>) {
        let pipe = match &mut self.protocol {
            Some(Protocol::Handshake(handshake)) => handshake.get_mut().get_mut(),
            Some(Protocol::Open(socket)) => socket.get_mut(),
            _ => return,
        };
        let result = match &mut self.tls {
            Some(tls) => match tls.receive(bytes, &mut pipe.incoming) {
                Ok(closed) => {
                    pipe.ended |= closed;
                    Ok(())
                }
                Err(error) => Err(error.to_string()),
            },
            None => match pipe.incoming.try_reserve(bytes.len()) {
                Ok(()) => {
                    pipe.incoming.extend_from_slice(bytes);
                    Ok(())
                }
                Err(_) => Err("out of memory".to_string()),
            },
        };
        if let Err(reason) = result {
            self.fail(reason, None, events);
            return;
        }
        self.advance(events);
    }

    /// Process the end of the connection, which the peer ended, and append what happened to
    /// `events`.
    pub fn receive_end(&mut self, events: &mut Vec<Event>) {
        if let Some(pipe) = self.pipe() {
            pipe.ended = true;
            self.advance(events);
        }
    }

    /// Send `message`, unless the connection isn't open or the closing handshake started.
    /// Returns whether the message was queued.
    pub fn send(&mut self, message: Message, events: &mut Vec<Event>) -> bool {
        let Some(Protocol::Open(socket)) = &mut self.protocol else {
            return false;
        };
        if !socket.can_write() {
            return false;
        }
        match socket.send(message) {
            Ok(()) => true,
            Err(error) => {
                self.fail(error.to_string(), None, events);
                false
            }
        }
    }

    /// Start the closing handshake with a close frame with `code` and `reason`, or without a
    /// status code for `None`, unless it started already.
    pub fn close(&mut self, code: Option<u16>, reason: &str, events: &mut Vec<Event>) {
        let Some(Protocol::Open(socket)) = &mut self.protocol else {
            return;
        };
        if !socket.can_write() {
            return;
        }
        let frame = code.map(|code| CloseFrame {
            code: CloseCode::from(code),
            reason: Utf8Bytes::from(reason),
        });
        if let Err(error) = socket.close(frame) {
            self.fail(error.to_string(), None, events);
        }
    }

    /// Fail the connection for `reason`: send a close frame with `code` if the connection is
    /// established and the closing handshake didn't start, and end the protocol. Appends
    /// [`Event::Failed`] to `events`.
    pub fn fail(&mut self, reason: String, code: Option<u16>, events: &mut Vec<Event>) {
        let mut pipe = match self.protocol.take() {
            Some(Protocol::Handshake(mut handshake)) => {
                std::mem::take(handshake.get_mut().get_mut())
            }
            Some(Protocol::Open(mut socket)) => {
                if let Some(code) = code {
                    let frame = CloseFrame {
                        code: CloseCode::from(code),
                        reason: Utf8Bytes::default(),
                    };
                    let _ = socket.close(Some(frame));
                }
                socket.into_inner()
            }
            Some(Protocol::Ended(pipe)) => pipe,
            None => Pipe::default(),
        };
        pipe.incoming = Vec::new();
        self.protocol = Some(Protocol::Ended(pipe));
        events.push(Event::Failed(reason));
    }

    /// The bytes to send, which are removed from the session.
    pub fn take_outgoing(&mut self) -> Vec<u8> {
        let plaintext = match &mut self.protocol {
            Some(Protocol::Handshake(handshake)) => {
                std::mem::take(&mut handshake.get_mut().get_mut().outgoing)
            }
            Some(Protocol::Open(socket)) => std::mem::take(&mut socket.get_mut().outgoing),
            Some(Protocol::Ended(pipe)) => std::mem::take(&mut pipe.outgoing),
            None => Vec::new(),
        };
        match &mut self.tls {
            Some(tls) => {
                if !plaintext.is_empty() && tls.send(&plaintext).is_err() {
                    return Vec::new();
                }
                tls.take_outgoing()
            }
            None => plaintext,
        }
    }

    /// Whether the protocol ended, so that nothing but [`Session::take_outgoing`]'s bytes remain to
    /// be sent.
    pub fn ended(&self) -> bool {
        matches!(self.protocol, Some(Protocol::Ended(_)))
    }

    /// Whether the closing handshake completed: a close frame was both sent and received.
    pub fn close_handshake_completed(&self) -> bool {
        match &self.protocol {
            Some(Protocol::Open(socket)) => self.close_received && !socket.can_write(),
            _ => false,
        }
    }

    /// End the TLS session of a `wss:` connection with a `close_notify` alert.
    pub fn close_tls(&mut self) {
        if let Some(tls) = &mut self.tls {
            tls.close();
        }
    }

    /// The pipe of the protocol, unless it ended.
    fn pipe(&mut self) -> Option<&mut Pipe> {
        match &mut self.protocol {
            Some(Protocol::Handshake(handshake)) => Some(handshake.get_mut().get_mut()),
            Some(Protocol::Open(socket)) => Some(socket.get_mut()),
            _ => None,
        }
    }

    /// Process the bytes the pipe received, and append what happened to `events`.
    fn advance(&mut self, events: &mut Vec<Event>) {
        if let Some(Protocol::Handshake(_)) = &self.protocol {
            let Some(Protocol::Handshake(handshake)) = self.protocol.take() else {
                unreachable!("the protocol is in the handshake");
            };
            match handshake.handshake() {
                Ok((socket, response)) => {
                    let protocol = match accepted_protocol(&response) {
                        Ok(protocol) => protocol,
                        Err(reason) => {
                            self.protocol = Some(Protocol::Open(socket));
                            self.fail(reason, None, events);
                            return;
                        }
                    };
                    self.protocol = Some(Protocol::Open(socket));
                    events.push(Event::Open { protocol });
                }
                Err(HandshakeError::Interrupted(handshake)) => {
                    self.protocol = Some(Protocol::Handshake(handshake));
                    return;
                }
                Err(HandshakeError::Failure(error)) => {
                    self.protocol = Some(Protocol::Ended(Pipe::default()));
                    events.push(Event::Failed(handshake_failure(&error)));
                    return;
                }
            }
        }
        loop {
            let Some(Protocol::Open(socket)) = &mut self.protocol else {
                return;
            };
            match socket.read() {
                Ok(Message::Text(text)) => events.push(Event::Text(text)),
                Ok(Message::Binary(bytes)) => events.push(Event::Binary(bytes)),
                Ok(Message::Close(frame)) => {
                    self.close_received = true;
                    let (code, reason) = match frame {
                        Some(frame) => (u16::from(frame.code), frame.reason.to_string()),
                        None => (1005, String::new()),
                    };
                    events.push(Event::CloseFrame { code, reason });
                }
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Err(Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(Error::ConnectionClosed | Error::AlreadyClosed) => {
                    self.end(events, self.close_received);
                    return;
                }
                Err(Error::Protocol(
                    tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                )) => {
                    self.end(events, false);
                    return;
                }
                Err(error) => {
                    let code = match &error {
                        Error::Utf8(_) => 1007,
                        Error::Capacity(_) => 1009,
                        _ => 1002,
                    };
                    self.fail(error.to_string(), Some(code), events);
                    return;
                }
            }
        }
    }

    /// End the protocol after the peer ended the connection, and append [`Event::Closed`] to
    /// `events`.
    fn end(&mut self, events: &mut Vec<Event>, clean: bool) {
        let pipe = match self.protocol.take() {
            Some(Protocol::Open(socket)) => socket.into_inner(),
            _ => Pipe::default(),
        };
        self.protocol = Some(Protocol::Ended(pipe));
        events.push(Event::Closed { clean });
    }
}

/// The subprotocol the server selected in `response`, or the reason the response fails the
/// connection: an extension, since the opening handshake requests none.
fn accepted_protocol(response: &Response) -> Result<String, String> {
    if response.headers().contains_key("Sec-WebSocket-Extensions") {
        return Err("the server selected an extension that wasn't requested".to_string());
    }
    Ok(response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string())
}

/// The reason for the failure of the opening handshake that failed with `error`.
fn handshake_failure(error: &Error) -> String {
    match error {
        Error::Http(response) => format!(
            "the server responded with status {} instead of switching protocols",
            response.status().as_u16()
        ),
        error => format!("the opening handshake failed: {error}"),
    }
}
