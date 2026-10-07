// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>

use super::algorithms;
use super::options::BYOBReaderReadOptions;
use super::read_request::ReadIntoRequest;
use super::readable_stream::ReadableStream;
use super::readable_stream::ReadableStreamImpl;
use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{ArrayBufferView, Promise};

/// <https://streams.spec.whatwg.org/#byob-reader-class>
#[webidl_interface(name = "ReadableStreamBYOBReader")]
pub struct BYOBReader {
    /// <https://streams.spec.whatwg.org/#BYOBReader-readintorequests>
    /// A list of read-into requests, used when a consumer requests chunks sooner than they are
    /// available.
    pub(crate) read_into_requests: std::collections::VecDeque<ReadIntoRequest>,
    /// `ReadableStreamGenericReader` mixin slot `[[stream]]`: the stream owning
    /// this reader, or `None` once released.
    ///
    /// <https://streams.spec.whatwg.org/#readablestreamgenericreader-stream>
    pub(crate) stream: Option<Heap<ReadableStreamImpl>>,
    /// `ReadableStreamGenericReader` mixin slot `[[closedPromise]]`.
    ///
    /// <https://streams.spec.whatwg.org/#readablestreamgenericreader-closedpromise>
    pub(crate) closed_promise: Heap<js::promise::Promise>,
}

#[webidl_methods]
impl BYOBReader {
    /// <https://streams.spec.whatwg.org/#byob-reader-constructor>
    ///
    /// Setup-style: the JS object is allocated with default data first, then
    /// `SetUpBYOBReader` populates it (it needs `&self` to wire the
    /// stream↔reader links and the closed promise).
    #[constructor]
    fn new(&self, scope: &Scope<'_>, stream: ReadableStream<'_>) -> Result<(), ExnThrown> {
        // Step 1: Perform ? `SetUpBYOBReader`(`this`, _stream_).
        algorithms::set_up_readable_stream_byob_reader(scope, self, &stream)
    }

    /// <https://streams.spec.whatwg.org/#dom-BYOBReader-closed>
    #[getter]
    fn closed<'r>(&self, scope: &'r Scope<'_>) -> Promise<'r> {
        // Step 1: Return `this`.`[[closedPromise]]`.
        self.data().closed_promise.get(scope)
    }

    /// <https://streams.spec.whatwg.org/#byob-reader-read>
    #[method]
    fn read<'r>(
        &self,
        scope: &'r Scope<'_>,
        view: ArrayBufferView<'_>,
        options: Option<BYOBReaderReadOptions>,
    ) -> Result<Promise<'r>, ExnThrown> {
        let min = options.map_or(1, |o| o.min.0) as usize;
        // Step 1: If _view_.[[ByteLength]] is 0, return `a promise rejected with` a ``TypeError``
        //         exception.
        if view.byte_length() == 0 {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamBYOBReader",
                c"read() view has a byte length of 0",
            ));
        }
        // Step 2: If _view_.[[ViewedArrayBuffer]].[[ByteLength]] is 0, return `a promise rejected
        //         with` a ``TypeError`` exception.
        let buffer = view.viewed_buffer(scope)?;
        if buffer.byte_length() == 0 {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamBYOBReader",
                c"read() view's buffer has a byte length of 0",
            ));
        }
        // Step 3: If ! `IsDetachedBuffer`(_view_.[[ViewedArrayBuffer]]) is true, return `a promise
        //         rejected with` a ``TypeError`` exception.
        if buffer.is_detached() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamBYOBReader",
                c"read() view's buffer is detached",
            ));
        }
        // Step 4: If _options_["``min``"] is 0, return `a promise rejected with` a ``TypeError``
        //         exception.
        if min == 0 {
            return Err(js::error::throw_type_error(
                scope,
                c"read() option min cannot be 0",
            ));
        }
        if view.view_kind().is_typed_array() {
            // Step 5: If _view_ has a [[TypedArrayName]] internal slot, If _options_["``min``"] >
            //         _view_.[[ArrayLength]], return `a promise rejected with` a ``RangeError``
            //         exception.
            if min > view.array_length() {
                return Err(js::error::throw_range_error(
                    scope,
                    c"read() option min exceeds the view's length",
                ));
            }
        } else {
            // Step 6: Otherwise (i.e., it is a ``DataView``), If _options_["``min``"] >
            //         _view_.[[ByteLength]], return `a promise rejected with` a ``RangeError``
            //         exception.
            if min > view.byte_length() {
                return Err(js::error::throw_range_error(
                    scope,
                    c"read() option min exceeds the view's byte length",
                ));
            }
        }
        // Step 7: If `this`.`[[stream]]` is undefined, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if self.data().stream.is_none() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamBYOBReader",
                c"Cannot read from a reader that is not attached to a stream",
            ));
        }
        // Step 8: Let _promise_ be `a new promise`.
        let promise = Promise::new_pending(scope)?;
        // Step 9: Let _readIntoRequest_ be a new `read-into request` with the following `items`:
        //         `chunk steps`, given _chunk_ `Resolve` _promise_ with «[ "``value``" →
        //         _chunk_, "``done``" → false ]». `close steps`, given _chunk_ `Resolve`
        //         _promise_ with «[ "``value``" → _chunk_, "``done``" → true ]». `error
        //         steps`, given _e_ `Reject` _promise_ with _e_.
        //         (The steps settle `promise`; see `ReadIntoRequest::Read`.)
        // Step 10: Perform ! `BYOBReaderRead`(`this`, _view_, _options_["``min``"],
        //          _readIntoRequest_).
        algorithms::readable_stream_byob_reader_read(
            scope,
            self,
            view,
            min,
            ReadIntoRequest::Read {
                promise: Heap::from(promise),
            },
        );
        // Step 11: Return _promise_.
        Ok(promise)
    }

    /// <https://streams.spec.whatwg.org/#byob-reader-release-lock>
    #[method]
    fn release_lock(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: If `this`.`[[stream]]` is undefined, return.
        if self.data().stream.is_none() {
            return Ok(());
        }
        // Step 2: Perform ! `BYOBReaderRelease`(`this`).
        algorithms::readable_stream_byob_reader_release(scope, self)
    }

    /// <https://streams.spec.whatwg.org/#dom-BYOBReader-cancel>
    #[method]
    fn cancel<'r>(
        &self,
        scope: &'r Scope<'_>,
        reason: Option<HandleValue<'r>>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: If `this`.`[[stream]]` is undefined, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if self.data().stream.is_none() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamBYOBReader",
                c"Cannot cancel a reader that has no stream",
            ));
        }
        // Step 2: Return ! `ReadableStreamReaderGenericCancel`(`this`, _reason_).
        let reason = reason.unwrap_or(HandleValue::undefined());
        Ok(algorithms::readable_stream_reader_generic_cancel(
            scope, self, reason,
        ))
    }
}
