// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>

use std::collections::VecDeque;

use super::algorithms;
use crate::queuing::{QueueWithSizes, ValueWithSize};
use crate::writable::writable_stream::{WritableStreamImpl, WritableStreamState};
use crate::writable::WritableStream;
use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use web_globals::signals::abort_controller::{AbortController, AbortControllerImpl};
use web_globals::signals::AbortSignal;

/// <https://streams.spec.whatwg.org/#ws-default-controller-class>
#[webidl_interface]
pub struct WritableStreamDefaultController {
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-abortalgorithm>
    /// A promise-returning algorithm, taking one argument (the abort reason), which communicates a
    /// requested abort to the underlying sink
    pub(crate) abort_algorithm: crate::support::Algorithm,
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-abortcontroller>
    /// An AbortController that can be used to abort the pending write or close operation when the
    /// stream is aborted.
    pub(crate) abort_controller: Heap<AbortControllerImpl>,
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-closealgorithm>
    /// A promise-returning algorithm which communicates a requested close to the underlying sink
    pub(crate) close_algorithm: crate::support::Algorithm,
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-queue>
    /// A list representing the stream’s internal queue of chunks
    pub(crate) queue: VecDeque<ValueWithSize>,
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-queuetotalsize>
    /// The total size of all the chunks stored in [[queue]] (see § 8.1 Queue-with-sizes)
    #[no_trace]
    pub(crate) queue_total_size: f64,
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-started>
    /// A boolean flag indicating whether the underlying sink has finished starting
    pub(crate) started: bool,
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-strategyhwm>
    /// A number supplied by the creator of the stream as part of the stream’s queuing strategy,
    /// indicating the point at which the stream will apply backpressure to its underlying sink
    pub(crate) strategy_hwm: f64,
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-strategysizealgorithm>
    /// An algorithm to calculate the size of enqueued chunks, as part of the stream’s queuing
    /// strategy
    pub(crate) strategy_size_algorithm: Heap<Value>,
    /// The `this` value JS write/close/abort algorithms are invoked with: the
    /// underlying sink. Not a spec slot, see the readable controller's
    /// `algorithm_receiver`.
    pub(crate) algorithm_receiver: Heap<Value>,
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-stream>
    /// The WritableStream instance controlled
    pub(crate) stream: Heap<WritableStreamImpl>,
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-writealgorithm>
    /// A promise-returning algorithm, taking one argument (the chunk to write), which writes data to
    /// the underlying sink
    pub(crate) write_algorithm: crate::support::Algorithm,
    /// The sink-write-reaction callbacks (`WritableStreamDefaultControllerProcessWrite`
    /// steps 4-5; payload = this controller), created on the first write and
    /// reused for every subsequent chunk. `None` until the first write.
    pub(crate) write_fulfilled_fn: Option<Heap<js::function::Function>>,
    pub(crate) write_rejected_fn: Option<Heap<js::function::Function>>,
}

#[webidl_methods]
impl WritableStreamDefaultController {
    fn new(&self, scope: &Scope<'_>, stream: WritableStream<'_>) -> Result<(), ExnThrown> {
        let data = self.data_mut();
        data.stream.set(stream);
        data.abort_controller
            .set(AbortController::new(scope).expect("AbortController can only fail due to OOM"));
        Ok(())
    }

    /// <https://streams.spec.whatwg.org/#ws-default-controller-signal>
    #[getter]
    fn signal<'r>(&self, scope: &'r Scope<'_>) -> AbortSignal<'r> {
        // WebIDL: AbortSignal
        // Step 1: Return `this`.`[[abortController]]`’s `signal`.
        let abort_controller: AbortController<'_> = self.data().abort_controller.get(scope);
        abort_controller.signal(scope)
    }

    /// <https://streams.spec.whatwg.org/#ws-default-controller-error>
    #[method]
    pub fn error(&self, scope: &Scope<'_>, e: Option<HandleValue<'_>>) -> Result<(), ExnThrown> {
        // Step 1: Let _state_ be `this`.`[[stream]]`.`[[state]]`.
        let stream: WritableStream<'_> = self.stream(scope);
        let state = stream.data().state;
        // Step 2: If _state_ is not "`writable`", return.
        if state != WritableStreamState::Writable {
            return Ok(());
        }
        // Step 3: Perform ! `WritableStreamDefaultControllerError`(`this`, _e_).
        let e = e.unwrap_or(HandleValue::undefined());
        algorithms::writable_stream_default_controller_error(scope, self, e);
        Ok(())
    }

    pub(crate) fn stream<'r>(&self, scope: &'r Scope<'_>) -> WritableStream<'r> {
        self.data().stream.get(scope)
    }
}

impl QueueWithSizes for WritableStreamDefaultControllerImpl {
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
