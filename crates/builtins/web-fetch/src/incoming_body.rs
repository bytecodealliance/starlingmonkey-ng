// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Streaming an incoming body straight from the host.
//!
//! Used as the underlying source for a `body` [`ReadableStream`], or directly in
//! [`consume_host_body`] for `text`/`json`/`arrayBuffer`/`bytes`.

use crate::algorithms::{convert_owned_bytes_to_js_value, Body, BodySource, ConsumeType};
use crate::body_mixin::BodyStorage;
use core_runtime::jsclass;
use core_runtime::jsmethods;
use js::conversion::FromJSVal;
use js::error::{ExnThrown, TypeError};
use js::function::cast_payload;
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt, RootedHeap};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{value, Function, Promise};
use platform::http::IncomingBody;
use web_streams::readable::default_controller::ReadableStreamDefaultControllerImpl;
use web_streams::readable::default_reader::DefaultReader;
use web_streams::readable::readable_stream::{ReadableStream, ReadableStreamImpl};
use web_streams::readable::ReadableStreamDefaultController;
use web_streams::AlgorithmArg;

/// Back a new external `ArrayBuffer` with `bytes`, zero-copy when the chunk is
/// uniquely owned. A shared chunk (another `Bytes` clone alive, e.g. a body
/// source retained elsewhere) is copied instead: JS gets write access to the
/// buffer, so it must not alias memory other owners can still read.
pub(crate) fn array_buffer_from_body_bytes<'r>(
    scope: &'r Scope<'_>,
    bytes: platform::http::BodyBytes,
) -> Result<js::ArrayBuffer<'r>, ExnThrown> {
    /// Uniquely-owned host bytes.
    struct UniqueBytes(bytes::BytesMut);
    // SAFETY: `BytesMut` guarantees exclusive ownership of its range, and the
    // storage is heap-backed (stable behind the moved handle).
    unsafe impl js::typedarray::ExternalBytes for UniqueBytes {
        fn as_mut_slice(&mut self) -> &mut [u8] {
            &mut self.0
        }
    }

    match bytes.try_into_mut() {
        Ok(unique) => js::ArrayBuffer::from_external(scope, UniqueBytes(unique)),
        Err(shared) => js::ArrayBuffer::from_external(scope, shared.to_vec()),
    }
}

/// `consume body` for a host-backed body: read the whole body from the host and
/// settle the returned promise with the converted value, bypassing `ReadableStream`.
pub(crate) fn consume_host_body<'r>(
    scope: &'r Scope<'_>,
    host_body: IncomingBody,
    conversion: ConsumeType,
) -> Result<Promise<'r>, ExnThrown> {
    let promise = Promise::new_pending(scope)?;
    // Only an active event loop drives the host read.
    if core_runtime::event_loop::reject_without_event_loop(scope, &promise, "reading a body")? {
        return Ok(promise);
    }
    let future = async move {
        match host_body.read_all().await {
            Ok(bytes) => PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
                // The whole body is owned here, so arrayBuffer()/bytes() back the result with
                // it directly (no copy when aligned).
                convert_owned_bytes_to_js_value(scope, bytes, conversion)
            })),
            Err(error) => PromiseOutcome::Reject(format!("Failed to read response body: {error}")),
        }
    };
    promise.spawn(PromiseFuture::new(future));
    Ok(promise)
}

/// Native byte source for a host-backed body's `.body` stream: holds the host
/// body and hands out one chunk per pull. `current_pull` is the promise of the
/// in-flight chunk read, kept so an abort can cancel that read (dropping it
/// closes the host connection).
///
/// A pull can be deferred: if this body feeds an identity transform that has not
/// been read yet, the host body is left whole. If the TS's readable end ends up being
/// used as an outgoing body (or otherwise as a stream passed to the host), this
/// body's stream can be used directly. See [`host_pull`].
#[jsclass(hidden)]
pub struct HostBodySource {
    #[no_trace]
    host_body: Option<IncomingBody>,
    current_pull: Option<Heap<js::promise::Promise>>,
    #[no_trace]
    pulled: bool,
    /// The `.body` stream this source backs, for asking whether anything
    /// downstream wants bytes yet.
    stream: Option<Heap<ReadableStreamImpl>>,
    /// A deferred pull's promise, resolved once its deferred read has been
    /// delivered — or when the body goes elsewhere and it never will be.
    deferred_pull: Option<Heap<js::promise::Promise>>,
    /// The controller a deferred pull will deliver to.
    deferred_controller: Option<Heap<ReadableStreamDefaultControllerImpl>>,
}

#[jsmethods]
impl HostBodySource {
    fn new(host_body: IncomingBody) -> Self {
        HostBodySourceImpl {
            host_body: Some(host_body),
            current_pull: None,
            pulled: false,
            stream: None,
            deferred_pull: None,
            deferred_controller: None,
        }
    }
}

impl HostBodySource<'_> {
    /// Take the host body if it has not started being read.
    ///
    /// Returns `None` once a read operation has happened or the body has already been taken.
    pub(crate) fn take_host_body(&self, scope: &Scope<'_>) -> Option<IncomingBody> {
        if self.data().pulled || self.data().current_pull.is_some() {
            return None;
        }
        let host_body = self.data_mut().host_body.take()?;
        // The body will now be used by a sink directly, obviating the deferred pull.
        self.settle_deferred_pull(scope, CloseStream::Yes);
        Some(host_body)
    }

    /// Abort the body: cancel the in-flight chunk read (if any) — which drops the
    /// host body and closes the connection — and drop any idle host body.
    pub(crate) fn abort(&self, scope: &Scope<'_>) {
        let pull = self.data_mut().current_pull.take_rooted(scope);
        if let Some(pull) = pull {
            js::promise::cancel_pending_future(pull);
        }
        // A deferred pull has no host read to cancel, but its promise must still be
        // settled, or the stream is left believing a pull is still running. The
        // stream is not closed here: the caller errors it instead.
        self.settle_deferred_pull(scope, CloseStream::No);
        self.data_mut().host_body = None;
    }

    /// Settle a deferred pull, if there is one, so the stream stops waiting on a
    /// read that is no longer coming.
    fn settle_deferred_pull(&self, scope: &Scope<'_>, close: CloseStream) {
        let Some(pull) = self.data_mut().deferred_pull.take_rooted(scope) else {
            return;
        };
        let controller = self.data_mut().deferred_controller.take_rooted(scope);
        if let (CloseStream::Yes, Some(controller)) = (close, controller) {
            let _ = controller.close(scope);
        }
        let _ = pull.resolve(scope, HandleValue::undefined());
    }
}

/// Whether settling a deferred pull should also close the stream it was pulling
/// for: it should when the body went elsewhere, but not when the stream is
/// already being torn down by a cancel or abort.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloseStream {
    Yes,
    No,
}

/// A `Request` or `Response` carrying a body still unread on the host.
///
/// Both keep a [`BodyStorage`], so the operations over it ([`materialize_host_body`],
/// [`clone_body_onto`], [`crate::outgoing_body::consume_outgoing_body`]) are written here once
/// rather than once per interface. The provided methods each take the storage borrow only for
/// their own duration, because a caller must not hold a borrow of the object's data across
/// [`host_body_stream`]: allocating the stream can trigger a GC, whose trace of this object
/// reads the same data.
pub(crate) trait HostBackedBodyOwner {
    /// Run `f` on this object's body storage. `f` must not allocate GC things.
    fn with_body<R>(&self, f: impl FnOnce(&mut BodyStorage) -> R) -> R;

    /// This object.
    fn object(&self) -> js::Object<'_>;

    /// Attribute the body's byte source to this object for the GC, replacing the previous
    /// amount. Called wherever the byte source is set, taken, or dropped.
    fn sync_body_accounting(&self) {
        let object = self.object();
        self.with_body(|body| body.sync_accounting(object));
    }

    /// Take the host body, but only while it is still unread — once a `.body`
    /// stream exists it owns the body, and the caller has nothing to do.
    fn take_unread_host_body(&self) -> Option<IncomingBody> {
        self.with_body(|body| body.stream.is_none().then(|| body.host.take())?)
    }

    /// The body record, if this object has a body. A copy: a [`BodySource::Bytes`] it carries is
    /// refcounted, so holding the copy holds the buffer.
    fn body_record(&self) -> Option<Body> {
        self.with_body(|body| body.record.clone())
    }

    /// Take the body's in-memory bytes, leaving it marked as read. The emptied source stays a byte
    /// source, so a `.body` asked for later still materializes the consumed stream it would have.
    fn take_byte_source(&self) -> Option<bytes::Bytes> {
        let taken = self.with_body(|body| {
            let record = body.record.as_mut()?;
            let BodySource::Bytes(bytes) = &mut record.source else {
                return None;
            };
            record.source_disturbed = true;
            Some(std::mem::take(bytes))
        })?;
        self.sync_body_accounting();
        Some(taken)
    }

    /// The `.body` stream, once materialized.
    fn body_stream<'r>(&self, scope: &'r Scope<'_>) -> Option<ReadableStream<'r>> {
        self.with_body(|body| body.stream.get(scope))
    }

    /// Take the reader acquired on the `.body` stream when the body was committed to being sent,
    /// if any. Sending reads the stream through it.
    fn take_send_reader<'r>(&self, _scope: &'r Scope<'_>) -> Option<DefaultReader<'r>> {
        None
    }

    /// Take the host body whether or not a stream exists (the outgoing path,
    /// which hands it straight to the transport).
    fn take_host_body(&self) -> Option<IncomingBody> {
        self.with_body(|body| body.host.take())
    }

    /// Replace the `.body` stream, and drop the byte and host sources it now
    /// supersedes — used after teeing, when the bytes live in the tee branches.
    fn replace_body_stream_after_tee(&self, stream: ReadableStream<'_>) {
        self.with_body(|body| {
            debug_assert!(body.stream.is_some());
            body.stream = Some(Heap::from(stream));
            if let Some(record) = body.record.as_mut() {
                record.source = BodySource::Null;
            }
            body.host = None;
            body.source = None;
        });
        self.sync_body_accounting();
    }
}

/// `clone a body` for a host-backed owner: materialize the stream if the body is
/// still on the host, tee it, leave branch one behind on `self`, and return the
/// cloned body record with branch two for the caller to install on the clone.
///
/// Shared by `Request.clone()` and `Response.clone()`, whose only real
/// difference is which object they build at the end.
pub(crate) fn clone_body_onto<'r>(
    scope: &'r Scope<'_>,
    object: &impl HostBackedBodyOwner,
) -> Result<(Option<crate::algorithms::Body>, Option<ReadableStream<'r>>), ExnThrown> {
    // A host-backed body has no materialized stream yet; bring it into existence (as the `.body`
    // getter does) so `clone a body` has a stream to tee.
    materialize_host_body(scope, object)?;
    // With no stream (an in-memory byte source, or no body at all) there is nothing to tee — copy
    // the body record and let each side materialize its own stream from its own byte copy.
    let Some(stream) = object.body_stream(scope) else {
        return Ok((object.body_record(), None));
    };
    let (out2, cloned_body) = crate::algorithms::clone_a_body(scope, object, &stream)?;
    Ok((Some(cloned_body), Some(out2)))
}

/// Bring a host-backed body's `.body` stream into existence, so `.body` has a
/// stream to hand out and `clone a body` has one to tee. A no-op for a body that
/// is not host-backed, or whose stream already exists.
pub(crate) fn materialize_host_body(
    scope: &Scope<'_>,
    object: &impl HostBackedBodyOwner,
) -> Result<(), ExnThrown> {
    let Some(host_body) = object.take_unread_host_body() else {
        return Ok(());
    };
    let (stream, source) = host_body_stream(scope, host_body)?;
    object.with_body(|body| {
        debug_assert!(body.stream.is_none());
        body.stream = Some(Heap::from(stream));
        body.source = Some(Heap::from(source));
    });
    Ok(())
}

/// Build the `.body` `ReadableStream` for a host-backed body: its underlying
/// source reads the host body one chunk per pull and enqueues it, closing at end
/// of body, so streaming or unbounded bodies are delivered progressively.
pub(crate) fn host_body_stream<'r>(
    scope: &'r Scope<'_>,
    host_body: IncomingBody,
) -> Result<(ReadableStream<'r>, HostBodySource<'r>), ExnThrown> {
    let state = HostBodySource::new(scope, host_body)?;
    let pull = AlgorithmArg::native(scope, host_pull, state)?;
    let cancel = AlgorithmArg::native(scope, host_cancel, state)?;
    // Record the source so the stream can be recognized as host-backed via
    // `ReadableStream::native_source`.
    let stream = ReadableStream::new_native(scope, state, pull, cancel)?;
    // Backlink the stream, so a pull can ask what it is feeding.
    state.data_mut().stream = Some(Heap::from(stream));
    Ok((stream, state))
}

/// Underlying-source `pull`: read the next host chunk and enqueue it (or close at
/// end of body / error on a read failure). Returns a promise resolved once the
/// chunk has been handled.
///
/// Makes use of the deferral machinery described in [`ReadableStream::deferred_demand`]
/// to avoid needlessly reading from a host stream. If, later on, the body used as an
/// outgoing stream, this allows it to be forwarded wholesale, skipping all handling
/// inside StarlingMonkey itself entirely.
fn host_pull(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<HostBodySource>(scope, payload);
    let promise = Promise::new_pending(scope)?;
    // Only an active event loop drives the host read. A rejected pull errors the stream.
    if core_runtime::event_loop::reject_without_event_loop(scope, &promise, "reading a body")? {
        return Ok(promise.as_value());
    }
    let controller = ReadableStreamDefaultController::from_jsval_throwing(scope, args.get(0), ())?;
    let stream = state.data().stream.get(scope);
    if let Some(demand) = stream.and_then(|stream| stream.deferred_demand(scope)) {
        state.data_mut().deferred_pull = Some(Heap::from(promise));
        state.data_mut().deferred_controller = Some(Heap::from(controller));
        let resume = Function::new_callback(scope, c"", 0, host_pull_resume, state)?;
        demand.add_reactions_ignoring_unhandled_rejection(scope, Some(*resume), None)?;
        return Ok(promise.as_value());
    }
    start_host_read(scope, state, controller, promise)?;
    Ok(promise.as_value())
}

/// Demand arrived for a deferred pull: perform the host read it deferred.
fn host_pull_resume(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<HostBodySource>(scope, payload);
    let controller = state.data_mut().deferred_controller.take_rooted(scope);
    let pull = state.data_mut().deferred_pull.take_rooted(scope);
    // Already settled: the body was claimed for direct forwarding, cancelled, or aborted while
    // this pull sat deferred.
    let (Some(controller), Some(promise)) = (controller, pull) else {
        return Ok(value::undefined());
    };
    // Demand can arrive where no event loop is active to drive the host read.
    if core_runtime::event_loop::reject_without_event_loop(scope, &promise, "reading a body")? {
        return Ok(value::undefined());
    }
    start_host_read(scope, state, controller, promise)?;
    Ok(value::undefined())
}

/// Read the next host chunk into `controller`, resolving `promise` once the chunk
/// has been handled. Shared by an immediate pull and a deferred one resuming.
fn start_host_read(
    scope: &Scope<'_>,
    state: HostBodySource<'_>,
    controller: ReadableStreamDefaultController<'_>,
    promise: Promise<'_>,
) -> Result<(), ExnThrown> {
    state.data_mut().pulled = true;
    let Some(host_body) = state.data_mut().host_body.take() else {
        // The body is gone: cancelled, or claimed for direct forwarding. Close the stream so
        // a reader does not hang waiting for a chunk that will never arrive.
        controller.close(scope)?;
        promise.resolve(scope, HandleValue::undefined())?;
        return Ok(());
    };
    // Remember the in-flight read so an abort can cancel it.
    state.data_mut().current_pull = Some(Heap::from(promise));
    // The controller and state are used after the host read, keep them rooted across it.
    let controller = RootedHeap::new(controller);
    let state = RootedHeap::new(state);
    let future = async move {
        let mut host_body = host_body;
        let result = host_body.next_chunk().await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            deliver_chunk(
                scope,
                controller.get(scope),
                state.get(scope),
                result,
                host_body,
            )
            .map(|_| HandleValue::undefined())
        }))
    };
    promise.spawn(PromiseFuture::new(future));
    Ok(())
}

/// Enqueue `result`'s chunk into the controller (keeping the host body for the
/// next pull), close at end of body, or error the stream on a read failure.
fn deliver_chunk(
    scope: &Scope<'_>,
    controller: ReadableStreamDefaultController,
    state: HostBodySource,
    result: Result<Option<platform::http::BodyBytes>, platform::http::Error>,
    host_body: IncomingBody,
) -> Result<(), ExnThrown> {
    // This pull has completed and can no longer be canceled.
    state.data_mut().current_pull = None;
    match result {
        Ok(Some(bytes)) => {
            // More may follow: keep the host body for the next pull.
            state.data_mut().host_body = Some(host_body);
            // Hand the host bytes to the engine without copying when they are uniquely owned
            // and aligned (a fresh host read buffer is both); shared or unaligned chunks are
            // copied.
            let len = bytes.len();
            let buffer = array_buffer_from_body_bytes(scope, bytes)?;
            let chunk = js::typedarray::construct_view(
                scope,
                js::typedarray::ViewKind::Uint8,
                buffer,
                0,
                len,
            )?;
            let chunk_value = scope.root_value(chunk.as_value());
            // Deliver via the controller abstract operation, not the
            // author-patchable `…prototype.enqueue` — a page must not be able to
            // observe or corrupt the network body's bytes.
            controller.enqueue(scope, chunk_value)?;
        }
        // End of body (host body dropped): close the stream.
        Ok(None) => {
            controller.close(scope)?;
        }
        // Read failure (host body dropped): error the stream.
        Err(error) => {
            let _ = TypeError(format!("Failed to read response body: {error}")).throw(scope);
            let reason = js::exception::take_pending(scope).ok_or(ExnThrown)?;
            controller.error(scope, Some(reason))?;
        }
    }
    Ok(())
}

/// Underlying-source `cancel`: drop the host body, ending the host read.
fn host_cancel(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<HostBodySource>(scope, payload);
    state.data_mut().host_body = None;
    // The stream is being torn down, so a deferred pull's demand will never arrive.
    state.settle_deferred_pull(scope, CloseStream::No);
    Ok(value::undefined())
}
