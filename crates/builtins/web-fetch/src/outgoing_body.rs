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

use core_runtime::jsclass;
use core_runtime::jsmethods;
use js::class::create_instance_with;
use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::handle::{Heap, RootedHeap};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Object, Promise, Uint8Array};
use platform::http::{BodySender, OutgoingBody};
use web_streams::readable::default_reader::{DefaultReader, DefaultReaderImpl};
use web_streams::readable::native_read::{
    acquire_native_reader, native_reader_read, NativeReadSteps,
};
use web_streams::readable::readable_stream::ReadableStream;

use crate::incoming_body::HostBackedBodyOwner;

/// A running tally of how outgoing bodies reached the transport.
///
/// Whether a host body is handed to the wire whole or pumped through JS chunk by chunk is
/// deliberately not observable from content — both deliver the same bytes, leave the donor stream
/// locked and disturbed, and read only internally. That also means losing the shortcut is invisible:
/// every byte would start travelling through JS and nothing would fail. These counters are the one
/// place that difference is visible, so a test can hold the choice in place.
///
/// Whether an in-memory body's owner let go of its buffer on the way out is invisible the same way:
/// the same bytes reach the wire either way, and only how long the buffer outlives them differs.
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
            // Empty ones sit out: there is no buffer to hold on to, and `Bytes::is_unique` answers
            // for the static one an empty body carries as though it were shared.
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

/// State for the body pump: the reader being drained, the channel's write end, and the bytes read
/// but not yet sent. The sender is taken out while a send waits for channel capacity, and dropped
/// (closing the body) when the stream ends or errors.
#[jsclass(hidden)]
pub struct OutgoingBodyPump {
    reader: Heap<DefaultReaderImpl>,
    #[no_trace]
    sender: Option<BodySender>,
    /// Bytes of the chunks read since the last send.
    #[no_trace]
    batch: Vec<u8>,
    /// How the stream ended, once its close or error steps ran, or a chunk was not a
    /// `Uint8Array`.
    #[no_trace]
    end: Option<StreamEnd>,
    /// Whether a read request is waiting for a chunk.
    #[no_trace]
    reading: bool,
    /// Whether [`run`] is on the stack.
    #[no_trace]
    running: bool,
}

/// How a pumped stream ended.
enum StreamEnd {
    Closed,
    Errored(String),
}

#[jsmethods]
impl OutgoingBodyPump {
    fn new() -> Self {
        // Internal type, always created with a real reader/sender via `create_instance_with`; this
        // hidden constructor exists only so the prototype is registered.
        OutgoingBodyPumpImpl {
            reader: Heap::default(),
            sender: None,
            batch: Vec::new(),
            end: None,
            reading: false,
            running: false,
        }
    }
}

/// Turn a body's materialized `ReadableStream` into a [`OutgoingBody`].
///
/// A host body that has never been read is handed straight through; anything
/// else is pumped through a `DefaultReader`, or cancelled unsent where
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
    // (see `incoming_body::host_pull`).
    let host_body = stream
        .native_source(scope)
        .and_then(|source| source.cast::<crate::incoming_body::HostBodySource>().ok())
        .and_then(|source| source.take_host_body(scope));
    match host_body {
        Some(host_body) => {
            // The bytes are transmitted directly, bypassing the stream, so leave the donor
            // stream in the same locked + disturbed state the pump path (acquiring a reader)
            // would: author code can no longer read it and `bodyUsed` reports true.
            stream
                .lock_and_disturb(scope)
                .expect("If this happens, there's a bug in the runtime");
            OutgoingBody::Host(host_body)
        }
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
    let state = create_instance_with::<OutgoingBodyPumpImpl>(scope, |_| OutgoingBodyPumpImpl {
        reader: Heap::from(reader),
        sender: Some(sender),
        batch: Vec::new(),
        end: None,
        reading: false,
        running: false,
    })?;
    run(scope, &state);
    Ok(body)
}

/// The most bytes the pump reads into one batch before sending it.
const BATCH_LIMIT: usize = 64 * 1024;

/// The pump's native read-request steps. Each records what it received in the pump's state and
/// then calls [`run`], which does nothing when the step ran from inside it.
const PUMP_STEPS: NativeReadSteps = NativeReadSteps {
    chunk: pump_chunk_step,
    close: pump_close_step,
    error: pump_error_step,
};

/// Advance the pump. Reads chunks into the batch for as long as reads complete synchronously and
/// the batch is below [`BATCH_LIMIT`], then sends the batch once a read has to wait, the batch is
/// full, or the stream has ended. A batch the channel accepts right away is followed by more
/// reads. Otherwise the pump waits until the channel accepts it (`spawn_send` resumes it), so it
/// never outruns the peer. At most one read or send is outstanding at a time.
fn run(scope: &Scope<'_>, state: &OutgoingBodyPump<'_>) {
    if state.data().running {
        return;
    }
    state.data_mut().running = true;
    loop {
        while can_read(state) {
            state.data_mut().reading = true;
            if read_next(scope, state).is_err() {
                js::exception::report_and_clear(scope, "reading a request or response body");
                state.data_mut().reading = false;
                fail_body(state, "the body stream could not be read");
            }
            if state.data().reading {
                break;
            }
        }
        if !send_batch(scope, state) {
            break;
        }
    }
    state.data_mut().running = false;
}

/// Whether the pump may issue another read: the channel is not waiting for a send, the stream has
/// not ended, no read is outstanding, and the batch has room.
fn can_read(state: &OutgoingBodyPump<'_>) -> bool {
    let data = state.data();
    data.sender.is_some() && data.end.is_none() && !data.reading && data.batch.len() < BATCH_LIMIT
}

/// Send the batch, or end the body, if there is reason to. Returns whether [`run`] should continue
/// reading: true if the channel accepted the batch right away.
fn send_batch(scope: &Scope<'_>, state: &OutgoingBodyPump<'_>) -> bool {
    let mut data = state.data_mut();
    if data.sender.is_none() {
        // A send is waiting for capacity, or the body is done.
        return false;
    }
    let ready = data.reading || data.end.is_some() || data.batch.len() >= BATCH_LIMIT;
    if !data.batch.is_empty() && ready {
        let batch = std::mem::take(&mut data.batch);
        let sent = data
            .sender
            .as_mut()
            .expect("checked above")
            .try_send_chunk(batch);
        drop(data);
        return match sent {
            Ok(()) => true,
            Err(batch) => {
                spawn_send(scope, state, Send::Chunk(batch));
                false
            }
        };
    }
    if data.batch.is_empty() {
        match data.end.take() {
            // End of body: dropping the sender closes the channel.
            Some(StreamEnd::Closed) => data.sender = None,
            Some(StreamEnd::Errored(message)) => {
                drop(data);
                spawn_send(scope, state, Send::Error(message));
            }
            None => {}
        }
    }
    false
}

/// Issue the next internal read, delivering to the pump's steps.
fn read_next(scope: &Scope<'_>, state: &OutgoingBodyPump<'_>) -> Result<(), ExnThrown> {
    let reader = state.data().reader.get(scope);
    native_reader_read(scope, reader, PUMP_STEPS, state.as_object())
}

/// A pending send to the body channel: a batch of bytes (after which the pump reads on) or a
/// terminal error (after which the pump stops).
enum Send {
    Chunk(Vec<u8>),
    Error(String),
}

/// Chunk steps: append a `Uint8Array` chunk's bytes to the batch. Anything else ends the body with
/// an error, since the spec's transmit-body chunk steps treat a non-`Uint8Array` chunk as a fatal
/// error.
fn pump_chunk_step(
    scope: &Scope<'_>,
    payload: Object<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = payload
        .cast::<OutgoingBodyPump>()
        .expect("payload is an OutgoingBodyPump");
    let mut data = state.data_mut();
    data.reading = false;
    match Uint8Array::from_jsval(scope, chunk, ()) {
        // The spec's "a copy of chunk": the chunk's bytes are copied once, into the batch.
        Ok(array) => {
            let batch = &mut data.batch;
            array.as_array_buffer_view().with_bytes(|bytes| {
                // Appending to a batch reserves room up to the limit, so the batch is copied once
                // rather than through every doubling of its capacity.
                if !batch.is_empty() && batch.capacity() - batch.len() < bytes.len() {
                    let wanted = BATCH_LIMIT.max(batch.len() + bytes.len()) - batch.len();
                    batch.reserve_exact(wanted);
                }
                batch.extend_from_slice(bytes);
            });
        }
        Err(_) => {
            data.end = Some(StreamEnd::Errored(
                "Body stream chunks must be of type Uint8Array".to_string(),
            ))
        }
    }
    drop(data);
    run(scope, &state);
    Ok(())
}

/// Close steps: end of body, after the batch is sent.
fn pump_close_step(scope: &Scope<'_>, payload: Object<'_>) -> Result<(), ExnThrown> {
    let state = payload
        .cast::<OutgoingBodyPump>()
        .expect("payload is an OutgoingBodyPump");
    let mut data = state.data_mut();
    data.reading = false;
    data.end = Some(StreamEnd::Closed);
    drop(data);
    run(scope, &state);
    Ok(())
}

/// Error steps: abort the body with the stream's error, after the batch is sent.
fn pump_error_step(
    scope: &Scope<'_>,
    payload: Object<'_>,
    _error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = payload
        .cast::<OutgoingBodyPump>()
        .expect("payload is an OutgoingBodyPump");
    let mut data = state.data_mut();
    data.reading = false;
    data.end = Some(StreamEnd::Errored("body stream errored".to_string()));
    drop(data);
    run(scope, &state);
    Ok(())
}

/// End the pump's body with an error, so the transport fails the send instead of treating the
/// body as complete.
fn fail_body(state: &OutgoingBodyPump<'_>, message: &str) {
    if let Some(sender) = state.data_mut().sender.take() {
        sender.fail(message.to_string());
    }
}

/// Send `item` to the body channel (taking the sender out of the pump), awaiting
/// channel capacity for backpressure. After a batch is accepted, the pump runs again.
/// After an error, the pump stops and the sender is dropped (closing the channel after
/// the queued error).
fn spawn_send(scope: &Scope<'_>, state: &OutgoingBodyPump<'_>, item: Send) {
    if state.data().sender.is_none() {
        return;
    }
    let Ok(promise) = Promise::new_pending(scope) else {
        js::exception::report_and_clear(scope, "sending a request or response body");
        fail_body(state, "the body could not be sent");
        return;
    };
    let Some(sender) = state.data_mut().sender.take() else {
        return;
    };
    let _ = promise.set_any_is_handled(scope);
    let state = RootedHeap::new(*state);
    let is_chunk = matches!(item, Send::Chunk(_));
    // Ensure the event loop stays alive until the receiver accepts the chunk or error.
    let interest =
        core_runtime::event_loop::with_active_event_loop(|el| el.acquire_interest_handle());
    let future = async move {
        let mut sender = sender;
        let accepted = match item {
            Send::Chunk(bytes) => sender.send_chunk(bytes).await,
            Send::Error(message) => {
                sender.send_error(message).await;
                false
            }
        };
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            drop(interest);
            if is_chunk {
                let state = state.get(scope);
                if accepted {
                    state.data_mut().sender = Some(sender);
                    run(scope, &state);
                } else {
                    // A refused chunk means the receiver is gone: the peer stopped reading the
                    // body, typically because the client disconnected. Cancel the stream, and
                    // transitively the underlying source, so it can stop producing chunks.
                    let reader = state.data().reader.get(scope);
                    match reader.cancel(scope, None) {
                        // Cancelling rejects if the underlying source's `cancel()` throws.
                        // There is no caller left to report that to: the body is already
                        // undeliverable. Mark it handled so it is not announced as an unhandled
                        // rejection on top.
                        Ok(promise) => {
                            let _ = promise.set_any_is_handled(scope);
                        }
                        Err(_) => js::exception::report_and_clear(
                            scope,
                            "cancelling an abandoned response body",
                        ),
                    }
                }
            }
            Ok(HandleValue::undefined())
        }))
    };
    promise.spawn(PromiseFuture::new(future));
}
