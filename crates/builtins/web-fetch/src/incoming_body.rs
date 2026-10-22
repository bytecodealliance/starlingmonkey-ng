// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Streaming an incoming body straight from the host.
//!
//! Read by the [`NativeByteSource`] of a `body` [`ReadableStream`], or directly in
//! [`consume_host_body`] for `text`/`json`/`arrayBuffer`/`bytes`.

use crate::algorithms::{convert_owned_bytes_to_js_value, Body, BodySource, ConsumeType};
use crate::body_mixin::BodyStorage;
use js::error::ExnThrown;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::Promise;
use platform::http::IncomingBody;
use web_streams::readable::default_reader::DefaultReader;
use web_streams::readable::native_byte_source::NativeByteSource;
use web_streams::readable::readable_stream::ReadableStream;

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
            Err(error) => PromiseOutcome::Reject(format!("the body could not be read: {error}")),
        }
    };
    promise.spawn(PromiseFuture::new(future));
    Ok(promise)
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

/// Build the `.body` `ReadableStream` for a host-backed body: a byte stream that reads the host
/// body when a reader asks for data, so streaming or unbounded bodies are delivered
/// progressively.
pub(crate) fn host_body_stream<'r>(
    scope: &'r Scope<'_>,
    host_body: IncomingBody,
) -> Result<(ReadableStream<'r>, NativeByteSource<'r>), ExnThrown> {
    NativeByteSource::create_stream(scope, host_body)
}
