// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Readable.from`: Node's `lib/internal/streams/from.js`.

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::{Callback, CallbackArgs};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::readable::{is_buffer, readable};
use super::util::{
    as_object, call, call_method, closure, error_value, is_function, next_tick_call,
    take_exception, this_object, truthy, type_error, Captures, Props,
};
use crate::errors::{ErrorKind, NodeError, Thrown};

/// The captures of the generated `_read` and `_destroy`: `[readable, iterator, isAsync, reading,
/// originalDestroy]`, where `originalDestroy` is the readable's `_destroy` before `from` replaced
/// it.
mod captures {
    pub(super) const READABLE: u32 = 0;
    pub(super) const ITERATOR: u32 = 1;
    pub(super) const IS_ASYNC: u32 = 2;
    pub(super) const READING: u32 = 3;
    pub(super) const ORIGINAL_DESTROY: u32 = 4;
    pub(super) const COUNT: usize = 5;
}

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
    if let Some(options) = options.and_then(|options| as_object(scope, options)) {
        merged.assign(scope, options)?;
    }
    Ok(merged)
}

/// The `read` of a readable `from` creates for a string or buffer, whose captures are `[value]`.
fn read_single(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = this_object(scope, &args)?;
    let value = Captures::of(scope, payload)?.get(0)?;
    call_method(scope, this, c"push", &[value])?;
    call_method(scope, this, c"push", &[HandleValue::null()])?;
    Ok(js::value::undefined())
}

/// `Readable.from(iterable, options)`.
pub(crate) fn from<'s>(
    scope: &'s Scope<'_>,
    iterable: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    use captures::*;
    let ctor = readable::class_constructor(scope)?;
    if iterable.is_string() || is_buffer(scope, iterable)? {
        let options = readable_options(scope, options, true)?;
        let read = closure(scope, 0, read_single, &[iterable.get()])?;
        options.set_property(scope, c"read", read)?;
        return Ok(Function::construct(scope, ctor, &[options])?);
    }
    let object = as_object(scope, iterable);
    let lookup = |code: js::native::SymbolCode| -> Result<Option<HandleValue<'s>>, ExnThrown> {
        let Some(object) = object else {
            return Ok(None);
        };
        let key = js::symbol::get_well_known_key(scope, code);
        let method = object.get_property_by_id(scope, scope.root_id(key))?;
        Ok(is_function(scope, method).then_some(method))
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
    let iterator = call(scope, iterable, method, &[])?;
    let options = readable_options(scope, options, false)?;
    let created = Function::construct(scope, ctor, &[options])?;
    let original_destroy = created.val(scope, c"_destroy")?;
    let state = js::Array::with_contents(scope, &[js::value::undefined(); COUNT])?;
    let state_value = scope.root_value((*state).to_jsval_raw_throwing(scope)?);
    let captures = Captures::of(scope, (*state).to_jsval_throwing(scope)?)?;
    captures.set(READABLE, created)?;
    captures.set(ITERATOR, iterator)?;
    captures.set(IS_ASYNC, is_async)?;
    captures.set(READING, false)?;
    captures.set(ORIGINAL_DESTROY, original_destroy)?;
    created.set_property(
        scope,
        c"_read",
        closure(scope, 0, read, &[state_value.get()])?,
    )?;
    created.set_property(
        scope,
        c"_destroy",
        closure(scope, 2, destroy, &[state_value.get()])?,
    )?;
    Ok(created)
}

/// The state of a function whose captures are `[state]`.
fn state_of<'s>(scope: &'s Scope<'_>, payload: HandleValue<'_>) -> Result<Captures<'s>, ExnThrown> {
    Captures::of(scope, Captures::of(scope, payload)?.get(0)?)
}

/// The generated `_read`.
fn read(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use captures::*;
    let state = state_of(scope, payload)?;
    if state.flag(READING)? {
        return Ok(js::value::undefined());
    }
    state.set(READING, true)?;
    if state.flag(IS_ASYNC)? {
        next_async(scope, &state)?;
    } else {
        next_sync(scope, &state)?;
    }
    Ok(js::value::undefined())
}

/// Destroy the readable with the pending exception, or with `error`.
fn fail(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let error = match error {
        Some(error) => scope.root_value(error.get()),
        None => take_exception(scope),
    };
    call_method(scope, state.obj(captures::READABLE)?, c"destroy", &[error])?;
    Ok(())
}

/// Push `value`, or fail on `null`. Returns whether the readable takes more.
fn push_value(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    use captures::*;
    if value.is_null() {
        state.set(READING, false)?;
        let error = error_value(scope, null_values())?;
        fail(scope, state, Some(error))?;
        return Ok(None);
    }
    let pushed = truthy(
        scope,
        call_method(scope, state.obj(READABLE)?, c"push", &[value])?,
    );
    if !pushed {
        state.set(READING, false)?;
    }
    Ok(Some(pushed))
}

/// Whether `value` is a thenable.
fn is_thenable(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    Ok(match as_object(scope, value) {
        Some(object) => is_function(scope, object.get_property(scope, c"then")?),
        None => false,
    })
}

/// `const { value, done } = result`, where `expression` names `result` in the `TypeError` for
/// `null` and `undefined`.
fn iterator_result<'s>(
    scope: &'s Scope<'_>,
    result: HandleValue<'s>,
    expression: &str,
) -> Result<(HandleValue<'s>, bool), ExnThrown> {
    if result.is_null() || result.is_undefined() {
        let kind = if result.is_null() {
            "null"
        } else {
            "undefined"
        };
        return Err(type_error(
            scope,
            &format!("Cannot destructure property 'value' of '{expression}' as it is {kind}."),
        ));
    }
    let result = Object::from_value_coerce(scope, result)?;
    let value = result.val(scope, c"value")?;
    let done = truthy(scope, result.get_property(scope, c"done")?);
    Ok((value, done))
}

/// Node's `nextSyncWithSyncValues` and `nextSyncWithAsyncValues`: pull from the iterator until the
/// readable is full, waiting for promise values. An exception destroys the readable.
fn next_sync(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    let iterator = state.obj(captures::ITERATOR)?;
    loop {
        match next_sync_step(scope, state, iterator) {
            Ok(true) => continue,
            Ok(false) => return Ok(()),
            Err(_) => return fail(scope, state, None),
        }
    }
}

/// One iteration of [`next_sync`]. Returns whether to continue.
fn next_sync_step(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    iterator: Object<'_>,
) -> Result<bool, ExnThrown> {
    let result = call_method(scope, iterator, c"next", &[] as &[HandleValue<'_>])?;
    let (value, done) = iterator_result(scope, result, "iterator.next(...)")?;
    if done {
        let readable = state.obj(captures::READABLE)?;
        call_method(scope, readable, c"push", &[HandleValue::null()])?;
        return Ok(false);
    }
    if is_thenable(scope, value)? {
        await_then(scope, state, value, on_sync_value)?;
        return Ok(false);
    }
    Ok(push_value(scope, state, value)? == Some(true))
}

/// Call `body` with `[state, value]` captures once `promise` fulfills, and fail if it rejects.
fn await_then(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    promise: HandleValue<'_>,
    body: Callback,
) -> Result<(), ExnThrown> {
    let promise = js::Promise::call_original_resolve(scope, promise)?;
    let state_value = scope.root_value(state.array_value()?);
    let fulfilled = closure(scope, 1, body, &[state_value.get()])?;
    let rejected = closure(scope, 1, on_rejected, &[state_value.get()])?;
    promise.then(scope, Some(fulfilled), Some(rejected))?;
    Ok(())
}

fn on_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    fail(scope, &state, Some(args.get(0)))?;
    Ok(js::value::undefined())
}

/// A promise value of a sync iterator fulfilled.
fn on_sync_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    match push_value(scope, &state, args.get(0)) {
        Ok(Some(true)) => next_sync(scope, &state)?,
        Ok(_) => {}
        Err(_) => fail(scope, &state, None)?,
    }
    Ok(js::value::undefined())
}

/// Node's `nextAsync`: await the iterator's next result, and push it.
fn next_async(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    let iterator = state.obj(captures::ITERATOR)?;
    let result = match call_method(scope, iterator, c"next", &[]) {
        Ok(result) => result,
        Err(_) => return fail(scope, state, None),
    };
    await_then(scope, state, result, on_async_result)
}

/// An async iterator's `next()` fulfilled with `result`.
fn on_async_result(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let step = (|| -> Result<bool, ExnThrown> {
        let (value, done) = iterator_result(scope, args.get(0), "(intermediate value)")?;
        if done {
            let readable = state.obj(captures::READABLE)?;
            call_method(scope, readable, c"push", &[HandleValue::null()])?;
            return Ok(false);
        }
        Ok(push_value(scope, &state, value)? == Some(true))
    })();
    match step {
        Ok(true) => next_async(scope, &state)?,
        Ok(false) => {}
        Err(_) => fail(scope, &state, None)?,
    }
    Ok(js::value::undefined())
}

/// The captures of the reactions that run Node.js's async `close(error)`: `[callback, error,
/// iterator, done]`.
mod close {
    pub(super) const CALLBACK: u32 = 0;
    pub(super) const ERROR: u32 = 1;
    pub(super) const ITERATOR: u32 = 2;
    pub(super) const DONE: u32 = 3;
}

/// The generated `_destroy(error, callback)`: call the readable's original `_destroy` with `error`
/// and [`after_original_destroy`].
fn destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let error = args.get(0);
    let after = closure(
        scope,
        1,
        after_original_destroy,
        &[state.array_value()?, error.get(), args.get(1).get()],
    )?;
    let this = scope.root_value(args.this());
    let original = state.get(captures::ORIGINAL_DESTROY)?;
    call(
        scope,
        this,
        original,
        &[error, after.to_jsval_throwing(scope)?],
    )?;
    Ok(js::value::undefined())
}

/// The callback the original `_destroy` gets, whose captures are `[state, error, callback]`: close
/// the iterator with the error it passes, or else `error`.
fn after_original_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    let error = if truthy(scope, args.get(0)) {
        args.get(0)
    } else {
        captures.get(1)?
    };
    close_iterator(scope, &state, error, captures.get(2)?)?;
    Ok(js::value::undefined())
}

/// Node.js's async `close(error)`, which closes the iterator with `throw` if there is an error and
/// then with `return`, awaiting each result and its `value`, followed by a tick calling
/// `callback`. Each `await` is a reaction to a resolved promise, so the callback runs after as many
/// microtasks as in Node.js.
fn close_iterator(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    error: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let iterator = state.get(captures::ITERATOR)?;
    let undefined = HandleValue::undefined();
    let captures = js::Array::with_contents(scope, &[callback, error, iterator, undefined])?;
    let captures = Captures::of(scope, captures.to_jsval_throwing(scope)?)?;
    let had_error = !error.is_null() && !error.is_undefined();
    let iterator = state.obj(captures::ITERATOR)?;
    let step = (|| -> Result<(), ExnThrown> {
        if had_error && super::util::has_method(scope, iterator, c"throw")? {
            let result = call_method(scope, iterator, c"throw", &[error])?;
            return await_value(scope, &captures, result, after_throw);
        }
        close_with_return(scope, &captures)
    })();
    if step.is_err() {
        close_failed_now(scope, &captures)?;
    }
    Ok(())
}

/// `await value`, continuing with `then` and the captures, or failing `close`.
fn await_value(
    scope: &Scope<'_>,
    captures: &Captures<'_>,
    value: HandleValue<'_>,
    then: Callback,
) -> Result<(), ExnThrown> {
    let promise = js::Promise::call_original_resolve(scope, value)?;
    let fulfilled = Function::new_callback(scope, c"", 1, then, captures.array)?;
    let rejected = Function::new_callback(scope, c"", 1, close_rejected, captures.array)?;
    promise.then(scope, Some(*fulfilled), Some(*rejected))?;
    Ok(())
}

/// The `return` step of `close`, or its end if the iterator has no `return`.
fn close_with_return(scope: &Scope<'_>, captures: &Captures<'_>) -> Result<(), ExnThrown> {
    let iterator = captures.obj(close::ITERATOR)?;
    if super::util::has_method(scope, iterator, c"return")? {
        let result = call_method(scope, iterator, c"return", &[])?;
        return await_value(scope, captures, result, after_return);
    }
    close_settled(scope, captures, None)
}

/// `const { value, done } = await iterator.throw(error); await value;`
fn after_throw(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let step = (|| -> Result<(), ExnThrown> {
        let result = as_object(scope, args.get(0));
        let (value, done) = match result {
            Some(result) => (
                result.get_property(scope, c"value")?,
                result.get_property(scope, c"done")?,
            ),
            None => (HandleValue::undefined(), HandleValue::undefined()),
        };
        captures.array.set_element(scope, close::DONE, done)?;
        await_value(scope, &captures, value, after_throw_value)
    })();
    if step.is_err() {
        close_failed_now(scope, &captures)?;
    }
    Ok(js::value::undefined())
}

/// `if (done) return;`, or on to the `return` step.
fn after_throw_value(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let step = if truthy(scope, captures.get(close::DONE)?) {
        close_settled(scope, &captures, None)
    } else {
        close_with_return(scope, &captures)
    };
    if step.is_err() {
        close_failed_now(scope, &captures)?;
    }
    Ok(js::value::undefined())
}

/// `const { value } = await iterator.return(); await value;`
fn after_return(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let step = (|| -> Result<(), ExnThrown> {
        let value = match as_object(scope, args.get(0)) {
            Some(result) => result.get_property(scope, c"value")?,
            None => HandleValue::undefined(),
        };
        await_value(scope, &captures, value, after_return_value)
    })();
    if step.is_err() {
        close_failed_now(scope, &captures)?;
    }
    Ok(js::value::undefined())
}

/// The end of `close`.
fn after_return_value(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    close_settled(scope, &captures, None)?;
    Ok(js::value::undefined())
}

/// An awaited value rejected, whose reason is the argument.
fn close_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let reason = args.get(0);
    close_settled(scope, &captures, Some(reason))?;
    Ok(js::value::undefined())
}

/// `close` threw synchronously: it settles with the pending exception.
fn close_failed_now(scope: &Scope<'_>, captures: &Captures<'_>) -> Result<(), ExnThrown> {
    let thrown = take_exception(scope);
    close_settled(scope, captures, Some(thrown))
}

/// `close` settled, rejected with `failure` if given: a reaction to that, one microtask later,
/// queues the tick calling the callback with the error, or with Node's
/// `aggregateTwoErrors(error, failure)` after a rejection.
fn close_settled(
    scope: &Scope<'_>,
    captures: &Captures<'_>,
    failure: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let error = captures.get(close::ERROR)?;
    let error = match failure {
        Some(failure) => super::destroy::aggregate_two_errors(scope, error, failure)?,
        None => error,
    };
    let payload = js::Array::with_contents(scope, &[captures.get(close::CALLBACK)?, error])?;
    let job = Function::new_callback(scope, c"", 0, call_back_in_tick, *payload)?;
    js::jobs::queue_microtask(scope, &job)
}

/// Queue a tick calling the captured callback with the captured error, as
/// `process.nextTick(cb, error)` does.
fn call_back_in_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    next_tick_call(scope, captures.get(0)?, &[captures.get(1)?])?;
    Ok(js::value::undefined())
}
