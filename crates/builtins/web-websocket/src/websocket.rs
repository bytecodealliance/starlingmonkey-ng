// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [`WebSocket`](https://websockets.spec.whatwg.org/#the-websocket-interface) interface.
//!
//! A socket's connection is a `platform::net` TCP connection, and a [`Session`] processes its
//! bytes. Connecting, each read and each write is a future spawned with a pending promise. The
//! future holds the socket rooted, and its settle callback processes the result and dispatches
//! the events. While a read or write is in progress, its future holds that half of the
//! connection. An open socket keeps the event loop alive, since a read is always in progress.

use std::cell::Cell;
use std::collections::VecDeque;

use core_runtime::event_loop::{with_active_event_loop, Task, TaskId};
use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods, webidl_union};
use js::conversion::{Clamp, FromJSVal, ToJSVal};
use js::error::ExnThrown;
use js::gc::handle::{Heap, OptionHeapExt, RootedHeap};
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::JSTracer;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Callable, Promise};
use platform::net::{ConnectionReader, ConnectionWriter, NetError};
use tungstenite::{Bytes, Message};
use url::Url;
use web_fetch::headers::HeadersInit;
use web_files::blob::{Blob, BlobPart};
use web_globals::dom_exception::throw_dom_exception;
use web_globals::events::algorithms::ScriptStackState;
use web_globals::events::event_target::{EventTarget, EventTargetImpl};
use web_globals::events::message_event::MessageEvent;
use web_globals::events::Event;

use crate::close_event::CloseEvent;
use crate::session::{self, Opening, Session};

js::instance_local! {
    /// Whether the constructor accepts Node.js's options. See [`set_node_compat`].
    static NODE_COMPAT: Cell<bool> = const { Cell::new(false) };
}

/// Set whether the constructor accepts the options object of Node.js's `WebSocket` as its second
/// argument, a `WebSocketInit` dictionary with `protocols` and `headers`, which it doesn't by
/// default.
pub fn set_node_compat(node_compat: bool) {
    NODE_COMPAT.with(|cell| cell.set(node_compat));
}

js::webidl_enum! {
    /// <https://websockets.spec.whatwg.org/#enumdef-binarytype>
    pub enum BinaryType {
        Blob => "blob",
        ArrayBuffer => "arraybuffer",
    }
}

/// The `protocols` argument, `(DOMString or sequence<DOMString>)`.
#[webidl_union]
pub enum Protocols {
    Sequence(Vec<String>),
    String(String),
}

/// The options of Node.js's `WebSocket`.
#[derive(Default)]
#[webidl_dictionary]
pub struct WebSocketInit {
    pub protocols: Option<Protocols>,
    pub headers: Option<HeadersInit>,
}

/// A connection and the protocol over it.
#[derive(Default)]
struct Link {
    session: Option<Session>,
    /// The halves of the connection that no read or write in progress holds. Both are `None`
    /// before the connection is established and after it ends.
    reader: Option<ConnectionReader>,
    writer: Option<ConnectionWriter>,
    /// The bytes to send that weren't handed to the connection yet.
    unsent: Vec<u8>,
    /// How many bytes the session produced, and how many of them were handed to the connection.
    produced: u64,
    sent: u64,
    /// For each message whose frame wasn't completely sent, the number of bytes the session had
    /// produced at the end of its frame, and the length of its data.
    messages: VecDeque<(u64, u64)>,
}

/// <https://websockets.spec.whatwg.org/#websocket>
#[webidl_interface(extends = EventTarget)]
pub struct WebSocket {
    parent: EventTargetImpl,
    /// <https://websockets.spec.whatwg.org/#concept-websocket-url>, serialized.
    #[no_trace]
    url: String,
    /// The serialization of the origin of `url`, the `origin` of message events.
    #[no_trace]
    origin: String,
    /// <https://websockets.spec.whatwg.org/#dom-websocket-readystate>
    #[no_trace]
    ready_state: u16,
    /// <https://websockets.spec.whatwg.org/#dom-websocket-protocol>
    #[no_trace]
    protocol: String,
    /// Whether `binaryType` is "`arraybuffer`".
    #[no_trace]
    binary_arraybuffer: bool,
    /// <https://websockets.spec.whatwg.org/#dom-websocket-bufferedamount>
    #[no_trace]
    buffered_amount: f64,
    /// The status code and reason of the close frame received, if one was.
    #[no_trace]
    close_frame: Option<(u16, String)>,
    #[no_trace]
    link: Link,
    /// Whether a task to update `buffered_amount` is queued.
    #[no_trace]
    update_queued: bool,
    /// The promises of the connect, read and write in progress.
    connect_op: Option<Heap<js::promise::Promise>>,
    read_op: Option<Heap<js::promise::Promise>>,
    write_op: Option<Heap<js::promise::Promise>>,
    onopen_handler: Option<Heap<js::function::Callable>>,
    onerror_handler: Option<Heap<js::function::Callable>>,
    onclose_handler: Option<Heap<js::function::Callable>>,
    onmessage_handler: Option<Heap<js::function::Callable>>,
}

#[webidl_methods]
impl WebSocket {
    pub const CONNECTING: u16 = 0;
    pub const OPEN: u16 = 1;
    pub const CLOSING: u16 = 2;
    pub const CLOSED: u16 = 3;

    /// <https://websockets.spec.whatwg.org/#dom-websocket-websocket>
    #[constructor]
    fn new(
        &self,
        scope: &Scope<'_>,
        url: String,
        protocols: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        if with_active_event_loop(|_| ()).is_none() {
            return Err(core_runtime::event_loop::throw_no_event_loop(
                scope,
                "`WebSocket`",
            ));
        }
        let (protocols, headers) = convert_protocols(scope, protocols)?;
        // Step 1: Let _baseURL_ be `this`'s `relevant settings object`'s `API base URL`.
        let base = web_globals::worker_location::current_location_url();
        // Step 2: Let _urlRecord_ be the result of `getting a URL record` given _url_ and
        //     _baseURL_.
        // Step 3: If _urlRecord_ is failure, then throw a "``SyntaxError``" ``DOMException``.
        let url_record = get_url_record(&url, base.as_ref())
            .map_err(|message| throw_dom_exception(scope, "SyntaxError", message))?;
        // Step 4: If _protocols_ is a string, set _protocols_ to a sequence consisting of just
        //     that string.
        // Step 5: If any of the values in _protocols_ occur more than once or otherwise fail to
        //     match the requirements for elements that comprise the value of
        //     ``Sec-WebSocket-Protocol`` fields as defined by The WebSocket protocol, then throw a
        //     "``SyntaxError``" ``DOMException``.
        for (index, protocol) in protocols.iter().enumerate() {
            if protocol.is_empty() || !protocol.bytes().all(is_token_byte) {
                return Err(throw_dom_exception(
                    scope,
                    "SyntaxError",
                    &format!("Invalid subprotocol `{protocol}`"),
                ));
            }
            if protocols[..index]
                .iter()
                .any(|other| other.eq_ignore_ascii_case(protocol))
            {
                return Err(throw_dom_exception(
                    scope,
                    "SyntaxError",
                    &format!("The subprotocol `{protocol}` is repeated"),
                ));
            }
        }
        // Step 6: Set `this`'s `url` to _urlRecord_.
        let host = match url_record.port() {
            Some(port) => format!("{}:{port}", url_record.host_str().unwrap_or_default()),
            None => url_record.host_str().unwrap_or_default().to_string(),
        };
        {
            let mut data = self.data_mut();
            data.url = url_record.to_string();
            data.origin = url_record.origin().ascii_serialization();
        }
        // Step 7: Let _client_ be `this`'s `relevant settings object`.
        // Step 8: Run this step `in parallel`:
        // Step 8.1: `Establish a WebSocket connection` given _urlRecord_, _protocols_, and
        //     _client_.
        let origin = base
            .map(|base| base.origin())
            .filter(|origin| origin.is_tuple())
            .map(|origin| origin.ascii_serialization());
        let mut headers = headers;
        if let Some(authorization) = url_credentials(&url_record) {
            headers.push(("Authorization".to_string(), authorization));
        }
        let session = Session::start(Opening {
            url: &url_record,
            host,
            protocols: &protocols,
            origin,
            headers: &headers,
        })
        .map_err(|message| throw_dom_exception(scope, "SyntaxError", &message))?;
        self.data_mut().link.session = Some(session);
        self.connect(scope, url_record)
    }

    /// <https://websockets.spec.whatwg.org/#dom-websocket-url>
    #[getter]
    fn url(&self) -> String {
        self.data().url.clone()
    }

    /// <https://websockets.spec.whatwg.org/#dom-websocket-readystate>
    #[getter]
    fn ready_state(&self) -> u16 {
        self.data().ready_state
    }

    /// <https://websockets.spec.whatwg.org/#dom-websocket-bufferedamount>
    #[getter]
    fn buffered_amount(&self) -> f64 {
        self.data().buffered_amount
    }

    /// <https://websockets.spec.whatwg.org/#dom-websocket-extensions>: no extension is ever
    /// negotiated.
    #[getter]
    fn extensions(&self) -> String {
        String::new()
    }

    /// <https://websockets.spec.whatwg.org/#dom-websocket-protocol>
    #[getter]
    fn protocol(&self) -> String {
        self.data().protocol.clone()
    }

    /// <https://websockets.spec.whatwg.org/#dom-websocket-binarytype>
    #[getter]
    fn binary_type(&self) -> &'static str {
        match self.data().binary_arraybuffer {
            true => BinaryType::ArrayBuffer.as_str(),
            false => BinaryType::Blob.as_str(),
        }
    }

    /// <https://websockets.spec.whatwg.org/#dom-websocket-binarytype>. A value that isn't a
    /// `BinaryType` is ignored.
    #[setter]
    fn set_binary_type(&self, value: String) {
        if let Ok(binary_type) = value.parse::<BinaryType>() {
            self.data_mut().binary_arraybuffer = binary_type == BinaryType::ArrayBuffer;
        }
    }

    /// <https://websockets.spec.whatwg.org/#dom-websocket-close>
    #[method]
    fn close(
        &self,
        scope: &Scope<'_>,
        code: Option<Clamp<u16>>,
        reason: Option<String>,
    ) -> Result<(), ExnThrown> {
        let code = code.map(|code| code.0);
        // Step 1: If _code_ is present, but is neither an integer equal to 1000 nor an integer in
        //     the range 3000 to 4999, inclusive, throw an "``InvalidAccessError``"
        //     ``DOMException``.
        if let Some(code) = code {
            if code != 1000 && !(3000..=4999).contains(&code) {
                return Err(throw_dom_exception(
                    scope,
                    "InvalidAccessError",
                    &format!("The close code must be 1000 or between 3000 and 4999, not {code}"),
                ));
            }
        }
        // Step 2: If _reason_ is present, then run these substeps:
        // Step 2.1: Let _reasonBytes_ be the result of `encoding` _reason_.
        // Step 2.2: If _reasonBytes_ is longer than 123 bytes, then throw a "``SyntaxError``"
        //     ``DOMException``.
        if let Some(reason) = &reason {
            if reason.len() > 123 {
                return Err(throw_dom_exception(
                    scope,
                    "SyntaxError",
                    "The close reason must not be longer than 123 bytes",
                ));
            }
        }
        // Step 3: Run the first matching steps from the following list:
        let ready_state = self.data().ready_state;
        match ready_state {
            // If `this`'s `ready state` is `CLOSING` (2) or `CLOSED` (3): Do nothing.
            WebSocketImpl::CLOSING | WebSocketImpl::CLOSED => return Ok(()),
            // If the WebSocket connection is not yet `established`: `Fail the WebSocket
            //     connection` and set `this`'s `ready state` to `CLOSING` (2).
            WebSocketImpl::CONNECTING => {
                self.data_mut().ready_state = WebSocketImpl::CLOSING;
                self.cancel_connect(scope);
                self.end_link(scope);
                return self.queue_task(TaskKind::Failed);
            }
            _ => {}
        }
        // If the `WebSocket closing handshake` has not yet been `started`: `Start the WebSocket
        //     closing handshake` and set `this`'s `ready state` to `CLOSING` (2).
        //   - If _code_ is not present and _reason_ is present, the status code of the close
        //     frame is 1000.
        // Otherwise: Set `this`'s `ready state` to `CLOSING` (2).
        let code = match (code, &reason) {
            (None, Some(_)) => Some(1000),
            (code, _) => code,
        };
        self.data_mut().ready_state = WebSocketImpl::CLOSING;
        let mut events = Vec::new();
        if let Some(session) = self.data_mut().link.session.as_mut() {
            session.close(code, reason.as_deref().unwrap_or_default(), &mut events);
        }
        self.flush(scope)?;
        self.handle_events(scope, events)
    }

    /// <https://websockets.spec.whatwg.org/#dom-websocket-send>. `data` is `(BufferSource or Blob
    /// or USVString)`, the union of [`BlobPart`].
    #[method]
    fn send(&self, scope: &Scope<'_>, data: BlobPart<'_>) -> Result<(), ExnThrown> {
        // Step 1: If `this`'s `ready state` is `CONNECTING`, then throw an
        //     "``InvalidStateError``" ``DOMException``.
        if self.data().ready_state == WebSocketImpl::CONNECTING {
            return Err(throw_dom_exception(
                scope,
                "InvalidStateError",
                "The WebSocket is still connecting",
            ));
        }
        // Step 2: Run the appropriate set of steps from the following list:
        let message = match data {
            BlobPart::USVString(text) => Message::Text(text.into()),
            BlobPart::Blob(blob) => Message::Binary(blob.data().bytes.clone()),
            // SAFETY: nothing runs JavaScript while the bytes are borrowed.
            BlobPart::ArrayBuffer(buffer) => match copy_bytes(unsafe { buffer.bytes() }) {
                Some(bytes) => Message::Binary(bytes),
                None => return self.fail_full(scope),
            },
            // SAFETY: nothing runs JavaScript while the bytes are borrowed.
            BlobPart::ArrayBufferView(view) => match copy_bytes(unsafe { view.bytes() }) {
                Some(bytes) => Message::Binary(bytes),
                None => return self.fail_full(scope),
            },
        };
        // Each set of steps increases `bufferedAmount` by the length of the data, and sends it if
        // the connection is established and the closing handshake hasn't started.
        let length = message.len() as u64;
        self.data_mut().buffered_amount += length as f64;
        let mut events = Vec::new();
        let sent = {
            let mut data = self.data_mut();
            let open = data.ready_state == WebSocketImpl::OPEN;
            match data.link.session.as_mut() {
                Some(session) if open => session.send(message, &mut events),
                _ => false,
            }
        };
        if sent {
            let mut data = self.data_mut();
            let link = &mut data.link;
            let outgoing = link.session.as_mut().map(Session::take_outgoing);
            if let Some(outgoing) = outgoing {
                if !queue_bytes(link, outgoing) {
                    drop(data);
                    return self.fail_full(scope);
                }
            }
            let end = link.produced;
            link.messages.push_back((end, length));
        }
        self.flush(scope)?;
        self.handle_events(scope, events)
    }

    /// <https://websockets.spec.whatwg.org/#handler-websocket-onopen>
    #[getter]
    fn onopen<'r>(&self, scope: &'r Scope<'_>) -> Option<Callable<'r>> {
        self.data().onopen_handler.get(scope)
    }

    /// <https://websockets.spec.whatwg.org/#handler-websocket-onopen>
    #[setter]
    fn set_onopen(&self, scope: &Scope<'_>, value: HandleValue<'_>) {
        let old = self.data().onopen_handler.get(scope);
        let handler = web_globals::events::replace_event_handler(scope, self, "open", old, value);
        match handler {
            Some(handler) => self.data_mut().onopen_handler.set(handler),
            None => self.data_mut().onopen_handler = None,
        }
    }

    /// <https://websockets.spec.whatwg.org/#handler-websocket-onerror>
    #[getter]
    fn onerror<'r>(&self, scope: &'r Scope<'_>) -> Option<Callable<'r>> {
        self.data().onerror_handler.get(scope)
    }

    /// <https://websockets.spec.whatwg.org/#handler-websocket-onerror>
    #[setter]
    fn set_onerror(&self, scope: &Scope<'_>, value: HandleValue<'_>) {
        let old = self.data().onerror_handler.get(scope);
        let handler = web_globals::events::replace_event_handler(scope, self, "error", old, value);
        match handler {
            Some(handler) => self.data_mut().onerror_handler.set(handler),
            None => self.data_mut().onerror_handler = None,
        }
    }

    /// <https://websockets.spec.whatwg.org/#handler-websocket-onclose>
    #[getter]
    fn onclose<'r>(&self, scope: &'r Scope<'_>) -> Option<Callable<'r>> {
        self.data().onclose_handler.get(scope)
    }

    /// <https://websockets.spec.whatwg.org/#handler-websocket-onclose>
    #[setter]
    fn set_onclose(&self, scope: &Scope<'_>, value: HandleValue<'_>) {
        let old = self.data().onclose_handler.get(scope);
        let handler = web_globals::events::replace_event_handler(scope, self, "close", old, value);
        match handler {
            Some(handler) => self.data_mut().onclose_handler.set(handler),
            None => self.data_mut().onclose_handler = None,
        }
    }

    /// <https://websockets.spec.whatwg.org/#handler-websocket-onmessage>
    #[getter]
    fn onmessage<'r>(&self, scope: &'r Scope<'_>) -> Option<Callable<'r>> {
        self.data().onmessage_handler.get(scope)
    }

    /// <https://websockets.spec.whatwg.org/#handler-websocket-onmessage>
    #[setter]
    fn set_onmessage(&self, scope: &Scope<'_>, value: HandleValue<'_>) {
        let old = self.data().onmessage_handler.get(scope);
        let handler =
            web_globals::events::replace_event_handler(scope, self, "message", old, value);
        match handler {
            Some(handler) => self.data_mut().onmessage_handler.set(handler),
            None => self.data_mut().onmessage_handler = None,
        }
    }
}

impl<'s> WebSocket<'s> {
    /// Connect to the host and port of `url`, and start the opening handshake once connected.
    /// A bad port, or a TLS session that couldn't be created, fails the connection without
    /// connecting.
    fn connect(&self, scope: &Scope<'_>, url: Url) -> Result<(), ExnThrown> {
        let port = url.port_or_known_default().unwrap_or_default();
        let host = match url.host() {
            Some(url::Host::Ipv6(addr)) => addr.to_string(),
            Some(host) => host.to_string(),
            None => String::new(),
        };
        let tls_failure = self
            .data()
            .link
            .session
            .as_ref()
            .and_then(|session| session.tls_failure().map(str::to_string));
        let socket = RootedHeap::new(*self);
        let future = async move {
            let result = match (tls_failure, web_fetch::algorithms::is_bad_port(port)) {
                (Some(reason), _) => Err(reason),
                (None, true) => Err(format!("port {port} is blocked")),
                (None, false) => connect_to(&host, port).await,
            };
            PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
                let socket = socket.get(scope);
                socket.data_mut().connect_op = None;
                socket.connected(scope, result)?;
                Ok(HandleValue::undefined())
            }))
        };
        let promise = Promise::new_pending(scope)?;
        promise.spawn(PromiseFuture::new(future));
        self.data_mut().connect_op = Some(Heap::from(promise));
        Ok(())
    }

    /// Start the opening handshake over the connection of `result`, or fail the WebSocket
    /// connection if connecting failed.
    fn connected(
        &self,
        scope: &Scope<'_>,
        result: Result<platform::net::Connection, String>,
    ) -> Result<(), ExnThrown> {
        let connection = match result {
            Ok(connection) => connection,
            Err(reason) => {
                return self.handle_events(scope, vec![session::Event::Failed(reason)]);
            }
        };
        {
            let mut data = self.data_mut();
            data.link.reader = Some(connection.reader);
            data.link.writer = Some(connection.writer);
        }
        self.flush(scope)?;
        self.start_read(scope)
    }

    /// Read the next bytes of the connection, unless a read is in progress or the connection
    /// ended.
    fn start_read(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let Some(mut reader) = self.data_mut().link.reader.take() else {
            return Ok(());
        };
        let socket = RootedHeap::new(*self);
        let future = async move {
            let result = reader.read().await;
            PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
                let socket = socket.get(scope);
                socket.data_mut().read_op = None;
                socket.received(scope, reader, result)?;
                Ok(HandleValue::undefined())
            }))
        };
        let promise = Promise::new_pending(scope)?;
        promise.spawn(PromiseFuture::new(future));
        self.data_mut().read_op = Some(Heap::from(promise));
        Ok(())
    }

    /// Process the result of a read of the connection, whose reader is `reader`.
    fn received(
        &self,
        scope: &Scope<'_>,
        reader: ConnectionReader,
        result: Result<Option<Vec<u8>>, NetError>,
    ) -> Result<(), ExnThrown> {
        let mut events = Vec::new();
        {
            let mut data = self.data_mut();
            let Some(session) = data.link.session.as_mut() else {
                return Ok(());
            };
            match &result {
                Ok(Some(bytes)) => session.receive(bytes, &mut events),
                Ok(None) => session.receive_end(&mut events),
                Err(_) => events.push(session::Event::Closed { clean: false }),
            }
            if let Ok(Some(_)) = result {
                if !session.ended() {
                    data.link.reader = Some(reader);
                }
            }
        }
        self.flush(scope)?;
        self.start_read(scope)?;
        self.end_sending_after_closing_handshake(scope)?;
        self.handle_events(scope, events)
    }

    /// End the sending side of the connection once the closing handshake completed and no write
    /// is in progress, so that the server closes the connection.
    fn end_sending_after_closing_handshake(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let finished = {
            let mut data = self.data_mut();
            let link = &mut data.link;
            let completed = link
                .session
                .as_ref()
                .is_some_and(Session::close_handshake_completed);
            match completed {
                true => link
                    .writer
                    .take()
                    .map(|writer| (writer, std::mem::take(&mut link.unsent))),
                false => None,
            }
        };
        match finished {
            Some((writer, unsent)) => finish_writing(scope, writer, unsent),
            None => Ok(()),
        }
    }

    /// Hand the bytes the session produced to the connection: the bytes it takes at once, and
    /// the rest in a write that runs until the connection took them. Does nothing while a write
    /// is in progress, whose completion calls this again.
    fn flush(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let mut full = false;
        let pending = {
            let mut data = self.data_mut();
            let link = &mut data.link;
            if let Some(outgoing) = link.session.as_mut().map(Session::take_outgoing) {
                full = !queue_bytes(link, outgoing);
            }
            match link.writer.as_mut() {
                Some(writer) if !link.unsent.is_empty() => {
                    let bytes = std::mem::take(&mut link.unsent);
                    let length = bytes.len();
                    let unwritten = writer.try_write(bytes);
                    link.sent += (length - unwritten.len()) as u64;
                    match unwritten.is_empty() {
                        true => None,
                        false => link.writer.take().map(|writer| (writer, unwritten)),
                    }
                }
                _ => None,
            }
        };
        if let Some((mut writer, bytes)) = pending {
            let socket = RootedHeap::new(*self);
            let length = bytes.len() as u64;
            let future = async move {
                let result = writer.write(bytes).await;
                PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
                    let socket = socket.get(scope);
                    socket.data_mut().write_op = None;
                    socket.written(scope, writer, length, result)?;
                    Ok(HandleValue::undefined())
                }))
            };
            let promise = Promise::new_pending(scope)?;
            promise.spawn(PromiseFuture::new(future));
            self.data_mut().write_op = Some(Heap::from(promise));
        }
        self.queue_update()?;
        if full {
            return self.fail_full(scope);
        }
        Ok(())
    }

    /// Process the completion of a write of `length` bytes to the connection, whose writer is
    /// `writer`.
    fn written(
        &self,
        scope: &Scope<'_>,
        writer: ConnectionWriter,
        length: u64,
        result: Result<(), NetError>,
    ) -> Result<(), ExnThrown> {
        if result.is_err() {
            let event = session::Event::Closed { clean: false };
            return self.handle_events(scope, vec![event]);
        }
        let ended = {
            let mut data = self.data_mut();
            data.link.sent += length;
            match data.link.session.is_some() {
                true => {
                    data.link.writer = Some(writer);
                    None
                }
                false => Some((writer, std::mem::take(&mut data.link.unsent))),
            }
        };
        match ended {
            Some((writer, unsent)) => finish_writing(scope, writer, unsent),
            None => {
                self.flush(scope)?;
                self.end_sending_after_closing_handshake(scope)
            }
        }
    }

    /// Dispatch the events of `events`, which happened in that order.
    fn handle_events(
        &self,
        scope: &Scope<'_>,
        events: Vec<session::Event>,
    ) -> Result<(), ExnThrown> {
        for event in events {
            match event {
                // <https://websockets.spec.whatwg.org/#feedback-from-the-protocol>: When the
                //     WebSocket connection is established, the user agent must queue a task to
                //     run these steps:
                session::Event::Open { protocol } => {
                    if self.data().ready_state != WebSocketImpl::CONNECTING {
                        continue;
                    }
                    // Step 1: Change the `ready state` to `OPEN` (1).
                    // Step 2: Change the ``extensions`` attribute's value to the `extensions in
                    //     use`, if it is not the null value.
                    // Step 3: Change the ``protocol`` attribute's value to the `subprotocol in
                    //     use`, if it is not the null value.
                    {
                        let mut data = self.data_mut();
                        data.ready_state = WebSocketImpl::OPEN;
                        data.protocol = protocol;
                    }
                    // Step 4: `Fire an event` named ``open`` at the ``WebSocket`` object.
                    self.fire(scope, "open")?;
                }
                // When a WebSocket message has been received with type _type_ and data _data_,
                //     the user agent must queue a task to follow these steps:
                session::Event::Text(text) => {
                    // Step 1: If `ready state` is not `OPEN` (1), then return.
                    if self.data().ready_state != WebSocketImpl::OPEN {
                        continue;
                    }
                    // Step 2: Let _dataForEvent_ be determined by switching on _type_ and
                    //     ``binaryType``:
                    //   - _type_ indicates that the data is Text: a new ``DOMString`` containing
                    //     _data_
                    let data = text.as_str().to_jsval_throwing(scope)?;
                    self.fire_message(scope, data)?;
                }
                session::Event::Binary(bytes) => {
                    if self.data().ready_state != WebSocketImpl::OPEN {
                        continue;
                    }
                    //   - _type_ indicates that the data is Binary and ``binaryType`` is
                    //     "`blob`": a new ``Blob`` object, created in the `relevant Realm` of
                    //     the ``WebSocket`` object, that represents _data_ as its raw data
                    //   - _type_ indicates that the data is Binary and ``binaryType`` is
                    //     "`arraybuffer`": a new ``ArrayBuffer`` object, created in the
                    //     `relevant Realm` of the ``WebSocket`` object, whose contents are
                    //     _data_
                    let arraybuffer = self.data().binary_arraybuffer;
                    let data = match arraybuffer {
                        true => {
                            let buffer = web_fetch::incoming_body::array_buffer_from_body_bytes(
                                scope, bytes,
                            )?;
                            scope.root_value(buffer.as_value())
                        }
                        false => {
                            let blob = Blob::create(scope, bytes, String::new())?;
                            scope.root_value(blob.as_value())
                        }
                    };
                    self.fire_message(scope, data)?;
                }
                // When the WebSocket closing handshake is started, the user agent must queue a
                //     task to change the `ready state` to `CLOSING` (2). (If the ``close()``
                //     method was called, the `ready state` will already be set to `CLOSING` (2)
                //     when this task runs.)
                session::Event::CloseFrame { code, reason } => {
                    let mut data = self.data_mut();
                    data.close_frame = Some((code, reason));
                    if data.ready_state == WebSocketImpl::OPEN {
                        data.ready_state = WebSocketImpl::CLOSING;
                    }
                }
                // A connection that ended without the closing handshake is failed, as in Node.js.
                session::Event::Closed { clean } => {
                    self.end_link(scope);
                    self.closed(scope, !clean, clean)?;
                }
                session::Event::Failed(_) => {
                    self.end_link(scope);
                    self.closed(scope, true, false)?;
                }
            }
        }
        Ok(())
    }

    /// <https://websockets.spec.whatwg.org/#feedback-from-the-protocol>: When the WebSocket
    /// connection is closed, possibly cleanly, the user agent must queue a task to run the
    /// following substeps. `failed` is whether the connection was failed.
    fn closed(&self, scope: &Scope<'_>, failed: bool, clean: bool) -> Result<(), ExnThrown> {
        if self.data().ready_state == WebSocketImpl::CLOSED {
            return Ok(());
        }
        // Step 1: Change the `ready state` to `CLOSED` (3).
        let (code, reason) = {
            let mut data = self.data_mut();
            data.ready_state = WebSocketImpl::CLOSED;
            match &data.close_frame {
                Some((code, reason)) if !failed => (*code, reason.clone()),
                _ => (1006, String::new()),
            }
        };
        // Step 2: If the user agent was required to `fail the WebSocket connection`, or if the
        //     WebSocket connection was closed after being `flagged as full`, `fire an event`
        //     named ``error`` at the ``WebSocket`` object.
        if failed {
            self.fire(scope, "error")?;
        }
        // Step 3: `Fire an event` named ``close`` at the ``WebSocket`` object, using
        //     ``CloseEvent``, with the ``wasClean`` attribute initialized to true if the
        //     connection closed cleanly and false otherwise, the ``code`` attribute initialized
        //     to the WebSocket connection close code, and the ``reason`` attribute initialized to
        //     the result of applying `UTF-8 decode without BOM` to the WebSocket connection close
        //     reason.
        let event = CloseEvent::new_trusted(scope, clean, code, reason)?;
        self.dispatch_trusted(scope, *event, ScriptStackState::Empty);
        Ok(())
    }

    /// End the connection: cancel the read in progress, and send the bytes the session hasn't sent
    /// before ending the sending side, unless a write is in progress, whose completion does that.
    fn end_link(&self, scope: &Scope<'_>) {
        let read = self.data_mut().read_op.take_rooted(scope);
        if let Some(read) = read {
            js::promise::cancel_pending_future(read);
        }
        let finished = {
            let mut data = self.data_mut();
            let link = &mut data.link;
            if let Some(mut session) = link.session.take() {
                session.close_tls();
                let outgoing = session.take_outgoing();
                queue_bytes(link, outgoing);
            }
            link.reader = None;
            match link.writer.take() {
                Some(writer) => Some((writer, std::mem::take(&mut link.unsent))),
                None => None,
            }
        };
        if let Some((writer, unsent)) = finished {
            let _ = finish_writing(scope, writer, unsent);
        }
    }

    /// Cancel the connect in progress.
    fn cancel_connect(&self, scope: &Scope<'_>) {
        let connect = self.data_mut().connect_op.take_rooted(scope);
        if let Some(connect) = connect {
            js::promise::cancel_pending_future(connect);
        }
    }

    /// Fail the WebSocket connection after the data of a message couldn't be buffered.
    fn fail_full(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let mut events = Vec::new();
        if let Some(session) = self.data_mut().link.session.as_mut() {
            session.fail("out of memory".to_string(), Some(1009), &mut events);
        }
        self.handle_events(scope, events)
    }

    /// Fire a trusted event named `event_type` at the socket.
    fn fire(&self, scope: &Scope<'_>, event_type: &str) -> Result<(), ExnThrown> {
        let event = Event::new_trusted(scope, event_type.to_string(), None)?;
        self.dispatch_trusted(scope, event, ScriptStackState::Empty);
        Ok(())
    }

    /// Fire a `message` event with `data` at the socket.
    fn fire_message(&self, scope: &Scope<'_>, data: HandleValue<'_>) -> Result<(), ExnThrown> {
        // Step 3: `Fire an event` named ``message`` at the ``WebSocket`` object, using
        //     ``MessageEvent``, with the ``origin`` attribute initialized to the `serialization`
        //     of the ``WebSocket`` object's `url`'s `origin`, and the ``data`` attribute
        //     initialized to _dataForEvent_.
        let event = MessageEvent::new_trusted(scope, "message", data, &[])?;
        event.set_origin(&self.data().origin);
        self.dispatch_trusted(scope, *event, ScriptStackState::Empty);
        Ok(())
    }

    /// Queue a task that updates `bufferedAmount`, unless one is queued.
    fn queue_update(&self) -> Result<(), ExnThrown> {
        if self.data().update_queued || self.data().link.messages.is_empty() {
            return Ok(());
        }
        self.data_mut().update_queued = true;
        self.queue_task(TaskKind::UpdateBufferedAmount)
    }

    /// Queue a task of `kind` for the socket.
    fn queue_task(&self, kind: TaskKind) -> Result<(), ExnThrown> {
        let task = SocketTask {
            socket: Heap::from(*self),
            kind,
        };
        with_active_event_loop(|event_loop| event_loop.queue_ready(Box::new(task)));
        Ok(())
    }

    /// Decrease `bufferedAmount` by the length of the data of the messages whose frames were
    /// sent completely.
    fn update_buffered_amount(&self) {
        let mut data = self.data_mut();
        data.update_queued = false;
        let mut drained = 0.0;
        let sent = data.link.sent;
        while let Some(&(end, length)) = data.link.messages.front() {
            if end > sent {
                break;
            }
            drained += length as f64;
            data.link.messages.pop_front();
        }
        data.buffered_amount -= drained;
    }
}

/// What a [`SocketTask`] does.
enum TaskKind {
    /// Update `bufferedAmount`.
    UpdateBufferedAmount,
    /// Run the steps of a failed connection that closed.
    Failed,
}

/// A task of a socket.
#[js::allow_unrooted_interior]
struct SocketTask {
    socket: Heap<WebSocketImpl>,
    kind: TaskKind,
}

impl Task for SocketTask {
    fn kind(&self) -> &'static str {
        "websocket"
    }

    fn run(self: Box<Self>, scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
        let socket: WebSocket<'_> = self.socket.take(scope);
        match self.kind {
            TaskKind::UpdateBufferedAmount => {
                socket.update_buffered_amount();
                Ok(())
            }
            TaskKind::Failed => socket.closed(scope, true, false),
        }
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn trace(&self, trc: *mut JSTracer) {
        // SAFETY: `trc` is the tracer the event loop traces its tasks with.
        unsafe {
            self.socket.trace(trc);
        }
    }
}

/// Send `unsent` with `writer` and end the sending side, in a future that holds nothing else.
fn finish_writing(
    scope: &Scope<'_>,
    mut writer: ConnectionWriter,
    unsent: Vec<u8>,
) -> Result<(), ExnThrown> {
    let unsent = writer.try_write(unsent);
    let future = async move {
        if unsent.is_empty() || writer.write(unsent).await.is_ok() {
            let _ = writer.shutdown().await;
        }
        PromiseOutcome::Resolve(Box::new(|_scope: &Scope<'_>| Ok(HandleValue::undefined())))
    };
    let promise = Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    Ok(())
}

/// Append `bytes` to the bytes `link` hasn't sent. Returns `false` if allocating the space for
/// them failed, which drops them.
fn queue_bytes(link: &mut Link, bytes: Vec<u8>) -> bool {
    if bytes.is_empty() {
        return true;
    }
    link.produced += bytes.len() as u64;
    if link.unsent.is_empty() {
        link.unsent = bytes;
        return true;
    }
    if link.unsent.try_reserve(bytes.len()).is_err() {
        return false;
    }
    link.unsent.extend_from_slice(&bytes);
    true
}

/// A copy of `bytes`, or `None` if allocating it failed.
fn copy_bytes(bytes: &[u8]) -> Option<Bytes> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len()).ok()?;
    copy.extend_from_slice(bytes);
    Some(Bytes::from(copy))
}

/// A TCP connection to `port` on `host`, trying its addresses in turn.
async fn connect_to(host: &str, port: u16) -> Result<platform::net::Connection, String> {
    let addrs = platform::net::resolve(host)
        .await
        .map_err(|error| error.to_string())?;
    let mut last_error = String::new();
    for addr in addrs {
        match platform::net::connect_stream(std::net::SocketAddr::new(addr, port), None).await {
            Ok(connection) => return Ok(connection),
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(last_error)
}

/// <https://websockets.spec.whatwg.org/#get-a-url-record>: the URL record of `url`, or why it
/// fails.
fn get_url_record(url: &str, base: Option<&Url>) -> Result<Url, &'static str> {
    // Step 1: Let _urlRecord_ be the result of applying the `URL parser` to _url_ with
    //     _baseURL_.
    // Step 2: If _urlRecord_ is failure, then throw a "``SyntaxError``" ``DOMException``.
    let mut url_record = Url::options()
        .base_url(base)
        .parse(url)
        .map_err(|_| "The URL is invalid")?;
    // Step 3: If _urlRecord_'s `scheme` is "`http`", then set _urlRecord_'s `scheme` to "`ws`".
    // Step 4: Otherwise, if _urlRecord_'s `scheme` is "`https`", set _urlRecord_'s `scheme` to
    //     "`wss`".
    let scheme = match url_record.scheme() {
        "http" => Some("ws"),
        "https" => Some("wss"),
        _ => None,
    };
    if let Some(scheme) = scheme {
        let _ = url_record.set_scheme(scheme);
    }
    // Step 5: If _urlRecord_'s `scheme` is not "`ws`" or "`wss`", then throw a
    //     "``SyntaxError``" ``DOMException``.
    if !matches!(url_record.scheme(), "ws" | "wss") {
        return Err("The URL's scheme must be ws, wss, http or https");
    }
    // Step 6: If _urlRecord_'s `fragment` is non-null, then throw a "``SyntaxError``"
    //     ``DOMException``.
    if url_record.fragment().is_some() {
        return Err("The URL must not have a fragment");
    }
    // Step 7: Return _urlRecord_.
    Ok(url_record)
}

/// The subprotocols and the further headers of the opening handshake that the constructor's
/// `protocols` argument gives. The headers are only given in Node.js's options object.
fn convert_protocols(
    scope: &Scope<'_>,
    protocols: Option<HandleValue<'_>>,
) -> Result<(Vec<String>, web_fetch::headers::HeaderList), ExnThrown> {
    let Some(protocols) = protocols.filter(|value| !value.is_undefined()) else {
        return Ok((Vec::new(), Vec::new()));
    };
    // Node.js's second argument is `(DOMString or sequence<DOMString> or WebSocketInit)`, which
    // takes an object that isn't iterable as the dictionary.
    let init = NODE_COMPAT.with(Cell::get)
        && protocols.is_object()
        && !js::conversion::is_iterable_value(scope, protocols)
            .map_err(|error| error.throw(scope))?;
    let (protocols, headers) = match init {
        false => (Protocols::from_jsval_throwing(scope, protocols, ())?, None),
        true => {
            let init = WebSocketInit::from_jsval_throwing(scope, protocols, ())?;
            (
                init.protocols.unwrap_or(Protocols::Sequence(Vec::new())),
                init.headers,
            )
        }
    };
    let protocols = match protocols {
        Protocols::Sequence(list) => list,
        Protocols::String(text) => vec![text],
    };
    let headers = match headers {
        Some(init) => {
            let headers =
                js::class::create_instance_with::<web_fetch::headers::HeadersImpl>(scope, |_| {
                    web_fetch::headers::HeadersImpl::from_list(
                        Vec::new(),
                        web_fetch::headers::Guard::None,
                    )
                })?;
            web_fetch::algorithms::fill_headers(scope, &headers, init)?;
            headers.list()
        }
        None => Vec::new(),
    };
    Ok((protocols, headers))
}

/// The `Authorization` header value of basic authentication with the username and password of
/// `url`, if it has either.
fn url_credentials(url: &Url) -> Option<String> {
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    let decode = |text: &str| percent_encoding::percent_decode_str(text).collect::<Vec<u8>>();
    let mut credentials = decode(url.username());
    credentials.push(b':');
    credentials.extend(decode(url.password().unwrap_or_default()));
    Some(format!(
        "Basic {}",
        web_globals::base64::encode(&credentials, false)
    ))
}

/// Whether `byte` is a `tchar` of RFC 9110, which the tokens of a `Sec-WebSocket-Protocol` field
/// consist of.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}
