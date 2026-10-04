// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>

use super::async_iterator::ReadableStreamAsyncIterator;
use super::enums::ReadableStreamType;
use super::options::{
    ReadableStreamGetReaderOptions, ReadableStreamIteratorOptions, StreamPipeOptions,
};
use super::underlying_source::UnderlyingSource;
use super::unions::{
    HeapReadableStreamController, HeapReadableStreamReader, ReadableStreamController,
    ReadableStreamReader,
};
use super::{algorithms, ReadableByteStreamController};
use crate::algorithms::{extract_high_water_mark, extract_size_algorithm};
use crate::queuing::QueuingStrategy;
use crate::readable::algorithms::{
    acquire_readable_stream_default_reader, create_readable_stream,
    readable_byte_stream_controller_error, readable_byte_stream_tee,
    readable_stream_default_controller_close, readable_stream_default_controller_enqueue,
    readable_stream_default_controller_error, readable_stream_default_tee,
};
use crate::readable::ReadableStreamDefaultController;
use crate::support::AlgorithmArg;
use crate::transform::readable_writable_pair::ReadableWritablePair;
use crate::writable::WritableStream;
use core_runtime::{webidl_interface, webidl_methods};
use js::prelude::ToJSVal;
use js::{
    conversion::FromJSVal, error::ExnThrown, gc::handle::Heap, gc::handle::OptionHeapExt,
    gc::scope::Scope, native::Value, prelude::HandleValue, Object, Promise, Uint8Array,
};
use web_globals::signals::AbortSignal;

/// Extract `(preventClose, preventAbort, preventCancel, signal)` from a
/// `StreamPipeOptions` dictionary, defaulting to all-false and an undefined
/// signal when the `options` argument was not supplied.
fn pipe_options<'r>(
    options: &Option<StreamPipeOptions<'r>>,
) -> (bool, bool, bool, Option<AbortSignal<'r>>) {
    match options {
        Some(o) => (o.prevent_close, o.prevent_abort, o.prevent_cancel, o.signal),
        None => (false, false, false, None),
    }
}

/// <https://streams.spec.whatwg.org/#rs-class>
#[webidl_interface]
pub struct ReadableStream {
    /// <https://streams.spec.whatwg.org/#readablestream-controller>
    /// A ReadableStreamDefaultController or ReadableByteStreamController created with the ability to
    /// control the state and queue of this stream
    ///
    /// `None` until `SetUpReadableStreamDefaultController` or `SetUpReadableByteStreamController`
    /// runs.
    pub(crate) controller: Option<HeapReadableStreamController>,
    /// <https://streams.spec.whatwg.org/#readablestream-disturbed>
    /// A boolean flag set to true when the stream has been read from or canceled
    pub(crate) disturbed: bool,
    /// <https://streams.spec.whatwg.org/#readablestream-reader>
    /// A DefaultReader or BYOBReader instance, if the stream is locked
    /// to a reader, or undefined if it is not
    pub(crate) reader: Option<HeapReadableStreamReader>,
    /// <https://streams.spec.whatwg.org/#readablestream-state>
    /// A string containing the stream’s current state, used internally; one of "readable",
    /// "closed", or "errored"
    #[no_trace]
    pub(crate) state: ReadableStreamState,
    /// <https://streams.spec.whatwg.org/#readablestream-storederror>
    /// A value indicating how the stream failed, to be given as a failure reason or exception when
    /// trying to operate on an errored stream
    pub(crate) stored_error: Heap<Value>,
    /// A native byte source backing this stream, set for streams created via
    /// [`ReadableStream::new_native`] and propagated through an identity
    /// `TransformStream` by `pipeTo`. A private slot with no content-visible
    /// effect: it lets a consumer recognize a stream backed by its own native
    /// source. `None` otherwise.
    pub(crate) native_byte_source: Option<Heap<js::object::Object>>,
    /// The identity `TransformStream` this stream is piped into, recorded by
    /// `pipeTo` when this stream is backed by a native byte source. Lets that
    /// source hold off reading until the transform's readable end is read; see
    /// [`ReadableStream::deferred_demand`]. `None` otherwise.
    pub(crate) piped_to_identity_transform:
        Option<Heap<crate::transform::transform_stream::TransformStreamImpl>>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ReadableStreamState {
    #[default]
    Readable,
    Closed,
    Errored,
}

#[webidl_methods]
impl ReadableStream {
    /// <https://streams.spec.whatwg.org/#rs-constructor>
    #[constructor]
    pub fn new(
        &self,
        scope: &Scope<'_>,
        underlying_source: Option<HandleValue<'_>>,
        strategy: Option<QueuingStrategy>,
    ) -> Result<(), ExnThrown> {
        // Step 1: If _underlyingSource_ is missing, set it to null.
        //         (A missing/undefined argument is `None`. The argument is taken as `any` rather
        //         than `object` so an explicit `null` can be rejected: the WebIDL `object` type is
        //         not nullable.)
        // Step 2: Let _underlyingSourceDict_ be _underlyingSource_, `converted to an IDL value` of
        //         type ``UnderlyingSource``. We cannot declare the _underlyingSource_ argument as
        //         having the ``UnderlyingSource`` type directly, because doing so would lose the
        //         reference to the original object. We need to retain the object so we can `invoke`
        //         the various methods on it.
        let underlying_source_value = match underlying_source {
            None => HandleValue::undefined(),
            Some(v) => {
                // The WebIDL `object underlyingSource` argument is non-nullable, so `null` (and any
                // non-object such as a number or string) throws a `TypeError`.
                if !v.is_object() {
                    return Err(js::error::throw_type_error(
                        scope,
                        c"ReadableStream constructor: underlyingSource must be an object",
                    ));
                }
                v
            }
        };
        let underlying_source_dict =
            UnderlyingSource::from_jsval_throwing(scope, underlying_source_value, ())?;
        // Step 3: Perform ! `InitializeReadableStream`(`this`).
        algorithms::initialize_readable_stream(self);
        // Step 4: If _underlyingSourceDict_["``type``"] is "``bytes``": ...
        if matches!(
            underlying_source_dict.r#type,
            Some(ReadableStreamType::Bytes)
        ) {
            // If _strategy_["``size``"] `exists`, throw a ``RangeError`` exception. Let
            // _highWaterMark_ be ? `ExtractHighWaterMark`(_strategy_, 0). Perform ?
            // `SetUpByteStreamControllerFromUnderlyingSource`(`this`, _underlyingSource_,
            // _underlyingSourceDict_, _highWaterMark_).
            if strategy.as_ref().is_some_and(|s| s.size.is_some()) {
                return Err(js::error::throw_range_error(
                    scope,
                    c"a byte stream's queuing strategy must not have a size function",
                ));
            }
            let high_water_mark = extract_high_water_mark(scope, &strategy, 0.0)?;
            return algorithms::set_up_readable_byte_stream_controller_from_underlying_source(
                scope,
                self,
                underlying_source_value,
                &underlying_source_dict,
                high_water_mark,
            );
        }
        // Step 5: Otherwise, Assert: _underlyingSourceDict_["``type``"] does not `exist`. Let
        //         _sizeAlgorithm_ be ! `ExtractSizeAlgorithm`(_strategy_). Let _highWaterMark_ be ?
        //         `ExtractHighWaterMark`(_strategy_, 1). Perform ?
        //         `SetUpDefaultControllerFromUnderlyingSource`(`this`,
        //         _underlyingSource_, _underlyingSourceDict_, _highWaterMark_, _sizeAlgorithm_).
        let size_algorithm = extract_size_algorithm(scope, &strategy)?;
        let high_water_mark = extract_high_water_mark(scope, &strategy, 1.0)?;
        algorithms::set_up_readable_stream_default_controller_from_underlying_source(
            scope,
            self,
            underlying_source_value,
            &underlying_source_dict,
            high_water_mark,
            size_algorithm,
        )
    }

    /// Create a `ReadableStream` driven by the provided `pull`/`cancel` algorithms, typically
    /// native callbacks.
    ///
    /// `pull` is called with the stream's controller as its argument when more data is wanted.
    /// `cancel` is called with the cancel reason.
    ///
    /// The stream is created with a high-water mark of `0`, so it'll pull data lazily
    /// instead of eagerly requesting the first chunk. Useful for leaving native data sources,
    /// such as external streams, untouched if no JS consumer is reading from the stream.
    ///
    /// `source` can be retrieved with [`ReadableStream::native_source`], letting a consumer
    /// recognize a stream backed by a particular native source. Pass `undefined` if there is
    /// no such object.
    pub fn new_native<'r>(
        scope: &'r Scope<'_>,
        underlying_source: impl ToJSVal<'r>,
        pull: AlgorithmArg<'r>,
        cancel: AlgorithmArg<'r>,
    ) -> Result<ReadableStream<'r>, ExnThrown> {
        let underlying_source = underlying_source.to_jsval_throwing(scope)?;
        let stream = create_readable_stream(
            scope,
            AlgorithmArg::None,
            pull,
            cancel,
            0.0,
            HandleValue::undefined(),
        )?;
        if let Ok(source) = js::Object::from_value(scope, underlying_source.get()) {
            stream.set_native_source(&source);
        }
        Ok(stream)
    }

    /// Create a readable byte stream whose `pull` and `cancel` algorithms are native, as
    /// [`ReadableStream::new_native`] does for a default stream. `pull` receives the
    /// stream's [`ReadableByteStreamController`](super::ReadableByteStreamController).
    ///
    /// The stream has a high-water mark of `0` and no `autoAllocateChunkSize`, so it pulls
    /// only when a reader asks for data.
    pub fn new_native_bytes<'r>(
        scope: &'r Scope<'_>,
        underlying_source: impl ToJSVal<'r>,
        pull: AlgorithmArg<'r>,
        cancel: AlgorithmArg<'r>,
    ) -> Result<ReadableStream<'r>, ExnThrown> {
        let underlying_source = underlying_source.to_jsval_throwing(scope)?;
        let stream =
            algorithms::create_readable_byte_stream(scope, AlgorithmArg::None, pull, cancel)?;
        if let Ok(source) = Object::from_value(scope, underlying_source.get()) {
            stream.set_native_source(&source);
        }
        Ok(stream)
    }

    /// Create a `ReadableStream` from an async or sync iterable, as `ReadableStream.from`
    /// does. Throws a `TypeError` if `iterable` is not iterable.
    pub fn from_iterable<'r>(
        scope: &'r Scope<'_>,
        iterable: HandleValue<'_>,
    ) -> Result<ReadableStream<'r>, ExnThrown> {
        algorithms::readable_stream_from_iterable(scope, iterable)
    }

    /// Create a `ReadableStream` that yields `bytes` as a single `Uint8Array` chunk
    /// and is then closed. The stream uses native (no-op) start/pull/cancel
    /// algorithms and the default queuing strategy, so it behaves as an ordinary
    /// `ReadableStream` whose reader produces `Uint8Array` chunks.
    ///
    /// An empty slice produces an immediately-closed stream with no chunks.
    pub fn from_bytes<'r>(
        scope: &'r Scope<'_>,
        bytes: &[u8],
    ) -> Result<ReadableStream<'r>, ExnThrown> {
        let none = AlgorithmArg::None;
        let undef = HandleValue::undefined();
        let stream = create_readable_stream(scope, none, none, none, 1.0, undef)?;
        let controller = stream
            .default_controller(scope)
            .expect("stream has a default controller");
        if !bytes.is_empty() {
            let chunk = Uint8Array::with_data(scope, bytes)?;
            let chunk_val = scope.root_value(chunk.as_value());
            readable_stream_default_controller_enqueue(scope, &controller, chunk_val)?;
        }
        readable_stream_default_controller_close(scope, &controller);
        Ok(stream)
    }

    /// Create a readable byte stream that yields `bytes` as a single `Uint8Array` chunk and is
    /// then closed, as [`ReadableStream::from_bytes`] does for a default stream. It can be read
    /// with a BYOB reader.
    pub fn from_bytes_with_byte_reading<'r>(
        scope: &'r Scope<'_>,
        bytes: &[u8],
    ) -> Result<ReadableStream<'r>, ExnThrown> {
        let none = AlgorithmArg::None;
        let stream = algorithms::create_readable_byte_stream(scope, none, none, none)?;
        let controller = stream
            .byte_controller(scope)
            .expect("stream has a byte controller");
        if !bytes.is_empty() {
            let chunk = Uint8Array::with_data(scope, bytes)?;
            let chunk = scope.root_value(chunk.as_value());
            let chunk = js::ArrayBufferView::from_jsval_throwing(scope, chunk, ())?;
            algorithms::readable_byte_stream_controller_enqueue(scope, &controller, chunk)?;
        }
        algorithms::readable_byte_stream_controller_close(scope, &controller)?;
        Ok(stream)
    }

    /// Create a locked and disturbed `ReadableStream`.
    ///
    /// Useful to implement other builtins' behavior of internally consuming a stream
    /// which is visible to content, such as the `body` field on `Request`/`Response`.
    pub fn new_consumed<'r>(scope: &'r Scope<'_>) -> Result<ReadableStream<'r>, ExnThrown> {
        let stream = ReadableStream::from_bytes(scope, &[])?;
        stream.lock_and_disturb(scope)?;
        Ok(stream)
    }

    /// Lock `stream` to an internal reader (never released) and mark it disturbed.
    ///
    /// Useful to implement other builtins' behavior of internally consuming a stream
    /// which is visible to content, such as the `body` field on `Request`/`Response`.
    pub fn lock_and_disturb(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        if self.is_locked() {
            return Ok(());
        }
        let _reader = acquire_readable_stream_default_reader(scope, self)?;
        self.data_mut().disturbed = true;
        Ok(())
    }

    /// <https://streams.spec.whatwg.org/#readablestream-locked>
    #[getter(name = "locked")]
    pub fn is_locked(&self) -> bool {
        // Step 1: Return ! `IsReadableStreamLocked`(`this`).
        // (Inlined)
        // Step 1: If _stream_.`[[reader]]` is undefined, return false.
        // Step 2: Return true.
        self.data().reader.is_some()
    }

    /// <https://streams.spec.whatwg.org/#readablestream-cancel>
    #[method]
    pub fn cancel<'r>(
        &self,
        scope: &'r Scope<'_>,
        reason: Option<HandleValue<'r>>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: If ! `IsReadableStreamLocked`(`this`) is true, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if self.is_locked() {
            return Err(js::error::throw_type_error(
                scope,
                c"Cannot cancel a stream that already has a reader",
            ));
        }
        // Step 2: Return ! `ReadableStreamCancel`(`this`, _reason_).
        let reason = reason.unwrap_or(HandleValue::undefined());
        Ok(algorithms::readable_stream_cancel(scope, self, reason))
    }

    /// <https://streams.spec.whatwg.org/#rs-get-reader>
    #[method]
    fn get_reader<'r>(
        &self,
        scope: &'r Scope<'_>,
        options: Option<ReadableStreamGetReaderOptions>,
    ) -> Result<ReadableStreamReader<'r>, ExnThrown> {
        // Step 1: If _options_["``mode``"] does not `exist`, return ?
        //         `AcquireDefaultReader`(`this`).
        let mode = options.and_then(|o| o.mode);
        if mode.is_none() {
            let reader = algorithms::acquire_readable_stream_default_reader(scope, self)?;
            return Ok(ReadableStreamReader::Default(reader));
        }
        // Step 2: Assert: _options_["``mode``"] is "``byob``".
        // Step 3: Return ? `AcquireBYOBReader`(`this`).
        let reader = algorithms::acquire_readable_stream_byob_reader(scope, self)?;
        Ok(ReadableStreamReader::Byob(reader))
    }

    /// <https://streams.spec.whatwg.org/#rs-pipe-through>
    #[method]
    fn pipe_through<'r>(
        &self,
        scope: &'r Scope<'_>,
        transform: ReadableWritablePair<'r>,
        options: Option<StreamPipeOptions>,
    ) -> Result<ReadableStream<'r>, ExnThrown> {
        // Step 1: If ! `IsReadableStreamLocked`(`this`) is true, throw a ``TypeError`` exception.
        if self.is_locked() {
            return Err(js::error::throw_type_error(
                scope,
                c"cannot pipe from a locked ReadableStream",
            ));
        }
        // Step 2: If ! `IsWritableStreamLocked`(_transform_["``writable``"]) is true, throw a
        //         ``TypeError`` exception.
        if crate::writable::algorithms::is_writable_stream_locked(&transform.writable) {
            return Err(js::error::throw_type_error(
                scope,
                c"cannot pipe to a locked WritableStream",
            ));
        }
        // Step 3: Let _signal_ be _options_["``signal``"] if it `exists`, or undefined otherwise.
        let (prevent_close, prevent_abort, prevent_cancel, signal) = pipe_options(&options);
        // Step 4: Let _promise_ be ! `ReadableStreamPipeTo`(`this`, _transform_["``writable``"],
        //         _options_["``preventClose``"], _options_["``preventAbort``"],
        //         _options_["``preventCancel``"], _signal_).
        let promise = algorithms::readable_stream_pipe_to(
            scope,
            self,
            &transform.writable,
            prevent_close,
            prevent_abort,
            prevent_cancel,
            signal,
        )?;
        // Step 5: Set _promise_.[[PromiseIsHandled]] to true.
        let _ = promise.set_any_is_handled(scope);
        // Step 6: Return _transform_["``readable``"].
        Ok(transform.readable)
    }

    /// <https://streams.spec.whatwg.org/#rs-pipe-to>
    #[method]
    fn pipe_to<'r>(
        &self,
        scope: &'r Scope<'_>,
        destination: WritableStream<'_>,
        options: Option<StreamPipeOptions<'_>>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: If ! `IsReadableStreamLocked`(`this`) is true, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if self.is_locked() {
            return Err(js::error::throw_type_error(
                scope,
                c"cannot pipe from a locked ReadableStream",
            ));
        }
        // Step 2: If ! `IsWritableStreamLocked`(_destination_) is true, return `a promise rejected
        //         with` a ``TypeError`` exception.
        if crate::writable::algorithms::is_writable_stream_locked(&destination) {
            return Err(js::error::throw_type_error(
                scope,
                c"cannot pipe to a locked WritableStream",
            ));
        }
        // Step 3: Let _signal_ be _options_["``signal``"] if it `exists`, or undefined otherwise.
        let (prevent_close, prevent_abort, prevent_cancel, signal) = pipe_options(&options);
        // Step 4: Return ! `ReadableStreamPipeTo`(`this`, _destination_,
        //         _options_["``preventClose``"], _options_["``preventAbort``"],
        //         _options_["``preventCancel``"], _signal_).
        algorithms::readable_stream_pipe_to(
            scope,
            self,
            &destination,
            prevent_close,
            prevent_abort,
            prevent_cancel,
            signal,
        )
    }

    /// The JS-exposed `tee` method.
    ///
    /// <https://streams.spec.whatwg.org/#rs-tee>
    #[method(name = "tee")]
    fn js_tee<'r>(&self, scope: &'r Scope<'_>) -> Result<Vec<ReadableStream<'r>>, ExnThrown> {
        // Step 1: Return ? `ReadableStreamTee`(`this`, false).
        let branches = self.tee(scope, false)?;
        Ok(vec![branches.0, branches.1])
    }

    /// Tee the stream into two branches.
    ///
    /// If `clone_for_branch2` is `true`, chunks will be cloned before being produced by the
    /// second branch.
    ///
    /// <https://streams.spec.whatwg.org/#readablestream-tee>
    pub fn tee<'r>(
        &self,
        scope: &'r Scope<'_>,
        clone_for_branch2: bool,
    ) -> Result<(ReadableStream<'r>, ReadableStream<'r>), ExnThrown> {
        // Step 1: Assert: _stream_ `implements` ``ReadableStream``.
        // Step 2: Assert: _cloneForBranch2_ is a boolean.
        // Step 3: If _stream_.`[[controller]]` `implements` ``ReadableByteStreamController``, return ?
        //         `ReadableByteStreamTee`(_stream_).
        if self.byte_controller(scope).is_some() {
            return readable_byte_stream_tee(scope, self);
        }
        // Step 4: Return ? `ReadableStreamDefaultTee`(_stream_, _cloneForBranch2_).
        readable_stream_default_tee(scope, self, clone_for_branch2)
    }

    /// <https://streams.spec.whatwg.org/#rs-asynciterator>
    /// The `values` method, also installed as `[Symbol.asyncIterator]`.
    #[method]
    fn values<'r>(
        &self,
        scope: &'r Scope<'_>,
        options: Option<ReadableStreamIteratorOptions>,
    ) -> Result<ReadableStreamAsyncIterator<'r>, ExnThrown> {
        // Step 1: Let _reader_ be ? `AcquireDefaultReader`(`this`).
        let reader = algorithms::acquire_readable_stream_default_reader(scope, self)?;
        // Step 2: Let _iterator_ be a `new` ``ReadableStreamAsyncIterator``.
        // Step 3: Set _iterator_'s reader to _reader_.
        // Step 4: Let _preventCancel_ be _options_["``preventCancel``"].
        // Step 5: Set _iterator_'s prevent cancel to _preventCancel_.
        //         (`ReadableStreamAsyncIterator::new` performs steps 3 and 5.)
        let prevent_cancel = options.is_some_and(|o| o.prevent_cancel);
        let iterator = ReadableStreamAsyncIterator::new(scope, reader, prevent_cancel)?;
        // Step 6: Return _iterator_.
        Ok(iterator)
    }

    /// <https://streams.spec.whatwg.org/#rs-from>
    #[static_method(name = "from")]
    fn js_from<'r>(
        scope: &'r Scope<'_>,
        async_iterable: HandleValue<'_>,
    ) -> Result<ReadableStream<'r>, ExnThrown> {
        // Step 1: Return ? `ReadableStreamFromIterable`(_asyncIterable_).
        algorithms::readable_stream_from_iterable(scope, async_iterable)
    }

    /// Whether the stream has been read from or canceled.
    pub fn is_disturbed(&self) -> bool {
        self.data().disturbed
    }

    /// Whether the stream is still readable — neither closed nor errored, so
    /// closing or erroring it would still have an observable effect.
    pub fn is_readable(&self) -> bool {
        self.data().state == ReadableStreamState::Readable
    }

    /// <https://streams.spec.whatwg.org/#readable-stream-cancel>
    /// Cancel the stream with `reason`, running its underlying source's cancel
    /// steps. This is `ReadableStreamCancel`, the internal algorithm, so — unlike
    /// the author-facing `cancel()` method — it applies to a locked stream too.
    /// Callers that hold a reader should prefer
    /// [`native_read::native_reader_cancel`](crate::readable::native_read::native_reader_cancel).
    /// Distinct from the author-facing [`cancel`](Self::cancel) method above,
    /// which rejects when the stream is locked.
    pub fn cancel_internal<'r>(
        &self,
        scope: &'r Scope<'_>,
        reason: HandleValue<'r>,
    ) -> Promise<'r> {
        algorithms::readable_stream_cancel(scope, self, reason)
    }

    pub fn error(&self, scope: &Scope<'_>, reason: HandleValue<'_>) {
        match self.controller(scope) {
            ReadableStreamController::Default(controller) => {
                readable_stream_default_controller_error(scope, &controller, reason)
            }
            ReadableStreamController::Byte(controller) => {
                readable_byte_stream_controller_error(scope, &controller, reason)
            }
        }
    }

    /// The stream's `[[controller]]`. Panics if the stream is not set up yet.
    pub(crate) fn controller<'r>(&self, scope: &'r Scope<'_>) -> ReadableStreamController<'r> {
        self.data()
            .controller
            .as_ref()
            .expect("stream has a controller")
            .get(scope)
    }

    /// The stream's `[[controller]]` if it is a `ReadableStreamDefaultController`.
    pub fn default_controller<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Option<ReadableStreamDefaultController<'r>> {
        match self.controller(scope) {
            ReadableStreamController::Default(controller) => Some(controller),
            ReadableStreamController::Byte(_) => None,
        }
    }

    /// The stream's `[[controller]]` if it is a `ReadableByteStreamController`.
    pub(crate) fn byte_controller<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Option<ReadableByteStreamController<'r>> {
        match self.controller(scope) {
            ReadableStreamController::Byte(controller) => Some(controller),
            ReadableStreamController::Default(_) => None,
        }
    }

    /// The native source object backing this stream, if any (see
    /// [`native_byte_source`](Self::native_byte_source)). Lets a consumer
    /// recognize a stream backed by a native source instead of content code.
    /// This works for stream pipelines as well as Readable/Writable streams
    /// connected through an identity `TransformStream`
    /// (see [`set_native_source`](Self::set_native_source)).
    pub fn native_source<'r>(&self, scope: &'r Scope<'_>) -> Option<Object<'r>> {
        self.data().native_byte_source.get(scope)
    }

    /// Record `source` as this stream's native byte source.
    pub fn set_native_source(&self, source: &Object<'_>) {
        self.data_mut().native_byte_source.set(*source);
    }

    /// For streams piped to an identity `TransformStream` whose readable end has not
    /// been read yet, the promise fulfilled once a consumer actually asks for data.
    /// `None` once that demand has arrived, or if this stream is not piped to such
    /// a transform.
    ///
    /// This allows native underlying stream sources to hold off on performing an
    /// actual read operation until a `pull` happens on the pipeline's readable end,
    /// without deviating from the spec. Instead of immediately performing the
    /// operation, it can subscribe to this promise and get notified once a `pull`
    /// happens, or on the next tick if one is already pending.
    ///
    /// Why is this needed at all? The fact that a `TransformStream`'s writable side
    /// has a high-water mark of `1` means that a `pull` is enqueued upon the TS's
    /// creation. When a `ReadableStream` is later piped to the TS, that `pull` is
    /// applied to it. Without this kind of mechanism, the source would have no
    /// choice but to immediately perform the operation, only for the result to be
    /// parked in the `TransformStream`.
    pub fn deferred_demand<'r>(&self, scope: &'r Scope<'_>) -> Option<Promise<'r>> {
        let transform = self.data().piped_to_identity_transform.get(scope)?;
        // Backpressure is set while the readable end holds no demand, and
        // cleared by the first read of it.
        if !transform.data().backpressure {
            return None;
        }
        // Bound rather than returned directly, so the `data()` borrow is released
        // before the promise handle leaves this scope.
        let demand = transform.data().backpressure_change_promise.get(scope);
        demand
    }
}
