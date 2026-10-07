// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>

use super::algorithms;
use super::writable_stream::WritableStream;
use super::writable_stream::WritableStreamImpl;
use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Promise;
use web_globals::events::algorithms::ScriptStackState;

/// <https://streams.spec.whatwg.org/#default-writer-class>
#[webidl_interface]
pub struct WritableStreamDefaultWriter {
    /// <https://streams.spec.whatwg.org/#WritableStreamDefaultWriter-closedpromise>
    /// A promise returned by the writer’s closed getter
    pub(crate) closed_promise: Heap<js::promise::Promise>,
    /// <https://streams.spec.whatwg.org/#WritableStreamDefaultWriter-readypromise>
    /// A promise returned by the writer’s ready getter
    pub(crate) ready_promise: Heap<js::promise::Promise>,
    /// <https://streams.spec.whatwg.org/#WritableStreamDefaultWriter-stream>
    /// A WritableStream instance that owns this reader, or `None` once released.
    pub(crate) stream: Option<Heap<WritableStreamImpl>>,
}

#[webidl_methods]
impl WritableStreamDefaultWriter {
    /// <https://streams.spec.whatwg.org/#default-writer-constructor>
    #[constructor]
    fn new(&self, scope: &Scope<'_>, stream: WritableStream<'_>) -> Result<(), ExnThrown> {
        // Step 1: Perform ? `SetUpDefaultWriter`(`this`, _stream_).
        algorithms::set_up_writable_stream_default_writer(scope, self, &stream)
    }

    /// <https://streams.spec.whatwg.org/#default-writer-closed>
    #[getter]
    pub fn closed<'r>(&self, scope: &'r Scope<'_>) -> Promise<'r> {
        // Step 1: Return `this`.`[[closedPromise]]`.
        self.data().closed_promise.get(scope)
    }

    /// <https://streams.spec.whatwg.org/#default-writer-desired-size>
    #[getter]
    fn desired_size(&self, scope: &Scope<'_>) -> Result<Option<f64>, ExnThrown> {
        // Step 1: If `this`.`[[stream]]` is undefined, throw a ``TypeError`` exception.
        if self.data().stream.is_none() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "WritableStreamDefaultWriter",
                c"Cannot read desiredSize of a writer that does not have an owner stream",
            ));
        }
        // Step 2: Return ! `DefaultWriterGetDesiredSize`(`this`).
        Ok(algorithms::writable_stream_default_writer_get_desired_size(
            scope, self,
        ))
    }

    /// <https://streams.spec.whatwg.org/#default-writer-ready>
    #[getter]
    pub fn ready<'r>(&self, scope: &'r Scope<'_>) -> Promise<'r> {
        // Step 1: Return `this`.`[[readyPromise]]`.
        self.data().ready_promise.get(scope)
    }

    /// <https://streams.spec.whatwg.org/#default-writer-abort>
    #[method(length = 0)]
    pub fn abort<'r>(
        &self,
        scope: &'r Scope<'_>,
        reason: HandleValue<'_>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: If `this`.`[[stream]]` is undefined, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if self.data().stream.is_none() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "WritableStreamDefaultWriter",
                c"Cannot abort a stream using a released writer",
            ));
        }
        // Step 2: Return ! `DefaultWriterAbort`(`this`, _reason_).
        Ok(algorithms::writable_stream_default_writer_abort(
            scope,
            self,
            reason,
            ScriptStackState::NonEmpty,
        ))
    }

    /// <https://streams.spec.whatwg.org/#default-writer-close>
    #[method]
    pub fn close<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _stream_ be `this`.`[[stream]]`.
        // Step 2: If _stream_ is undefined, return `a promise rejected with` a ``TypeError``
        //         exception.
        let stream = match algorithms::writer_stream(scope, self) {
            Some(stream) => stream,
            None => {
                return Err(crate::algorithms::throw_invalid_state(
                    scope,
                    "WritableStreamDefaultWriter",
                    c"Cannot close a stream using a released writer",
                ));
            }
        };
        // Step 3: If ! `WritableStreamCloseQueuedOrInFlight`(_stream_) is true, return `a promise
        //         rejected with` a ``TypeError`` exception.
        if algorithms::writable_stream_close_queued_or_in_flight(&stream) {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "WritableStreamDefaultWriter",
                c"Cannot close an already-closing stream",
            ));
        }
        // Step 4: Return ! `DefaultWriterClose`(`this`).
        Ok(algorithms::writable_stream_default_writer_close(
            scope, self,
        ))
    }

    /// <https://streams.spec.whatwg.org/#default-writer-release-lock>
    #[method]
    fn release_lock(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: Let _stream_ be `this`.`[[stream]]`.
        // Step 2: If _stream_ is undefined, return.
        if self.data().stream.is_none() {
            return Ok(());
        }
        // Step 3: Assert: _stream_.`[[writer]]` is not undefined.
        // Step 4: Perform ! `DefaultWriterRelease`(`this`).
        algorithms::writable_stream_default_writer_release(scope, self);
        Ok(())
    }

    /// <https://streams.spec.whatwg.org/#default-writer-write>
    #[method]
    pub fn write<'r>(
        &self,
        scope: &'r Scope<'_>,
        chunk: Option<HandleValue<'_>>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: If `this`.`[[stream]]` is undefined, return `a promise rejected with` a
        //         ``TypeError`` exception.
        if self.data().stream.is_none() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "WritableStreamDefaultWriter",
                c"Cannot write to a stream using a released writer",
            ));
        }
        // Step 2: Return ! `DefaultWriterWrite`(`this`, _chunk_).
        let chunk = chunk.unwrap_or(HandleValue::undefined());
        Ok(algorithms::writable_stream_default_writer_write(
            scope, self, chunk,
        ))
    }
}
