// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Pumping a `ReadableStream` into a native sink.
//!
//! A [`NativePump`] reads a stream through an internal reader (see
//! [`native_read`](super::native_read)) and appends each chunk to a [`PumpBatch`]. It writes the
//! batch to a [`PumpSink`] once a read has to wait, the batch is full, or the stream has ended.
//! At most one read and one write are outstanding at a time. No read starts while a write is in
//! flight, but a read that was waiting when the write started can complete during it, and its
//! chunk goes into the next batch.
//!
//! The pump finishes once the stream has ended and every batch is written, or when it is stopped.
//! It then drops the chunks not yet written and calls [`PumpSink::finish`] with the [`PumpEnd`].
//! For [`PumpEnd::Failed`], it prints the failure to `stderr`. It cancels the stream unless it
//! closed, and calls the callback given to [`NativePump::on_finish`].

use std::any::Any;
use std::collections::TryReserveError;
use std::future::Future;
use std::pin::Pin;

use core_runtime::{jsclass, jsmethods};
use js::error::{ExnThrown, ThrowException, TypeError};
use js::gc::handle::{Heap, RootedHeap};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Object, Promise};

use super::default_reader::{DefaultReader, DefaultReaderImpl};
use super::native_read::{native_reader_cancel, native_reader_read, NativeReadSteps};

/// The number of bytes at which a byte batch is full.
pub const PUMP_BATCH_BYTES: usize = 64 * 1024;

/// The cancellation reason of a stream whose sink's receiver went away.
const RECEIVER_DROPPED: &str = "the stream's reader was dropped";

/// How a [`NativePump`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PumpEnd {
    /// The stream closed.
    Closed,
    /// The stream errored, a chunk was rejected by [`PumpBatch::append`] or [`PumpBatch::check`],
    /// or the stream couldn't be read. The message describes the failure.
    Failed(String),
    /// The pump stopped before the stream ended: the sink's receiver went away, or
    /// [`NativePump::stop`] was called. The message gives the reason.
    Stopped(String),
}

/// The chunks a [`NativePump`] has read but not yet written.
pub trait PumpBatch: Sized + 'static {
    /// Append `chunk`. An `Err` ends the pump with [`PumpEnd::Failed`] once the chunks appended
    /// before it are written.
    fn append(&mut self, scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<(), String>;

    /// Check the batch right before it is written. To reject a chunk, truncate the batch before
    /// it and return an `Err`. The chunks before it are written, and the pump then ends with
    /// [`PumpEnd::Failed`].
    fn check(&mut self, _scope: &Scope<'_>) -> Result<(), String> {
        Ok(())
    }

    fn is_empty(&self) -> bool;

    /// Whether the batch is written before more chunks are read.
    fn is_full(&self) -> bool;

    /// Take the batch's chunks, leaving it empty.
    fn take(&mut self) -> Self;
}

/// The destination to which a [`NativePump`] writes its batches.
pub trait PumpSink: Sized + 'static {
    type Batch: PumpBatch;

    /// Whether the future returned by [`write`](Self::write) is polled once right away if an
    /// event loop is active. Otherwise its first poll happens in the event loop.
    const POLL_WRITE_AT_ONCE: bool = false;

    /// Write `batch` if that can complete without waiting. Otherwise returns `batch`, which the
    /// pump then passes to [`write`](Self::write).
    fn try_write(&mut self, batch: Self::Batch) -> Result<(), Self::Batch> {
        Err(batch)
    }

    /// Write `batch`. The future resolves to the sink once all of `batch` was written, or to
    /// `None` if the receiver went away. The pump then ends with [`PumpEnd::Stopped`] without
    /// calling [`finish`](Self::finish).
    fn write(self, batch: Self::Batch) -> impl Future<Output = Option<Self>> + 'static;

    /// Returns the message of the [`PumpEnd::Failed`] for a stream that errored with `error`.
    fn describe_source_error(scope: &Scope<'_>, error: HandleValue<'_>) -> String;

    /// End the sink's output. Called once when the pump finishes, after its last write. Not called
    /// if a write's future resolved to `None`. The default implementation drops the sink.
    fn finish(self, _scope: &Scope<'_>, _end: &PumpEnd) {}
}

/// Append `bytes` to `batch`. Growing a non-empty `batch` reserves room up to
/// [`PUMP_BATCH_BYTES`], so a batch filled by small chunks is copied at most once.
pub fn append_bytes(batch: &mut Vec<u8>, bytes: &[u8]) -> Result<(), TryReserveError> {
    if batch.capacity() - batch.len() < bytes.len() {
        let wanted = if batch.is_empty() {
            bytes.len()
        } else {
            PUMP_BATCH_BYTES.max(batch.len() + bytes.len()) - batch.len()
        };
        batch.try_reserve_exact(wanted)?;
    }
    batch.extend_from_slice(bytes);
    Ok(())
}

/// A write in flight. It resolves to the sink, boxed as `Any`, or to `None` if the receiver went
/// away.
type SinkWrite = Pin<Box<dyn Future<Output = Option<Box<dyn Any>>>>>;

/// A sink and its batch, with the sink's type erased.
trait ErasedSink {
    fn append(&mut self, scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<(), String>;
    fn check(&mut self, scope: &Scope<'_>) -> Result<(), String>;
    fn is_empty(&self) -> bool;
    fn is_full(&self) -> bool;
    fn is_writing(&self) -> bool;
    /// Write the batch with [`PumpSink::try_write`]. Returns whether it was written.
    fn try_write(&mut self) -> bool;
    /// Write the batch with [`PumpSink::write`].
    fn write(&mut self) -> SinkWrite;
    fn poll_write_at_once(&self) -> bool;
    /// Return the sink from a write that completed.
    fn restore(&mut self, sink: Box<dyn Any>);
    fn describe_source_error(&self, scope: &Scope<'_>, error: HandleValue<'_>) -> String;
    /// Drop the chunks not yet written, and call [`PumpSink::finish`] unless a write is in flight.
    fn finish(&mut self, scope: &Scope<'_>, end: &PumpEnd);
}

struct SinkSlot<S: PumpSink> {
    /// `None` while a write is in flight, and after the sink finished.
    sink: Option<S>,
    batch: S::Batch,
}

impl<S: PumpSink> ErasedSink for SinkSlot<S> {
    fn append(&mut self, scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<(), String> {
        self.batch.append(scope, chunk)
    }

    fn check(&mut self, scope: &Scope<'_>) -> Result<(), String> {
        self.batch.check(scope)
    }

    fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    fn is_full(&self) -> bool {
        self.batch.is_full()
    }

    fn is_writing(&self) -> bool {
        self.sink.is_none()
    }

    fn try_write(&mut self) -> bool {
        let sink = self.sink.as_mut().expect("no write is in flight");
        match sink.try_write(self.batch.take()) {
            Ok(()) => true,
            Err(batch) => {
                self.batch = batch;
                false
            }
        }
    }

    fn write(&mut self) -> SinkWrite {
        let sink = self.sink.take().expect("no write is in flight");
        let write = sink.write(self.batch.take());
        Box::pin(async move {
            let sink = write.await?;
            Some(Box::new(sink) as Box<dyn Any>)
        })
    }

    fn poll_write_at_once(&self) -> bool {
        S::POLL_WRITE_AT_ONCE
    }

    fn restore(&mut self, sink: Box<dyn Any>) {
        let sink = sink.downcast::<S>().expect("a write returns its own sink");
        self.sink = Some(*sink);
    }

    fn describe_source_error(&self, scope: &Scope<'_>, error: HandleValue<'_>) -> String {
        S::describe_source_error(scope, error)
    }

    fn finish(&mut self, scope: &Scope<'_>, end: &PumpEnd) {
        drop(self.batch.take());
        if let Some(sink) = self.sink.take() {
            sink.finish(scope, end);
        }
    }
}

/// The state of a pump from a `ReadableStream` to a [`PumpSink`].
#[jsclass(hidden)]
pub struct NativePump {
    reader: Heap<DefaultReaderImpl>,
    /// The sink and its batch. `None` only while a sink method runs, so that no borrow of this
    /// data is held across it.
    #[no_trace]
    sink: Option<Box<dyn ErasedSink>>,
    /// How the pump ends, once the stream ended, a chunk was rejected, or the pump was stopped.
    #[no_trace]
    end: Option<PumpEnd>,
    /// Whether a read request is waiting for a chunk.
    #[no_trace]
    reading: bool,
    /// Whether [`run`] is on the stack.
    #[no_trace]
    running: bool,
    #[no_trace]
    finished: bool,
    #[no_trace]
    on_finish: Option<Box<dyn FnOnce()>>,
}

#[jsmethods]
impl NativePump {
    fn new(reader: DefaultReader, sink: Box<dyn ErasedSink>) -> Self {
        NativePumpImpl {
            reader: Heap::from(reader),
            sink: Some(sink),
            end: None,
            reading: false,
            running: false,
            finished: false,
            on_finish: None,
        }
    }
}

impl NativePump<'_> {
    /// Create a pump that reads with `reader`, a reader from
    /// [`acquire_native_reader`](super::native_read::acquire_native_reader), and writes to
    /// `sink`, collecting chunks into `batch`, which must be empty. The pump starts reading once
    /// [`start`](Self::start) is called.
    pub fn create<'r, S: PumpSink>(
        scope: &'r Scope<'_>,
        reader: DefaultReader<'_>,
        sink: S,
        batch: S::Batch,
    ) -> Result<NativePump<'r>, ExnThrown> {
        let slot = SinkSlot {
            sink: Some(sink),
            batch,
        };
        NativePump::new(scope, reader, Box::new(slot))
    }

    /// Call `f` once the pump has finished.
    pub fn on_finish(&self, f: impl FnOnce() + 'static) {
        self.data_mut().on_finish = Some(Box::new(f));
    }

    /// Start reading the stream.
    pub fn start(&self, scope: &Scope<'_>) {
        run(scope, self);
    }

    /// Finish the pump with [`PumpEnd::Stopped`], giving `reason`. The stream is cancelled with a
    /// `TypeError` whose message is `reason`, and the chunks not yet written are dropped. A
    /// pump that has finished is left as it is.
    pub fn stop(&self, scope: &Scope<'_>, reason: String) {
        if self.data().finished {
            return;
        }
        self.data_mut().end = Some(PumpEnd::Stopped(reason));
        finish(scope, self);
    }

    pub fn is_finished(&self) -> bool {
        self.data().finished
    }

    /// Run `f` on the sink, with the sink taken out of this pump's data while `f` runs. Returns
    /// `None` if the sink is already taken out, by a call further up the stack.
    fn with_sink<R>(&self, f: impl FnOnce(&mut dyn ErasedSink) -> R) -> Option<R> {
        let mut sink = self.data_mut().sink.take()?;
        let result = f(&mut *sink);
        self.data_mut().sink = Some(sink);
        Some(result)
    }
}

/// The pump's read-request steps. Each records what it received and then calls [`run`], which
/// does nothing when the step ran from inside it.
const PUMP_STEPS: NativeReadSteps = NativeReadSteps {
    chunk: pump_chunk,
    close: pump_close,
    error: pump_error,
};

/// Advance the pump: read chunks into the batch for as long as reads complete synchronously and
/// the batch isn't full, then write the batch. A batch written synchronously is followed by
/// more reads.
fn run(scope: &Scope<'_>, pump: &NativePump<'_>) {
    if pump.data().running || pump.data().finished {
        return;
    }
    pump.data_mut().running = true;
    loop {
        while can_read(pump) {
            pump.data_mut().reading = true;
            let reader = pump.data().reader.get(scope);
            if native_reader_read(scope, reader, PUMP_STEPS, pump.as_object()).is_err() {
                let message = ExnThrown::capture(scope).to_string();
                let mut data = pump.data_mut();
                data.reading = false;
                data.end = Some(PumpEnd::Failed(message));
            }
            if pump.data().reading {
                break;
            }
        }
        if !flush(scope, pump) {
            break;
        }
    }
    pump.data_mut().running = false;
}

/// Whether the pump may issue another read: it hasn't finished, the stream hasn't ended, no read
/// or write is outstanding, and the batch isn't full.
fn can_read(pump: &NativePump<'_>) -> bool {
    let data = pump.data();
    !data.finished
        && data.end.is_none()
        && !data.reading
        && data
            .sink
            .as_ref()
            .is_some_and(|sink| !sink.is_writing() && !sink.is_full())
}

/// Write the batch if a read has to wait, the batch is full, or the stream has ended. Finish the
/// pump once the stream has ended and the batch is written. Returns whether [`run`] should read
/// on, which is the case if the batch was written synchronously.
fn flush(scope: &Scope<'_>, pump: &NativePump<'_>) -> bool {
    if pump.data().finished {
        return false;
    }
    let Some((writing, empty, full)) =
        pump.with_sink(|sink| (sink.is_writing(), sink.is_empty(), sink.is_full()))
    else {
        return false;
    };
    if writing {
        return false;
    }
    let ready = full || pump.data().reading || pump.data().end.is_some();
    if !empty && ready {
        if let Some(Err(message)) = pump.with_sink(|sink| sink.check(scope)) {
            pump.data_mut().end = Some(PumpEnd::Failed(message));
        }
        // `check` can empty the batch, which then isn't written.
        if pump.with_sink(|sink| sink.is_empty()) == Some(false) {
            if pump.with_sink(|sink| sink.try_write()) == Some(true) {
                return true;
            }
            spawn_write(scope, pump);
            return false;
        }
    }
    if pump.data().end.is_some() && pump.with_sink(|sink| sink.is_empty()) == Some(true) {
        finish(scope, pump);
    }
    false
}

/// Write the batch with [`PumpSink::write`], and continue the pump once the write completes.
fn spawn_write(scope: &Scope<'_>, pump: &NativePump<'_>) {
    let Ok(promise) = Promise::new_pending(scope) else {
        let message = ExnThrown::capture(scope).to_string();
        pump.data_mut().end = Some(PumpEnd::Failed(message));
        finish(scope, pump);
        return;
    };
    let Some((write, poll_now)) = pump.with_sink(|sink| (sink.write(), sink.poll_write_at_once()))
    else {
        return;
    };
    let pump = RootedHeap::new(*pump);
    let future = async move {
        let sink = write.await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            write_done(scope, &pump.get(scope), sink);
            Ok(HandleValue::undefined())
        }))
    };
    let future = PromiseFuture::new(future);
    if poll_now && core_runtime::event_loop::has_active_event_loop() {
        promise.spawn_polled(future);
    } else {
        promise.spawn(future);
    }
}

/// Continue the pump after a write. `sink` is `None` if the receiver went away.
fn write_done(scope: &Scope<'_>, pump: &NativePump<'_>, sink: Option<Box<dyn Any>>) {
    let Some(sink) = sink else {
        if !pump.data().finished {
            pump.data_mut().end = Some(PumpEnd::Stopped(RECEIVER_DROPPED.to_string()));
            finish(scope, pump);
        }
        return;
    };
    pump.with_sink(|slot| slot.restore(sink));
    if pump.data().finished {
        // The pump was stopped during the write.
        let end = pump.data().end.clone().expect("a finished pump has an end");
        pump.with_sink(|slot| slot.finish(scope, &end));
        return;
    }
    run(scope, pump);
}

/// Finish the pump with its `end`: drop the chunks not yet written, finish the sink unless a write
/// is in flight, print a failure, cancel the stream unless it closed, and call the `on_finish`
/// callback.
fn finish(scope: &Scope<'_>, pump: &NativePump<'_>) {
    let end = {
        let mut data = pump.data_mut();
        data.finished = true;
        data.end.clone().expect("a finishing pump has an end")
    };
    pump.with_sink(|sink| sink.finish(scope, &end));
    if let PumpEnd::Failed(message) = &end {
        eprintln!("a stream's source ended the stream early: {message}");
    }
    if let PumpEnd::Failed(message) | PumpEnd::Stopped(message) = &end {
        let _ = TypeError(message.clone()).throw(scope);
        let reason = js::exception::take_pending_or_undefined(scope);
        let reader = pump.data().reader.get(scope);
        let cancelled = native_reader_cancel(scope, reader, reason);
        // The cancellation rejects for a stream that errored, and nothing observes it.
        let _ = cancelled.set_any_is_handled(scope);
    }
    let on_finish = pump.data_mut().on_finish.take();
    if let Some(on_finish) = on_finish {
        on_finish();
    }
}

fn pump_chunk(
    scope: &Scope<'_>,
    payload: Object<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let pump = payload
        .cast::<NativePump>()
        .expect("payload is a NativePump");
    pump.data_mut().reading = false;
    if pump.data().finished {
        return Ok(());
    }
    // A chunk that arrives after the stream ended early isn't written.
    if pump.data().end.is_none() {
        if let Some(Err(message)) = pump.with_sink(|sink| sink.append(scope, chunk)) {
            pump.data_mut().end = Some(PumpEnd::Failed(message));
        }
    }
    run(scope, &pump);
    Ok(())
}

fn pump_close(scope: &Scope<'_>, payload: Object<'_>) -> Result<(), ExnThrown> {
    let pump = payload
        .cast::<NativePump>()
        .expect("payload is a NativePump");
    pump.data_mut().reading = false;
    pump.data_mut().end.get_or_insert(PumpEnd::Closed);
    run(scope, &pump);
    Ok(())
}

fn pump_error(
    scope: &Scope<'_>,
    payload: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let pump = payload
        .cast::<NativePump>()
        .expect("payload is a NativePump");
    pump.data_mut().reading = false;
    if let Some(message) = pump.with_sink(|sink| sink.describe_source_error(scope, error)) {
        pump.data_mut().end.get_or_insert(PumpEnd::Failed(message));
    }
    run(scope, &pump);
    Ok(())
}
