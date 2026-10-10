// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Read all bytes from a ReadableStream.
//!
//! <https://streams.spec.whatwg.org/#readablestreamdefaultreader-read-all-bytes>
//!
//! Drains a default reader to a single byte sequence using *internal* read
//! requests, then hands the bytes to a caller-supplied success callback. Fetch's
//! `fully read a body` is the sole consumer: it acquires an internal reader for a
//! body's stream and reads all the bytes, converting them to the requested value
//! (text, JSON, an `ArrayBuffer`, …) in the success callback.
//!
//! Reading via internal read requests — rather than the author-facing
//! `reader.read()` — is what makes this immune to a hijacked `Object.prototype.then`:
//! the chunk steps receive the chunk directly and accumulate its bytes, never
//! building an `{ value, done }` iterator-result object nor resolving a promise
//! with one, so the engine never performs thenable assimilation that would look up
//! and invoke an author-installed `then`. (The same property is why `pipeTo`
//! survives that attack.)

use super::algorithms::{
    acquire_readable_stream_default_reader, readable_stream_default_reader_read,
};
use super::default_reader::DefaultReaderImpl;
use super::read_request::ReadRequest;
use super::readable_stream::ReadableStream;
use crate::readable::DefaultReader;
use core_runtime::{jsclass, jsmethods};
use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::function::cast_payload;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleValue, OptionHeapExt};
use js::value;
use js::{Function, Promise, Uint8Array};

/// The completion of a read-all-bytes drain (the caller's `successSteps`):
/// receives the payload value given to [`read_all_bytes`] and the assembled
/// bytes **by value** — no intermediate JS `Uint8Array` round-trip — and
/// returns the value the result promise resolves with (or throws to reject
/// it).
pub type ReadAllBytesComplete =
    for<'r> fn(&'r Scope<'r>, HandleValue<'_>, Vec<u8>) -> Result<HandleValue<'r>, ExnThrown>;

/// State threaded through the read loop: the reader being drained, the bytes
/// accumulated so far, the promise to settle, and the native success callback
/// (plus its payload value) to run on the complete byte sequence once the
/// stream closes. The stream erroring rejects the result promise directly with
/// the stream's error.
#[jsclass(hidden)]
pub(crate) struct ReadAllBytesState {
    reader: Heap<DefaultReaderImpl>,
    promise: Heap<js::promise::Promise>,
    #[no_trace]
    on_complete: Option<ReadAllBytesComplete>,
    payload: Heap<Value>,
    #[no_trace]
    bytes: Vec<u8>,
    /// The deferred-read callback (payload = this state object), allocated in
    /// [`read_all_bytes`] right after the state is created (`Option` only
    /// because the callback's payload is the state object, which must exist
    /// first) and queued directly on the job queue per chunk, so the read
    /// loop allocates nothing per chunk.
    read_next_fn: Option<Heap<js::function::Function>>,
}

// Needed to fully initialize the class. Intentionally left blank.
#[jsmethods]
impl ReadAllBytesState {
    fn new(
        reader: DefaultReader,
        promise: Promise,
        on_complete: ReadAllBytesComplete,
        payload: HandleValue,
    ) -> Self {
        Self {
            reader: Heap::from(reader),
            promise: Heap::from(promise),
            on_complete: Some(on_complete),
            payload: Heap::from(payload.get()),
            bytes: Vec::new(),
            read_next_fn: None,
        }
    }
}

/// `Read all bytes` from a default reader for `stream`, returning a promise that
/// settles with the result of `on_complete` applied to the assembled bytes (or
/// rejects if the stream errors, a chunk is not a `Uint8Array`, or `on_complete`
/// throws). `payload` is handed back to `on_complete` unchanged.
pub fn read_all_bytes<'r>(
    scope: &'r Scope<'_>,
    stream: ReadableStream<'_>,
    on_complete: ReadAllBytesComplete,
    payload: HandleValue<'_>,
) -> Result<Promise<'r>, ExnThrown> {
    let promise = Promise::new_pending(scope)?;
    // Acquire an *internal* default reader (not the author-facing `getReader()`),
    // so its read requests deliver chunks to native steps rather than to author
    // code via promise resolution.
    //
    // The reader is deliberately never released (neither close nor error steps do
    // so): the sole caller is the Fetch "fully read a body" path, which consumes a
    // body exactly once and must leave the stream locked-and-disturbed so it
    // cannot be re-read (mirroring `body::readable_stream_consumed`). A future
    // non-fetch caller that wants the stream reusable afterward must release it
    // itself.
    let reader = acquire_readable_stream_default_reader(scope, &stream)?;
    let state = ReadAllBytesState::new(scope, reader, promise, on_complete, payload)?;
    // The deferred-read callback, reused for every chunk. Created here (not in
    // the closure above) because its payload is the state object.
    let read_next_fn = Function::new_callback(scope, c"", 0, read_next_microtask, state)?;
    state.data_mut().read_next_fn.set(read_next_fn);
    read_next(scope, state);
    Ok(promise)
}

/// Issue the next internal read on the state's reader, delivering the result to
/// the `Consume` read request's steps.
fn read_next(scope: &Scope<'_>, state: ReadAllBytesState<'_>) {
    let reader = state.data().reader.get(scope);
    readable_stream_default_reader_read(
        scope,
        reader,
        ReadRequest::Consume {
            state: Heap::from(state),
        },
    );
}

/// The `Consume` read request's chunk steps, per the spec's read-loop:
///
/// > chunk steps, given chunk
/// > 1. If chunk is not a Uint8Array object, call failureSteps with a TypeError
/// >    and abort these steps.
/// > 2. Append the bytes represented by chunk to bytes.
/// > 3. Read-loop given reader, bytes, successSteps, and failureSteps.
///
/// Step 3's next read is deferred by one microtask: queued chunks are delivered
/// synchronously (`readable_stream_default_reader_read`'s pull steps run the
/// chunk steps on the same stack), so issuing the next read directly from here
/// recurses once per queued chunk and overflows the stack on a body with many
/// queued chunks. One flat microtask per chunk keeps the loop iterative, the
/// same way the tee and pipe read requests defer their per-chunk work.
pub(crate) fn read_all_bytes_chunk_steps(
    scope: &Scope<'_>,
    state: ReadAllBytesState<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: If chunk is not a Uint8Array object, call failureSteps with a
    //         TypeError and abort these steps.
    let Ok(array) = Uint8Array::from_jsval(scope, chunk, ()) else {
        js::error::throw_type_error(scope, c"a body stream chunk must be a Uint8Array");
        let promise = state.data().promise.get(scope);
        return promise.reject_with_pending(scope);
    };
    // Step 2: Append the bytes represented by chunk to bytes.
    let mut data = state.data_mut();
    let bytes = &mut data.bytes;
    let appended = array.with_slice(|chunk| {
        let reserved = bytes.try_reserve(chunk.len()).is_ok();
        if reserved {
            bytes.extend_from_slice(chunk);
        }
        reserved
    });
    drop(data);
    if !appended {
        js::exception::report_out_of_memory(scope);
        let promise = state.data().promise.get(scope);
        return promise.reject_with_pending(scope);
    }
    // Step 3: Read-loop — deferred one microtask (see above): queue the
    // state's deferred-read callback directly on the job queue.
    let microtask = state
        .data()
        .read_next_fn
        .get(scope)
        .expect("created in read_all_bytes");
    js::jobs::queue_microtask(scope, &microtask)
}

/// The deferred step 3 of the chunk steps: issue the next internal read
/// (payload = the `ReadAllBytesState`).
fn read_next_microtask(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ReadAllBytesState>(scope, payload);
    read_next(scope, state);
    Ok(value::undefined())
}

/// The `Consume` read request's close steps: hand the accumulated bytes (by
/// value — no `Uint8Array` round-trip) to the native success callback, and
/// resolve the result promise with its return value (or reject if it throws).
pub(crate) fn read_all_bytes_close_steps(
    scope: &Scope<'_>,
    state: ReadAllBytesState<'_>,
) -> Result<(), ExnThrown> {
    let bytes = std::mem::take(&mut state.data_mut().bytes);
    let promise = state.data().promise.get(scope);
    let on_complete = state
        .data()
        .on_complete
        .expect("state is always created with a completion");
    let payload = state.data().payload.get(scope);
    match on_complete(scope, payload, bytes) {
        Ok(value) => promise.resolve(scope, value),
        Err(_) => promise.reject_with_pending(scope),
    }
}

/// The `Consume` read request's error steps: reject the result promise with the
/// stream's stored error.
pub(crate) fn read_all_bytes_error_steps(
    scope: &Scope<'_>,
    state: ReadAllBytesState<'_>,
    e: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let promise = state.data().promise.get(scope);
    promise.reject(scope, e)
}
