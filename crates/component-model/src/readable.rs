// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Component Model `stream<T>` as `ReadableStream` (wasm-only).
//!
//! A `stream<T>` lifted into JS, as an export's argument or an import's result,
//! becomes a `ReadableStream` backed by the stream's readable end: a readable
//! byte stream of `Uint8Array` chunks for `stream<u8>`, and a default stream with
//! one chunk per element for any other `T`. The stream reads from the host only
//! when a reader asks for data, and cancelling it drops the readable end.
//!
//! Lowering a `stream<T>` out of JS accepts a `ReadableStream`, or any other
//! async or sync iterable object, which is converted as `ReadableStream.from`
//! converts it. A lifted stream that is still unread and unlocked is handed back
//! by its handle. Any other stream is locked, and a pump reads it and writes its
//! chunks to a fresh `stream<T>`, whose readable end the host receives.
//! A `stream<u8>` takes `ArrayBufferView` and `ArrayBuffer` chunks, and any other
//! `stream<T>` takes each chunk as one element.
//!
//! The pump is a pending send of the event loop it was started on (see
//! [`core_runtime::event_loop::PendingSend`]), so the call that started it
//! finishes once the source ends, the host stops reading, or the loop has no
//! other work left that could make the source produce more. A source that
//! errors, or produces a chunk of the wrong kind, ends the `stream<T>` early and
//! logs the error to stderr, since a `stream<T>` has no way to carry an error.
//! The source is cancelled once a write finds the host's reader dropped, which
//! happens when the source produces its next chunk, and when nothing left on
//! the loop can make it produce more.

#![cfg(target_arch = "wasm32")]

use core_runtime::{jsclass, jsmethods};
use js::class::create_instance_with;
use js::conversion::FromJSVal;
use js::error::{throw_type_error, ExnThrown};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt, RootedHeap};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object, Promise};
use web_streams::readable::default_reader::DefaultReaderImpl;
use web_streams::readable::native_read::{
    acquire_native_reader, native_reader_cancel, native_reader_read, NativeReadSteps,
};
use web_streams::readable::{
    ReadableByteStreamController, ReadableStream, ReadableStreamDefaultController,
};
use web_streams::AlgorithmArg;
use wit_bindgen::rt::async_support::{StreamReader, StreamResult, StreamWriter};

use crate::interpreter::wit_stream;
use crate::streams::{
    elem_is_u8, generic_stream_vtable, u8_stream_vtable, with_elem_type_future, ElemType, JsElem,
    SendDone,
};

/// The most bytes one read from a `stream<u8>` asks the host for, and the most
/// the pump collects into one write.
const BYTE_CHUNK: usize = 64 * 1024;

/// The most elements one read from any other `stream<T>` asks the host for, and
/// the most the pump collects into one write.
const VALUE_CHUNK: usize = 64;

/// The readable end of a lifted `stream<T>`.
enum Reader {
    Bytes(StreamReader<u8>),
    Values(StreamReader<JsElem>),
}

impl Reader {
    /// Take the readable end's handle, leaving this reader unable to drop it.
    fn take_handle(self) -> u32 {
        match self {
            Reader::Bytes(reader) => reader.take_handle(),
            Reader::Values(reader) => reader.take_handle(),
        }
    }
}

/// The writable end of a `stream<T>` the pump fills.
enum Writer {
    Bytes(StreamWriter<u8>),
    Values(StreamWriter<JsElem>),
}

/// The chunks the pump has read but not yet written.
enum Batch {
    Bytes(Vec<u8>),
    Values(Vec<JsElem>),
}

impl Default for Batch {
    fn default() -> Self {
        Batch::Bytes(Vec::new())
    }
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
            Batch::Bytes(bytes) => bytes.len() >= BYTE_CHUNK,
            Batch::Values(values) => values.len() >= VALUE_CHUNK,
        }
    }

    /// An empty batch of the same kind, leaving `self` holding the chunks.
    fn empty_like(&self) -> Batch {
        match self {
            Batch::Bytes(_) => Batch::Bytes(Vec::new()),
            Batch::Values(_) => Batch::Values(Vec::new()),
        }
    }
}

/// The underlying source of a lifted `stream<T>`'s `ReadableStream`.
#[jsclass(hidden)]
pub struct IncomingStream {
    /// The `stream<T>` type index.
    #[no_trace]
    index: u32,
    /// The readable end, while it is neither being read nor released. `None`
    /// once the host dropped the writable end, the stream was cancelled, or the
    /// handle was handed back to the host.
    #[no_trace]
    reader: Option<Reader>,
    /// Whether the stream has pulled from the readable end.
    #[no_trace]
    pulled: bool,
    /// The promise of the pull whose read is in flight, for cancelling it.
    current_pull: Option<Heap<js::promise::Promise>>,
    /// Whether the stream was cancelled, after which a finished read's reader
    /// is dropped rather than kept.
    #[no_trace]
    cancelled: bool,
    /// The buffer of a pull whose bytes were copied into JavaScript, which the
    /// next pull that asks for as many bytes reads into.
    #[no_trace]
    spare: Option<Vec<u8>>,
}

#[jsmethods]
impl IncomingStream {
    fn new() -> Self {
        // Internal type, created through `create_instance_with`. This constructor
        // only registers the prototype.
        IncomingStreamImpl::default()
    }
}

/// Lift the readable end `handle` of a `stream<T>` with type index `index` into a
/// `ReadableStream`.
pub fn stream_from_handle(scope: &Scope<'_>, index: u32, handle: u32) -> Result<Value, ExnThrown> {
    let ty = wit_stream(index as usize);
    let bytes = elem_is_u8(ty.ty());
    let reader = if bytes {
        Reader::Bytes(StreamReader::new(handle, u8_stream_vtable(ty, index)))
    } else {
        Reader::Values(StreamReader::new(handle, generic_stream_vtable(ty, index)))
    };
    let state = create_instance_with::<IncomingStreamImpl>(scope, |_| IncomingStreamImpl {
        index,
        reader: Some(reader),
        ..Default::default()
    })?;
    let pull = AlgorithmArg::native(scope, incoming_pull, state)?;
    let cancel = AlgorithmArg::native(scope, incoming_cancel, state)?;
    let stream = if bytes {
        ReadableStream::new_native_bytes(scope, state, pull, cancel)?
    } else {
        ReadableStream::new_native(scope, state, pull, cancel)?
    };
    Ok(stream.as_value())
}

/// Underlying-source `pull`: read the next batch from the host and enqueue it,
/// or close the stream once the host dropped its writable end.
fn incoming_pull(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<IncomingStream>(scope, payload);
    let controller = Object::from_value(scope, args.get(0))
        .map_err(|_| throw_type_error(scope, c"pull expects a controller"))?;
    let promise = Promise::new_pending(scope)?;
    // Only an active event loop drives the read, and a rejected pull errors the
    // stream.
    if core_runtime::event_loop::reject_without_event_loop(scope, &promise, "reading a stream")? {
        return Ok(promise.as_value());
    }
    state.data_mut().pulled = true;
    let reader = state.data_mut().reader.take();
    let Some(reader) = reader else {
        // The readable end is gone, so no more chunks will arrive.
        close(scope, controller)?;
        promise.resolve(scope, HandleValue::undefined())?;
        return Ok(promise.as_value());
    };
    state.data_mut().current_pull = Some(Heap::from(promise));
    let index = state.data().index;
    // A BYOB read waiting for bytes asks for at most what its view holds, so a
    // read that completes fits the view.
    let byob_length = controller
        .cast::<ReadableByteStreamController>()
        .ok()
        .and_then(|controller| controller.byob_request(scope))
        .and_then(|request| request.view(scope))
        .map(|view| view.byte_length());
    let capacity = byob_length.map_or(BYTE_CHUNK, |length| length.clamp(1, BYTE_CHUNK));
    let spare = state
        .data_mut()
        .spare
        .take()
        .filter(|spare| spare.capacity() == capacity);
    let controller = RootedHeap::new(controller);
    let state = RootedHeap::new(state);
    let future = async move {
        // A read that completes with no items is repeated, since a pull that
        // enqueues nothing is not followed by another.
        let (reader, status, batch) = match reader {
            Reader::Bytes(mut reader) => {
                let mut bytes = spare.unwrap_or_else(|| Vec::with_capacity(capacity));
                bytes.clear();
                let status = loop {
                    let (status, read) = reader.read(bytes).await;
                    bytes = read;
                    if status != StreamResult::Complete(0) {
                        break status;
                    }
                };
                (Reader::Bytes(reader), status, Batch::Bytes(bytes))
            }
            Reader::Values(mut reader) => {
                let ty = wit_stream(index as usize);
                let mut values = Vec::with_capacity(VALUE_CHUNK);
                let status = loop {
                    // The per-element `lift` runs inside a poll of the read future.
                    let read = reader.read(values);
                    let (status, read) = with_elem_type_future(ElemType::Stream(ty), read).await;
                    values = read;
                    if status != StreamResult::Complete(0) {
                        break status;
                    }
                };
                (Reader::Values(reader), status, Batch::Values(values))
            }
        };
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            deliver(
                scope,
                state.get(scope),
                controller.get(scope),
                reader,
                status,
                batch,
            )
            .map(|()| HandleValue::undefined())
        }))
    };
    promise.spawn(PromiseFuture::new(future));
    Ok(promise.as_value())
}

/// Enqueue a pull's `batch`, and close the stream if the host dropped its
/// writable end with nothing left to deliver. A stream cancelled while the
/// read was in flight gets nothing, and its reader is dropped.
fn deliver(
    scope: &Scope<'_>,
    state: IncomingStream<'_>,
    controller: Object<'_>,
    reader: Reader,
    status: StreamResult,
    batch: Batch,
) -> Result<(), ExnThrown> {
    state.data_mut().current_pull = None;
    if state.data().cancelled {
        return Ok(());
    }
    let ended = matches!(status, StreamResult::Dropped) && batch.is_empty();
    if !ended {
        state.data_mut().reader = Some(reader);
    }
    match batch {
        Batch::Bytes(bytes) if !bytes.is_empty() => {
            let controller = controller
                .cast::<ReadableByteStreamController>()
                .map_err(|_| throw_type_error(scope, c"expected a byte stream controller"))?;
            state.data_mut().spare = deliver_bytes(scope, controller, bytes)?;
        }
        Batch::Bytes(_) => {}
        Batch::Values(values) => {
            let controller = controller
                .cast::<ReadableStreamDefaultController>()
                .map_err(|_| throw_type_error(scope, c"expected a default stream controller"))?;
            for value in &values {
                // SAFETY: read for immediate consumption by `root_value`. `values`
                // keeps every element registered and traced until it drops.
                let value = scope.root_value(unsafe { value.get() });
                controller.enqueue(scope, value)?;
            }
        }
    }
    if ended {
        close(scope, controller)?;
    }
    Ok(())
}

/// Hand `bytes` to the byte stream `controller` controls: written into the view
/// of a waiting BYOB read, or enqueued as a chunk. A chunk takes over the
/// allocation of `bytes` unless most of it is unused. Returns `bytes` if they
/// were copied, for another pull to reuse.
fn deliver_bytes(
    scope: &Scope<'_>,
    controller: ReadableByteStreamController<'_>,
    bytes: Vec<u8>,
) -> Result<Option<Vec<u8>>, ExnThrown> {
    if let Some(request) = controller.byob_request(scope) {
        if let Some(view) = request.view(scope) {
            // SAFETY: nothing runs between taking the slice and the copy, so no
            // GC can move or detach the buffer while the slice is live.
            let written = unsafe {
                let target = view.bytes_mut();
                (target.len() >= bytes.len()).then(|| {
                    target[..bytes.len()].copy_from_slice(&bytes);
                    bytes.len()
                })
            };
            if let Some(written) = written {
                request.respond(scope, js::conversion::EnforceRange(written as u64))?;
                return Ok(Some(bytes));
            }
        }
    }
    let length = bytes.len();
    let (chunk, spare) = if length * 2 >= bytes.capacity() {
        let buffer = js::ArrayBuffer::from_external(scope, bytes)?;
        (js::Uint8Array::with_buffer(scope, buffer, 0, length)?, None)
    } else {
        (js::Uint8Array::with_data(scope, &bytes)?, Some(bytes))
    };
    controller.enqueue(scope, chunk.as_array_buffer_view())?;
    Ok(spare)
}

/// Close the stream `controller` controls, which is either kind of controller.
/// A BYOB read still waiting on a byte stream is answered with no bytes, which
/// the reader sees as the end of the stream.
fn close(scope: &Scope<'_>, controller: Object<'_>) -> Result<(), ExnThrown> {
    if let Ok(controller) = controller.cast::<ReadableByteStreamController>() {
        controller.close(scope)?;
        match controller.byob_request(scope) {
            Some(request) => request.respond(scope, js::conversion::EnforceRange(0)),
            None => Ok(()),
        }
    } else if let Ok(controller) = controller.cast::<ReadableStreamDefaultController>() {
        controller.close(scope)
    } else {
        Err(throw_type_error(scope, c"expected a stream controller"))
    }
}

/// Underlying-source `cancel`: drop the readable end, cancelling a read in flight.
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

/// The handle of `stream`'s readable end, if `stream` is a lifted `stream<T>` of
/// type index `index` that nothing has read or locked. The stream is left locked
/// and disturbed, since its chunks now go to the host directly.
fn take_untouched_handle(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    index: u32,
) -> Option<u32> {
    if stream.is_locked() || stream.is_disturbed() {
        return None;
    }
    let state = stream.native_source(scope)?.cast::<IncomingStream>().ok()?;
    if state.data().pulled || state.data().index != index {
        return None;
    }
    let reader = state.data_mut().reader.take()?;
    let handle = reader.take_handle();
    stream
        .lock_and_disturb(scope)
        .expect("an unlocked stream can be locked");
    Some(handle)
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
    // SAFETY: `tx` is the writable handle `ty.new()` just minted, and each vtable
    // is this stream type's.
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
    let state = match create_instance_with::<StreamPumpImpl>(scope, |_| StreamPumpImpl {
        reader: Heap::from(reader),
        index,
        writer: Some(writer),
        batch,
        ..Default::default()
    }) {
        Ok(state) => state,
        Err(e) => {
            // SAFETY: the wit-dylib `drop-readable` extern takes the live handle,
            // which nothing else owns. The writer closed with the failed closure.
            unsafe { ty.drop_readable()(rx) };
            return Err(e);
        }
    };
    // Once the event loop has no other work left, the pump is finished: the
    // stream ends and its source is cancelled.
    let done = SendDone::default();
    let pump = RootedHeap::new(state);
    let registered = done.register(move || {
        crate::init::with_scope(|scope| {
            let state = pump.get(scope);
            if !state.data().done.is_set() {
                finish(
                    scope,
                    &state,
                    Some("nothing is left that could make the stream's source produce more".into()),
                );
            }
        })
    });
    if registered {
        state.data_mut().done = done;
    }
    // The pump starts from a microtask, so lowering the stream runs none of the
    // source's JavaScript.
    let started = Function::new_callback(scope, c"", 0, start_pump, state)
        .and_then(|start| js::jobs::queue_microtask(scope, &start));
    if started.is_err() {
        let message = describe_pending(scope);
        finish(scope, &state, Some(message));
    }
    Ok(rx)
}

/// Microtask: start the pump [`stream_to_handle`] created.
fn start_pump(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<StreamPump>(scope, payload);
    run(scope, &state);
    Ok(js::value::undefined())
}

/// How the pump's source ended.
enum End {
    Closed,
    /// The source errored, or produced a chunk of the wrong kind.
    Errored(String),
}

/// The state of a pump writing a `ReadableStream`'s chunks to a `stream<T>`. The
/// writer is taken out while a write is in flight, and dropped, which ends the
/// `stream<T>`, once the pump is done.
#[jsclass(hidden)]
pub struct StreamPump {
    reader: Heap<DefaultReaderImpl>,
    /// The `stream<T>` type index.
    #[no_trace]
    index: u32,
    #[no_trace]
    writer: Option<Writer>,
    #[no_trace]
    batch: Batch,
    /// How the source ended, once its close or error steps ran or it produced a
    /// chunk of the wrong kind.
    #[no_trace]
    end: Option<End>,
    /// Whether a read request is waiting for a chunk.
    #[no_trace]
    reading: bool,
    /// Whether [`run`] is on the stack.
    #[no_trace]
    running: bool,
    /// Set once the pump is done, which completes its pending send.
    #[no_trace]
    done: SendDone,
}

#[jsmethods]
impl StreamPump {
    fn new() -> Self {
        // Internal type, created through `create_instance_with`. This constructor
        // only registers the prototype.
        StreamPumpImpl::default()
    }
}

/// The pump's native read-request steps. Each records what it received and then
/// calls [`run`], which does nothing when the step ran from inside it.
const PUMP_STEPS: NativeReadSteps = NativeReadSteps {
    chunk: pump_chunk,
    close: pump_close,
    error: pump_error,
};

/// Advance the pump: read chunks into the batch for as long as reads complete
/// synchronously and the batch has room, then write the batch once a read has to
/// wait, the batch is full, or the source has ended. At most one read or write is
/// outstanding at a time.
fn run(scope: &Scope<'_>, state: &StreamPump<'_>) {
    if state.data().running {
        return;
    }
    state.data_mut().running = true;
    while can_read(state) {
        state.data_mut().reading = true;
        let reader = state.data().reader.get(scope);
        if native_reader_read(scope, reader, PUMP_STEPS, state.as_object()).is_err() {
            let message = describe_pending(scope);
            state.data_mut().reading = false;
            state.data_mut().end = Some(End::Errored(message));
        }
        if state.data().reading {
            break;
        }
    }
    write_batch(scope, state);
    state.data_mut().running = false;
}

/// Whether the pump may issue another read: no write is in flight, the source has
/// not ended, no read is outstanding, and the batch has room.
fn can_read(state: &StreamPump<'_>) -> bool {
    let data = state.data();
    data.writer.is_some() && data.end.is_none() && !data.reading && !data.batch.is_full()
}

/// Write the batch if a read has to wait, the batch is full, or the source has
/// ended, and finish the pump once the source has ended and the batch is written.
fn write_batch(scope: &Scope<'_>, state: &StreamPump<'_>) {
    let mut data = state.data_mut();
    if data.writer.is_none() {
        // A write is in flight, or the pump is done.
        return;
    }
    let ready = data.reading || data.end.is_some() || data.batch.is_full();
    if !data.batch.is_empty() && ready {
        let empty = data.batch.empty_like();
        let mut batch = std::mem::replace(&mut data.batch, empty);
        let index = data.index;
        drop(data);
        // Lowering the batch traps on a resource wrapper or stream it holds twice,
        // or a wrapper disposed of since its chunk was read, so the batch is
        // checked first. It is written up to the element that fails, and the
        // stream then ends early.
        if let Batch::Values(values) = &mut batch {
            if let Err((failed, message)) = check_batch(scope, index, values) {
                values.truncate(failed);
                state.data_mut().end = Some(End::Errored(message));
            }
        }
        if batch.is_empty() {
            write_batch(scope, state);
            return;
        }
        let writer = state.data_mut().writer.take().expect("checked above");
        spawn_write(scope, state, writer, batch);
        return;
    }
    if data.batch.is_empty() {
        match data.end.take() {
            Some(End::Closed) => {
                drop(data);
                finish(scope, state, None);
            }
            Some(End::Errored(message)) => {
                drop(data);
                eprintln!("a stream's source ended the stream early: {message}");
                finish(scope, state, Some(message));
            }
            None => {}
        }
    }
}

/// Check that the elements of `values`, copies of chunks of the `stream<T>` with
/// type index `index`, lower together (see [`crate::validate::check_each`]).
/// Returns the index of the first that does not, and why.
fn check_batch(scope: &Scope<'_>, index: u32, values: &[JsElem]) -> Result<(), (usize, String)> {
    let element = crate::interpreter::stream_element(index);
    let Some(shape) = element
        .shape
        .as_ref()
        .filter(|shape| crate::validate::moves_handles(shape))
    else {
        return Ok(());
    };
    // SAFETY: each value is rooted before the next is read.
    let values = values
        .iter()
        .map(|value| scope.root_value(unsafe { value.get() }));
    crate::validate::check_each(scope, values, shape)
        .map_err(|(failed, mismatch)| (failed, mismatch.describe("a chunk")))
}

/// Write `batch` to the `stream<T>`, and advance the pump once the host has taken
/// all of it. A host that hung up first ends the pump and cancels the source.
fn spawn_write(scope: &Scope<'_>, state: &StreamPump<'_>, writer: Writer, batch: Batch) {
    let Ok(promise) = Promise::new_pending(scope) else {
        let message = describe_pending(scope);
        drop(writer);
        finish(scope, state, Some(message));
        return;
    };
    let index = state.data().index;
    let state = RootedHeap::new(*state);
    let future = async move {
        let (writer, delivered) = match (writer, batch) {
            (Writer::Bytes(mut writer), Batch::Bytes(bytes)) => {
                let leftover = writer.write_all(bytes).await;
                (Writer::Bytes(writer), leftover.is_empty())
            }
            (Writer::Values(mut writer), Batch::Values(values)) => {
                let ty = wit_stream(index as usize);
                // The per-element `lower`, and the `lift` of an unsent tail, run
                // inside polls of the write future.
                let write = writer.write_all(values);
                let leftover = with_elem_type_future(ElemType::Stream(ty), write).await;
                (Writer::Values(writer), leftover.is_empty())
            }
            _ => unreachable!("a pump's batch matches its writer"),
        };
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let state = state.get(scope);
            if delivered {
                state.data_mut().writer = Some(writer);
                run(scope, &state);
            } else {
                drop(writer);
                finish(
                    scope,
                    &state,
                    Some("the stream's reader was dropped".to_string()),
                );
            }
            Ok(HandleValue::undefined())
        }))
    };
    // The write's first poll lowers the batch, so with an event loop active it
    // runs now, while the batch is in the state `check_batch` checked.
    crate::streams::spawn_transfer(&promise, PromiseFuture::new(future));
}

/// End the pump: drop the writer, which ends the `stream<T>`, and release the
/// event loop. With a `cancel_reason`, the source is cancelled with a `TypeError`
/// carrying it, so it stops producing chunks nothing will read.
fn finish(scope: &Scope<'_>, state: &StreamPump<'_>, cancel_reason: Option<String>) {
    {
        let mut data = state.data_mut();
        data.writer = None;
        data.batch = data.batch.empty_like();
        data.done.set();
    }
    let Some(reason) = cancel_reason else {
        return;
    };
    let _ = js::error::TypeError(reason).throw(scope);
    let reason = js::exception::take_pending_or_undefined(scope);
    let reader = state.data().reader.get(scope);
    let cancelled = native_reader_cancel(scope, reader, reason);
    // The cancellation rejects for a source that already errored, and nothing
    // observes it.
    let _ = cancelled.set_any_is_handled(scope);
}

/// Chunk steps: add the chunk to the batch, or end the pump if it is the wrong
/// kind of chunk for the `stream<T>`.
fn pump_chunk(
    scope: &Scope<'_>,
    payload: Object<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = payload
        .cast::<StreamPump>()
        .expect("payload is a StreamPump");
    state.data_mut().reading = false;
    let bytes_chunk = matches!(state.data().batch, Batch::Bytes(_));
    if bytes_chunk {
        let appended = match &mut state.data_mut().batch {
            Batch::Bytes(batch) => append_chunk_bytes(scope, chunk, batch),
            Batch::Values(_) => unreachable!("checked above"),
        };
        if !appended {
            state.data_mut().end = Some(End::Errored(
                "a stream<u8> chunk must be an ArrayBufferView or an ArrayBuffer".to_string(),
            ));
        }
    } else {
        // A chunk is checked here, where a mismatch can end the stream. The
        // lowering, which runs inside a poll of the write, would trap on it. The
        // write lowers a copy, so a change the source makes to the chunk after
        // enqueueing it has no effect on what is sent.
        let element = crate::interpreter::stream_element(state.data().index);
        let copy = match &element.shape {
            Some(shape) => crate::validate::snapshot(scope, chunk, shape),
            None => Ok(chunk),
        };
        match copy {
            Err(mismatch) => {
                state.data_mut().end = Some(End::Errored(mismatch.describe("a chunk")));
            }
            Ok(copy) => {
                let value = JsElem::new(copy.get());
                if let Batch::Values(batch) = &mut state.data_mut().batch {
                    batch.push(value);
                }
            }
        }
    }
    run(scope, &state);
    Ok(())
}

/// Append the bytes of a `stream<u8>` chunk to `batch`, returning `false` if it
/// is neither an `ArrayBufferView` nor an `ArrayBuffer`.
fn append_chunk_bytes(scope: &Scope<'_>, chunk: HandleValue<'_>, batch: &mut Vec<u8>) -> bool {
    // SAFETY: growing `batch` runs no JS, so no GC can move or detach the
    // buffer while its slice is live.
    unsafe {
        if let Ok(view) = js::ArrayBufferView::from_jsval(scope, chunk, ()) {
            batch.extend_from_slice(view.bytes());
            return true;
        }
        match js::ArrayBuffer::from_jsval(scope, chunk, ()) {
            Ok(buffer) => {
                batch.extend_from_slice(buffer.bytes());
                true
            }
            Err(_) => false,
        }
    }
}

/// Close steps: end the `stream<T>` once the batch is written.
fn pump_close(scope: &Scope<'_>, payload: Object<'_>) -> Result<(), ExnThrown> {
    let state = payload
        .cast::<StreamPump>()
        .expect("payload is a StreamPump");
    state.data_mut().reading = false;
    state.data_mut().end = Some(End::Closed);
    run(scope, &state);
    Ok(())
}

/// Error steps: end the `stream<T>` early once the batch is written.
fn pump_error(
    scope: &Scope<'_>,
    payload: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = payload
        .cast::<StreamPump>()
        .expect("payload is a StreamPump");
    state.data_mut().reading = false;
    let message = describe(scope, error);
    state.data_mut().end = Some(End::Errored(message));
    run(scope, &state);
    Ok(())
}

/// The message and stack of `value`, as an uncaught exception reports them.
pub(crate) fn describe(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    js::exception::set_pending(
        scope,
        value,
        js::native::ExceptionStackBehavior::DoNotCapture,
    );
    describe_pending(scope)
}

/// The message and stack of the pending exception, which is cleared.
fn describe_pending(scope: &Scope<'_>) -> String {
    ExnThrown::capture(scope).to_string()
}
