// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The default asynchronous iterator for `ReadableStream`.
//!
//! <https://streams.spec.whatwg.org/#rs-asynciterator>
//!
//! `ReadableStream.prototype.values()` and `[Symbol.asyncIterator]()` return an
//! instance of this interface. Its prototype chains to `%AsyncIteratorPrototype%`
//! (set up in `add_to_global`), which supplies `[Symbol.asyncIterator]` returning
//! `this`. The `next`/`return` methods implement WebIDL §3.7.10.2's
//! default async iterator semantics: calls are serialized through an "ongoing
//! promise", and a finished iterator yields `{ value: undefined, done: true }`.

use super::algorithms;
use super::default_reader::{DefaultReader, DefaultReaderImpl};
use super::read_request::ReadRequest;
use crate::algorithms::{pair_parts, pair_payload};
use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::function::cast_payload;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::iteration::create_iter_result;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleValue};
use js::{Function, Object, Promise};

/// <https://streams.spec.whatwg.org/#rs-asynciterator>
// WebIDL §3.7.10: the class string of an asynchronous iterator prototype object
// is the interface identifier followed by " AsyncIterator" (with a space).
#[webidl_interface(hidden, to_string_tag = "ReadableStream AsyncIterator")]
pub struct ReadableStreamAsyncIterator {
    /// The `DefaultReader` acquired for this iteration.
    pub(crate) reader: Heap<DefaultReaderImpl>,
    /// The `preventCancel` option captured at creation.
    #[no_trace]
    pub(crate) prevent_cancel: bool,
    /// WebIDL default async iterator `[[isFinished]]`.
    #[no_trace]
    pub(crate) is_finished: bool,
    /// WebIDL default async iterator `[[ongoingPromise]]`, serializing calls.
    pub(crate) ongoing_promise: Option<Heap<js::promise::Promise>>,
    /// The unique "end of iteration" sentinel (see `get_next_iteration_result`),
    /// and the per-iterator reaction callbacks for `next()` (payload = this
    /// iterator). All are created on the first `next()` and reused for every
    /// subsequent call, so iterating allocates no callback objects per chunk.
    /// `None` until the first `next()`.
    pub(crate) end_of_iteration: Option<Heap<js::object::Object>>,
    pub(crate) next_fulfilled_fn: Option<Heap<js::function::Function>>,
    pub(crate) next_rejected_fn: Option<Heap<js::function::Function>>,
    pub(crate) after_ongoing_next_fn: Option<Heap<js::function::Function>>,
}

#[webidl_methods]
impl ReadableStreamAsyncIterator {
    /// Not exposed to JS, called by `ReadableStream.prototype.values`.
    fn new(reader: DefaultReader<'_>, prevent_cancel: bool) -> Self {
        ReadableStreamAsyncIteratorImpl {
            reader: Heap::from(reader),
            prevent_cancel,
            ..Default::default()
        }
    }

    /// <https://webidl.spec.whatwg.org/#dfn-asynchronous-iterator-prototype-object>: `next`.
    #[method]
    fn next<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _interface_ be the `interface` for which the `asynchronous iterator prototype
        //         object` exists.
        // Step 2: Let _thisValidationPromiseCapability_ be ! NewPromiseCapability(%Promise%).
        // Step 3: Let _thisValue_ be the *this* value.
        // Step 4: Let _object_ be Completion(ToObject(_thisValue_)).
        // Step 5: IfAbruptRejectPromise(_object_, _thisValidationPromiseCapability_).
        // Step 6: If _object_ `is a platform object`, then `perform a security check`, passing:
        //         the platform object _object_, the identifier "`next`", and the type
        //         "`method`". If this threw an exception _e_, then: Perform ! Call(
        //         _thisValidationPromiseCapability_.[[Reject]], undefined, « _e_ »). Return
        //         _thisValidationPromiseCapability_.[[Promise]].
        // Step 7: If _object_ is not a `default asynchronous iterator object` for _interface_,
        //         then: Let _error_ be a new TypeError. Perform ! Call(
        //         _thisValidationPromiseCapability_.[[Reject]], undefined, « _error_ »). Return
        //         _thisValidationPromiseCapability_.[[Promise]].
        //         (Steps 1-7: the `Result<Promise>` method style brand-checks `this` and returns a
        //         rejected promise on failure. There are no security checks in this runtime.)
        // Step 8: Let _nextSteps_ be the following steps: (implemented in `run_next_steps`)
        // Step 9: Let _ongoingPromise_ be _object_'s `ongoing promise`.
        let ongoing_promise = self.data().ongoing_promise.get(scope);
        // Step 10: If _ongoingPromise_ is not null, then:
        let ongoing = if let Some(ongoing_promise) = ongoing_promise {
            // Step 10.1: Let _afterOngoingPromiseCapability_ be ! NewPromiseCapability(%Promise%).
            // Step 10.2: Let _onSettled_ be CreateBuiltinFunction(_nextSteps_, 0, "", « »).
            if self.data().after_ongoing_next_fn.is_none() {
                let cb = Function::new_callback(scope, c"", 1, after_ongoing_next, self)?;
                self.data_mut().after_ongoing_next_fn.set(cb);
            }
            let on_settled = self
                .data()
                .after_ongoing_next_fn
                .get(scope)
                .expect("created above");
            // Step 10.3: Perform PerformPromiseThen(_ongoingPromise_, _onSettled_, _onSettled_,
            //            _afterOngoingPromiseCapability_).
            // Step 10.4: Set _object_'s `ongoing promise` to
            //            _afterOngoingPromiseCapability_.[[Promise]].
            ongoing_promise.then(scope, Some(*on_settled), Some(*on_settled))?
        } else {
            // Step 11: Otherwise:
            // Step 11.1: Set _object_'s `ongoing promise` to the result of running _nextSteps_.
            run_next_steps(scope, self)?
        };
        self.data_mut().ongoing_promise.set(ongoing);
        // Step 12: Return _object_'s `ongoing promise`.
        Ok(ongoing)
    }

    /// <https://webidl.spec.whatwg.org/#dfn-asynchronous-iterator-prototype-object>: `return`.
    #[method(name = "return", length = 1)]
    fn iterator_return<'r>(
        &self,
        scope: &'r Scope<'_>,
        value: Option<HandleValue<'r>>,
    ) -> Result<Promise<'r>, ExnThrown> {
        let value = value.unwrap_or(HandleValue::undefined());
        // Step 1: Let _interface_ be the `interface` for which the `asynchronous iterator prototype
        //         object` exists.
        // Step 2: Let _returnPromiseCapability_ be ! NewPromiseCapability(%Promise%).
        // Step 3: Let _thisValue_ be the *this* value.
        // Step 4: Let _object_ be Completion(ToObject(_thisValue_)).
        // Step 5: IfAbruptRejectPromise(_object_, _returnPromiseCapability_).
        // Step 6: If _object_ `is a platform object`, then `perform a security check`, passing:
        //         the platform object _object_, the identifier "`return`", and the type
        //         "`method`". If this threw an exception _e_, then: Perform ! Call(
        //         _returnPromiseCapability_.[[Reject]], undefined, « _e_ »). Return
        //         _returnPromiseCapability_.[[Promise]].
        // Step 7: If _object_ is not a `default asynchronous iterator object` for _interface_,
        //         then: Let _error_ be a new TypeError. Perform ! Call(
        //         _returnPromiseCapability_.[[Reject]], undefined, « _error_ »). Return
        //         _returnPromiseCapability_.[[Promise]].
        //         (Steps 1-7: the `Result<Promise>` method style brand-checks `this` and returns a
        //         rejected promise on failure. There are no security checks in this runtime.)
        // Step 8: Let _returnSteps_ be the following steps: (implemented in `run_return_steps`)
        // Step 9: Let _ongoingPromise_ be _object_'s `ongoing promise`.
        let ongoing_promise = self.data().ongoing_promise.get(scope);
        // Step 10: If _ongoingPromise_ is not null, then:
        let ongoing = if let Some(ongoing_promise) = ongoing_promise {
            // Step 10.1: Let _afterOngoingPromiseCapability_ be ! NewPromiseCapability(%Promise%).
            // Step 10.2: Let _onSettled_ be CreateBuiltinFunction(_returnSteps_, 0, "", « »).
            let payload = pair_payload(scope, self, value)?;
            let on_settled = Function::new_callback(scope, c"", 1, after_ongoing_return, payload)?;
            // Step 10.3: Perform PerformPromiseThen(_ongoingPromise_, _onSettled_, _onSettled_,
            //            _afterOngoingPromiseCapability_).
            // Step 10.4: Set _object_'s `ongoing promise` to
            //            _afterOngoingPromiseCapability_.[[Promise]].
            ongoing_promise.then(scope, Some(*on_settled), Some(*on_settled))?
        } else {
            // Step 11: Otherwise:
            // Step 11.1: Set _object_'s `ongoing promise` to the result of running _returnSteps_.
            run_return_steps(scope, self, value)?
        };
        self.data_mut().ongoing_promise.set(ongoing);
        // Step 12: Let _fulfillSteps_ be the following steps:
        // Step 12.1: Return CreateIteratorResultObject(_value_, true).
        //            (Implemented in `return_fulfilled`.)
        // Step 13: Let _onFulfilled_ be CreateBuiltinFunction(_fulfillSteps_, 1, "", « »).
        let on_fulfilled = Function::new_callback(scope, c"", 1, return_fulfilled, value)?;
        // Step 14: Perform PerformPromiseThen(_object_'s `ongoing promise`, _onFulfilled_,
        //          undefined, _returnPromiseCapability_).
        // Step 15: Return _returnPromiseCapability_.[[Promise]].
        ongoing.then(scope, Some(*on_fulfilled), None)
    }
}

/// The async iterator's `[[ongoingPromise]]` next continuation (payload = the
/// iterator): run the next steps regardless of how the previous call settled.
fn after_ongoing_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let iter = cast_payload::<ReadableStreamAsyncIterator>(scope, payload);
    Ok(run_next_steps(scope, &iter)?.as_value())
}

/// The async iterator's `[[ongoingPromise]]` return continuation (payload =
/// `[iterator, value]`).
fn after_ongoing_return(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let (iter_v, value) = pair_parts(scope, payload);
    let iter = cast_payload::<ReadableStreamAsyncIterator>(scope, iter_v);
    Ok(run_return_steps(scope, &iter, value)?.as_value())
}

/// The _nextSteps_ of the asynchronous iterator `next` method.
fn run_next_steps<'r>(
    scope: &'r Scope<'_>,
    iter: &ReadableStreamAsyncIterator<'_>,
) -> Result<Promise<'r>, ExnThrown> {
    // Step 1: Let _nextPromiseCapability_ be ! NewPromiseCapability(%Promise%).
    // Step 2: If _object_'s `is finished` is true, then:
    if iter.data().is_finished {
        // Step 2.1: Let _result_ be CreateIteratorResultObject(undefined, true).
        let result = create_iter_result(scope, HandleValue::undefined(), true)?;
        // Step 2.2: Perform ! Call(_nextPromiseCapability_.[[Resolve]], undefined, « _result_ »).
        // Step 2.3: Return _nextPromiseCapability_.[[Promise]].
        return Promise::new_resolved_with_value(scope, result);
    }
    // Step 3: Let _kind_ be _object_'s `kind`.
    //         (A value asynchronously iterable declaration has no kind to use.)
    // Step 4: Let _nextPromise_ be the result of `getting the next iteration result` with
    //         _object_'s `target` and _object_.
    // Step 5: Let _fulfillSteps_ be the following steps, given _next_: (implemented in
    //         `next_fulfilled`)
    // Step 6: Let _onFulfilled_ be CreateBuiltinFunction(_fulfillSteps_, 1, "", « »).
    // Step 7: Let _rejectSteps_ be the following steps, given _reason_: (implemented in
    //         `next_rejected`)
    // Step 8: Let _onRejected_ be CreateBuiltinFunction(_rejectSteps_, 1, "", « »).
    // The unique sentinel with which the close steps resolve the next-iteration
    // promise (recognized by identity in `next_fulfilled`) and the reaction
    // callbacks are per-iterator; create them on the first `next()` and reuse
    // them for every subsequent chunk.
    if iter.data().next_fulfilled_fn.is_none() {
        // TODO: share the sentinel per-global.
        let sentinel = Object::new_plain(scope)?;
        iter.data_mut().end_of_iteration.set(sentinel);
        let on_f = Function::new_callback(scope, c"", 1, next_fulfilled, iter)?;
        iter.data_mut().next_fulfilled_fn.set(on_f);
        let on_r = Function::new_callback(scope, c"", 1, next_rejected, iter)?;
        iter.data_mut().next_rejected_fn.set(on_r);
    }
    let sentinel = iter
        .data()
        .end_of_iteration
        .get(scope)
        .expect("sentinel is created with the callbacks");
    let next_promise = get_next_iteration_result(scope, iter, sentinel)?;
    let on_f = iter
        .data()
        .next_fulfilled_fn
        .get(scope)
        .expect("created above");
    let on_r = iter
        .data()
        .next_rejected_fn
        .get(scope)
        .expect("created above");
    // Step 9: Perform PerformPromiseThen(_nextPromise_, _onFulfilled_, _onRejected_,
    //         _nextPromiseCapability_).
    // Step 10: Return _nextPromiseCapability_.[[Promise]].
    next_promise.then(scope, Some(*on_f), Some(*on_r))
}

/// The fulfill steps of `next()`: build the `{ value, done }` result from the
/// next-iteration promise's value. The value is either the unique end-of-iteration
/// sentinel (the iterator finished) or the raw chunk (already adopted by the
/// promise resolution, so a thenable chunk arrives here unwrapped). The payload is
/// the iterator; the sentinel lives in its `end_of_iteration` slot.
fn next_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let iter = cast_payload::<ReadableStreamAsyncIterator>(scope, payload);
    let sentinel = iter
        .data()
        .end_of_iteration
        .get(scope)
        .expect("sentinel is created with this callback");
    let next = args.get(0);
    // Step 1: Set _object_'s `ongoing promise` to null.
    iter.data_mut().ongoing_promise = None;
    // Step 2: If _next_ is `end of iteration`, then:
    if next.get() == sentinel.as_value() {
        // Step 2.1: Set _object_'s `is finished` to true.
        iter.data_mut().is_finished = true;
        // Step 2.2: Return CreateIteratorResultObject(undefined, true).
        let result = create_iter_result(scope, HandleValue::undefined(), true)?;
        return Ok(result.as_value());
    }
    // Step 3: Otherwise, if _interface_ has a `pair asynchronously iterable declaration`:
    //         (`ReadableStream` has a value declaration.)
    // Step 4: Otherwise:
    // Step 4.1: Assert: _interface_ has a `value asynchronously iterable declaration`.
    // Step 4.2: Assert: _next_ is a value of the type that appears in the declaration.
    // Step 4.3: Let _value_ be _next_, `converted to a JavaScript value`.
    // Step 4.4: Return CreateIteratorResultObject(_value_, false).
    let result = create_iter_result(scope, next, false)?;
    Ok(result.as_value())
}

/// The _rejectSteps_ of the asynchronous iterator `next` method.
fn next_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let iter = cast_payload::<ReadableStreamAsyncIterator>(scope, payload);
    // Step 1: Set _object_'s `ongoing promise` to null.
    iter.data_mut().ongoing_promise = None;
    // Step 2: Set _object_'s `is finished` to true.
    iter.data_mut().is_finished = true;
    // Step 3: Throw _reason_.
    Err(js::exception::set_pending(
        scope,
        args.get(0),
        js::native::ExceptionStackBehavior::Capture,
    ))
}

/// The _returnSteps_ of the asynchronous iterator `return` method.
fn run_return_steps<'r>(
    scope: &'r Scope<'_>,
    iter: &ReadableStreamAsyncIterator<'_>,
    value: HandleValue<'r>,
) -> Result<Promise<'r>, ExnThrown> {
    // Step 1: Let _returnPromiseCapability_ be ! NewPromiseCapability(%Promise%).
    // Step 2: If _object_'s `is finished` is true, then:
    if iter.data().is_finished {
        // Step 2.1: Let _result_ be CreateIteratorResultObject(_value_, true).
        let result = create_iter_result(scope, value, true)?;
        // Step 2.2: Perform ! Call(_returnPromiseCapability_.[[Resolve]], undefined,
        //           « _result_ »).
        // Step 2.3: Return _returnPromiseCapability_.[[Promise]].
        return Promise::new_resolved_with_value(scope, result);
    }
    // Step 3: Set _object_'s `is finished` to true.
    iter.data_mut().is_finished = true;
    // Step 4: Return the result of running the `asynchronous iterator return` algorithm for
    //         _interface_, given _object_'s `target`, _object_, and _value_.
    asynchronous_iterator_return(scope, iter, value)
}

/// The _fulfillSteps_ of the asynchronous iterator `return` method (payload = the
/// `return` argument).
fn return_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let result = create_iter_result(scope, payload, true)?;
    Ok(result.as_value())
}

/// The Streams "asynchronous iterator return steps": cancel (unless
/// `preventCancel`) and release the reader.
fn asynchronous_iterator_return<'r>(
    scope: &'r Scope<'_>,
    iter: &ReadableStreamAsyncIterator<'_>,
    arg: HandleValue<'r>,
) -> Result<Promise<'r>, ExnThrown> {
    let reader = iter_reader(scope, iter);
    if !iter.data().prevent_cancel {
        let result = algorithms::readable_stream_reader_generic_cancel(scope, &reader, arg);
        algorithms::readable_stream_default_reader_release(scope, &reader)?;
        Ok(result)
    } else {
        algorithms::readable_stream_default_reader_release(scope, &reader)?;
        Promise::new_resolved_with_value(scope, HandleValue::undefined())
    }
}

/// `get the next iteration result`: read a chunk, returning a promise the read
/// request resolves with the raw chunk (adopting a thenable) or `end_of_iteration`
/// on close, or rejects on error.
fn get_next_iteration_result<'r>(
    scope: &'r Scope<'_>,
    iter: &ReadableStreamAsyncIterator<'_>,
    end_of_iteration: Object<'_>,
) -> Result<Promise<'r>, ExnThrown> {
    let reader = iter_reader(scope, iter);
    let promise = Promise::new_pending(scope)?;
    // Build the request directly into the call so it is never held as an
    // untraced `#[must_root]` local.
    algorithms::readable_stream_default_reader_read(
        scope,
        reader,
        ReadRequest::AsyncIter {
            promise: Heap::from(promise),
            reader: Heap::from(reader),
            end_of_iteration: Heap::from(end_of_iteration),
        },
    );
    Ok(promise)
}

fn iter_reader<'r>(
    scope: &'r Scope<'_>,
    iter: &ReadableStreamAsyncIterator<'_>,
) -> DefaultReader<'r> {
    iter.data().reader.get(scope)
}

// --- ReadRequest::AsyncIter step bodies (called from `read_request.rs`) -------

/// Close steps: release the reader, then resolve `promise` with the end-of-iteration
/// sentinel (recognised by `next()`'s fulfill steps).
pub(crate) fn async_iter_close_steps(
    scope: &Scope<'_>,
    promise: Promise<'_>,
    reader: DefaultReader<'_>,
    end_of_iteration: Object<'_>,
) -> Result<(), ExnThrown> {
    algorithms::readable_stream_default_reader_release(scope, &reader)?;
    promise.resolve(scope, end_of_iteration)
}

/// Error steps: release the reader, reject with the error.
pub(crate) fn async_iter_error_steps(
    scope: &Scope<'_>,
    promise: Promise<'_>,
    reader: DefaultReader<'_>,
    e: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    algorithms::readable_stream_default_reader_release(scope, &reader)?;
    promise.reject(scope, e)
}
