// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Standalone algorithms from <https://streams.spec.whatwg.org/>

use js::error::ExnThrown;
use js::exception::take_pending_or_undefined;
use js::function::cast_payload;
use js::function::EmptyArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::RootedTraceableBox;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleValue, OptionHeapExt, ToJSVal};
use js::{value, Function, Promise};
use web_globals::events::algorithms::ScriptStackState;
use web_globals::signals::abort_controller::AbortController;

use crate::algorithms::{
    dequeue_value, enqueue_value_with_size, invalid_state_error, is_non_negative_number,
    pair_parts, pair_payload, reset_queue, resolved_undefined_promise,
};
use crate::queuing::{QueueWithSizes, ValueWithSize};
use crate::support;
use crate::support::{Algorithm, AlgorithmArg};
use crate::writable::underlying_sink::UnderlyingSink;
use crate::writable::writable_stream::{
    PendingAbortRequest, PromiseSlot, WritableStream, WritableStreamImpl, WritableStreamState,
};
use crate::writable::WritableStreamDefaultController;
use crate::writable::WritableStreamDefaultWriter;

// ---------------------------------------------------------------------------
// Writable-side accessors and reaction callbacks.
// ---------------------------------------------------------------------------

/// The writable stream's `[[writer]]`, or `None` if unlocked.
fn writable_stream_writer<'r>(
    scope: &'r Scope<'_>,
    stream: &WritableStream<'_>,
) -> Option<WritableStreamDefaultWriter<'r>> {
    stream.data().writer.get(scope)
}

/// The writer's `[[stream]]`, or `None` once released.
pub(crate) fn writer_stream<'r>(
    scope: &'r Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
) -> Option<WritableStream<'r>> {
    writer.data().stream.get(scope)
}

/// `SetUpWritableStreamDefaultController` step 17.
fn ws_start_promise_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<WritableStreamDefaultController>(scope, payload);
    let stream = controller.stream(scope);
    debug_assert!(matches!(
        stream.data().state,
        WritableStreamState::Writable | WritableStreamState::Erroring
    ));
    controller.data_mut().started = true;
    writable_stream_default_controller_advance_queue_if_needed(scope, &controller);
    Ok(value::undefined())
}

/// `SetUpWritableStreamDefaultController` step 18.
fn ws_start_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<WritableStreamDefaultController>(scope, payload);
    let stream = controller.stream(scope);
    debug_assert!(matches!(
        stream.data().state,
        WritableStreamState::Writable | WritableStreamState::Erroring
    ));
    controller.data_mut().started = true;
    writable_stream_deal_with_rejection(scope, &stream, args.get(0));
    Ok(value::undefined())
}

/// `WritableStreamDefaultControllerProcessWrite` step 4.
fn ws_write_promise_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<WritableStreamDefaultController>(scope, payload);
    let stream = controller.stream(scope);
    writable_stream_finish_in_flight_write(scope, &stream);
    let state = stream.data().state;
    debug_assert!(matches!(
        state,
        WritableStreamState::Writable | WritableStreamState::Erroring
    ));
    dequeue_value(scope, &mut *controller.data_mut());
    if !writable_stream_close_queued_or_in_flight(&stream) && state == WritableStreamState::Writable
    {
        let backpressure = writable_stream_default_controller_get_backpressure(&controller);
        writable_stream_update_backpressure(scope, &stream, backpressure);
    }
    writable_stream_default_controller_advance_queue_if_needed(scope, &controller);
    Ok(value::undefined())
}

/// `WritableStreamDefaultControllerProcessWrite` step 5.
fn ws_write_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<WritableStreamDefaultController>(scope, payload);
    let stream = controller.stream(scope);
    if stream.data().state == WritableStreamState::Writable {
        writable_stream_default_controller_clear_algorithms(&controller);
    }
    writable_stream_finish_in_flight_write_with_error(scope, &stream, args.get(0));
    Ok(value::undefined())
}

/// `WritableStreamDefaultControllerProcessClose` step 7.
fn ws_close_promise_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<WritableStream>(scope, payload);
    writable_stream_finish_in_flight_close(scope, &stream);
    Ok(value::undefined())
}

/// `WritableStreamDefaultControllerProcessClose` step 8.
fn ws_close_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<WritableStream>(scope, payload);
    writable_stream_finish_in_flight_close_with_error(scope, &stream, args.get(0));
    Ok(value::undefined())
}

/// Pack `[stream, abortPromise]` into a JS array for the abort reactions, which
/// need both the stream and the pending abort request's promise (the latter is
/// no longer reachable from the stream once `[[pendingAbortRequest]]` is
/// cleared).
fn abort_reaction_payload<'r>(
    scope: &'r Scope<'_>,
    stream: &WritableStream<'r>,
    abort_promise: &Promise<'r>,
) -> Result<HandleValue<'r>, ExnThrown> {
    pair_payload(scope, stream, abort_promise)
}

/// Unpack the `[stream, abortPromise]` abort-reaction payload.
fn abort_reaction_parts<'r>(
    scope: &'r Scope<'_>,
    payload: HandleValue<'_>,
) -> (WritableStream<'r>, Promise<'r>) {
    let (stream_v, promise_v) = pair_parts(scope, payload);
    (
        cast_payload::<WritableStream>(scope, stream_v),
        cast_payload::<Promise>(scope, promise_v),
    )
}

/// `WritableStreamFinishErroring` step 13.
fn ws_abort_promise_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let (stream, abort_promise) = abort_reaction_parts(scope, payload);
    abort_promise.resolve(scope, HandleValue::undefined())?;
    writable_stream_reject_close_and_closed_promise_if_needed(scope, &stream);
    Ok(value::undefined())
}

/// `WritableStreamFinishErroring` step 14.
fn ws_abort_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let (stream, abort_promise) = abort_reaction_parts(scope, payload);
    abort_promise.reject(scope, args.get(0)).expect("reject");
    writable_stream_reject_close_and_closed_promise_if_needed(scope, &stream);
    Ok(value::undefined())
}

// ---------------------------------------------------------------------------
// Controller internal methods, native algorithm callbacks, and reactions.
// ---------------------------------------------------------------------------

/// <https://streams.spec.whatwg.org/#ws-default-controller-private-abort>
/// [[AbortSteps]](reason) implements the [[AbortSteps]] contract. It performs the following steps:
pub(crate) fn abort_steps<'r>(
    scope: &'r Scope<'_>,
    controller: &WritableStreamDefaultController<'_>,
    reason: HandleValue<'r>,
) -> Promise<'r> {
    // Step 1: Let _result_ be the result of performing `this`.`[[abortAlgorithm]]`, passing
    //         _reason_.
    let abort_algorithm = controller.data().abort_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let result = support::invoke_promise_algorithm(scope, abort_algorithm, receiver, &[reason]);
    // Step 2: Perform ! `WritableStreamDefaultControllerClearAlgorithms`(`this`).
    writable_stream_default_controller_clear_algorithms(controller);
    // Step 3: Return _result_.
    result
}

/// <https://streams.spec.whatwg.org/#ws-default-controller-private-error>
/// [[ErrorSteps]]() implements the [[ErrorSteps]] contract. It performs the following steps:
pub(crate) fn error_steps(controller: &WritableStreamDefaultController<'_>) {
    // Step 1: Perform ! `ResetQueue`(`this`).
    reset_queue(&mut *controller.data_mut());
}

/// <https://streams.spec.whatwg.org/#acquire-writable-stream-default-writer>
/// AcquireDefaultWriter(stream) performs the following steps:
pub(crate) fn acquire_writable_stream_default_writer<'r>(
    scope: &'r Scope<'_>,
    stream: &WritableStream<'_>,
) -> Result<WritableStreamDefaultWriter<'r>, ExnThrown> {
    // Step 1: Let _writer_ be a `new` ``WritableStreamDefaultWriter``.
    // Step 2: Perform ? `SetUpDefaultWriter`(_writer_, _stream_).
    //         The writer's constructor runs SetUp; creating via the factory performs both steps and
    //         propagates the setup's exception.
    let writer = WritableStreamDefaultWriter::new(scope, *stream)?;
    // Step 3: Return _writer_.
    Ok(writer)
}

/// <https://streams.spec.whatwg.org/#create-writable-stream>
/// CreateWritableStream(startAlgorithm, writeAlgorithm, closeAlgorithm, abortAlgorithm, highWaterMark, sizeAlgorithm) performs the following steps:
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_writable_stream<'r>(
    scope: &'r Scope<'_>,
    start_algorithm: AlgorithmArg<'r>,
    write_algorithm: AlgorithmArg<'r>,
    close_algorithm: AlgorithmArg<'r>,
    abort_algorithm: AlgorithmArg<'r>,
    high_water_mark: f64,
    size_algorithm: impl ToJSVal<'r>,
) -> Result<WritableStream<'r>, ExnThrown> {
    // Step 1: Assert: ! `IsNonNegativeNumber`(_highWaterMark_) is true.
    debug_assert!(is_non_negative_number(high_water_mark));
    // Step 2: Let _stream_ be a `new` ``WritableStream``.
    let stream = js::class::create_instance_with::<WritableStreamImpl>(scope, |_| {
        WritableStreamImpl::default()
    })?;
    // Step 3: Perform ! `InitializeWritableStream`(_stream_).
    initialize_writable_stream(&stream);
    // Step 4: Let _controller_ be a `new` ``WritableStreamDefaultController``.
    let controller = WritableStreamDefaultController::new(scope, stream)?;
    // Step 5: Perform ? `SetUpWritableStreamDefaultController`(_stream_, _controller_,
    //         _startAlgorithm_, _writeAlgorithm_, _closeAlgorithm_, _abortAlgorithm_,
    //         _highWaterMark_, _sizeAlgorithm_).
    //         The algorithms are native (no JS receiver), so `algorithm_receiver` is undefined.
    let receiver = HandleValue::undefined();
    set_up_writable_stream_default_controller(
        scope,
        &stream,
        &controller,
        start_algorithm,
        write_algorithm,
        close_algorithm,
        abort_algorithm,
        receiver,
        high_water_mark,
        size_algorithm.to_jsval_throwing(scope)?,
    )?;
    // Step 6: Return _stream_.
    Ok(stream)
}

/// <https://streams.spec.whatwg.org/#initialize-writable-stream>
/// InitializeWritableStream(stream) performs the following steps:
pub(crate) fn initialize_writable_stream(stream: &WritableStream<'_>) {
    let mut data = stream.data_mut();
    // Step 1: Set _stream_.`[[state]]` to "`writable`".
    data.state = WritableStreamState::Writable;
    // Step 2: Set _stream_.`[[storedError]]`, _stream_.`[[writer]]`, _stream_.`[[controller]]`,
    //         _stream_.`[[inFlightWriteRequest]]`, _stream_.`[[closeRequest]]`,
    //         _stream_.`[[inFlightCloseRequest]]`, and _stream_.`[[pendingAbortRequest]]` to
    //         undefined.
    data.stored_error.set(value::undefined());
    data.writer = None;
    data.controller = None;
    data.in_flight_write_request = None;
    data.close_request = None;
    data.in_flight_close_request = None;
    data.pending_abort_request = None;
    // Step 3: Set _stream_.`[[writeRequests]]` to a new empty `list`.
    data.write_requests.clear();
    // Step 4: Set _stream_.`[[backpressure]]` to false.
    data.backpressure = false;
}

/// <https://streams.spec.whatwg.org/#is-writable-stream-locked>
/// IsWritableStreamLocked(stream) performs the following steps:
pub(crate) fn is_writable_stream_locked(stream: &WritableStream<'_>) -> bool {
    // Step 1: If _stream_.`[[writer]]` is undefined, return false.
    // Step 2: Return true.
    stream.data().writer.is_some()
}

/// <https://streams.spec.whatwg.org/#set-up-writable-stream-default-writer>
/// SetUpDefaultWriter(writer, stream) performs the following steps:
pub(crate) fn set_up_writable_stream_default_writer(
    scope: &Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
    stream: &WritableStream<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: If ! `IsWritableStreamLocked`(_stream_) is true, throw a ``TypeError`` exception.
    if is_writable_stream_locked(stream) {
        return Err(crate::algorithms::throw_invalid_state(
            scope,
            "WritableStream",
            c"This stream has already been locked for exclusive writing by another writer",
        ));
    }
    // Step 2: Set _writer_.`[[stream]]` to _stream_.
    writer.data_mut().stream.set(*stream);
    // Step 3: Set _stream_.`[[writer]]` to _writer_.
    stream.data_mut().writer.set(*writer);
    // Step 4: Let _state_ be _stream_.`[[state]]`.
    let state = stream.data().state;
    match state {
        // Step 5: If _state_ is "`writable`", If ! `WritableStreamCloseQueuedOrInFlight`(_stream_) is
        //         false and _stream_.`[[backpressure]]` is true, set _writer_.`[[readyPromise]]` to `a
        //         new promise`. Otherwise, set _writer_.`[[readyPromise]]` to `a promise resolved with`
        //         undefined. Set _writer_.`[[closedPromise]]` to `a new promise`.
        WritableStreamState::Writable => {
            if !writable_stream_close_queued_or_in_flight(stream) && stream.data().backpressure {
                writer
                    .data_mut()
                    .ready_promise
                    .set(Promise::new_pending(scope)?);
            } else {
                writer
                    .data_mut()
                    .ready_promise
                    .set(resolved_undefined_promise(scope));
            }
            writer
                .data_mut()
                .closed_promise
                .set(Promise::new_pending(scope)?);
        }
        // Step 6: Otherwise, if _state_ is "`erroring`", Set _writer_.`[[readyPromise]]` to `a promise
        //         rejected with` _stream_.`[[storedError]]`. Set
        //         _writer_.`[[readyPromise]]`.[[PromiseIsHandled]] to true. Set
        //         _writer_.`[[closedPromise]]` to `a new promise`.
        WritableStreamState::Erroring => {
            let stored_error = stream.data().stored_error.get(scope);
            let ready = Promise::new_rejected_with_error(scope, stored_error)?;
            ready.set_settled_is_handled(scope)?;
            writer.data_mut().ready_promise.set(ready);
            writer
                .data_mut()
                .closed_promise
                .set(Promise::new_pending(scope)?);
        }
        // Step 7: Otherwise, if _state_ is "`closed`", Set _writer_.`[[readyPromise]]` to `a promise
        //         resolved with` undefined. Set _writer_.`[[closedPromise]]` to `a promise resolved
        //         with` undefined.
        WritableStreamState::Closed => {
            writer
                .data_mut()
                .ready_promise
                .set(resolved_undefined_promise(scope));
            writer
                .data_mut()
                .closed_promise
                .set(resolved_undefined_promise(scope));
        }
        // Step 8: Otherwise, Assert: _state_ is "`errored`". Let _storedError_ be
        //         _stream_.`[[storedError]]`. Set _writer_.`[[readyPromise]]` to `a promise rejected
        //         with` _storedError_. Set _writer_.`[[readyPromise]]`.[[PromiseIsHandled]] to true.
        //         Set _writer_.`[[closedPromise]]` to `a promise rejected with` _storedError_. Set
        //         _writer_.`[[closedPromise]]`.[[PromiseIsHandled]] to true.
        WritableStreamState::Errored => {
            let stored_error = stream.data().stored_error.get(scope);
            let ready = Promise::new_rejected_with_error(scope, stored_error)?;
            ready.set_settled_is_handled(scope)?;
            writer.data_mut().ready_promise.set(ready);
            let closed = Promise::new_rejected_with_error(scope, stored_error)?;
            closed.set_settled_is_handled(scope)?;
            writer.data_mut().closed_promise.set(closed);
        }
    }
    Ok(())
}

/// <https://streams.spec.whatwg.org/#writable-stream-abort>
/// WritableStreamAbort(stream, reason) performs the following steps:
///
/// Step 2 signals abort on the stream's own controller, running the sink's abort algorithms, so
/// `script_stack_state` is threaded in from the caller (see [`ScriptStackState`]).
pub(crate) fn writable_stream_abort<'r>(
    scope: &'r Scope<'_>,
    stream: &WritableStream<'_>,
    reason: HandleValue<'_>,
    script_stack_state: ScriptStackState,
) -> Promise<'r> {
    // Step 1: If _stream_.`[[state]]` is "`closed`" or "`errored`", return `a promise resolved
    //         with` undefined.
    let state = stream.data().state;
    if matches!(
        state,
        WritableStreamState::Closed | WritableStreamState::Errored
    ) {
        return resolved_undefined_promise(scope);
    }
    // Step 2: `Signal abort` on _stream_.`[[controller]]`.`[[abortController]]` with _reason_.
    let controller = stream.controller(scope);
    let abort_controller: AbortController<'_> = controller.data().abort_controller.get(scope);
    abort_controller
        .abort(scope, reason, script_stack_state)
        .unwrap_or_else(|_| {
            // The spec doesn't define what to do if signaling abort throws, but we shouldn't let
            // that prevent the stream from transitioning to erroring and rejecting the promise
            // returned by this method, so we catch and ignore any exception.
            js::exception::clear(scope);
        });

    // Step 3: Let _state_ be _stream_.`[[state]]`.
    let state = stream.data().state;
    // Step 4: If _state_ is "`closed`" or "`errored`", return `a promise resolved with` undefined.
    //         We re-check the state because `signaling abort` runs author code and that might have
    //         changed the state.
    if matches!(
        state,
        WritableStreamState::Closed | WritableStreamState::Errored
    ) {
        return resolved_undefined_promise(scope);
    }
    // Step 5: If _stream_.`[[pendingAbortRequest]]` is not undefined, return
    //         _stream_.`[[pendingAbortRequest]]`’s `promise`.
    if stream.data().pending_abort_request.is_some() {
        return stream
            .data()
            .pending_abort_request
            .as_ref()
            .unwrap()
            .promise
            .get(scope);
    }
    // Step 6: Assert: _state_ is "`writable`" or "`erroring`".
    debug_assert!(matches!(
        state,
        WritableStreamState::Writable | WritableStreamState::Erroring
    ));
    // Step 7: Let _wasAlreadyErroring_ be false.
    let mut was_already_erroring = false;
    let mut effective_reason = reason;
    // Step 8: If _state_ is "`erroring`", Set _wasAlreadyErroring_ to true. Set _reason_ to
    //         undefined.
    if state == WritableStreamState::Erroring {
        was_already_erroring = true;
        effective_reason = HandleValue::undefined();
    }
    // Step 9: Let _promise_ be `a new promise`.
    let promise = Promise::new_pending(scope).expect("new promise");
    // Step 10: Set _stream_.`[[pendingAbortRequest]]` to a new `pending abort request` whose
    //          `promise` is _promise_, `reason` is _reason_, and `was already erroring` is
    //          _wasAlreadyErroring_.
    stream.data_mut().pending_abort_request = Some(PendingAbortRequest {
        promise: Heap::from(promise),
        reason: Heap::from(effective_reason.get()),
        was_already_erroring,
    });
    // Step 11: If _wasAlreadyErroring_ is false, perform ! `WritableStreamStartErroring`(_stream_,
    //          _reason_).
    if !was_already_erroring {
        writable_stream_start_erroring(scope, stream, effective_reason);
    }
    // Step 12: Return _promise_.
    promise
}

/// <https://streams.spec.whatwg.org/#writable-stream-close>
/// WritableStreamClose(stream) performs the following steps:
pub(crate) fn writable_stream_close<'r>(
    scope: &'r Scope<'_>,
    stream: &WritableStream<'_>,
) -> Promise<'r> {
    // Step 1: Let _state_ be _stream_.`[[state]]`.
    let state = stream.data().state;
    // Step 2: If _state_ is "`closed`" or "`errored`", return `a promise rejected with` a
    //         ``TypeError`` exception.
    if matches!(
        state,
        WritableStreamState::Closed | WritableStreamState::Errored
    ) {
        crate::algorithms::throw_invalid_state(scope, "WritableStream", c"The stream (in closed or errored state) is not in the writable state and cannot be closed");
        return Promise::new_rejected_with_pending_error(scope).expect("rejected promise");
    }
    // Step 3: Assert: _state_ is "`writable`" or "`erroring`".
    debug_assert!(matches!(
        state,
        WritableStreamState::Writable | WritableStreamState::Erroring
    ));
    // Step 4: Assert: ! `WritableStreamCloseQueuedOrInFlight`(_stream_) is false.
    debug_assert!(!writable_stream_close_queued_or_in_flight(stream));
    // Step 5: Let _promise_ be `a new promise`.
    let promise = Promise::new_pending(scope).expect("new promise");
    // Step 6: Set _stream_.`[[closeRequest]]` to _promise_.
    stream.data_mut().close_request = Some(PromiseSlot::new(promise));
    // Step 7: Let _writer_ be _stream_.`[[writer]]`.
    // Step 8: If _writer_ is not undefined, and _stream_.`[[backpressure]]` is true, and _state_ is
    //         "`writable`", `resolve` _writer_.`[[readyPromise]]` with undefined.
    if let Some(writer) = writable_stream_writer(scope, stream) {
        if stream.data().backpressure && state == WritableStreamState::Writable {
            writer
                .data()
                .ready_promise
                .get(scope)
                .resolve(scope, HandleValue::undefined())
                .unwrap();
        }
    }
    // Step 9: Perform ! `WritableStreamDefaultControllerClose`(_stream_.`[[controller]]`).
    let controller = stream.controller(scope);
    writable_stream_default_controller_close(scope, &controller);
    // Step 10: Return _promise_.
    promise
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-writer-close-with-error-propagation>
/// DefaultWriterCloseWithErrorPropagation(writer) performs the following steps:
pub(crate) fn writable_stream_default_writer_close_with_error_propagation<'r>(
    scope: &'r Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
) -> Promise<'r> {
    // Step 1: Let _stream_ be _writer_.`[[stream]]`.
    // Step 2: Assert: _stream_ is not undefined.
    let stream = writer_stream(scope, writer).expect("writer has a stream");
    // Step 3: Let _state_ be _stream_.`[[state]]`.
    let state = stream.data().state;
    // Step 4: If ! `WritableStreamCloseQueuedOrInFlight`(_stream_) is true or _state_ is
    //         "`closed`", return `a promise resolved with` undefined.
    if writable_stream_close_queued_or_in_flight(&stream) || state == WritableStreamState::Closed {
        return resolved_undefined_promise(scope);
    }
    // Step 5: If _state_ is "`errored`", return `a promise rejected with`
    //         _stream_.`[[storedError]]`.
    if state == WritableStreamState::Errored {
        let stored_error = stream.data().stored_error.get(scope);
        return Promise::new_rejected_with_error(scope, stored_error).expect("rejected promise");
    }
    // Step 6: Assert: _state_ is "`writable`" or "`erroring`".
    debug_assert!(matches!(
        state,
        WritableStreamState::Writable | WritableStreamState::Erroring
    ));
    // Step 7: Return ! `DefaultWriterClose`(_writer_).
    writable_stream_default_writer_close(scope, writer)
}

/// <https://streams.spec.whatwg.org/#writable-stream-add-write-request>
/// WritableStreamAddWriteRequest(stream) performs the following steps:
pub(crate) fn writable_stream_add_write_request<'r>(
    scope: &'r Scope<'_>,
    stream: &WritableStream<'_>,
) -> Promise<'r> {
    // Step 1: Assert: ! `IsWritableStreamLocked`(_stream_) is true.
    debug_assert!(is_writable_stream_locked(stream));
    // Step 2: Assert: _stream_.`[[state]]` is "`writable`".
    debug_assert_eq!(stream.data().state, WritableStreamState::Writable);
    // Step 3: Let _promise_ be `a new promise`.
    let promise = Promise::new_pending(scope).expect("new promise");
    // Step 4: `Append` _promise_ to _stream_.`[[writeRequests]]`.
    stream
        .data_mut()
        .write_requests
        .push_back(PromiseSlot::new(promise));
    // Step 5: Return _promise_.
    promise
}

/// <https://streams.spec.whatwg.org/#writable-stream-close-queued-or-in-flight>
/// WritableStreamCloseQueuedOrInFlight(stream) performs the following steps:
pub(crate) fn writable_stream_close_queued_or_in_flight(stream: &WritableStream<'_>) -> bool {
    // Step 1: If _stream_.`[[closeRequest]]` is undefined and _stream_.`[[inFlightCloseRequest]]`
    //         is undefined, return false.
    // Step 2: Return true.
    stream.data().close_request.is_some() || stream.data().in_flight_close_request.is_some()
}

/// <https://streams.spec.whatwg.org/#writable-stream-deal-with-rejection>
/// WritableStreamDealWithRejection(stream, error) performs the following steps:
pub(crate) fn writable_stream_deal_with_rejection(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
    error: HandleValue<'_>,
) {
    // Step 1: Let _state_ be _stream_.`[[state]]`.
    let state = stream.data().state;
    // Step 2: If _state_ is "`writable`", Perform ! `WritableStreamStartErroring`(_stream_,
    //         _error_). Return.
    if state == WritableStreamState::Writable {
        writable_stream_start_erroring(scope, stream, error);
        return;
    }
    // Step 3: Assert: _state_ is "`erroring`".
    debug_assert_eq!(state, WritableStreamState::Erroring);
    // Step 4: Perform ! `WritableStreamFinishErroring`(_stream_).
    writable_stream_finish_erroring(scope, stream);
}

/// <https://streams.spec.whatwg.org/#writable-stream-finish-erroring>
/// WritableStreamFinishErroring(stream) performs the following steps:
pub(crate) fn writable_stream_finish_erroring(scope: &Scope<'_>, stream: &WritableStream<'_>) {
    // Step 1: Assert: _stream_.`[[state]]` is "`erroring`".
    debug_assert_eq!(stream.data().state, WritableStreamState::Erroring);
    // Step 2: Assert: ! `WritableStreamHasOperationMarkedInFlight`(_stream_) is false.
    debug_assert!(!writable_stream_has_operation_marked_in_flight(stream));
    // Step 3: Set _stream_.`[[state]]` to "`errored`".
    stream.data_mut().state = WritableStreamState::Errored;
    // Step 4: Perform ! _stream_.`[[controller]]`.`[[ErrorSteps]]`().
    let controller = stream.controller(scope);
    error_steps(&controller);
    // Step 5: Let _storedError_ be _stream_.`[[storedError]]`.
    let stored_error = stream.data().stored_error.get(scope);
    // Step 6: `For each` _writeRequest_ of _stream_.`[[writeRequests]]`: `Reject` _writeRequest_
    //         with _storedError_.
    // Step 7: Set _stream_.`[[writeRequests]]` to an empty `list`.
    //         Draining via `pop_front` empties the list and rejects each entry, equivalent to
    //         rejecting then clearing, and keeps no unrooted collection local.
    while !stream.data().write_requests.is_empty() {
        stream
            .data_mut()
            .write_requests
            .pop_front()
            .unwrap()
            .root(scope)
            .into_promise()
            .reject(scope, stored_error)
            .expect("reject write request");
    }
    // Step 8: If _stream_.`[[pendingAbortRequest]]` is undefined, Perform !
    //         `WritableStreamRejectCloseAndClosedPromiseIfNeeded`(_stream_). Return.
    if stream.data().pending_abort_request.is_none() {
        writable_stream_reject_close_and_closed_promise_if_needed(scope, stream);
        return;
    }
    // Step 9: Let _abortRequest_ be _stream_.`[[pendingAbortRequest]]`.
    // Step 10: Set _stream_.`[[pendingAbortRequest]]` to undefined.
    // `abort_request` is moved out of the (traced) stream, so keep it in a
    // `RootedTraceableBox` while we read its `Heap` fields: rooting `reason`
    // below can compact, and the request must stay traced so the subsequent read
    // of `promise` sees a current pointer.
    let mut abort_request = RootedTraceableBox::new(Some(
        stream.data_mut().pending_abort_request.take().unwrap(),
    ));
    // Step 11: If _abortRequest_’s `was already erroring` is true, `Reject` _abortRequest_’s
    //          `promise` with _storedError_. Perform !
    //          `WritableStreamRejectCloseAndClosedPromiseIfNeeded`(_stream_). Return.
    if abort_request.as_ref().unwrap().was_already_erroring {
        abort_request
            .take()
            .unwrap()
            .root(scope)
            .into_promise()
            .reject(scope, stored_error)
            .expect("reject abort promise");
        writable_stream_reject_close_and_closed_promise_if_needed(scope, stream);
        return;
    }
    // Step 12: Let _promise_ be ! _stream_.`[[controller]]`.`[[AbortSteps]]`(_abortRequest_’s
    //          `reason`).
    //
    // Both reads happen while `abort_request` is still traced (in the box); the
    // rooted `abort_reason`/`abort_promise` then survive `abort_steps` (author
    // code that can GC) on their own.
    let abort_reason = abort_request.as_ref().unwrap().reason.get(scope);
    let abort_promise: Promise<'_> = abort_request.as_ref().unwrap().promise.get(scope);
    drop(abort_request);
    let promise = abort_steps(scope, &controller, abort_reason);
    // Step 13: `Upon fulfillment` of _promise_, `Resolve` _abortRequest_’s `promise` with
    //          undefined. Perform ! `WritableStreamRejectCloseAndClosedPromiseIfNeeded`(_stream_).
    // Step 14: `Upon rejection` of _promise_ with reason _reason_, `Reject` _abortRequest_’s
    //          `promise` with _reason_. Perform !
    //          `WritableStreamRejectCloseAndClosedPromiseIfNeeded`(_stream_).
    // (Steps 13 and 14 are implemented by `ws_abort_promise_fulfilled` / `ws_abort_promise_rejected`.)
    let payload =
        abort_reaction_payload(scope, stream, &abort_promise).expect("build abort payload");
    support::react(
        scope,
        &promise,
        Some((ws_abort_promise_fulfilled, payload)),
        Some((ws_abort_promise_rejected, payload)),
    )
    .expect("attach abort reactions");
}

/// <https://streams.spec.whatwg.org/#writable-stream-finish-in-flight-close>
/// WritableStreamFinishInFlightClose(stream) performs the following steps:
pub(crate) fn writable_stream_finish_in_flight_close(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
) {
    // Step 1: Assert: _stream_.`[[inFlightCloseRequest]]` is not undefined.
    // Step 2: `Resolve` _stream_.`[[inFlightCloseRequest]]` with undefined.
    // Step 3: Set _stream_.`[[inFlightCloseRequest]]` to undefined.
    let in_flight_promise = stream
        .data_mut()
        .in_flight_close_request
        .take()
        .expect("in-flight close request")
        .root(scope)
        .into_promise();
    in_flight_promise
        .resolve(scope, HandleValue::undefined())
        .unwrap();
    // Step 4: Let _state_ be _stream_.`[[state]]`.
    let state = stream.data().state;
    // Step 5: Assert: _stream_.`[[state]]` is "`writable`" or "`erroring`".
    debug_assert!(matches!(
        state,
        WritableStreamState::Writable | WritableStreamState::Erroring
    ));
    // Step 6: If _state_ is "`erroring`", Set _stream_.`[[storedError]]` to undefined. If
    //         _stream_.`[[pendingAbortRequest]]` is not undefined, `Resolve`
    //         _stream_.`[[pendingAbortRequest]]`’s `promise` with undefined. Set
    //         _stream_.`[[pendingAbortRequest]]` to undefined.
    if state == WritableStreamState::Erroring {
        stream.data_mut().stored_error.set(value::undefined());
        if stream.data().pending_abort_request.is_some() {
            let abort_promise = stream
                .data_mut()
                .pending_abort_request
                .take()
                .unwrap()
                .root(scope)
                .into_promise();
            abort_promise
                .resolve(scope, HandleValue::undefined())
                .unwrap();
        }
    }
    // Step 7: Set _stream_.`[[state]]` to "`closed`".
    stream.data_mut().state = WritableStreamState::Closed;
    // Step 8: Let _writer_ be _stream_.`[[writer]]`.
    // Step 9: If _writer_ is not undefined, `resolve` _writer_.`[[closedPromise]]` with undefined.
    if let Some(writer) = writable_stream_writer(scope, stream) {
        writer
            .data()
            .closed_promise
            .get(scope)
            .resolve(scope, HandleValue::undefined())
            .unwrap();
    }
    // Step 10: Assert: _stream_.`[[pendingAbortRequest]]` is undefined.
    debug_assert!(stream.data().pending_abort_request.is_none());
    // Step 11: Assert: _stream_.`[[storedError]]` is undefined.
    debug_assert!(stream.data().stored_error.is_undefined());
}

/// <https://streams.spec.whatwg.org/#writable-stream-finish-in-flight-close-with-error>
/// WritableStreamFinishInFlightCloseWithError(stream, error) performs the following steps:
pub(crate) fn writable_stream_finish_in_flight_close_with_error(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
    error: HandleValue<'_>,
) {
    // Step 1: Assert: _stream_.`[[inFlightCloseRequest]]` is not undefined.
    // Step 2: `Reject` _stream_.`[[inFlightCloseRequest]]` with _error_.
    // Step 3: Set _stream_.`[[inFlightCloseRequest]]` to undefined.
    stream
        .data_mut()
        .in_flight_close_request
        .take()
        .expect("in-flight close request")
        .root(scope)
        .into_promise()
        .reject(scope, error)
        .expect("reject in-flight close");
    // Step 4: Assert: _stream_.`[[state]]` is "`writable`" or "`erroring`".
    debug_assert!(matches!(
        stream.data().state,
        WritableStreamState::Writable | WritableStreamState::Erroring
    ));
    // Step 5: If _stream_.`[[pendingAbortRequest]]` is not undefined, `Reject`
    //         _stream_.`[[pendingAbortRequest]]`’s `promise` with _error_. Set
    //         _stream_.`[[pendingAbortRequest]]` to undefined.
    if stream.data().pending_abort_request.is_some() {
        stream
            .data_mut()
            .pending_abort_request
            .take()
            .unwrap()
            .root(scope)
            .into_promise()
            .reject(scope, error)
            .expect("reject abort promise");
    }
    // Step 6: Perform ! `WritableStreamDealWithRejection`(_stream_, _error_).
    writable_stream_deal_with_rejection(scope, stream, error);
}

/// <https://streams.spec.whatwg.org/#writable-stream-finish-in-flight-write>
/// WritableStreamFinishInFlightWrite(stream) performs the following steps:
pub(crate) fn writable_stream_finish_in_flight_write(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
) {
    // Step 1: Assert: _stream_.`[[inFlightWriteRequest]]` is not undefined.
    // Step 2: `Resolve` _stream_.`[[inFlightWriteRequest]]` with undefined.
    // Step 3: Set _stream_.`[[inFlightWriteRequest]]` to undefined.
    let promise = stream
        .data_mut()
        .in_flight_write_request
        .take()
        .expect("in-flight write request")
        .root(scope)
        .into_promise();
    promise.resolve(scope, HandleValue::undefined()).unwrap();
}

/// <https://streams.spec.whatwg.org/#writable-stream-finish-in-flight-write-with-error>
/// WritableStreamFinishInFlightWriteWithError(stream, error) performs the following steps:
pub(crate) fn writable_stream_finish_in_flight_write_with_error(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
    error: HandleValue<'_>,
) {
    // Step 1: Assert: _stream_.`[[inFlightWriteRequest]]` is not undefined.
    // Step 2: `Reject` _stream_.`[[inFlightWriteRequest]]` with _error_.
    // Step 3: Set _stream_.`[[inFlightWriteRequest]]` to undefined.
    stream
        .data_mut()
        .in_flight_write_request
        .take()
        .expect("in-flight write request")
        .root(scope)
        .into_promise()
        .reject(scope, error)
        .expect("reject in-flight write");
    // Step 4: Assert: _stream_.`[[state]]` is "`writable`" or "`erroring`".
    debug_assert!(matches!(
        stream.data().state,
        WritableStreamState::Writable | WritableStreamState::Erroring
    ));
    // Step 5: Perform ! `WritableStreamDealWithRejection`(_stream_, _error_).
    writable_stream_deal_with_rejection(scope, stream, error);
}

/// <https://streams.spec.whatwg.org/#writable-stream-has-operation-marked-in-flight>
/// WritableStreamHasOperationMarkedInFlight(stream) performs the following steps:
pub(crate) fn writable_stream_has_operation_marked_in_flight(stream: &WritableStream<'_>) -> bool {
    // Step 1: If _stream_.`[[inFlightWriteRequest]]` is undefined and
    //         _stream_.`[[inFlightCloseRequest]]` is undefined, return false.
    // Step 2: Return true.
    stream.data().in_flight_write_request.is_some()
        || stream.data().in_flight_close_request.is_some()
}

/// <https://streams.spec.whatwg.org/#writable-stream-mark-close-request-in-flight>
/// WritableStreamMarkCloseRequestInFlight(stream) performs the following steps:
pub(crate) fn writable_stream_mark_close_request_in_flight(stream: &WritableStream<'_>) {
    // Step 1: Assert: _stream_.`[[inFlightCloseRequest]]` is undefined.
    debug_assert!(stream.data().in_flight_close_request.is_none());
    // Step 2: Assert: _stream_.`[[closeRequest]]` is not undefined.
    debug_assert!(stream.data().close_request.is_some());
    // Step 3: Set _stream_.`[[inFlightCloseRequest]]` to _stream_.`[[closeRequest]]`.
    // Step 4: Set _stream_.`[[closeRequest]]` to undefined.
    // Move the slot between two traced fields in one borrow, with no `#[must_root]`
    // local in between.
    let mut data = stream.data_mut();
    data.in_flight_close_request = data.close_request.take();
}

/// <https://streams.spec.whatwg.org/#writable-stream-mark-first-write-request-in-flight>
/// WritableStreamMarkFirstWriteRequestInFlight(stream) performs the following steps:
pub(crate) fn writable_stream_mark_first_write_request_in_flight(stream: &WritableStream<'_>) {
    // Step 1: Assert: _stream_.`[[inFlightWriteRequest]]` is undefined.
    debug_assert!(stream.data().in_flight_write_request.is_none());
    // Step 2: Assert: _stream_.`[[writeRequests]]` is not empty.
    debug_assert!(!stream.data().write_requests.is_empty());
    // Step 3: Let _writeRequest_ be _stream_.`[[writeRequests]]`[0].
    // Step 4: `Remove` _writeRequest_ from _stream_.`[[writeRequests]]`.
    // Step 5: Set _stream_.`[[inFlightWriteRequest]]` to _writeRequest_.
    // Move the slot between two traced fields in one borrow, with no `#[must_root]`
    // local in between.
    let mut data = stream.data_mut();
    data.in_flight_write_request = Some(data.write_requests.pop_front().unwrap());
}

/// <https://streams.spec.whatwg.org/#writable-stream-reject-close-and-closed-promise-if-needed>
/// WritableStreamRejectCloseAndClosedPromiseIfNeeded(stream) performs the following steps:
pub(crate) fn writable_stream_reject_close_and_closed_promise_if_needed(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
) {
    // Step 1: Assert: _stream_.`[[state]]` is "`errored`".
    debug_assert_eq!(stream.data().state, WritableStreamState::Errored);
    let stored_error = stream.data().stored_error.get(scope);
    // Step 2: If _stream_.`[[closeRequest]]` is not undefined, Assert:
    //         _stream_.`[[inFlightCloseRequest]]` is undefined. `Reject`
    //         _stream_.`[[closeRequest]]` with _stream_.`[[storedError]]`. Set
    //         _stream_.`[[closeRequest]]` to undefined.
    if stream.data().close_request.is_some() {
        debug_assert!(stream.data().in_flight_close_request.is_none());
        stream
            .data_mut()
            .close_request
            .take()
            .unwrap()
            .root(scope)
            .into_promise()
            .reject(scope, stored_error)
            .expect("reject close request");
    }
    // Step 3: Let _writer_ be _stream_.`[[writer]]`.
    // Step 4: If _writer_ is not undefined, `Reject` _writer_.`[[closedPromise]]` with
    //         _stream_.`[[storedError]]`. Set _writer_.`[[closedPromise]]`.[[PromiseIsHandled]] to
    //         true.
    if let Some(writer) = writable_stream_writer(scope, stream) {
        let closed = writer.data().closed_promise.get(scope);
        closed.reject(scope, stored_error).expect("reject closed");
        closed
            .set_settled_is_handled(scope)
            .expect("set closed handled");
    }
}

/// <https://streams.spec.whatwg.org/#writable-stream-start-erroring>
/// WritableStreamStartErroring(stream, reason) performs the following steps:
pub(crate) fn writable_stream_start_erroring(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
    reason: HandleValue<'_>,
) {
    // Step 1: Assert: _stream_.`[[storedError]]` is undefined.
    debug_assert!(stream.data().stored_error.is_undefined());
    // Step 2: Assert: _stream_.`[[state]]` is "`writable`".
    debug_assert_eq!(stream.data().state, WritableStreamState::Writable);
    // Step 3: Let _controller_ be _stream_.`[[controller]]`.
    // Step 4: Assert: _controller_ is not undefined.
    let controller = stream.controller(scope);
    // Step 5: Set _stream_.`[[state]]` to "`erroring`".
    stream.data_mut().state = WritableStreamState::Erroring;
    // Step 6: Set _stream_.`[[storedError]]` to _reason_.
    stream.data_mut().stored_error.set(reason.get());
    // Step 7: Let _writer_ be _stream_.`[[writer]]`.
    // Step 8: If _writer_ is not undefined, perform !
    //         `DefaultWriterEnsureReadyPromiseRejected`(_writer_, _reason_).
    if let Some(writer) = writable_stream_writer(scope, stream) {
        writable_stream_default_writer_ensure_ready_promise_rejected(scope, &writer, reason);
    }
    // Step 9: If ! `WritableStreamHasOperationMarkedInFlight`(_stream_) is false and
    //         _controller_.`[[started]]` is true, perform !
    //         `WritableStreamFinishErroring`(_stream_).
    if !writable_stream_has_operation_marked_in_flight(stream) && controller.data().started {
        writable_stream_finish_erroring(scope, stream);
    }
}

/// <https://streams.spec.whatwg.org/#writable-stream-update-backpressure>
/// WritableStreamUpdateBackpressure(stream, backpressure) performs the following steps:
pub(crate) fn writable_stream_update_backpressure(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
    backpressure: bool,
) {
    // Step 1: Assert: _stream_.`[[state]]` is "`writable`".
    debug_assert_eq!(stream.data().state, WritableStreamState::Writable);
    // Step 2: Assert: ! `WritableStreamCloseQueuedOrInFlight`(_stream_) is false.
    debug_assert!(!writable_stream_close_queued_or_in_flight(stream));
    // Step 3: Let _writer_ be _stream_.`[[writer]]`.
    let current = stream.data().backpressure;
    // Step 4: If _writer_ is not undefined and _backpressure_ is not _stream_.`[[backpressure]]`,
    //         If _backpressure_ is true, set _writer_.`[[readyPromise]]` to `a new promise`.
    //         Otherwise, Assert: _backpressure_ is false. `Resolve` _writer_.`[[readyPromise]]`
    //         with undefined.
    if let Some(writer) = writable_stream_writer(scope, stream) {
        if backpressure != current {
            if backpressure {
                let promise = Promise::new_pending(scope).expect("new promise");
                writer.data_mut().ready_promise.set(promise);
            } else {
                let ready = writer.data().ready_promise.get(scope);
                ready.resolve(scope, HandleValue::undefined()).unwrap();
            }
        }
    }
    // Step 5: Set _stream_.`[[backpressure]]` to _backpressure_.
    stream.data_mut().backpressure = backpressure;
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-writer-abort>
/// DefaultWriterAbort(writer, reason) performs the following steps:
pub(crate) fn writable_stream_default_writer_abort<'r>(
    scope: &'r Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
    reason: HandleValue<'_>,
    script_stack_state: ScriptStackState,
) -> Promise<'r> {
    // Step 1: Let _stream_ be _writer_.`[[stream]]`.
    // Step 2: Assert: _stream_ is not undefined.
    let stream = writer_stream(scope, writer).expect("writer has a stream");
    // Step 3: Return ! `WritableStreamAbort`(_stream_, _reason_).
    writable_stream_abort(scope, &stream, reason, script_stack_state)
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-writer-close>
/// DefaultWriterClose(writer) performs the following steps:
pub(crate) fn writable_stream_default_writer_close<'r>(
    scope: &'r Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
) -> Promise<'r> {
    // Step 1: Let _stream_ be _writer_.`[[stream]]`.
    // Step 2: Assert: _stream_ is not undefined.
    let stream = writer_stream(scope, writer).expect("writer has a stream");
    // Step 3: Return ! `WritableStreamClose`(_stream_).
    writable_stream_close(scope, &stream)
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-writer-ensure-closed-promise-rejected>
/// DefaultWriterEnsureClosedPromiseRejected(writer, error) performs the following steps:
pub(crate) fn writable_stream_default_writer_ensure_closed_promise_rejected(
    scope: &Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
    error: HandleValue<'_>,
) {
    let closed = writer.data().closed_promise.get(scope);
    // Step 1: If _writer_.`[[closedPromise]]`.[[PromiseState]] is "`pending`", `reject`
    //         _writer_.`[[closedPromise]]` with _error_.
    if closed.is_pending() {
        closed.reject(scope, error).expect("reject closed promise");
    } else {
        // Step 2: Otherwise, set _writer_.`[[closedPromise]]` to `a promise rejected with` _error_.
        let rejected = Promise::new_rejected_with_error(scope, error).expect("rejected promise");
        writer.data_mut().closed_promise.set(rejected);
    }
    // Step 3: Set _writer_.`[[closedPromise]]`.[[PromiseIsHandled]] to true.
    writer
        .data()
        .closed_promise
        .get(scope)
        .set_settled_is_handled(scope)
        .expect("set closed handled");
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-writer-ensure-ready-promise-rejected>
/// DefaultWriterEnsureReadyPromiseRejected(writer, error) performs the following steps:
pub(crate) fn writable_stream_default_writer_ensure_ready_promise_rejected(
    scope: &Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
    error: HandleValue<'_>,
) {
    let ready = writer.data().ready_promise.get(scope);
    // Step 1: If _writer_.`[[readyPromise]]`.[[PromiseState]] is "`pending`", `reject`
    //         _writer_.`[[readyPromise]]` with _error_.
    if ready.is_pending() {
        ready.reject(scope, error).expect("reject ready promise");
    } else {
        // Step 2: Otherwise, set _writer_.`[[readyPromise]]` to `a promise rejected with` _error_.
        let rejected = Promise::new_rejected_with_error(scope, error).expect("rejected promise");
        writer.data_mut().ready_promise.set(rejected);
    }
    // Step 3: Set _writer_.`[[readyPromise]]`.[[PromiseIsHandled]] to true.
    writer
        .data()
        .ready_promise
        .get(scope)
        .set_settled_is_handled(scope)
        .expect("set ready handled");
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-writer-get-desired-size>
/// DefaultWriterGetDesiredSize(writer) performs the following steps:
pub(crate) fn writable_stream_default_writer_get_desired_size(
    scope: &Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
) -> Option<f64> {
    // Step 1: Let _stream_ be _writer_.`[[stream]]`.
    let stream = writer_stream(scope, writer).expect("writer has a stream");
    // Step 2: Let _state_ be _stream_.`[[state]]`.
    let state = stream.data().state;
    match state {
        // Step 3: If _state_ is "`errored`" or "`erroring`", return null.
        WritableStreamState::Errored | WritableStreamState::Erroring => None,
        // Step 4: If _state_ is "`closed`", return 0.
        WritableStreamState::Closed => Some(0.0),
        // Step 5: Return ! `WritableStreamDefaultControllerGetDesiredSize`(_stream_.`[[controller]]`).
        WritableStreamState::Writable => {
            let controller = stream.controller(scope);
            Some(writable_stream_default_controller_get_desired_size(
                &controller,
            ))
        }
    }
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-writer-release>
/// DefaultWriterRelease(writer) performs the following steps:
pub(crate) fn writable_stream_default_writer_release(
    scope: &Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
) {
    // Step 1: Let _stream_ be _writer_.`[[stream]]`.
    // Step 2: Assert: _stream_ is not undefined.
    let stream = writer_stream(scope, writer).expect("writer has a stream");
    // Step 3: Assert: _stream_.`[[writer]]` is _writer_.
    // Step 4: Let _releasedError_ be a new ``TypeError``.
    let released_error = invalid_state_error(
        scope,
        "WritableStreamDefaultWriter",
        c"Writer was released and can no longer be used to monitor the stream's closedness",
    );
    // Step 5: Perform ! `DefaultWriterEnsureReadyPromiseRejected`(_writer_,
    //         _releasedError_).
    writable_stream_default_writer_ensure_ready_promise_rejected(scope, writer, released_error);
    // Step 6: Perform ! `DefaultWriterEnsureClosedPromiseRejected`(_writer_,
    //         _releasedError_).
    writable_stream_default_writer_ensure_closed_promise_rejected(scope, writer, released_error);
    // Step 7: Set _stream_.`[[writer]]` to undefined.
    stream.data_mut().writer = None;
    // Step 8: Set _writer_.`[[stream]]` to undefined.
    writer.data_mut().stream = None;
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-writer-write>
/// DefaultWriterWrite(writer, chunk) performs the following steps:
pub(crate) fn writable_stream_default_writer_write<'r>(
    scope: &'r Scope<'_>,
    writer: &WritableStreamDefaultWriter<'_>,
    chunk: HandleValue<'_>,
) -> Promise<'r> {
    // Step 1: Let _stream_ be _writer_.`[[stream]]`.
    // Step 2: Assert: _stream_ is not undefined.
    let stream = writer_stream(scope, writer).expect("writer has a stream");
    // Step 3: Let _controller_ be _stream_.`[[controller]]`.
    let controller = stream.controller(scope);
    // Step 4: Let _chunkSize_ be ! `WritableStreamDefaultControllerGetChunkSize`(_controller_,
    //         _chunk_).
    let chunk_size = writable_stream_default_controller_get_chunk_size(scope, &controller, chunk);
    // Step 5: If _stream_ is not equal to _writer_.`[[stream]]`, return `a promise rejected with` a
    //         ``TypeError`` exception.
    let still_owned = writer
        .data()
        .stream
        .as_ref()
        .is_some_and(|s| stream.eq_heap(s));
    if !still_owned {
        crate::algorithms::throw_invalid_state(
            scope,
            "WritableStreamDefaultWriter",
            c"Cannot write to a stream using a released writer",
        );
        return Promise::new_rejected_with_pending_error(scope).expect("rejected promise");
    }
    // Step 6: Let _state_ be _stream_.`[[state]]`.
    let state = stream.data().state;
    // Step 7: If _state_ is "`errored`", return `a promise rejected with`
    //         _stream_.`[[storedError]]`.
    if state == WritableStreamState::Errored {
        let stored_error = stream.data().stored_error.get(scope);
        return Promise::new_rejected_with_error(scope, stored_error).expect("rejected promise");
    }
    // Step 8: If ! `WritableStreamCloseQueuedOrInFlight`(_stream_) is true or _state_ is
    //         "`closed`", return `a promise rejected with` a ``TypeError`` exception indicating
    //         that the stream is closing or closed.
    if writable_stream_close_queued_or_in_flight(&stream) || state == WritableStreamState::Closed {
        crate::algorithms::throw_invalid_state(
            scope,
            "WritableStreamDefaultWriter",
            c"The stream is closing or closed and cannot be written to",
        );
        return Promise::new_rejected_with_pending_error(scope).expect("rejected promise");
    }
    // Step 9: If _state_ is "`erroring`", return `a promise rejected with`
    //         _stream_.`[[storedError]]`.
    if state == WritableStreamState::Erroring {
        let stored_error = stream.data().stored_error.get(scope);
        return Promise::new_rejected_with_error(scope, stored_error).expect("rejected promise");
    }
    // Step 10: Assert: _state_ is "`writable`".
    debug_assert_eq!(state, WritableStreamState::Writable);
    // Step 11: Let _promise_ be ! `WritableStreamAddWriteRequest`(_stream_).
    let promise = writable_stream_add_write_request(scope, &stream);
    // Step 12: Perform ! `WritableStreamDefaultControllerWrite`(_controller_, _chunk_,
    //          _chunkSize_).
    writable_stream_default_controller_write(scope, &controller, chunk, chunk_size);
    // Step 13: Return _promise_.
    promise
}

/// <https://streams.spec.whatwg.org/#set-up-writable-stream-default-controller>
/// SetUpWritableStreamDefaultController(stream, controller, startAlgorithm, writeAlgorithm, closeAlgorithm, abortAlgorithm, highWaterMark, sizeAlgorithm) performs the following steps:
#[allow(clippy::too_many_arguments)]
pub(crate) fn set_up_writable_stream_default_controller(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
    controller: &WritableStreamDefaultController<'_>,
    start_algorithm: AlgorithmArg<'_>,
    write_algorithm: AlgorithmArg<'_>,
    close_algorithm: AlgorithmArg<'_>,
    abort_algorithm: AlgorithmArg<'_>,
    algorithm_receiver: HandleValue<'_>,
    high_water_mark: f64,
    size_algorithm: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Assert: _stream_ `implements` ``WritableStream``.
    // Step 2: Assert: _stream_.`[[controller]]` is undefined.
    debug_assert!(stream.data().controller.is_none());
    // Step 3: Set _controller_.`[[stream]]` to _stream_.
    //         (Already done by `WritableStreamDefaultController::new`.)
    debug_assert!(stream.eq_heap(&controller.data().stream));
    // Step 4: Set _stream_.`[[controller]]` to _controller_.
    stream.data_mut().controller.set(*controller);
    // Step 5: Perform ! `ResetQueue`(_controller_).
    reset_queue(&mut *controller.data_mut());
    // Step 6: Set _controller_.`[[abortController]]` to a new ``AbortController``.
    // Already done by `WritableStreamDefaultController::new`, because the field
    // needs to be initialized before that returns.
    // Step 7: Set _controller_.`[[started]]` to false.
    controller.data_mut().started = false;
    // Step 8: Set _controller_.`[[strategySizeAlgorithm]]` to _sizeAlgorithm_.
    controller
        .data_mut()
        .strategy_size_algorithm
        .set(size_algorithm.get());
    // Step 9: Set _controller_.`[[strategyHWM]]` to _highWaterMark_.
    controller.data_mut().strategy_hwm = high_water_mark;
    // Step 10: Set _controller_.`[[writeAlgorithm]]` to _writeAlgorithm_.
    controller.data_mut().write_algorithm = Algorithm::from(write_algorithm);
    // Step 11: Set _controller_.`[[closeAlgorithm]]` to _closeAlgorithm_.
    controller.data_mut().close_algorithm = Algorithm::from(close_algorithm);
    // Step 12: Set _controller_.`[[abortAlgorithm]]` to _abortAlgorithm_.
    controller.data_mut().abort_algorithm = Algorithm::from(abort_algorithm);
    // (The algorithms close over `algorithm_receiver` — the underlying sink — as their `this`.)
    controller
        .data_mut()
        .algorithm_receiver
        .set(algorithm_receiver.get());
    // Step 13: Let _backpressure_ be !
    //          `WritableStreamDefaultControllerGetBackpressure`(_controller_).
    let backpressure = writable_stream_default_controller_get_backpressure(controller);
    // Step 14: Perform ! `WritableStreamUpdateBackpressure`(_stream_, _backpressure_).
    writable_stream_update_backpressure(scope, stream, backpressure);
    // Step 15: Let _startResult_ be the result of performing _startAlgorithm_. (This may throw an
    //          exception.)
    let start_result =
        support::invoke_algorithm(scope, start_algorithm, algorithm_receiver, &[controller])?;
    // Step 16: Let _startPromise_ be `a promise resolved with` _startResult_.
    //          WebIDL "a promise resolved with" always creates a *new* promise (it
    //          does not return the value as-is the way `Promise.resolve` does for
    //          a promise input), so when `startResult` is itself a promise — as
    //          for a `TransformStream`'s writable side — adopting it adds a
    //          microtask tick. That tick is observable: it orders the writable's
    //          `[[started]]` transition after a same-job `readable.cancel()`.
    let start_promise = Promise::new_resolved_with_value(scope, start_result)?;
    // Step 17: `Upon fulfillment` of _startPromise_, Assert: _stream_.`[[state]]` is "`writable`"
    //          or "`erroring`". Set _controller_.`[[started]]` to true. Perform !
    //          `WritableStreamDefaultControllerAdvanceQueueIfNeeded`(_controller_).
    // Step 18: `Upon rejection` of _startPromise_ with reason _r_, Assert: _stream_.`[[state]]` is
    //          "`writable`" or "`erroring`". Set _controller_.`[[started]]` to true. Perform !
    //          `WritableStreamDealWithRejection`(_stream_, _r_).
    // (Steps 17 and 18 are implemented by `ws_start_promise_fulfilled` / `ws_start_promise_rejected`.)
    support::react(
        scope,
        &start_promise,
        Some((ws_start_promise_fulfilled, controller)),
        Some((ws_start_promise_rejected, controller)),
    )?;
    Ok(())
}

/// <https://streams.spec.whatwg.org/#set-up-writable-stream-default-controller-from-underlying-sink>
/// SetUpWritableStreamDefaultControllerFromUnderlyingSink(stream, underlyingSink, underlyingSinkDict, highWaterMark, sizeAlgorithm) performs the following steps:
pub(crate) fn set_up_writable_stream_default_controller_from_underlying_sink(
    scope: &Scope<'_>,
    stream: &WritableStream<'_>,
    underlying_sink: HandleValue<'_>,
    underlying_sink_dict: &UnderlyingSink<'_>,
    high_water_mark: f64,
    size_algorithm: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Let _controller_ be a `new` ``WritableStreamDefaultController``.
    let controller = WritableStreamDefaultController::new(scope, *stream)?;
    // The write/close/abort/start algorithms are the sink's callbacks, invoked with `this` =
    // _underlyingSink_ (passed below as the algorithm receiver). An absent callback is no
    // algorithm, which the invoker treats as the resolved-undefined algorithm.
    // Step 2: Let _startAlgorithm_ be an algorithm that returns undefined.
    // Step 6: If _underlyingSinkDict_["``start``"] `exists`, then set _startAlgorithm_ to an
    //         algorithm which returns the result of `invoking` _underlyingSinkDict_["``start``"]
    //         with argument list « _controller_ », exception behavior "`rethrow`", and `callback
    //         this value` _underlyingSink_.
    let start_algorithm = AlgorithmArg::from_member(scope, underlying_sink_dict.start.as_ref());
    // Step 3: Let _writeAlgorithm_ be an algorithm that returns `a promise resolved with`
    //         undefined.
    // Step 7: If _underlyingSinkDict_["``write``"] `exists`, then set _writeAlgorithm_ to an
    //         algorithm which takes an argument _chunk_ and returns the result of `invoking`
    //         _underlyingSinkDict_["``write``"] with argument list « _chunk_, _controller_ » and
    //         `callback this value` _underlyingSink_.
    let write_algorithm = AlgorithmArg::from_member(scope, underlying_sink_dict.write.as_ref());
    // Step 4: Let _closeAlgorithm_ be an algorithm that returns `a promise resolved with`
    //         undefined.
    // Step 8: If _underlyingSinkDict_["``close``"] `exists`, then set _closeAlgorithm_ to an
    //         algorithm which returns the result of `invoking` _underlyingSinkDict_["``close``"]
    //         with argument list «» and `callback this value` _underlyingSink_.
    let close_algorithm = AlgorithmArg::from_member(scope, underlying_sink_dict.close.as_ref());
    // Step 5: Let _abortAlgorithm_ be an algorithm that returns `a promise resolved with`
    //         undefined.
    // Step 9: If _underlyingSinkDict_["``abort``"] `exists`, then set _abortAlgorithm_ to an
    //         algorithm which takes an argument _reason_ and returns the result of `invoking`
    //         _underlyingSinkDict_["``abort``"] with argument list « _reason_ » and `callback
    //         this value` _underlyingSink_.
    let abort_algorithm = AlgorithmArg::from_member(scope, underlying_sink_dict.abort.as_ref());
    // Step 10: Perform ? `SetUpWritableStreamDefaultController`(_stream_, _controller_,
    //          _startAlgorithm_, _writeAlgorithm_, _closeAlgorithm_, _abortAlgorithm_,
    //          _highWaterMark_, _sizeAlgorithm_).
    set_up_writable_stream_default_controller(
        scope,
        stream,
        &controller,
        start_algorithm,
        write_algorithm,
        close_algorithm,
        abort_algorithm,
        underlying_sink,
        high_water_mark,
        size_algorithm,
    )
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-advance-queue-if-needed>
/// WritableStreamDefaultControllerAdvanceQueueIfNeeded(controller) performs the following steps:
pub(crate) fn writable_stream_default_controller_advance_queue_if_needed(
    scope: &Scope<'_>,
    controller: &WritableStreamDefaultController<'_>,
) {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: If _controller_.`[[started]]` is false, return.
    if !controller.data().started {
        return;
    }
    // Step 3: If _stream_.`[[inFlightWriteRequest]]` is not undefined, return.
    if stream.data().in_flight_write_request.is_some() {
        return;
    }
    // Step 4: Let _state_ be _stream_.`[[state]]`.
    let state = stream.data().state;
    // Step 5: Assert: _state_ is not "`closed`" or "`errored`".
    debug_assert!(!matches!(
        state,
        WritableStreamState::Closed | WritableStreamState::Errored
    ));
    // Step 6: If _state_ is "`erroring`", Perform ! `WritableStreamFinishErroring`(_stream_).
    //         Return.
    if state == WritableStreamState::Erroring {
        writable_stream_finish_erroring(scope, &stream);
        return;
    }
    // Step 7: If _controller_.`[[queue]]` is empty, return.
    if controller.data().queue.is_empty() {
        return;
    }
    // Step 8: Let _value_ be ! `PeekQueueValue`(_controller_).
    // Step 9: If _value_ is the `close sentinel`, perform !
    //         `WritableStreamDefaultControllerProcessClose`(_controller_).
    let is_close_sentinel = matches!(
        controller.data().queue.front(),
        Some(ValueWithSize::CloseSentinel)
    );
    if is_close_sentinel {
        writable_stream_default_controller_process_close(scope, controller);
    } else {
        // Step 10: Otherwise, perform ! `WritableStreamDefaultControllerProcessWrite`(_controller_,
        //          _value_).
        let value = peek_queue_value(scope, &*controller.data());
        writable_stream_default_controller_process_write(scope, controller, value);
    }
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-clear-algorithms>
/// WritableStreamDefaultControllerClearAlgorithms(controller) is called once the stream is closed or errored and the algorithms will not be executed any more. By removing the algorithm references it permits the underlying sink object to be garbage collected even if the WritableStream itself is still referenced. This is observable using weak references. See tc39/proposal-weakrefs#31 for more detail. It performs the following steps:
pub(crate) fn writable_stream_default_controller_clear_algorithms(
    controller: &WritableStreamDefaultController<'_>,
) {
    // Step 1: Set _controller_.`[[writeAlgorithm]]` to undefined.
    controller.data_mut().write_algorithm = Algorithm::None;
    // Step 2: Set _controller_.`[[closeAlgorithm]]` to undefined.
    controller.data_mut().close_algorithm = Algorithm::None;
    // Step 3: Set _controller_.`[[abortAlgorithm]]` to undefined.
    controller.data_mut().abort_algorithm = Algorithm::None;
    // Step 4: Set _controller_.`[[strategySizeAlgorithm]]` to undefined.
    controller
        .data_mut()
        .strategy_size_algorithm
        .set(value::undefined());
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-close>
/// WritableStreamDefaultControllerClose(controller) performs the following steps:
pub(crate) fn writable_stream_default_controller_close(
    scope: &Scope<'_>,
    controller: &WritableStreamDefaultController<'_>,
) {
    // Step 1: Perform ! `EnqueueValueWithSize`(_controller_, `close sentinel`, 0).
    //         Size 0 leaves the total queue size unchanged.
    controller
        .data_mut()
        .queue
        .push_back(ValueWithSize::CloseSentinel);
    // Step 2: Perform ! `WritableStreamDefaultControllerAdvanceQueueIfNeeded`(_controller_).
    writable_stream_default_controller_advance_queue_if_needed(scope, controller);
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-error>
/// WritableStreamDefaultControllerError(controller, error) performs the following steps:
pub(crate) fn writable_stream_default_controller_error(
    scope: &Scope<'_>,
    controller: &WritableStreamDefaultController<'_>,
    error: HandleValue<'_>,
) {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: Assert: _stream_.`[[state]]` is "`writable`".
    debug_assert_eq!(stream.data().state, WritableStreamState::Writable);
    // Step 3: Perform ! `WritableStreamDefaultControllerClearAlgorithms`(_controller_).
    writable_stream_default_controller_clear_algorithms(controller);
    // Step 4: Perform ! `WritableStreamStartErroring`(_stream_, _error_).
    writable_stream_start_erroring(scope, &stream, error);
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-error-if-needed>
/// WritableStreamDefaultControllerErrorIfNeeded(controller, error) performs the following steps:
pub(crate) fn writable_stream_default_controller_error_if_needed(
    scope: &Scope<'_>,
    controller: &WritableStreamDefaultController<'_>,
    error: HandleValue<'_>,
) {
    // Step 1: If _controller_.`[[stream]]`.`[[state]]` is "`writable`", perform !
    //         `WritableStreamDefaultControllerError`(_controller_, _error_).
    let stream = controller.stream(scope);
    if stream.data().state == WritableStreamState::Writable {
        writable_stream_default_controller_error(scope, controller, error);
    }
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-get-backpressure>
/// WritableStreamDefaultControllerGetBackpressure(controller) performs the following steps:
pub(crate) fn writable_stream_default_controller_get_backpressure(
    controller: &WritableStreamDefaultController<'_>,
) -> bool {
    // Step 1: Let _desiredSize_ be ! `WritableStreamDefaultControllerGetDesiredSize`(_controller_).
    let desired_size = writable_stream_default_controller_get_desired_size(controller);
    // Step 2: Return true if _desiredSize_ ≤ 0, or false otherwise.
    desired_size <= 0.0
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-get-chunk-size>
pub(crate) fn writable_stream_default_controller_get_chunk_size(
    scope: &Scope<'_>,
    controller: &WritableStreamDefaultController<'_>,
    chunk: HandleValue<'_>,
) -> f64 {
    let size_algorithm = controller.data().strategy_size_algorithm.get(scope);
    // Step 1: If _controller_.`[[strategySizeAlgorithm]]` is undefined, then:
    if size_algorithm.is_undefined() {
        // Step 1.1: Assert: _controller_.`[[stream]]`.`[[state]]` is not "`writable`".
        //           (`undefined` also stands for `ExtractSizeAlgorithm`'s "algorithm that
        //           returns 1" here, so the assertion only holds for a cleared algorithm.)
        // Step 1.2: Return 1.
        return 1.0;
    }
    // Step 2: Let _returnValue_ be the result of performing
    //         _controller_.`[[strategySizeAlgorithm]]`, passing in _chunk_, and interpreting the
    //         result as a `completion record`.
    let return_value = support::invoke_algorithm(
        scope,
        support::AlgorithmArg::Js(size_algorithm),
        HandleValue::undefined(),
        &[chunk],
    )
    .and_then(|v| {
        use js::conversion::FromJSVal;
        f64::from_jsval_throwing(scope, v, ())
    });
    match return_value {
        // Step 4: Return _returnValue_.[[Value]].
        Ok(size) => size,
        // Step 3: If _returnValue_ is an abrupt completion, Perform !
        //         `WritableStreamDefaultControllerErrorIfNeeded`(_controller_,
        //         _returnValue_.[[Value]]). Return 1.
        Err(_) => {
            let value = take_pending_or_undefined(scope);
            writable_stream_default_controller_error_if_needed(scope, controller, value);
            1.0
        }
    }
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-get-desired-size>
/// WritableStreamDefaultControllerGetDesiredSize(controller) performs the following steps:
pub(crate) fn writable_stream_default_controller_get_desired_size(
    controller: &WritableStreamDefaultController<'_>,
) -> f64 {
    // Step 1: Return _controller_.`[[strategyHWM]]` − _controller_.`[[queueTotalSize]]`.
    controller.data().strategy_hwm - controller.data().queue_total_size
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-process-close>
/// WritableStreamDefaultControllerProcessClose(controller) performs the following steps:
pub(crate) fn writable_stream_default_controller_process_close(
    scope: &Scope<'_>,
    controller: &WritableStreamDefaultController<'_>,
) {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: Perform ! `WritableStreamMarkCloseRequestInFlight`(_stream_).
    writable_stream_mark_close_request_in_flight(&stream);
    // Step 3: Perform ! `DequeueValue`(_controller_).
    dequeue_value(scope, &mut *controller.data_mut());
    // Step 4: Assert: _controller_.`[[queue]]` is empty.
    debug_assert!(controller.data().queue.is_empty());
    // Step 5: Let _sinkClosePromise_ be the result of performing _controller_.`[[closeAlgorithm]]`.
    let close_algorithm = controller.data().close_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let sink_close_promise =
        support::invoke_promise_algorithm(scope, close_algorithm, receiver, EmptyArgs);
    // Step 6: Perform ! `WritableStreamDefaultControllerClearAlgorithms`(_controller_).
    writable_stream_default_controller_clear_algorithms(controller);
    // Step 7: `Upon fulfillment` of _sinkClosePromise_, Perform !
    //         `WritableStreamFinishInFlightClose`(_stream_).
    // Step 8: `Upon rejection` of _sinkClosePromise_ with reason _reason_, Perform !
    //         `WritableStreamFinishInFlightCloseWithError`(_stream_, _reason_).
    // (Steps 7 and 8 are implemented by `ws_close_promise_fulfilled` / `ws_close_promise_rejected`.)
    support::react(
        scope,
        &sink_close_promise,
        Some((ws_close_promise_fulfilled, stream)),
        Some((ws_close_promise_rejected, stream)),
    )
    .expect("attach close reactions");
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-process-write>
/// WritableStreamDefaultControllerProcessWrite(controller, chunk) performs the following steps:
pub(crate) fn writable_stream_default_controller_process_write(
    scope: &Scope<'_>,
    controller: &WritableStreamDefaultController<'_>,
    chunk: HandleValue<'_>,
) {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: Perform ! `WritableStreamMarkFirstWriteRequestInFlight`(_stream_).
    writable_stream_mark_first_write_request_in_flight(&stream);
    // Step 3: Let _sinkWritePromise_ be the result of performing _controller_.`[[writeAlgorithm]]`,
    //         passing in _chunk_.
    let write_algorithm = controller.data().write_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    // The from-underlying-sink write callback is invoked with « chunk, controller » (a native
    // write algorithm ignores the extra controller argument).
    let sink_write_promise = support::invoke_promise_algorithm(
        scope,
        write_algorithm,
        receiver,
        &[chunk, scope.root_value(controller.as_value())],
    );
    // Step 4: `Upon fulfillment` of _sinkWritePromise_, Perform !
    //         `WritableStreamFinishInFlightWrite`(_stream_). Let _state_ be _stream_.`[[state]]`.
    //         Assert: _state_ is "`writable`" or "`erroring`". Perform !
    //         `DequeueValue`(_controller_). If ! `WritableStreamCloseQueuedOrInFlight`(_stream_) is
    //         false and _state_ is "`writable`", Let _backpressure_ be !
    //         `WritableStreamDefaultControllerGetBackpressure`(_controller_). Perform !
    //         `WritableStreamUpdateBackpressure`(_stream_, _backpressure_). Perform !
    //         `WritableStreamDefaultControllerAdvanceQueueIfNeeded`(_controller_).
    // Step 5: `Upon rejection` of _sinkWritePromise_ with _reason_, If _stream_.`[[state]]` is
    //         "`writable`", perform !
    //         `WritableStreamDefaultControllerClearAlgorithms`(_controller_). Perform !
    //         `WritableStreamFinishInFlightWriteWithError`(_stream_, _reason_).
    // (Steps 4 and 5 are implemented by `ws_write_promise_fulfilled` /
    // `ws_write_promise_rejected`.)
    if controller.data().write_fulfilled_fn.is_none() {
        let fulfilled =
            Function::new_callback(scope, c"", 1, ws_write_promise_fulfilled, controller)
                .expect("create write reaction");
        controller.data_mut().write_fulfilled_fn.set(fulfilled);
        let rejected = Function::new_callback(scope, c"", 1, ws_write_promise_rejected, controller)
            .expect("create write reaction");
        controller.data_mut().write_rejected_fn.set(rejected);
    }
    let fulfilled = controller
        .data()
        .write_fulfilled_fn
        .get(scope)
        .expect("created above");
    let rejected = controller
        .data()
        .write_rejected_fn
        .get(scope)
        .expect("created above");
    sink_write_promise
        .add_reactions_ignoring_unhandled_rejection(scope, Some(*fulfilled), Some(*rejected))
        .expect("attach write reactions");
}

/// <https://streams.spec.whatwg.org/#writable-stream-default-controller-write>
/// WritableStreamDefaultControllerWrite(controller, chunk, chunkSize) performs the following steps:
pub(crate) fn writable_stream_default_controller_write(
    scope: &Scope<'_>,
    controller: &WritableStreamDefaultController<'_>,
    chunk: HandleValue<'_>,
    chunk_size: f64,
) {
    // Step 1: Let _enqueueResult_ be `EnqueueValueWithSize`(_controller_, _chunk_, _chunkSize_).
    // Step 2: If _enqueueResult_ is an abrupt completion, Perform !
    //         `WritableStreamDefaultControllerErrorIfNeeded`(_controller_,
    //         _enqueueResult_.[[Value]]). Return.
    if enqueue_value_with_size(scope, &mut *controller.data_mut(), chunk, chunk_size).is_err() {
        let value = take_pending_or_undefined(scope);
        writable_stream_default_controller_error_if_needed(scope, controller, value);
        return;
    }
    // Step 3: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 4: If ! `WritableStreamCloseQueuedOrInFlight`(_stream_) is false and
    //         _stream_.`[[state]]` is "`writable`", Let _backpressure_ be !
    //         `WritableStreamDefaultControllerGetBackpressure`(_controller_). Perform !
    //         `WritableStreamUpdateBackpressure`(_stream_, _backpressure_).
    if !writable_stream_close_queued_or_in_flight(&stream)
        && stream.data().state == WritableStreamState::Writable
    {
        let backpressure = writable_stream_default_controller_get_backpressure(controller);
        writable_stream_update_backpressure(scope, &stream, backpressure);
    }
    // Step 5: Perform ! `WritableStreamDefaultControllerAdvanceQueueIfNeeded`(_controller_).
    writable_stream_default_controller_advance_queue_if_needed(scope, controller);
}

/// <https://streams.spec.whatwg.org/#peek-queue-value>
/// PeekQueueValue(container) performs the following steps:
pub(crate) fn peek_queue_value<'r>(
    scope: &'r Scope<'_>,
    container: &impl QueueWithSizes,
) -> HandleValue<'r> {
    // Step 1: Assert: _container_ has [[queue]] and [[queueTotalSize]] internal slots.
    //         (Guaranteed by the `QueueWithSizes` trait bound.)
    // Step 2: Assert: _container_.[[queue]] is not `empty`.
    debug_assert!(!container.queue().is_empty());
    // Step 3: Let _valueWithSize_ be _container_.[[queue]][0].
    let value_with_size = container.queue().front().unwrap();
    // Step 4: Return _valueWithSize_’s `value`.
    //         (The close sentinel's value is never read, so it peeks as undefined.)
    match value_with_size {
        ValueWithSize::Chunk { value, .. } => value.get(scope),
        ValueWithSize::CloseSentinel => HandleValue::undefined(),
    }
}
