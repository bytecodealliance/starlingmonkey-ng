// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>

use std::collections::VecDeque;

use super::algorithms;
use super::readable_stream::ReadableStreamImpl;
use crate::queuing::{QueueWithSizes, ValueWithSize};
use crate::readable::ReadableStream;
use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;

/// <https://streams.spec.whatwg.org/#rs-default-controller-class>
#[webidl_interface]
pub struct ReadableStreamDefaultController {
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-cancelalgorithm>
    /// A promise-returning algorithm, taking one argument (the cancel reason), which communicates a
    /// requested cancelation to the underlying source
    pub(crate) cancel_algorithm: crate::support::Algorithm,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-closerequested>
    /// A boolean flag indicating whether the stream has been closed by its underlying source, but
    /// still has chunks in its internal queue that have not yet been read
    pub(crate) close_requested: bool,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-pullagain>
    /// A boolean flag set to true if the stream’s mechanisms requested a call to the underlying
    /// source’s pull algorithm to pull more data, but the pull could not yet be done since a
    /// previous call is still executing
    pub(crate) pull_again: bool,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-pullalgorithm>
    /// A promise-returning algorithm that pulls data from the underlying source
    pub(crate) pull_algorithm: crate::support::Algorithm,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-pulling>
    /// A boolean flag set to true while the underlying source’s pull algorithm is executing and
    /// the returned promise has not yet fulfilled, used to prevent reentrant calls
    pub(crate) pulling: bool,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-queue>
    /// A list representing the stream’s internal queue of chunks
    pub(crate) queue: VecDeque<ValueWithSize>,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-queuetotalsize>
    /// The total size of all the chunks stored in [[queue]] (see § 8.1 Queue-with-sizes)
    #[no_trace]
    pub(crate) queue_total_size: f64,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-started>
    /// A boolean flag indicating whether the underlying source has finished starting
    pub(crate) started: bool,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-strategyhwm>
    /// A number supplied to the constructor as part of the stream’s queuing strategy, indicating
    /// the point at which the stream will apply backpressure to its underlying source
    pub(crate) strategy_hwm: f64,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-strategysizealgorithm>
    /// An algorithm to calculate the size of enqueued chunks, as part of the stream’s queuing
    /// strategy
    pub(crate) strategy_size_algorithm: Heap<Value>,
    /// The `this` value the start/pull/cancel algorithms are invoked with.
    ///
    /// Not a spec slot: the spec models pull/cancel as algorithms closing over
    /// the underlying source. We store a JS algorithm's callback plus its
    /// receiver, the underlying source object. Native algorithms don't use it.
    pub(crate) algorithm_receiver: Heap<Value>,
    /// <https://streams.spec.whatwg.org/#ReadableStreamDefaultController-stream>
    /// The ReadableStream instance controlled
    pub(crate) stream: Heap<ReadableStreamImpl>,
    /// The pull-reaction callbacks (`DefaultControllerCallPullIfNeeded` steps
    /// 7-8; payload = this controller), created on the first pull.
    pub(crate) pull_fulfilled_fn: Option<Heap<js::function::Function>>,
    pub(crate) pull_rejected_fn: Option<Heap<js::function::Function>>,
}

#[webidl_methods]
impl ReadableStreamDefaultController {
    fn new(stream: ReadableStream<'_>) -> Self {
        ReadableStreamDefaultControllerImpl {
            stream: Heap::from(stream),
            ..Default::default()
        }
    }

    /// <https://streams.spec.whatwg.org/#rs-default-controller-desired-size>
    #[getter]
    pub fn desired_size(&self, scope: &Scope<'_>) -> Option<f64> {
        // Step 1: Return ! `DefaultControllerGetDesiredSize`(`this`).
        algorithms::readable_stream_default_controller_get_desired_size(scope, self)
    }

    /// <https://streams.spec.whatwg.org/#rs-default-controller-close>
    #[method]
    pub fn close(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: If ! `DefaultControllerCanCloseOrEnqueue`(`this`) is false, throw a
        //         ``TypeError`` exception.
        if !algorithms::readable_stream_default_controller_can_close_or_enqueue(scope, self) {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamDefaultController",
                c"The stream is not in a state that permits close",
            ));
        }
        // Step 2: Perform ! `DefaultControllerClose`(`this`).
        algorithms::readable_stream_default_controller_close(scope, self);
        Ok(())
    }

    /// <https://streams.spec.whatwg.org/#rs-default-controller-enqueue>
    #[method(length = 0)]
    pub fn enqueue(&self, scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<(), ExnThrown> {
        // Step 1: If ! `DefaultControllerCanCloseOrEnqueue`(`this`) is false, throw a
        //         ``TypeError`` exception.
        if !algorithms::readable_stream_default_controller_can_close_or_enqueue(scope, self) {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamDefaultController",
                c"The stream is not in a state that permits enqueue",
            ));
        }
        // Step 2: Perform ? `DefaultControllerEnqueue`(`this`, _chunk_).
        algorithms::readable_stream_default_controller_enqueue(scope, self, chunk)
    }

    /// <https://streams.spec.whatwg.org/#rs-default-controller-error>
    #[method]
    pub fn error(&self, scope: &Scope<'_>, e: Option<HandleValue<'_>>) -> Result<(), ExnThrown> {
        // Step 1: Perform ! `DefaultControllerError`(`this`, _e_).
        let e = e.unwrap_or(HandleValue::undefined());
        algorithms::readable_stream_default_controller_error(scope, self, e);
        Ok(())
    }

    pub(crate) fn stream<'r>(&self, scope: &'r Scope<'_>) -> ReadableStream<'r> {
        self.data().stream.get(scope)
    }
}

impl QueueWithSizes for ReadableStreamDefaultControllerImpl {
    fn queue(&self) -> &VecDeque<ValueWithSize> {
        &self.queue
    }
    fn queue_mut(&mut self) -> &mut VecDeque<ValueWithSize> {
        &mut self.queue
    }
    fn queue_total_size(&self) -> f64 {
        self.queue_total_size
    }
    fn set_queue_total_size(&mut self, size: f64) {
        self.queue_total_size = size;
    }
}
