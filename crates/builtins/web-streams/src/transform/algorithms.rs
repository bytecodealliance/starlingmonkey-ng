// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Standalone algorithms from <https://streams.spec.whatwg.org/>

use js::error::ExnThrown;
use js::exception::take_pending_or_undefined;
use js::function::cast_payload;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleValue, OptionHeapExt};
use js::value;
use js::Function;
use js::Promise;

use super::transform_stream::TransformStream;
use super::transform_stream_default_controller::TransformStreamDefaultController;
use super::transformer::Transformer;
use crate::algorithms::{invalid_state_error, pair_parts, pair_payload};
use crate::readable::algorithms::{
    create_readable_stream, readable_stream_default_controller_can_close_or_enqueue,
    readable_stream_default_controller_close, readable_stream_default_controller_enqueue,
    readable_stream_default_controller_error, readable_stream_default_controller_get_desired_size,
    readable_stream_default_controller_has_backpressure,
};
use crate::readable::readable_stream::{ReadableStream, ReadableStreamState};
use crate::support;
use crate::support::{Algorithm, AlgorithmArg};
use crate::writable::algorithms::{
    create_writable_stream, writable_stream_default_controller_error_if_needed,
};
use crate::writable::writable_stream::{WritableStream, WritableStreamState};

// ---------------------------------------------------------------------------
// Transform-stream accessors, native algorithm callbacks, and reactions.
// ---------------------------------------------------------------------------

/// The transform stream's `[[controller]]`.
fn ts_controller<'r>(
    scope: &'r Scope<'_>,
    stream: &TransformStream<'_>,
) -> TransformStreamDefaultController<'r> {
    stream
        .data()
        .controller
        .get(scope)
        .expect("transform stream has a controller")
}

/// The transform stream's `[[readable]]`.
fn ts_readable<'r>(scope: &'r Scope<'_>, stream: &TransformStream<'_>) -> ReadableStream<'r> {
    stream.data().readable.get(scope)
}

/// The transform stream's `[[writable]]`.
fn ts_writable<'r>(scope: &'r Scope<'_>, stream: &TransformStream<'_>) -> WritableStream<'r> {
    stream.data().writable.get(scope)
}

/// The controller's `[[stream]]`.
fn ts_controller_stream<'r>(
    scope: &'r Scope<'_>,
    controller: &TransformStreamDefaultController<'_>,
) -> TransformStream<'r> {
    controller.data().stream.get(scope)
}

/// The controller's `[[finishPromise]]` (asserted set).
fn ts_finish_promise<'r>(
    scope: &'r Scope<'_>,
    controller: &TransformStreamDefaultController<'_>,
) -> Promise<'r> {
    controller
        .data()
        .finish_promise
        .get(scope)
        .expect("finish promise is set")
}

// Native algorithm callbacks built by `InitializeTransformStream` (payload = the
// transform stream, except the start algorithm whose payload is the start
// promise). Each returns the underlying transform algorithm's promise as a value.

/// The desired size of the transform controller's readable side (the
/// `TransformStreamDefaultController.desiredSize` getter).
pub(crate) fn transform_controller_readable_desired_size(
    scope: &Scope<'_>,
    controller: &TransformStreamDefaultController<'_>,
) -> Option<f64> {
    let stream = ts_controller_stream(scope, controller);
    let readable = ts_readable(scope, &stream);
    let readable_controller = readable
        .default_controller(scope)
        .expect("readable side has a default controller");
    readable_stream_default_controller_get_desired_size(scope, &readable_controller)
}

/// `InitializeTransformStream` step 1: the start algorithm returns `startPromise`.
fn ts_start_native(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    Ok(*payload)
}

/// The default transform algorithm (`SetUpTransformStreamDefaultControllerFromTransformer`
/// step 2): enqueue the chunk; payload = the controller.
fn ts_default_transform_native(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<TransformStreamDefaultController>(scope, payload);
    transform_stream_default_controller_enqueue(scope, &controller, args.get(0))?;
    Ok(value::undefined())
}

fn ts_sink_write_native(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TransformStream>(scope, payload);
    let promise = transform_stream_default_sink_write_algorithm(scope, &stream, args.get(0));
    Ok(promise.as_value())
}

fn ts_sink_abort_native(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TransformStream>(scope, payload);
    let promise = transform_stream_default_sink_abort_algorithm(scope, &stream, args.get(0));
    Ok(promise.as_value())
}

fn ts_sink_close_native(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TransformStream>(scope, payload);
    let promise = transform_stream_default_sink_close_algorithm(scope, &stream);
    Ok(promise.as_value())
}

fn ts_source_pull_native(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TransformStream>(scope, payload);
    let promise = transform_stream_default_source_pull_algorithm(scope, &stream);
    Ok(promise.as_value())
}

fn ts_source_cancel_native(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TransformStream>(scope, payload);
    let promise = transform_stream_default_source_cancel_algorithm(scope, &stream, args.get(0));
    Ok(promise.as_value())
}

/// `TransformStreamDefaultControllerPerformTransform` step 2 rejection steps.
fn ts_perform_transform_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<TransformStreamDefaultController>(scope, payload);
    let stream = ts_controller_stream(scope, &controller);
    let r = args.get(0);
    // Perform ! TransformStreamError(stream, r). Throw r.
    transform_stream_error(scope, &stream, r);
    Err(js::exception::set_pending(
        scope,
        r,
        js::native::ExceptionStackBehavior::DoNotCapture,
    ))
}

/// `TransformStreamDefaultSinkWriteAlgorithm` step 3 fulfillment steps
/// (payload = the controller; the chunk is taken from the controller's
/// `pending_write_chunk` slot).
fn ts_sink_write_after_backpressure(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    controller: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<TransformStreamDefaultController>(scope, controller);
    let chunk = controller.data().pending_write_chunk.get(scope);
    controller
        .data()
        .pending_write_chunk
        .set(value::undefined());
    let stream = ts_controller_stream(scope, &controller);
    let writable = ts_writable(scope, &stream);
    let state = writable.data().state;
    // If state is "erroring", throw writable's storedError.
    if state == WritableStreamState::Erroring {
        let stored_error = writable.data().stored_error.get(scope);
        return Err(js::exception::set_pending(
            scope,
            stored_error,
            js::native::ExceptionStackBehavior::DoNotCapture,
        ));
    }
    debug_assert_eq!(state, WritableStreamState::Writable);
    // Return ! TransformStreamDefaultControllerPerformTransform(controller, chunk).
    let promise = transform_stream_default_controller_perform_transform(scope, &controller, chunk);
    Ok(promise.as_value())
}

/// `TransformStreamDefaultSinkCloseAlgorithm` step 7 fulfillment.
fn ts_sink_close_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<TransformStreamDefaultController>(scope, payload);
    let stream = ts_controller_stream(scope, &controller);
    let readable = ts_readable(scope, &stream);
    let finish = ts_finish_promise(scope, &controller);
    if readable.data().state == ReadableStreamState::Errored {
        let stored_error = readable.data().stored_error.get(scope);
        finish.reject(scope, stored_error).expect("reject finish");
    } else {
        let readable_controller = readable
            .default_controller(scope)
            .expect("readable side has a default controller");
        readable_stream_default_controller_close(scope, &readable_controller);
        finish.resolve(scope, HandleValue::undefined())?;
    }
    Ok(value::undefined())
}

/// `TransformStreamDefaultSinkCloseAlgorithm` step 7 rejection.
fn ts_sink_close_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<TransformStreamDefaultController>(scope, payload);
    let stream = ts_controller_stream(scope, &controller);
    let readable = ts_readable(scope, &stream);
    let r = args.get(0);
    let readable_controller = readable
        .default_controller(scope)
        .expect("readable side has a default controller");
    readable_stream_default_controller_error(scope, &readable_controller, r);
    ts_finish_promise(scope, &controller)
        .reject(scope, r)
        .expect("reject finish");
    Ok(value::undefined())
}

/// `TransformStreamDefaultSinkAbortAlgorithm` step 7 fulfillment
/// (payload = [controller, reason]).
fn ts_sink_abort_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let (controller_value, reason) = pair_parts(scope, payload);
    let controller = cast_payload::<TransformStreamDefaultController>(scope, controller_value);
    let stream = ts_controller_stream(scope, &controller);
    let readable = ts_readable(scope, &stream);
    let finish = ts_finish_promise(scope, &controller);
    if readable.data().state == ReadableStreamState::Errored {
        let stored_error = readable.data().stored_error.get(scope);
        finish.reject(scope, stored_error).expect("reject finish");
    } else {
        let readable_controller = readable
            .default_controller(scope)
            .expect("readable side has a default controller");
        readable_stream_default_controller_error(scope, &readable_controller, reason);
        finish.resolve(scope, HandleValue::undefined())?;
    }
    Ok(value::undefined())
}

/// `TransformStreamDefaultSinkAbortAlgorithm` step 7 rejection
/// (payload = [controller, reason]).
fn ts_sink_abort_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let (controller_value, _reason) = pair_parts(scope, payload);
    let controller = cast_payload::<TransformStreamDefaultController>(scope, controller_value);
    let stream = ts_controller_stream(scope, &controller);
    let readable = ts_readable(scope, &stream);
    let r = args.get(0);
    let readable_controller = readable
        .default_controller(scope)
        .expect("readable side has a default controller");
    readable_stream_default_controller_error(scope, &readable_controller, r);
    ts_finish_promise(scope, &controller)
        .reject(scope, r)
        .expect("reject finish");
    Ok(value::undefined())
}

/// `TransformStreamDefaultSourceCancelAlgorithm` step 7 fulfillment
/// (payload = [controller, reason]).
fn ts_source_cancel_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let (controller_value, reason) = pair_parts(scope, payload);
    let controller = cast_payload::<TransformStreamDefaultController>(scope, controller_value);
    let stream = ts_controller_stream(scope, &controller);
    let writable = ts_writable(scope, &stream);
    let finish = ts_finish_promise(scope, &controller);
    if writable.data().state == WritableStreamState::Errored {
        let stored_error = writable.data().stored_error.get(scope);
        finish.reject(scope, stored_error).expect("reject finish");
    } else {
        let writable_controller = writable.controller(scope);
        writable_stream_default_controller_error_if_needed(scope, &writable_controller, reason);
        transform_stream_unblock_write(scope, &stream);
        finish.resolve(scope, HandleValue::undefined())?;
    }
    Ok(value::undefined())
}

/// `TransformStreamDefaultSourceCancelAlgorithm` step 7 rejection
/// (payload = [controller, reason]).
fn ts_source_cancel_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let (controller_value, _reason) = pair_parts(scope, payload);
    let controller = cast_payload::<TransformStreamDefaultController>(scope, controller_value);
    let stream = ts_controller_stream(scope, &controller);
    let writable = ts_writable(scope, &stream);
    let r = args.get(0);
    let writable_controller = writable.controller(scope);
    writable_stream_default_controller_error_if_needed(scope, &writable_controller, r);
    transform_stream_unblock_write(scope, &stream);
    ts_finish_promise(scope, &controller)
        .reject(scope, r)
        .expect("reject finish");
    Ok(value::undefined())
}

/// <https://streams.spec.whatwg.org/#initialize-transform-stream>
/// InitializeTransformStream(stream, startPromise, writableHighWaterMark, writableSizeAlgorithm, readableHighWaterMark, readableSizeAlgorithm) performs the following steps:
#[allow(clippy::too_many_arguments)]
pub(crate) fn initialize_transform_stream(
    scope: &Scope<'_>,
    stream: &TransformStream<'_>,
    start_promise: &Promise<'_>,
    writable_high_water_mark: f64,
    writable_size_algorithm: HandleValue<'_>,
    readable_high_water_mark: f64,
    readable_size_algorithm: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Let _startAlgorithm_ be an algorithm that returns _startPromise_.
    let start = AlgorithmArg::native(scope, ts_start_native, start_promise)?;
    // Step 2: Let _writeAlgorithm_ be the following steps, taking a _chunk_ argument: Return !
    //         `TransformStreamDefaultSinkWriteAlgorithm`(_stream_, _chunk_).
    let write = AlgorithmArg::native(scope, ts_sink_write_native, *stream)?;
    // Step 3: Let _abortAlgorithm_ be the following steps, taking a _reason_ argument: Return !
    //         `TransformStreamDefaultSinkAbortAlgorithm`(_stream_, _reason_).
    let abort = AlgorithmArg::native(scope, ts_sink_abort_native, *stream)?;
    // Step 4: Let _closeAlgorithm_ be the following steps: Return !
    //         `TransformStreamDefaultSinkCloseAlgorithm`(_stream_).
    let close = AlgorithmArg::native(scope, ts_sink_close_native, *stream)?;
    // Step 5: Set _stream_.`[[writable]]` to ! `CreateWritableStream`(_startAlgorithm_,
    //         _writeAlgorithm_, _closeAlgorithm_, _abortAlgorithm_, _writableHighWaterMark_,
    //         _writableSizeAlgorithm_).
    let writable = create_writable_stream(
        scope,
        start,
        write,
        close,
        abort,
        writable_high_water_mark,
        writable_size_algorithm,
    )?;
    stream.data_mut().writable.set(writable);
    // Step 6: Let _pullAlgorithm_ be the following steps: Return !
    //         `TransformStreamDefaultSourcePullAlgorithm`(_stream_).
    let pull = AlgorithmArg::native(scope, ts_source_pull_native, *stream)?;
    // Step 7: Let _cancelAlgorithm_ be the following steps, taking a _reason_ argument: Return !
    //         `TransformStreamDefaultSourceCancelAlgorithm`(_stream_, _reason_).
    let cancel = AlgorithmArg::native(scope, ts_source_cancel_native, *stream)?;
    // Step 8: Set _stream_.`[[readable]]` to ! `CreateReadableStream`(_startAlgorithm_,
    //         _pullAlgorithm_, _cancelAlgorithm_, _readableHighWaterMark_,
    //         _readableSizeAlgorithm_).
    let readable = create_readable_stream(
        scope,
        start,
        pull,
        cancel,
        readable_high_water_mark,
        readable_size_algorithm,
    )?;
    stream.data_mut().readable.set(readable);
    // Step 9: Set _stream_.`[[backpressure]]` and _stream_.`[[backpressureChangePromise]]` to
    //         undefined. The `[[backpressure]]` slot is set to undefined so that it can be
    //         initialized by `TransformStreamSetBackpressure`. Alternatively, implementations can
    //         use a strictly boolean value for `[[backpressure]]` and change the way it is
    //         initialized. This will not be visible to user code so long as the initialization is
    //         correctly completed before the transformer’s ``start()`` method is called.
    //         (We use a boolean `[[backpressure]]`, default false, and `None` change promise.)
    // Step 10: Perform ! `TransformStreamSetBackpressure`(_stream_, true).
    transform_stream_set_backpressure(scope, stream, true);
    // Step 11: Set _stream_.`[[controller]]` to undefined.
    stream.data_mut().controller = None;
    Ok(())
}

/// <https://streams.spec.whatwg.org/#transform-stream-error>
/// TransformStreamError(stream, e) performs the following steps:
pub(crate) fn transform_stream_error(
    scope: &Scope<'_>,
    stream: &TransformStream<'_>,
    e: HandleValue<'_>,
) {
    // Step 1: Perform !
    //         `DefaultControllerError`(_stream_.`[[readable]]`.`[[controller]]`,
    //         _e_).
    let readable = ts_readable(scope, stream);
    let readable_controller = readable
        .default_controller(scope)
        .expect("readable side has a default controller");
    readable_stream_default_controller_error(scope, &readable_controller, e);
    // Step 2: Perform ! `TransformStreamErrorWritableAndUnblockWrite`(_stream_, _e_).
    transform_stream_error_writable_and_unblock_write(scope, stream, e);
}

/// <https://streams.spec.whatwg.org/#transform-stream-error-writable-and-unblock-write>
/// TransformStreamErrorWritableAndUnblockWrite(stream, e) performs the following steps:
pub(crate) fn transform_stream_error_writable_and_unblock_write(
    scope: &Scope<'_>,
    stream: &TransformStream<'_>,
    e: HandleValue<'_>,
) {
    // Step 1: Perform !
    //         `TransformStreamDefaultControllerClearAlgorithms`(_stream_.`[[controller]]`).
    let controller = ts_controller(scope, stream);
    transform_stream_default_controller_clear_algorithms(&controller);
    // Step 2: Perform !
    //         `WritableStreamDefaultControllerErrorIfNeeded`(_stream_.`[[writable]]`.`[[controller]]`,
    //         _e_).
    let writable = ts_writable(scope, stream);
    let writable_controller = writable.controller(scope);
    writable_stream_default_controller_error_if_needed(scope, &writable_controller, e);
    // Step 3: Perform ! `TransformStreamUnblockWrite`(_stream_).
    transform_stream_unblock_write(scope, stream);
}

/// <https://streams.spec.whatwg.org/#transform-stream-set-backpressure>
/// TransformStreamSetBackpressure(stream, backpressure) performs the following steps:
pub(crate) fn transform_stream_set_backpressure(
    scope: &Scope<'_>,
    stream: &TransformStream<'_>,
    backpressure: bool,
) {
    // Step 1: Assert: _stream_.`[[backpressure]]` is not _backpressure_.
    debug_assert_ne!(stream.data().backpressure, backpressure);
    // Step 2: If _stream_.`[[backpressureChangePromise]]` is not undefined, `resolve`
    //         stream.`[[backpressureChangePromise]]` with undefined.
    let promise = stream.data().backpressure_change_promise.get(scope);
    if let Some(promise) = promise {
        promise.resolve(scope, HandleValue::undefined()).unwrap();
    }
    // Step 3: Set _stream_.`[[backpressureChangePromise]]` to `a new promise`.
    let new_promise = Promise::new_pending(scope).expect("new promise");
    stream
        .data_mut()
        .backpressure_change_promise
        .set(new_promise);
    // Step 4: Set _stream_.`[[backpressure]]` to _backpressure_.
    stream.data_mut().backpressure = backpressure;
}

/// <https://streams.spec.whatwg.org/#transform-stream-unblock-write>
/// TransformStreamUnblockWrite(stream) performs the following steps:
pub(crate) fn transform_stream_unblock_write(scope: &Scope<'_>, stream: &TransformStream<'_>) {
    // Step 1: If _stream_.`[[backpressure]]` is true, perform !
    //         `TransformStreamSetBackpressure`(_stream_, false).
    if stream.data().backpressure {
        transform_stream_set_backpressure(scope, stream, false);
    }
}

/// <https://streams.spec.whatwg.org/#set-up-transform-stream-default-controller>
/// SetUpTransformStreamDefaultController(stream, controller, transformAlgorithm, flushAlgorithm, cancelAlgorithm) performs the following steps:
pub(crate) fn set_up_transform_stream_default_controller(
    _scope: &Scope<'_>,
    stream: &TransformStream<'_>,
    controller: &TransformStreamDefaultController<'_>,
    transform_algorithm: AlgorithmArg<'_>,
    flush_algorithm: AlgorithmArg<'_>,
    cancel_algorithm: AlgorithmArg<'_>,
    algorithm_receiver: HandleValue<'_>,
) {
    // Step 1: Assert: _stream_ `implements` ``TransformStream``.
    // Step 2: Assert: _stream_.`[[controller]]` is undefined.
    debug_assert!(stream.data().controller.is_none());
    // Step 3: Set _controller_.`[[stream]]` to _stream_.
    //         (Already done by `TransformStreamDefaultController::new`.)
    debug_assert!(stream.eq_heap(&controller.data().stream));
    // Step 4: Set _stream_.`[[controller]]` to _controller_.
    stream.data_mut().controller.set(*controller);
    // Step 5: Set _controller_.`[[transformAlgorithm]]` to _transformAlgorithm_.
    controller.data_mut().transform_algorithm = Algorithm::from(transform_algorithm);
    // Step 6: Set _controller_.`[[flushAlgorithm]]` to _flushAlgorithm_.
    controller.data_mut().flush_algorithm = Algorithm::from(flush_algorithm);
    // Step 7: Set _controller_.`[[cancelAlgorithm]]` to _cancelAlgorithm_.
    controller.data_mut().cancel_algorithm = Algorithm::from(cancel_algorithm);
    // (The algorithms close over `algorithm_receiver` — the transformer — as their `this`.)
    controller
        .data_mut()
        .algorithm_receiver
        .set(algorithm_receiver.get());
}

/// <https://streams.spec.whatwg.org/#set-up-transform-stream-default-controller-from-transformer>
/// SetUpTransformStreamDefaultControllerFromTransformer(stream, transformer, transformerDict) performs the following steps:
pub(crate) fn set_up_transform_stream_default_controller_from_transformer(
    scope: &Scope<'_>,
    stream: &TransformStream<'_>,
    transformer: HandleValue<'_>,
    transformer_dict: &Transformer<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Let _controller_ be a `new` ``TransformStreamDefaultController``.
    let controller = TransformStreamDefaultController::new(scope, *stream)?;
    // Step 2: Let _transformAlgorithm_ be the following steps, taking a _chunk_ argument: Let
    //         _result_ be `TransformStreamDefaultControllerEnqueue`(_controller_, _chunk_). If
    //         _result_ is an abrupt completion, return `a promise rejected with`
    //         _result_.[[Value]]. Otherwise, return `a promise resolved with` undefined.
    // Step 5: If _transformerDict_["``transform``"] `exists`, set _transformAlgorithm_ to an
    //         algorithm which takes an argument _chunk_ and returns the result of `invoking`
    //         _transformerDict_["``transform``"] with argument list « _chunk_, _controller_ » and
    //         `callback this value` _transformer_.
    let transform_algorithm = match transformer_dict.transform.as_ref() {
        Some(transform) => AlgorithmArg::Js(scope.root_value(transform.as_value())),
        None => AlgorithmArg::native(scope, ts_default_transform_native, controller)?,
    };
    // Step 3: Let _flushAlgorithm_ be an algorithm which returns `a promise resolved with`
    //         undefined.
    // Step 6: If _transformerDict_["``flush``"] `exists`, set _flushAlgorithm_ to an algorithm
    //         which returns the result of `invoking` _transformerDict_["``flush``"] with argument
    //         list « _controller_ » and `callback this value` _transformer_.
    let flush_algorithm = AlgorithmArg::from_member(scope, transformer_dict.flush.as_ref());
    // Step 4: Let _cancelAlgorithm_ be an algorithm which returns `a promise resolved with`
    //         undefined.
    // Step 7: If _transformerDict_["``cancel``"] `exists`, set _cancelAlgorithm_ to an algorithm
    //         which takes an argument _reason_ and returns the result of `invoking`
    //         _transformerDict_["``cancel``"] with argument list « _reason_ » and `callback this
    //         value` _transformer_.
    let cancel_algorithm = AlgorithmArg::from_member(scope, transformer_dict.cancel.as_ref());
    // Step 8: Perform ! `SetUpTransformStreamDefaultController`(_stream_, _controller_,
    //         _transformAlgorithm_, _flushAlgorithm_, _cancelAlgorithm_).
    set_up_transform_stream_default_controller(
        scope,
        stream,
        &controller,
        transform_algorithm,
        flush_algorithm,
        cancel_algorithm,
        transformer,
    );
    Ok(())
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-controller-clear-algorithms>
/// TransformStreamDefaultControllerClearAlgorithms(controller) is called once the stream is closed or errored and the algorithms will not be executed any more. By removing the algorithm references it permits the transformer object to be garbage collected even if the TransformStream itself is still referenced. This is observable using weak references. See tc39/proposal-weakrefs#31 for more detail. It performs the following steps:
pub(crate) fn transform_stream_default_controller_clear_algorithms(
    controller: &TransformStreamDefaultController<'_>,
) {
    // Step 1: Set _controller_.`[[transformAlgorithm]]` to undefined.
    controller.data_mut().transform_algorithm = Algorithm::None;
    // Step 2: Set _controller_.`[[flushAlgorithm]]` to undefined.
    controller.data_mut().flush_algorithm = Algorithm::None;
    // Step 3: Set _controller_.`[[cancelAlgorithm]]` to undefined.
    controller.data_mut().cancel_algorithm = Algorithm::None;
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-controller-enqueue>
/// TransformStreamDefaultControllerEnqueue(controller, chunk) performs the following steps:
pub(crate) fn transform_stream_default_controller_enqueue(
    scope: &Scope<'_>,
    controller: &TransformStreamDefaultController<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = ts_controller_stream(scope, controller);
    // Step 2: Let _readableController_ be _stream_.`[[readable]]`.`[[controller]]`.
    let readable = ts_readable(scope, &stream);
    let readable_controller = readable
        .default_controller(scope)
        .expect("readable side has a default controller");
    // Step 3: If ! `DefaultControllerCanCloseOrEnqueue`(_readableController_) is
    //         false, throw a ``TypeError`` exception.
    if !readable_stream_default_controller_can_close_or_enqueue(scope, &readable_controller) {
        return Err(crate::algorithms::throw_invalid_state(
            scope,
            "TransformStreamDefaultController",
            c"Readable side is not in a state that permits enqueue",
        ));
    }
    // Step 4: Let _enqueueResult_ be `DefaultControllerEnqueue`(_readableController_,
    //         _chunk_).
    // Step 5: If _enqueueResult_ is an abrupt completion, Perform !
    //         `TransformStreamErrorWritableAndUnblockWrite`(_stream_, _enqueueResult_.[[Value]]).
    //         Throw _stream_.`[[readable]]`.`[[storedError]]`.
    if readable_stream_default_controller_enqueue(scope, &readable_controller, chunk).is_err() {
        let error = take_pending_or_undefined(scope);
        transform_stream_error_writable_and_unblock_write(scope, &stream, error);
        let stored_error = readable.data().stored_error.get(scope);
        return Err(js::exception::set_pending(
            scope,
            stored_error,
            js::native::ExceptionStackBehavior::DoNotCapture,
        ));
    }
    // Step 6: Let _backpressure_ be !
    //         `DefaultControllerHasBackpressure`(_readableController_).
    let backpressure =
        readable_stream_default_controller_has_backpressure(scope, &readable_controller);
    // Step 7: If _backpressure_ is not _stream_.`[[backpressure]]`, Assert: _backpressure_ is true.
    //         Perform ! `TransformStreamSetBackpressure`(_stream_, true).
    if backpressure != stream.data().backpressure {
        debug_assert!(backpressure);
        transform_stream_set_backpressure(scope, &stream, true);
    }
    Ok(())
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-controller-error>
/// TransformStreamDefaultControllerError(controller, e) performs the following steps:
pub(crate) fn transform_stream_default_controller_error(
    scope: &Scope<'_>,
    controller: &TransformStreamDefaultController<'_>,
    e: HandleValue<'_>,
) {
    // Step 1: Perform ! `TransformStreamError`(_controller_.`[[stream]]`, _e_).
    let stream = ts_controller_stream(scope, controller);
    transform_stream_error(scope, &stream, e);
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-controller-perform-transform>
/// TransformStreamDefaultControllerPerformTransform(controller, chunk) performs the following steps:
pub(crate) fn transform_stream_default_controller_perform_transform<'r>(
    scope: &'r Scope<'_>,
    controller: &TransformStreamDefaultController<'_>,
    chunk: HandleValue<'r>,
) -> Promise<'r> {
    // Step 1: Let _transformPromise_ be the result of performing
    //         _controller_.`[[transformAlgorithm]]`, passing _chunk_.
    //         The from-transformer transform callback is invoked with « chunk, controller ».
    let transform_algorithm = controller.data().transform_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let transform_promise = support::invoke_promise_algorithm(
        scope,
        transform_algorithm,
        receiver,
        &[chunk, scope.root_value(controller.as_value())],
    );
    // Step 2: Return the result of `reacting` to _transformPromise_ with the following rejection
    //         steps given the argument _r_: Perform !
    //         `TransformStreamError`(_controller_.`[[stream]]`, _r_). Throw _r_.
    // (The rejection callback's payload — the controller — never changes, so it
    // is created on the first transform and reused for every subsequent chunk.)
    if controller.data().transform_rejected_fn.is_none() {
        let on_rejected =
            Function::new_callback(scope, c"", 1, ts_perform_transform_rejected, controller)
                .expect("cb");
        controller.data_mut().transform_rejected_fn.set(on_rejected);
    }
    let on_rejected = controller
        .data()
        .transform_rejected_fn
        .get(scope)
        .expect("created above");
    transform_promise
        .then(scope, None, Some(*on_rejected))
        .expect("then")
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-controller-terminate>
/// TransformStreamDefaultControllerTerminate(controller) performs the following steps:
pub(crate) fn transform_stream_default_controller_terminate(
    scope: &Scope<'_>,
    controller: &TransformStreamDefaultController<'_>,
) {
    // Step 1: Let _stream_ be _controller_.`[[stream]]`.
    let stream = ts_controller_stream(scope, controller);
    // Step 2: Let _readableController_ be _stream_.`[[readable]]`.`[[controller]]`.
    let readable = ts_readable(scope, &stream);
    let readable_controller = readable
        .default_controller(scope)
        .expect("readable side has a default controller");
    // Step 3: Perform ! `DefaultControllerClose`(_readableController_).
    readable_stream_default_controller_close(scope, &readable_controller);
    // Step 4: Let _error_ be a ``TypeError`` exception indicating that the stream has been
    //         terminated.
    let error = invalid_state_error(
        scope,
        "TransformStreamDefaultController",
        c"The stream has been terminated",
    );
    // Step 5: Perform ! `TransformStreamErrorWritableAndUnblockWrite`(_stream_, _error_).
    transform_stream_error_writable_and_unblock_write(scope, &stream, error);
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-sink-write-algorithm>
/// TransformStreamDefaultSinkWriteAlgorithm(stream, chunk) performs the following steps:
pub(crate) fn transform_stream_default_sink_write_algorithm<'r>(
    scope: &'r Scope<'_>,
    stream: &TransformStream<'_>,
    chunk: HandleValue<'r>,
) -> Promise<'r> {
    // Step 1: Assert: _stream_.`[[writable]]`.`[[state]]` is "`writable`".
    debug_assert_eq!(
        ts_writable(scope, stream).data().state,
        WritableStreamState::Writable
    );
    // Step 2: Let _controller_ be _stream_.`[[controller]]`.
    let controller = ts_controller(scope, stream);
    // Step 3: If _stream_.`[[backpressure]]` is true, Let _backpressureChangePromise_ be
    //         _stream_.`[[backpressureChangePromise]]`. Assert: _backpressureChangePromise_ is not
    //         undefined. Return the result of `reacting` to _backpressureChangePromise_ with the
    //         following fulfillment steps: Let _writable_ be _stream_.`[[writable]]`. Let _state_
    //         be _writable_.`[[state]]`. If _state_ is "`erroring`", throw
    //         _writable_.`[[storedError]]`. Assert: _state_ is "`writable`". Return !
    //         `TransformStreamDefaultControllerPerformTransform`(_controller_, _chunk_).
    // (The fulfillment steps are implemented by `ts_sink_write_after_backpressure`.)
    if stream.data().backpressure {
        let backpressure_change_promise = stream
            .data()
            .backpressure_change_promise
            .get(scope)
            .expect("backpressureChangePromise is set");
        // Park the chunk on the controller and reuse the per-controller callback,
        // so only the returned derived promise is allocated per chunk.
        controller.data().pending_write_chunk.set(*chunk);
        if controller.data().write_after_backpressure_fn.is_none() {
            let on_fulfilled =
                Function::new_callback(scope, c"", 1, ts_sink_write_after_backpressure, controller)
                    .expect("cb");
            controller
                .data_mut()
                .write_after_backpressure_fn
                .set(on_fulfilled);
        }
        let on_fulfilled = controller
            .data()
            .write_after_backpressure_fn
            .get(scope)
            .expect("created above");
        return backpressure_change_promise
            .then(scope, Some(*on_fulfilled), None)
            .expect("then");
    }
    // Step 4: Return ! `TransformStreamDefaultControllerPerformTransform`(_controller_, _chunk_).
    transform_stream_default_controller_perform_transform(scope, &controller, chunk)
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-sink-abort-algorithm>
/// TransformStreamDefaultSinkAbortAlgorithm(stream, reason) performs the following steps:
pub(crate) fn transform_stream_default_sink_abort_algorithm<'r>(
    scope: &'r Scope<'_>,
    stream: &TransformStream<'_>,
    reason: HandleValue<'_>,
) -> Promise<'r> {
    // Step 1: Let _controller_ be _stream_.`[[controller]]`.
    let controller = ts_controller(scope, stream);
    // Step 2: If _controller_.`[[finishPromise]]` is not undefined, return
    //         _controller_.`[[finishPromise]]`.
    if controller.data().finish_promise.is_some() {
        return ts_finish_promise(scope, &controller);
    }
    // Step 3: Let _readable_ be _stream_.`[[readable]]`. (Used by the reaction.)
    // Step 4: Let _controller_.`[[finishPromise]]` be a new promise.
    let finish = Promise::new_pending(scope).expect("new promise");
    controller.data_mut().finish_promise.set(finish);
    // Step 5: Let _cancelPromise_ be the result of performing _controller_.`[[cancelAlgorithm]]`,
    //         passing _reason_.
    let cancel_algorithm = controller.data().cancel_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let cancel_promise =
        support::invoke_promise_algorithm(scope, cancel_algorithm, receiver, &[reason]);
    // Step 6: Perform ! `TransformStreamDefaultControllerClearAlgorithms`(_controller_).
    transform_stream_default_controller_clear_algorithms(&controller);
    // Step 7: `React` to _cancelPromise_: If _cancelPromise_ was fulfilled, then: If
    //         _readable_.`[[state]]` is "`errored`", `reject` _controller_.`[[finishPromise]]` with
    //         _readable_.`[[storedError]]`. Otherwise: Perform !
    //         `DefaultControllerError`(_readable_.`[[controller]]`, _reason_).
    //         `Resolve` _controller_.`[[finishPromise]]` with undefined. If _cancelPromise_ was
    //         rejected with reason _r_, then: Perform !
    //         `DefaultControllerError`(_readable_.`[[controller]]`, _r_). `Reject`
    //         _controller_.`[[finishPromise]]` with _r_.
    // (Implemented by `ts_sink_abort_fulfilled` / `ts_sink_abort_rejected`, carrying
    //  [controller, reason].)
    let payload = pair_payload(scope, controller, reason).expect("payload");
    support::react(
        scope,
        &cancel_promise,
        Some((ts_sink_abort_fulfilled, payload)),
        Some((ts_sink_abort_rejected, payload)),
    )
    .expect("react");
    // Step 8: Return _controller_.`[[finishPromise]]`.
    finish
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-sink-close-algorithm>
/// TransformStreamDefaultSinkCloseAlgorithm(stream) performs the following steps:
pub(crate) fn transform_stream_default_sink_close_algorithm<'r>(
    scope: &'r Scope<'_>,
    stream: &TransformStream<'_>,
) -> Promise<'r> {
    // Step 1: Let _controller_ be _stream_.`[[controller]]`.
    let controller = ts_controller(scope, stream);
    // Step 2: If _controller_.`[[finishPromise]]` is not undefined, return
    //         _controller_.`[[finishPromise]]`.
    if controller.data().finish_promise.is_some() {
        return ts_finish_promise(scope, &controller);
    }
    // Step 3: Let _readable_ be _stream_.`[[readable]]`. (Used by the reaction.)
    // Step 4: Let _controller_.`[[finishPromise]]` be a new promise.
    let finish = Promise::new_pending(scope).expect("new promise");
    controller.data_mut().finish_promise.set(finish);
    // Step 5: Let _flushPromise_ be the result of performing _controller_.`[[flushAlgorithm]]`.
    //         The from-transformer flush callback is invoked with « controller ».
    let flush_algorithm = controller.data().flush_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let flush_promise =
        support::invoke_promise_algorithm(scope, flush_algorithm, receiver, &[controller]);
    // Step 6: Perform ! `TransformStreamDefaultControllerClearAlgorithms`(_controller_).
    transform_stream_default_controller_clear_algorithms(&controller);
    // Step 7: `React` to _flushPromise_: If _flushPromise_ was fulfilled, then: If
    //         _readable_.`[[state]]` is "`errored`", `reject` _controller_.`[[finishPromise]]` with
    //         _readable_.`[[storedError]]`. Otherwise: Perform !
    //         `DefaultControllerClose`(_readable_.`[[controller]]`). `Resolve`
    //         _controller_.`[[finishPromise]]` with undefined. If _flushPromise_ was rejected with
    //         reason _r_, then: Perform !
    //         `DefaultControllerError`(_readable_.`[[controller]]`, _r_). `Reject`
    //         _controller_.`[[finishPromise]]` with _r_.
    // (Implemented by `ts_sink_close_fulfilled` / `ts_sink_close_rejected`.)
    support::react(
        scope,
        &flush_promise,
        Some((ts_sink_close_fulfilled, controller)),
        Some((ts_sink_close_rejected, controller)),
    )
    .expect("react");
    // Step 8: Return _controller_.`[[finishPromise]]`.
    finish
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-source-cancel>
/// TransformStreamDefaultSourceCancelAlgorithm(stream, reason) performs the following steps:
pub(crate) fn transform_stream_default_source_cancel_algorithm<'r>(
    scope: &'r Scope<'_>,
    stream: &TransformStream<'_>,
    reason: HandleValue<'_>,
) -> Promise<'r> {
    // Step 1: Let _controller_ be _stream_.`[[controller]]`.
    let controller = ts_controller(scope, stream);
    // Step 2: If _controller_.`[[finishPromise]]` is not undefined, return
    //         _controller_.`[[finishPromise]]`.
    if controller.data().finish_promise.is_some() {
        return ts_finish_promise(scope, &controller);
    }
    // Step 3: Let _writable_ be _stream_.`[[writable]]`. (Used by the reaction.)
    // Step 4: Let _controller_.`[[finishPromise]]` be a new promise.
    let finish = Promise::new_pending(scope).expect("new promise");
    controller.data_mut().finish_promise.set(finish);
    // Step 5: Let _cancelPromise_ be the result of performing _controller_.`[[cancelAlgorithm]]`,
    //         passing _reason_.
    let cancel_algorithm = controller.data().cancel_algorithm.get(scope);
    let receiver = controller.data().algorithm_receiver.get(scope);
    let cancel_promise =
        support::invoke_promise_algorithm(scope, cancel_algorithm, receiver, &[reason]);
    // Step 6: Perform ! `TransformStreamDefaultControllerClearAlgorithms`(_controller_).
    transform_stream_default_controller_clear_algorithms(&controller);
    // Step 7: `React` to _cancelPromise_: If _cancelPromise_ was fulfilled, then: If
    //         _writable_.`[[state]]` is "`errored`", `reject` _controller_.`[[finishPromise]]` with
    //         _writable_.`[[storedError]]`. Otherwise: Perform !
    //         `WritableStreamDefaultControllerErrorIfNeeded`(_writable_.`[[controller]]`,
    //         _reason_). Perform ! `TransformStreamUnblockWrite`(_stream_). `Resolve`
    //         _controller_.`[[finishPromise]]` with undefined. If _cancelPromise_ was rejected with
    //         reason _r_, then: Perform !
    //         `WritableStreamDefaultControllerErrorIfNeeded`(_writable_.`[[controller]]`, _r_).
    //         Perform ! `TransformStreamUnblockWrite`(_stream_). `Reject`
    //         _controller_.`[[finishPromise]]` with _r_.
    // (Implemented by `ts_source_cancel_fulfilled` / `ts_source_cancel_rejected`, carrying
    //  [controller, reason].)
    let payload = pair_payload(scope, controller, reason).expect("payload");
    support::react(
        scope,
        &cancel_promise,
        Some((ts_source_cancel_fulfilled, payload)),
        Some((ts_source_cancel_rejected, payload)),
    )
    .expect("react");
    // Step 8: Return _controller_.`[[finishPromise]]`.
    finish
}

/// <https://streams.spec.whatwg.org/#transform-stream-default-source-pull>
/// TransformStreamDefaultSourcePullAlgorithm(stream) performs the following steps:
pub(crate) fn transform_stream_default_source_pull_algorithm<'r>(
    scope: &'r Scope<'_>,
    stream: &TransformStream<'_>,
) -> Promise<'r> {
    // Step 1: Assert: _stream_.`[[backpressure]]` is true.
    debug_assert!(stream.data().backpressure);
    // Step 2: Assert: _stream_.`[[backpressureChangePromise]]` is not undefined.
    debug_assert!(stream.data().backpressure_change_promise.is_some());
    // Step 3: Perform ! `TransformStreamSetBackpressure`(_stream_, false).
    transform_stream_set_backpressure(scope, stream, false);
    // Step 4: Return _stream_.`[[backpressureChangePromise]]`.
    stream
        .data()
        .backpressure_change_promise
        .get(scope)
        .expect("backpressureChangePromise is set")
}
