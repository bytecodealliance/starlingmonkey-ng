// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Standalone algorithms from <https://streams.spec.whatwg.org/>

use core_runtime::{jsclass, jsmethods};
use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::exception::take_pending_or_undefined;
use js::function::cast_payload;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::heap::RootedTraceableBox;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleObject, HandleValue, ToJSVal};
use js::{value, Function, Object, Promise};
use web_globals::signals::{AbortSignal, AbortSignalImpl};

use super::byob_reader::BYOBReader;
use super::byob_request::ReadableStreamBYOBRequest;
use super::byte_stream_controller::{
    ByteQueueEntry, PullIntoDescriptor, ReadableByteStreamController,
    ReadableByteStreamControllerImpl, ReaderType,
};
use super::default_controller::{
    ReadableStreamDefaultController, ReadableStreamDefaultControllerImpl,
};
use super::default_reader::DefaultReader;
use super::read_request::{ReadIntoRequest, ReadRequest};
use super::readable_stream::{ReadableStream, ReadableStreamImpl, ReadableStreamState};
use super::underlying_source::UnderlyingSource;
use super::unions::{HeapReadableStreamController, HeapReadableStreamReader, ReadableStreamReader};
use crate::algorithms::{
    composite_reason, dequeue_value, enqueue_value_with_size, invalid_state_error,
    is_non_negative_number, pair_parts, pair_payload, reset_queue, resolved_undefined_promise,
};
use crate::readable::default_reader::DefaultReaderImpl;
use crate::support;
use crate::support::{Algorithm, AlgorithmArg};
use crate::writable::algorithms::{
    acquire_writable_stream_default_writer, is_writable_stream_locked, writable_stream_abort,
    writable_stream_close_queued_or_in_flight,
    writable_stream_default_writer_close_with_error_propagation,
    writable_stream_default_writer_release, writable_stream_default_writer_write, writer_stream,
};
use crate::writable::default_writer::{
    WritableStreamDefaultWriter, WritableStreamDefaultWriterImpl,
};
use crate::writable::writable_stream::{WritableStream, WritableStreamImpl, WritableStreamState};
use web_globals::events::algorithms::ScriptStackState;

// ---------------------------------------------------------------------------
// Private accessors for the stream's `[[reader]]` slot.
// ---------------------------------------------------------------------------

/// The stream's `[[reader]]`, or `None` if the stream is unlocked.
fn stream_reader<'r>(
    scope: &'r Scope<'_>,
    stream: &ReadableStream<'_>,
) -> Option<ReadableStreamReader<'r>> {
    let reader = stream.data().reader.as_ref()?.get(scope);
    Some(reader)
}

/// The stream's `[[reader]]` as a default reader, or `None` if unlocked or
/// locked to a BYOB reader.
fn stream_default_reader<'r>(
    scope: &'r Scope<'_>,
    stream: &ReadableStream<'_>,
) -> Option<DefaultReader<'r>> {
    match stream_reader(scope, stream)? {
        ReadableStreamReader::Default(reader) => Some(reader),
        ReadableStreamReader::Byob(_) => None,
    }
}

/// The stream's `[[reader]]` as a BYOB reader, or `None` if unlocked or locked
/// to a default reader.
fn stream_byob_reader<'r>(
    scope: &'r Scope<'_>,
    stream: &ReadableStream<'_>,
) -> Option<BYOBReader<'r>> {
    match stream_reader(scope, stream)? {
        ReadableStreamReader::Byob(reader) => Some(reader),
        ReadableStreamReader::Default(_) => None,
    }
}

/// The `[[closedPromise]]` of the stream's reader, whichever reader type it is
/// (the `ReadableStreamGenericReader` mixin slot).
fn stream_reader_closed_promise<'r>(
    scope: &'r Scope<'_>,
    stream: &ReadableStream<'_>,
) -> Promise<'r> {
    stream_reader(scope, stream)
        .expect("a locked stream has a reader")
        .generic_closed_promise(scope)
}

/// The `ReadableStreamGenericReader` mixin (WHATWG Streams §4.8), shared by the
/// default and BYOB readers: access to the `[[stream]]` and `[[closedPromise]]`
/// slots. The generic reader operations (`ReadableStreamReaderGeneric*`) run
/// against any reader through this trait.
pub(crate) trait GenericReader {
    /// `[[stream]]` as a rooted stream, or `None` once released.
    fn generic_stream<'r>(&self, scope: &'r Scope<'_>) -> Option<ReadableStream<'r>>;
    /// Set `[[stream]]`.
    fn set_generic_stream(&self, stream: &ReadableStream<'_>);
    /// Set `[[stream]]` to undefined.
    fn clear_generic_stream(&self);
    /// `[[closedPromise]]` rooted.
    fn generic_closed_promise<'r>(&self, scope: &'r Scope<'_>) -> Promise<'r>;
    /// Set `[[closedPromise]]`.
    fn set_generic_closed_promise(&self, promise: Promise<'_>);
    /// The reader as a `ReadableStreamReader` union value.
    fn as_generic(&self) -> ReadableStreamReader<'_>;
}

impl GenericReader for DefaultReader<'_> {
    fn generic_stream<'r>(&self, scope: &'r Scope<'_>) -> Option<ReadableStream<'r>> {
        self.data().stream.get(scope)
    }
    fn set_generic_stream(&self, stream: &ReadableStream<'_>) {
        self.data_mut().stream.set(*stream);
    }
    fn clear_generic_stream(&self) {
        self.data_mut().stream = None;
    }
    fn generic_closed_promise<'r>(&self, scope: &'r Scope<'_>) -> Promise<'r> {
        self.data().closed_promise.get(scope)
    }
    fn set_generic_closed_promise(&self, promise: Promise<'_>) {
        self.data_mut().closed_promise.set(promise);
    }
    fn as_generic(&self) -> ReadableStreamReader<'_> {
        ReadableStreamReader::Default(*self)
    }
}

impl GenericReader for BYOBReader<'_> {
    fn generic_stream<'r>(&self, scope: &'r Scope<'_>) -> Option<ReadableStream<'r>> {
        self.data().stream.get(scope)
    }
    fn set_generic_stream(&self, stream: &ReadableStream<'_>) {
        self.data_mut().stream.set(*stream);
    }
    fn clear_generic_stream(&self) {
        self.data_mut().stream = None;
    }
    fn generic_closed_promise<'r>(&self, scope: &'r Scope<'_>) -> Promise<'r> {
        self.data().closed_promise.get(scope)
    }
    fn set_generic_closed_promise(&self, promise: Promise<'_>) {
        self.data_mut().closed_promise.set(promise);
    }
    fn as_generic(&self) -> ReadableStreamReader<'_> {
        ReadableStreamReader::Byob(*self)
    }
}

/// Construct a view over `buffer` and return it as a rooted value, panicking on
/// failure. Used where the spec performs an infallible `! Construct(...)` (e.g.
/// the empty view handed to a read-into request's close steps).
fn construct_view_or_throw<'r>(
    scope: &'r Scope<'_>,
    kind: js::typedarray::ViewKind,
    buffer: js::ArrayBuffer<'_>,
    byte_offset: usize,
    length: usize,
) -> HandleValue<'r> {
    let view = js::typedarray::construct_view(scope, kind, buffer, byte_offset, length)
        .expect("constructing a view over a fresh buffer");
    scope.root_value(view.as_value())
}

/// Handle an abrupt completion inside `DefaultControllerEnqueue`:
/// capture the pending exception value, error the controller with it, re-arm the
/// pending exception, and return `ExnThrown` so the caller re-throws the same
/// completion (the spec's "error the controller with result.[[Value]]; return
/// result").
fn error_controller_with_pending(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
) -> ExnThrown {
    let value = take_pending_or_undefined(scope);
    readable_stream_default_controller_error(scope, controller, value);
    js::exception::set_pending(
        scope,
        value,
        js::native::ExceptionStackBehavior::DoNotCapture,
    )
}

/// A native fulfillment reaction that ignores its argument and returns
/// undefined, used to implement the spec's "react to `p` with a fulfillment
/// step that returns undefined".
fn return_undefined(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    Ok(value::undefined())
}

// Reaction callbacks for `SetUpDefaultController`'s start promise
// and `DefaultControllerCallPullIfNeeded`'s pull promise. Each
// carries the controller as its payload value.

/// `SetUpDefaultController` step 11.
fn start_promise_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<ReadableStreamDefaultController>(scope, payload);
    // Set _controller_.[[started]] to true.
    controller.data_mut().started = true;
    debug_assert!(!controller.data().pulling);
    debug_assert!(!controller.data().pull_again);
    // Perform ! DefaultControllerCallPullIfNeeded(controller).
    readable_stream_default_controller_call_pull_if_needed(scope, &controller);
    Ok(value::undefined())
}

/// `SetUpDefaultController` step 12.
fn start_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<ReadableStreamDefaultController>(scope, payload);
    // Perform ! DefaultControllerError(controller, r).
    readable_stream_default_controller_error(scope, &controller, args.get(0));
    Ok(value::undefined())
}

/// `DefaultControllerCallPullIfNeeded` step 7.
fn pull_promise_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<ReadableStreamDefaultController>(scope, payload);
    // Set _controller_.[[pulling]] to false.
    controller.data_mut().pulling = false;
    // If _controller_.[[pullAgain]] is true, set it to false and call pull-if-needed again.
    if controller.data().pull_again {
        controller.data_mut().pull_again = false;
        readable_stream_default_controller_call_pull_if_needed(scope, &controller);
    }
    Ok(value::undefined())
}

/// `DefaultControllerCallPullIfNeeded` step 8.
fn pull_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<ReadableStreamDefaultController>(scope, payload);
    // Perform ! DefaultControllerError(controller, e).
    readable_stream_default_controller_error(scope, &controller, args.get(0));
    Ok(value::undefined())
}

// Reaction callbacks for the byte controller's start and pull promises, mirroring
// the default controller's above. Each carries the byte controller as its payload.

/// `SetUpByteStreamController` step 16.
fn byte_start_promise_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<ReadableByteStreamController>(scope, payload);
    // Set _controller_.[[started]] to true.
    controller.data_mut().started = true;
    debug_assert!(!controller.data().pulling);
    debug_assert!(!controller.data().pull_again);
    // Perform ! ByteStreamControllerCallPullIfNeeded(controller).
    readable_byte_stream_controller_call_pull_if_needed(scope, &controller);
    Ok(value::undefined())
}

/// `SetUpByteStreamController` step 17.
fn byte_start_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<ReadableByteStreamController>(scope, payload);
    // Perform ! ByteStreamControllerError(controller, r).
    readable_byte_stream_controller_error(scope, &controller, args.get(0));
    Ok(value::undefined())
}

/// `ByteStreamControllerCallPullIfNeeded` step 7.
fn byte_pull_promise_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<ReadableByteStreamController>(scope, payload);
    // Set _controller_.[[pulling]] to false.
    controller.data_mut().pulling = false;
    // If _controller_.[[pullAgain]] is true, set it to false and call pull-if-needed again.
    if controller.data().pull_again {
        controller.data_mut().pull_again = false;
        readable_byte_stream_controller_call_pull_if_needed(scope, &controller);
    }
    Ok(value::undefined())
}

/// `ByteStreamControllerCallPullIfNeeded` step 8.
fn byte_pull_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<ReadableByteStreamController>(scope, payload);
    // Perform ! ByteStreamControllerError(controller, e).
    readable_byte_stream_controller_error(scope, &controller, args.get(0));
    Ok(value::undefined())
}

/// The body of `ReadableStreamDefaultTee`'s pull algorithm (step 13's read
/// driving), shared by the pull callback and the read-request microtask's
/// `readAgain` re-pull.
fn tee_pull(scope: &Scope<'_>, state: TeeState<'_>) {
    // If _reading_ is true, Set _readAgain_ to true. Return.
    if state.data().reading {
        state.data_mut().read_again = true;
        return;
    }
    // Set _reading_ to true.
    state.data_mut().reading = true;
    // Let _readRequest_ be a `read request` ... Perform ! DefaultReaderRead.
    // Build the request directly into the call (after rooting the reader) so it
    // is never held as an untraced `#[must_root]` local across an allocation.
    let reader = state.data().reader.get(scope);
    readable_stream_default_reader_read(
        scope,
        reader,
        ReadRequest::Tee {
            state: Heap::from(state),
        },
    );
}

/// `ReadableStreamDefaultTee` step 13 pull algorithm callback.
fn tee_pull_native(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<TeeState>(scope, payload);
    tee_pull(scope, state);
    // Return `a promise resolved with` undefined.
    // Fully internal, so the per-global reused instance serves.
    Ok(Promise::shared_resolved_undefined(scope)?.as_value())
}

/// Shared cancelN body (returns the cancel promise as a value): payload = state, `which1`
/// selects branch 1 or 2.
fn tee_cancel(
    scope: &Scope<'_>,
    state: TeeState<'_>,
    reason: HandleValue<'_>,
    which1: bool,
) -> Value {
    // Set _canceledN_ to true. Set _reasonN_ to _reason_.
    let both_canceled = if which1 {
        state.data_mut().canceled1 = true;
        state.data_mut().reason1.set(*reason);
        state.data().canceled2
    } else {
        state.data_mut().canceled2 = true;
        state.data_mut().reason2.set(*reason);
        state.data().canceled1
    };

    let cancel_promise: Promise<'_> = state.data().cancel_promise.get(scope);

    // If the other branch is canceled, cancel the source with the composite
    // reason « _reason1_, _reason2_ » and resolve the cancel promise with the result.
    if both_canceled {
        let r1 = state.data().reason1.get(scope);
        let r2 = state.data().reason2.get(scope);
        let composite = composite_reason(scope, r1, r2).expect("composite reason");
        let stream = state.data().stream.get(scope);
        let cancel_result = readable_stream_cancel(scope, &stream, composite);
        cancel_promise
            .resolve(scope, cancel_result)
            .expect("resolve cancel");
    }
    // Return _cancelPromise_.
    cancel_promise.as_value()
}

/// `ReadableStreamDefaultTee` step 14 cancel1 algorithm (payload = state).
fn tee_cancel1_native(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<TeeState>(scope, payload);
    Ok(tee_cancel(scope, state, args.get(0), true))
}

/// `ReadableStreamDefaultTee` step 15 cancel2 algorithm (payload = state).
fn tee_cancel2_native(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<TeeState>(scope, payload);
    Ok(tee_cancel(scope, state, args.get(0), false))
}

/// `ReadableStreamDefaultTee` step 19: reader closed-promise rejection (payload = state).
fn tee_closed_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<TeeState>(scope, payload);
    let r = args.get(0);
    let branch1_controller = state.data().branch1_controller.get(scope);
    let branch2_controller = state.data().branch2_controller.get(scope);
    readable_stream_default_controller_error(scope, &branch1_controller, r);
    readable_stream_default_controller_error(scope, &branch2_controller, r);
    if !state.data().canceled1 || !state.data().canceled2 {
        state
            .data()
            .cancel_promise
            .get(scope)
            .resolve(scope, HandleValue::undefined())?;
    }
    Ok(value::undefined())
}

/// Tee branches are created with the default (constant-1) size algorithm, so
/// their controllers' `enqueue` can never run the size algorithm and therefore
/// never throws — matching the spec's `! ReadableStreamDefaultControllerEnqueue`.
const TEE_ENQUEUE_INFALLIBLE: &str =
    "tee branch enqueue is infallible: branches use the default constant-1 size algorithm";

/// The chunk-steps microtask of the tee read request.
fn tee_chunk_microtask(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<TeeState>(scope, payload);
    let chunk = state.data().pending_chunk.get(scope);
    state.data().pending_chunk.set(value::undefined());
    // Set _readAgain_ to false.
    state.data_mut().read_again = false;
    // Let _chunk1_ and _chunk2_ be _chunk_.
    // _chunk1_ is the unmodified `chunk`; _chunk2_ may be replaced by a structured clone
    // below when _cloneForBranch2_ is true.
    let mut chunk2 = chunk;
    // If _canceled2_ is false and _cloneForBranch2_ is true,
    if !state.data().canceled2 && state.data().clone_for_branch2 {
        // Let _cloneResult_ be `StructuredClone`(_chunk2_).
        // SAFETY: a default-realm structured clone with no custom callbacks; the buffer is owned
        // and dropped by the wrapper.
        match unsafe {
            js::structured_clone::clone(scope, chunk2, std::ptr::null(), std::ptr::null_mut())
        } {
            // Otherwise, set _chunk2_ to _cloneResult_.`[[Value]]`.
            Ok(cloned) => chunk2 = cloned,
            // If _cloneResult_ is an abrupt completion,
            Err(_) => {
                let clone_err = take_pending_or_undefined(scope);
                // Perform ! `ReadableStreamDefaultControllerError`(branch1.`[[controller]]`,
                //          _cloneResult_.`[[Value]]`).
                let branch1_controller = state.data().branch1_controller.get(scope);
                readable_stream_default_controller_error(scope, &branch1_controller, clone_err);
                // Perform ! `ReadableStreamDefaultControllerError`(branch2.`[[controller]]`,
                //          _cloneResult_.`[[Value]]`).
                let branch2_controller = state.data().branch2_controller.get(scope);
                readable_stream_default_controller_error(scope, &branch2_controller, clone_err);
                // `Resolve` _cancelPromise_ with ! `ReadableStreamCancel`(_stream_,
                //          _cloneResult_.`[[Value]]`).
                let stream = state.data().stream.get(scope);
                let cancel_result = readable_stream_cancel(scope, &stream, clone_err);
                state
                    .data()
                    .cancel_promise
                    .get(scope)
                    .resolve(scope, cancel_result)
                    .expect("resolve cancel");
                // Return.
                return Ok(value::undefined());
            }
        }
    }
    // If _canceled1_ is false, perform ! `ReadableStreamDefaultControllerEnqueue`(branch1, chunk1).
    if !state.data().canceled1 {
        let branch1_controller = state.data().branch1_controller.get(scope);
        readable_stream_default_controller_enqueue(scope, &branch1_controller, chunk)
            .expect(TEE_ENQUEUE_INFALLIBLE);
    }
    // If _canceled2_ is false, perform ! `ReadableStreamDefaultControllerEnqueue`(branch2, chunk2).
    if !state.data().canceled2 {
        let branch2_controller = state.data().branch2_controller.get(scope);
        readable_stream_default_controller_enqueue(scope, &branch2_controller, chunk2)
            .expect(TEE_ENQUEUE_INFALLIBLE);
    }
    // Set _reading_ to false. If _readAgain_ is true, perform _pullAlgorithm_.
    state.data_mut().reading = false;
    if state.data().read_again {
        tee_pull(scope, state);
    }
    Ok(value::undefined())
}

/// The tee read request's `chunk steps`: queue a microtask to drive both branches.
/// <https://streams.spec.whatwg.org/#ref-for-read-request-chunk-steps%E2%91%A2>
pub(crate) fn tee_read_request_chunk_steps(
    scope: &Scope<'_>,
    state: TeeState<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    state.data().pending_chunk.set(*chunk);
    let microtask = state.data().chunk_microtask_fn.get(scope);
    js::jobs::queue_microtask(scope, &microtask)
}

/// The tee read request's `close steps`: close both non-canceled branches and resolve the cancel
/// promise.
pub(crate) fn tee_read_request_close_steps(
    scope: &Scope<'_>,
    state: TeeState<'_>,
) -> Result<(), ExnThrown> {
    // Set _reading_ to false.
    state.data_mut().reading = false;
    // If _canceled1_ is false, close branch1.
    if !state.data().canceled1 {
        let branch1_controller = state.data().branch1_controller.get(scope);
        readable_stream_default_controller_close(scope, &branch1_controller);
    }
    // If _canceled2_ is false, close branch2.
    if !state.data().canceled2 {
        let branch2_controller = state.data().branch2_controller.get(scope);
        readable_stream_default_controller_close(scope, &branch2_controller);
    }
    // If _canceled1_ is false or _canceled2_ is false, resolve _cancelPromise_ with undefined.
    if !state.data().canceled1 || !state.data().canceled2 {
        state
            .data()
            .cancel_promise
            .get(scope)
            .resolve(scope, HandleValue::undefined())
    } else {
        Ok(())
    }
}

/// The tee read request's `error steps`: just clear the reading flag.
pub(crate) fn tee_read_request_error_steps(
    _scope: &Scope<'_>,
    state: TeeState<'_>,
    _e: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Set _reading_ to false.
    state.data_mut().reading = false;
    Ok(())
}

/// <https://streams.spec.whatwg.org/#rs-default-controller-private-cancel>
/// [[CancelSteps]](reason) implements the [[CancelSteps]] contract. It performs the following steps:
pub(crate) fn cancel_steps<'r>(
    scope: &'r Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
    reason: HandleValue<'r>,
) -> Promise<'r> {
    // Step 1: Perform ! `ResetQueue`(`this`).
    reset_queue(&mut *controller.data_mut());
    // Step 2: Let _result_ be the result of performing `this`.`[[cancelAlgorithm]]`, passing
    //         _reason_.
    let cancel_algorithm = controller.data().cancel_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let result = support::invoke_promise_algorithm(scope, cancel_algorithm, receiver, &[reason]);
    // Step 3: Perform ! `DefaultControllerClearAlgorithms`(`this`).
    readable_stream_default_controller_clear_algorithms(controller);
    // Step 4: Return _result_.
    result
}

/// <https://streams.spec.whatwg.org/#rs-default-controller-private-pull>
/// [[PullSteps]](readRequest) implements the [[PullSteps]] contract. It performs the following steps:
pub(crate) fn pull_steps(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
    read_request: &mut RootedTraceableBox<Option<ReadRequest>>,
) {
    // Step 1: Let _stream_ be `this`.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: If `this`.`[[queue]]` is not `empty`, Let _chunk_ be ! `DequeueValue`(`this`). If
    //         `this`.`[[closeRequested]]` is true and `this`.`[[queue]]` `is empty`, Perform !
    //         `DefaultControllerClearAlgorithms`(`this`). Perform !
    //         `ReadableStreamClose`(_stream_). Otherwise, perform !
    //         `DefaultControllerCallPullIfNeeded`(`this`). Perform _readRequest_’s
    //         `chunk steps`, given _chunk_.
    if !controller.data().queue.is_empty() {
        let chunk = dequeue_value(scope, &mut *controller.data_mut());
        if controller.data().close_requested && controller.data().queue.is_empty() {
            readable_stream_default_controller_clear_algorithms(controller);
            readable_stream_close(scope, &stream);
        } else {
            readable_stream_default_controller_call_pull_if_needed(scope, controller);
        }
        read_request
            .take()
            .unwrap()
            .root(scope)
            .chunk_steps(scope, chunk);
    } else {
        // Step 3: Otherwise, Perform ! `ReadableStreamAddReadRequest`(_stream_, _readRequest_).
        //         Perform ! `DefaultControllerCallPullIfNeeded`(`this`).
        readable_stream_add_read_request(scope, &stream, read_request.take().unwrap());
        readable_stream_default_controller_call_pull_if_needed(scope, controller);
    }
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-ReadableStreamDefaultController-releasesteps>
/// [[ReleaseSteps]]() implements the [[ReleaseSteps]] contract. It performs the following steps:
pub(crate) fn release_steps() {
    // Step 1: Return.
}

/// <https://streams.spec.whatwg.org/#rbs-controller-private-cancel>
/// [[CancelSteps]](reason) implements the [[CancelSteps]] contract. It performs the following steps:
pub(crate) fn byte_cancel_steps<'r>(
    scope: &'r Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    reason: HandleValue<'r>,
) -> Promise<'r> {
    // Step 1: Perform ! `ByteStreamControllerClearPendingPullIntos`(`this`).
    readable_byte_stream_controller_clear_pending_pull_intos(scope, controller);
    // Step 2: Perform ! `ResetQueue`(`this`).
    reset_byte_queue(controller);
    // Step 3: Let _result_ be the result of performing `this`.`[[cancelAlgorithm]]`, passing in
    //         _reason_.
    let cancel_algorithm = controller.data().cancel_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let result = support::invoke_promise_algorithm(scope, cancel_algorithm, receiver, &[reason]);
    // Step 4: Perform ! `ByteStreamControllerClearAlgorithms`(`this`).
    readable_byte_stream_controller_clear_algorithms(controller);
    // Step 5: Return _result_.
    result
}

/// <https://streams.spec.whatwg.org/#rbs-controller-private-pull>
/// [[PullSteps]](readRequest) implements the [[PullSteps]] contract. It performs the following steps:
pub(crate) fn byte_pull_steps(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    read_request: &mut RootedTraceableBox<Option<ReadRequest>>,
) {
    // Step 1: Let _stream_ be `this`.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: Assert: ! `ReadableStreamHasDefaultReader`(_stream_) is true.
    debug_assert!(readable_stream_has_default_reader(scope, &stream));
    // Step 3: If `this`.`[[queueTotalSize]]` > 0, Assert: !
    //         `ReadableStreamGetNumReadRequests`(_stream_) is 0. Perform !
    //         `ByteStreamControllerFillReadRequestFromQueue`(`this`, _readRequest_).
    //         Return.
    if controller.data().queue_total_size > 0.0 {
        debug_assert_eq!(readable_stream_get_num_read_requests(scope, &stream), 0);
        readable_byte_stream_controller_fill_read_request_from_queue(
            scope,
            controller,
            read_request,
        )
        .expect("fill read request from queue");
        return;
    }
    // Step 4: Let _autoAllocateChunkSize_ be `this`.`[[autoAllocateChunkSize]]`.
    let auto_allocate_chunk_size = controller.data().auto_allocate_chunk_size;
    // Step 5: If _autoAllocateChunkSize_ is not undefined, Let _buffer_ be
    //         `Construct`(``%ArrayBuffer%``, « _autoAllocateChunkSize_ »). If _buffer_ is an abrupt
    //         completion, Perform _readRequest_’s `error steps`, given _buffer_.[[Value]]. Return.
    //         Let _pullIntoDescriptor_ be a new `pull-into descriptor` with `buffer`
    //         _buffer_.[[Value]], `buffer byte length` _autoAllocateChunkSize_, `byte offset` 0,
    //         `byte length` _autoAllocateChunkSize_, `bytes filled` 0, `minimum fill` 1, `element
    //         size` 1, `view constructor` ``%Uint8Array%``, and `reader type` "`default`".
    //         `Append` _pullIntoDescriptor_ to `this`.`[[pendingPullIntos]]`.
    if let Some(size) = auto_allocate_chunk_size {
        let size = size as usize;
        let buffer = match js::ArrayBuffer::new(scope, size) {
            Ok(b) => b,
            Err(_) => {
                let error = take_pending_or_undefined(scope);
                read_request
                    .take()
                    .unwrap()
                    .root(scope)
                    .error_steps(scope, error);
                return;
            }
        };
        controller
            .data_mut()
            .pending_pull_intos
            .push_back(PullIntoDescriptor {
                buffer: Heap::from(buffer),
                buffer_byte_length: size,
                byte_offset: 0,
                byte_length: size,
                bytes_filled: 0,
                minimum_fill: 1,
                element_size: 1,
                view_kind: js::typedarray::ViewKind::Uint8,
                reader_type: ReaderType::Default,
            });
    }
    // Step 6: Perform ! `ReadableStreamAddReadRequest`(_stream_, _readRequest_).
    readable_stream_add_read_request(scope, &stream, read_request.take().unwrap());
    // Step 7: Perform ! `ByteStreamControllerCallPullIfNeeded`(`this`).
    readable_byte_stream_controller_call_pull_if_needed(scope, controller);
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-ReadableByteStreamController-releasesteps>
/// [[ReleaseSteps]]() implements the [[ReleaseSteps]] contract. It performs the following steps:
pub(crate) fn byte_release_steps(controller: &ReadableByteStreamController<'_>) {
    // Step 1: If `this`.`[[pendingPullIntos]]` is not empty, Let _firstPendingPullInto_ be
    //         `this`.`[[pendingPullIntos]]`[0]. Set _firstPendingPullInto_’s `reader type` to
    //         "`none`". Set `this`.`[[pendingPullIntos]]` to the list « _firstPendingPullInto_ ».
    if !controller.data().pending_pull_intos.is_empty() {
        // Set the first descriptor's reader type and drop the rest in place,
        // without moving a `#[must_root]` descriptor out of the traced list.
        let mut data = controller.data_mut();
        data.pending_pull_intos[0].reader_type = ReaderType::None;
        data.pending_pull_intos.truncate(1);
    }
}

/// <https://streams.spec.whatwg.org/#acquire-readable-stream-byob-reader>
/// AcquireBYOBReader(stream) performs the following steps:
pub(crate) fn acquire_readable_stream_byob_reader<'r>(
    scope: &'r Scope<'_>,
    stream: &ReadableStream<'_>,
) -> Result<BYOBReader<'r>, ExnThrown> {
    // Step 1: Let _reader_ be a `new` ``BYOBReader``.
    // Step 2: Perform ? `SetUpBYOBReader`(_reader_, _stream_).
    //         The reader's constructor runs SetUpBYOBReader, so creating it via the
    //         factory performs both steps; the factory propagates the setup's exception.
    let reader = BYOBReader::new(scope, *stream)?;
    // Step 3: Return _reader_.
    Ok(reader)
}

/// <https://streams.spec.whatwg.org/#acquire-readable-stream-reader>
/// AcquireDefaultReader(stream) performs the following steps:
pub(crate) fn acquire_readable_stream_default_reader<'r>(
    scope: &'r Scope<'_>,
    stream: &ReadableStream<'_>,
) -> Result<DefaultReader<'r>, ExnThrown> {
    // Step 1: Let _reader_ be a `new` ``DefaultReader``.
    // Step 2: Perform ? `SetUpDefaultReader`(_reader_, _stream_).
    //         The reader's constructor runs SetUpDefaultReader, so creating it via
    //         the factory performs both steps; the factory propagates the setup's exception.
    let reader = DefaultReader::new(scope, *stream)?;
    // Step 3: Return _reader_.
    Ok(reader)
}

/// <https://streams.spec.whatwg.org/#create-readable-stream>
/// CreateReadableStream(startAlgorithm, pullAlgorithm, cancelAlgorithm[, highWaterMark, [, sizeAlgorithm]]) performs the following steps:
pub(crate) fn create_readable_stream<'r>(
    scope: &'r Scope<'_>,
    start_algorithm: AlgorithmArg<'r>,
    pull_algorithm: AlgorithmArg<'r>,
    cancel_algorithm: AlgorithmArg<'r>,
    high_water_mark: f64,
    size_algorithm: impl ToJSVal<'r>,
) -> Result<ReadableStream<'r>, ExnThrown> {
    // Step 1: If _highWaterMark_ was not passed, set it to 1. (Passed by the caller.)
    // Step 2: If _sizeAlgorithm_ was not passed, set it to an algorithm that returns 1. (Passed.)
    // Step 3: Assert: ! `IsNonNegativeNumber`(_highWaterMark_) is true.
    debug_assert!(is_non_negative_number(high_water_mark));
    // Step 4: Let _stream_ be a `new` ``ReadableStream``.
    let stream = js::class::create_instance_with::<ReadableStreamImpl>(scope, |_| {
        ReadableStreamImpl::default()
    })?;
    // Step 5: Perform ! `InitializeReadableStream`(_stream_).
    initialize_readable_stream(&stream);
    // Step 6: Let _controller_ be a `new` ``ReadableStreamDefaultController``.
    let controller = ReadableStreamDefaultController::new(scope, stream)?;
    // Step 7: Perform ? `SetUpDefaultController`(_stream_, _controller_,
    //         _startAlgorithm_, _pullAlgorithm_, _cancelAlgorithm_, _highWaterMark_,
    //         _sizeAlgorithm_).
    //         The algorithms are native (no JS receiver), so `algorithm_receiver` is undefined.
    let receiver = HandleValue::undefined();
    set_up_readable_stream_default_controller(
        scope,
        &stream,
        &controller,
        start_algorithm,
        pull_algorithm,
        cancel_algorithm,
        receiver,
        high_water_mark,
        size_algorithm.to_jsval_throwing(scope)?,
    )?;
    // Step 8: Return _stream_.
    Ok(stream)
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-createreadablebytestream>
/// CreateReadableByteStream(startAlgorithm, pullAlgorithm, cancelAlgorithm) performs the following steps:
pub(crate) fn create_readable_byte_stream<'r>(
    scope: &'r Scope<'_>,
    start_algorithm: AlgorithmArg<'r>,
    pull_algorithm: AlgorithmArg<'r>,
    cancel_algorithm: AlgorithmArg<'r>,
) -> Result<ReadableStream<'r>, ExnThrown> {
    // Step 1: Let _stream_ be a `new` ``ReadableStream``.
    let stream = js::class::create_instance_with::<ReadableStreamImpl>(scope, |_| {
        ReadableStreamImpl::default()
    })?;
    // Step 2: Perform ! `InitializeReadableStream`(_stream_).
    initialize_readable_stream(&stream);
    // Step 3: Let _controller_ be a `new` ``ReadableByteStreamController``.
    let controller = ReadableByteStreamController::new(scope, stream)?;
    // Step 4: Perform ? `SetUpByteStreamController`(_stream_, _controller_,
    //         _startAlgorithm_, _pullAlgorithm_, _cancelAlgorithm_, 0, undefined).
    //         The algorithms are native (no JS receiver), so `algorithm_receiver` is undefined.
    let receiver = HandleValue::undefined();
    set_up_readable_byte_stream_controller(
        scope,
        &stream,
        &controller,
        start_algorithm,
        pull_algorithm,
        cancel_algorithm,
        receiver,
        0.0,
        None,
    )?;
    // Step 5: Return _stream_.
    Ok(stream)
}

/// <https://streams.spec.whatwg.org/#initialize-readable-stream>
/// InitializeReadableStream(stream) performs the following steps:
pub(crate) fn initialize_readable_stream(stream: &ReadableStream<'_>) {
    let mut data = stream.data_mut();
    // Step 1: Set _stream_.`[[state]]` to "`readable`".
    data.state = ReadableStreamState::Readable;
    // Step 2: Set _stream_.`[[reader]]` and _stream_.`[[storedError]]` to undefined.
    data.reader = None;
    data.stored_error.set(value::undefined());
    // Step 3: Set _stream_.`[[disturbed]]` to false.
    data.disturbed = false;
}

/// <https://streams.spec.whatwg.org/#readable-stream-from-iterable>
/// ReadableStreamFromIterable(asyncIterable) performs the following steps:
///
/// The iterator record and the `async`/`sync` distinction are captured in a
/// `js::iteration::AsyncIteratorRecord`, held by a GC-traced
/// `FromIterableState` that backs the pull and cancel algorithms (native
/// `Function::new_callback`s carrying the state as their payload). For a sync
/// iterable, `[[NextMethod]]` is the record's native async-from-sync `next`,
/// so `IteratorNext` uniformly yields a promise of an iterator result.
pub(crate) fn readable_stream_from_iterable<'r>(
    scope: &'r Scope<'_>,
    async_iterable: HandleValue<'_>,
) -> Result<ReadableStream<'r>, ExnThrown> {
    // Step 1: Let _stream_ be undefined.
    // Step 2: Let _iteratorRecord_ be ? `GetIterator`(_asyncIterable_, async).
    let state = FromIterableState::new(scope, async_iterable)?;
    // Step 3: Let _startAlgorithm_ be an algorithm that returns undefined.
    // Step 4: Let _pullAlgorithm_ be the following steps: Let _nextResult_ be
    //         `IteratorNext`(_iteratorRecord_). If _nextResult_ is an abrupt completion, return `a
    //         promise rejected with` _nextResult_.[[Value]]. Let _nextPromise_ be `a promise
    //         resolved with` _nextResult_.[[Value]]. Return the result of `reacting` to
    //         _nextPromise_ with the following fulfillment steps, given _iterResult_: If
    //         _iterResult_ `is not an Object`, throw a ``TypeError``. Let _done_ be ?
    //         `IteratorComplete`(_iterResult_). If _done_ is true: Perform !
    //         `DefaultControllerClose`(_stream_.`[[controller]]`). Otherwise: Let
    //         _value_ be ? `IteratorValue`(_iterResult_). Perform !
    //         `DefaultControllerEnqueue`(_stream_.`[[controller]]`, _value_).
    // Step 5: Let _cancelAlgorithm_ be the following steps, given _reason_: Let _iterator_ be
    //         _iteratorRecord_.[[Iterator]]. Let _returnMethod_ be `GetMethod`(_iterator_,
    //         "`return`"). If _returnMethod_ is an abrupt completion, return `a promise rejected
    //         with` _returnMethod_.[[Value]]. If _returnMethod_.[[Value]] is undefined, return `a
    //         promise resolved with` undefined. Let _returnResult_ be
    //         `Call`(_returnMethod_.[[Value]], _iterator_, « _reason_ »). If _returnResult_ is an
    //         abrupt completion, return `a promise rejected with` _returnResult_.[[Value]]. Let
    //         _returnPromise_ be `a promise resolved with` _returnResult_.[[Value]]. Return the
    //         result of `reacting` to _returnPromise_ with the following fulfillment steps, given
    //         _iterResult_: If _iterResult_ `is not an Object`, throw a ``TypeError``. Return
    //         undefined.
    // Step 6: Set _stream_ to ! `CreateReadableStream`(_startAlgorithm_, _pullAlgorithm_,
    //         _cancelAlgorithm_, 0).
    //         (Step 3's start algorithm is the default undefined; steps 4 and 5's pull/cancel
    //         algorithms are the native `from_pull_native` / `from_cancel_native`, carrying the
    //         iterator state as their payload.)
    let pull = AlgorithmArg::native(scope, from_pull_native, state)?;
    let cancel = AlgorithmArg::native(scope, from_cancel_native, state)?;
    let stream = create_readable_stream(
        scope,
        AlgorithmArg::None,
        pull,
        cancel,
        0.0,
        HandleValue::undefined(),
    )?;
    // Step 7: Return _stream_.
    Ok(stream)
}

/// The internal state backing `ReadableStreamFromIterable`'s pull and cancel
/// algorithms.
#[jsclass(hidden)]
pub(crate) struct FromIterableState {
    /// The `GetIterator(asyncIterable, async)` record driving the iteration.
    record: Heap<js::iteration::AsyncIteratorRecordImpl>,
    /// The pull algorithm's fulfillment callback (payload = the stream's
    /// controller), created on the first pull and reused for every subsequent
    /// iteration. `None` until then.
    pull_fulfilled_fn: Option<Heap<js::function::Function>>,
}

#[jsmethods]
impl FromIterableState<'_> {
    /// Step 2: Let _iteratorRecord_ be ? `GetIterator`(_asyncIterable_, async).
    fn new(&self, scope: &Scope<'_>, async_iterable: HandleValue<'_>) -> Result<(), ExnThrown> {
        let record = js::iteration::get_async_iterator(scope, async_iterable)?;
        self.data_mut().record.set(record);
        Ok(())
    }
}

/// `ReadableStreamFromIterable` step 4 pull algorithm (payload = state, arg 0 =
/// the controller).
fn from_pull_native(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromIterableState>(scope, payload);
    let controller = args.get(0);
    let record: js::iteration::AsyncIteratorRecord<'_> = state.data().record.get(scope);
    // Let _nextResult_ be `IteratorNext`(_iteratorRecord_). If abrupt, return a
    // promise rejected with its value.
    let next_result = match record.call_next(scope) {
        Ok(r) => r,
        Err(_) => {
            return Ok(Promise::new_rejected_with_pending_error(scope)?.as_value());
        }
    };
    // Let _nextPromise_ be a promise resolved with _nextResult_.
    let next_promise = Promise::call_original_resolve(scope, next_result)?;
    // React to _nextPromise_ with the fulfillment steps (`from_pull_fulfilled`).
    // The callback's payload — the stream's controller — is the same for every
    // pull, so it is created once and reused for every subsequent iteration.
    if state.data().pull_fulfilled_fn.is_none() {
        let cb = Function::new_callback(scope, c"", 1, from_pull_fulfilled, controller)?;
        state.data_mut().pull_fulfilled_fn.set(cb);
    }
    let cb = state
        .data()
        .pull_fulfilled_fn
        .get(scope)
        .expect("created above");
    Ok(next_promise.then(scope, Some(*cb), None)?.as_value())
}

/// The pull algorithm's fulfillment steps (payload = the controller, arg 0 = the
/// iterator result): close on done, otherwise enqueue the value.
fn from_pull_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let iter_result = args.get(0);
    let controller = cast_payload::<ReadableStreamDefaultController>(scope, payload);
    // If _iterResult_ is not an Object, throw a TypeError.
    let result_obj = js::iteration::iter_result_object(scope, iter_result)?;
    // Let _done_ be ? `IteratorComplete`(_iterResult_).
    let done = js::iteration::iter_result_done(scope, &result_obj)?;
    if done {
        readable_stream_default_controller_close(scope, &controller);
    } else {
        // Let _value_ be ? `IteratorValue`(_iterResult_).
        let value = js::iteration::iter_result_value(scope, &result_obj)?;
        readable_stream_default_controller_enqueue(scope, &controller, value)?;
    }
    Ok(value::undefined())
}

/// `ReadableStreamFromIterable` step 5 cancel algorithm (payload = state, arg 0 =
/// the cancellation reason): the iterator's `return`-method path, delegated to
/// `AsyncIteratorRecord::return_promise`.
fn from_cancel_native(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromIterableState>(scope, payload);
    let record: js::iteration::AsyncIteratorRecord<'_> = state.data().record.get(scope);
    let p = record.return_promise(scope, args.get(0))?;
    Ok(p.as_value())
}

/// <https://streams.spec.whatwg.org/#readable-stream-pipe-to>
/// ReadableStreamPipeTo(source, dest, preventClose, preventAbort, preventCancel[, signal]) performs the following steps:
///
/// The pipe operation runs across many native promise reactions. Its shared
/// mutable state (the reader, writer, flags, the parked chunk and pending-write
/// count, the result `promise`, and the pending shutdown action) lives in a
/// `PipeState` so it is GC-traced and reachable through the callbacks' payload;
/// see the `pipe_*` helpers below.
pub(crate) fn readable_stream_pipe_to<'r>(
    scope: &'r Scope<'_>,
    source: &ReadableStream<'_>,
    dest: &WritableStream<'_>,
    prevent_close: bool,
    prevent_abort: bool,
    prevent_cancel: bool,
    signal: Option<AbortSignal<'_>>,
) -> Result<Promise<'r>, ExnThrown> {
    // Step 1: Assert: _source_ `implements` ``ReadableStream``.
    // Step 2: Assert: _dest_ `implements` ``WritableStream``.
    // Step 3: Assert: _preventClose_, _preventAbort_, and _preventCancel_ are all booleans.
    // Step 4: If _signal_ was not given, let _signal_ be undefined.
    // Step 5: Assert: either _signal_ is undefined, or _signal_ `implements` ``AbortSignal``.
    // Steps 1-5 are type system enforced.

    // Step 6: Assert: ! `IsReadableStreamLocked`(_source_) is false.
    debug_assert!(!source.is_locked());

    // Step 7: Assert: ! `IsWritableStreamLocked`(_dest_) is false.
    debug_assert!(!is_writable_stream_locked(dest));

    // Step 8: If _source_.`[[controller]]` `implements` ``ReadableByteStreamController``, let
    //         _reader_ be either ! `AcquireBYOBReader`(_source_) or !
    //         `AcquireDefaultReader`(_source_), at the user agent’s discretion.
    //         (We use the default reader for byte streams too: the pipe loop reads chunks and
    //         never supplies a view.)
    // Step 9: Otherwise, let _reader_ be ! `AcquireDefaultReader`(_source_).
    let reader = acquire_readable_stream_default_reader(scope, source)?;

    // Step 10: Let _writer_ be ! `AcquireDefaultWriter`(_dest_).
    let writer = acquire_writable_stream_default_writer(scope, dest)?;

    // Step 11: Set _source_.`[[disturbed]]` to true.
    source.data_mut().disturbed = true;

    // Not a spec step: if `source` is backed by a native source and `dest` is the writable
    // end of an identity `TransformStream`, set everything up to enable taking shortcuts
    // between native sources and sinks later on:
    // - Apply the input stream's native source to the TS's readable stream.
    // - Set the TS on the input stream, needed for `ReadableStream::deferred_demand`
    //
    // The first of these gives native sinks access to the native source, while the second
    // one allows native sources to detect that a `pull` is triggered by a TransformStream,
    // and defer acting on it until there's actual demand.
    if let Some(host_source) = source.native_source(scope) {
        let transform = dest.data().identity_transform.get(scope);
        if let Some(transform) = transform {
            // Only when this pipe will close the destination behind it. Handing the native source
            // on says "the whole of what comes out of this transform is that one body", and a sink
            // that takes it up bypasses the transform entirely — so anything written into the
            // transform afterwards would go to a readable end nobody reads, stalling its writer and
            // never reaching the wire. This pipe holds the writer lock until it is done and then
            // closes the destination, so nothing can follow it; `preventClose` is precisely the
            // author saying they intend to write more, and disqualifies the claim.
            if !prevent_close {
                transform
                    .data()
                    .readable
                    .get(scope)
                    .set_native_source(&host_source);
            }
            source.data_mut().piped_to_identity_transform.set(transform);
        }
    }

    // Step 12: Let _shuttingDown_ be false.
    // (implicit)

    // Step 13: Let _promise_ be `a new promise`.
    // Step 14: [..]
    let state = PipeState::new(
        scope,
        *source,
        *dest,
        reader,
        writer,
        prevent_close,
        prevent_abort,
        prevent_cancel,
        signal,
    )?;

    // Step 14.2: If _signal_ is `aborted`, perform _abortAlgorithm_ and return _promise_.
    if let Some(signal) = signal {
        if signal.aborted() {
            return Ok(state.data().promise.get(scope));
        }
    }

    // Step 15: `In parallel` but not really; see `#905`, using _reader_ and _writer_, read all
    //          `chunks` from _source_ and write them to _dest_. Due to the locking provided by the
    //          reader and writer, the exact manner in which this happens is not observable to
    //          author code, and so there is flexibility in how this is done. The following
    //          constraints apply regardless of the exact algorithm used: *Public API must not be
    //          used:* while reading or writing, or performing any of the operations below, the
    //          JavaScript-modifiable reader, writer, and stream APIs (i.e. methods on the
    //          appropriate prototypes) must not be used. Instead, the streams must be manipulated
    //          directly. *Backpressure must be enforced:* While
    //          `DefaultWriterGetDesiredSize`(_writer_) is ≤ 0 or is null, the user
    //          agent must not read from _reader_. If _reader_ is a `BYOB reader`,
    //          `DefaultWriterGetDesiredSize`(_writer_) should be used as a basis to
    //          determine the size of the chunks read from _reader_. It’s frequently inefficient
    //          to read chunks that are too small or too large. Other information might be factored
    //          in to determine the optimal chunk size. Reads or writes should not be delayed for
    //          reasons other than these backpressure signals. ``An implementation that waits for
    //          each write to successfully complete before proceeding to the next read/write
    //          operation violates this recommendation. In doing so, such an implementation makes
    //          the `internal queue` of _dest_ useless, as it ensures _dest_ always contains at most
    //          one queued `chunk`. *Shutdown must stop activity:* if _shuttingDown_ becomes true,
    //          the user agent must not initiate further reads from _reader_, and must only perform
    //          writes of already-read `chunks`, as described below. In particular, the user agent
    //          must check the below conditions before performing any reads or writes, since they
    //          might lead to immediate shutdown. *Error and close states must be propagated:* the
    //          following conditions must be applied in order. *Errors must be propagated forward:*
    //          if _source_.`[[state]]` is or becomes "`errored`", then If _preventAbort_ is false,
    //          `shutdown with an action` of ! `WritableStreamAbort`(_dest_,
    //          _source_.`[[storedError]]`) and with _source_.`[[storedError]]`. Otherwise,
    //          `shutdown` with _source_.`[[storedError]]`. *Errors must be propagated backward:* if
    //          _dest_.`[[state]]` is or becomes "`errored`", then If _preventCancel_ is false,
    //          `shutdown with an action` of ! `ReadableStreamCancel`(_source_,
    //          _dest_.`[[storedError]]`) and with _dest_.`[[storedError]]`. Otherwise, `shutdown`
    //          with _dest_.`[[storedError]]`. *Closing must be propagated forward:* if
    //          _source_.`[[state]]` is or becomes "`closed`", then If _preventClose_ is false,
    //          `shutdown with an action` of !
    //          `DefaultWriterCloseWithErrorPropagation`(_writer_). Otherwise,
    //          `shutdown`. *Closing must be propagated backward:* if !
    //          `WritableStreamCloseQueuedOrInFlight`(_dest_) is true or _dest_.`[[state]]` is
    //          "`closed`", then Assert: no `chunks` have been read or written. Let _destClosed_ be
    //          a new ``TypeError``. If _preventCancel_ is false, `shutdown with an action` of !
    //          `ReadableStreamCancel`(_source_, _destClosed_) and with _destClosed_. Otherwise,
    //          `shutdown` with _destClosed_. _Shutdown with an action_: if any of the above
    //          requirements ask to shutdown with an action _action_, optionally with an error
    //          _originalError_, then: If _shuttingDown_ is true, abort these substeps. Set
    //          _shuttingDown_ to true. If _dest_.`[[state]]` is "`writable`" and !
    //          `WritableStreamCloseQueuedOrInFlight`(_dest_) is false, If any `chunks` have been
    //          read but not yet written, write them to _dest_. Wait until every `chunk` that has
    //          been read has been written (i.e. the corresponding promises have settled). Let _p_
    //          be the result of performing _action_. `Upon fulfillment` of _p_, `finalize`, passing
    //          along _originalError_ if it was given. `Upon rejection` of _p_ with reason
    //          _newError_, `finalize` with _newError_. _Shutdown_: if any of the above requirements
    //          or steps ask to shutdown, optionally with an error _error_, then: If _shuttingDown_
    //          is true, abort these substeps. Set _shuttingDown_ to true. If _dest_.`[[state]]` is
    //          "`writable`" and ! `WritableStreamCloseQueuedOrInFlight`(_dest_) is false, If any
    //          `chunks` have been read but not yet written, write them to _dest_. Wait until every
    //          `chunk` that has been read has been written (i.e. the corresponding promises have
    //          settled). `Finalize`, passing along _error_ if it was given. _Finalize_: both forms
    //          of shutdown will eventually ask to finalize, optionally with an error _error_, which
    //          means to perform the following steps: Perform !
    //          `DefaultWriterRelease`(_writer_). If _reader_ `implements`
    //          ``BYOBReader``, perform ! `BYOBReaderRelease`(_reader_).
    //          Otherwise, perform ! `DefaultReaderRelease`(_reader_). If _signal_ is
    //          not undefined, `remove` _abortAlgorithm_ from _signal_. If _error_ was given,
    //          `reject` _promise_ with _error_. Otherwise, `resolve` _promise_ with undefined.
    //
    // (Step 15 in code: set up the four error/close propagation reactions, then
    // start the read-write loop. The propagation, shutdown, finalize, and loop
    // logic live in the `pipe_*` helpers below.)
    pipe_setup_propagation_and_start(scope, state)?;

    // Step 16: Return _promise_.
    // Bind to a local so the `data()` guard drops before `state` does.
    let promise = state.data().promise.get(scope);
    Ok(promise)
}

// ---------------------------------------------------------------------------
// `ReadableStreamPipeTo` propagation reactions, shutdown machinery, and the
// read-write loop. See `readable_stream_pipe_to` above; the shared state is a
// `PipeState` (its fields read/written through `state.data()`/`data_mut()`), and
// every reaction receives the state object as its payload.
// ---------------------------------------------------------------------------

/// The action a pipe shutdown performs once pending writes finish.
#[derive(Clone, Copy)]
enum PipeAction {
    /// `Shutdown` without an action.
    None,
    AbortDest,
    CancelSource,
    CloseWriter,
    AbortAlgorithm,
}

/// A pipe shutdown in progress: the spec's _shuttingDown_ is true exactly while
/// the pipe state holds one.
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct PipeShutdown {
    #[no_trace]
    action: PipeAction,
    /// The error passed to `action`. Unused for `PipeAction::None` and
    /// `PipeAction::CloseWriter`.
    action_error: Heap<Value>,
    /// The error the pipe finalizes with, if any.
    original_error: Option<Heap<Value>>,
    /// Waiting for `pending_writes` to reach zero. The settle reaction that
    /// drains the count proceeds with the shutdown.
    #[no_trace]
    waiting_for_writes: bool,
}

/// The pipe's shutdown actions never run on an empty stack: they are reached either from a promise
/// reaction driving the pipe, or from the abort algorithm the signal invokes, and both are already
/// inside a JS invocation.
const PIPE_SCRIPT_STACK_STATE: ScriptStackState = ScriptStackState::NonEmpty;

fn pipe_source<'r>(scope: &'r Scope<'_>, state: PipeState<'_>) -> ReadableStream<'r> {
    state.data().source.get(scope)
}

fn pipe_dest<'r>(scope: &'r Scope<'_>, state: PipeState<'_>) -> WritableStream<'r> {
    state.data().dest.get(scope)
}

fn pipe_reader<'r>(scope: &'r Scope<'_>, state: PipeState<'_>) -> DefaultReader<'r> {
    state.data().reader.get(scope)
}

fn pipe_writer<'r>(scope: &'r Scope<'_>, state: PipeState<'_>) -> WritableStreamDefaultWriter<'r> {
    state.data().writer.get(scope)
}

fn pipe_promise<'r>(scope: &'r Scope<'_>, state: PipeState<'_>) -> Promise<'r> {
    state.data().promise.get(scope)
}

/// Step 15: set up the four error/close propagation observers, then start the
/// read-write loop.
fn pipe_setup_propagation_and_start(
    scope: &Scope<'_>,
    state: PipeState<'_>,
) -> Result<(), ExnThrown> {
    let source: ReadableStream<'_> = state.data().source.get(scope);
    let dest: WritableStream<'_> = state.data().dest.get(scope);
    let reader: DefaultReader<'_> = state.data().reader.get(scope);
    let writer: WritableStreamDefaultWriter<'_> = state.data().writer.get(scope);
    let prevent_cancel = state.data().prevent_cancel;

    let reader_closed = reader.data().closed_promise.get(scope);
    let writer_closed = writer.data().closed_promise.get(scope);

    // Errors must be propagated forward: if source is or becomes "errored", shut
    // down (aborting dest unless preventAbort).
    if source.data().state == ReadableStreamState::Errored {
        let stored = source.data().stored_error.get(scope);
        pipe_fwd_error(scope, state, stored);
    } else {
        support::react(
            scope,
            &reader_closed,
            None,
            Some((pipe_fwd_error_rejected, state)),
        )?;
    }

    // Errors must be propagated backward: if dest is or becomes "errored", shut
    // down (cancelling source unless preventCancel).
    if dest.data().state == WritableStreamState::Errored {
        let stored = dest.data().stored_error.get(scope);
        pipe_bwd_error(scope, state, stored);
    } else {
        support::react(
            scope,
            &writer_closed,
            None,
            Some((pipe_bwd_error_rejected, state)),
        )?;
    }

    // Closing must be propagated forward: if source is or becomes "closed", shut
    // down (closing dest unless preventClose).
    if source.data().state == ReadableStreamState::Closed {
        pipe_fwd_close(scope, state);
    } else {
        support::react(
            scope,
            &reader_closed,
            Some((pipe_fwd_close_fulfilled, state)),
            None,
        )?;
    }

    // Closing must be propagated backward: if dest is already closing or closed,
    // shut down with a TypeError (cancelling source unless preventCancel).
    if writable_stream_close_queued_or_in_flight(&dest)
        || dest.data().state == WritableStreamState::Closed
    {
        let dest_closed = invalid_state_error(
            scope,
            "ReadableStream",
            c"the destination writable stream closed before all data could be piped to it",
        );
        if !prevent_cancel {
            pipe_begin_shutdown_with_action(
                scope,
                state,
                PipeAction::CancelSource,
                dest_closed,
                Some(dest_closed),
            );
        } else {
            pipe_shutdown(scope, state, Some(dest_closed));
        }
    }

    // Start the loop.
    pipe_step(scope, state);
    Ok(())
}

/// One turn of the pipe loop: when the writer signals it is ready (backpressure
/// has cleared), read a chunk and write it. Stops once `shuttingDown` is set.
fn pipe_step(scope: &Scope<'_>, state: PipeState<'_>) {
    // Shutdown must stop activity: do not initiate further reads.
    if state.data().shutdown.is_some() {
        return;
    }

    // Backpressure must be enforced: wait until the writer is ready before
    // reading. The reactions are the per-pipe callbacks created in
    // `PipeState::new`; attaching them directly allocates nothing per loop turn.
    let writer = pipe_writer(scope, state);
    let ready = writer.data().ready_promise.get(scope);
    let fulfilled = state.data().ready_fulfilled_fn.get(scope);
    let rejected = state.data().ready_rejected_fn.get(scope);
    let _ =
        ready.add_reactions_ignoring_unhandled_rejection(scope, Some(*fulfilled), Some(*rejected));
}

/// The writer is ready: issue a read whose chunk steps write the chunk and loop.
fn pipe_ready_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    // Re-check before reading: shutdown may have begun (and released the reader)
    // while we waited for the writer.
    if state.data().shutdown.is_some() {
        return Ok(value::undefined());
    }

    let reader = pipe_reader(scope, state);
    readable_stream_default_reader_read(
        scope,
        reader,
        ReadRequest::Pipe {
            state: Heap::from(state),
        },
    );
    Ok(value::undefined())
}

/// The writer's ready promise rejected (the destination errored). The backward
/// error-propagation reaction drives shutdown; nothing to do here, but consuming
/// the rejection keeps it from surfacing as unhandled.
fn pipe_ready_rejected(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    Ok(value::undefined())
}

/// The pipe read request's chunk steps: park the chunk, count its pending
/// write, and schedule the deferred write that will write it to the destination
/// and continue the loop.
///
/// The write and the next read are deliberately deferred by one microtask rather
/// than run synchronously here. The spec's pipe step 15 is informal about timing
/// (whatwg/streams#1243), but every browser defers: Gecko's `OnReadFulfilled`
/// does `Promise.resolve().then(() => { write(chunk); readNext(); })`. Deferring
/// matters because a chunk can be delivered to these chunk steps synchronously
/// (e.g. when `enqueue()` fulfils a pending read in the same turn); running the
/// sink's `write()` synchronously from `enqueue()` is observable and forbidden
/// (`streams/piping/general-addition.any.js`: "enqueue() must not synchronously
/// call write algorithm").
pub(crate) fn pipe_read_request_chunk_steps(
    scope: &Scope<'_>,
    state: PipeState<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    state.data().pending_chunk.set(*chunk);
    state.data_mut().pending_writes += 1;
    let deferred = state.data().deferred_write_fn.get(scope);
    js::jobs::queue_microtask(scope, &deferred)
}

/// Runs one microtask after a chunk reaches the pipe read request: writes the
/// parked chunk to the destination and continues the loop. Deferring to here is
/// what keeps `enqueue()` from synchronously invoking the sink's write algorithm
/// (see `pipe_read_request_chunk_steps`).
fn pipe_deferred_write(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    let chunk = state.data().pending_chunk.get(scope);
    state.data().pending_chunk.set(value::undefined());
    let writer = pipe_writer(scope, state);

    // A no-wait shutdown (the destination already errored or close-queued) can
    // finalize, releasing the writer, while this microtask is queued, but writing
    // through a released writer violates `WritableStreamDefaultWriterWrite`'s
    // "stream is not undefined" assert, so we drop the chunk instead.
    // A clean close is unaffected: its finalize waits for `pending_writes` to drain,
    // which happens only after this write runs, so the writer is still attached here.
    if writer_stream(scope, &writer).is_none() {
        // Nothing consults the count after finalize, but keep it exact.
        debug_assert!(state.data().pending_writes > 0);
        state.data_mut().pending_writes -= 1;
        return Ok(value::undefined());
    }
    let write_promise = writable_stream_default_writer_write(scope, &writer, chunk);

    // Track settlement on the write's own promise: both settle paths run
    // `pipe_write_settled`, so no derived promise is needed and the rejection
    // case is consumed (backward error propagation — the writer's closed
    // promise — drives shutdown for a failed write).
    let settled = state.data().write_settled_fn.get(scope);
    write_promise.add_reactions_ignoring_unhandled_rejection(
        scope,
        Some(*settled),
        Some(*settled),
    )?;
    pipe_step(scope, state);
    Ok(value::undefined())
}

/// A write settled (fulfilled or rejected): one fewer chunk is outstanding. If
/// a shutdown is waiting for the writes to drain and this was the last one,
/// proceed with the recorded action.
fn pipe_write_settled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    debug_assert!(state.data().pending_writes > 0);
    state.data_mut().pending_writes -= 1;
    if state.data().pending_writes == 0 && pipe_stop_waiting_for_writes(state) {
        pipe_shutdown_do_proceed(scope, state);
    }
    Ok(value::undefined())
}

// --- Error/close propagation actions ---------------------------------------

fn pipe_fwd_error(scope: &Scope<'_>, state: PipeState<'_>, stored_error: HandleValue<'_>) {
    if state.data().prevent_abort {
        pipe_shutdown(scope, state, Some(stored_error));
    } else {
        pipe_begin_shutdown_with_action(
            scope,
            state,
            PipeAction::AbortDest,
            stored_error,
            Some(stored_error),
        );
    }
}

fn pipe_fwd_error_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    pipe_fwd_error(scope, state, args.get(0));
    Ok(value::undefined())
}

fn pipe_bwd_error(scope: &Scope<'_>, state: PipeState<'_>, stored_error: HandleValue<'_>) {
    if state.data().prevent_cancel {
        pipe_shutdown(scope, state, Some(stored_error));
    } else {
        pipe_begin_shutdown_with_action(
            scope,
            state,
            PipeAction::CancelSource,
            stored_error,
            Some(stored_error),
        );
    }
}

fn pipe_bwd_error_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    pipe_bwd_error(scope, state, args.get(0));
    Ok(value::undefined())
}

fn pipe_fwd_close(scope: &Scope<'_>, state: PipeState<'_>) {
    if state.data().prevent_close {
        pipe_shutdown(scope, state, None);
    } else {
        let undef = HandleValue::undefined();
        pipe_begin_shutdown_with_action(scope, state, PipeAction::CloseWriter, undef, None);
    }
}

fn pipe_fwd_close_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    pipe_fwd_close(scope, state);
    Ok(value::undefined())
}

// --- Shutdown / finalize ----------------------------------------------------

/// Begin a shutdown that, after pending writes finish, performs `action` with
/// `action_error` and finalizes with `original_error` (if any).
fn pipe_begin_shutdown_with_action(
    scope: &Scope<'_>,
    state: PipeState<'_>,
    action: PipeAction,
    action_error: HandleValue<'_>,
    original_error: Option<HandleValue<'_>>,
) {
    if state.data().shutdown.is_some() {
        return;
    }
    state.data_mut().shutdown = Some(PipeShutdown {
        action,
        action_error: Heap::from(action_error.get()),
        original_error: original_error.map(|e| Heap::from(e.get())),
        waiting_for_writes: false,
    });
    pipe_shutdown_wait_then_proceed(scope, state);
}

/// Begin a shutdown that, after pending writes finish, finalizes directly (with
/// `error` if one was given).
fn pipe_shutdown(scope: &Scope<'_>, state: PipeState<'_>, error: Option<HandleValue<'_>>) {
    let undefined = HandleValue::undefined();
    pipe_begin_shutdown_with_action(scope, state, PipeAction::None, undefined, error);
}

/// The shutdown's action and the error passed to it. Panics if the pipe is not
/// shutting down.
fn pipe_action<'r>(scope: &'r Scope<'_>, state: PipeState<'_>) -> (PipeAction, HandleValue<'r>) {
    let data = state.data();
    let shutdown = data.shutdown.as_ref().expect("the pipe is shutting down");
    (shutdown.action, shutdown.action_error.get(scope))
}

/// Set the shutdown's `waiting_for_writes` flag. Panics if the pipe is not
/// shutting down.
fn pipe_wait_for_writes(state: PipeState<'_>) {
    let mut data = state.data_mut();
    let shutdown = data.shutdown.as_mut().expect("the pipe is shutting down");
    shutdown.waiting_for_writes = true;
}

/// Clear the shutdown's `waiting_for_writes` flag, returning whether it was set.
fn pipe_stop_waiting_for_writes(state: PipeState<'_>) -> bool {
    let mut data = state.data_mut();
    match data.shutdown.as_mut() {
        Some(shutdown) => std::mem::replace(&mut shutdown.waiting_for_writes, false),
        None => false,
    }
}

/// The error the shutdown finalizes with, if any.
fn pipe_original_error<'r>(scope: &'r Scope<'_>, state: PipeState<'_>) -> Option<HandleValue<'r>> {
    let data = state.data();
    let shutdown = data.shutdown.as_ref().expect("the pipe is shutting down");
    shutdown.original_error.as_ref().map(|e| e.get(scope))
}

/// If the destination can still accept writes, wait for the in-flight writes to
/// finish before proceeding; otherwise proceed immediately.
///
/// "Wait until every chunk that has been read has been written (i.e. the
/// corresponding promises have settled)" is implemented on the `pending_writes`
/// count rather than the spec's promise chaining. The count is pipe-internal,
/// so the difference is unobservable. With writes outstanding, the last write's
/// settle reaction (`pipe_write_settled`) proceeds; with none, proceed after
/// one microtask, matching the reference algorithm's
/// `uponFulfillment(waitForWritesToFinish(), …)` deferral.
fn pipe_shutdown_wait_then_proceed(scope: &Scope<'_>, state: PipeState<'_>) {
    let dest: WritableStream<'_> = state.data().dest.get(scope);
    if dest.data().state == WritableStreamState::Writable
        && !writable_stream_close_queued_or_in_flight(&dest)
    {
        if state.data().pending_writes > 0 {
            pipe_wait_for_writes(state);
        } else {
            let result =
                Function::new_callback(scope, c"", 0, pipe_shutdown_proceed_deferred, state)
                    .and_then(|cb| js::jobs::queue_microtask(scope, &cb));
            support::report_failure(scope, result, "pipeTo");
        }
    } else {
        pipe_shutdown_do_proceed(scope, state);
    }
}

/// One microtask after a shutdown began with no writes outstanding: proceed,
/// unless a chunk from a read that was already in flight arrived in the
/// meantime, in which case hand off to its write's settle reaction.
fn pipe_shutdown_proceed_deferred(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    if state.data().pending_writes > 0 {
        pipe_wait_for_writes(state);
    } else {
        pipe_shutdown_do_proceed(scope, state);
    }
    Ok(value::undefined())
}

/// Perform the recorded shutdown action (if any) and then finalize.
fn pipe_shutdown_do_proceed(scope: &Scope<'_>, state: PipeState<'_>) {
    if matches!(pipe_action(scope, state).0, PipeAction::None) {
        let error = pipe_original_error(scope, state);
        pipe_finalize(scope, state, error);
        return;
    }
    let action_promise = pipe_run_action(scope, state);
    let result = support::react(
        scope,
        &action_promise,
        Some((pipe_action_fulfilled, state)),
        Some((pipe_action_rejected, state)),
    );
    support::report_failure(scope, result, "pipeTo");
}

fn pipe_run_action<'r>(scope: &'r Scope<'_>, state: PipeState<'_>) -> Promise<'r> {
    // The actions run author code (abort listeners, sink `abort()`), which can re-enter the
    // pipe's read request and borrow `state` mutably, so no borrow is held across them.
    let (action, error) = pipe_action(scope, state);
    match action {
        PipeAction::AbortDest => {
            let dest = pipe_dest(scope, state);
            writable_stream_abort(scope, &dest, error, PIPE_SCRIPT_STACK_STATE)
        }
        PipeAction::CancelSource => {
            let source = pipe_source(scope, state);
            readable_stream_cancel(scope, &source, error)
        }
        PipeAction::CloseWriter => {
            let writer = pipe_writer(scope, state);
            writable_stream_default_writer_close_with_error_propagation(scope, &writer)
        }
        PipeAction::AbortAlgorithm => pipe_build_abort_actions_promise(scope, state, error),
        PipeAction::None => unreachable!(),
    }
}

/// The abort algorithm's action: get a promise to wait for all of (conditionally)
/// aborting the destination and cancelling the source, using the abort reason.
fn pipe_build_abort_actions_promise<'r>(
    scope: &'r Scope<'_>,
    state: PipeState<'_>,
    error: HandleValue<'r>,
) -> Promise<'r> {
    let mut promises: Vec<Promise<'r>> = Vec::new();
    if !state.data().prevent_abort {
        let dest = pipe_dest(scope, state);
        let p = if dest.data().state == WritableStreamState::Writable {
            writable_stream_abort(scope, &dest, error, PIPE_SCRIPT_STACK_STATE)
        } else {
            Promise::shared_resolved_undefined(scope).expect("shared resolved promise")
        };
        promises.push(p);
    }
    if !state.data().prevent_cancel {
        let source = pipe_source(scope, state);
        let p = if source.data().state == ReadableStreamState::Readable {
            readable_stream_cancel(scope, &source, error)
        } else {
            Promise::shared_resolved_undefined(scope).expect("shared resolved promise")
        };
        promises.push(p);
    }
    let handles: Vec<HandleObject> = promises.iter().map(|p| p.handle()).collect();
    Promise::wait_for_all_from(scope, &handles).expect("wait for all")
}

fn pipe_action_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    let error = pipe_original_error(scope, state);
    pipe_finalize(scope, state, error);
    Ok(value::undefined())
}

fn pipe_action_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    pipe_finalize(scope, state, Some(args.get(0)));
    Ok(value::undefined())
}

/// Release the reader and writer, detach the abort listener, and settle the
/// pipe's result promise.
fn pipe_finalize(scope: &Scope<'_>, state: PipeState<'_>, error: Option<HandleValue<'_>>) {
    let writer = pipe_writer(scope, state);
    writable_stream_default_writer_release(scope, &writer);
    let reader = pipe_reader(scope, state);
    let result = readable_stream_default_reader_release(scope, &reader);
    support::report_failure(scope, result, "pipeTo");
    let signal = state.data().signal.get(scope);
    if let Some(signal) = signal {
        // Detach the abort algorithm registered in `PipeState::new`, so a later
        // abort of a still-live signal does not run it after the pipe has
        // finished. (Harmless if it did since shutdown is idempotent, but this avoids
        // keeping the finished pipe reachable from the signal.)
        let abort_fn = state.data().abort_algorithm.get(scope);
        if let Some(abort_fn) = abort_fn {
            web_globals::signals::algorithms::remove_abort_algorithm(&signal, &abort_fn);
        }
    }
    let promise = pipe_promise(scope, state);
    let result = match error {
        Some(e) => promise.reject(scope, e),
        None => promise.resolve(scope, HandleValue::undefined()),
    };
    support::report_failure(scope, result, "pipeTo");
}

/// The abort algorithm registered on the pipe's signal: shut down with an action
/// that waits for all of the (conditional) abort-dest and cancel-source actions.
fn pipe_run_abort_algorithm(scope: &Scope<'_>, state: PipeState<'_>) {
    let signal: AbortSignal<'_> = state.data().signal.get(scope).unwrap();
    let error = signal.reason(scope);
    pipe_begin_shutdown_with_action(scope, state, PipeAction::AbortAlgorithm, error, Some(error));
}

fn pipe_abort_algorithm(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    pipe_run_abort_algorithm(scope, state);
    Ok(value::undefined())
}

#[jsclass(hidden)]
pub(crate) struct TeeState {
    stream: Heap<ReadableStreamImpl>,
    reader: Heap<DefaultReaderImpl>,
    cancel_promise: Heap<js::promise::Promise>,
    clone_for_branch2: bool,
    reading: bool,
    read_again: bool,
    canceled1: bool,
    canceled2: bool,
    reason1: Heap<Value>,
    reason2: Heap<Value>,
    branch1_controller: Heap<ReadableStreamDefaultControllerImpl>,
    branch2_controller: Heap<ReadableStreamDefaultControllerImpl>,
    /// The chunk delivered by the current read, parked until the chunk-steps
    /// microtask consumes it.
    pending_chunk: Heap<Value>,
    /// The chunk-steps microtask callback, allocated once in the constructor
    /// and queued directly on the job queue per chunk.
    chunk_microtask_fn: Heap<js::function::Function>,
}

/// Steps 4-18 of `ReadableStreamDefaultTee` are algorithm.
#[jsmethods]
impl TeeState<'_> {
    fn new(
        &self,
        scope: &Scope<'_>,
        stream: ReadableStream<'_>,
        reader: DefaultReader<'_>,
        clone_for_branch2: bool,
    ) -> Result<(), ExnThrown> {
        self.data_mut().clone_for_branch2 = clone_for_branch2;

        // Step 4: Let _reading_ be false.
        // Step 5: Let _readAgain_ be false.
        // Step 6: Let _canceled1_ be false.
        // Step 7: Let _canceled2_ be false.
        // Step 8: Let _reason1_ be undefined.
        // Step 9: Let _reason2_ be undefined.
        // Step 10: Let _branch1_ be undefined.
        // Step 11: Let _branch2_ be undefined.
        // (all implicit)

        // Step 12: Let _cancelPromise_ be `a new promise`.
        self.data_mut()
            .cancel_promise
            .set(Promise::new_pending(scope)?);
        self.data_mut().stream.set(stream);
        self.data_mut().reader.set(reader);

        // The chunk-steps microtask callback, reused for every chunk.
        let microtask = Function::new_callback(scope, c"", 0, tee_chunk_microtask, self)?;
        self.data_mut().chunk_microtask_fn.set(microtask);

        // Step 13: Let _pullAlgorithm_ be the following steps: (steps implemented in `tee_pull_native` and its callees)
        let pull = AlgorithmArg::native(scope, tee_pull_native, *self)?;

        // Step 14: Let _cancel1Algorithm_ be the following steps: (steps implemented in `tee_cancel1_native` and its callees)
        let cancel1 = AlgorithmArg::native(scope, tee_cancel1_native, *self)?;

        // Step 15: Let _cancel2Algorithm_ be the following steps: (steps implemented in `tee_cancel2_native` and its callees)
        let cancel2 = AlgorithmArg::native(scope, tee_cancel2_native, *self)?;

        // Step 16: Let _startAlgorithm_ be an algorithm that returns undefined.
        let start = AlgorithmArg::None;
        let hwm = HandleValue::undefined();

        // Step 17: Set _branch1_ to ! `CreateReadableStream`(_startAlgorithm_, _pullAlgorithm_,
        //          _cancel1Algorithm_).
        let branch1 = create_readable_stream(scope, start, pull, cancel1, 1.0, hwm)?;
        let controller = branch1
            .default_controller(scope)
            .expect("a default controller");
        self.data_mut().branch1_controller.set(controller);

        // Step 18: Set _branch2_ to ! `CreateReadableStream`(_startAlgorithm_, _pullAlgorithm_,
        //          _cancel2Algorithm_).
        let branch2 = create_readable_stream(scope, start, pull, cancel2, 1.0, hwm)?;
        let controller = branch2
            .default_controller(scope)
            .expect("a default controller");
        self.data_mut().branch2_controller.set(controller);

        Ok(())
    }
}

#[jsclass(hidden)]
pub(crate) struct PipeState {
    source: Heap<ReadableStreamImpl>,
    dest: Heap<WritableStreamImpl>,
    reader: Heap<DefaultReaderImpl>,
    writer: Heap<WritableStreamDefaultWriterImpl>,
    prevent_close: bool,
    prevent_abort: bool,
    prevent_cancel: bool,
    shutdown: Option<PipeShutdown>,
    signal: Option<Heap<AbortSignalImpl>>,
    /// The chunk read by the current loop turn, parked until the deferred write
    /// consumes it. Single-occupancy: the next read is only issued after
    /// `pipe_deferred_write` has taken the value out (see
    /// `pipe_read_request_chunk_steps`).
    pending_chunk: Heap<Value>,
    /// The number of chunks that have been read but whose writes have not yet
    /// settled. Counted eagerly at the read request's chunk steps; decremented
    /// by each write's settle reaction (`pipe_write_settled`). Shutdown's "wait
    /// until every chunk that has been read has been written" is implemented as
    /// waiting for this count to drain. The count is pipe-internal, so the
    /// difference from the spec's promise chaining is unobservable.
    pending_writes: u32,
    /// The loop's reaction callbacks, allocated once in the constructor, so the
    /// steady-state loop allocates no callback objects per chunk.
    deferred_write_fn: Heap<js::function::Function>,
    write_settled_fn: Heap<js::function::Function>,
    ready_fulfilled_fn: Heap<js::function::Function>,
    ready_rejected_fn: Heap<js::function::Function>,
    promise: Heap<js::promise::Promise>,
    abort_algorithm: Option<Heap<js::function::Function>>,
}

#[jsmethods]
impl PipeState<'_> {
    fn new(
        &self,
        scope: &Scope<'_>,
        source: ReadableStream<'_>,
        dest: WritableStream<'_>,
        reader: DefaultReader<'_>,
        writer: WritableStreamDefaultWriter<'_>,
        prevent_close: bool,
        prevent_abort: bool,
        prevent_cancel: bool,
        signal: Option<AbortSignal<'_>>,
    ) -> Result<(), ExnThrown> {
        self.data_mut().source.set(source);
        self.data_mut().dest.set(dest);
        self.data_mut().reader.set(reader);
        self.data_mut().writer.set(writer);
        self.data_mut().prevent_close = prevent_close;
        self.data_mut().prevent_abort = prevent_abort;
        self.data_mut().prevent_cancel = prevent_cancel;
        self.data_mut().signal = signal.map(Heap::from);

        let deferred = Function::new_callback(scope, c"", 0, pipe_deferred_write, self)?;
        self.data_mut().deferred_write_fn.set(deferred);
        let settled = Function::new_callback(scope, c"", 1, pipe_write_settled, self)?;
        self.data_mut().write_settled_fn.set(settled);
        let ready_ok = Function::new_callback(scope, c"", 1, pipe_ready_fulfilled, self)?;
        self.data_mut().ready_fulfilled_fn.set(ready_ok);
        // The ready-rejection swallower uses no per-pipe state; share one per global.
        let ready_err = js::class::get_or_init_shared_function(
            scope,
            pipe_ready_rejected as *const () as usize,
            |scope| Function::new_callback(scope, c"", 1, pipe_ready_rejected, ()),
        )?;
        self.data_mut().ready_rejected_fn.set(ready_err);
        self.data_mut().promise.set(Promise::new_pending(scope)?);

        // Step 14: If _signal_ is not undefined, Let _abortAlgorithm_ be the following steps: Let
        //          _error_ be _signal_’s `abort reason`. Let _actions_ be an empty `ordered set`. If
        //          _preventAbort_ is false, `append` the following action to _actions_: If
        //          _dest_.`[[state]]` is "`writable`", return ! `WritableStreamAbort`(_dest_, _error_).
        //          Otherwise, return `a promise resolved with` undefined. If _preventCancel_ is false,
        //          `append` the following action action to _actions_: If _source_.`[[state]]` is
        //          "`readable`", return ! `ReadableStreamCancel`(_source_, _error_). Otherwise, return
        //          `a promise resolved with` undefined. `Shutdown with an action` consisting of
        //          `getting a promise to wait for all` of the actions in _actions_, and with _error_.
        //          If _signal_ is `aborted`, perform _abortAlgorithm_ and return _promise_. `Add`
        //          _abortAlgorithm_ to _signal_.
        if let Some(signal) = signal {
            let abort_fn = Function::new_callback(scope, c"", 0, pipe_abort_algorithm, self)?;
            self.data_mut().abort_algorithm.set(abort_fn);
            if signal.aborted() {
                pipe_run_abort_algorithm(scope, *self);
                return Ok(());
            }
            // `Add` _abortAlgorithm_ to _signal_.
            web_globals::signals::algorithms::add_abort_algorithm(&signal, &abort_fn);
        }
        Ok(())
    }
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-readablestreamdefaulttee>
/// ReadableStreamDefaultTee(stream, cloneForBranch2) performs the following steps:
pub(crate) fn readable_stream_default_tee<'r>(
    scope: &'r Scope<'_>,
    stream: &ReadableStream<'_>,
    clone_for_branch2: bool,
) -> Result<(ReadableStream<'r>, ReadableStream<'r>), ExnThrown> {
    // Step 1: Assert: _stream_ `implements` ``ReadableStream``.
    // Step 2: Assert: _cloneForBranch2_ is a boolean.
    // Step 3: Let _reader_ be ? `AcquireDefaultReader`(_stream_).
    let reader = acquire_readable_stream_default_reader(scope, stream)?;

    // Steps 4-18 implemented in `TeeState::new` and the native functions for the algorithms it sets up.
    let state = TeeState::new(scope, *stream, reader, clone_for_branch2)?;

    // Step 19: `Upon rejection` of _reader_.`[[closedPromise]]` with reason _r_, Perform !
    //          `DefaultControllerError`(_branch1_.`[[controller]]`, _r_). Perform !
    //          `DefaultControllerError`(_branch2_.`[[controller]]`, _r_). If
    //          _canceled1_ is false or _canceled2_ is false, `resolve` _cancelPromise_ with
    //          undefined.
    let on_rejected = Function::new_callback(scope, c"", 1, tee_closed_rejected, state)?;
    reader
        .closed(scope)
        .add_reactions(scope, None, Some(*on_rejected))?;

    // Step 20: Return « _branch1_, _branch2_ ».
    let branch1 = state.data().branch1_controller.get(scope).stream(scope);
    let branch2 = state.data().branch2_controller.get(scope).stream(scope);
    Ok((branch1, branch2))
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-readablebytestreamtee>
/// ReadableByteStreamTee(stream) performs the following steps:
///
/// The closures of steps 14-20 are the `byte_tee_*` helper functions below; the
/// shared mutable state lives in a `ByteTeeState` passed as each callback's
/// payload, mirroring `ReadableStreamDefaultTee`.
pub(crate) fn readable_byte_stream_tee<'r>(
    scope: &'r Scope<'_>,
    stream: &ReadableStream<'_>,
) -> Result<(ReadableStream<'r>, ReadableStream<'r>), ExnThrown> {
    // Step 1: Assert: _stream_ `implements` ``ReadableStream``.
    // Step 2: Assert: _stream_.`[[controller]]` `implements` ``ReadableByteStreamController``.
    debug_assert!(stream.byte_controller(scope).is_some());
    // Step 3: Let _reader_ be ? `AcquireDefaultReader`(_stream_).
    // Step 4: Let _reading_ be false.
    // Step 5: Let _readAgainForBranch1_ be false.
    // Step 6: Let _readAgainForBranch2_ be false.
    // Step 7: Let _canceled1_ be false.
    // Step 8: Let _canceled2_ be false.
    // Step 9: Let _reason1_ be undefined.
    // Step 10: Let _reason2_ be undefined.
    // Step 11: Let _branch1_ be undefined.
    // Step 12: Let _branch2_ be undefined.
    // Step 13: Let _cancelPromise_ be `a new promise`.
    // Step 14: Let _forwardReaderError_ be the following steps, taking a _thisReader_ argument:
    //          `Upon rejection` of _thisReader_.`[[closedPromise]]` with reason _r_, If
    //          _thisReader_ is not _reader_, return. Perform !
    //          `ByteStreamControllerError`(_branch1_.`[[controller]]`, _r_). Perform !
    //          `ByteStreamControllerError`(_branch2_.`[[controller]]`, _r_). If _canceled1_
    //          is false or _canceled2_ is false, `resolve` _cancelPromise_ with undefined.
    // Step 15: Let _pullWithDefaultReader_ be the following steps: If _reader_ `implements`
    //          ``BYOBReader``, Assert: _reader_.`[[readIntoRequests]]` is `empty`.
    //          Perform ! `BYOBReaderRelease`(_reader_). Set _reader_ to !
    //          `AcquireDefaultReader`(_stream_). Perform _forwardReaderError_, given
    //          _reader_. Let _readRequest_ be a `read request` with the following `items`: `chunk
    //          steps`, given _chunk_ `Queue a microtask` to perform the following steps: Set
    //          _readAgainForBranch1_ to false. Set _readAgainForBranch2_ to false. Let _chunk1_ and
    //          _chunk2_ be _chunk_. If _canceled1_ is false and _canceled2_ is false, Let
    //          _cloneResult_ be `CloneAsUint8Array`(_chunk_). If _cloneResult_ is an abrupt
    //          completion, Perform !
    //          `ByteStreamControllerError`(_branch1_.`[[controller]]`,
    //          _cloneResult_.[[Value]]). Perform !
    //          `ByteStreamControllerError`(_branch2_.`[[controller]]`,
    //          _cloneResult_.[[Value]]). `Resolve` _cancelPromise_ with !
    //          `ReadableStreamCancel`(_stream_, _cloneResult_.[[Value]]). Return. Otherwise, set
    //          _chunk2_ to _cloneResult_.[[Value]]. If _canceled1_ is false, perform !
    //          `ByteStreamControllerEnqueue`(_branch1_.`[[controller]]`, _chunk1_). If
    //          _canceled2_ is false, perform !
    //          `ByteStreamControllerEnqueue`(_branch2_.`[[controller]]`, _chunk2_). Set
    //          _reading_ to false. If _readAgainForBranch1_ is true, perform _pull1Algorithm_.
    //          Otherwise, if _readAgainForBranch2_ is true, perform _pull2Algorithm_. The microtask
    //          delay here is necessary because it takes at least a microtask to detect errors, when
    //          we use _reader_.`[[closedPromise]]` below. We want errors in _stream_ to error both
    //          branches immediately, so we cannot let successful synchronously-available reads
    //          happen ahead of asynchronously-available errors. `close steps` Set _reading_ to
    //          false. If _canceled1_ is false, perform !
    //          `ByteStreamControllerClose`(_branch1_.`[[controller]]`). If _canceled2_ is
    //          false, perform ! `ByteStreamControllerClose`(_branch2_.`[[controller]]`). If
    //          _branch1_.`[[controller]]`.`[[pendingPullIntos]]` is not `empty`, perform !
    //          `ByteStreamControllerRespond`(_branch1_.`[[controller]]`, 0). If
    //          _branch2_.`[[controller]]`.`[[pendingPullIntos]]` is not `empty`, perform !
    //          `ByteStreamControllerRespond`(_branch2_.`[[controller]]`, 0). If _canceled1_
    //          is false or _canceled2_ is false, `resolve` _cancelPromise_ with undefined. `error
    //          steps` Set _reading_ to false. Perform ! `DefaultReaderRead`(_reader_,
    //          _readRequest_).
    // Step 16: Let _pullWithBYOBReader_ be the following steps, given _view_ and _forBranch2_: If
    //          _reader_ `implements` ``DefaultReader``, Assert:
    //          _reader_.`[[readRequests]]` is `empty`. Perform !
    //          `DefaultReaderRelease`(_reader_). Set _reader_ to !
    //          `AcquireBYOBReader`(_stream_). Perform _forwardReaderError_, given
    //          _reader_. Let _byobBranch_ be _branch2_ if _forBranch2_ is true, and _branch1_
    //          otherwise. Let _otherBranch_ be _branch2_ if _forBranch2_ is false, and _branch1_
    //          otherwise. Let _readIntoRequest_ be a `read-into request` with the following
    //          `items`: `chunk steps`, given _chunk_ `Queue a microtask` to perform the following
    //          steps: Set _readAgainForBranch1_ to false. Set _readAgainForBranch2_ to false. Let
    //          _byobCanceled_ be _canceled2_ if _forBranch2_ is true, and _canceled1_ otherwise.
    //          Let _otherCanceled_ be _canceled2_ if _forBranch2_ is false, and _canceled1_
    //          otherwise. If _otherCanceled_ is false, Let _cloneResult_ be
    //          `CloneAsUint8Array`(_chunk_). If _cloneResult_ is an abrupt completion, Perform !
    //          `ByteStreamControllerError`(_byobBranch_.`[[controller]]`,
    //          _cloneResult_.[[Value]]). Perform !
    //          `ByteStreamControllerError`(_otherBranch_.`[[controller]]`,
    //          _cloneResult_.[[Value]]). `Resolve` _cancelPromise_ with !
    //          `ReadableStreamCancel`(_stream_, _cloneResult_.[[Value]]). Return. Otherwise, let
    //          _clonedChunk_ be _cloneResult_.[[Value]]. If _byobCanceled_ is false, perform !
    //          `ByteStreamControllerRespondWithNewView`(_byobBranch_.`[[controller]]`,
    //          _chunk_). Perform !
    //          `ByteStreamControllerEnqueue`(_otherBranch_.`[[controller]]`,
    //          _clonedChunk_). Otherwise, if _byobCanceled_ is false, perform !
    //          `ByteStreamControllerRespondWithNewView`(_byobBranch_.`[[controller]]`,
    //          _chunk_). Set _reading_ to false. If _readAgainForBranch1_ is true, perform
    //          _pull1Algorithm_. Otherwise, if _readAgainForBranch2_ is true, perform
    //          _pull2Algorithm_. The microtask delay here is necessary because it takes at least a
    //          microtask to detect errors, when we use _reader_.`[[closedPromise]]` below. We want
    //          errors in _stream_ to error both branches immediately, so we cannot let successful
    //          synchronously-available reads happen ahead of asynchronously-available errors.
    //          `close steps`, given _chunk_ Set _reading_ to false. Let _byobCanceled_ be
    //          _canceled2_ if _forBranch2_ is true, and _canceled1_ otherwise. Let _otherCanceled_
    //          be _canceled2_ if _forBranch2_ is false, and _canceled1_ otherwise. If
    //          _byobCanceled_ is false, perform !
    //          `ByteStreamControllerClose`(_byobBranch_.`[[controller]]`). If
    //          _otherCanceled_ is false, perform !
    //          `ByteStreamControllerClose`(_otherBranch_.`[[controller]]`). If _chunk_ is
    //          not undefined, Assert: _chunk_.[[ByteLength]] is 0. If _byobCanceled_ is false,
    //          perform !
    //          `ByteStreamControllerRespondWithNewView`(_byobBranch_.`[[controller]]`,
    //          _chunk_). If _otherCanceled_ is false and
    //          _otherBranch_.`[[controller]]`.`[[pendingPullIntos]]` is not `empty`, perform !
    //          `ByteStreamControllerRespond`(_otherBranch_.`[[controller]]`, 0). If
    //          _byobCanceled_ is false or _otherCanceled_ is false, `resolve` _cancelPromise_ with
    //          undefined. `error steps` Set _reading_ to false. Perform !
    //          `BYOBReaderRead`(_reader_, _view_, 1, _readIntoRequest_).
    // Step 17: Let _pull1Algorithm_ be the following steps: If _reading_ is true, Set
    //          _readAgainForBranch1_ to true. Return `a promise resolved with` undefined. Set
    //          _reading_ to true. Let _byobRequest_ be !
    //          `ByteStreamControllerGetBYOBRequest`(_branch1_.`[[controller]]`). If
    //          _byobRequest_ is null, perform _pullWithDefaultReader_. Otherwise, perform
    //          _pullWithBYOBReader_, given _byobRequest_.`[[view]]` and false. Return `a promise
    //          resolved with` undefined.
    // Step 18: Let _pull2Algorithm_ be the following steps: If _reading_ is true, Set
    //          _readAgainForBranch2_ to true. Return `a promise resolved with` undefined. Set
    //          _reading_ to true. Let _byobRequest_ be !
    //          `ByteStreamControllerGetBYOBRequest`(_branch2_.`[[controller]]`). If
    //          _byobRequest_ is null, perform _pullWithDefaultReader_. Otherwise, perform
    //          _pullWithBYOBReader_, given _byobRequest_.`[[view]]` and true. Return `a promise
    //          resolved with` undefined.
    // Step 19: Let _cancel1Algorithm_ be the following steps, taking a _reason_ argument: Set
    //          _canceled1_ to true. Set _reason1_ to _reason_. If _canceled2_ is true, Let
    //          _compositeReason_ be ! `CreateArrayFromList`(« _reason1_, _reason2_ »). Let
    //          _cancelResult_ be ! `ReadableStreamCancel`(_stream_, _compositeReason_). `Resolve`
    //          _cancelPromise_ with _cancelResult_. Return _cancelPromise_.
    // Step 20: Let _cancel2Algorithm_ be the following steps, taking a _reason_ argument: Set
    //          _canceled2_ to true. Set _reason2_ to _reason_. If _canceled1_ is true, Let
    //          _compositeReason_ be ! `CreateArrayFromList`(« _reason1_, _reason2_ »). Let
    //          _cancelResult_ be ! `ReadableStreamCancel`(_stream_, _compositeReason_). `Resolve`
    //          _cancelPromise_ with _cancelResult_. Return _cancelPromise_.
    // Step 21: Let _startAlgorithm_ be an algorithm that returns undefined.
    // Step 22: Set _branch1_ to ! `CreateReadableByteStream`(_startAlgorithm_, _pull1Algorithm_,
    //          _cancel1Algorithm_).
    // Step 23: Set _branch2_ to ! `CreateReadableByteStream`(_startAlgorithm_, _pull2Algorithm_,
    //          _cancel2Algorithm_).
    // Step 24: Perform _forwardReaderError_, given _reader_.
    // Step 25: Return « _branch1_, _branch2_ ».
    //
    // (Steps 3-24 implemented in `ByteTeeState::new` and the `byte_tee_*` helpers.)
    let state = ByteTeeState::new(scope, *stream)?;
    // Step 25: Return « _branch1_, _branch2_ ».
    let branch1 = state.data().branch1_controller.get(scope).stream(scope);
    let branch2 = state.data().branch2_controller.get(scope).stream(scope);
    Ok((branch1, branch2))
}

/// The shared state backing `ReadableByteStreamTee`. Not exposed to JS
/// (`hidden`); created internally and reached through each callback's payload.
/// The `reader` holds whichever reader type (default or BYOB) is currently in
/// use; it is swapped by `pullWithDefaultReader` / `pullWithBYOBReader`.
#[jsclass(hidden)]
pub(crate) struct ByteTeeState {
    stream: Heap<ReadableStreamImpl>,
    reader: HeapReadableStreamReader,
    reading: bool,
    read_again_for_branch1: bool,
    read_again_for_branch2: bool,
    canceled1: bool,
    canceled2: bool,
    reason1: Heap<Value>,
    reason2: Heap<Value>,
    branch1_controller: Heap<ReadableByteStreamControllerImpl>,
    branch2_controller: Heap<ReadableByteStreamControllerImpl>,
    cancel_promise: Heap<js::promise::Promise>,
    /// The chunk (and, for a BYOB read, its target branch) delivered by the
    /// current read, parked until the chunk-steps microtask consumes it.
    pending_chunk: Heap<Value>,
    pending_for_branch2: bool,
    /// The two chunk-steps microtask callbacks, allocated once in the constructor
    /// and queued directly on the job queue per chunk, so delivery allocates nothing.
    default_microtask_fn: Heap<js::function::Function>,
    byob_microtask_fn: Heap<js::function::Function>,
}

#[jsmethods]
impl ByteTeeState<'_> {
    fn new(&self, scope: &Scope<'_>, stream: ReadableStream<'_>) -> Result<(), ExnThrown> {
        self.data_mut().stream.set(stream);
        // Step 3: _reader_ = `AcquireDefaultReader`(_stream_).
        let reader =
            ReadableStreamReader::Default(acquire_readable_stream_default_reader(scope, &stream)?);
        self.data_mut().reader = HeapReadableStreamReader::from(reader);
        // Steps 4-12: _reading_, _readAgainForBranchN_, _canceledN_ are false;
        // _reasonN_ and _branchN_ are undefined (the field defaults; branches set below).
        // Step 13: _cancelPromise_ = `a new promise`.
        self.data_mut()
            .cancel_promise
            .set(Promise::new_pending(scope)?);

        // The chunk-steps microtask callbacks, reused for every chunk.
        let default_mt = Function::new_callback(scope, c"", 0, byte_tee_default_microtask, self)?;
        self.data_mut().default_microtask_fn.set(default_mt);
        let byob_mt = Function::new_callback(scope, c"", 0, byte_tee_byob_microtask, self)?;
        self.data_mut().byob_microtask_fn.set(byob_mt);

        let start = AlgorithmArg::None;
        // Step 22: _branch1_ = `CreateReadableByteStream`(start, _pull1Algorithm_, _cancel1Algorithm_).
        let pull1 = AlgorithmArg::native(scope, byte_tee_pull1, *self)?;
        let cancel1 = AlgorithmArg::native(scope, byte_tee_cancel1, *self)?;
        let branch1 = create_readable_byte_stream(scope, start, pull1, cancel1)?;
        let controller = branch1.byte_controller(scope).expect("a byte controller");
        self.data_mut().branch1_controller.set(controller);
        // Step 23: _branch2_ = `CreateReadableByteStream`(start, _pull2Algorithm_, _cancel2Algorithm_).
        let pull2 = AlgorithmArg::native(scope, byte_tee_pull2, *self)?;
        let cancel2 = AlgorithmArg::native(scope, byte_tee_cancel2, *self)?;
        let branch2 = create_readable_byte_stream(scope, start, pull2, cancel2)?;
        let controller = branch2.byte_controller(scope).expect("a byte controller");
        self.data_mut().branch2_controller.set(controller);
        // Step 24: Perform _forwardReaderError_, given _reader_.
        byte_tee_forward_reader_error(scope, *self, reader)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// `ReadableByteStreamTee` reader switching and callbacks. The shared state is a
// `ByteTeeState` (above), passed as every callback's payload. Its `reader` holds
// whichever reader type (default or BYOB) is currently in use; it is swapped by
// `pullWithDefaultReader` / `pullWithBYOBReader`.
// ---------------------------------------------------------------------------

fn byte_tee_stream<'r>(scope: &'r Scope<'_>, state: ByteTeeState<'_>) -> ReadableStream<'r> {
    state.data().stream.get(scope)
}

fn byte_tee_reader<'r>(scope: &'r Scope<'_>, state: ByteTeeState<'_>) -> ReadableStreamReader<'r> {
    state.data().reader.get(scope)
}

/// Branch 1 or branch 2 of the byte tee.
fn byte_tee_branch_controller<'r>(
    scope: &'r Scope<'_>,
    state: ByteTeeState<'_>,
    for_branch2: bool,
) -> ReadableByteStreamController<'r> {
    if for_branch2 {
        state.data().branch2_controller.get(scope)
    } else {
        state.data().branch1_controller.get(scope)
    }
}

fn byte_tee_cancel_promise<'r>(scope: &'r Scope<'_>, state: ByteTeeState<'_>) -> Promise<'r> {
    state.data().cancel_promise.get(scope)
}

/// Set `reading` to false (the byte-tee read/read-into requests' `error steps`).
pub(crate) fn byte_tee_set_not_reading(_scope: &Scope<'_>, state: ByteTeeState<'_>) {
    state.data_mut().reading = false;
}

/// A chunk value as an `ArrayBufferView` (byte-tee chunks are always views).
fn byte_tee_view<'r>(scope: &'r Scope<'_>, v: HandleValue<'_>) -> js::ArrayBufferView<'r> {
    js::ArrayBufferView::from_jsval(scope, v, ()).expect("byte-tee chunk is an ArrayBufferView")
}

/// `CloneAsUint8Array`(_O_): clone the view's region into a fresh `Uint8Array`.
fn byte_tee_clone_as_uint8array<'r>(
    scope: &'r Scope<'_>,
    chunk: HandleValue<'_>,
) -> Result<HandleValue<'r>, ExnThrown> {
    let view = js::ArrayBufferView::from_jsval_throwing(scope, chunk, ())?;
    let length = view.byte_length();
    let byte_offset = view.byte_offset();
    let buffer = view.viewed_buffer(scope)?;
    let cloned = buffer.clone_region(scope, byte_offset, length)?;
    let array = js::Uint8Array::with_buffer(scope, cloned, 0, length)?;
    Ok(scope.root_value(array.as_value()))
}

/// Step 14 `forwardReaderError`: forward an error on `thisReader`'s closed promise
/// to both branches (but only while `thisReader` is still the current reader).
fn byte_tee_forward_reader_error(
    scope: &Scope<'_>,
    state: ByteTeeState<'_>,
    this_reader: ReadableStreamReader<'_>,
) -> Result<(), ExnThrown> {
    let closed = this_reader.generic_closed_promise(scope);
    let payload = pair_payload(scope, state, this_reader.as_object())?;
    support::react(
        scope,
        &closed,
        None,
        Some((byte_tee_forward_rejected, payload)),
    )
}

fn byte_tee_forward_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let (state_v, this_reader_v) = pair_parts(scope, payload);
    let state = cast_payload::<ByteTeeState>(scope, state_v);
    let r = args.get(0);
    // If _thisReader_ is not _reader_ (the current reader), return.
    let this_reader =
        Object::from_value(scope, *this_reader_v).expect("payload reader is an object");
    let current = byte_tee_reader(scope, state).as_object();
    if this_reader.as_raw() != current.as_raw() {
        return Ok(value::undefined());
    }
    let branch1 = byte_tee_branch_controller(scope, state, false);
    readable_byte_stream_controller_error(scope, &branch1, r);
    let branch2 = byte_tee_branch_controller(scope, state, true);
    readable_byte_stream_controller_error(scope, &branch2, r);
    if !state.data().canceled1 || !state.data().canceled2 {
        byte_tee_cancel_promise(scope, state).resolve(scope, HandleValue::undefined())?;
    }
    Ok(value::undefined())
}

/// Step 15 `pullWithDefaultReader`: switch to a default reader if needed, then
/// issue a default read driving both branches.
fn byte_tee_pull_with_default_reader(scope: &Scope<'_>, state: ByteTeeState<'_>) {
    let reader = match byte_tee_reader(scope, state) {
        ReadableStreamReader::Default(reader) => reader,
        ReadableStreamReader::Byob(byob) => {
            debug_assert!(byob.data().read_into_requests.is_empty());
            support::report_failure(
                scope,
                readable_stream_byob_reader_release(scope, &byob),
                "ReadableByteStreamTee",
            );
            let stream = byte_tee_stream(scope, state);
            let new_reader = acquire_readable_stream_default_reader(scope, &stream)
                .expect("acquire default reader");
            let new_generic = new_reader.as_generic();
            state.data_mut().reader = HeapReadableStreamReader::from(new_generic);
            support::report_failure(
                scope,
                byte_tee_forward_reader_error(scope, state, new_generic),
                "ReadableByteStreamTee",
            );
            new_reader
        }
    };
    readable_stream_default_reader_read(
        scope,
        reader,
        ReadRequest::ByteTeeDefault {
            state: Heap::from(state),
        },
    );
}

/// Step 16 `pullWithBYOBReader`: switch to a BYOB reader if needed, then issue a
/// BYOB read into `view` driving both branches.
fn byte_tee_pull_with_byob_reader(
    scope: &Scope<'_>,
    state: ByteTeeState<'_>,
    view: Option<js::ArrayBufferView<'_>>,
    for_branch2: bool,
) {
    let reader = match byte_tee_reader(scope, state) {
        ReadableStreamReader::Byob(reader) => reader,
        ReadableStreamReader::Default(default) => {
            debug_assert!(default.data().read_requests.is_empty());
            support::report_failure(
                scope,
                readable_stream_default_reader_release(scope, &default),
                "ReadableByteStreamTee",
            );
            let stream = byte_tee_stream(scope, state);
            let new_reader =
                acquire_readable_stream_byob_reader(scope, &stream).expect("acquire BYOB reader");
            let new_generic = new_reader.as_generic();
            state.data_mut().reader = HeapReadableStreamReader::from(new_generic);
            support::report_failure(
                scope,
                byte_tee_forward_reader_error(scope, state, new_generic),
                "ReadableByteStreamTee",
            );
            new_reader
        }
    };
    let Some(view) = view else {
        return;
    };
    readable_stream_byob_reader_read(
        scope,
        &reader,
        view,
        1,
        ReadIntoRequest::ByteTeeByob {
            state: Heap::from(state),
            for_branch2,
        },
    );
}

/// Shared pull body for branch 1/2 (`pull1Algorithm`/`pull2Algorithm`, steps 17-18).
fn byte_tee_pull(scope: &Scope<'_>, state: ByteTeeState<'_>, for_branch2: bool) {
    if state.data().reading {
        if for_branch2 {
            state.data_mut().read_again_for_branch2 = true;
        } else {
            state.data_mut().read_again_for_branch1 = true;
        }
        return;
    }
    state.data_mut().reading = true;
    let controller = byte_tee_branch_controller(scope, state, for_branch2);
    match readable_byte_stream_controller_get_byob_request(scope, &controller) {
        None => byte_tee_pull_with_default_reader(scope, state),
        Some(byob_request) => {
            let view = byob_request.data().view.get(scope);
            byte_tee_pull_with_byob_reader(scope, state, view, for_branch2);
        }
    }
}

fn byte_tee_pull1(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ByteTeeState>(scope, payload);
    byte_tee_pull(scope, state, false);
    // The returned resolved promise is internal (the branch controller only
    // attaches its pull reactions to it): the per-global reused instance serves.
    Ok(Promise::shared_resolved_undefined(scope)?.as_value())
}

fn byte_tee_pull2(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ByteTeeState>(scope, payload);
    byte_tee_pull(scope, state, true);
    // As in `byte_tee_pull1`: the per-global reused instance serves.
    Ok(Promise::shared_resolved_undefined(scope)?.as_value())
}

/// Shared cancelN body (steps 19-20): record the reason and, once both branches
/// are canceled, cancel the source with the composite reason.
fn byte_tee_cancel(
    scope: &Scope<'_>,
    state: ByteTeeState<'_>,
    reason: HandleValue<'_>,
    for_branch2: bool,
) -> Value {
    // Set _canceledN_ to true. Set _reasonN_ to _reason_.
    let other_canceled = if for_branch2 {
        state.data_mut().canceled2 = true;
        state.data_mut().reason2.set(*reason);
        state.data().canceled1
    } else {
        state.data_mut().canceled1 = true;
        state.data_mut().reason1.set(*reason);
        state.data().canceled2
    };
    // If the other branch is canceled, cancel the source with the composite
    // reason « _reason1_, _reason2_ » and resolve the cancel promise.
    if other_canceled {
        let r1 = state.data().reason1.get(scope);
        let r2 = state.data().reason2.get(scope);
        let composite = composite_reason(scope, r1, r2).expect("composite reason");
        let stream = byte_tee_stream(scope, state);
        let cancel_result = readable_stream_cancel(scope, &stream, composite);
        byte_tee_cancel_promise(scope, state)
            .resolve(scope, cancel_result)
            .expect("resolve cancel");
    }
    byte_tee_cancel_promise(scope, state).as_value()
}

fn byte_tee_cancel1(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ByteTeeState>(scope, payload);
    Ok(byte_tee_cancel(scope, state, args.get(0), false))
}

fn byte_tee_cancel2(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ByteTeeState>(scope, payload);
    Ok(byte_tee_cancel(scope, state, args.get(0), true))
}

/// The default read request's `chunk steps`: queue a microtask to drive both
/// branches (the delay lets stream errors win over a synchronous read).
pub(crate) fn byte_tee_default_chunk_steps(
    scope: &Scope<'_>,
    state: ByteTeeState<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    state.data().pending_chunk.set(*chunk);
    let microtask = state.data().default_microtask_fn.get(scope);
    js::jobs::queue_microtask(scope, &microtask)
}

/// The chunk enqueued into a byte-tee branch is always a fresh, non-detached,
/// transferable view (the original read result or a `CloneAsUint8Array` clone),
/// so `ReadableByteStreamControllerEnqueue` never throws — matching the spec's
/// `! ReadableByteStreamControllerEnqueue`.
const BYTE_TEE_ENQUEUE_INFALLIBLE: &str =
    "byte tee branch enqueue is infallible: the chunk is a fresh, transferable view";

/// `ReadableByteStreamTee` closes each branch and responds to its pending
/// pull-into using the branch's own created descriptor and an aligned
/// (zero-length) view, so these never throw — matching the spec's
/// `! ReadableByteStreamControllerClose` / `! …Respond` / `! …RespondWithNewView`.
const BYTE_TEE_SETTLE_INFALLIBLE: &str =
    "byte tee branch close/respond is infallible: created descriptor, aligned view (spec `!`)";

/// Error both branches with `error`, cancel the underlying source, and resolve
/// the tee's cancel promise — the byte-tee read-request steps' shared failure
/// path. It backs the chunk steps' chunk-clone failure, and the close steps'
/// fallible branch close/respond (see [`byte_tee_default_close_steps`]).
fn byte_tee_error_both_branches_and_cancel(
    scope: &Scope<'_>,
    state: ByteTeeState<'_>,
    error: HandleValue<'_>,
) {
    let b1 = byte_tee_branch_controller(scope, state, false);
    readable_byte_stream_controller_error(scope, &b1, error);
    let b2 = byte_tee_branch_controller(scope, state, true);
    readable_byte_stream_controller_error(scope, &b2, error);
    let stream = byte_tee_stream(scope, state);
    let cancel_result = readable_stream_cancel(scope, &stream, error);
    byte_tee_cancel_promise(scope, state)
        .resolve(scope, cancel_result)
        .expect("resolve cancel");
}

fn byte_tee_default_microtask(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ByteTeeState>(scope, payload);
    let chunk = state.data().pending_chunk.get(scope);
    state.data().pending_chunk.set(value::undefined());
    state.data_mut().read_again_for_branch1 = false;
    state.data_mut().read_again_for_branch2 = false;
    let canceled1 = state.data().canceled1;
    let canceled2 = state.data().canceled2;
    let mut chunk2 = chunk;
    if !canceled1 && !canceled2 {
        match byte_tee_clone_as_uint8array(scope, chunk) {
            Ok(cloned) => chunk2 = cloned,
            Err(_) => {
                let clone_err = take_pending_or_undefined(scope);
                byte_tee_error_both_branches_and_cancel(scope, state, clone_err);
                return Ok(value::undefined());
            }
        }
    }
    // If _canceled1_ is false, perform ! `ReadableByteStreamControllerEnqueue`(branch1, chunk1).
    if !canceled1 {
        let b1 = byte_tee_branch_controller(scope, state, false);
        readable_byte_stream_controller_enqueue(scope, &b1, byte_tee_view(scope, chunk))
            .expect(BYTE_TEE_ENQUEUE_INFALLIBLE);
    }
    // If _canceled2_ is false, perform ! `ReadableByteStreamControllerEnqueue`(branch2, chunk2).
    if !canceled2 {
        let b2 = byte_tee_branch_controller(scope, state, true);
        readable_byte_stream_controller_enqueue(scope, &b2, byte_tee_view(scope, chunk2))
            .expect(BYTE_TEE_ENQUEUE_INFALLIBLE);
    }
    state.data_mut().reading = false;
    if state.data().read_again_for_branch1 {
        byte_tee_pull(scope, state, false);
    } else if state.data().read_again_for_branch2 {
        byte_tee_pull(scope, state, true);
    }
    Ok(value::undefined())
}

/// The default read request's `close steps`: close both branches and respond 0
/// to any pending BYOB pull-into.
pub(crate) fn byte_tee_default_close_steps(
    scope: &Scope<'_>,
    state: ByteTeeState<'_>,
) -> Result<(), ExnThrown> {
    state.data_mut().reading = false;
    let canceled1 = state.data().canceled1;
    let canceled2 = state.data().canceled2;
    // The branch close/respond steps are infallible in the spec, but a
    // branch whose pending BYOB pull-into is partially filled below its element
    // size (reachable when a consumer does a multi-byte-element BYOB read and the
    // source delivers a non-aligned remainder) makes `…ControllerClose` throw its
    // "insufficient bytes" `TypeError`. Run them as a fallible unit; on failure,
    // error both branches and cancel the source rather than aborting.
    let settle = (|| -> Result<(), ExnThrown> {
        let b1 = byte_tee_branch_controller(scope, state, false);
        // If _canceled1_ is false, perform ! `ReadableByteStreamControllerClose`(branch1).
        if !canceled1 {
            readable_byte_stream_controller_close(scope, &b1)?;
        }
        let b2 = byte_tee_branch_controller(scope, state, true);
        // If _canceled2_ is false, perform ! `ReadableByteStreamControllerClose`(branch2).
        if !canceled2 {
            readable_byte_stream_controller_close(scope, &b2)?;
        }
        // If branch1.[[controller]].[[pendingPullIntos]] is not empty, perform !
        // `ReadableByteStreamControllerRespond`(branch1, 0).
        if !b1.data().pending_pull_intos.is_empty() {
            readable_byte_stream_controller_respond(scope, &b1, 0)?;
        }
        // If branch2.[[controller]].[[pendingPullIntos]] is not empty, perform !
        // `ReadableByteStreamControllerRespond`(branch2, 0).
        if !b2.data().pending_pull_intos.is_empty() {
            readable_byte_stream_controller_respond(scope, &b2, 0)?;
        }
        Ok(())
    })();

    if settle.is_err() {
        let error = take_pending_or_undefined(scope);
        byte_tee_error_both_branches_and_cancel(scope, state, error);
        return Ok(());
    }

    if !canceled1 || !canceled2 {
        byte_tee_cancel_promise(scope, state).resolve(scope, HandleValue::undefined())
    } else {
        Ok(())
    }
}

/// The BYOB read-into request's `chunk steps`: queue a microtask to respond to
/// the BYOB branch and enqueue a clone into the other branch.
pub(crate) fn byte_tee_byob_chunk_steps(
    scope: &Scope<'_>,
    state: ByteTeeState<'_>,
    for_branch2: bool,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    state.data().pending_chunk.set(*chunk);
    state.data_mut().pending_for_branch2 = for_branch2;
    let microtask = state.data().byob_microtask_fn.get(scope);
    js::jobs::queue_microtask(scope, &microtask)
}

fn byte_tee_byob_microtask(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ByteTeeState>(scope, payload);
    let chunk = state.data().pending_chunk.get(scope);
    state.data().pending_chunk.set(value::undefined());
    let for_branch2 = state.data().pending_for_branch2;
    state.data_mut().read_again_for_branch1 = false;
    state.data_mut().read_again_for_branch2 = false;
    // _byobBranch_ is the branch this read-into targets; _otherBranch_ is the
    // other. _byobCanceled_/_otherCanceled_ follow the same selection.
    let byob_canceled = if for_branch2 {
        state.data().canceled2
    } else {
        state.data().canceled1
    };
    let other_canceled = if for_branch2 {
        state.data().canceled1
    } else {
        state.data().canceled2
    };
    if !other_canceled {
        match byte_tee_clone_as_uint8array(scope, chunk) {
            Ok(cloned_chunk) => {
                // If _byobCanceled_ is false, perform !
                // `ReadableByteStreamControllerRespondWithNewView`(byobBranch, chunk).
                if !byob_canceled {
                    let bc = byte_tee_branch_controller(scope, state, for_branch2);
                    readable_byte_stream_controller_respond_with_new_view(
                        scope,
                        &bc,
                        byte_tee_view(scope, chunk),
                    )
                    .expect(BYTE_TEE_SETTLE_INFALLIBLE);
                }
                // Perform ! `ReadableByteStreamControllerEnqueue`(otherBranch, clonedChunk).
                let oc = byte_tee_branch_controller(scope, state, !for_branch2);
                readable_byte_stream_controller_enqueue(
                    scope,
                    &oc,
                    byte_tee_view(scope, cloned_chunk),
                )
                .expect(BYTE_TEE_ENQUEUE_INFALLIBLE);
            }
            Err(_) => {
                let clone_err = take_pending_or_undefined(scope);
                byte_tee_error_both_branches_and_cancel(scope, state, clone_err);
                return Ok(value::undefined());
            }
        }
    } else if !byob_canceled {
        // Otherwise, if _byobCanceled_ is false, perform !
        // `ReadableByteStreamControllerRespondWithNewView`(byobBranch, chunk).
        let bc = byte_tee_branch_controller(scope, state, for_branch2);
        readable_byte_stream_controller_respond_with_new_view(
            scope,
            &bc,
            byte_tee_view(scope, chunk),
        )
        .expect(BYTE_TEE_SETTLE_INFALLIBLE);
    }
    state.data_mut().reading = false;
    if state.data().read_again_for_branch1 {
        byte_tee_pull(scope, state, false);
    } else if state.data().read_again_for_branch2 {
        byte_tee_pull(scope, state, true);
    }
    Ok(value::undefined())
}

/// The BYOB read-into request's `close steps`: close both branches, respond to a
/// zero-length view, and respond 0 to the other branch's pending pull-into.
pub(crate) fn byte_tee_byob_close_steps(
    scope: &Scope<'_>,
    state: ByteTeeState<'_>,
    for_branch2: bool,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    state.data_mut().reading = false;
    // _byobBranch_ is the branch this read-into targets; _otherBranch_ is the
    // other. _byobCanceled_/_otherCanceled_ follow the same selection.
    let byob_canceled = if for_branch2 {
        state.data().canceled2
    } else {
        state.data().canceled1
    };
    let other_canceled = if for_branch2 {
        state.data().canceled1
    } else {
        state.data().canceled2
    };
    // The branch close/respond steps are infallible in the spec, but a
    // branch whose pending BYOB pull-into is partially filled below its element
    // size (reachable when a consumer does a multi-byte-element BYOB read and the
    // source delivers a non-aligned remainder) makes `…ControllerClose` throw its
    // "insufficient bytes" `TypeError`. Run them as a fallible unit; on failure,
    // error both branches and cancel the source rather than aborting.
    let settle = (|| -> Result<(), ExnThrown> {
        // If _byobCanceled_ is false, perform ! `ReadableByteStreamControllerClose`(byobBranch).
        if !byob_canceled {
            let bc = byte_tee_branch_controller(scope, state, for_branch2);
            readable_byte_stream_controller_close(scope, &bc)?;
        }
        // If _otherCanceled_ is false, perform ! `ReadableByteStreamControllerClose`(otherBranch).
        if !other_canceled {
            let oc = byte_tee_branch_controller(scope, state, !for_branch2);
            readable_byte_stream_controller_close(scope, &oc)?;
        }
        if !chunk.is_undefined() {
            // If _byobCanceled_ is false, perform !
            // `ReadableByteStreamControllerRespondWithNewView`(byobBranch, chunk).
            if !byob_canceled {
                let bc = byte_tee_branch_controller(scope, state, for_branch2);
                readable_byte_stream_controller_respond_with_new_view(
                    scope,
                    &bc,
                    byte_tee_view(scope, chunk),
                )?;
            }
            // If _otherCanceled_ is false and otherBranch's [[pendingPullIntos]] is not empty,
            // perform ! `ReadableByteStreamControllerRespond`(otherBranch, 0).
            let oc = byte_tee_branch_controller(scope, state, !for_branch2);
            if !other_canceled && !oc.data().pending_pull_intos.is_empty() {
                readable_byte_stream_controller_respond(scope, &oc, 0)?;
            }
        }
        Ok(())
    })();

    if settle.is_err() {
        let error = take_pending_or_undefined(scope);
        byte_tee_error_both_branches_and_cancel(scope, state, error);
        return Ok(());
    }

    if !byob_canceled || !other_canceled {
        byte_tee_cancel_promise(scope, state).resolve(scope, HandleValue::undefined())
    } else {
        Ok(())
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-add-read-into-request>
/// ReadableStreamAddReadIntoRequest(stream, readRequest) performs the following steps:
///
/// Boundary: takes the `#[must_root]` request by value and immediately moves it
/// into the reader's traced `[[readIntoRequests]]`; nothing allocates in
/// between, so the request never sits untraced.
#[js::allow_unrooted]
pub(crate) fn readable_stream_add_read_into_request(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    read_into_request: ReadIntoRequest,
) {
    // Step 1: Assert: _stream_.`[[reader]]` `implements` ``BYOBReader``.
    // Step 2: Assert: _stream_.`[[state]]` is "`readable`" or "`closed`".
    // Step 3: `Append` _readRequest_ to _stream_.`[[reader]]`.`[[readIntoRequests]]`.
    let reader = stream_byob_reader(scope, stream).expect("stream has a BYOB reader");
    debug_assert!(matches!(
        stream.data().state,
        ReadableStreamState::Readable | ReadableStreamState::Closed
    ));
    reader
        .data_mut()
        .read_into_requests
        .push_back(read_into_request);
}

/// <https://streams.spec.whatwg.org/#readable-stream-add-read-request>
/// ReadableStreamAddReadRequest(stream, readRequest) performs the following steps:
///
/// Boundary: takes the `#[must_root]` request by value and immediately moves it
/// into the reader's traced `[[readRequests]]`; nothing allocates in between.
#[js::allow_unrooted]
pub(crate) fn readable_stream_add_read_request(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    read_request: ReadRequest,
) {
    // Step 1: Assert: _stream_.`[[reader]]` `implements` ``DefaultReader``.
    // Step 2: Assert: _stream_.`[[state]]` is "`readable`".
    // Step 3: `Append` _readRequest_ to _stream_.`[[reader]]`.`[[readRequests]]`.
    let reader = stream_default_reader(scope, stream).expect("stream has a default reader");
    reader.data_mut().read_requests.push_back(read_request);
}

/// <https://streams.spec.whatwg.org/#readable-stream-cancel>
/// ReadableStreamCancel(stream, reason) performs the following steps:
pub(crate) fn readable_stream_cancel<'r>(
    scope: &'r Scope<'_>,
    stream: &ReadableStream<'_>,
    reason: HandleValue<'r>,
) -> Promise<'r> {
    // Step 1: Set _stream_.`[[disturbed]]` to true.
    stream.data_mut().disturbed = true;
    let state = stream.data().state;
    match state {
        // Step 2: If _stream_.`[[state]]` is "`closed`", return `a promise resolved with` undefined.
        ReadableStreamState::Closed => {
            return resolved_undefined_promise(scope);
        }
        // Step 3: If _stream_.`[[state]]` is "`errored`", return `a promise rejected with`
        //         _stream_.`[[storedError]]`.
        ReadableStreamState::Errored => {
            let stored_error = stream.data().stored_error.get(scope);
            return Promise::new_rejected_with_error(scope, stored_error)
                .expect("rejected promise");
        }
        ReadableStreamState::Readable => {}
    }
    // Step 4: Perform ! `ReadableStreamClose`(_stream_).
    readable_stream_close(scope, stream);
    // Step 5: Let _reader_ be _stream_.`[[reader]]`.
    // Step 6: If _reader_ is not undefined and _reader_ `implements` ``BYOBReader``,
    //         Let _readIntoRequests_ be _reader_.`[[readIntoRequests]]`. Set
    //         _reader_.`[[readIntoRequests]]` to an empty `list`. `For each` _readIntoRequest_ of
    //         _readIntoRequests_, Perform _readIntoRequest_’s `close steps`, given undefined.
    if let Some(reader) = stream_byob_reader(scope, stream) {
        let undef = HandleValue::undefined();
        settle_request_snapshot(
            // Drop `data_mut()` guard before call.
            {
                let mut data = reader.data_mut();
                std::mem::take(&mut data.read_into_requests)
            },
            |read_into_request| {
                read_into_request.root(scope).close_steps(scope, undef);
            },
        );
    }
    // Step 7: Let _sourceCancelPromise_ be ! _stream_.`[[controller]]`.`[[CancelSteps]]`(_reason_).
    //         The controller is polymorphic: a byte controller runs its own [[CancelSteps]].
    let source_cancel_promise = if let Some(byte_controller) = stream.byte_controller(scope) {
        byte_cancel_steps(scope, &byte_controller, reason)
    } else {
        let controller = stream
            .default_controller(scope)
            .expect("stream has a default controller");
        cancel_steps(scope, &controller, reason)
    };
    // Step 8: Return the result of `reacting` to _sourceCancelPromise_ with a fulfillment step that
    //         returns undefined.
    let on_fulfilled = js::class::get_or_init_shared_function(
        scope,
        return_undefined as *const () as usize,
        |scope| Function::new_callback(scope, c"", 1, return_undefined, ()),
    )
    .expect("create reaction");
    source_cancel_promise
        .then(scope, Some(*on_fulfilled), None)
        .expect("then")
}

/// Settle every request in a snapshot taken from a reader's traced
/// `[[readRequests]]`/`[[readIntoRequests]]`, keeping the requests not yet
/// settled GC-traced throughout.
///
/// The spec snapshots the list (sets the reader's list to empty) and then
/// iterates, settling each request. Each `ReadRequest`/`ReadIntoRequest` carries
/// untraced `Heap`s; settling one allocates and can compact, which would stale
/// the `Heap`s of the requests still waiting their turn. Rooting the snapshot
/// (`RootedTraceableBox`) and draining it front-to-back keeps the remainder
/// traced — and therefore pointer-current — across every settle step.
fn settle_request_snapshot<T: js::heap::Trace + 'static>(
    snapshot: std::collections::VecDeque<T>,
    mut settle: impl FnMut(T),
) {
    let mut snapshot = RootedTraceableBox::new(snapshot);
    while let Some(request) = snapshot.pop_front() {
        settle(request);
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-close>
/// ReadableStreamClose(stream) performs the following steps:
pub(crate) fn readable_stream_close(scope: &Scope<'_>, stream: &ReadableStream<'_>) {
    // Step 1: Assert: _stream_.`[[state]]` is "`readable`".
    debug_assert_eq!(stream.data().state, ReadableStreamState::Readable);
    // Step 2: Set _stream_.`[[state]]` to "`closed`".
    stream.data_mut().state = ReadableStreamState::Closed;
    // Step 3: Let _reader_ be _stream_.`[[reader]]`.
    // Step 4: If _reader_ is undefined, return.
    if stream.data().reader.is_none() {
        return;
    }
    // Step 5: `Resolve` _reader_.`[[closedPromise]]` with undefined.
    stream_reader_closed_promise(scope, stream)
        .resolve(scope, HandleValue::undefined())
        .expect("resolve closed promise");
    // Step 6: If _reader_ `implements` ``DefaultReader``, Let _readRequests_ be
    //         _reader_.`[[readRequests]]`. Set _reader_.`[[readRequests]]` to an empty `list`. `For
    //         each` _readRequest_ of _readRequests_, Perform _readRequest_’s `close steps`.
    //         A BYOB reader has no `[[readRequests]]`; its pending read-into requests are left as-is.
    if let Some(reader) = stream_default_reader(scope, stream) {
        settle_request_snapshot(
            // Drop `data_mut()` guard before call.
            {
                let mut data = reader.data_mut();
                std::mem::take(&mut data.read_requests)
            },
            |read_request| {
                read_request.root(scope).close_steps(scope);
            },
        );
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-error>
/// ReadableStreamError(stream, e) performs the following steps:
pub(crate) fn readable_stream_error(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    e: HandleValue<'_>,
) {
    // Step 1: Assert: _stream_.`[[state]]` is "`readable`".
    debug_assert_eq!(stream.data().state, ReadableStreamState::Readable);
    // Step 2: Set _stream_.`[[state]]` to "`errored`".
    stream.data_mut().state = ReadableStreamState::Errored;
    // Step 3: Set _stream_.`[[storedError]]` to _e_.
    stream.data_mut().stored_error.set(e.get());
    // Step 4: Let _reader_ be _stream_.`[[reader]]`.
    // Step 5: If _reader_ is undefined, return.
    if stream.data().reader.is_none() {
        return;
    }
    // Step 6: `Reject` _reader_.`[[closedPromise]]` with _e_.
    stream_reader_closed_promise(scope, stream)
        .reject(scope, e)
        .expect("reject closed promise");
    // Step 7: Set _reader_.`[[closedPromise]]`.[[PromiseIsHandled]] to true.
    stream_reader_closed_promise(scope, stream)
        .set_settled_is_handled(scope)
        .expect("set closed promise handled");
    // Step 8: If _reader_ `implements` ``DefaultReader``, Perform !
    //         `DefaultReaderErrorReadRequests`(_reader_, _e_).
    if let Some(reader) = stream_default_reader(scope, stream) {
        readable_stream_default_reader_error_read_requests(scope, &reader, e);
    } else {
        // Step 9: Otherwise, Assert: _reader_ `implements` ``BYOBReader``. Perform !
        //         `BYOBReaderErrorReadIntoRequests`(_reader_, _e_).
        let reader = stream_byob_reader(scope, stream).expect("BYOB reader");
        readable_stream_byob_reader_error_read_into_requests(scope, &reader, e);
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-fulfill-read-into-request>
/// ReadableStreamFulfillReadIntoRequest(stream, chunk, done) performs the following steps:
///
/// Boundary: removes the first request from the reader's traced
/// `[[readIntoRequests]]` and settles it immediately, with no allocation in
/// between, so the by-value `#[must_root]` request never sits untraced.
pub(crate) fn readable_stream_fulfill_read_into_request(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    chunk: HandleValue<'_>,
    done: bool,
) {
    // Step 1: Assert: ! `ReadableStreamHasBYOBReader`(_stream_) is true.
    debug_assert!(readable_stream_has_byob_reader(scope, stream));
    // Step 2: Let _reader_ be _stream_.`[[reader]]`.
    let reader = stream_byob_reader(scope, stream).expect("stream has a BYOB reader");
    // Step 3: Assert: _reader_.`[[readIntoRequests]]` is not `empty`.
    debug_assert!(!reader.data().read_into_requests.is_empty());
    // Step 4: Let _readIntoRequest_ be _reader_.`[[readIntoRequests]]`[0].
    // Step 5: `Remove` _readIntoRequest_ from _reader_.`[[readIntoRequests]]`.
    let read_into_request = reader
        .data_mut()
        .read_into_requests
        .pop_front()
        .expect("a non-empty read-into request list")
        .root(scope);
    if done {
        // Step 6: If _done_ is true, perform _readIntoRequest_’s `close steps`, given _chunk_.
        read_into_request.close_steps(scope, chunk);
    } else {
        // Step 7: Otherwise, perform _readIntoRequest_’s `chunk steps`, given _chunk_.
        read_into_request.chunk_steps(scope, chunk);
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-fulfill-read-request>
/// ReadableStreamFulfillReadRequest(stream, chunk, done) performs the following steps:
///
/// Boundary: removes the first request from the reader's traced
/// `[[readRequests]]` and settles it immediately, with no allocation in between.
pub(crate) fn readable_stream_fulfill_read_request(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    chunk: HandleValue<'_>,
    done: bool,
) {
    // Step 1: Assert: ! `ReadableStreamHasDefaultReader`(_stream_) is true.
    // Step 2: Let _reader_ be _stream_.`[[reader]]`.
    let reader = stream_default_reader(scope, stream).expect("stream has a default reader");
    // Step 3: Assert: _reader_.`[[readRequests]]` is not `empty`.
    debug_assert!(!reader.data().read_requests.is_empty());
    // Step 4: Let _readRequest_ be _reader_.`[[readRequests]]`[0].
    // Step 5: `Remove` _readRequest_ from _reader_.`[[readRequests]]`.
    // Root the popped request immediately: the bound value is the non-`must_root`
    // `StackReadRequest`, so it is held across the step calls below without an
    // `allow_unrooted`.
    let read_request = reader
        .data_mut()
        .read_requests
        .pop_front()
        .expect("a non-empty read request list")
        .root(scope);
    if done {
        // Step 6: If _done_ is true, perform _readRequest_’s `close steps`.
        read_request.close_steps(scope);
    } else {
        // Step 7: Otherwise, perform _readRequest_’s `chunk steps`, given _chunk_.
        read_request.chunk_steps(scope, chunk);
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-get-num-read-into-requests>
/// ReadableStreamGetNumReadIntoRequests(stream) performs the following steps:
pub(crate) fn readable_stream_get_num_read_into_requests(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
) -> usize {
    // Step 1: Assert: ! `ReadableStreamHasBYOBReader`(_stream_) is true.
    let reader = stream_byob_reader(scope, stream).expect("stream has a BYOB reader");
    // Step 2: Return _stream_.`[[reader]]`.`[[readIntoRequests]]`’s `size`.
    // Bind the guard to a local so it drops before `reader` at end of scope.
    let data = reader.data();
    data.read_into_requests.len()
}

/// <https://streams.spec.whatwg.org/#readable-stream-get-num-read-requests>
/// ReadableStreamGetNumReadRequests(stream) performs the following steps:
pub(crate) fn readable_stream_get_num_read_requests(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
) -> usize {
    // Step 1: Assert: ! `ReadableStreamHasDefaultReader`(_stream_) is true.
    // Step 2: Return _stream_.`[[reader]]`.`[[readRequests]]`’s `size`.
    let reader = stream_default_reader(scope, stream).expect("stream has a default reader");
    let len = reader.data().read_requests.len();
    len
}

/// <https://streams.spec.whatwg.org/#readable-stream-has-byob-reader>
/// ReadableStreamHasBYOBReader(stream) performs the following steps:
pub(crate) fn readable_stream_has_byob_reader(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
) -> bool {
    // Step 1: Let _reader_ be _stream_.`[[reader]]`.
    // Step 2: If _reader_ is undefined, return false.
    if stream.data().reader.is_none() {
        return false;
    }
    // Step 3: If _reader_ `implements` ``BYOBReader``, return true.
    // Step 4: Return false.
    stream_byob_reader(scope, stream).is_some()
}

/// <https://streams.spec.whatwg.org/#readable-stream-has-default-reader>
/// ReadableStreamHasDefaultReader(stream) performs the following steps:
pub(crate) fn readable_stream_has_default_reader(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
) -> bool {
    // Step 1: Let _reader_ be _stream_.`[[reader]]`.
    // Step 2: If _reader_ is undefined, return false.
    if stream.data().reader.is_none() {
        return false;
    }
    // Step 3: If _reader_ `implements` ``DefaultReader``, return true.
    // Step 4: Return false.
    stream_default_reader(scope, stream).is_some()
}

/// <https://streams.spec.whatwg.org/#readable-stream-reader-generic-cancel>
/// ReadableStreamReaderGenericCancel(reader, reason) performs the following steps:
pub(crate) fn readable_stream_reader_generic_cancel<'r>(
    scope: &'r Scope<'_>,
    reader: &impl GenericReader,
    reason: HandleValue<'r>,
) -> Promise<'r> {
    // Step 1: Let _stream_ be _reader_.`[[stream]]`.
    // Step 2: Assert: _stream_ is not undefined.
    let stream = reader.generic_stream(scope).expect("reader has a stream");
    // Step 3: Return ! `ReadableStreamCancel`(_stream_, _reason_).
    readable_stream_cancel(scope, &stream, reason)
}

/// <https://streams.spec.whatwg.org/#readable-stream-reader-generic-initialize>
/// ReadableStreamReaderGenericInitialize(reader, stream) performs the following steps:
pub(crate) fn readable_stream_reader_generic_initialize(
    scope: &Scope<'_>,
    reader: &impl GenericReader,
    stream: &ReadableStream<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Set _reader_.`[[stream]]` to _stream_.
    reader.set_generic_stream(stream);
    // Step 2: Set _stream_.`[[reader]]` to _reader_.
    stream.data_mut().reader = Some(HeapReadableStreamReader::from(reader.as_generic()));
    let state = stream.data().state;
    let closed_promise = match state {
        // Step 3: If _stream_.`[[state]]` is "`readable`", Set _reader_.`[[closedPromise]]` to `a
        //         new promise`.
        ReadableStreamState::Readable => Promise::new_pending(scope)?,
        // Step 4: Otherwise, if _stream_.`[[state]]` is "`closed`", Set
        //         _reader_.`[[closedPromise]]` to `a promise resolved with` undefined.
        ReadableStreamState::Closed => {
            Promise::new_resolved_with_value(scope, HandleValue::undefined())?
        }
        // Step 5: Otherwise, Assert: _stream_.`[[state]]` is "`errored`". Set
        //         _reader_.`[[closedPromise]]` to `a promise rejected with` _stream_.`[[storedError]]`.
        //         Set _reader_.`[[closedPromise]]`.[[PromiseIsHandled]] to true.
        ReadableStreamState::Errored => {
            let stored_error = stream.data().stored_error.get(scope);
            let promise = Promise::new_rejected_with_error(scope, stored_error)?;
            promise.set_settled_is_handled(scope)?;
            promise
        }
    };
    reader.set_generic_closed_promise(closed_promise);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-stream-reader-generic-release>
/// ReadableStreamReaderGenericRelease(reader) performs the following steps:
pub(crate) fn readable_stream_reader_generic_release(
    scope: &Scope<'_>,
    reader: &impl GenericReader,
) -> Result<(), ExnThrown> {
    // Step 1: Let _stream_ be _reader_.`[[stream]]`.
    // Step 2: Assert: _stream_ is not undefined.
    let stream = reader.generic_stream(scope).expect("reader has a stream");
    // Step 3: Assert: _stream_.`[[reader]]` is _reader_.
    let type_error = invalid_state_error(
        scope,
        "ReadableStream",
        c"Reader was released and can no longer be used to monitor the stream's closedness",
    );
    let state = stream.data().state;
    if state == ReadableStreamState::Readable {
        // Step 4: If _stream_.`[[state]]` is "`readable`", `reject` _reader_.`[[closedPromise]]`
        //         with a ``TypeError`` exception.
        reader
            .generic_closed_promise(scope)
            .reject(scope, type_error)?;
    } else {
        // Step 5: Otherwise, set _reader_.`[[closedPromise]]` to `a promise rejected with` a
        //         ``TypeError`` exception.
        let rejected = Promise::new_rejected_with_error(scope, type_error)?;
        reader.set_generic_closed_promise(rejected);
    }
    // Step 6: Set _reader_.`[[closedPromise]]`.[[PromiseIsHandled]] to true.
    reader
        .generic_closed_promise(scope)
        .set_settled_is_handled(scope)?;
    // Step 7: Perform ! _stream_.`[[controller]]`.`[[ReleaseSteps]]`().
    //         The controller is polymorphic: a byte controller runs its own [[ReleaseSteps]].
    if let Some(byte_controller) = stream.byte_controller(scope) {
        byte_release_steps(&byte_controller);
    } else {
        release_steps();
    }
    // Step 8: Set _stream_.`[[reader]]` to undefined.
    stream.data_mut().reader = None;
    // Step 9: Set _reader_.`[[stream]]` to undefined.
    reader.clear_generic_stream();
    Ok(())
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-BYOBReadererrorreadintorequests>
/// BYOBReaderErrorReadIntoRequests(reader, e) performs the following steps:
pub(crate) fn readable_stream_byob_reader_error_read_into_requests(
    scope: &Scope<'_>,
    reader: &BYOBReader<'_>,
    e: HandleValue<'_>,
) {
    // Step 1: Let _readIntoRequests_ be _reader_.`[[readIntoRequests]]`.
    // Step 2: Set _reader_.`[[readIntoRequests]]` to a new empty `list`.
    // Step 3: `For each` _readIntoRequest_ of _readIntoRequests_, Perform _readIntoRequest_’s
    //         `error steps`, given _e_.
    // A block expression drops the `data_mut()` guard before the call (the
    // closure re-enters this reader's data) while passing the snapshot directly.
    settle_request_snapshot(
        {
            let mut data = reader.data_mut();
            std::mem::take(&mut data.read_into_requests)
        },
        |read_into_request| {
            read_into_request.root(scope).error_steps(scope, e);
        },
    );
}

/// <https://streams.spec.whatwg.org/#readable-stream-byob-reader-read>
/// BYOBReaderRead(reader, view, min, readIntoRequest) performs the following steps:
///
/// Boundary: takes the `#[must_root]` request by value and, as its first act,
/// roots it in a `RootedTraceableBox` for the rest of the synchronous path.
#[js::allow_unrooted]
pub(crate) fn readable_stream_byob_reader_read(
    scope: &Scope<'_>,
    reader: &BYOBReader<'_>,
    view: js::ArrayBufferView<'_>,
    min: usize,
    read_into_request: ReadIntoRequest,
) {
    // The read-into request carries a `Heap` — the `read()` promise, or the
    // byte-tee state — that nothing else traces while this synchronous path
    // runs. The steps below transfer the view's buffer and construct views,
    // any of which can move that referent under a compacting GC; an untraced
    // `Heap` would then be left pointing at freed memory and its settling
    // (`*_steps` → `Heap::take`) would dereference it. Root it for the duration
    // so the GC keeps its pointer current, and hand it to each consumer via
    // `take` (leaving the box empty, which traces to nothing).
    let mut request = RootedTraceableBox::new(Some(read_into_request));
    // Step 1: Let _stream_ be _reader_.`[[stream]]`.
    // Step 2: Assert: _stream_ is not undefined.
    // Step 3: Set _stream_.`[[disturbed]]` to true.
    // Step 4: If _stream_.`[[state]]` is "`errored`", perform _readIntoRequest_’s `error steps`
    //         given _stream_.`[[storedError]]`.
    // Step 5: Otherwise, perform !
    //         `ByteStreamControllerPullInto`(_stream_.`[[controller]]`, _view_, _min_,
    //         _readIntoRequest_).
    let stream = reader
        .data()
        .stream
        .get(scope)
        .expect("reader has a stream");
    stream.data_mut().disturbed = true;
    if stream.data().state == ReadableStreamState::Errored {
        let stored_error = stream.data().stored_error.get(scope);
        request
            .take()
            .unwrap()
            .root(scope)
            .error_steps(scope, stored_error);
    } else {
        let controller = stream
            .byte_controller(scope)
            .expect("byte stream has a byte controller");
        readable_byte_stream_controller_pull_into(scope, &controller, view, min, &mut request);
    }
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-BYOBReaderrelease>
/// BYOBReaderRelease(reader) performs the following steps:
pub(crate) fn readable_stream_byob_reader_release(
    scope: &Scope<'_>,
    reader: &BYOBReader<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Perform ! `ReadableStreamReaderGenericRelease`(_reader_).
    readable_stream_reader_generic_release(scope, reader)?;
    // Step 2: Let _e_ be a new ``TypeError`` exception.
    let e = invalid_state_error(scope, "ReadableStream", c"Reader was released");
    // Step 3: Perform ! `BYOBReaderErrorReadIntoRequests`(_reader_, _e_).
    readable_stream_byob_reader_error_read_into_requests(scope, reader, e);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-DefaultReadererrorreadrequests>
/// DefaultReaderErrorReadRequests(reader, e) performs the following steps:
pub(crate) fn readable_stream_default_reader_error_read_requests(
    scope: &Scope<'_>,
    reader: &DefaultReader<'_>,
    e: HandleValue<'_>,
) {
    // Step 1: Let _readRequests_ be _reader_.`[[readRequests]]`.
    // Step 2: Set _reader_.`[[readRequests]]` to a new empty `list`.
    // Step 3: `For each` _readRequest_ of _readRequests_, Perform _readRequest_’s `error steps`,
    //         given _e_.
    settle_request_snapshot(
        // Drop `data_mut()` guard before call.
        {
            let mut data = reader.data_mut();
            std::mem::take(&mut data.read_requests)
        },
        |read_request| {
            read_request.root(scope).error_steps(scope, e);
        },
    );
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-reader-read>
/// DefaultReaderRead(reader, readRequest) performs the following steps:
///
/// Boundary: takes the `#[must_root]` request by value and, as its first act,
/// roots it in a `RootedTraceableBox` for the rest of the synchronous path.
#[js::allow_unrooted]
pub(crate) fn readable_stream_default_reader_read(
    scope: &Scope<'_>,
    reader: DefaultReader<'_>,
    read_request: ReadRequest,
) {
    // The read request carries an untraced `Heap` (the `read()` promise, or the
    // tee/pipe/async-iterator state). The steps below — `stream`/controller
    // lookups, `[[PullSteps]]`, queue fills — all allocate and can compact,
    // which would stale that `Heap`. Root it for the duration and hand it to
    // each consumer via `take` (see `readable_stream_byob_reader_read` for the
    // same pattern on the BYOB side).
    let mut read_request = RootedTraceableBox::new(Some(read_request));
    // Step 1: Let _stream_ be _reader_.`[[stream]]`.
    // Step 2: Assert: _stream_ is not undefined.
    let stream: ReadableStream<'_> = reader
        .data()
        .stream
        .get(scope)
        .expect("reader has a stream");
    // Step 3: Set _stream_.`[[disturbed]]` to true.
    stream.data_mut().disturbed = true;
    let state = stream.data().state;
    match state {
        // Step 4: If _stream_.`[[state]]` is "`closed`", perform _readRequest_’s `close steps`.
        ReadableStreamState::Closed => {
            read_request.take().unwrap().root(scope).close_steps(scope);
        }
        // Step 5: Otherwise, if _stream_.`[[state]]` is "`errored`", perform _readRequest_’s `error
        //         steps` given _stream_.`[[storedError]]`.
        ReadableStreamState::Errored => {
            let stored_error = stream.data().stored_error.get(scope);
            read_request
                .take()
                .unwrap()
                .root(scope)
                .error_steps(scope, stored_error);
        }
        // Step 6: Otherwise, Assert: _stream_.`[[state]]` is "`readable`". Perform !
        //         _stream_.`[[controller]]`.`[[PullSteps]]`(_readRequest_).
        //         The controller is polymorphic: a byte controller runs its own [[PullSteps]].
        ReadableStreamState::Readable => {
            if let Some(byte_controller) = stream.byte_controller(scope) {
                byte_pull_steps(scope, &byte_controller, &mut read_request);
            } else {
                let controller = stream
                    .default_controller(scope)
                    .expect("stream has a controller");
                pull_steps(scope, &controller, &mut read_request);
            }
        }
    }
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-DefaultReaderrelease>
/// DefaultReaderRelease(reader) performs the following steps:
pub(crate) fn readable_stream_default_reader_release(
    scope: &Scope<'_>,
    reader: &DefaultReader<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Perform ! `ReadableStreamReaderGenericRelease`(_reader_).
    readable_stream_reader_generic_release(scope, reader)?;
    // Step 2: Let _e_ be a new ``TypeError`` exception.
    let e = invalid_state_error(scope, "ReadableStream", c"Reader was released");
    // Step 3: Perform ! `DefaultReaderErrorReadRequests`(_reader_, _e_).
    readable_stream_default_reader_error_read_requests(scope, reader, e);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#set-up-readable-stream-byob-reader>
/// SetUpBYOBReader(reader, stream) performs the following steps:
pub(crate) fn set_up_readable_stream_byob_reader(
    scope: &Scope<'_>,
    reader: &BYOBReader<'_>,
    stream: &ReadableStream<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: If ! `IsReadableStreamLocked`(_stream_) is true, throw a ``TypeError`` exception.
    if stream.is_locked() {
        return Err(crate::algorithms::throw_invalid_state(
            scope,
            "ReadableStream",
            c"This stream has already been locked for exclusive reading by another reader",
        ));
    }
    // Step 2: If _stream_.`[[controller]]` does not `implement` ``ReadableByteStreamController``,
    //         throw a ``TypeError`` exception.
    if stream.byte_controller(scope).is_none() {
        return Err(js::error::throw_type_error(
            scope,
            c"Cannot use a BYOB reader with a non-byte stream",
        ));
    }
    // Step 3: Perform ! `ReadableStreamReaderGenericInitialize`(_reader_, _stream_).
    readable_stream_reader_generic_initialize(scope, reader, stream)?;
    // Step 4: Set _reader_.`[[readIntoRequests]]` to a new empty `list`.
    reader.data_mut().read_into_requests = std::collections::VecDeque::new();
    Ok(())
}

/// <https://streams.spec.whatwg.org/#set-up-readable-stream-default-reader>
/// SetUpDefaultReader(reader, stream) performs the following steps:
pub(crate) fn set_up_readable_stream_default_reader(
    scope: &Scope<'_>,
    reader: &DefaultReader<'_>,
    stream: &ReadableStream<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: If ! `IsReadableStreamLocked`(_stream_) is true, throw a ``TypeError`` exception.
    if stream.is_locked() {
        return Err(crate::algorithms::throw_invalid_state(
            scope,
            "ReadableStream",
            c"This stream has already been locked for exclusive reading by another reader",
        ));
    }
    // Step 2: Perform ! `ReadableStreamReaderGenericInitialize`(_reader_, _stream_).
    readable_stream_reader_generic_initialize(scope, reader, stream)?;
    // Step 3: Set _reader_.`[[readRequests]]` to a new empty `list`.
    reader.data_mut().read_requests = std::collections::VecDeque::new();
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-call-pull-if-needed>
/// DefaultControllerCallPullIfNeeded(controller) performs the following steps:
pub(crate) fn readable_stream_default_controller_call_pull_if_needed(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
) {
    // Step 1: Let _shouldPull_ be ! `DefaultControllerShouldCallPull`(_controller_).
    let should_pull = readable_stream_default_controller_should_call_pull(scope, controller);
    // Step 2: If _shouldPull_ is false, return.
    if !should_pull {
        return;
    }
    // Step 3: If _controller_.`[[pulling]]` is true, Set _controller_.`[[pullAgain]]` to true.
    //         Return.
    if controller.data().pulling {
        controller.data_mut().pull_again = true;
        return;
    }
    // Step 4: Assert: _controller_.`[[pullAgain]]` is false.
    debug_assert!(!controller.data().pull_again);
    // Step 5: Set _controller_.`[[pulling]]` to true.
    controller.data_mut().pulling = true;
    // Step 6: Let _pullPromise_ be the result of performing _controller_.`[[pullAlgorithm]]`.
    //         The pull algorithm is invoked with the controller as its argument (the underlying
    //         source's `pull(controller)`).
    let pull_algorithm = controller.data().pull_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let pull_promise =
        support::invoke_promise_algorithm(scope, pull_algorithm, receiver, &[controller]);
    // Step 7: `Upon fulfillment` of _pullPromise_, Set _controller_.`[[pulling]]` to false. If
    //         _controller_.`[[pullAgain]]` is true, Set _controller_.`[[pullAgain]]` to false.
    //         Perform ! `DefaultControllerCallPullIfNeeded`(_controller_).
    // Step 8: `Upon rejection` of _pullPromise_ with reason _e_, Perform !
    //         `DefaultControllerError`(_controller_, _e_).
    // (Steps 7 and 8 are implemented by `pull_promise_fulfilled` / `pull_promise_rejected`.)
    if controller.data().pull_fulfilled_fn.is_none() {
        let fulfilled = Function::new_callback(scope, c"", 1, pull_promise_fulfilled, controller)
            .expect("create pull reaction");
        controller.data_mut().pull_fulfilled_fn.set(fulfilled);
        let rejected = Function::new_callback(scope, c"", 1, pull_promise_rejected, controller)
            .expect("create pull reaction");
        controller.data_mut().pull_rejected_fn.set(rejected);
    }
    let fulfilled = controller
        .data()
        .pull_fulfilled_fn
        .get(scope)
        .expect("created above");
    let rejected = controller
        .data()
        .pull_rejected_fn
        .get(scope)
        .expect("created above");
    pull_promise
        .add_reactions_ignoring_unhandled_rejection(scope, Some(*fulfilled), Some(*rejected))
        .expect("attach pull reactions");
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-should-call-pull>
/// DefaultControllerShouldCallPull(controller) performs the following steps:
pub(crate) fn readable_stream_default_controller_should_call_pull(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
) -> bool {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: If ! `DefaultControllerCanCloseOrEnqueue`(_controller_) is false,
    //         return false.
    if !readable_stream_default_controller_can_close_or_enqueue(scope, controller) {
        return false;
    }
    // Step 3: If _controller_.`[[started]]` is false, return false.
    if !controller.data().started {
        return false;
    }
    // Step 4: If ! `IsReadableStreamLocked`(_stream_) is true and !
    //         `ReadableStreamGetNumReadRequests`(_stream_) > 0, return true.
    if stream.is_locked() && readable_stream_get_num_read_requests(scope, &stream) > 0 {
        return true;
    }
    // Step 5: Let _desiredSize_ be ! `DefaultControllerGetDesiredSize`(_controller_).
    let desired_size = readable_stream_default_controller_get_desired_size(scope, controller);
    // Step 6: Assert: _desiredSize_ is not null.
    let desired_size = desired_size.expect("desiredSize is not null when readable");
    // Step 7: If _desiredSize_ > 0, return true.
    // Step 8: Return false.
    desired_size > 0.0
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-clear-algorithms>
/// DefaultControllerClearAlgorithms(controller) is called once the stream is closed or errored and the algorithms will not be executed any more. By removing the algorithm references it permits the underlying source object to be garbage collected even if the ReadableStream itself is still referenced. This is observable using weak references. See tc39/proposal-weakrefs#31 for more detail. It performs the following steps:
pub(crate) fn readable_stream_default_controller_clear_algorithms(
    controller: &ReadableStreamDefaultController<'_>,
) {
    // Step 1: Set _controller_.`[[pullAlgorithm]]` to undefined.
    controller.data_mut().pull_algorithm = Algorithm::None;
    // Step 2: Set _controller_.`[[cancelAlgorithm]]` to undefined.
    controller.data_mut().cancel_algorithm = Algorithm::None;
    // Step 3: Set _controller_.`[[strategySizeAlgorithm]]` to undefined.
    controller
        .data_mut()
        .strategy_size_algorithm
        .set(value::undefined());
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-close>
/// DefaultControllerClose(controller) performs the following steps:
pub(crate) fn readable_stream_default_controller_close(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
) {
    // Step 1: If ! `DefaultControllerCanCloseOrEnqueue`(_controller_) is false,
    //         return.
    if !readable_stream_default_controller_can_close_or_enqueue(scope, controller) {
        return;
    }
    // Step 2: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 3: Set _controller_.`[[closeRequested]]` to true.
    controller.data_mut().close_requested = true;
    // Step 4: If _controller_.`[[queue]]` `is empty`, Perform !
    //         `DefaultControllerClearAlgorithms`(_controller_). Perform !
    //         `ReadableStreamClose`(_stream_).
    if controller.data().queue.is_empty() {
        readable_stream_default_controller_clear_algorithms(controller);
        readable_stream_close(scope, &stream);
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-enqueue>
/// DefaultControllerEnqueue(controller, chunk) performs the following steps:
pub(crate) fn readable_stream_default_controller_enqueue(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: If ! `DefaultControllerCanCloseOrEnqueue`(_controller_) is false,
    //         return.
    if !readable_stream_default_controller_can_close_or_enqueue(scope, controller) {
        return Ok(());
    }
    // Step 2: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 3: If ! `IsReadableStreamLocked`(_stream_) is true and !
    //         `ReadableStreamGetNumReadRequests`(_stream_) > 0, perform !
    //         `ReadableStreamFulfillReadRequest`(_stream_, _chunk_, false).
    if stream.is_locked() && readable_stream_get_num_read_requests(scope, &stream) > 0 {
        readable_stream_fulfill_read_request(scope, &stream, chunk, false);
    } else {
        // Step 4: Otherwise, Let _result_ be the result of performing
        //         _controller_.`[[strategySizeAlgorithm]]`, passing in _chunk_, and interpreting the
        //         result as a `completion record`. If _result_ is an abrupt completion, Perform !
        //         `DefaultControllerError`(_controller_, _result_.[[Value]]). Return
        //         _result_. Let _chunkSize_ be _result_.[[Value]]. Let _enqueueResult_ be
        //         `EnqueueValueWithSize`(_controller_, _chunk_, _chunkSize_). If _enqueueResult_ is
        //         an abrupt completion, Perform ! `DefaultControllerError`(_controller_,
        //         _enqueueResult_.[[Value]]). Return _enqueueResult_.
        let size_algorithm = controller.data().strategy_size_algorithm.get(scope);
        let undef = HandleValue::undefined();
        let chunk_size = if size_algorithm.is_undefined() {
            // Absent size algorithm: the constant-1 algorithm.
            Ok(1.0)
        } else {
            support::invoke_algorithm(
                scope,
                support::AlgorithmArg::Js(size_algorithm),
                undef,
                &[chunk],
            )
            .and_then(|v| {
                use js::conversion::FromJSVal;
                f64::from_jsval_throwing(scope, v, ())
            })
        };
        let chunk_size = match chunk_size {
            Ok(size) => size,
            // Abrupt completion from the size algorithm: error the controller with the thrown
            // value and re-throw it.
            Err(_) => return Err(error_controller_with_pending(scope, controller)),
        };
        if enqueue_value_with_size(scope, &mut *controller.data_mut(), chunk, chunk_size).is_err() {
            return Err(error_controller_with_pending(scope, controller));
        }
    }
    // Step 5: Perform ! `DefaultControllerCallPullIfNeeded`(_controller_).
    readable_stream_default_controller_call_pull_if_needed(scope, controller);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-error>
/// DefaultControllerError(controller, e) performs the following steps:
pub(crate) fn readable_stream_default_controller_error(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
    e: HandleValue<'_>,
) {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: If _stream_.`[[state]]` is not "`readable`", return.
    if stream.data().state != ReadableStreamState::Readable {
        return;
    }
    // Step 3: Perform ! `ResetQueue`(_controller_).
    reset_queue(&mut *controller.data_mut());
    // Step 4: Perform ! `DefaultControllerClearAlgorithms`(_controller_).
    readable_stream_default_controller_clear_algorithms(controller);
    // Step 5: Perform ! `ReadableStreamError`(_stream_, _e_).
    readable_stream_error(scope, &stream, e);
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-get-desired-size>
/// DefaultControllerGetDesiredSize(controller) performs the following steps:
pub(crate) fn readable_stream_default_controller_get_desired_size(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
) -> Option<f64> {
    // Step 1: Let _state_ be _controller_.`[[stream]]`.`[[state]]`.
    let state = controller.stream(scope).data().state;
    match state {
        // Step 2: If _state_ is "`errored`", return null.
        ReadableStreamState::Errored => None,
        // Step 3: If _state_ is "`closed`", return 0.
        ReadableStreamState::Closed => Some(0.0),
        // Step 4: Return _controller_.`[[strategyHWM]]` − _controller_.`[[queueTotalSize]]`.
        ReadableStreamState::Readable => {
            Some(controller.data().strategy_hwm - controller.data().queue_total_size)
        }
    }
}

/// <https://streams.spec.whatwg.org/#rs-default-controller-has-backpressure>
/// ReadableStreamDefaultControllerHasBackpressure(controller) is used in the implementation of TransformStream. It performs the following steps:
pub(crate) fn readable_stream_default_controller_has_backpressure(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
) -> bool {
    // Step 1: If ! `ReadableStreamDefaultControllerShouldCallPull`(_controller_) is true, return
    //         false.
    if readable_stream_default_controller_should_call_pull(scope, controller) {
        return false;
    }
    // Step 2: Otherwise, return true.
    true
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-can-close-or-enqueue>
/// DefaultControllerCanCloseOrEnqueue(controller) performs the following steps:
pub(crate) fn readable_stream_default_controller_can_close_or_enqueue(
    scope: &Scope<'_>,
    controller: &ReadableStreamDefaultController<'_>,
) -> bool {
    // Step 1: Let _state_ be _controller_.`[[stream]]`.`[[state]]`.
    let state = controller.stream(scope).data().state;
    // Step 2: If _controller_.`[[closeRequested]]` is false and _state_ is "`readable`", return
    //         true.
    // Step 3: Otherwise, return false.
    !controller.data().close_requested && state == ReadableStreamState::Readable
}

/// <https://streams.spec.whatwg.org/#set-up-readable-stream-default-controller>
/// SetUpDefaultController(stream, controller, startAlgorithm, pullAlgorithm, cancelAlgorithm, highWaterMark, sizeAlgorithm) performs the following steps:
#[allow(clippy::too_many_arguments)]
pub(crate) fn set_up_readable_stream_default_controller(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    controller: &ReadableStreamDefaultController<'_>,
    start_algorithm: AlgorithmArg<'_>,
    pull_algorithm: AlgorithmArg<'_>,
    cancel_algorithm: AlgorithmArg<'_>,
    algorithm_receiver: HandleValue<'_>,
    high_water_mark: f64,
    size_algorithm: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Assert: _stream_.`[[controller]]` is undefined.
    debug_assert!(stream.data().controller.is_none());
    // Step 2: Set _controller_.`[[stream]]` to _stream_.
    //         (Already done by `ReadableStreamDefaultController::new`.)
    debug_assert!(stream.eq_heap(&controller.data().stream));
    // Step 3: Perform ! `ResetQueue`(_controller_).
    reset_queue(&mut *controller.data_mut());
    // Step 4: Set _controller_.`[[started]]`, _controller_.`[[closeRequested]]`,
    //         _controller_.`[[pullAgain]]`, and _controller_.`[[pulling]]` to false.
    {
        let mut data = controller.data_mut();
        data.started = false;
        data.close_requested = false;
        data.pull_again = false;
        data.pulling = false;
    }
    // Step 5: Set _controller_.`[[strategySizeAlgorithm]]` to _sizeAlgorithm_ and
    //         _controller_.`[[strategyHWM]]` to _highWaterMark_.
    controller
        .data_mut()
        .strategy_size_algorithm
        .set(size_algorithm.get());
    controller.data_mut().strategy_hwm = high_water_mark;
    // Step 6: Set _controller_.`[[pullAlgorithm]]` to _pullAlgorithm_.
    controller.data_mut().pull_algorithm = Algorithm::from(pull_algorithm);
    // Step 7: Set _controller_.`[[cancelAlgorithm]]` to _cancelAlgorithm_.
    controller.data_mut().cancel_algorithm = Algorithm::from(cancel_algorithm);
    // (The algorithms close over `algorithm_receiver` — the underlying source — as their
    // `this` value; see the `algorithm_receiver` field.)
    controller
        .data_mut()
        .algorithm_receiver
        .set(algorithm_receiver.get());
    // Step 8: Set _stream_.`[[controller]]` to _controller_.
    stream.data_mut().controller = Some(HeapReadableStreamController::from(*controller));
    // Step 9: Let _startResult_ be the result of performing _startAlgorithm_. (This might throw an
    //         exception.)
    let start_result =
        support::invoke_algorithm(scope, start_algorithm, algorithm_receiver, &[controller])?;
    // Step 10: Let _startPromise_ be `a promise resolved with` _startResult_.
    //          As in the writable controller's setup, WebIDL "a promise resolved
    //          with" creates a new promise (it does not return a promise input
    //          as-is the way `Promise.resolve` does).
    let start_promise = Promise::new_resolved_with_value(scope, start_result)?;
    // Step 11: `Upon fulfillment` of _startPromise_, Set _controller_.`[[started]]` to true.
    //          Assert: _controller_.`[[pulling]]` is false. Assert: _controller_.`[[pullAgain]]` is
    //          false. Perform ! `DefaultControllerCallPullIfNeeded`(_controller_).
    // Step 12: `Upon rejection` of _startPromise_ with reason _r_, Perform !
    //          `DefaultControllerError`(_controller_, _r_).
    // (Steps 11 and 12 are implemented by `start_promise_fulfilled` / `start_promise_rejected`.)
    support::react(
        scope,
        &start_promise,
        Some((start_promise_fulfilled, controller)),
        Some((start_promise_rejected, controller)),
    )?;
    Ok(())
}

/// <https://streams.spec.whatwg.org/#set-up-readable-stream-default-controller-from-underlying-source>
/// SetUpDefaultControllerFromUnderlyingSource(stream, underlyingSource, underlyingSourceDict, highWaterMark, sizeAlgorithm) performs the following steps:
pub(crate) fn set_up_readable_stream_default_controller_from_underlying_source(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    underlying_source: HandleValue<'_>,
    underlying_source_dict: &UnderlyingSource<'_>,
    high_water_mark: f64,
    size_algorithm: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Let _controller_ be a `new` ``ReadableStreamDefaultController``.
    let controller = ReadableStreamDefaultController::new(scope, *stream)?;
    // The start/pull/cancel algorithms are the source's callbacks, invoked with `this` =
    // _underlyingSource_ (passed below as the algorithm receiver). An absent callback is no
    // algorithm, which the invoker treats as the resolved-undefined / constant algorithm.
    // Step 2: Let _startAlgorithm_ be an algorithm that returns undefined.
    // Step 5: If _underlyingSourceDict_["``start``"] `exists`, then set _startAlgorithm_ to an
    //         algorithm which returns the result of `invoking` _underlyingSourceDict_["``start``"]
    //         with argument list « _controller_ » and `callback this value` _underlyingSource_.
    let start_algorithm = AlgorithmArg::from_member(scope, underlying_source_dict.start.as_ref());
    // Step 3: Let _pullAlgorithm_ be an algorithm that returns `a promise resolved with` undefined.
    // Step 6: If _underlyingSourceDict_["``pull``"] `exists`, then set _pullAlgorithm_ to an
    //         algorithm which returns the result of `invoking` _underlyingSourceDict_["``pull``"]
    //         with argument list « _controller_ » and `callback this value` _underlyingSource_.
    let pull_algorithm = AlgorithmArg::from_member(scope, underlying_source_dict.pull.as_ref());
    // Step 4: Let _cancelAlgorithm_ be an algorithm that returns `a promise resolved with`
    //         undefined.
    // Step 7: If _underlyingSourceDict_["``cancel``"] `exists`, then set _cancelAlgorithm_ to an
    //         algorithm which takes an argument _reason_ and returns the result of `invoking`
    //         _underlyingSourceDict_["``cancel``"] with argument list « _reason_ » and `callback
    //         this value` _underlyingSource_.
    let cancel_algorithm = AlgorithmArg::from_member(scope, underlying_source_dict.cancel.as_ref());
    // Step 8: Perform ? `SetUpDefaultController`(_stream_, _controller_,
    //         _startAlgorithm_, _pullAlgorithm_, _cancelAlgorithm_, _highWaterMark_,
    //         _sizeAlgorithm_).
    set_up_readable_stream_default_controller(
        scope,
        stream,
        &controller,
        start_algorithm,
        pull_algorithm,
        cancel_algorithm,
        underlying_source,
        high_water_mark,
        size_algorithm,
    )
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-call-pull-if-needed>
/// ByteStreamControllerCallPullIfNeeded(controller) performs the following steps:
pub(crate) fn readable_byte_stream_controller_call_pull_if_needed(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) {
    // Step 1: Let _shouldPull_ be ! `ByteStreamControllerShouldCallPull`(_controller_).
    let should_pull = readable_byte_stream_controller_should_call_pull(scope, controller);
    // Step 2: If _shouldPull_ is false, return.
    if !should_pull {
        return;
    }
    // Step 3: If _controller_.`[[pulling]]` is true, Set _controller_.`[[pullAgain]]` to true.
    //         Return.
    if controller.data().pulling {
        controller.data_mut().pull_again = true;
        return;
    }
    // Step 4: Assert: _controller_.`[[pullAgain]]` is false.
    debug_assert!(!controller.data().pull_again);
    // Step 5: Set _controller_.`[[pulling]]` to true.
    controller.data_mut().pulling = true;
    // Step 6: Let _pullPromise_ be the result of performing _controller_.`[[pullAlgorithm]]`.
    //         The pull algorithm is invoked with the controller as its argument (the underlying
    //         byte source's `pull(controller)`).
    let pull_algorithm = controller.data().pull_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let pull_promise =
        support::invoke_promise_algorithm(scope, pull_algorithm, receiver, &[controller]);
    // Step 7: `Upon fulfillment` of _pullPromise_, Set _controller_.`[[pulling]]` to false. If
    //         _controller_.`[[pullAgain]]` is true, Set _controller_.`[[pullAgain]]` to false.
    //         Perform ! `ByteStreamControllerCallPullIfNeeded`(_controller_).
    // Step 8: `Upon rejection` of _pullPromise_ with reason _e_, Perform !
    //         `ByteStreamControllerError`(_controller_, _e_).
    // (Steps 7 and 8 are implemented by `byte_pull_promise_fulfilled` / `byte_pull_promise_rejected`.)
    if controller.data().pull_fulfilled_fn.is_none() {
        let fulfilled =
            Function::new_callback(scope, c"", 1, byte_pull_promise_fulfilled, controller)
                .expect("create byte pull reaction");
        controller.data_mut().pull_fulfilled_fn.set(fulfilled);
        let rejected =
            Function::new_callback(scope, c"", 1, byte_pull_promise_rejected, controller)
                .expect("create byte pull reaction");
        controller.data_mut().pull_rejected_fn.set(rejected);
    }
    let fulfilled = controller
        .data()
        .pull_fulfilled_fn
        .get(scope)
        .expect("created above");
    let rejected = controller
        .data()
        .pull_rejected_fn
        .get(scope)
        .expect("created above");
    pull_promise
        .add_reactions_ignoring_unhandled_rejection(scope, Some(*fulfilled), Some(*rejected))
        .expect("attach byte pull reactions");
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-clear-algorithms>
/// ByteStreamControllerClearAlgorithms(controller) is called once the stream is closed or errored and the algorithms will not be executed any more. By removing the algorithm references it permits the underlying byte source object to be garbage collected even if the ReadableStream itself is still referenced. This is observable using weak references. See tc39/proposal-weakrefs#31 for more detail. It performs the following steps:
pub(crate) fn readable_byte_stream_controller_clear_algorithms(
    controller: &ReadableByteStreamController<'_>,
) {
    // Step 1: Set _controller_.`[[pullAlgorithm]]` to undefined.
    controller.data_mut().pull_algorithm = Algorithm::None;
    // Step 2: Set _controller_.`[[cancelAlgorithm]]` to undefined.
    controller.data_mut().cancel_algorithm = Algorithm::None;
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-clear-pending-pull-intos>
/// ByteStreamControllerClearPendingPullIntos(controller) performs the following steps:
pub(crate) fn readable_byte_stream_controller_clear_pending_pull_intos(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) {
    // Step 1: Perform ! `ByteStreamControllerInvalidateBYOBRequest`(_controller_).
    readable_byte_stream_controller_invalidate_byob_request(scope, controller);
    // Step 2: Set _controller_.`[[pendingPullIntos]]` to a new empty `list`.
    controller.data_mut().pending_pull_intos.clear();
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-close>
/// ByteStreamControllerClose(controller) performs the following steps:
pub(crate) fn readable_byte_stream_controller_close(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: If _controller_.`[[closeRequested]]` is true or _stream_.`[[state]]` is not
    //         "`readable`", return.
    if controller.data().close_requested || stream.data().state != ReadableStreamState::Readable {
        return Ok(());
    }
    // Step 3: If _controller_.`[[queueTotalSize]]` > 0, Set _controller_.`[[closeRequested]]` to
    //         true. Return.
    if controller.data().queue_total_size > 0.0 {
        controller.data_mut().close_requested = true;
        return Ok(());
    }
    // Step 4: If _controller_.`[[pendingPullIntos]]` is not empty, Let _firstPendingPullInto_ be
    //         _controller_.`[[pendingPullIntos]]`[0]. If the remainder after dividing
    //         _firstPendingPullInto_’s `bytes filled` by _firstPendingPullInto_’s `element
    //         size` is not 0, Let _e_ be a new ``TypeError`` exception. Perform !
    //         `ByteStreamControllerError`(_controller_, _e_). Throw _e_.
    if !controller.data().pending_pull_intos.is_empty() {
        let (bytes_filled, element_size) = {
            let data = controller.data();
            let first = &data.pending_pull_intos[0];
            (first.bytes_filled, first.element_size)
        };
        if bytes_filled % element_size != 0 {
            let e = invalid_state_error(
                scope,
                "ReadableStream",
                c"Insufficient bytes to fill the pending pull-into",
            );
            readable_byte_stream_controller_error(scope, controller, e);
            return Err(js::exception::set_pending(
                scope,
                e,
                js::native::ExceptionStackBehavior::DoNotCapture,
            ));
        }
    }
    // Step 5: Perform ! `ByteStreamControllerClearAlgorithms`(_controller_).
    readable_byte_stream_controller_clear_algorithms(controller);
    // Step 6: Perform ! `ReadableStreamClose`(_stream_).
    readable_stream_close(scope, &stream);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-commit-pull-into-descriptor>
/// ByteStreamControllerCommitPullIntoDescriptor(stream, pullIntoDescriptor) performs the following steps:
pub(crate) fn readable_byte_stream_controller_commit_pull_into_descriptor(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    pull_into_descriptor: &PullIntoDescriptor,
) -> Result<(), ExnThrown> {
    // Step 1: Assert: _stream_.`[[state]]` is not "`errored`".
    debug_assert_ne!(stream.data().state, ReadableStreamState::Errored);
    // Step 2: Assert: _pullIntoDescriptor_.`reader type` is not "`none`".
    debug_assert_ne!(pull_into_descriptor.reader_type, ReaderType::None);
    // Step 3: Let _done_ be false.
    let mut done = false;
    // Step 4: If _stream_.`[[state]]` is "`closed`", Assert: the remainder after dividing
    //         _pullIntoDescriptor_’s `bytes filled` by _pullIntoDescriptor_’s `element size` is
    //         0. Set _done_ to true.
    if stream.data().state == ReadableStreamState::Closed {
        debug_assert_eq!(
            pull_into_descriptor.bytes_filled % pull_into_descriptor.element_size,
            0
        );
        done = true;
    }
    // Step 5: Let _filledView_ be !
    //         `ByteStreamControllerConvertPullIntoDescriptor`(_pullIntoDescriptor_).
    let filled_view =
        readable_byte_stream_controller_convert_pull_into_descriptor(scope, pull_into_descriptor)?;
    let filled_view = scope.root_value(filled_view.as_value());
    // Step 6: If _pullIntoDescriptor_’s `reader type` is "`default`", Perform !
    //         `ReadableStreamFulfillReadRequest`(_stream_, _filledView_, _done_).
    if pull_into_descriptor.reader_type == ReaderType::Default {
        readable_stream_fulfill_read_request(scope, stream, filled_view, done);
    } else {
        // Step 7: Otherwise, Assert: _pullIntoDescriptor_’s `reader type` is "`byob`". Perform !
        //         `ReadableStreamFulfillReadIntoRequest`(_stream_, _filledView_, _done_).
        debug_assert_eq!(pull_into_descriptor.reader_type, ReaderType::Byob);
        readable_stream_fulfill_read_into_request(scope, stream, filled_view, done);
    }
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-convert-pull-into-descriptor>
/// ByteStreamControllerConvertPullIntoDescriptor(pullIntoDescriptor) performs the following steps:
fn readable_byte_stream_controller_convert_pull_into_descriptor<'r>(
    scope: &'r Scope<'_>,
    pull_into_descriptor: &PullIntoDescriptor,
) -> Result<Object<'r>, ExnThrown> {
    // Step 1: Let _bytesFilled_ be _pullIntoDescriptor_’s `bytes filled`.
    let bytes_filled = pull_into_descriptor.bytes_filled;
    // Step 2: Let _elementSize_ be _pullIntoDescriptor_’s `element size`.
    let element_size = pull_into_descriptor.element_size;
    // Step 3: Assert: _bytesFilled_ ≤ _pullIntoDescriptor_’s `byte length`.
    debug_assert!(bytes_filled <= pull_into_descriptor.byte_length);
    // Step 4: Assert: the remainder after dividing _bytesFilled_ by _elementSize_ is 0.
    debug_assert_eq!(bytes_filled % element_size, 0);
    // Step 5: Let _buffer_ be ! `TransferArrayBuffer`(_pullIntoDescriptor_’s `buffer`).
    let buffer = descriptor_buffer(scope, pull_into_descriptor).transfer(scope)?;
    // Step 6: Return ! `Construct`(_pullIntoDescriptor_’s `view constructor`, « _buffer_,
    //         _pullIntoDescriptor_’s `byte offset`, _bytesFilled_ ÷ _elementSize_ »).
    js::typedarray::construct_view(
        scope,
        pull_into_descriptor.view_kind,
        buffer,
        pull_into_descriptor.byte_offset,
        bytes_filled / element_size,
    )
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-enqueue>
/// ByteStreamControllerEnqueue(controller, chunk) performs the following steps:
pub(crate) fn readable_byte_stream_controller_enqueue(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    chunk: js::ArrayBufferView<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: If _controller_.`[[closeRequested]]` is true or _stream_.`[[state]]` is not
    //         "`readable`", return.
    if controller.data().close_requested || stream.data().state != ReadableStreamState::Readable {
        return Ok(());
    }
    // Step 3: Let _buffer_ be _chunk_.[[ViewedArrayBuffer]].
    let buffer = chunk.viewed_buffer(scope)?;
    // Step 4: Let _byteOffset_ be _chunk_.[[ByteOffset]].
    let byte_offset = chunk.byte_offset();
    // Step 5: Let _byteLength_ be _chunk_.[[ByteLength]].
    let byte_length = chunk.byte_length();
    // Step 6: If ! `IsDetachedBuffer`(_buffer_) is true, throw a ``TypeError`` exception.
    if buffer.is_detached() {
        return Err(crate::algorithms::throw_invalid_state(
            scope,
            "ReadableByteStreamController",
            c"enqueue() view's buffer is detached",
        ));
    }
    // Step 7: Let _transferredBuffer_ be ? `TransferArrayBuffer`(_buffer_).
    let transferred_buffer = buffer.transfer(scope)?;
    // Step 8: If _controller_.`[[pendingPullIntos]]` is not `empty`, Let _firstPendingPullInto_ be
    //         _controller_.`[[pendingPullIntos]]`[0]. If !
    //         `IsDetachedBuffer`(_firstPendingPullInto_’s `buffer`) is true, throw a
    //         ``TypeError`` exception. Perform !
    //         `ByteStreamControllerInvalidateBYOBRequest`(_controller_). Set
    //         _firstPendingPullInto_’s `buffer` to !
    //         `TransferArrayBuffer`(_firstPendingPullInto_’s `buffer`). If
    //         _firstPendingPullInto_’s `reader type` is "`none`", perform ?
    //         `ByteStreamControllerEnqueueDetachedPullIntoToQueue`(_controller_,
    //         _firstPendingPullInto_).
    if !controller.data().pending_pull_intos.is_empty() {
        let first_buffer = controller.data().pending_pull_intos[0].buffer.get(scope);
        if first_buffer.is_detached() {
            return Err(js::error::throw_type_error(
                scope,
                c"the pending pull-into's buffer is detached",
            ));
        }
        readable_byte_stream_controller_invalidate_byob_request(scope, controller);
        let transferred = first_buffer.transfer(scope)?;
        controller.data_mut().pending_pull_intos[0]
            .buffer
            .set(transferred);
        if controller.data().pending_pull_intos[0].reader_type == ReaderType::None {
            readable_byte_stream_controller_enqueue_detached_pull_into_to_queue(scope, controller)?;
        }
    }
    // Step 9: If ! `ReadableStreamHasDefaultReader`(_stream_) is true, Perform !
    //         `ByteStreamControllerProcessReadRequestsUsingQueue`(_controller_). If !
    //         `ReadableStreamGetNumReadRequests`(_stream_) is 0, Assert:
    //         _controller_.`[[pendingPullIntos]]` is `empty`. Perform !
    //         `ByteStreamControllerEnqueueChunkToQueue`(_controller_, _transferredBuffer_,
    //         _byteOffset_, _byteLength_). Otherwise, Assert: _controller_.`[[queue]]` `is empty`.
    //         If _controller_.`[[pendingPullIntos]]` is not `empty`, Assert:
    //         _controller_.`[[pendingPullIntos]]`[0]'s `reader type` is "`default`". Perform !
    //         `ByteStreamControllerShiftPendingPullInto`(_controller_). Let
    //         _transferredView_ be ! `Construct`(``%Uint8Array%``, « _transferredBuffer_,
    //         _byteOffset_, _byteLength_ »). Perform !
    //         `ReadableStreamFulfillReadRequest`(_stream_, _transferredView_, false).
    if readable_stream_has_default_reader(scope, &stream) {
        readable_byte_stream_controller_process_read_requests_using_queue(scope, controller)?;
        if readable_stream_get_num_read_requests(scope, &stream) == 0 {
            debug_assert!(controller.data().pending_pull_intos.is_empty());
            readable_byte_stream_controller_enqueue_chunk_to_queue(
                controller,
                transferred_buffer,
                byte_offset,
                byte_length,
            );
        } else {
            debug_assert!(controller.data().queue.is_empty());
            // A pending pull-into here can only come from auto-allocation, whose reader type is
            // "default"; shift it off before delivering the chunk to the waiting read request.
            if !controller.data().pending_pull_intos.is_empty() {
                debug_assert_eq!(
                    controller.data().pending_pull_intos[0].reader_type,
                    ReaderType::Default
                );
                readable_byte_stream_controller_shift_pending_pull_into(controller);
            }
            let transferred_view =
                js::Uint8Array::with_buffer(scope, transferred_buffer, byte_offset, byte_length)?;
            let view_value = scope.root_value(transferred_view.as_value());
            readable_stream_fulfill_read_request(scope, &stream, view_value, false);
        }
    } else if readable_stream_has_byob_reader(scope, &stream) {
        // Step 10: Otherwise, if ! `ReadableStreamHasBYOBReader`(_stream_) is true, Perform !
        //          `ByteStreamControllerEnqueueChunkToQueue`(_controller_, _transferredBuffer_,
        //          _byteOffset_, _byteLength_). Let _filledPullIntos_ be the result of performing !
        //          `ByteStreamControllerProcessPullIntoDescriptorsUsingQueue`(_controller_).
        //          `For each` _filledPullInto_ of _filledPullIntos_, Perform !
        //          `ByteStreamControllerCommitPullIntoDescriptor`(_stream_, _filledPullInto_).
        readable_byte_stream_controller_enqueue_chunk_to_queue(
            controller,
            transferred_buffer,
            byte_offset,
            byte_length,
        );
        let filled_pull_intos =
            readable_byte_stream_controller_process_pull_into_descriptors_using_queue(
                scope, controller,
            );
        for filled_pull_into in &filled_pull_intos {
            readable_byte_stream_controller_commit_pull_into_descriptor(
                scope,
                &stream,
                filled_pull_into,
            )?;
        }
    } else {
        // Step 11: Otherwise, Assert: ! `IsReadableStreamLocked`(_stream_) is false. Perform !
        //          `ByteStreamControllerEnqueueChunkToQueue`(_controller_, _transferredBuffer_,
        //          _byteOffset_, _byteLength_).
        debug_assert!(!stream.is_locked());
        readable_byte_stream_controller_enqueue_chunk_to_queue(
            controller,
            transferred_buffer,
            byte_offset,
            byte_length,
        );
    }
    // Step 12: Perform ! `ByteStreamControllerCallPullIfNeeded`(_controller_).
    readable_byte_stream_controller_call_pull_if_needed(scope, controller);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-enqueue-chunk-to-queue>
/// ByteStreamControllerEnqueueChunkToQueue(controller, buffer, byteOffset, byteLength) performs the following steps:
pub(crate) fn readable_byte_stream_controller_enqueue_chunk_to_queue(
    controller: &ReadableByteStreamController<'_>,
    buffer: js::ArrayBuffer<'_>,
    byte_offset: usize,
    byte_length: usize,
) {
    // Step 1: `Append` a new `readable byte stream queue entry` with `buffer` _buffer_, `byte
    //         offset` _byteOffset_, and `byte length` _byteLength_ to _controller_.`[[queue]]`.
    controller.data_mut().queue.push_back(ByteQueueEntry {
        buffer: Heap::from(buffer),
        byte_offset,
        byte_length,
    });
    // Step 2: Set _controller_.`[[queueTotalSize]]` to _controller_.`[[queueTotalSize]]` +
    //         _byteLength_.
    controller.data_mut().queue_total_size += byte_length as f64;
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-ByteStreamControllerenqueueclonedchunktoqueue>
/// ByteStreamControllerEnqueueClonedChunkToQueue(controller, buffer, byteOffset, byteLength) performs the following steps:
fn readable_byte_stream_controller_enqueue_cloned_chunk_to_queue(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    buffer: js::ArrayBuffer<'_>,
    byte_offset: usize,
    byte_length: usize,
) -> Result<(), ExnThrown> {
    // Step 1: Let _cloneResult_ be `CloneArrayBuffer`(_buffer_, _byteOffset_, _byteLength_,
    //         ``%ArrayBuffer%``).
    let clone_result = buffer.clone_region(scope, byte_offset, byte_length);
    // Step 2: If _cloneResult_ is an abrupt completion, Perform !
    //         `ByteStreamControllerError`(_controller_, _cloneResult_.[[Value]]). Return
    //         _cloneResult_.
    let cloned = match clone_result {
        Ok(b) => b,
        Err(_) => {
            let error = take_pending_or_undefined(scope);
            readable_byte_stream_controller_error(scope, controller, error);
            return Err(js::exception::set_pending(
                scope,
                error,
                js::native::ExceptionStackBehavior::DoNotCapture,
            ));
        }
    };
    // Step 3: Perform ! `ByteStreamControllerEnqueueChunkToQueue`(_controller_,
    //         _cloneResult_.[[Value]], 0, _byteLength_).
    readable_byte_stream_controller_enqueue_chunk_to_queue(controller, cloned, 0, byte_length);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-ByteStreamControllerenqueuedetachedpullintotoqueue>
/// ByteStreamControllerEnqueueDetachedPullIntoToQueue(controller, pullIntoDescriptor) performs the following steps:
/// `pull_into_descriptor` is the head of `[[pendingPullIntos]]` at every call
/// site, so it is read from there directly rather than passed in.
fn readable_byte_stream_controller_enqueue_detached_pull_into_to_queue(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) -> Result<(), ExnThrown> {
    let (reader_type, byte_offset, bytes_filled, buffer) = {
        let data = controller.data();
        let head = &data.pending_pull_intos[0];
        (
            head.reader_type,
            head.byte_offset,
            head.bytes_filled,
            head.buffer.get(scope),
        )
    };
    // Step 1: Assert: _pullIntoDescriptor_’s `reader type` is "`none`".
    debug_assert_eq!(reader_type, ReaderType::None);
    // Step 2: If _pullIntoDescriptor_’s `bytes filled` > 0, perform ?
    //         `ByteStreamControllerEnqueueClonedChunkToQueue`(_controller_,
    //         _pullIntoDescriptor_’s `buffer`, _pullIntoDescriptor_’s `byte offset`,
    //         _pullIntoDescriptor_’s `bytes filled`).
    if bytes_filled > 0 {
        readable_byte_stream_controller_enqueue_cloned_chunk_to_queue(
            scope,
            controller,
            buffer,
            byte_offset,
            bytes_filled,
        )?;
    }
    // Step 3: Perform ! `ByteStreamControllerShiftPendingPullInto`(_controller_).
    readable_byte_stream_controller_shift_pending_pull_into(controller);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-error>
/// ByteStreamControllerError(controller, e) performs the following steps:
pub(crate) fn readable_byte_stream_controller_error(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    e: HandleValue<'_>,
) {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: If _stream_.`[[state]]` is not "`readable`", return.
    if stream.data().state != ReadableStreamState::Readable {
        return;
    }
    // Step 3: Perform ! `ByteStreamControllerClearPendingPullIntos`(_controller_).
    readable_byte_stream_controller_clear_pending_pull_intos(scope, controller);
    // Step 4: Perform ! `ResetQueue`(_controller_).
    //         The byte controller's `[[queue]]` holds byte-stream queue entries, so the §8.1
    //         ResetQueue is applied directly: empty the queue and zero its total size.
    reset_byte_queue(controller);
    // Step 5: Perform ! `ByteStreamControllerClearAlgorithms`(_controller_).
    readable_byte_stream_controller_clear_algorithms(controller);
    // Step 6: Perform ! `ReadableStreamError`(_stream_, _e_).
    readable_stream_error(scope, &stream, e);
}

/// `ResetQueue` (<https://streams.spec.whatwg.org/#reset-queue>) specialised for
/// the byte controller, whose `[[queue]]` is a list of byte-stream queue
/// entries rather than value-with-size records.
fn reset_byte_queue(controller: &ReadableByteStreamController<'_>) {
    let mut data = controller.data_mut();
    data.queue.clear();
    data.queue_total_size = 0.0;
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-fill-head-pull-into-descriptor>
/// ByteStreamControllerFillHeadPullIntoDescriptor(controller, size, pullIntoDescriptor) performs the following steps:
fn readable_byte_stream_controller_fill_head_pull_into_descriptor(
    controller: &ReadableByteStreamController<'_>,
    size: usize,
    pull_into_descriptor: &mut PullIntoDescriptor,
) {
    // Step 1: Assert: either _controller_.`[[pendingPullIntos]]` `is empty`, or
    //         _controller_.`[[pendingPullIntos]]`[0] is _pullIntoDescriptor_.
    //         Every caller pops the head descriptor into a stack local before filling it, so its
    //         identity against `[[pendingPullIntos]][0]` is no longer observable here, and the deque
    //         legitimately still holds the descriptors queued behind it. The invariant — that
    //         fill_head only ever runs on the current head descriptor — holds by construction at the
    //         call sites, so there is nothing left to assert structurally.
    // Step 2: Assert: _controller_.`[[ReadableStreamBYOBRequest]]` is null.
    debug_assert!(controller.data().byob_request.is_none());
    // Step 3: Set _pullIntoDescriptor_’s `bytes filled` to `bytes filled` + _size_.
    pull_into_descriptor.bytes_filled += size;
}

/// Copy `count` bytes from `src[src_start..]` to `dest[dest_start..]`, where both
/// are `ArrayBuffer` backing stores. The streams spec's `CopyDataBlockBytes`.
fn copy_data_block_bytes(
    dest: js::ArrayBuffer<'_>,
    dest_start: usize,
    src: js::ArrayBuffer<'_>,
    src_start: usize,
    count: usize,
) {
    if count == 0 {
        return;
    }
    // SAFETY: `dest` and `src` are distinct live, non-detached `ArrayBuffer`s
    // (the descriptor's buffer and a queue entry's buffer); no GC runs during
    // the memcpy, and the ranges are bounds-checked by the slice indexing.
    unsafe {
        let d = dest.bytes_mut();
        let s = src.bytes();
        d[dest_start..dest_start + count].copy_from_slice(&s[src_start..src_start + count]);
    }
}

/// The descriptor's `buffer` slot as a rooted `ArrayBuffer`.
fn descriptor_buffer<'r>(
    scope: &'r Scope<'_>,
    descriptor: &PullIntoDescriptor,
) -> js::ArrayBuffer<'r> {
    descriptor.buffer.get(scope)
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-fill-pull-into-descriptor-from-queue>
/// ByteStreamControllerFillPullIntoDescriptorFromQueue(controller, pullIntoDescriptor) performs the following steps:
fn readable_byte_stream_controller_fill_pull_into_descriptor_from_queue(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    pull_into_descriptor: &mut PullIntoDescriptor,
) -> bool {
    // Step 1: Let _maxBytesToCopy_ be min(_controller_.`[[queueTotalSize]]`,
    //         _pullIntoDescriptor_’s `byte length` − _pullIntoDescriptor_’s `bytes filled`).
    let max_bytes_to_copy = (controller.data().queue_total_size as usize)
        .min(pull_into_descriptor.byte_length - pull_into_descriptor.bytes_filled);
    // Step 2: Let _maxBytesFilled_ be _pullIntoDescriptor_’s `bytes filled` + _maxBytesToCopy_.
    let max_bytes_filled = pull_into_descriptor.bytes_filled + max_bytes_to_copy;
    // Step 3: Let _totalBytesToCopyRemaining_ be _maxBytesToCopy_.
    let mut total_bytes_to_copy_remaining = max_bytes_to_copy;
    // Step 4: Let _ready_ be false.
    let mut ready = false;
    // Step 5: Assert: ! `IsDetachedBuffer`(_pullIntoDescriptor_’s `buffer`) is false.
    debug_assert!(!descriptor_buffer(scope, pull_into_descriptor).is_detached());
    // Step 6: Assert: _pullIntoDescriptor_’s `bytes filled` < _pullIntoDescriptor_’s `minimum
    //         fill`.
    debug_assert!(pull_into_descriptor.bytes_filled < pull_into_descriptor.minimum_fill);
    // Step 7: Let _remainderBytes_ be the remainder after dividing _maxBytesFilled_ by
    //         _pullIntoDescriptor_’s `element size`.
    let remainder_bytes = max_bytes_filled % pull_into_descriptor.element_size;
    // Step 8: Let _maxAlignedBytes_ be _maxBytesFilled_ − _remainderBytes_.
    let max_aligned_bytes = max_bytes_filled - remainder_bytes;
    // Step 9: If _maxAlignedBytes_ ≥ _pullIntoDescriptor_’s `minimum fill`, Set
    //         _totalBytesToCopyRemaining_ to _maxAlignedBytes_ − _pullIntoDescriptor_’s `bytes
    //         filled`. Set _ready_ to true. A descriptor for a ``read()`` request that is not yet
    //         filled up to its minimum length will stay at the head of the queue, so the
    //         `underlying source` can keep filling it.
    if max_aligned_bytes >= pull_into_descriptor.minimum_fill {
        total_bytes_to_copy_remaining = max_aligned_bytes - pull_into_descriptor.bytes_filled;
        ready = true;
    }
    // Step 10: Let _queue_ be _controller_.`[[queue]]`.
    //          (Accessed via `controller.data_mut().queue` below.)
    // Step 11: `While` _totalBytesToCopyRemaining_ > 0, Let _headOfQueue_ be _queue_[0]. Let
    //          _bytesToCopy_ be min(_totalBytesToCopyRemaining_, _headOfQueue_’s `byte length`).
    //          Let _destStart_ be _pullIntoDescriptor_’s `byte offset` + _pullIntoDescriptor_’s
    //          `bytes filled`. Let _descriptorBuffer_ be _pullIntoDescriptor_’s `buffer`. Let
    //          _queueBuffer_ be _headOfQueue_’s `buffer`. Let _queueByteOffset_ be
    //          _headOfQueue_’s `byte offset`. Assert: !
    //          `CanCopyDataBlockBytes`(_descriptorBuffer_, _destStart_, _queueBuffer_,
    //          _queueByteOffset_, _bytesToCopy_) is true. If this assertion were to fail (due to a
    //          bug in this specification or its implementation), then the next step may read from
    //          or write to potentially invalid memory. The user agent should always check this
    //          assertion, and stop in an `implementation-defined` manner if it fails (e.g. by
    //          crashing the process, or by `erroring the stream`). Perform !
    //          `CopyDataBlockBytes`(_descriptorBuffer_.[[ArrayBufferData]], _destStart_,
    //          _queueBuffer_.[[ArrayBufferData]], _queueByteOffset_, _bytesToCopy_). If
    //          _headOfQueue_’s `byte length` is _bytesToCopy_, `Remove` _queue_[0]. Otherwise,
    //          Set _headOfQueue_’s `byte offset` to _headOfQueue_’s `byte offset` +
    //          _bytesToCopy_. Set _headOfQueue_’s `byte length` to _headOfQueue_’s `byte
    //          length` − _bytesToCopy_. Set _controller_.`[[queueTotalSize]]` to
    //          _controller_.`[[queueTotalSize]]` − _bytesToCopy_. Perform !
    //          `ByteStreamControllerFillHeadPullIntoDescriptor`(_controller_,
    //          _bytesToCopy_, _pullIntoDescriptor_). Set _totalBytesToCopyRemaining_ to
    //          _totalBytesToCopyRemaining_ − _bytesToCopy_.
    let descriptor_buffer = descriptor_buffer(scope, pull_into_descriptor);
    while total_bytes_to_copy_remaining > 0 {
        let (head_byte_offset, head_byte_length, queue_buffer) = {
            let data = controller.data();
            let head = &data.queue[0];
            (head.byte_offset, head.byte_length, head.buffer.get(scope))
        };
        let bytes_to_copy = total_bytes_to_copy_remaining.min(head_byte_length);
        let dest_start = pull_into_descriptor.byte_offset + pull_into_descriptor.bytes_filled;
        copy_data_block_bytes(
            descriptor_buffer,
            dest_start,
            queue_buffer,
            head_byte_offset,
            bytes_to_copy,
        );
        if head_byte_length == bytes_to_copy {
            controller.data_mut().queue.pop_front();
        } else {
            let mut data = controller.data_mut();
            let head = &mut data.queue[0];
            head.byte_offset += bytes_to_copy;
            head.byte_length -= bytes_to_copy;
        }
        controller.data_mut().queue_total_size -= bytes_to_copy as f64;
        readable_byte_stream_controller_fill_head_pull_into_descriptor(
            controller,
            bytes_to_copy,
            pull_into_descriptor,
        );
        total_bytes_to_copy_remaining -= bytes_to_copy;
    }
    // Step 12: If _ready_ is false, Assert: _controller_.`[[queueTotalSize]]` is 0. Assert:
    //          _pullIntoDescriptor_’s `bytes filled` > 0. Assert: _pullIntoDescriptor_’s `bytes
    //          filled` < _pullIntoDescriptor_’s `minimum fill`.
    if !ready {
        debug_assert_eq!(controller.data().queue_total_size, 0.0);
        debug_assert!(pull_into_descriptor.bytes_filled > 0);
        debug_assert!(pull_into_descriptor.bytes_filled < pull_into_descriptor.minimum_fill);
    }
    // Step 13: Return _ready_.
    ready
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-ByteStreamControllerfillreadrequestfromqueue>
/// ByteStreamControllerFillReadRequestFromQueue(controller, readRequest) performs the following steps:
pub(crate) fn readable_byte_stream_controller_fill_read_request_from_queue(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    read_request: &mut RootedTraceableBox<Option<ReadRequest>>,
) -> Result<(), ExnThrown> {
    // Step 1: Assert: _controller_.`[[queueTotalSize]]` > 0.
    debug_assert!(controller.data().queue_total_size > 0.0);
    // Step 2: Let _entry_ be _controller_.`[[queue]]`[0].
    // Step 3: `Remove` _entry_ from _controller_.`[[queue]]`.
    // Consume the head entry: `into_parts` roots its buffer to the scope (and
    // drops the now-untraced `Heap` on its still-live pointer) before
    // `HandleQueueDrain` below can compact, so no stale pointer is left behind.
    let (buffer, byte_offset, byte_length) = controller
        .data_mut()
        .queue
        .pop_front()
        .expect("byte queue is not empty")
        .root(scope)
        .into_parts();
    // Step 4: Set _controller_.`[[queueTotalSize]]` to _controller_.`[[queueTotalSize]]` −
    //         _entry_’s `byte length`.
    controller.data_mut().queue_total_size -= byte_length as f64;
    // Step 5: Perform ! `ByteStreamControllerHandleQueueDrain`(_controller_).
    readable_byte_stream_controller_handle_queue_drain(scope, controller);
    // Step 6: Let _view_ be ! `Construct`(``%Uint8Array%``, « _entry_’s `buffer`, _entry_’s
    //         `byte offset`, _entry_’s `byte length` »).
    let view = js::Uint8Array::with_buffer(scope, buffer, byte_offset, byte_length)?;
    // Step 7: Perform _readRequest_’s `chunk steps`, given _view_.
    let view_value = scope.root_value(view.as_value());
    read_request
        .take()
        .unwrap()
        .root(scope)
        .chunk_steps(scope, view_value);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-ByteStreamControllergetbyobrequest>
/// ByteStreamControllerGetBYOBRequest(controller) performs the following steps:
pub(crate) fn readable_byte_stream_controller_get_byob_request<'r>(
    scope: &'r Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) -> Option<ReadableStreamBYOBRequest<'r>> {
    // Step 1: If _controller_.`[[ReadableStreamBYOBRequest]]` is null and _controller_.`[[pendingPullIntos]]` is
    //         not `empty`, Let _firstDescriptor_ be _controller_.`[[pendingPullIntos]]`[0]. Let
    //         _view_ be ! `Construct`(``%Uint8Array%``, « _firstDescriptor_’s `buffer`,
    //         _firstDescriptor_’s `byte offset` + _firstDescriptor_’s `bytes filled`,
    //         _firstDescriptor_’s `byte length` − _firstDescriptor_’s `bytes filled` »). Let
    //         _byobRequest_ be a `new` ``ReadableStreamBYOBRequest``. Set
    //         _byobRequest_.`[[controller]]` to _controller_. Set _byobRequest_.`[[view]]` to
    //         _view_. Set _controller_.`[[ReadableStreamBYOBRequest]]` to _byobRequest_.
    if controller.data().byob_request.is_none() && !controller.data().pending_pull_intos.is_empty()
    {
        let (buffer, byte_offset, bytes_filled, byte_length) = {
            let data = controller.data();
            let first = &data.pending_pull_intos[0];
            (
                first.buffer.get(scope),
                first.byte_offset,
                first.bytes_filled,
                first.byte_length,
            )
        };
        let view = js::Uint8Array::with_buffer(
            scope,
            buffer,
            byte_offset + bytes_filled,
            byte_length - bytes_filled,
        )
        .expect("constructing a Uint8Array over the pending pull-into");
        let byob_request =
            ReadableStreamBYOBRequest::new(scope).expect("creating a ReadableStreamBYOBRequest");
        byob_request.data_mut().controller.set(*controller);
        byob_request
            .data_mut()
            .view
            .set(view.as_array_buffer_view());
        controller.data_mut().byob_request.set(byob_request);
    }
    // Step 2: Return _controller_.`[[ReadableStreamBYOBRequest]]`.
    controller.data().byob_request.get(scope)
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-get-desired-size>
/// ByteStreamControllerGetDesiredSize(controller) performs the following steps:
pub(crate) fn readable_byte_stream_controller_get_desired_size(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) -> Option<f64> {
    // Step 1: Let _state_ be _controller_.`[[stream]]`.`[[state]]`.
    let state = controller.stream(scope).data().state;
    match state {
        // Step 2: If _state_ is "`errored`", return null.
        ReadableStreamState::Errored => None,
        // Step 3: If _state_ is "`closed`", return 0.
        ReadableStreamState::Closed => Some(0.0),
        // Step 4: Return _controller_.`[[strategyHWM]]` − _controller_.`[[queueTotalSize]]`.
        ReadableStreamState::Readable => {
            Some(controller.data().strategy_hwm - controller.data().queue_total_size)
        }
    }
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-handle-queue-drain>
/// ByteStreamControllerHandleQueueDrain(controller) performs the following steps:
pub(crate) fn readable_byte_stream_controller_handle_queue_drain(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) {
    // Step 1: Assert: _controller_.`[[stream]]`.`[[state]]` is "`readable`".
    debug_assert_eq!(
        controller.stream(scope).data().state,
        ReadableStreamState::Readable
    );
    // Step 2: If _controller_.`[[queueTotalSize]]` is 0 and _controller_.`[[closeRequested]]` is
    //         true, Perform ! `ByteStreamControllerClearAlgorithms`(_controller_). Perform
    //         ! `ReadableStreamClose`(_controller_.`[[stream]]`).
    if controller.data().queue_total_size == 0.0 && controller.data().close_requested {
        readable_byte_stream_controller_clear_algorithms(controller);
        let stream = controller.stream(scope);
        readable_stream_close(scope, &stream);
    } else {
        // Step 3: Otherwise, Perform ! `ByteStreamControllerCallPullIfNeeded`(_controller_).
        readable_byte_stream_controller_call_pull_if_needed(scope, controller);
    }
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-invalidate-byob-request>
/// ByteStreamControllerInvalidateBYOBRequest(controller) performs the following steps:
pub(crate) fn readable_byte_stream_controller_invalidate_byob_request(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) {
    // Step 1: If _controller_.`[[ReadableStreamBYOBRequest]]` is null, return.
    let byob_request = match controller.data().byob_request.get(scope) {
        Some(req) => req,
        None => return,
    };
    // Step 2: Set _controller_.`[[ReadableStreamBYOBRequest]]`.`[[controller]]` to undefined.
    byob_request.data_mut().controller = None;
    // Step 3: Set _controller_.`[[ReadableStreamBYOBRequest]]`.`[[view]]` to null.
    byob_request.data_mut().view = None;
    // Step 4: Set _controller_.`[[ReadableStreamBYOBRequest]]` to null.
    controller.data_mut().byob_request = None;
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-process-pull-into-descriptors-using-queue>
/// ByteStreamControllerProcessPullIntoDescriptorsUsingQueue(controller) performs the following steps:
fn readable_byte_stream_controller_process_pull_into_descriptors_using_queue(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) -> Vec<RootedTraceableBox<PullIntoDescriptor>> {
    // Step 1: Assert: _controller_.`[[closeRequested]]` is false.
    debug_assert!(!controller.data().close_requested);
    // Step 2: Let _filledPullIntos_ be a new empty `list`.
    let mut filled_pull_intos = Vec::new();
    // Step 3: `While` _controller_.`[[pendingPullIntos]]` is not `empty`, If
    //         _controller_.`[[queueTotalSize]]` is 0, then `break`. Let _pullIntoDescriptor_ be
    //         _controller_.`[[pendingPullIntos]]`[0]. If !
    //         `ByteStreamControllerFillPullIntoDescriptorFromQueue`(_controller_,
    //         _pullIntoDescriptor_) is true, Perform !
    //         `ByteStreamControllerShiftPendingPullInto`(_controller_). `Append`
    //         _pullIntoDescriptor_ to _filledPullIntos_.
    //         The head descriptor is temporarily taken out of the deque so it can be mutated in
    //         place: if it fills, it stays out (the spec's ShiftPendingPullInto); otherwise it is
    //         returned to the head (partially filled), and since the queue is then drained the loop
    //         terminates on the next `queueTotalSize is 0` check.
    while !controller.data().pending_pull_intos.is_empty() {
        if controller.data().queue_total_size == 0.0 {
            break;
        }
        // Take the head descriptor out of the deque into a `RootedTraceableBox`
        // so it is never an untraced `#[must_root]` local; `take()` hands it back
        // when it is appended to the filled list or returned to the deque.
        let mut pull_into_descriptor = RootedTraceableBox::new(Some(
            controller
                .data_mut()
                .pending_pull_intos
                .pop_front()
                .expect("pendingPullIntos is not empty"),
        ));
        if readable_byte_stream_controller_fill_pull_into_descriptor_from_queue(
            scope,
            controller,
            pull_into_descriptor.as_mut().unwrap(),
        ) {
            // The filled descriptor is no longer in the (traced) deque but is
            // committed later by callers across GC-triggering steps, so it stays
            // rooted in a `RootedTraceableBox`.
            filled_pull_intos.push(RootedTraceableBox::new(
                pull_into_descriptor.take().unwrap(),
            ));
        } else {
            controller
                .data_mut()
                .pending_pull_intos
                .push_front(pull_into_descriptor.take().unwrap());
        }
    }
    // Step 4: Return _filledPullIntos_.
    filled_pull_intos
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-ByteStreamControllerprocessreadrequestsusingqueue>
/// ByteStreamControllerProcessReadRequestsUsingQueue(controller) performs the following steps:
pub(crate) fn readable_byte_stream_controller_process_read_requests_using_queue(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Let _reader_ be _controller_.`[[stream]]`.`[[reader]]`.
    let stream = controller.stream(scope);
    // Step 2: Assert: _reader_ `implements` ``DefaultReader``.
    let reader = stream_default_reader(scope, &stream).expect("reader is a default reader");
    // Step 3: While _reader_.`[[readRequests]]` is not `empty`, If
    //         _controller_.`[[queueTotalSize]]` is 0, return. Let _readRequest_ be
    //         _reader_.`[[readRequests]]`[0]. `Remove` _readRequest_ from
    //         _reader_.`[[readRequests]]`. Perform !
    //         `ByteStreamControllerFillReadRequestFromQueue`(_controller_, _readRequest_).
    while !reader.data().read_requests.is_empty() {
        if controller.data().queue_total_size == 0.0 {
            return Ok(());
        }
        let mut read_request = RootedTraceableBox::new(reader.data_mut().read_requests.pop_front());
        readable_byte_stream_controller_fill_read_request_from_queue(
            scope,
            controller,
            &mut read_request,
        )?;
    }
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-pull-into>
/// ByteStreamControllerPullInto(controller, view, min, readIntoRequest) performs the following steps:
pub(crate) fn readable_byte_stream_controller_pull_into(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    view: js::ArrayBufferView<'_>,
    min: usize,
    read_into_request: &mut RootedTraceableBox<Option<ReadIntoRequest>>,
) {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: Let _elementSize_ be 1.
    // Step 3: Let _ctor_ be ``%DataView%``.
    // Step 4: If _view_ has a [[TypedArrayName]] internal slot (i.e., it is not a ``DataView``),
    //         Set _elementSize_ to the element size specified in `the typed array constructors
    //         table` for _view_.[[TypedArrayName]]. Set _ctor_ to the constructor specified in `the
    //         typed array constructors table` for _view_.[[TypedArrayName]].
    //         The view's kind yields both its element size and its constructor; a DataView reports
    //         kind `DataView` with element size 1, covering steps 2-4 uniformly.
    let view_kind = view.view_kind();
    let element_size = view_kind.element_size();
    // Step 5: Let _minimumFill_ be _min_ × _elementSize_.
    let minimum_fill = min * element_size;
    // Step 6: Assert: _minimumFill_ ≥ 0 and _minimumFill_ ≤ _view_.[[ByteLength]].
    debug_assert!(minimum_fill <= view.byte_length());
    // Step 7: Assert: the remainder after dividing _minimumFill_ by _elementSize_ is 0.
    debug_assert_eq!(minimum_fill % element_size, 0);
    // Step 8: Let _byteOffset_ be _view_.[[ByteOffset]].
    let byte_offset = view.byte_offset();
    // Step 9: Let _byteLength_ be _view_.[[ByteLength]].
    let byte_length = view.byte_length();
    // Step 10: Let _bufferResult_ be `TransferArrayBuffer`(_view_.[[ViewedArrayBuffer]]).
    let viewed_buffer = match view.viewed_buffer(scope) {
        Ok(b) => b,
        Err(_) => {
            let error = take_pending_or_undefined(scope);
            read_into_request
                .take()
                .unwrap()
                .root(scope)
                .error_steps(scope, error);
            return;
        }
    };
    let buffer_result = viewed_buffer.transfer(scope);
    // Step 11: If _bufferResult_ is an abrupt completion, Perform _readIntoRequest_’s `error
    //          steps`, given _bufferResult_.[[Value]]. Return.
    let buffer = match buffer_result {
        Ok(b) => b,
        Err(_) => {
            let error = take_pending_or_undefined(scope);
            read_into_request
                .take()
                .unwrap()
                .root(scope)
                .error_steps(scope, error);
            return;
        }
    };
    // Step 12: Let _buffer_ be _bufferResult_.[[Value]].
    let buffer_byte_length = buffer.byte_length();
    // Step 13: Let _pullIntoDescriptor_ be a new `pull-into descriptor` with `buffer` _buffer_
    //          `buffer byte length` _buffer_.[[ArrayBufferByteLength]] `byte offset` _byteOffset_
    //          `byte length` _byteLength_ `bytes filled` 0 `minimum fill` _minimumFill_ `element
    //          size` _elementSize_ `view constructor` _ctor_ `reader type` "`byob`"
    // Root the descriptor in a `RootedTraceableBox` from the moment it is built:
    // it is held across `FillPullIntoDescriptorFromQueue`/`ConvertPullIntoDescriptor`
    // and view construction, all of which can compact, and its `buffer` `Heap`
    // must stay current (and never drop stale). `take()` hands it back when a
    // branch appends it to the (traced) `[[pendingPullIntos]]`.
    let mut pull_into_descriptor = RootedTraceableBox::new(Some(PullIntoDescriptor {
        buffer: Heap::from(buffer),
        buffer_byte_length,
        byte_offset,
        byte_length,
        bytes_filled: 0,
        minimum_fill,
        element_size,
        view_kind,
        reader_type: ReaderType::Byob,
    }));
    // Step 14: If _controller_.`[[pendingPullIntos]]` is not empty, `Append` _pullIntoDescriptor_
    //          to _controller_.`[[pendingPullIntos]]`. Perform !
    //          `ReadableStreamAddReadIntoRequest`(_stream_, _readIntoRequest_). Return.
    if !controller.data().pending_pull_intos.is_empty() {
        controller
            .data_mut()
            .pending_pull_intos
            .push_back(pull_into_descriptor.take().unwrap());
        readable_stream_add_read_into_request(scope, &stream, read_into_request.take().unwrap());
        return;
    }
    // Step 15: If _stream_.`[[state]]` is "`closed`", Let _emptyView_ be ! `Construct`(_ctor_, «
    //          _pullIntoDescriptor_’s `buffer`, _pullIntoDescriptor_’s `byte offset`, 0 »).
    //          Perform _readIntoRequest_’s `close steps`, given _emptyView_. Return.
    if stream.data().state == ReadableStreamState::Closed {
        // The descriptor stays rooted in the box while the empty view is
        // constructed and the close steps run (both can GC).
        let buffer = descriptor_buffer(scope, pull_into_descriptor.as_ref().unwrap());
        let byte_offset = pull_into_descriptor.as_ref().unwrap().byte_offset;
        let empty_view = construct_view_or_throw(scope, view_kind, buffer, byte_offset, 0);
        read_into_request
            .take()
            .unwrap()
            .root(scope)
            .close_steps(scope, empty_view);
        return;
    }
    // Step 16: If _controller_.`[[queueTotalSize]]` > 0, If !
    //          `ByteStreamControllerFillPullIntoDescriptorFromQueue`(_controller_,
    //          _pullIntoDescriptor_) is true, Let _filledView_ be !
    //          `ByteStreamControllerConvertPullIntoDescriptor`(_pullIntoDescriptor_).
    //          Perform ! `ByteStreamControllerHandleQueueDrain`(_controller_). Perform
    //          _readIntoRequest_’s `chunk steps`, given _filledView_. Return. If
    //          _controller_.`[[closeRequested]]` is true, Let _e_ be a ``TypeError`` exception.
    //          Perform ! `ByteStreamControllerError`(_controller_, _e_). Perform
    //          _readIntoRequest_’s `error steps`, given _e_. Return.
    if controller.data().queue_total_size > 0.0 {
        if readable_byte_stream_controller_fill_pull_into_descriptor_from_queue(
            scope,
            controller,
            pull_into_descriptor.as_mut().unwrap(),
        ) {
            let filled_view = readable_byte_stream_controller_convert_pull_into_descriptor(
                scope,
                pull_into_descriptor.as_ref().unwrap(),
            )
            .expect("convert pull-into descriptor");
            let filled_view = scope.root_value(filled_view.as_value());
            readable_byte_stream_controller_handle_queue_drain(scope, controller);
            read_into_request
                .take()
                .unwrap()
                .root(scope)
                .chunk_steps(scope, filled_view);
            return;
        }
        if controller.data().close_requested {
            let e = invalid_state_error(
                scope,
                "ReadableStream",
                c"Insufficient bytes to fill the requested view",
            );
            readable_byte_stream_controller_error(scope, controller, e);
            read_into_request
                .take()
                .unwrap()
                .root(scope)
                .error_steps(scope, e);
            return;
        }
    }
    // Step 17: `Append` _pullIntoDescriptor_ to _controller_.`[[pendingPullIntos]]`.
    controller
        .data_mut()
        .pending_pull_intos
        .push_back(pull_into_descriptor.take().unwrap());
    // Step 18: Perform ! `ReadableStreamAddReadIntoRequest`(_stream_, _readIntoRequest_).
    readable_stream_add_read_into_request(scope, &stream, read_into_request.take().unwrap());
    // Step 19: Perform ! `ByteStreamControllerCallPullIfNeeded`(_controller_).
    readable_byte_stream_controller_call_pull_if_needed(scope, controller);
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-respond>
/// ByteStreamControllerRespond(controller, bytesWritten) performs the following steps:
pub(crate) fn readable_byte_stream_controller_respond(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    bytes_written: u64,
) -> Result<(), ExnThrown> {
    // Step 1: Assert: _controller_.`[[pendingPullIntos]]` is not empty.
    // Step 2: Let _firstDescriptor_ be _controller_.`[[pendingPullIntos]]`[0].
    // Step 3: Let _state_ be _controller_.`[[stream]]`.`[[state]]`.
    // Step 4: If _state_ is "`closed`", If _bytesWritten_ is not 0, throw a ``TypeError``
    //         exception.
    // Step 5: Otherwise, Assert: _state_ is "`readable`". If _bytesWritten_ is 0, throw a
    //         ``TypeError`` exception. If _firstDescriptor_’s `bytes filled` + _bytesWritten_ >
    //         _firstDescriptor_’s `byte length`, throw a ``RangeError`` exception.
    // Step 6: Set _firstDescriptor_’s `buffer` to ! `TransferArrayBuffer`(_firstDescriptor_’s
    //         `buffer`).
    // Step 7: Perform ? `ByteStreamControllerRespondInternal`(_controller_,
    //         _bytesWritten_).
    debug_assert!(!controller.data().pending_pull_intos.is_empty());
    let state = controller.stream(scope).data().state;
    let (bytes_filled, byte_length) = {
        let data = controller.data();
        let first = &data.pending_pull_intos[0];
        (first.bytes_filled, first.byte_length)
    };
    if state == ReadableStreamState::Closed {
        if bytes_written != 0 {
            return Err(js::error::throw_type_error(
                scope,
                c"bytesWritten must be 0 for a closed stream",
            ));
        }
    } else {
        debug_assert_eq!(state, ReadableStreamState::Readable);
        if bytes_written == 0 {
            return Err(js::error::throw_type_error(
                scope,
                c"bytesWritten must not be 0 for a readable stream",
            ));
        }
        // `bytesWritten` is a WebIDL `unsigned long long`; compare in u64 so a value
        // beyond `usize::MAX` (reachable on the 32-bit wasm target) still triggers the
        // RangeError instead of truncating into the valid range. `byte_length` ≥
        // `bytes_filled` is an invariant, so the subtraction cannot underflow.
        if bytes_written > (byte_length - bytes_filled) as u64 {
            return Err(js::error::throw_range_error(
                scope,
                c"bytesWritten exceeds the requested view's length",
            ));
        }
    }
    // Validation above bounds `bytes_written` by `byte_length`, so the cast fits `usize`.
    let bytes_written = bytes_written as usize;
    let transferred =
        descriptor_buffer(scope, &controller.data().pending_pull_intos[0]).transfer(scope)?;
    controller.data_mut().pending_pull_intos[0]
        .buffer
        .set(transferred);
    readable_byte_stream_controller_respond_internal(scope, controller, bytes_written)
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-respond-in-closed-state>
/// ByteStreamControllerRespondInClosedState(controller, firstDescriptor) performs the following steps:
pub(crate) fn readable_byte_stream_controller_respond_in_closed_state(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Assert: the remainder after dividing _firstDescriptor_’s `bytes filled` by
    //         _firstDescriptor_’s `element size` is 0.
    // Step 2: If _firstDescriptor_’s `reader type` is "`none`", perform !
    //         `ByteStreamControllerShiftPendingPullInto`(_controller_).
    // Step 3: Let _stream_ be _controller_.`[[stream]]`.
    // Step 4: If ! `ReadableStreamHasBYOBReader`(_stream_) is true, Let _filledPullIntos_ be a new
    //         empty `list`. `While` _filledPullIntos_’s `size` < !
    //         `ReadableStreamGetNumReadIntoRequests`(_stream_), Let _pullIntoDescriptor_ be !
    //         `ByteStreamControllerShiftPendingPullInto`(_controller_). `Append`
    //         _pullIntoDescriptor_ to _filledPullIntos_. `For each` _filledPullInto_ of
    //         _filledPullIntos_, Perform !
    //         `ByteStreamControllerCommitPullIntoDescriptor`(_stream_, _filledPullInto_).
    {
        let data = controller.data();
        let first = &data.pending_pull_intos[0];
        debug_assert_eq!(first.bytes_filled % first.element_size, 0);
    }
    if controller.data().pending_pull_intos[0].reader_type == ReaderType::None {
        readable_byte_stream_controller_shift_pending_pull_into(controller);
    }
    let stream = controller.stream(scope);
    if readable_stream_has_byob_reader(scope, &stream) {
        // The spec collects all the descriptors into `filledPullIntos` and then
        // commits them. A descriptor shifted out of `[[pendingPullIntos]]` is no
        // longer traced by the controller, and `CommitPullIntoDescriptor` runs
        // steps that can GC (`TransferArrayBuffer`, constructing the view). A
        // shifted descriptor must therefore stay rooted for the whole commit —
        // both so its `buffer` survives until it is transferred, and so its
        // `Heap`'s drop write barrier sees a live pointer — so each is held in a
        // `RootedTraceableBox` while committed. Capture the count up front, then
        // shift and commit one descriptor at a time so the remaining descriptors
        // stay traced in `[[pendingPullIntos]]`. This is not observable: the count
        // is only reduced by these commits (which fulfill read-into requests via
        // queued reactions, never synchronous author code), so committing each
        // descriptor as it is shifted is equivalent to shifting all then
        // committing all.
        let num_read_into_requests = readable_stream_get_num_read_into_requests(scope, &stream);
        for _ in 0..num_read_into_requests {
            let descriptor = RootedTraceableBox::new(
                readable_byte_stream_controller_shift_pending_pull_into(controller),
            );
            readable_byte_stream_controller_commit_pull_into_descriptor(
                scope,
                &stream,
                &descriptor,
            )?;
        }
    }
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-respond-in-readable-state>
/// ByteStreamControllerRespondInReadableState(controller, bytesWritten, pullIntoDescriptor) performs the following steps:
pub(crate) fn readable_byte_stream_controller_respond_in_readable_state(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    bytes_written: usize,
) -> Result<(), ExnThrown> {
    // Step 1: Assert: _pullIntoDescriptor_’s `bytes filled` + _bytesWritten_ ≤
    //         _pullIntoDescriptor_’s `byte length`.
    // Step 2: Perform ! `ByteStreamControllerFillHeadPullIntoDescriptor`(_controller_,
    //         _bytesWritten_, _pullIntoDescriptor_).
    // Step 3: If _pullIntoDescriptor_’s `reader type` is "`none`", Perform ?
    //         `ByteStreamControllerEnqueueDetachedPullIntoToQueue`(_controller_,
    //         _pullIntoDescriptor_). Let _filledPullIntos_ be the result of performing !
    //         `ByteStreamControllerProcessPullIntoDescriptorsUsingQueue`(_controller_).
    //         `For each` _filledPullInto_ of _filledPullIntos_, Perform !
    //         `ByteStreamControllerCommitPullIntoDescriptor`(_controller_.`[[stream]]`,
    //         _filledPullInto_). Return.
    // Step 4: If _pullIntoDescriptor_’s `bytes filled` < _pullIntoDescriptor_’s `minimum fill`,
    //         return. A descriptor for a ``read()`` request that is not yet filled up to its
    //         minimum length will stay at the head of the queue, so the `underlying source` can
    //         keep filling it.
    // Step 5: Perform ! `ByteStreamControllerShiftPendingPullInto`(_controller_).
    // Step 6: Let _remainderSize_ be the remainder after dividing _pullIntoDescriptor_’s `bytes
    //         filled` by _pullIntoDescriptor_’s `element size`.
    // Step 7: If _remainderSize_ > 0, Let _end_ be _pullIntoDescriptor_’s `byte offset` +
    //         _pullIntoDescriptor_’s `bytes filled`. Perform ?
    //         `ByteStreamControllerEnqueueClonedChunkToQueue`(_controller_,
    //         _pullIntoDescriptor_’s `buffer`, _end_ − _remainderSize_, _remainderSize_).
    // Step 8: Set _pullIntoDescriptor_’s `bytes filled` to _pullIntoDescriptor_’s `bytes
    //         filled` − _remainderSize_.
    // Step 9: Let _filledPullIntos_ be the result of performing !
    //         `ByteStreamControllerProcessPullIntoDescriptorsUsingQueue`(_controller_).
    // Step 10: Perform !
    //          `ByteStreamControllerCommitPullIntoDescriptor`(_controller_.`[[stream]]`,
    //          _pullIntoDescriptor_).
    // Step 11: `For each` _filledPullInto_ of _filledPullIntos_, Perform !
    //          `ByteStreamControllerCommitPullIntoDescriptor`(_controller_.`[[stream]]`,
    //          _filledPullInto_).
    // Move the head descriptor out of the (traced) deque straight into a
    // `RootedTraceableBox` so it is never an untraced `#[must_root]` local across
    // the allocation-bearing steps below; `take()` hands it back when a branch
    // re-inserts it.
    let mut pull_into_descriptor = RootedTraceableBox::new(Some(
        controller
            .data_mut()
            .pending_pull_intos
            .pop_front()
            .expect("pendingPullIntos is not empty"),
    ));
    debug_assert!(
        pull_into_descriptor.as_ref().unwrap().bytes_filled + bytes_written
            <= pull_into_descriptor.as_ref().unwrap().byte_length
    );
    readable_byte_stream_controller_fill_head_pull_into_descriptor(
        controller,
        bytes_written,
        pull_into_descriptor.as_mut().unwrap(),
    );
    if pull_into_descriptor.as_ref().unwrap().reader_type == ReaderType::None {
        controller
            .data_mut()
            .pending_pull_intos
            .push_front(pull_into_descriptor.take().unwrap());
        readable_byte_stream_controller_enqueue_detached_pull_into_to_queue(scope, controller)?;
        let filled_pull_intos =
            readable_byte_stream_controller_process_pull_into_descriptors_using_queue(
                scope, controller,
            );
        let stream = controller.stream(scope);
        for filled_pull_into in &filled_pull_intos {
            readable_byte_stream_controller_commit_pull_into_descriptor(
                scope,
                &stream,
                filled_pull_into,
            )?;
        }
        return Ok(());
    }
    if pull_into_descriptor.as_ref().unwrap().bytes_filled
        < pull_into_descriptor.as_ref().unwrap().minimum_fill
    {
        controller
            .data_mut()
            .pending_pull_intos
            .push_front(pull_into_descriptor.take().unwrap());
        return Ok(());
    }
    // The descriptor stays in the `RootedTraceableBox` (traced) while it is
    // committed below across GC-triggering steps (`EnqueueClonedChunkToQueue`,
    // `CommitPullIntoDescriptor`), so its `buffer` survives.
    let remainder_size = pull_into_descriptor.as_ref().unwrap().bytes_filled
        % pull_into_descriptor.as_ref().unwrap().element_size;
    if remainder_size > 0 {
        let end = pull_into_descriptor.as_ref().unwrap().byte_offset
            + pull_into_descriptor.as_ref().unwrap().bytes_filled;
        let buffer = descriptor_buffer(scope, pull_into_descriptor.as_ref().unwrap());
        readable_byte_stream_controller_enqueue_cloned_chunk_to_queue(
            scope,
            controller,
            buffer,
            end - remainder_size,
            remainder_size,
        )?;
    }
    pull_into_descriptor.as_mut().unwrap().bytes_filled -= remainder_size;
    let filled_pull_intos =
        readable_byte_stream_controller_process_pull_into_descriptors_using_queue(
            scope, controller,
        );
    let stream = controller.stream(scope);
    readable_byte_stream_controller_commit_pull_into_descriptor(
        scope,
        &stream,
        pull_into_descriptor.as_ref().unwrap(),
    )?;
    for filled_pull_into in &filled_pull_intos {
        readable_byte_stream_controller_commit_pull_into_descriptor(
            scope,
            &stream,
            filled_pull_into,
        )?;
    }
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-respond-internal>
/// ByteStreamControllerRespondInternal(controller, bytesWritten) performs the following steps:
pub(crate) fn readable_byte_stream_controller_respond_internal(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    bytes_written: usize,
) -> Result<(), ExnThrown> {
    // Step 1: Let _firstDescriptor_ be _controller_.`[[pendingPullIntos]]`[0].
    // Step 2: Assert: ! `CanTransferArrayBuffer`(_firstDescriptor_’s `buffer`) is true.
    // Step 3: Perform ! `ByteStreamControllerInvalidateBYOBRequest`(_controller_).
    // Step 4: Let _state_ be _controller_.`[[stream]]`.`[[state]]`.
    // Step 5: If _state_ is "`closed`", Assert: _bytesWritten_ is 0. Perform !
    //         `ByteStreamControllerRespondInClosedState`(_controller_, _firstDescriptor_).
    // Step 6: Otherwise, Assert: _state_ is "`readable`". Assert: _bytesWritten_ > 0. Perform ?
    //         `ByteStreamControllerRespondInReadableState`(_controller_, _bytesWritten_,
    //         _firstDescriptor_).
    // Step 7: Perform ! `ByteStreamControllerCallPullIfNeeded`(_controller_).
    readable_byte_stream_controller_invalidate_byob_request(scope, controller);
    let state = controller.stream(scope).data().state;
    if state == ReadableStreamState::Closed {
        debug_assert_eq!(bytes_written, 0);
        readable_byte_stream_controller_respond_in_closed_state(scope, controller)?;
    } else {
        debug_assert_eq!(state, ReadableStreamState::Readable);
        debug_assert!(bytes_written > 0);
        readable_byte_stream_controller_respond_in_readable_state(
            scope,
            controller,
            bytes_written,
        )?;
    }
    readable_byte_stream_controller_call_pull_if_needed(scope, controller);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-respond-with-new-view>
/// ByteStreamControllerRespondWithNewView(controller, view) performs the following steps:
pub(crate) fn readable_byte_stream_controller_respond_with_new_view(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
    view: js::ArrayBufferView<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Assert: _controller_.`[[pendingPullIntos]]` is not `empty`.
    // Step 2: Assert: ! `IsDetachedBuffer`(_view_.[[ViewedArrayBuffer]]) is false.
    // Step 3: Let _firstDescriptor_ be _controller_.`[[pendingPullIntos]]`[0].
    // Step 4: Let _state_ be _controller_.`[[stream]]`.`[[state]]`.
    // Step 5: If _state_ is "`closed`", If _view_.[[ByteLength]] is not 0, throw a ``TypeError``
    //         exception.
    // Step 6: Otherwise, Assert: _state_ is "`readable`". If _view_.[[ByteLength]] is 0, throw a
    //         ``TypeError`` exception.
    // Step 7: If _firstDescriptor_’s `byte offset` + _firstDescriptor_’ `bytes filled` is not
    //         _view_.[[ByteOffset]], throw a ``RangeError`` exception.
    // Step 8: If _firstDescriptor_’s `buffer byte length` is not
    //         _view_.[[ViewedArrayBuffer]].[[ByteLength]], throw a ``RangeError`` exception.
    // Step 9: If _firstDescriptor_’s `bytes filled` + _view_.[[ByteLength]] >
    //         _firstDescriptor_’s `byte length`, throw a ``RangeError`` exception.
    // Step 10: Let _viewByteLength_ be _view_.[[ByteLength]].
    // Step 11: Set _firstDescriptor_’s `buffer` to ?
    //          `TransferArrayBuffer`(_view_.[[ViewedArrayBuffer]]).
    // Step 12: Perform ? `ByteStreamControllerRespondInternal`(_controller_,
    //          _viewByteLength_).
    debug_assert!(!controller.data().pending_pull_intos.is_empty());
    let view_buffer = view.viewed_buffer(scope)?;
    debug_assert!(!view_buffer.is_detached());
    let (first_byte_offset, first_bytes_filled, first_buffer_byte_length, first_byte_length) = {
        let data = controller.data();
        let first = &data.pending_pull_intos[0];
        (
            first.byte_offset,
            first.bytes_filled,
            first.buffer_byte_length,
            first.byte_length,
        )
    };
    let state = controller.stream(scope).data().state;
    let view_byte_length = view.byte_length();
    if state == ReadableStreamState::Closed {
        if view_byte_length != 0 {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamBYOBRequest",
                c"view byte length must be 0 for a closed stream",
            ));
        }
    } else {
        debug_assert_eq!(state, ReadableStreamState::Readable);
        if view_byte_length == 0 {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamBYOBRequest",
                c"view byte length must not be 0 for a readable stream",
            ));
        }
    }
    if first_byte_offset + first_bytes_filled != view.byte_offset() {
        return Err(crate::algorithms::throw_invalid_value(
            scope,
            "ReadableStreamBYOBRequest",
            js::error::JSExnType::JSEXN_RANGEERR,
            c"the view's byte offset does not match the pending pull-into",
            "view",
            scope.root_value(view.as_value()),
        ));
    }
    if first_buffer_byte_length != view_buffer.byte_length() {
        return Err(crate::algorithms::throw_invalid_value(
            scope,
            "ReadableStreamBYOBRequest",
            js::error::JSExnType::JSEXN_RANGEERR,
            c"the view's buffer byte length does not match the pending pull-into",
            "view",
            scope.root_value(view.as_value()),
        ));
    }
    if first_bytes_filled + view_byte_length > first_byte_length {
        return Err(crate::algorithms::throw_invalid_value(
            scope,
            "ReadableStreamBYOBRequest",
            js::error::JSExnType::JSEXN_RANGEERR,
            c"the view's byte length exceeds the pending pull-into",
            "view",
            scope.root_value(view.as_value()),
        ));
    }
    let transferred = view_buffer.transfer(scope)?;
    controller.data_mut().pending_pull_intos[0]
        .buffer
        .set(transferred);
    readable_byte_stream_controller_respond_internal(scope, controller, view_byte_length)
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-shift-pending-pull-into>
/// ByteStreamControllerShiftPendingPullInto(controller) performs the following steps:
///
/// Boundary: removes the head descriptor from the traced `[[pendingPullIntos]]`
/// and returns it by value. Callers must either drop it immediately (no
/// allocation in between) or root it (`RootedTraceableBox`); a caller that binds
/// it across an allocation is still flagged by crown at the binding.
#[js::allow_unrooted]
pub(crate) fn readable_byte_stream_controller_shift_pending_pull_into(
    controller: &ReadableByteStreamController<'_>,
) -> PullIntoDescriptor {
    // Step 1: Assert: _controller_.`[[ReadableStreamBYOBRequest]]` is null.
    debug_assert!(controller.data().byob_request.is_none());
    // Step 2: Let _descriptor_ be _controller_.`[[pendingPullIntos]]`[0].
    // Step 3: `Remove` _descriptor_ from _controller_.`[[pendingPullIntos]]`.
    // Step 4: Return _descriptor_.
    controller
        .data_mut()
        .pending_pull_intos
        .pop_front()
        .expect("pendingPullIntos is not empty")
}

/// <https://streams.spec.whatwg.org/#readable-byte-stream-controller-should-call-pull>
/// ByteStreamControllerShouldCallPull(controller) performs the following steps:
pub(crate) fn readable_byte_stream_controller_should_call_pull(
    scope: &Scope<'_>,
    controller: &ReadableByteStreamController<'_>,
) -> bool {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = controller.stream(scope);
    // Step 2: If _stream_.`[[state]]` is not "`readable`", return false.
    if stream.data().state != ReadableStreamState::Readable {
        return false;
    }
    // Step 3: If _controller_.`[[closeRequested]]` is true, return false.
    if controller.data().close_requested {
        return false;
    }
    // Step 4: If _controller_.`[[started]]` is false, return false.
    if !controller.data().started {
        return false;
    }
    // Step 5: If ! `ReadableStreamHasDefaultReader`(_stream_) is true and !
    //         `ReadableStreamGetNumReadRequests`(_stream_) > 0, return true.
    if readable_stream_has_default_reader(scope, &stream)
        && readable_stream_get_num_read_requests(scope, &stream) > 0
    {
        return true;
    }
    // Step 6: If ! `ReadableStreamHasBYOBReader`(_stream_) is true and !
    //         `ReadableStreamGetNumReadIntoRequests`(_stream_) > 0, return true.
    if readable_stream_has_byob_reader(scope, &stream)
        && readable_stream_get_num_read_into_requests(scope, &stream) > 0
    {
        return true;
    }
    // Step 7: Let _desiredSize_ be ! `ByteStreamControllerGetDesiredSize`(_controller_).
    let desired_size = readable_byte_stream_controller_get_desired_size(scope, controller);
    // Step 8: Assert: _desiredSize_ is not null.
    let desired_size = desired_size.expect("desiredSize is not null when readable");
    // Step 9: If _desiredSize_ > 0, return true.
    // Step 10: Return false.
    desired_size > 0.0
}

/// <https://streams.spec.whatwg.org/#set-up-readable-byte-stream-controller>
/// SetUpByteStreamController(stream, controller, startAlgorithm, pullAlgorithm, cancelAlgorithm, highWaterMark, autoAllocateChunkSize) performs the following steps:
#[allow(clippy::too_many_arguments)]
pub(crate) fn set_up_readable_byte_stream_controller(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    controller: &ReadableByteStreamController<'_>,
    start_algorithm: AlgorithmArg<'_>,
    pull_algorithm: AlgorithmArg<'_>,
    cancel_algorithm: AlgorithmArg<'_>,
    algorithm_receiver: HandleValue<'_>,
    high_water_mark: f64,
    auto_allocate_chunk_size: Option<u64>,
) -> Result<(), ExnThrown> {
    // Step 1: Assert: _stream_.`[[controller]]` is undefined.
    debug_assert!(stream.data().controller.is_none());
    // Step 2: If _autoAllocateChunkSize_ is not undefined, Assert: !
    //         `IsInteger`(_autoAllocateChunkSize_) is true. Assert: _autoAllocateChunkSize_ is
    //         positive.
    if let Some(size) = auto_allocate_chunk_size {
        debug_assert!(size > 0);
    }
    // Step 3: Set _controller_.`[[stream]]` to _stream_.
    //         (Already done by `ReadableByteStreamController::new`.)
    debug_assert!(stream.eq_heap(&controller.data().stream));
    // Step 4: Set _controller_.`[[pullAgain]]` and _controller_.`[[pulling]]` to false.
    {
        let mut data = controller.data_mut();
        data.pull_again = false;
        data.pulling = false;
    }
    // Step 5: Set _controller_.`[[ReadableStreamBYOBRequest]]` to null.
    controller.data_mut().byob_request = None;
    // Step 6: Perform ! `ResetQueue`(_controller_).
    reset_byte_queue(controller);
    // Step 7: Set _controller_.`[[closeRequested]]` and _controller_.`[[started]]` to false.
    {
        let mut data = controller.data_mut();
        data.close_requested = false;
        data.started = false;
    }
    // Step 8: Set _controller_.`[[strategyHWM]]` to _highWaterMark_.
    controller.data_mut().strategy_hwm = high_water_mark;
    // Step 9: Set _controller_.`[[pullAlgorithm]]` to _pullAlgorithm_.
    controller.data_mut().pull_algorithm = Algorithm::from(pull_algorithm);
    // Step 10: Set _controller_.`[[cancelAlgorithm]]` to _cancelAlgorithm_.
    controller.data_mut().cancel_algorithm = Algorithm::from(cancel_algorithm);
    // (The algorithms close over `algorithm_receiver` — the underlying byte source — as their
    // `this` value; see the `algorithm_receiver` field.)
    controller
        .data_mut()
        .algorithm_receiver
        .set(algorithm_receiver.get());
    // Step 11: Set _controller_.`[[autoAllocateChunkSize]]` to _autoAllocateChunkSize_.
    controller.data_mut().auto_allocate_chunk_size = auto_allocate_chunk_size;
    // Step 12: Set _controller_.`[[pendingPullIntos]]` to a new empty `list`.
    controller.data_mut().pending_pull_intos.clear();
    // Step 13: Set _stream_.`[[controller]]` to _controller_.
    stream.data_mut().controller = Some(HeapReadableStreamController::from(*controller));
    // Step 14: Let _startResult_ be the result of performing _startAlgorithm_.
    let start_result =
        support::invoke_algorithm(scope, start_algorithm, algorithm_receiver, &[controller])?;
    // Step 15: Let _startPromise_ be `a promise resolved with` _startResult_.
    //          As in the default controller's setup, WebIDL "a promise resolved with" creates a new
    //          promise (it does not return a promise input as-is the way `Promise.resolve` does).
    let start_promise = Promise::new_resolved_with_value(scope, start_result)?;
    // Step 16: `Upon fulfillment` of _startPromise_, Set _controller_.`[[started]]` to true.
    //          Assert: _controller_.`[[pulling]]` is false. Assert: _controller_.`[[pullAgain]]` is
    //          false. Perform ! `ByteStreamControllerCallPullIfNeeded`(_controller_).
    // Step 17: `Upon rejection` of _startPromise_ with reason _r_, Perform !
    //          `ByteStreamControllerError`(_controller_, _r_).
    // (Steps 16 and 17 are implemented by `byte_start_promise_fulfilled` /
    // `byte_start_promise_rejected`.)
    support::react(
        scope,
        &start_promise,
        Some((byte_start_promise_fulfilled, controller)),
        Some((byte_start_promise_rejected, controller)),
    )?;
    Ok(())
}

/// <https://streams.spec.whatwg.org/#set-up-readable-byte-stream-controller-from-underlying-source>
/// SetUpByteStreamControllerFromUnderlyingSource(stream, underlyingSource, underlyingSourceDict, highWaterMark) performs the following steps:
pub(crate) fn set_up_readable_byte_stream_controller_from_underlying_source(
    scope: &Scope<'_>,
    stream: &ReadableStream<'_>,
    underlying_source: HandleValue<'_>,
    underlying_source_dict: &UnderlyingSource<'_>,
    high_water_mark: f64,
) -> Result<(), ExnThrown> {
    // Step 1: Let _controller_ be a `new` ``ReadableByteStreamController``.
    let controller = ReadableByteStreamController::new(scope, *stream)?;
    // The start/pull/cancel algorithms are the source's callbacks, invoked with `this` =
    // _underlyingSource_ (passed below as the algorithm receiver). An absent callback is no
    // algorithm, which the invoker treats as the resolved-undefined / constant algorithm.
    // Step 2: Let _startAlgorithm_ be an algorithm that returns undefined.
    // Step 5: If _underlyingSourceDict_["``start``"] `exists`, then set _startAlgorithm_ to an
    //         algorithm which returns the result of `invoking` _underlyingSourceDict_["``start``"]
    //         with argument list « _controller_ » and `callback this value` _underlyingSource_.
    let start_algorithm = AlgorithmArg::from_member(scope, underlying_source_dict.start.as_ref());
    // Step 3: Let _pullAlgorithm_ be an algorithm that returns `a promise resolved with` undefined.
    // Step 6: If _underlyingSourceDict_["``pull``"] `exists`, then set _pullAlgorithm_ to an
    //         algorithm which returns the result of `invoking` _underlyingSourceDict_["``pull``"]
    //         with argument list « _controller_ » and `callback this value` _underlyingSource_.
    let pull_algorithm = AlgorithmArg::from_member(scope, underlying_source_dict.pull.as_ref());
    // Step 4: Let _cancelAlgorithm_ be an algorithm that returns `a promise resolved with`
    //         undefined.
    // Step 7: If _underlyingSourceDict_["``cancel``"] `exists`, then set _cancelAlgorithm_ to an
    //         algorithm which takes an argument _reason_ and returns the result of `invoking`
    //         _underlyingSourceDict_["``cancel``"] with argument list « _reason_ » and `callback
    //         this value` _underlyingSource_.
    let cancel_algorithm = AlgorithmArg::from_member(scope, underlying_source_dict.cancel.as_ref());
    // Step 8: Let _autoAllocateChunkSize_ be _underlyingSourceDict_["``autoAllocateChunkSize``"],
    //         if it `exists`, or undefined otherwise.
    let auto_allocate_chunk_size = underlying_source_dict
        .auto_allocate_chunk_size
        .map(|size| size.0);
    // Step 9: If _autoAllocateChunkSize_ is 0, then throw a ``TypeError`` exception.
    if auto_allocate_chunk_size == Some(0) {
        return Err(js::error::throw_type_error(
            scope,
            c"autoAllocateChunkSize cannot be 0",
        ));
    }
    // Step 10: Perform ? `SetUpByteStreamController`(_stream_, _controller_,
    //          _startAlgorithm_, _pullAlgorithm_, _cancelAlgorithm_, _highWaterMark_,
    //          _autoAllocateChunkSize_).
    set_up_readable_byte_stream_controller(
        scope,
        stream,
        &controller,
        start_algorithm,
        pull_algorithm,
        cancel_algorithm,
        underlying_source,
        high_water_mark,
        auto_allocate_chunk_size,
    )
}
