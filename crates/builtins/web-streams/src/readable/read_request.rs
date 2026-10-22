// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Read requests and read-into requests, one enum variant per consumer.
//!
//! <https://streams.spec.whatwg.org/#read-request>

use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::iteration::create_iter_result;
use js::prelude::HandleValue;
use js::promise::Promise;

use crate::readable::algorithms::{
    byte_tee_byob_chunk_steps, byte_tee_byob_close_steps, byte_tee_default_chunk_steps,
    byte_tee_default_close_steps, byte_tee_set_not_reading, pipe_read_request_chunk_steps,
    tee_read_request_chunk_steps, tee_read_request_close_steps, tee_read_request_error_steps,
    ByteTeeStateImpl, PipeStateImpl, TeeStateImpl,
};
use crate::readable::default_reader::DefaultReaderImpl;
use crate::readable::read_all_bytes::{
    read_all_bytes_chunk_steps, read_all_bytes_close_steps, read_all_bytes_error_steps,
    ReadAllBytesStateImpl,
};
use crate::support::report_failure;

/// An entry in a reader's `[[readRequests]]` list.
#[js::must_root]
#[derive(core_runtime::Traceable, js::ScopeRoot)]
pub(crate) enum ReadRequest {
    /// A `DefaultReader.read()` call.
    ///
    /// <https://streams.spec.whatwg.org/#default-reader-read>: chunk steps
    /// resolve the promise with `{ value: chunk, done: false }`, close steps
    /// with `{ value: undefined, done: true }`, and error steps reject it.
    Read {
        /// The promise returned to the caller of `read()`.
        promise: Heap<Promise>,
    },
    /// The read request used by `ReadableStreamDefaultTee`'s pull algorithm.
    /// Its steps drive both branches; the shared tee state is a `TeeState` (see
    /// `crate::algorithms`'s tee helpers).
    ///
    /// <https://streams.spec.whatwg.org/#abstract-opdef-readablestreamdefaulttee>
    Tee {
        /// The tee state object.
        state: Heap<TeeStateImpl>,
    },
    /// The read request used by `ReadableStreamPipeTo`'s pipe loop. Its chunk
    /// steps write the chunk to the destination and continue the loop; close and
    /// error steps do nothing (the closed-promise propagation reactions drive
    /// shutdown). The shared pipe state is a `PipeState` (see `crate::algorithms`'s
    /// pipe helpers).
    ///
    /// <https://streams.spec.whatwg.org/#readable-stream-pipe-to>
    Pipe {
        /// The pipe state object.
        state: Heap<PipeStateImpl>,
    },
    /// The read request used by the `ReadableStream` async iterator's `next()`.
    /// Its steps settle the per-call promise and release the reader on close or
    /// error.
    ///
    /// <https://streams.spec.whatwg.org/#rs-asynciterator>
    AsyncIter {
        /// The promise returned by `get the next iteration result`.
        promise: Heap<js::promise::Promise>,
        /// The reader to release on close or error.
        reader: Heap<DefaultReaderImpl>,
        /// The unique "end of iteration" sentinel: the close steps resolve
        /// `promise` with it, and `next()`'s fulfill steps recognise it by
        /// identity to finish the iterator.
        end_of_iteration: Heap<js::object::Object>,
    },
    /// The default-reader read request used by `ReadableByteStreamTee`'s
    /// `pullWithDefaultReader`. Its steps drive both byte branches; the shared
    /// tee state is a `ByteTeeState` (see `crate::algorithms`'s byte-tee helpers).
    ///
    /// <https://streams.spec.whatwg.org/#abstract-opdef-readablebytestreamtee>
    ByteTeeDefault {
        /// The byte-tee state object.
        state: Heap<ByteTeeStateImpl>,
    },
    /// The internal read request used by `Read all bytes from reader`. Its chunk
    /// steps accumulate the chunk's bytes and issue the next internal read, its
    /// close steps run the caller's success callback on the assembled bytes, and
    /// its error steps reject the result promise. The shared state is a
    /// `ReadAllBytesState` (see `crate::readable::read_all_bytes`).
    ///
    /// <https://streams.spec.whatwg.org/#readablestreamdefaultreader-read-all-bytes>
    Consume {
        /// The read-all-bytes state object.
        state: Heap<ReadAllBytesStateImpl>,
    },
    /// An out-of-crate native consumer's read request (see
    /// `crate::readable::native_read`): the steps are plain fn pointers
    /// receiving the rooted `payload`.
    Native {
        /// The consumer's step functions.
        #[no_trace]
        steps: crate::readable::native_read::NativeReadSteps,
        /// The consumer's rooted state object.
        payload: Heap<js::object::Object>,
    },
}

impl<'s> StackReadRequest<'s> {
    /// Perform the read request's `chunk steps` given `chunk`.
    pub(crate) fn chunk_steps(self, scope: &Scope<'_>, chunk: HandleValue<'_>) {
        let result = match self {
            StackReadRequest::Read { promise } => create_iter_result(scope, chunk, false)
                .and_then(|result| promise.resolve(scope, result)),
            StackReadRequest::Tee { state } => tee_read_request_chunk_steps(scope, state, chunk),
            StackReadRequest::Pipe { state } => pipe_read_request_chunk_steps(scope, state, chunk),
            // `next()`'s fulfill steps wrap the raw chunk in `{ value, done }`.
            StackReadRequest::AsyncIter { promise, .. } => promise.resolve(scope, chunk),
            StackReadRequest::ByteTeeDefault { state } => {
                byte_tee_default_chunk_steps(scope, state, chunk)
            }
            StackReadRequest::Consume { state } => read_all_bytes_chunk_steps(scope, state, chunk),
            StackReadRequest::Native { steps, payload } => (steps.chunk)(scope, payload, chunk),
        };
        report_failure(scope, result, "read request chunk steps");
    }

    /// Perform the read request's `close steps`.
    pub(crate) fn close_steps(self, scope: &Scope<'_>) {
        let result = match self {
            StackReadRequest::Read { promise } => {
                create_iter_result(scope, HandleValue::undefined(), true)
                    .and_then(|result| promise.resolve(scope, result))
            }
            StackReadRequest::Tee { state } => tee_read_request_close_steps(scope, state),
            // The pipe loop's close steps do nothing — shutdown is driven by the
            // closing-propagation reaction on the reader's closed promise.
            StackReadRequest::Pipe { .. } => Ok(()),
            StackReadRequest::AsyncIter {
                promise,
                reader,
                end_of_iteration,
            } => super::async_iterator::async_iter_close_steps(
                scope,
                promise,
                reader,
                end_of_iteration,
            ),
            StackReadRequest::ByteTeeDefault { state } => {
                byte_tee_default_close_steps(scope, state)
            }
            StackReadRequest::Consume { state } => read_all_bytes_close_steps(scope, state),
            StackReadRequest::Native { steps, payload } => (steps.close)(scope, payload),
        };
        report_failure(scope, result, "read request close steps");
    }

    /// Perform the read request's `error steps` given `e`.
    pub(crate) fn error_steps(self, scope: &Scope<'_>, e: HandleValue<'_>) {
        let result = match self {
            StackReadRequest::Read { promise } => promise.reject(scope, e),
            StackReadRequest::Tee { state } => tee_read_request_error_steps(scope, state, e),
            // The pipe loop's error steps do nothing — shutdown is driven by the
            // error-propagation reaction on the reader's closed promise.
            StackReadRequest::Pipe { .. } => Ok(()),
            StackReadRequest::AsyncIter {
                promise, reader, ..
            } => super::async_iterator::async_iter_error_steps(scope, promise, reader, e),
            // The byte-tee default read request's error steps only set `reading`
            // to false; stream errors are forwarded via the reader's closed promise.
            StackReadRequest::ByteTeeDefault { state } => {
                let _ = e;
                byte_tee_set_not_reading(scope, state);
                Ok(())
            }
            StackReadRequest::Consume { state } => read_all_bytes_error_steps(scope, state, e),
            StackReadRequest::Native { steps, payload } => (steps.error)(scope, payload, e),
        };
        report_failure(scope, result, "read request error steps");
    }
}

/// An entry in a BYOB reader's `[[readIntoRequests]]` list.
///
/// <https://streams.spec.whatwg.org/#read-into-request>
///
/// Like [`ReadRequest`] but for `BYOBReader.read(view)`. Its
/// `chunk`/`close` steps both receive a view (the filled or empty destination),
/// unlike a default read request whose close step produces `undefined`.
#[js::must_root]
#[derive(core_runtime::Traceable, js::ScopeRoot)]
pub(crate) enum ReadIntoRequest {
    /// A `BYOBReader.read(view)` call.
    ///
    /// <https://streams.spec.whatwg.org/#byob-reader-read>: chunk steps resolve
    /// the promise with `{ value: chunk, done: false }`, close steps with
    /// `{ value: chunk, done: true }`, and error steps reject it.
    Read {
        /// The promise returned to the caller of `read()`.
        promise: Heap<Promise>,
    },
    /// The BYOB read-into request used by `ReadableByteStreamTee`'s
    /// `pullWithBYOBReader`. Its steps drive both byte branches; the shared tee
    /// state is a `ByteTeeState` (see `crate::algorithms`'s byte-tee helpers).
    ///
    /// <https://streams.spec.whatwg.org/#abstract-opdef-readablebytestreamtee>
    ByteTeeByob {
        /// The byte-tee state object.
        state: Heap<ByteTeeStateImpl>,
        /// Whether this read-into targets branch 2 (`forBranch2`).
        #[no_trace]
        for_branch2: bool,
    },
}

impl<'s> StackReadIntoRequest<'s> {
    /// Perform the read-into request's `chunk steps` given `chunk`.
    pub(crate) fn chunk_steps(self, scope: &Scope<'_>, chunk: HandleValue<'_>) {
        let result = match self {
            StackReadIntoRequest::Read { promise } => create_iter_result(scope, chunk, false)
                .and_then(|result| promise.resolve(scope, result)),
            StackReadIntoRequest::ByteTeeByob { state, for_branch2 } => {
                byte_tee_byob_chunk_steps(scope, state, for_branch2, chunk)
            }
        };
        report_failure(scope, result, "read-into request chunk steps");
    }

    /// Perform the read-into request's `close steps` given `chunk`.
    pub(crate) fn close_steps(self, scope: &Scope<'_>, chunk: HandleValue<'_>) {
        let result = match self {
            StackReadIntoRequest::Read { promise } => create_iter_result(scope, chunk, true)
                .and_then(|result| promise.resolve(scope, result)),
            StackReadIntoRequest::ByteTeeByob { state, for_branch2 } => {
                byte_tee_byob_close_steps(scope, state, for_branch2, chunk)
            }
        };
        report_failure(scope, result, "read-into request close steps");
    }

    /// Perform the read-into request's `error steps` given `e`.
    pub(crate) fn error_steps(self, scope: &Scope<'_>, e: HandleValue<'_>) {
        let result = match self {
            StackReadIntoRequest::Read { promise } => promise.reject(scope, e),
            // The byte-tee BYOB read-into request's error steps only set `reading`
            // to false; stream errors are forwarded via the reader's closed promise.
            StackReadIntoRequest::ByteTeeByob { state, .. } => {
                let _ = e;
                byte_tee_set_not_reading(scope, state);
                Ok(())
            }
        };
        report_failure(scope, result, "read-into request error steps");
    }
}
