// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>

use super::algorithms;
use super::read_request::ReadRequest;
use super::readable_stream::{ReadableStream, ReadableStreamImpl};
use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::{HandleValue, OptionHeapExt};
use js::Promise;

/// <https://streams.spec.whatwg.org/#default-reader-class>
#[webidl_interface(name = "ReadableStreamDefaultReader")]
pub struct DefaultReader {
    /// <https://streams.spec.whatwg.org/#DefaultReader-readrequests>
    /// A list of read requests, used when a consumer requests chunks sooner than they are available.
    pub(crate) read_requests: std::collections::VecDeque<ReadRequest>,
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
impl DefaultReader {
    /// <https://streams.spec.whatwg.org/#default-reader-constructor>
    ///
    /// Setup-style: the JS object is allocated with default data first, then
    /// `SetUpDefaultReader` populates it (it needs `&self` to wire
    /// the stream↔reader links and the closed promise).
    #[constructor]
    pub fn new(&self, scope: &Scope<'_>, stream: ReadableStream<'_>) -> Result<(), ExnThrown> {
        // Step 1: Perform ? `SetUpDefaultReader`(`this`, _stream_).
        algorithms::set_up_readable_stream_default_reader(scope, self, &stream)
    }

    /// <https://streams.spec.whatwg.org/#dom-DefaultReader-closed>
    #[getter]
    pub fn closed<'r>(&self, scope: &'r Scope<'_>) -> Promise<'r> {
        // Step 1: Return `this`.`[[closedPromise]]`.
        self.data().closed_promise.get(scope)
    }

    /// <https://streams.spec.whatwg.org/#default-reader-read>
    #[method]
    pub fn read<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: If `this`.`[[stream]]` is undefined, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if self.data().stream.is_none() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamDefaultReader",
                c"Cannot read from a reader that does not have an owner stream",
            ));
        }
        // Step 2: Let _promise_ be `a new promise`.
        let promise = Promise::new_pending(scope)?;
        // Step 3: Let _readRequest_ be a new `read request` with the following `items`: `chunk
        //         steps`, given _chunk_ `Resolve` _promise_ with «[ "``value``" → _chunk_,
        //         "``done``" → false ]». `close steps` `Resolve` _promise_ with «[ "``value``"
        //         → undefined, "``done``" → true ]». `error steps`, given _e_ `Reject`
        //         _promise_ with _e_. (See `ReadRequest::Read` and its step methods.)
        // Step 4: Perform ! `DefaultReaderRead`(`this`, _readRequest_).
        algorithms::readable_stream_default_reader_read(
            scope,
            *self,
            ReadRequest::Read {
                promise: Heap::from(promise),
            },
        );
        // Step 5: Return _promise_.
        Ok(promise)
    }

    /// <https://streams.spec.whatwg.org/#default-reader-release-lock>
    #[method]
    fn release_lock(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: If `this`.`[[stream]]` is undefined, return.
        if self.data().stream.is_none() {
            return Ok(());
        }
        // Step 2: Perform ! `DefaultReaderRelease`(`this`).
        algorithms::readable_stream_default_reader_release(scope, self)
    }

    /// <https://streams.spec.whatwg.org/#DefaultReader-cancel>
    #[method]
    pub fn cancel<'r>(
        &self,
        scope: &'r Scope<'_>,
        reason: Option<HandleValue<'r>>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: If `this`.`[[stream]]` is undefined, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if self.data().stream.is_none() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamDefaultReader",
                c"Cannot cancel a reader that does not have an owner stream",
            ));
        }
        // Step 2: Return ! `ReadableStreamReaderGenericCancel`(`this`, _reason_).
        let reason = reason.unwrap_or(HandleValue::undefined());
        Ok(algorithms::readable_stream_reader_generic_cancel(
            scope, self, reason,
        ))
    }

    pub fn stream<'r>(&self, scope: &'r Scope<'_>) -> Option<ReadableStream<'r>> {
        self.data().stream.get(scope)
    }
}
