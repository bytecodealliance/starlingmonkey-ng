// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Async HTTP transport.
//!
//! `send` returns once the response status and headers are available; the body
//! is a [`IncomingBody`] read lazily from the host (reqwest on native, the
//! WASIp3 incoming body stream on wasm). Callers either drain it with
//! [`IncomingBody::read_all`] or read it piece by piece with
//! [`IncomingBody::read_into`]. Natively, `IncomingBody::next_chunk` also returns
//! each chunk as it was received.
//!
//! This module holds the platform-agnostic parts; the backends live in
//! [`http_native`](crate::http_native) and [`http_wasm`](crate::http_wasm),
//! whose platform-specific items are re-exported here.

#[cfg(not(target_arch = "wasm32"))]
pub use crate::http_native::{incoming_body_channel, send, IncomingBody, IncomingBodySender};
#[cfg(target_arch = "wasm32")]
pub use crate::http_wasm::{
    build_outgoing_response, read_incoming_request, send, AbandonBody, BodyDone, BodySendOutcome,
    IncomingBody,
};

/// An outgoing HTTP request.
pub struct Request {
    /// The HTTP method (e.g. `GET`, `POST`).
    pub method: String,
    /// The absolute request URL.
    pub url: String,
    /// Header (name, value) pairs, in order. Values are `ByteString`s, which are
    /// [`isomorphic_encode`]d when sent.
    pub headers: Vec<(String, String)>,
    /// The request body.
    pub body: OutgoingBody,
}

/// A body chunk a [`Remaining`] budget can trim.
pub trait BodyChunk {
    /// The chunk's length in bytes.
    fn byte_len(&self) -> usize;
    /// Shorten the chunk to its first `len` bytes.
    fn truncate_to(&mut self, len: usize);
}

impl BodyChunk for bytes::Bytes {
    fn byte_len(&self) -> usize {
        self.len()
    }

    fn truncate_to(&mut self, len: usize) {
        self.truncate(len);
    }
}

impl BodyChunk for Vec<u8> {
    fn byte_len(&self) -> usize {
        self.len()
    }

    fn truncate_to(&mut self, len: usize) {
        self.truncate(len);
    }
}

/// The content a length-framed body may still send, held by whichever transport writes that body.
/// A response is framed by the `Content-Length` it declares, so content past that never goes on
/// the wire, and content short of it leaves the message unfinished.
///
/// `None` if no length was declared.
pub struct Remaining(Option<u64>);

impl Remaining {
    pub fn new(declared_length: Option<u64>) -> Self {
        Remaining(declared_length)
    }

    /// Trim `chunk` so that it doesn't cause the total body length to exceed the declared length.
    /// `None` once no bytes can be sent anymore at all.
    pub fn take<T: BodyChunk>(&mut self, mut chunk: T) -> Option<T> {
        let Some(remaining) = self.0.as_mut() else {
            return Some(chunk);
        };
        if *remaining == 0 {
            return None;
        }
        let taken = chunk
            .byte_len()
            .min(usize::try_from(*remaining).unwrap_or(usize::MAX));
        *remaining -= taken as u64;
        chunk.truncate_to(taken);
        Some(chunk)
    }

    /// Whether the body sent so far is shorter than the declared length.
    pub fn is_unfilled(&self) -> bool {
        self.0.is_some_and(|remaining| remaining > 0)
    }
}

/// The transport's end of a streamed outgoing body: the receiving half of [`body_channel`], named
/// so a transport can hold one without naming the channel crate itself.
pub type OutgoingBodyReceiver = futures_channel::mpsc::Receiver<Result<Vec<u8>, Error>>;

/// An outgoing request body: an in-memory byte sequence, a stream of chunks fed
/// from elsewhere (such as a JS `ReadableStream` pumped through a channel), or
/// a host response body handed straight through.
pub enum OutgoingBody {
    /// A complete byte sequence, empty for no body.
    Bytes(bytes::Bytes),
    /// Chunks delivered on a bounded channel. A final `Err` aborts the body, and
    /// closing the channel ends it. Created via [`body_channel`].
    Stream(OutgoingBodyReceiver),
    /// A host body sent unchanged, for an incoming body used directly as an outgoing one or
    /// piped through an identity `TransformStream`.
    Host(IncomingBody),
    /// No body to send, because none is left or none was ever produced: a stream
    /// body consumed by a previous send (a redirect cannot replay it), or one
    /// deliberately not produced because nobody would read it.
    Consumed,
}

/// How many chunks a body channel holds before the producer waits.
pub(crate) const BODY_CHANNEL_CAPACITY: usize = 8;

/// The size up to which [`coalesce_queued_chunks`] appends queued chunks.
const COALESCE_LIMIT: usize = 64 * 1024;

/// Append chunks already queued in `receiver` to `chunk` without waiting, so the transport can write
/// them in one call.
///
/// Appending stops at the first of:
/// - `chunk` reaching 64 KiB. The last appended chunk can take it past that.
/// - an empty queue or a closed channel. The caller's next read from `receiver` reports the close.
/// - a queued `Err`, which is returned. The body failed after the bytes now in `chunk`.
pub fn coalesce_queued_chunks(
    receiver: &mut OutgoingBodyReceiver,
    chunk: &mut Vec<u8>,
) -> Option<Error> {
    while chunk.len() < COALESCE_LIMIT {
        let next = match receiver.try_recv() {
            Ok(Ok(next)) => next,
            Ok(Err(error)) => return Some(error),
            Err(_) => break,
        };
        let len = chunk.len() + next.len();
        if len > chunk.capacity() {
            // Reserve up to the limit on the first reallocation, so later appends within the limit
            // don't copy `chunk` again.
            chunk.reserve_exact(len.max(COALESCE_LIMIT) - chunk.len());
        }
        chunk.extend_from_slice(&next);
    }
    None
}

/// Wait for channel capacity, then enqueue `item`. Returns `false` if the receiver was dropped.
pub(crate) async fn send_on<T>(sender: &mut futures_channel::mpsc::Sender<T>, item: T) -> bool {
    if futures_lite::future::poll_fn(|cx| sender.poll_ready(cx))
        .await
        .is_err()
    {
        return false;
    }
    sender.start_send(item).is_ok()
}

/// The write end of a streaming [`OutgoingBody`]: a producer (e.g. a JS
/// `ReadableStream`) sends body chunks here. The send awaits channel
/// capacity, so it paces the producer to the peer (backpressure). Dropping it
/// ends the body.
pub struct BodySender(futures_channel::mpsc::Sender<Result<Vec<u8>, Error>>);

impl BodySender {
    /// Append a body chunk, awaiting channel capacity. Returns `false` if the
    /// receiver was dropped.
    pub async fn send_chunk(&mut self, chunk: Vec<u8>) -> bool {
        send_on(&mut self.0, Ok(chunk)).await
    }

    /// Append a body chunk if the channel has capacity for it right away. Returns the chunk if it
    /// has no capacity, or if the receiver was dropped.
    pub fn try_send_chunk(&mut self, chunk: Vec<u8>) -> Result<(), Vec<u8>> {
        self.0
            .try_send(Ok(chunk))
            .map_err(|error| error.into_inner().expect("the chunk sent above is `Ok`"))
    }

    /// Abort the body with an error (so the send fails), awaiting capacity.
    pub async fn send_error(&mut self, message: String) -> bool {
        send_on(&mut self.0, Err(Error(message))).await
    }

    /// Abort the body with an error without waiting for capacity. If the channel is full, the
    /// error is not queued and the body ends after the chunks already queued.
    pub fn fail(mut self, message: String) {
        let _ = self.0.try_send(Err(Error(message)));
    }
}

/// Create a streaming request body and its write end. Send chunks through the
/// [`BodySender`] (which applies backpressure); drop it to end the body.
pub fn body_channel() -> (BodySender, OutgoingBody) {
    let (sender, receiver) = futures_channel::mpsc::channel(BODY_CHANNEL_CAPACITY);
    (BodySender(sender), OutgoingBody::Stream(receiver))
}

/// A body that fails as soon as the transport reads it, for a caller that is committed to
/// sending but cannot produce the bytes. An empty body instead would make the truncated request
/// look successful.
pub fn failed_body(message: String) -> OutgoingBody {
    let (mut sender, body) = futures_channel::mpsc::channel(1);
    // The channel is empty and has capacity, so this cannot fail.
    let _ = sender.try_send(Err(Error(message)));
    drop(sender);
    OutgoingBody::Stream(body)
}

/// An HTTP response: status and headers, and [`IncomingBody`] stream.
pub struct Response {
    /// The HTTP status code.
    pub status: u16,
    /// Response header (name, value) pairs, in order.
    ///
    /// The wire bytes [`isomorphic_decode`]d, so every code unit is ≤ 0xFF and
    /// stands for exactly one byte received.
    pub headers: Vec<(String, String)>,
    /// The response body stream.
    pub body: IncomingBody,
}

/// <https://infra.spec.whatwg.org/#isomorphic-encode>
///
/// Encode a header value to its wire bytes: one byte per code unit, not UTF-8.
///
/// A code unit above 0xFF cannot occur in a `ByteString` and is replaced by `?`
/// rather than truncated into an unrelated byte.
pub fn isomorphic_encode(value: &str) -> Vec<u8> {
    value
        .chars()
        .map(|c| u8::try_from(c as u32).unwrap_or(b'?'))
        .collect()
}

/// <https://infra.spec.whatwg.org/#isomorphic-decode>
///
/// Decode wire bytes to a header value: one code unit per byte. The inverse of
/// [`isomorphic_encode`].
pub fn isomorphic_decode(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// A transport error (DNS, connection, TLS, protocol, …).
#[derive(Debug, Clone)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// A read-only, refcounted byte sequence from a body.
pub type BodyBytes = bytes::Bytes;
