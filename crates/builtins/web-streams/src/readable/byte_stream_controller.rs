// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>

use std::collections::VecDeque;

use crate::readable::ReadableStream;

use super::byob_request::ReadableStreamBYOBRequest;
use super::byob_request::ReadableStreamBYOBRequestImpl;
use super::readable_stream::ReadableStreamImpl;
use super::readable_stream::ReadableStreamState;
use core_runtime::Traceable;
use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;

/// A readable byte stream queue entry: a contiguous region of an
/// `ArrayBuffer`.
///
/// <https://streams.spec.whatwg.org/#readable-byte-stream-queue-entry>
///
/// `must_root`: holds a `Heap`, so crown rejects holding it by value across an
/// allocation. It is traced inside the controller's `[[queue]]`; once
/// `pop_front`'d it must be consumed (extract its buffer, then drop it) before
/// anything allocates, or kept in a `RootedTraceableBox`.
#[js::must_root]
#[derive(Traceable, Default, js::ScopeRoot)]
pub(crate) struct ByteQueueEntry {
    /// The `ArrayBuffer` backing this entry's bytes.
    pub(crate) buffer: Heap<js::typedarray::ArrayBuffer>,
    /// The offset, in bytes, of this entry's region within `buffer`.
    #[no_trace]
    pub(crate) byte_offset: usize,
    /// The length, in bytes, of this entry's region.
    #[no_trace]
    pub(crate) byte_length: usize,
}

impl<'s> StackByteQueueEntry<'s> {
    /// Consume the rooted entry, returning its buffer and the region's
    /// `(byte_offset, byte_length)`.
    pub(crate) fn into_parts(self) -> (js::ArrayBuffer<'s>, usize, usize) {
        (self.buffer, self.byte_offset, self.byte_length)
    }
}

/// Which kind of reader a pull-into descriptor will be committed to.
///
/// <https://streams.spec.whatwg.org/#pull-into-descriptor-reader-type>
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) enum ReaderType {
    /// The descriptor was created for a default `read()` with auto-allocation.
    #[default]
    Default,
    /// The descriptor was created for a BYOB `read(view)`.
    Byob,
    /// The descriptor's reader has been released; bytes already filled are kept
    /// but no longer delivered to a request.
    None,
}

/// A pull-into descriptor: a destination region an underlying byte source is
/// being asked to fill, plus the bookkeeping to turn the filled bytes back into
/// a view of the requested type.
///
/// <https://streams.spec.whatwg.org/#pull-into-descriptor>
///
/// `must_root`: holds a `Heap` (the buffer), so crown rejects holding it by
/// value across an allocation. It is traced inside the controller's
/// `[[pendingPullIntos]]`; once moved out it must be consumed immediately or
/// kept in a `RootedTraceableBox`.
#[js::must_root]
#[derive(Traceable)]
pub(crate) struct PullIntoDescriptor {
    /// The `ArrayBuffer` being filled.
    pub(crate) buffer: Heap<js::typedarray::ArrayBuffer>,
    /// The byte length of `buffer`.
    #[no_trace]
    pub(crate) buffer_byte_length: usize,
    /// The offset into `buffer` at which the requested region begins.
    #[no_trace]
    pub(crate) byte_offset: usize,
    /// The byte length of the requested region.
    #[no_trace]
    pub(crate) byte_length: usize,
    /// How many bytes of the region have been filled so far.
    #[no_trace]
    pub(crate) bytes_filled: usize,
    /// The minimum number of bytes that must be filled before the descriptor is
    /// committed (the `min` of a BYOB `read`, scaled by element size).
    #[no_trace]
    pub(crate) minimum_fill: usize,
    /// The element size of the view to construct.
    #[no_trace]
    pub(crate) element_size: usize,
    /// The view kind to construct over the filled region (the spec's `view
    /// constructor`).
    #[no_trace]
    pub(crate) view_kind: js::typedarray::ViewKind,
    /// Which reader this descriptor will be committed to.
    #[no_trace]
    pub(crate) reader_type: ReaderType,
}

/// <https://streams.spec.whatwg.org/#rbs-controller-class>
#[webidl_interface]
pub struct ReadableByteStreamController {
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-autoallocatechunksize>
    /// A positive integer, when the automatic buffer allocation feature is enabled. In that case,
    /// this value specifies the size of buffer to allocate. It is undefined otherwise.
    pub(crate) auto_allocate_chunk_size: Option<u64>,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-ReadableStreamBYOBRequest>
    /// A ReadableStreamBYOBRequest instance representing the current BYOB pull request, or null if
    /// there are no pending requests
    pub(crate) byob_request: Option<Heap<ReadableStreamBYOBRequestImpl>>,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-cancelalgorithm>
    /// A promise-returning algorithm, taking one argument (the cancel reason), which communicates a
    /// requested cancelation to the underlying byte source
    pub(crate) cancel_algorithm: crate::support::Algorithm,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-closerequested>
    /// A boolean flag indicating whether the stream has been closed by its underlying byte source,
    /// but still has chunks in its internal queue that have not yet been read
    pub(crate) close_requested: bool,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-pullagain>
    /// A boolean flag set to true if the stream’s mechanisms requested a call to the underlying
    /// byte source’s pull algorithm to pull more data, but the pull could not yet be done since a
    /// previous call is still executing
    pub(crate) pull_again: bool,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-pullalgorithm>
    /// A promise-returning algorithm that pulls data from the underlying byte source
    pub(crate) pull_algorithm: crate::support::Algorithm,
    /// The `this` value with which JS pull and cancel algorithms are invoked: the
    /// underlying byte source object. Mirrors the default controller's
    /// `algorithm_receiver` slot.
    pub(crate) algorithm_receiver: Heap<Value>,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-pulling>
    /// A boolean flag set to true while the underlying byte source’s pull algorithm is executing
    /// and the returned promise has not yet fulfilled, used to prevent reentrant calls
    pub(crate) pulling: bool,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-pendingpullintos>
    /// A list of pull-into descriptors
    pub(crate) pending_pull_intos: VecDeque<PullIntoDescriptor>,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-queue>
    /// A list of readable byte stream queue entries representing the stream’s internal queue of
    /// chunks
    pub(crate) queue: VecDeque<ByteQueueEntry>,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-queuetotalsize>
    /// The total size, in bytes, of all the chunks stored in [[queue]] (see § 8.1 Queue-with-sizes)
    pub(crate) queue_total_size: f64,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-started>
    /// A boolean flag indicating whether the underlying byte source has finished starting
    pub(crate) started: bool,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-strategyhwm>
    /// A number supplied to the constructor as part of the stream’s queuing strategy, indicating
    /// the point at which the stream will apply backpressure to its underlying byte source
    pub(crate) strategy_hwm: f64,
    /// <https://streams.spec.whatwg.org/#ReadableByteStreamController-stream>
    /// The ReadableStream instance controlled
    pub(crate) stream: Heap<ReadableStreamImpl>,
    /// The pull-reaction callbacks (`ByteStreamControllerCallPullIfNeeded`
    /// steps 7-8; payload = this controller), created on the first pull and
    /// reused for every subsequent pull.
    pub(crate) pull_fulfilled_fn: Option<Heap<js::function::Function>>,
    pub(crate) pull_rejected_fn: Option<Heap<js::function::Function>>,
}

#[webidl_methods]
impl ReadableByteStreamController {
    /// <https://streams.spec.whatwg.org/#dom-ReadableByteStreamController-constructor>
    fn new(stream: ReadableStream<'_>) -> Self {
        ReadableByteStreamControllerImpl {
            stream: Heap::from(stream),
            ..Default::default()
        }
    }

    /// <https://streams.spec.whatwg.org/#rbs-controller-byob-request>
    #[getter]
    pub fn byob_request<'r>(&self, scope: &'r Scope<'_>) -> Option<ReadableStreamBYOBRequest<'r>> {
        // Step 1: Return ! `ByteStreamControllerGetBYOBRequest`(`this`).
        super::algorithms::readable_byte_stream_controller_get_byob_request(scope, self)
    }

    /// <https://streams.spec.whatwg.org/#rbs-controller-desired-size>
    #[getter]
    pub fn desired_size(&self, scope: &Scope<'_>) -> Option<f64> {
        // Step 1: Return ! `ByteStreamControllerGetDesiredSize`(`this`).
        super::algorithms::readable_byte_stream_controller_get_desired_size(scope, self)
    }

    /// <https://streams.spec.whatwg.org/#rbs-controller-close>
    #[method]
    pub fn close(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: If `this`.`[[closeRequested]]` is true, throw a ``TypeError`` exception.
        if self.data().close_requested {
            return Err(js::error::throw_type_error(
                scope,
                c"close() called on a controller that already requested close",
            ));
        }
        // Step 2: If `this`.`[[stream]]`.`[[state]]` is not "`readable`", throw a ``TypeError``
        //         exception.
        let stream = self.data().stream.get(scope);
        if stream.data().state != ReadableStreamState::Readable {
            return Err(js::error::throw_type_error(
                scope,
                c"close() called on a controller whose stream is not readable",
            ));
        }
        // Step 3: Perform ? `ByteStreamControllerClose`(`this`).
        super::algorithms::readable_byte_stream_controller_close(scope, self)
    }

    /// <https://streams.spec.whatwg.org/#rbs-controller-enqueue>
    #[method]
    pub fn enqueue(
        &self,
        scope: &Scope<'_>,
        view: js::ArrayBufferView<'_>,
    ) -> Result<(), ExnThrown> {
        // Step 1: If _chunk_.[[ByteLength]] is 0, throw a ``TypeError`` exception.
        if view.byte_length() == 0 {
            return Err(js::error::throw_type_error(
                scope,
                c"enqueue() view has a byte length of 0",
            ));
        }
        // Step 2: If _chunk_.[[ViewedArrayBuffer]].[[ByteLength]] is 0, throw a ``TypeError``
        //         exception.
        if view.viewed_buffer(scope)?.byte_length() == 0 {
            return Err(js::error::throw_type_error(
                scope,
                c"enqueue() view's buffer has a byte length of 0",
            ));
        }
        // Step 3: If `this`.`[[closeRequested]]` is true, throw a ``TypeError`` exception.
        if self.data().close_requested {
            return Err(js::error::throw_type_error(
                scope,
                c"enqueue() called on a controller that requested close",
            ));
        }
        // Step 4: If `this`.`[[stream]]`.`[[state]]` is not "`readable`", throw a ``TypeError``
        //         exception.
        let stream = self.data().stream.get(scope);
        if stream.data().state != ReadableStreamState::Readable {
            return Err(js::error::throw_type_error(
                scope,
                c"enqueue() called on a controller whose stream is not readable",
            ));
        }
        // Step 5: Return ? `ByteStreamControllerEnqueue`(`this`, _chunk_).
        super::algorithms::readable_byte_stream_controller_enqueue(scope, self, view)
    }

    /// <https://streams.spec.whatwg.org/#rbs-controller-error>
    #[method]
    pub fn error(&self, scope: &Scope<'_>, e: Option<HandleValue<'_>>) -> Result<(), ExnThrown> {
        // Step 1: Perform ! `ByteStreamControllerError`(`this`, _e_).
        let e = e.unwrap_or(HandleValue::undefined());
        super::algorithms::readable_byte_stream_controller_error(scope, self, e);
        Ok(())
    }

    pub(crate) fn stream<'r>(&self, scope: &'r Scope<'_>) -> ReadableStream<'r> {
        self.data().stream.get(scope)
    }
}
