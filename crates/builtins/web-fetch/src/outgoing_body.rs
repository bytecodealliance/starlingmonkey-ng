// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Handing a JS body stream to the host transport as an outgoing body.
//!
//! This implements `HTTP-network fetch` Step 8.5's "transmit _request_'s `body`"
//! (<https://fetch.spec.whatwg.org/#concept-http-network-fetch>), i.e.
//! `incrementally read a body` and the `incrementally-read loop`
//! (<https://fetch.spec.whatwg.org/#body-incrementally-read>): each chunk is
//! delivered to native steps (_processBodyChunk_ = send through the channel),
//! end-of-body closes the channel, and an error or non-`Uint8Array` chunk aborts
//! the body.
//!
//! A body whose stream came straight from the host and has never been read is
//! handed through untouched. Otherwise, the stream is pumped: chunks from an
//! internal reader are sent through a channel which the platform transport reads
//! as it sends the body.

use std::future::Future;

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Promise, Uint8Array};
use platform::http::{BodySender, IncomingBody, OutgoingBody};
use web_streams::readable::default_reader::DefaultReader;
use web_streams::readable::native_byte_source::take_untouched_reader;
use web_streams::readable::native_pump::{
    append_bytes, NativePump, PumpBatch, PumpEnd, PumpSink, PUMP_BATCH_BYTES,
};
use web_streams::readable::native_read::acquire_native_reader;
use web_streams::readable::readable_stream::ReadableStream;

use crate::incoming_body::HostBackedBodyOwner;

/// Counts of how outgoing bodies reached the transport, for tests.
///
/// Content cannot observe whether a host body was handed to the transport whole or pumped through
/// JS, or whether an in-memory body's owner released its buffer when handing it over.
pub mod paths_taken {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Bodies handed to the transport whole, host body and all.
    pub static SHORTCUT: AtomicUsize = AtomicUsize::new(0);
    /// Bodies pumped through JS, chunk by chunk.
    pub static PUMPED: AtomicUsize = AtomicUsize::new(0);
    /// Byte bodies handed over as the only reference left to their buffer.
    pub static SOLE_BYTES: AtomicUsize = AtomicUsize::new(0);
    /// Byte bodies whose owner still held a reference of its own when it handed them over.
    pub static SHARED_BYTES: AtomicUsize = AtomicUsize::new(0);

    /// `(shortcut, pumped)` since the last [`reset`].
    pub fn counts() -> (usize, usize) {
        (
            SHORTCUT.load(Ordering::Relaxed),
            PUMPED.load(Ordering::Relaxed),
        )
    }

    /// `(sole, shared)` since the last [`reset`], counting non-empty byte bodies only.
    pub fn byte_counts() -> (usize, usize) {
        (
            SOLE_BYTES.load(Ordering::Relaxed),
            SHARED_BYTES.load(Ordering::Relaxed),
        )
    }

    /// Start counting again. Callers that compare counts must not run concurrently with other
    /// traffic on the same process.
    pub fn reset() {
        SHORTCUT.store(0, Ordering::Relaxed);
        PUMPED.store(0, Ordering::Relaxed);
        SOLE_BYTES.store(0, Ordering::Relaxed);
        SHARED_BYTES.store(0, Ordering::Relaxed);
    }

    pub(super) fn note(body: &platform::http::OutgoingBody) {
        match body {
            platform::http::OutgoingBody::Host(_) => &SHORTCUT,
            platform::http::OutgoingBody::Stream(_) => &PUMPED,
            // Empty ones sit out: there is no buffer to hold on to, and `Bytes::is_unique` returns
            // `false` for the static buffer of an empty body.
            platform::http::OutgoingBody::Bytes(bytes) if !bytes.is_empty() => {
                if bytes.is_unique() {
                    &SOLE_BYTES
                } else {
                    &SHARED_BYTES
                }
            }
            // Neither route, and nothing held: an empty body or none at all.
            _ => return,
        }
        .fetch_add(1, Ordering::Relaxed);
    }
}

/// The body to send on the wire for a host-backed owner.
///
/// With `start_reading` false a `ReadableStream` body is canceled instead of being read, which
/// would call its underlying source's `pull()` method.
pub(crate) fn consume_outgoing_body(
    scope: &Scope<'_>,
    object: &impl HostBackedBodyOwner,
    start_reading: bool,
) -> OutgoingBody {
    let body = outgoing_body_inner(scope, object, start_reading);
    paths_taken::note(&body);
    body
}

fn outgoing_body_inner(
    scope: &Scope<'_>,
    object: &impl HostBackedBodyOwner,
    start_reading: bool,
) -> OutgoingBody {
    if let Some(bytes) = object.take_byte_source() {
        return OutgoingBody::Bytes(bytes);
    }
    if let Some(host_body) = object.take_host_body() {
        return OutgoingBody::Host(host_body);
    }
    match object.body_stream(scope) {
        Some(stream) => {
            let reader = object.take_send_reader(scope);
            outgoing_body_from_stream(scope, stream, reader, start_reading)
        }
        None => OutgoingBody::Bytes(bytes::Bytes::new()),
    }
}

/// Turn a body's materialized `ReadableStream` into a [`OutgoingBody`].
///
/// A host body that has never been read is handed straight through; anything
/// else is pumped through a `DefaultReader`, or cancelled unsent if
/// `start_reading` is false. The pump reads through `reader` when the caller
/// already locked the stream with one, and acquires its own otherwise.
///
/// By this point the request/response is committed to being sent, so a failure
/// to start reading cannot be thrown to the caller. It becomes a body that
/// fails when the transport reads it, which fails the send.
pub(crate) fn outgoing_body_from_stream(
    scope: &Scope<'_>,
    stream: ReadableStream<'_>,
    reader: Option<DefaultReader<'_>>,
    start_reading: bool,
) -> OutgoingBody {
    // The shortcut refuses once the body has actually been read from: a chunk may sit in a
    // buffer the shortcut cannot see (a transform's queue), and bypassing the stream would
    // drop those bytes. The pump drains the stream in order instead. A pipe that has merely
    // _asked_ for a chunk does not count: that pull is deferred without touching the host body
    // (see `NativeByteSource`). Taking the host body leaves the stream locked and disturbed, as
    // the pump would: author code can no longer read it and `bodyUsed` reports true.
    let host_body = take_untouched_reader::<IncomingBody>(scope, &stream);
    match host_body {
        Some(host_body) => OutgoingBody::Host(host_body),
        // Cancelled rather than left alone: nothing will read this stream now, and cancelling is
        // what makes it release whatever it draws from — an upstream response feeding a transform,
        // say. It runs the handler's `cancel()`, not its `pull()`, so no content is produced.
        None if !start_reading => {
            let promise = stream.cancel_internal(scope, HandleValue::undefined());
            // Rejects if `cancel()` throws. The response carries no body either way and there is no
            // caller left to tell, so mark it handled rather than announcing an unhandled rejection.
            let _ = promise.set_any_is_handled(scope);
            OutgoingBody::Consumed
        }
        None => pump_body_from_stream(scope, stream, reader).unwrap_or_else(|_| {
            // The pump could not be started, so the exception it left pending has no caller to
            // propagate to. Clear it rather than leaving it to surface at an unrelated point,
            // and let the failing body carry the failure instead.
            js::exception::clear(scope);
            platform::http::failed_body("the request body stream could not be read".to_string())
        }),
    }
}

/// Start pumping `stream` into a new streaming [`OutgoingBody`]: returns the body
/// (handed to the platform transport) and kicks off the read loop.
fn pump_body_from_stream(
    scope: &Scope<'_>,
    stream: ReadableStream<'_>,
    reader: Option<DefaultReader<'_>>,
) -> Result<OutgoingBody, ExnThrown> {
    let (sender, body) = platform::http::body_channel();
    let reader = match reader {
        Some(reader) => reader,
        None => acquire_native_reader(scope, &stream)?,
    };
    let pump = NativePump::create(scope, reader, BodySink(sender), BodyBatch::default())?;
    pump.start(scope);
    Ok(body)
}

/// The bytes of the chunks read since the last send.
#[derive(Default)]
struct BodyBatch(Vec<u8>);

impl PumpBatch for BodyBatch {
    /// Append a `Uint8Array` chunk's bytes. Any other chunk ends the body with an error, since the
    /// spec's transmit-body chunk steps treat a non-`Uint8Array` chunk as a fatal error.
    fn append(&mut self, scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<(), String> {
        let Ok(array) = Uint8Array::from_jsval(scope, chunk, ()) else {
            return Err("Body stream chunks must be of type Uint8Array".to_string());
        };
        // The spec's "a copy of chunk": the chunk's bytes are copied once, into the batch.
        array
            .with_slice(|bytes| append_bytes(&mut self.0, bytes))
            .map_err(|_| "The body chunk could not be allocated".to_string())
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn is_full(&self) -> bool {
        self.0.len() >= PUMP_BATCH_BYTES
    }

    fn take(&mut self) -> Self {
        BodyBatch(std::mem::take(&mut self.0))
    }
}

/// The write end of the channel from which the platform transport reads the body. A batch that the
/// channel doesn't accept right away waits for capacity, so the pump never outruns the peer.
struct BodySink(BodySender);

impl PumpSink for BodySink {
    type Batch = BodyBatch;

    fn try_write(&mut self, batch: BodyBatch) -> Result<(), BodyBatch> {
        self.0.try_send_chunk(batch.0).map_err(BodyBatch)
    }

    fn write(mut self, batch: BodyBatch) -> impl Future<Output = Option<Self>> + 'static {
        // The event loop is kept alive until the receiver accepts the batch.
        let interest =
            core_runtime::event_loop::with_active_event_loop(|el| el.acquire_interest_handle());
        async move {
            let accepted = self.0.send_chunk(batch.0).await;
            drop(interest);
            accepted.then_some(self)
        }
    }

    fn describe_source_error(_scope: &Scope<'_>, _error: HandleValue<'_>) -> String {
        "body stream errored".to_string()
    }

    /// End the body. A failed pump aborts it with the failure's message, which fails the send.
    /// Otherwise, dropping the sender closes the channel.
    fn finish(self, scope: &Scope<'_>, end: &PumpEnd) {
        if let PumpEnd::Failed(message) = end {
            spawn_send_error(scope, self.0, message.clone());
        }
    }
}

/// Abort the body with `message`, waiting for channel capacity so the error is queued after the
/// chunks already sent. The sender is dropped afterwards, which closes the channel.
fn spawn_send_error(scope: &Scope<'_>, mut sender: BodySender, message: String) {
    let Ok(promise) = Promise::new_pending(scope) else {
        js::exception::report_and_clear(scope, "sending a request or response body");
        sender.fail(message);
        return;
    };
    let _ = promise.set_any_is_handled(scope);
    // The event loop is kept alive until the receiver accepts the error.
    let interest =
        core_runtime::event_loop::with_active_event_loop(|el| el.acquire_interest_handle());
    let future = async move {
        sender.send_error(message).await;
        drop(interest);
        PromiseOutcome::Resolve(Box::new(|_scope: &Scope<'_>| Ok(HandleValue::undefined())))
    };
    promise.spawn(PromiseFuture::new(future));
}
