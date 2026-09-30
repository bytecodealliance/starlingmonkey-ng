// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `stream` values, which are `ReadableStream`s in JavaScript.
//!
//! A lifted `stream` is a `ReadableStream` that reads from the stream's
//! readable end. A `stream<u8>` is a readable byte stream with `Uint8Array`
//! chunks. Any other `stream<T>` is a default stream with one chunk per
//! element, and a `stream` without a payload type has an `undefined` chunk per
//! element. Data is read from the host only when a reader requests it, and
//! cancelling the stream drops the readable end.
//!
//! A lowered `stream` can be given as a `ReadableStream`, or as any other sync
//! or async iterable object, which is converted with the same rules as
//! `ReadableStream.from`. A lifted stream that hasn't been read from or locked
//! is lowered as its readable end. A `stream<u8>` is lowered the same way if it
//! is the body of a `fetch` response or request, or a lifted `stream<u8>` of
//! another type. Any other stream is locked, and a pump reads
//! its chunks and writes them to a new `stream`, whose readable end is passed
//! to the host. A `stream<u8>` accepts `ArrayBufferView` and `ArrayBuffer`
//! chunks. Any other `stream<T>` accepts each chunk as one element, and a
//! `stream` without a payload type accepts `undefined` chunks.
//!
//! The pump is a pending send of the event loop on which it was started (see
//! [`core_runtime::event_loop::PendingSend`]). The call that started it
//! completes when the source ends, when the host stops reading, or when the
//! loop has no other work that could make the source produce more chunks. If
//! the source errors or produces a chunk of the wrong kind, the `stream` ends
//! early and the error is printed to `stderr`, because a `stream` can't hold an
//! error. The source is cancelled when a write finds that the host dropped its
//! reader, which happens when the source produces its next chunk, and when the
//! loop has no other work that could make it produce more.

#![cfg(target_arch = "wasm32")]

use core_runtime::{jsclass, jsmethods};
use js::conversion::FromJSVal;
use js::error::{throw_type_error, ExnThrown};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt, RootedHeap};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Promise};
use platform::http::IncomingBody;
use web_streams::readable::native_byte_source::{take_untouched_reader, NativeByteSource};
use web_streams::readable::native_pump::{
    append_bytes, NativePump, PumpBatch, PumpSink, PUMP_BATCH_BYTES,
};
use web_streams::readable::native_read::acquire_native_reader;
use web_streams::readable::{ReadableStream, ReadableStreamDefaultController};
use web_streams::AlgorithmArg;
use wit_bindgen::rt::async_support::{StreamReader, StreamResult, StreamWriter};

use crate::interpreter::wit_stream;
use crate::streams::{
    elem_is_u8, generic_stream_vtable, u8_stream_vtable, with_elem_type_future, Elem, ElemType,
    SendDone,
};

/// The maximum number of elements that one read from a `stream<T>` other than
/// `stream<u8>` requests from the host, and that the pump collects into one
/// write.
const VALUE_CHUNK: usize = 64;

/// The writable end of the `stream` written by a pump.
pub(crate) enum Writer {
    Bytes(StreamWriter<u8>),
    Values(StreamWriter<Elem>),
}

/// The chunks the pump has read but not yet written.
pub(crate) enum Batch {
    Bytes(Vec<u8>),
    Values(Vec<Elem>),
}

impl Batch {
    fn is_empty(&self) -> bool {
        match self {
            Batch::Bytes(bytes) => bytes.is_empty(),
            Batch::Values(values) => values.is_empty(),
        }
    }

    fn is_full(&self) -> bool {
        match self {
            Batch::Bytes(bytes) => bytes.len() >= PUMP_BATCH_BYTES,
            Batch::Values(values) => values.len() >= VALUE_CHUNK,
        }
    }

    /// Return an empty batch of the same kind.
    fn empty_like(&self) -> Batch {
        match self {
            Batch::Bytes(_) => Batch::Bytes(Vec::new()),
            Batch::Values(_) => Batch::Values(Vec::new()),
        }
    }
}

/// The underlying source of a lifted `stream<T>`'s `ReadableStream`, for a `T`
/// other than `u8`.
#[jsclass(hidden)]
pub(crate) struct IncomingStream {
    /// The `stream<T>` type index.
    #[no_trace]
    index: u32,
    /// The readable end. `None` while a read is pending, and after the host
    /// dropped the writable end, the stream was cancelled, or the readable end
    /// was passed back to the host.
    #[no_trace]
    reader: Option<StreamReader<Elem>>,
    /// Whether the stream has pulled from the readable end.
    #[no_trace]
    pulled: bool,
    /// The promise of the pull whose read is pending, used to cancel the read.
    current_pull: Option<Heap<js::promise::Promise>>,
    /// Whether the stream was cancelled, after which a finished read's reader
    /// is dropped rather than kept.
    #[no_trace]
    cancelled: bool,
}

#[jsmethods]
impl IncomingStream {
    /// The source reading `reader`, the readable end of a `stream<T>` with type
    /// index `index`.
    fn new(index: u32, reader: StreamReader<Elem>) -> Self {
        IncomingStreamImpl {
            index,
            reader: Some(reader),
            ..Default::default()
        }
    }
}

/// Lift the readable end `handle` of a `stream<T>` with type index `index` into a
/// `ReadableStream`.
pub fn stream_from_handle(scope: &Scope<'_>, index: u32, handle: u32) -> Result<Value, ExnThrown> {
    let ty = wit_stream(index as usize);
    if elem_is_u8(ty.ty()) {
        let reader = StreamReader::new(handle, u8_stream_vtable(ty, index));
        let (stream, _) =
            NativeByteSource::create_stream(scope, IncomingBody::from_stream(reader))?;
        return Ok(stream.as_value());
    }
    let reader = StreamReader::new(handle, generic_stream_vtable(ty, index));
    let state = IncomingStream::new(scope, index, reader)?;
    let pull = AlgorithmArg::native(scope, incoming_pull, state)?;
    let cancel = AlgorithmArg::native(scope, incoming_cancel, state)?;
    Ok(ReadableStream::new_native(scope, state, pull, cancel)?.as_value())
}

/// Underlying-source `pull`: read the next elements from the host and enqueue
/// them, or close the stream once the host dropped its writable end.
fn incoming_pull(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<IncomingStream>(scope, payload);
    let controller = ReadableStreamDefaultController::from_jsval_throwing(scope, args.get(0), ())?;
    let promise = Promise::new_pending(scope)?;
    // The read requires an active event loop. A rejected pull errors the
    // stream.
    if core_runtime::event_loop::reject_without_event_loop(scope, &promise, "reading a stream")? {
        return Ok(promise.as_value());
    }
    state.data_mut().pulled = true;
    let reader = state.data_mut().reader.take();
    let Some(mut reader) = reader else {
        // The readable end is gone, so no more elements will arrive.
        controller.close(scope)?;
        promise.resolve(scope, HandleValue::undefined())?;
        return Ok(promise.as_value());
    };
    state.data_mut().current_pull = Some(Heap::from(promise));
    let index = state.data().index;
    let controller = RootedHeap::new(controller);
    let state = RootedHeap::new(state);
    let future = async move {
        let ty = wit_stream(index as usize);
        let mut values = Vec::with_capacity(VALUE_CHUNK);
        // A read that completes with no elements is repeated, since a pull that
        // doesn't enqueue a chunk is not followed by another.
        let status = loop {
            // The per-element `lift` runs inside a poll of the read future.
            let read = reader.read(values);
            let (status, read) = with_elem_type_future(ElemType::Stream(ty), read).await;
            values = read;
            if status != StreamResult::Complete(0) {
                break status;
            }
        };
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            deliver(
                scope,
                state.get(scope),
                controller.get(scope),
                reader,
                status,
                values,
            )
            .map(|()| HandleValue::undefined())
        }))
    };
    promise.spawn(PromiseFuture::new(future));
    Ok(promise.as_value())
}

/// Enqueue the elements of a pull, and close the stream if the host dropped its
/// writable end with nothing left to deliver. A stream that was cancelled
/// during the read doesn't receive the elements, and its reader is dropped.
fn deliver(
    scope: &Scope<'_>,
    state: IncomingStream<'_>,
    controller: ReadableStreamDefaultController<'_>,
    reader: StreamReader<Elem>,
    status: StreamResult,
    values: Vec<Elem>,
) -> Result<(), ExnThrown> {
    state.data_mut().current_pull = None;
    if state.data().cancelled {
        return Ok(());
    }
    let ended = matches!(status, StreamResult::Dropped) && values.is_empty();
    if !ended {
        state.data_mut().reader = Some(reader);
    }
    for value in &values {
        // SAFETY: read for immediate consumption by `root_value`. `values` keeps
        // every element registered and traced until it drops.
        let value = scope.root_value(unsafe { value.get() });
        controller.enqueue(scope, value)?;
    }
    if ended {
        controller.close(scope)?;
    }
    Ok(())
}

/// Underlying-source `cancel`: drop the readable end, and cancel a pending read.
fn incoming_cancel(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<IncomingStream>(scope, payload);
    state.data_mut().cancelled = true;
    let pull = state.data_mut().current_pull.take_rooted(scope);
    if let Some(pull) = pull {
        // Dropping the read's future cancels the read and drops the reader it owns.
        js::promise::cancel_pending_future(pull);
    }
    state.data_mut().reader = None;
    Ok(js::value::undefined())
}

/// Return the handle of `stream`'s readable end if `stream` hasn't been read
/// from or locked, and is one of these:
///
/// - a lifted `stream` of the type with index `index`
/// - if that type is a `stream<u8>`: a lifted `stream<u8>` of any type, the
///   body of a `fetch` response or request, or the readable end of an identity
///   `TransformStream` to which one of them is piped
///
/// The stream is then locked and disturbed, because its chunks go to the host
/// directly.
fn take_untouched_handle(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    index: u32,
) -> Option<u32> {
    if stream.is_locked() || stream.is_disturbed() {
        return None;
    }
    // Every `stream<u8>` type has the same canonical ABI representation, so a
    // `stream<u8>` of any type, or a `fetch` body, is passed on as it is.
    if elem_is_u8(wit_stream(index as usize).ty()) {
        let bytes = take_untouched_reader::<IncomingBody>(scope, stream)?;
        return Some(bytes.into_stream().take_handle());
    }
    let state = stream.native_source(scope)?.cast::<IncomingStream>().ok()?;
    if state.data().pulled || state.data().index != index {
        return None;
    }
    let reader = state.data_mut().reader.take()?;
    stream
        .lock_and_disturb(scope)
        .expect("an unlocked stream can be locked");
    Some(reader.take_handle())
}

/// Lower `value` to the readable end of a `stream<T>` with type index `index`.
///
/// Throws a `TypeError` if `value` is neither a `ReadableStream` nor an iterable
/// object, or is a locked `ReadableStream`.
pub fn stream_to_handle(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    index: u32,
) -> Result<u32, ExnThrown> {
    let stream = match ReadableStream::from_jsval(scope, value, ()) {
        Ok(stream) => stream,
        Err(_) if value.is_object() => ReadableStream::from_iterable(scope, value)?,
        Err(_) => {
            return Err(throw_type_error(
                scope,
                c"a stream must be a ReadableStream or an iterable object",
            ))
        }
    };
    if let Some(handle) = take_untouched_handle(scope, &stream, index) {
        return Ok(handle);
    }
    // Locking the stream is the step that can fail, so it runs before the
    // `stream<T>` is created.
    let reader = acquire_native_reader(scope, &stream)?;

    let ty = wit_stream(index as usize);
    // SAFETY: `ty.new()` is the wit-dylib `stream.new` extern for this type. It
    // returns the writer and reader handle pair with no preconditions.
    let handles = unsafe { ty.new()() };
    let tx = (handles >> 32) as u32;
    let rx = (handles & 0xFFFF_FFFF) as u32;
    // SAFETY: `tx` is the writable handle just created by `ty.new()`, and each
    // vtable is this stream type's.
    let (writer, batch) = unsafe {
        if elem_is_u8(ty.ty()) {
            (
                Writer::Bytes(StreamWriter::new(tx, u8_stream_vtable(ty, index))),
                Batch::Bytes(Vec::new()),
            )
        } else {
            (
                Writer::Values(StreamWriter::new(tx, generic_stream_vtable(ty, index))),
                Batch::Values(Vec::new()),
            )
        }
    };
    let batch = PumpChunks { index, batch };
    let pump = match NativePump::create(scope, reader, writer, batch) {
        Ok(pump) => pump,
        Err(e) => {
            // SAFETY: the wit-dylib `drop-readable` extern takes the live handle,
            // which nothing else owns. The writer was dropped with the failed
            // factory's arguments.
            unsafe { ty.drop_readable()(rx) };
            return Err(e);
        }
    };
    // Once the event loop has no other work left, the pump is stopped: the
    // stream ends and its source is cancelled.
    let done = SendDone::default();
    let stopping = RootedHeap::new(pump);
    let registered = done.register(move || {
        crate::init::with_scope(|scope| {
            let reason = "nothing is left that could make the stream's source produce more";
            stopping.get(scope).stop(scope, reason.into());
        })
    });
    if registered {
        pump.on_finish(move || done.set());
    }
    // The pump starts from a microtask, so lowering the stream runs none of the
    // source's JavaScript.
    let started = Function::new_callback(scope, c"", 0, start_pump, pump)
        .and_then(|start| js::jobs::queue_microtask(scope, &start));
    if started.is_err() {
        let message = describe_pending(scope);
        pump.stop(scope, message);
    }
    Ok(rx)
}

/// The microtask that starts a pump created by [`stream_to_handle`].
fn start_pump(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    cast_payload::<NativePump>(scope, payload).start(scope);
    Ok(js::value::undefined())
}

/// The chunks a pump has read but not yet written to the `stream<T>` with type
/// index `index`.
pub(crate) struct PumpChunks {
    index: u32,
    batch: Batch,
}

impl PumpBatch for PumpChunks {
    /// Append `chunk`, or fail with the reason the stream ends if it can't be
    /// written to the `stream`.
    fn append(&mut self, scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<(), String> {
        match &mut self.batch {
            Batch::Bytes(bytes) => append_chunk_bytes(scope, chunk, bytes).map_err(String::from),
            Batch::Values(values) => {
                // The chunk is checked here, where a mismatch ends the stream.
                // Lowering it during the write would trap instead. The chunk's
                // snapshot is written, so later changes to the chunk don't
                // affect what is sent.
                let element = crate::interpreter::stream_element(self.index);
                let snapshot =
                    crate::validate::snapshot_payload(scope, chunk, element.shape.as_ref())
                        .map_err(|mismatch| mismatch.describe("a chunk"))?;
                values.push(Elem::payload(snapshot));
                Ok(())
            }
        }
    }

    /// Lowering the batch traps if it holds a resource wrapper or stream twice,
    /// or a wrapper that was disposed of after its chunk was read, so the batch
    /// is checked first. If an element fails the check, the elements before it
    /// are written, and the stream then ends early.
    fn check(&mut self, scope: &Scope<'_>) -> Result<(), String> {
        let Batch::Values(values) = &mut self.batch else {
            return Ok(());
        };
        check_batch(scope, self.index, values).map_err(|(failed, message)| {
            values.truncate(failed);
            message
        })
    }

    fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    fn is_full(&self) -> bool {
        self.batch.is_full()
    }

    fn take(&mut self) -> Self {
        let empty = self.batch.empty_like();
        PumpChunks {
            index: self.index,
            batch: std::mem::replace(&mut self.batch, empty),
        }
    }
}

/// Check that `values`, the snapshots of the chunks of the stream with type
/// index `index`, can be lowered together (see [`crate::validate::check_each`]).
/// Returns the index of the first element that can't be lowered, and the
/// reason.
fn check_batch(scope: &Scope<'_>, index: u32, values: &[Elem]) -> Result<(), (usize, String)> {
    let element = crate::interpreter::stream_element(index);
    let Some(shape) = element
        .shape
        .as_ref()
        .filter(|shape| crate::validate::moves_handles(shape))
    else {
        return Ok(());
    };
    let values = values.iter().map(|value| {
        value
            .shaped()
            .expect("a stream with a payload type writes snapshots")
    });
    crate::validate::check_each(scope, values, shape)
        .map_err(|(failed, mismatch)| (failed, mismatch.describe("a chunk")))
}

impl PumpSink for Writer {
    type Batch = PumpChunks;

    /// The write's first poll lowers the batch, so it happens while the batch is
    /// as checked by [`PumpChunks::check`].
    const POLL_WRITE_AT_ONCE: bool = true;

    async fn write(self, chunks: PumpChunks) -> Option<Self> {
        match (self, chunks.batch) {
            (Writer::Bytes(mut writer), Batch::Bytes(bytes)) => {
                let leftover = writer.write_all(bytes).await;
                leftover.is_empty().then_some(Writer::Bytes(writer))
            }
            (Writer::Values(mut writer), Batch::Values(values)) => {
                let ty = wit_stream(chunks.index as usize);
                // The per-element `lower`, and the `lift` of an unsent tail, run
                // inside polls of the write future.
                let write = writer.write_all(values);
                let leftover = with_elem_type_future(ElemType::Stream(ty), write).await;
                leftover.is_empty().then_some(Writer::Values(writer))
            }
            _ => unreachable!("a pump's batch matches its writer"),
        }
    }

    fn describe_source_error(scope: &Scope<'_>, error: HandleValue<'_>) -> String {
        crate::exports::capture_value(scope, error).to_string()
    }
}

/// Append the bytes of a `stream<u8>` chunk to `batch`. Fails with the reason
/// the stream ends if the chunk is neither an `ArrayBufferView` nor an
/// `ArrayBuffer`, or if growing `batch` fails.
fn append_chunk_bytes(
    scope: &Scope<'_>,
    chunk: HandleValue<'_>,
    batch: &mut Vec<u8>,
) -> Result<(), &'static str> {
    let append = |bytes: &[u8]| {
        append_bytes(batch, bytes).map_err(|_| "out of memory while buffering a stream<u8> chunk")
    };
    if let Ok(view) = js::ArrayBufferView::from_jsval(scope, chunk, ()) {
        return view.with_bytes(append);
    }
    match js::ArrayBuffer::from_jsval(scope, chunk, ()) {
        Ok(buffer) => buffer.with_bytes(append),
        Err(_) => Err("a stream<u8> chunk must be an ArrayBufferView or an ArrayBuffer"),
    }
}

/// The message and stack of the pending exception, which is cleared.
fn describe_pending(scope: &Scope<'_>) -> String {
    ExnThrown::capture(scope).to_string()
}
