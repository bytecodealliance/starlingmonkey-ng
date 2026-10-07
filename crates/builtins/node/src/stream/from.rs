// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Readable.from`: Node's `lib/internal/streams/from.js`.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, Callback, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::readable::{is_buffer, readable};
use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::support::{has_method, is_thenable};

/// `ERR_STREAM_NULL_VALUES`.
fn null_values() -> NodeError {
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_STREAM_NULL_VALUES",
        "May not write null values to stream",
    )
}

/// The options of the readable `from` creates: `{ objectMode: true, highWaterMark: 1, ...opts }`,
/// without the high water mark for a string or buffer.
fn readable_options<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'_>>,
    single: bool,
) -> Result<Object<'s>, ExnThrown> {
    let merged = Object::new_plain(scope)?;
    merged.set_property(scope, c"objectMode", true)?;
    if !single {
        merged.set_property(scope, c"highWaterMark", 1)?;
    }
    if let Some(options) = options.and_then(|options| Object::from_value(scope, options).ok()) {
        merged.assign(scope, options)?;
    }
    Ok(merged)
}

/// The `read` of a readable `from` creates for a string or buffer, whose payload is the value.
fn read_single(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    Function::call_by_name(scope, this.handle(), c"push", &[payload])?;
    Function::call_by_name(scope, this.handle(), c"push", &[HandleValue::null()])?;
    Ok(js::value::undefined())
}

/// The state of a readable `from` creates for an iterable: the readable, the iterator, whether it
/// is async, whether a read is in progress, the readable's `_destroy` before `from` replaced it,
/// and the reactions to the values and results the iterator produces.
#[core_runtime::jsclass(hidden)]
pub(crate) struct FromState {
    readable: Heap<js::object::Object>,
    iterator: Heap<js::object::Object>,
    original_destroy: Heap<Value>,
    on_sync_value: Option<Heap<js::object::Object>>,
    on_async_result: Option<Heap<js::object::Object>>,
    on_rejected: Option<Heap<js::object::Object>>,
    #[no_trace]
    is_async: bool,
    #[no_trace]
    reading: bool,
}

#[core_runtime::jsmethods]
impl FromState {}

/// `Readable.from(iterable, options)`.
pub(crate) fn from<'s>(
    scope: &'s Scope<'_>,
    iterable: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let ctor = readable::class_constructor(scope)?;
    if iterable.is_string() || is_buffer(scope, iterable)? {
        let options = readable_options(scope, options, true)?;
        let read = Function::new_callback(scope, c"read", 0, read_single, iterable)?;
        options.set_property(scope, c"read", *read)?;
        return Ok(Function::construct(scope, ctor, &[options])?);
    }
    let object = Object::from_value(scope, iterable).ok();
    let lookup = |code: js::native::SymbolCode| -> Result<Option<HandleValue<'s>>, ExnThrown> {
        let Some(object) = object else {
            return Ok(None);
        };
        let key = js::symbol::get_well_known_key(scope, code);
        let method = object.get_property_by_id(scope, scope.root_id(key))?;
        Ok(js::function::is_callable(method).then_some(method))
    };
    let (method, is_async) = match lookup(js::native::SymbolCode::asyncIterator)? {
        Some(method) => (method, true),
        None => match lookup(js::native::SymbolCode::iterator)? {
            Some(method) => (method, false),
            None => {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "iterable",
                    "an instance of Iterable",
                    iterable,
                )
                .into())
            }
        },
    };
    let iterator = Function::call(scope, iterable, method, js::function::EmptyArgs)?;
    let iterator = Object::from_value_coerce(scope, iterator)?;
    let options = readable_options(scope, options, false)?;
    let created = Function::construct(scope, ctor, &[options])?;
    let original_destroy = created.get_property(scope, c"_destroy")?;
    let state = js::class::create_instance_with::<FromStateImpl>(scope, |_| FromStateImpl {
        readable: Heap::from(created),
        iterator: Heap::from(iterator),
        original_destroy: Heap::from(original_destroy.get()),
        on_sync_value: None,
        on_async_result: None,
        on_rejected: None,
        is_async,
        reading: false,
    })?;
    let reaction = |body: Callback| Function::new_callback(scope, c"", 1, body, state);
    let on_sync_value = reaction(on_sync_value)?;
    let on_async_result = reaction(on_async_result)?;
    let on_rejected = reaction(on_rejected)?;
    {
        let mut data = state.data_mut();
        data.on_sync_value = Some(Heap::from(*on_sync_value));
        data.on_async_result = Some(Heap::from(*on_async_result));
        data.on_rejected = Some(Heap::from(*on_rejected));
    }
    let read = Function::new_callback(scope, c"_read", 0, read, state)?;
    created.set_property(scope, c"_read", *read)?;
    let destroy = Function::new_callback(scope, c"_destroy", 2, destroy, state)?;
    created.set_property(scope, c"_destroy", *destroy)?;
    Ok(created)
}

/// The generated `_read`, whose payload is the [`FromState`].
fn read(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromState>(scope, payload);
    if state.data().reading {
        return Ok(js::value::undefined());
    }
    state.data_mut().reading = true;
    if state.data().is_async {
        next_async(scope, state)?;
    } else {
        next_sync(scope, state)?;
    }
    Ok(js::value::undefined())
}

/// Destroy the readable with the pending exception, or with `error`.
fn fail(
    scope: &Scope<'_>,
    state: FromState<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let error = match error {
        Some(error) => scope.root_value(error.get()),
        None => js::exception::take_pending_or_undefined(scope),
    };
    let readable = state.data().readable.get(scope);
    Function::call_by_name(scope, readable.handle(), c"destroy", &[error])?;
    Ok(())
}

/// Push `value`, or fail on `null`. Returns whether the readable takes more.
fn push_value(
    scope: &Scope<'_>,
    state: FromState<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    if value.is_null() {
        state.data_mut().reading = false;
        let error = null_values().to_value(scope)?;
        fail(scope, state, Some(error))?;
        return Ok(None);
    }
    let readable = state.data().readable.get(scope);
    let pushed = js::conversion::to_boolean(Function::call_by_name(
        scope,
        readable.handle(),
        c"push",
        &[value],
    )?);
    if !pushed {
        state.data_mut().reading = false;
    }
    Ok(Some(pushed))
}

/// `const { value, done } = result`, where `expression` names `result` in the `TypeError` for
/// `null` and `undefined`.
fn iterator_result<'s>(
    scope: &'s Scope<'_>,
    result: HandleValue<'s>,
    expression: &str,
) -> Result<(HandleValue<'s>, bool), ExnThrown> {
    if result.is_null_or_undefined() {
        let kind = if result.is_null() {
            "null"
        } else {
            "undefined"
        };
        return Err(js::error::TypeError(String::from(&format!(
            "Cannot destructure property 'value' of '{expression}' as it is {kind}."
        )))
        .throw(scope));
    }
    let result = Object::from_value_coerce(scope, result)?;
    let value = js::iteration::iter_result_value(scope, &result)?;
    let done = js::iteration::iter_result_done(scope, &result)?;
    Ok((value, done))
}

/// End the readable.
fn push_end(scope: &Scope<'_>, state: FromState<'_>) -> Result<(), ExnThrown> {
    let readable = state.data().readable.get(scope);
    Function::call_by_name(scope, readable.handle(), c"push", &[HandleValue::null()])?;
    Ok(())
}

/// Node's `nextSyncWithSyncValues` and `nextSyncWithAsyncValues`: pull from the iterator until the
/// readable is full, waiting for promise values. An exception destroys the readable.
fn next_sync(scope: &Scope<'_>, state: FromState<'_>) -> Result<(), ExnThrown> {
    loop {
        match next_sync_step(scope, state) {
            Ok(true) => continue,
            Ok(false) => return Ok(()),
            Err(_) => return fail(scope, state, None),
        }
    }
}

/// One iteration of [`next_sync`]. Returns whether to continue.
fn next_sync_step(scope: &Scope<'_>, state: FromState<'_>) -> Result<bool, ExnThrown> {
    let iterator = state.data().iterator.get(scope);
    let result =
        Function::call_by_name(scope, iterator.handle(), c"next", js::function::EmptyArgs)?;
    let (value, done) = iterator_result(scope, result, "iterator.next(...)")?;
    if done {
        push_end(scope, state)?;
        return Ok(false);
    }
    if is_thenable(scope, value)? {
        let on_sync_value = state.data().on_sync_value.get(scope);
        await_then(scope, state, value, on_sync_value)?;
        return Ok(false);
    }
    Ok(push_value(scope, state, value)? == Some(true))
}

/// `await promise`, continuing with `fulfilled`, and failing if it rejects.
fn await_then(
    scope: &Scope<'_>,
    state: FromState<'_>,
    promise: HandleValue<'_>,
    fulfilled: Option<Object<'_>>,
) -> Result<(), ExnThrown> {
    let promise = js::Promise::call_original_resolve(scope, promise)?;
    let rejected = state.data().on_rejected.get(scope);
    promise.add_reactions(scope, fulfilled, rejected)
}

fn on_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromState>(scope, payload);
    fail(scope, state, Some(args.get(0)))?;
    Ok(js::value::undefined())
}

/// A promise value of a sync iterator fulfilled.
fn on_sync_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromState>(scope, payload);
    match push_value(scope, state, args.get(0)) {
        Ok(Some(true)) => next_sync(scope, state)?,
        Ok(_) => {}
        Err(_) => fail(scope, state, None)?,
    }
    Ok(js::value::undefined())
}

/// Node's `nextAsync`: await the iterator's next result, and push it.
fn next_async(scope: &Scope<'_>, state: FromState<'_>) -> Result<(), ExnThrown> {
    let iterator = state.data().iterator.get(scope);
    let result =
        match Function::call_by_name(scope, iterator.handle(), c"next", js::function::EmptyArgs) {
            Ok(result) => result,
            Err(_) => return fail(scope, state, None),
        };
    let on_async_result = state.data().on_async_result.get(scope);
    await_then(scope, state, result, on_async_result)
}

/// An async iterator's `next()` fulfilled with `result`.
fn on_async_result(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromState>(scope, payload);
    let step = (|| -> Result<bool, ExnThrown> {
        let (value, done) = iterator_result(scope, args.get(0), "(intermediate value)")?;
        if done {
            push_end(scope, state)?;
            return Ok(false);
        }
        Ok(push_value(scope, state, value)? == Some(true))
    })();
    match step {
        Ok(true) => next_async(scope, state)?,
        Ok(false) => {}
        Err(_) => fail(scope, state, None)?,
    }
    Ok(js::value::undefined())
}

/// The generated `_destroy(error, callback)`, whose payload is the [`FromState`]: call the
/// readable's original `_destroy` with `error` and [`after_original_destroy`].
fn destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromState>(scope, payload);
    let error = args.get(0);
    let (iterator, original) = {
        let data = state.data();
        (data.iterator.get(scope), data.original_destroy.get(scope))
    };
    let close = js::class::create_instance_with::<FromCloseImpl>(scope, |_| FromCloseImpl {
        callback: Heap::from(args.get(1).get()),
        error: Heap::from(error.get()),
        iterator: Heap::from(iterator),
        done: Heap::default(),
    })?;
    let after = Function::new_callback(scope, c"", 1, after_original_destroy, close)?;
    let this = scope.root_value(args.this());
    Function::call(
        scope,
        this,
        original,
        &[error, (*after).to_jsval_throwing(scope)?],
    )?;
    Ok(js::value::undefined())
}

/// Node.js's async `close(error)` of a `from` readable's iterator: the `_destroy` callback, the
/// error, the iterator, and the `done` of its `throw()` result.
#[core_runtime::jsclass(hidden)]
pub(crate) struct FromClose {
    callback: Heap<Value>,
    error: Heap<Value>,
    iterator: Heap<js::object::Object>,
    done: Heap<Value>,
}

#[core_runtime::jsmethods]
impl FromClose {}

/// The callback the original `_destroy` gets, whose payload is the [`FromClose`]: close the
/// iterator with the error it passes, or else the `_destroy` error.
fn after_original_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let close = cast_payload::<FromClose>(scope, payload);
    if js::conversion::to_boolean(args.get(0)) {
        close.data().error.set(args.get(0).get());
    }
    close_iterator(scope, close)?;
    Ok(js::value::undefined())
}

/// Node.js's async `close(error)`, which closes the iterator with `throw` if there is an error and
/// then with `return`, awaiting each result and its `value`, followed by a tick calling
/// `callback`. Each `await` is a reaction to a resolved promise, so the callback runs after as many
/// microtasks as in Node.js.
fn close_iterator(scope: &Scope<'_>, close: FromClose<'_>) -> Result<(), ExnThrown> {
    let (iterator, error) = {
        let data = close.data();
        (data.iterator.get(scope), data.error.get(scope))
    };
    let had_error = !error.is_null_or_undefined();
    let step = (|| -> Result<(), ExnThrown> {
        if had_error && has_method(scope, iterator, c"throw")? {
            let result = Function::call_by_name(scope, iterator.handle(), c"throw", &[error])?;
            return await_value(scope, close, result, after_throw);
        }
        close_with_return(scope, close)
    })();
    if step.is_err() {
        close_failed_now(scope, close)?;
    }
    Ok(())
}

/// `await value`, continuing with `then`, or failing `close`.
fn await_value(
    scope: &Scope<'_>,
    close: FromClose<'_>,
    value: HandleValue<'_>,
    then: Callback,
) -> Result<(), ExnThrown> {
    let promise = js::Promise::call_original_resolve(scope, value)?;
    let fulfilled = Function::new_callback(scope, c"", 1, then, close)?;
    let rejected = Function::new_callback(scope, c"", 1, close_rejected, close)?;
    promise.add_reactions(scope, Some(*fulfilled), Some(*rejected))
}

/// The `return` step of `close`, or its end if the iterator has no `return`.
fn close_with_return(scope: &Scope<'_>, close: FromClose<'_>) -> Result<(), ExnThrown> {
    let iterator = close.data().iterator.get(scope);
    if has_method(scope, iterator, c"return")? {
        let result =
            Function::call_by_name(scope, iterator.handle(), c"return", js::function::EmptyArgs)?;
        return await_value(scope, close, result, after_return);
    }
    close_settled(scope, close, None)
}

/// `const { value, done } = await iterator.throw(error); await value;`
fn after_throw(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let close = cast_payload::<FromClose>(scope, payload);
    let step = (|| -> Result<(), ExnThrown> {
        let result = Object::from_value(scope, args.get(0)).ok();
        let (value, done) = match result {
            Some(result) => (
                result.get_property(scope, c"value")?,
                result.get_property(scope, c"done")?,
            ),
            None => (HandleValue::undefined(), HandleValue::undefined()),
        };
        close.data().done.set(done.get());
        await_value(scope, close, value, after_throw_value)
    })();
    if step.is_err() {
        close_failed_now(scope, close)?;
    }
    Ok(js::value::undefined())
}

/// `if (done) return;`, or on to the `return` step.
fn after_throw_value(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let close = cast_payload::<FromClose>(scope, payload);
    let done = js::conversion::to_boolean(close.data().done.get(scope));
    let step = if done {
        close_settled(scope, close, None)
    } else {
        close_with_return(scope, close)
    };
    if step.is_err() {
        close_failed_now(scope, close)?;
    }
    Ok(js::value::undefined())
}

/// `const { value } = await iterator.return(); await value;`
fn after_return(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let close = cast_payload::<FromClose>(scope, payload);
    let step = (|| -> Result<(), ExnThrown> {
        let value = match Object::from_value(scope, args.get(0)).ok() {
            Some(result) => result.get_property(scope, c"value")?,
            None => HandleValue::undefined(),
        };
        await_value(scope, close, value, after_return_value)
    })();
    if step.is_err() {
        close_failed_now(scope, close)?;
    }
    Ok(js::value::undefined())
}

/// The end of `close`.
fn after_return_value(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    close_settled(scope, cast_payload::<FromClose>(scope, payload), None)?;
    Ok(js::value::undefined())
}

/// An awaited value rejected, whose reason is the argument.
fn close_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let close = cast_payload::<FromClose>(scope, payload);
    close_settled(scope, close, Some(args.get(0)))?;
    Ok(js::value::undefined())
}

/// `close` threw synchronously: it settles with the pending exception.
fn close_failed_now(scope: &Scope<'_>, close: FromClose<'_>) -> Result<(), ExnThrown> {
    let thrown = js::exception::take_pending_or_undefined(scope);
    close_settled(scope, close, Some(thrown))
}

/// `close` settled, rejected with `failure` if given: a reaction to that, one microtask later,
/// queues the tick calling the callback with the error, or with Node's
/// `aggregateTwoErrors(error, failure)` after a rejection.
fn close_settled(
    scope: &Scope<'_>,
    close: FromClose<'_>,
    failure: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    if let Some(failure) = failure {
        let error = close.data().error.get(scope);
        let error = super::destroy::aggregate_two_errors(scope, error, failure)?;
        close.data().error.set(error.get());
    }
    let job = Function::new_callback(scope, c"", 0, call_back_in_tick, close)?;
    js::jobs::queue_microtask(scope, &job)
}

/// Queue a tick calling the `_destroy` callback with the error, as `process.nextTick(cb, error)`
/// does, whose payload is the [`FromClose`].
fn call_back_in_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let close = cast_payload::<FromClose>(scope, payload);
    let (callback, error) = {
        let data = close.data();
        (data.callback.get(scope), data.error.get(scope))
    };
    if js::function::is_callable(callback) {
        js::jobs::queue_tick(scope, callback, &[error])?;
    }
    Ok(js::value::undefined())
}
