// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>

use super::algorithms;
use super::default_controller::WritableStreamDefaultControllerImpl;
use super::default_writer::WritableStreamDefaultWriter;
use super::default_writer::WritableStreamDefaultWriterImpl;
use super::underlying_sink::UnderlyingSink;
use crate::algorithms::extract_high_water_mark;
use crate::algorithms::extract_size_algorithm;
use crate::queuing::QueuingStrategy;
use crate::writable::WritableStreamDefaultController;
use core_runtime::{webidl_interface, webidl_methods};
use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Promise;
use web_globals::events::algorithms::ScriptStackState;

/// A traced slot holding a `Promise`.
///
/// `must_root`: the slot wraps a `Heap<Promise>`, so crown rejects holding a
/// `PromiseSlot` by value in a plain local across an allocation — the
/// untraced-`Heap`-across-GC hazard. While it lives inside a traced container (a
/// stream's write-request queue or an in-flight/close slot) its `Heap` is traced
/// normally; once moved out via `take`/`pop_front` it must be consumed
/// immediately (`into_promise`) or kept in a `RootedTraceableBox`.
#[js::must_root]
#[derive(core_runtime::Traceable, Default, js::ScopeRoot)]
pub(crate) struct PromiseSlot(Heap<js::promise::Promise>);

impl PromiseSlot {
    /// Wrap a promise in a slot.
    pub(crate) fn new(promise: Promise<'_>) -> Self {
        Self(Heap::from(promise))
    }
}

impl<'s> StackPromiseSlot<'s> {
    /// Consume the rooted slot, returning its promise.
    pub(crate) fn into_promise(self) -> Promise<'s> {
        self.0
    }
}

/// A pending abort request.
///
/// <https://streams.spec.whatwg.org/#writablestream-pendingabortrequest>
///
/// `must_root`: holds `Heap` fields, so crown rejects holding it by value across
/// an allocation. It lives in the stream's traced `[[pendingAbortRequest]]`
/// slot; once moved out it must be consumed immediately or kept rooted.
#[js::must_root]
#[derive(core_runtime::Traceable, Default, js::ScopeRoot)]
pub(crate) struct PendingAbortRequest {
    /// The promise returned by the `abort()` call that produced this request.
    pub(crate) promise: Heap<js::promise::Promise>,
    /// The abort reason.
    pub(crate) reason: Heap<Value>,
    /// Whether the stream was already erroring when the abort was requested.
    #[no_trace]
    pub(crate) was_already_erroring: bool,
}

impl<'s> StackPendingAbortRequest<'s> {
    /// Consume the rooted request, returning its `promise` (and dropping the
    /// reason).
    pub(crate) fn into_promise(self) -> Promise<'s> {
        self.promise
    }
}

/// <https://streams.spec.whatwg.org/#ws-class>
#[webidl_interface]
pub struct WritableStream {
    /// <https://streams.spec.whatwg.org/#writablestream-backpressure>
    /// A boolean indicating the backpressure signal set by the controller
    pub(crate) backpressure: bool,
    /// <https://streams.spec.whatwg.org/#writablestream-closerequest>
    /// The promise returned from the writer’s close() method
    pub(crate) close_request: Option<PromiseSlot>,
    /// <https://streams.spec.whatwg.org/#writablestream-controller>
    /// A WritableStreamDefaultController created with the ability to control the state and queue of
    /// this stream
    ///
    /// `Option` because the stream is created before `SetUp...Controller` wires
    /// it; always `Some` thereafter.
    pub(crate) controller: Option<Heap<WritableStreamDefaultControllerImpl>>,
    /// <https://streams.spec.whatwg.org/#writablestream-inflightwriterequest>
    /// A slot set to the promise for the current in-flight write operation while the underlying
    /// sink’s write algorithm is executing and has not yet fulfilled, used to prevent reentrant
    /// calls
    pub(crate) in_flight_write_request: Option<PromiseSlot>,
    /// <https://streams.spec.whatwg.org/#writablestream-inflightcloserequest>
    /// A slot set to the promise for the current in-flight close operation while the underlying
    /// sink’s close algorithm is executing and has not yet fulfilled, used to prevent the abort()
    /// method from interrupting close
    pub(crate) in_flight_close_request: Option<PromiseSlot>,
    /// <https://streams.spec.whatwg.org/#writablestream-pendingabortrequest>
    /// A pending abort request
    pub(crate) pending_abort_request: Option<PendingAbortRequest>,
    /// <https://streams.spec.whatwg.org/#writablestream-state>
    /// A string containing the stream’s current state, used internally; one of "writable",
    /// "closed", "erroring", or "errored"
    #[no_trace]
    pub(crate) state: WritableStreamState,
    /// <https://streams.spec.whatwg.org/#writablestream-storederror>
    /// A value indicating how the stream failed, to be given as a failure reason or exception when
    /// trying to operate on the stream while in the "errored" state
    pub(crate) stored_error: Heap<Value>,
    /// <https://streams.spec.whatwg.org/#writablestream-writer>
    /// A WritableStreamDefaultWriter instance, if the stream is locked to a writer, or undefined if
    /// it is not
    pub(crate) writer: Option<Heap<WritableStreamDefaultWriterImpl>>,
    /// <https://streams.spec.whatwg.org/#writablestream-writerequests>
    /// A list of promises representing the stream’s internal queue of write requests not yet
    /// processed by the underlying sink
    pub(crate) write_requests: std::collections::VecDeque<PromiseSlot>,
    /// If this writable is the writable end of an identity `TransformStream`,
    /// this holds that transform. `pipeTo` uses it to propagate a native byte
    /// source through the identity transform, and to let that source defer
    /// reading until the transform's readable end is read.
    /// `None` for a standalone writable or a non-identity transform.
    pub(crate) identity_transform:
        Option<Heap<crate::transform::transform_stream::TransformStreamImpl>>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WritableStreamState {
    #[default]
    Writable,
    Closed,
    Erroring,
    Errored,
}

#[webidl_methods]
impl WritableStream {
    /// <https://streams.spec.whatwg.org/#ws-constructor>
    #[constructor]
    pub fn new(
        &self,
        scope: &Scope<'_>,
        underlying_sink: Option<HandleValue<'_>>,
        strategy: Option<QueuingStrategy>,
    ) -> Result<(), ExnThrown> {
        // Step 1: If _underlyingSink_ is missing, set it to null.
        //         (A missing/undefined argument is `None`; taken as `any` so an explicit `null`
        //         can be rejected — the WebIDL `object` type is not nullable.)
        // Step 2: Let _underlyingSinkDict_ be _underlyingSink_, `converted to an IDL value` of type
        //         ``UnderlyingSink``. We cannot declare the _underlyingSink_ argument as having the
        //         ``UnderlyingSink`` type directly, because doing so would lose the reference to
        //         the original object. We need to retain the object so we can `invoke` the various
        //         methods on it.
        let underlying_sink_value = match underlying_sink {
            None => HandleValue::undefined(),
            Some(v) => {
                if !v.is_object() {
                    return Err(js::error::throw_binding_error(
                        scope,
                        &js::class::constructor_site::<WritableStreamImpl>(),
                        js::error::BindingError::InvalidArgument {
                            index: 0,
                            value: v,
                            message:
                                c"WritableStream constructor: underlyingSink must be an object",
                        },
                    ));
                }
                v
            }
        };
        let underlying_sink_dict =
            UnderlyingSink::from_jsval_throwing(scope, underlying_sink_value, ())?;
        // Step 3: If _underlyingSinkDict_["``type``"] `exists`, throw a ``RangeError`` exception.
        //         This is to allow us to add new potential types in the future, without
        //         backward-compatibility concerns.
        if let Some(sink_type) = underlying_sink_dict.r#type {
            return Err(crate::algorithms::throw_invalid_value(
                scope,
                "WritableStream",
                js::error::JSExnType::JSEXN_RANGEERR,
                c"Invalid type is specified",
                "type",
                sink_type,
            ));
        }
        // Step 4: Perform ! `InitializeWritableStream`(`this`).
        algorithms::initialize_writable_stream(self);
        // Step 5: Let _sizeAlgorithm_ be ! `ExtractSizeAlgorithm`(_strategy_).
        let size_algorithm = extract_size_algorithm(scope, &strategy)?;
        // Step 6: Let _highWaterMark_ be ? `ExtractHighWaterMark`(_strategy_, 1).
        let high_water_mark = extract_high_water_mark(scope, &strategy, 1.0)?;
        // Step 7: Perform ? `SetUpWritableStreamDefaultControllerFromUnderlyingSink`(`this`,
        //         _underlyingSink_, _underlyingSinkDict_, _highWaterMark_, _sizeAlgorithm_).
        algorithms::set_up_writable_stream_default_controller_from_underlying_sink(
            scope,
            self,
            underlying_sink_value,
            &underlying_sink_dict,
            high_water_mark,
            size_algorithm,
        )
    }

    /// <https://streams.spec.whatwg.org/#ws-locked>
    #[getter]
    fn locked(&self) -> bool {
        // Step 1: Return ! `IsWritableStreamLocked`(`this`).
        algorithms::is_writable_stream_locked(self)
    }

    /// <https://streams.spec.whatwg.org/#writablestream-abort>
    #[method(length = 0)]
    fn abort<'r>(
        &self,
        scope: &'r Scope<'_>,
        reason: HandleValue<'_>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: If ! `IsWritableStreamLocked`(`this`) is true, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if algorithms::is_writable_stream_locked(self) {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "WritableStream",
                c"Cannot abort a stream that already has a writer",
            ));
        }
        // Step 2: Return ! `WritableStreamAbort`(`this`, _reason_).
        Ok(algorithms::writable_stream_abort(
            scope,
            self,
            reason,
            ScriptStackState::NonEmpty,
        ))
    }

    /// <https://streams.spec.whatwg.org/#writablestream-close>
    #[method]
    pub fn close<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: If ! `IsWritableStreamLocked`(`this`) is true, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if algorithms::is_writable_stream_locked(self) {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "WritableStream",
                c"Cannot close a stream that already has a writer",
            ));
        }
        // Step 2: If ! `WritableStreamCloseQueuedOrInFlight`(`this`) is true, return `a promise
        //         rejected with` a ``TypeError`` exception.
        if algorithms::writable_stream_close_queued_or_in_flight(self) {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "WritableStream",
                c"Cannot close an already-closing stream",
            ));
        }
        // Step 3: Return ! `WritableStreamClose`(`this`).
        Ok(algorithms::writable_stream_close(scope, self))
    }

    /// <https://streams.spec.whatwg.org/#ws-get-writer>
    #[method]
    pub fn get_writer<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Result<WritableStreamDefaultWriter<'r>, ExnThrown> {
        // Step 1: Return ? `AcquireDefaultWriter`(`this`).
        algorithms::acquire_writable_stream_default_writer(scope, self)
    }

    pub(crate) fn controller<'r>(
        &'r self,
        scope: &'r Scope<'_>,
    ) -> WritableStreamDefaultController<'r> {
        self.data()
            .controller
            .get(scope)
            .expect("stream has a controller")
    }
}
