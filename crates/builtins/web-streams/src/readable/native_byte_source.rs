// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A readable byte stream whose bytes come from a native reader.
//!
//! [`NativeByteSource::create_stream`] creates a byte stream that reads from a [`ByteReader`]
//! when a reader of the stream asks for data. Each read requests at most 64 KiB, or the length of
//! a pending BYOB request's view. The bytes read are copied into that view if they fit. Otherwise,
//! they are enqueued as a chunk that uses the read's buffer if at least half of it is used, and a
//! copy if not. The next read with the same capacity reuses a buffer that wasn't enqueued. The end
//! of the reader closes the stream, and a failed read errors it with a `TypeError`. Cancelling the
//! stream drops the reader, along with a read in flight.
//!
//! While the stream is piped to an identity `TransformStream` whose readable end has no demand, a
//! pull waits for demand before reading (see [`ReadableStream::deferred_demand`]). Until a read
//! starts, [`take_untouched_reader`] can take the reader, so that a native consumer reads from it
//! directly.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use core_runtime::{jsclass, jsmethods};
use js::conversion::FromJSVal;
use js::error::{ExnThrown, ThrowException, TypeError};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt, RootedHeap};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{value, Function, Promise};

use super::byte_stream_controller::{
    ReadableByteStreamController, ReadableByteStreamControllerImpl,
};
use super::readable_stream::{ReadableStream, ReadableStreamImpl};
use crate::AlgorithmArg;

/// The maximum number of bytes that one read requests from a [`ByteReader`].
const READ_BYTES: usize = 64 * 1024;

/// The result of [`ByteReader::read`].
pub enum ByteRead<R> {
    /// The reader, and the bytes read, which are never empty.
    Bytes(R, Vec<u8>),
    /// The reader has no more bytes.
    End,
    /// The read failed. The stream is errored with a `TypeError` with this message.
    Failed(String),
}

/// A native source of bytes for [`NativeByteSource`]. The source doesn't trace its reader, so a
/// reader must not hold GC things.
pub trait ByteReader: Sized + 'static {
    /// Read the next bytes. `buf` is empty, and has capacity for the number of bytes wanted. The
    /// bytes read can be in `buf` or in another allocation, and can be more than wanted.
    fn read(self, buf: Vec<u8>) -> impl Future<Output = ByteRead<Self>> + 'static;
}

impl ByteReader for platform::http::IncomingBody {
    async fn read(mut self, buf: Vec<u8>) -> ByteRead<Self> {
        match self.read_into(buf).await {
            Ok(Some(bytes)) => ByteRead::Bytes(self, bytes),
            Ok(None) => ByteRead::End,
            Err(error) => ByteRead::Failed(format!("the body could not be read: {error}")),
        }
    }
}

/// A [`ByteReader`] with its type erased.
trait ErasedReader {
    fn read(self: Box<Self>, buf: Vec<u8>) -> ErasedRead;
    fn as_any(&self) -> &dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

/// A read from an [`ErasedReader`].
type ErasedRead = Pin<Box<dyn Future<Output = ByteRead<Box<dyn ErasedReader>>>>>;

impl<R: ByteReader> ErasedReader for R {
    fn read(self: Box<Self>, buf: Vec<u8>) -> ErasedRead {
        let read = ByteReader::read(*self, buf);
        Box::pin(async move {
            match read.await {
                ByteRead::Bytes(reader, bytes) => {
                    ByteRead::Bytes(Box::new(reader) as Box<dyn ErasedReader>, bytes)
                }
                ByteRead::End => ByteRead::End,
                ByteRead::Failed(message) => ByteRead::Failed(message),
            }
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// The underlying source of a byte stream created by [`NativeByteSource::create_stream`].
#[jsclass(hidden)]
pub struct NativeByteSource {
    /// `None` while a read is in flight, and after the reader ended or failed, the stream was
    /// cancelled, the source was aborted, or the reader was taken.
    #[no_trace]
    reader: Option<Box<dyn ErasedReader>>,
    /// Whether a read has started.
    #[no_trace]
    pulled: bool,
    /// The promise of the pull whose read is in flight, used to cancel the read.
    current_pull: Option<Heap<js::promise::Promise>>,
    /// Whether the stream was cancelled or the source aborted. The bytes of a read that completes
    /// afterwards are not delivered.
    #[no_trace]
    cancelled: bool,
    /// The buffer of a read whose bytes were copied into JavaScript. The next read with the same
    /// capacity reuses it.
    #[no_trace]
    spare: Option<Vec<u8>>,
    /// The stream this source reads for, used to look up its deferred demand.
    stream: Option<Heap<ReadableStreamImpl>>,
    /// A deferred pull's promise, resolved once its read was delivered, or once the reader was
    /// taken or dropped.
    deferred_pull: Option<Heap<js::promise::Promise>>,
    /// The controller to which a deferred pull delivers.
    deferred_controller: Option<Heap<ReadableByteStreamControllerImpl>>,
}

#[jsmethods]
impl NativeByteSource {
    fn new(reader: Box<dyn ErasedReader>) -> Self {
        NativeByteSourceImpl {
            reader: Some(reader),
            ..Default::default()
        }
    }
}

/// Whether settling a deferred pull also closes the stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloseStream {
    /// The reader was taken, so the stream ends.
    Yes,
    /// The stream is being cancelled or errored by its caller.
    No,
}

impl NativeByteSource<'_> {
    /// Create a byte stream that reads from `reader`, and the stream's source.
    pub fn create_stream<'r, R: ByteReader>(
        scope: &'r Scope<'_>,
        reader: R,
    ) -> Result<(ReadableStream<'r>, NativeByteSource<'r>), ExnThrown> {
        let source = NativeByteSource::new(scope, Box::new(reader))?;
        let pull = AlgorithmArg::native(scope, pull, source)?;
        let cancel = AlgorithmArg::native(scope, cancel, source)?;
        let stream = ReadableStream::new_native_bytes(scope, source, pull, cancel)?;
        source.data_mut().stream = Some(Heap::from(stream));
        Ok((stream, source))
    }

    /// Stop reading: drop a read in flight and the reader, and settle a deferred pull without
    /// closing the stream. The caller errors the stream.
    pub fn abort(&self, scope: &Scope<'_>) {
        self.data_mut().cancelled = true;
        let pull = self.data_mut().current_pull.take_rooted(scope);
        if let Some(pull) = pull {
            // Dropping the read's future drops the reader it owns.
            js::promise::cancel_pending_future(pull);
        }
        self.data_mut().reader = None;
        self.data_mut().spare = None;
        self.settle_deferred_pull(scope, CloseStream::No);
    }

    /// Resolve a deferred pull, if there is one, and close the stream if `close` is
    /// [`CloseStream::Yes`].
    fn settle_deferred_pull(&self, scope: &Scope<'_>, close: CloseStream) {
        let Some(pull) = self.data_mut().deferred_pull.take_rooted(scope) else {
            return;
        };
        let controller = self.data_mut().deferred_controller.take_rooted(scope);
        if let (CloseStream::Yes, Some(controller)) = (close, controller) {
            let _ = close_stream(scope, controller);
        }
        let _ = pull.resolve(scope, HandleValue::undefined());
    }
}

/// Take the reader of the [`NativeByteSource`] of `stream` if no read has started and the reader
/// is an `R`.
///
/// `stream` is the source's own stream, or the readable end of an identity `TransformStream` to
/// which that stream is piped. The source's stream closes right away if a pull is waiting for
/// demand, and on its next pull otherwise. `stream` is locked and disturbed, since its bytes are
/// read from the reader directly from now on.
pub fn take_untouched_reader<R: ByteReader>(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
) -> Option<R> {
    let source = stream
        .native_source(scope)?
        .cast::<NativeByteSource>()
        .ok()?;
    if source.data().pulled {
        return None;
    }
    let reader = source.data_mut().reader.take()?;
    if !reader.as_any().is::<R>() {
        source.data_mut().reader = Some(reader);
        return None;
    }
    let reader = *reader
        .into_any()
        .downcast::<R>()
        .expect("the reader was checked above");
    source.settle_deferred_pull(scope, CloseStream::Yes);
    stream
        .lock_and_disturb(scope)
        .expect("an unlocked stream can be locked");
    Some(reader)
}

/// Underlying-source `pull`: read the next bytes and deliver them, or close the stream once the
/// reader ended. A pull while the stream is piped to an identity `TransformStream` without demand
/// waits for demand.
fn pull(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let source = cast_payload::<NativeByteSource>(scope, payload);
    let promise = Promise::new_pending(scope)?;
    // The read requires an active event loop. A rejected pull errors the stream.
    if core_runtime::event_loop::reject_without_event_loop(scope, &promise, "reading a stream")? {
        return Ok(promise.as_value());
    }
    let controller = ReadableByteStreamController::from_jsval_throwing(scope, args.get(0), ())?;
    // A pull waits for demand only while there is a reader to read from. Without one,
    // `start_read` closes the stream.
    let stream = source.data().stream.get(scope);
    let demand = if source.data().reader.is_some() {
        stream.and_then(|stream| stream.deferred_demand(scope))
    } else {
        None
    };
    if let Some(demand) = demand {
        source.data_mut().deferred_pull = Some(Heap::from(promise));
        source.data_mut().deferred_controller = Some(Heap::from(controller));
        let resume = Function::new_callback(scope, c"", 0, resume_pull, source)?;
        demand.add_reactions_ignoring_unhandled_rejection(scope, Some(*resume), None)?;
        return Ok(promise.as_value());
    }
    start_read(scope, source, controller, promise)?;
    Ok(promise.as_value())
}

/// Demand arrived for a deferred pull: start the read it deferred.
fn resume_pull(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let source = cast_payload::<NativeByteSource>(scope, payload);
    let controller = source.data_mut().deferred_controller.take_rooted(scope);
    let pull = source.data_mut().deferred_pull.take_rooted(scope);
    // The pull was settled while it waited: the reader was taken, or the stream was cancelled
    // or aborted.
    let (Some(controller), Some(promise)) = (controller, pull) else {
        return Ok(value::undefined());
    };
    // Demand can arrive where no event loop is active to drive the read.
    if core_runtime::event_loop::reject_without_event_loop(scope, &promise, "reading a stream")? {
        return Ok(value::undefined());
    }
    start_read(scope, source, controller, promise)?;
    Ok(value::undefined())
}

/// Read the next bytes into `controller`'s stream, and resolve `promise` once they were
/// delivered.
fn start_read(
    scope: &Scope<'_>,
    source: NativeByteSource<'_>,
    controller: ReadableByteStreamController<'_>,
    promise: Promise<'_>,
) -> Result<(), ExnThrown> {
    source.data_mut().pulled = true;
    let reader = source.data_mut().reader.take();
    let Some(reader) = reader else {
        // The reader ended, or was taken or dropped, so no more bytes will arrive.
        close_stream(scope, controller)?;
        promise.resolve(scope, HandleValue::undefined())?;
        return Ok(());
    };
    source.data_mut().current_pull = Some(Heap::from(promise));
    // A pending BYOB read requests at most its view's length, so that the bytes fit into it.
    let byob_length = controller
        .byob_request(scope)
        .and_then(|request| request.view(scope))
        .map(|view| view.byte_length());
    let capacity = byob_length.map_or(READ_BYTES, |length| length.clamp(1, READ_BYTES));
    let spare = source
        .data_mut()
        .spare
        .take()
        .filter(|spare| spare.capacity() == capacity);
    let mut buf = spare.unwrap_or_else(|| Vec::with_capacity(capacity));
    buf.clear();
    let controller = RootedHeap::new(controller);
    let source = RootedHeap::new(source);
    let future = async move {
        let read = reader.read(buf).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            deliver(scope, source.get(scope), controller.get(scope), read)
                .map(|()| HandleValue::undefined())
        }))
    };
    promise.spawn(PromiseFuture::new(future));
    Ok(())
}

/// Deliver the result of a read to `controller`'s stream. A source that was cancelled or aborted
/// during the read doesn't deliver the bytes, and drops the reader.
fn deliver(
    scope: &Scope<'_>,
    source: NativeByteSource<'_>,
    controller: ReadableByteStreamController<'_>,
    read: ByteRead<Box<dyn ErasedReader>>,
) -> Result<(), ExnThrown> {
    source.data_mut().current_pull = None;
    if source.data().cancelled {
        return Ok(());
    }
    match read {
        ByteRead::Bytes(reader, bytes) => {
            source.data_mut().reader = Some(reader);
            let spare = deliver_bytes(scope, controller, bytes)?;
            source.data_mut().spare = spare;
            Ok(())
        }
        ByteRead::End => {
            source.data_mut().spare = None;
            close_stream(scope, controller)
        }
        ByteRead::Failed(message) => {
            source.data_mut().spare = None;
            let _ = TypeError(message).throw(scope);
            let reason = js::exception::take_pending(scope).ok_or(ExnThrown)?;
            controller.error(scope, Some(reason))
        }
    }
}

/// Copy `bytes` into the view of a pending BYOB request of `controller`'s stream if they fit, and
/// enqueue them as a chunk otherwise. The chunk uses the allocation of `bytes`, unless less than
/// half of it is used. Returns `bytes` if they were copied, so that another read can reuse the
/// buffer.
fn deliver_bytes(
    scope: &Scope<'_>,
    controller: ReadableByteStreamController<'_>,
    bytes: Vec<u8>,
) -> Result<Option<Vec<u8>>, ExnThrown> {
    if let Some(request) = controller.byob_request(scope) {
        if let Some(view) = request.view(scope) {
            let written = view.with_bytes_mut(|target| {
                (target.len() >= bytes.len()).then(|| {
                    target[..bytes.len()].copy_from_slice(&bytes);
                    bytes.len()
                })
            });
            if let Some(written) = written {
                request.respond(scope, js::conversion::EnforceRange(written as u64))?;
                return Ok(Some(bytes));
            }
        }
    }
    if bytes.len() * 2 >= bytes.capacity() {
        controller.enqueue_bytes(scope, bytes)?;
        return Ok(None);
    }
    controller.enqueue_bytes(scope, bytes.to_vec())?;
    Ok(Some(bytes))
}

/// Close `controller`'s stream. A pending BYOB request is answered with no bytes, which its
/// reader receives as the end of the stream.
fn close_stream(
    scope: &Scope<'_>,
    controller: ReadableByteStreamController<'_>,
) -> Result<(), ExnThrown> {
    controller.close(scope)?;
    match controller.byob_request(scope) {
        Some(request) => request.respond(scope, js::conversion::EnforceRange(0)),
        None => Ok(()),
    }
}

/// Underlying-source `cancel`: drop the reader and a read in flight.
fn cancel(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    cast_payload::<NativeByteSource>(scope, payload).abort(scope);
    Ok(value::undefined())
}
