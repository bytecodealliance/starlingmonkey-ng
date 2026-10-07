// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>

use super::algorithms;
use super::transform_stream_default_controller::TransformStreamDefaultController;
use super::transform_stream_default_controller::TransformStreamDefaultControllerImpl;
use super::transformer::Transformer;
use crate::algorithms::extract_high_water_mark;
use crate::algorithms::extract_size_algorithm;
use crate::queuing::QueuingStrategy;
use crate::readable::readable_stream::ReadableStreamImpl;
use crate::readable::ReadableStream;
use crate::writable::writable_stream::WritableStreamImpl;
use crate::writable::WritableStream;
use crate::AlgorithmArg;
use core_runtime::{webidl_interface, webidl_methods};
use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Promise;

/// <https://streams.spec.whatwg.org/#ts-class>
#[webidl_interface]
pub struct TransformStream {
    /// <https://streams.spec.whatwg.org/#transformstream-backpressure>
    /// Whether there was backpressure on [[readable]] the last time it was observed
    ///
    /// The spec initializes this to undefined, then `TransformStreamSetBackpressure`
    /// sets it; we use a strictly boolean value (allowed by the spec note),
    /// initialized to `false` so the first `SetBackpressure(true)` differs.
    pub(crate) backpressure: bool,
    /// <https://streams.spec.whatwg.org/#transformstream-backpressurechangepromise>
    /// A promise which is fulfilled and replaced every time the value of [[backpressure]] changes
    pub(crate) backpressure_change_promise: Option<Heap<js::promise::Promise>>,
    /// <https://streams.spec.whatwg.org/#transformstream-controller>
    /// A TransformStreamDefaultController created with the ability to control [[readable]] and
    /// [[writable]]
    ///
    /// `Option`: `InitializeTransformStream` sets it to undefined, then
    /// `SetUpTransformStreamDefaultController` populates it.
    pub(crate) controller: Option<Heap<TransformStreamDefaultControllerImpl>>,
    /// <https://streams.spec.whatwg.org/#transformstream-readable>
    /// The ReadableStream instance controlled by this object
    pub(crate) readable: Heap<ReadableStreamImpl>,
    /// <https://streams.spec.whatwg.org/#transformstream-writable>
    /// The WritableStream instance controlled by this object
    pub(crate) writable: Heap<WritableStreamImpl>,
}

#[webidl_methods]
impl TransformStream {
    /// <https://streams.spec.whatwg.org/#ts-constructor>
    #[constructor]
    fn new(
        &self,
        scope: &Scope<'_>,
        transformer: Option<HandleValue<'_>>,
        writable_strategy: Option<QueuingStrategy>,
        readable_strategy: Option<QueuingStrategy>,
    ) -> Result<(), ExnThrown> {
        // Step 1: If _transformer_ is missing, set it to null. (A missing/undefined argument is
        //         `None`; taken as `any` so an explicit `null` can be rejected.)
        // Step 2: Let _transformerDict_ be _transformer_, `converted to an IDL value` of type
        //         ``Transformer``. We cannot declare the _transformer_ argument as having the
        //         ``Transformer`` type directly, because doing so would lose the reference to the
        //         original object. We need to retain the object so we can `invoke` the various
        //         methods on it.
        let transformer_value = match transformer {
            None => HandleValue::undefined(),
            Some(v) => {
                if !v.is_object() {
                    return Err(js::error::throw_binding_error(
                        scope,
                        &js::class::constructor_site::<TransformStreamImpl>(),
                        js::error::BindingError::InvalidArgument {
                            index: 0,
                            value: v,
                            message: c"TransformStream constructor: transformer must be an object",
                        },
                    ));
                }
                v
            }
        };
        let transformer_dict = Transformer::from_jsval_throwing(scope, transformer_value, ())?;
        // Step 3: If _transformerDict_["``readableType``"] `exists`, throw a ``RangeError``
        //         exception.
        if let Some(readable_type) = transformer_dict.readable_type {
            return Err(crate::algorithms::throw_invalid_value(
                scope,
                "TransformStream",
                js::error::JSExnType::JSEXN_RANGEERR,
                c"Invalid readableType specified",
                "transformer.readableType",
                readable_type,
            ));
        }
        // Step 4: If _transformerDict_["``writableType``"] `exists`, throw a ``RangeError``
        //         exception.
        if let Some(writable_type) = transformer_dict.writable_type {
            return Err(crate::algorithms::throw_invalid_value(
                scope,
                "TransformStream",
                js::error::JSExnType::JSEXN_RANGEERR,
                c"Invalid writableType specified",
                "transformer.writableType",
                writable_type,
            ));
        }
        // Step 5: Let _readableHighWaterMark_ be ? `ExtractHighWaterMark`(_readableStrategy_, 0).
        let readable_high_water_mark = extract_high_water_mark(scope, &readable_strategy, 0.0)?;
        // Step 6: Let _readableSizeAlgorithm_ be ! `ExtractSizeAlgorithm`(_readableStrategy_).
        let readable_size_algorithm = extract_size_algorithm(scope, &readable_strategy)?;
        // Step 7: Let _writableHighWaterMark_ be ? `ExtractHighWaterMark`(_writableStrategy_, 1).
        let writable_high_water_mark = extract_high_water_mark(scope, &writable_strategy, 1.0)?;
        // Step 8: Let _writableSizeAlgorithm_ be ! `ExtractSizeAlgorithm`(_writableStrategy_).
        let writable_size_algorithm = extract_size_algorithm(scope, &writable_strategy)?;
        // Step 9: Let _startPromise_ be `a new promise`.
        let start_promise = Promise::new_pending(scope)?;
        // Step 10: Perform ! `InitializeTransformStream`(`this`, _startPromise_,
        //          _writableHighWaterMark_, _writableSizeAlgorithm_, _readableHighWaterMark_,
        //          _readableSizeAlgorithm_).
        algorithms::initialize_transform_stream(
            scope,
            self,
            &start_promise,
            writable_high_water_mark,
            writable_size_algorithm,
            readable_high_water_mark,
            readable_size_algorithm,
        )?;
        // Step 11: Perform ? `SetUpTransformStreamDefaultControllerFromTransformer`(`this`,
        //          _transformer_, _transformerDict_).
        algorithms::set_up_transform_stream_default_controller_from_transformer(
            scope,
            self,
            transformer_value,
            &transformer_dict,
        )?;
        // Step 12: If _transformerDict_["``start``"] `exists`, then `resolve` _startPromise_ with
        //          the result of `invoking` _transformerDict_["``start``"] with argument list «
        //          `this`.`[[controller]]` » and `callback this value` _transformer_.
        if let Some(start) = transformer_dict.start.as_ref() {
            let controller: TransformStreamDefaultController<'_> = self
                .data()
                .controller
                .get(scope)
                .expect("controller is set");
            let result = js::Function::call(scope, transformer_value, start, &[controller])?;
            start_promise.resolve(scope, result)?;
        } else {
            // Step 13: Otherwise, `resolve` _startPromise_ with undefined.
            start_promise.resolve(scope, HandleValue::undefined())?;
        }
        // Not a spec step: mark an identity transform by linking its writable to its readable,
        // so a native byte source piped into the writable can be propagated to the readable
        // directly. Only a transform that runs no content code per chunk qualifies, so the
        // shortcut has no content-visible effect: a transformer with any callback
        // (`transform`/`start`/`flush`/`cancel` — which could enqueue extra chunks, or expect a
        // cancel hook), or either strategy carrying a `size` function (invoked once per chunk on
        // the non-shortcut path), disqualifies it.
        let no_strategy_size = readable_strategy
            .as_ref()
            .and_then(|s| s.size.as_ref())
            .is_none()
            && writable_strategy
                .as_ref()
                .and_then(|s| s.size.as_ref())
                .is_none();
        if no_strategy_size
            && transformer_dict.transform.is_none()
            && transformer_dict.start.is_none()
            && transformer_dict.flush.is_none()
            && transformer_dict.cancel.is_none()
        {
            self.data()
                .writable
                .get(scope)
                .data_mut()
                .identity_transform
                .set(*self);
        }
        Ok(())
    }

    /// A new `TransformStream`, set up with the native algorithms `transform`, `flush` and
    /// `cancel`. A thrown exception rejects an algorithm's promise, and a result that isn't a
    /// promise resolves it. `transform` is called with the chunk and the stream's
    /// [`TransformStreamDefaultController`], `flush` with the controller, and `cancel` with the
    /// reason.
    ///
    /// <https://streams.spec.whatwg.org/#transformstream-set-up>
    pub fn new_native<'r>(
        scope: &'r Scope<'_>,
        transform: AlgorithmArg<'_>,
        flush: AlgorithmArg<'_>,
        cancel: AlgorithmArg<'_>,
    ) -> Result<TransformStream<'r>, ExnThrown> {
        let stream = js::class::create_instance_with::<TransformStreamImpl>(scope, |_| {
            TransformStreamImpl::default()
        })?;
        // Step 1: Let _writableHighWaterMark_ be 1.
        let writable_high_water_mark = 1.0;
        // Step 2: Let _writableSizeAlgorithm_ be an algorithm that returns 1.
        // Step 4: Let _readableSizeAlgorithm_ be an algorithm that returns 1.
        // An undefined size algorithm returns 1.
        let size_algorithm = HandleValue::undefined();
        // Step 3: Let _readableHighWaterMark_ be 0.
        let readable_high_water_mark = 0.0;
        // Step 5: Let _transformAlgorithmWrapper_ be an algorithm that runs these steps given a
        //         value _chunk_:
        // Step 5.1: Let _result_ be the result of running _transformAlgorithm_ given _chunk_. If
        //           this throws an exception _e_, return `a promise rejected with` _e_.
        // Step 5.2: If _result_ is a ``Promise``, then return _result_.
        // Step 5.3: Return `a promise resolved with` undefined.
        // Step 6: Let _flushAlgorithmWrapper_ be an algorithm that runs these steps:
        // Step 6.1: Let _result_ be the result of running _flushAlgorithm_, if _flushAlgorithm_
        //           was given, or null otherwise. If this throws an exception _e_, return `a
        //           promise rejected with` _e_.
        // Step 6.2: If _result_ is a ``Promise``, then return _result_.
        // Step 6.3: Return `a promise resolved with` undefined.
        // Step 7: Let _cancelAlgorithmWrapper_ be an algorithm that runs these steps given a value
        //         _reason_:
        // Step 7.1: Let _result_ be the result of running _cancelAlgorithm_ given _reason_, if
        //           _cancelAlgorithm_ was given, or null otherwise. If this throws an exception
        //           _e_, return `a promise rejected with` _e_.
        // Step 7.2: If _result_ is a ``Promise``, then return _result_.
        // Step 7.3: Return `a promise resolved with` undefined.
        // `support::invoke_promise_algorithm` runs native algorithms as these wrappers do.
        // Step 8: Let _startPromise_ be `a promise resolved with` undefined.
        let start_promise = Promise::new_resolved_with_value(scope, HandleValue::undefined())?;
        // Step 9: Perform ! `InitializeTransformStream`(_stream_, _startPromise_,
        //         _writableHighWaterMark_, _writableSizeAlgorithm_, _readableHighWaterMark_,
        //         _readableSizeAlgorithm_).
        algorithms::initialize_transform_stream(
            scope,
            &stream,
            &start_promise,
            writable_high_water_mark,
            size_algorithm,
            readable_high_water_mark,
            size_algorithm,
        )?;
        // Step 10: Let _controller_ be a `new` ``TransformStreamDefaultController``.
        let controller = TransformStreamDefaultController::new(scope, stream)?;
        // Step 11: Perform ! `SetUpTransformStreamDefaultController`(_stream_, _controller_,
        //          _transformAlgorithmWrapper_, _flushAlgorithmWrapper_,
        //          _cancelAlgorithmWrapper_).
        algorithms::set_up_transform_stream_default_controller(
            scope,
            &stream,
            &controller,
            transform,
            flush,
            cancel,
            HandleValue::undefined(),
        );
        Ok(stream)
    }

    /// Enqueue `chunk` into the readable side of a stream from [`TransformStream::new_native`].
    /// Throws if the readable side is closed or errored, or if its size algorithm throws.
    ///
    /// <https://streams.spec.whatwg.org/#transformstream-enqueue>
    pub fn enqueue(&self, scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<(), ExnThrown> {
        // To `enqueue` the JavaScript value _chunk_ into a ``TransformStream`` _stream_, perform
        // ! `TransformStreamDefaultControllerEnqueue`(_stream_.`[[controller]]`, _chunk_).
        let controller: TransformStreamDefaultController<'_> = self
            .data()
            .controller
            .get(scope)
            .expect("controller is set");
        algorithms::transform_stream_default_controller_enqueue(scope, &controller, chunk)
    }

    /// <https://streams.spec.whatwg.org/#ts-readable>
    #[getter]
    pub fn readable<'r>(&self, scope: &'r Scope<'_>) -> ReadableStream<'r> {
        // Step 1: Return `this`.`[[readable]]`.
        self.data().readable.get(scope)
    }

    /// <https://streams.spec.whatwg.org/#ts-writable>
    #[getter]
    pub fn writable<'r>(&self, scope: &'r Scope<'_>) -> WritableStream<'r> {
        // Step 1: Return `this`.`[[writable]]`.
        self.data().writable.get(scope)
    }
}
