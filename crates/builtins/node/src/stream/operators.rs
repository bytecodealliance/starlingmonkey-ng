// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The iterator helpers of `Readable.prototype`: `map`, `filter`, `flatMap`, `drop` and `take`,
//! which return a `Readable.from` stream, and `every`, `forEach`, `reduce`, `toArray`, `some` and
//! `find`, which return a promise. Node's `lib/internal/streams/operators.js`.
//!
//! Node writes these as async generators and async functions. Here a generator is an object with
//! `next`, `return` and `throw` whose body is a state machine of promise reactions (see
//! [`new_generator`]), and a `for await` loop is a [`loop_open`] loop that calls back for each
//! value and at the end.

use std::ffi::CStr;

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{Callback, CallbackArgs};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::readable::array_push;
use super::util::{
    as_object, call, call_method, closure, error_value, is_function, listen, same, take_exception,
    truthy, type_error, Captures, Props,
};
use crate::errors::{
    out_of_range_received, validate_abort_signal, validate_function, validate_integer,
    validate_object, ErrorKind, NodeError, Thrown,
};

/// `Number.MAX_SAFE_INTEGER`, the default maximum of `validateInteger`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A new state array of `count` `undefined` values.
fn new_state<'s>(scope: &'s Scope<'_>, count: usize) -> Result<Captures<'s>, ExnThrown> {
    let array = js::Array::new(scope, count)?;
    for index in 0..count as u32 {
        array.set_element(scope, index, HandleValue::undefined())?;
    }
    Captures::of(scope, (*array).to_jsval_throwing(scope)?)
}

/// The state of a function whose captures are `[state]`.
fn state_of<'s>(scope: &'s Scope<'_>, payload: HandleValue<'_>) -> Result<Captures<'s>, ExnThrown> {
    Captures::of(scope, Captures::of(scope, payload)?.get(0)?)
}

/// A function running `body` with the captures `[state]`.
fn bind<'s>(
    scope: &'s Scope<'_>,
    state: &Captures<'_>,
    nargs: u32,
    body: Callback,
) -> Result<Object<'s>, ExnThrown> {
    closure(scope, nargs, body, &[state.array_value()?])
}

/// The length of the array `array`.
fn array_len(scope: &Scope<'_>, array: Object<'_>) -> Result<u32, ExnThrown> {
    Ok(array.num(scope, c"length")? as u32)
}

/// Remove the first element of the array `array`.
fn array_shift(scope: &Scope<'_>, array: Object<'_>) -> Result<(), ExnThrown> {
    let length = array_len(scope, array)?;
    if length == 0 {
        return Ok(());
    }
    for index in 1..length {
        let value = array.get_element(scope, index)?;
        array.set_element(scope, index - 1, value)?;
    }
    array.set_property(scope, c"length", length - 1)
}

/// Run `fulfilled` or `rejected` once `Promise.resolve(value)` settles.
fn await_value(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    fulfilled: Object<'_>,
    rejected: Object<'_>,
) -> Result<(), ExnThrown> {
    let value = scope.root_value(value.get());
    js::Promise::call_original_resolve(scope, value)?.then(
        scope,
        Some(fulfilled),
        Some(rejected),
    )?;
    Ok(())
}

/// The promise at `index` of `captures`.
fn promise_at<'s>(
    scope: &'s Scope<'_>,
    captures: &Captures<'s>,
    index: u32,
) -> Result<js::Promise<'s>, ExnThrown> {
    captures
        .obj(index)?
        .cast::<js::Promise>()
        .map_err(|_| type_error(scope, "not a promise"))
}

/// Resolve or reject `promise` unless it settled.
fn settle(
    scope: &Scope<'_>,
    promise: js::Promise<'_>,
    reject: bool,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if !promise.is_pending() {
        return Ok(());
    }
    if reject {
        promise.reject(scope, value)
    } else {
        promise.resolve(scope, value)
    }
}

/// A new `AbortError`, with `cause` if given.
fn abort_error<'s>(
    scope: &'s Scope<'_>,
    cause: Option<HandleValue<'_>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    crate::errors::abort_error(scope, cause)
}

/// `options?.signal?.aborted`, for `options` an object or `undefined`.
fn signal_aborted(scope: &Scope<'_>, options: HandleValue<'_>) -> Result<bool, ExnThrown> {
    let Some(options) = as_object(scope, options) else {
        return Ok(false);
    };
    let signal = options.val(scope, c"signal")?;
    match as_object(scope, signal) {
        Some(signal) => Ok(truthy(scope, signal.val(scope, c"aborted")?)),
        None => Ok(false),
    }
}

/// `options?.signal?.reason`.
fn signal_reason<'s>(
    scope: &'s Scope<'_>,
    options: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let signal = match as_object(scope, options) {
        Some(options) => options.val(scope, c"signal")?,
        None => return Ok(HandleValue::undefined()),
    };
    match as_object(scope, signal) {
        Some(signal) => signal.val(scope, c"reason"),
        None => Ok(HandleValue::undefined()),
    }
}

/// The validation the operators share: `options` must be `null`, `undefined` or an object, and
/// `options.signal` must be `null`, `undefined` or an `AbortSignal`. Returns `options` as an
/// object, or `undefined`.
fn validate_options<'s>(
    scope: &'s Scope<'_>,
    options: HandleValue<'s>,
) -> Result<HandleValue<'s>, Thrown> {
    if options.is_null() || options.is_undefined() {
        return Ok(HandleValue::undefined());
    }
    let object = validate_object(scope, options, "options")?;
    let signal = object.val(scope, c"signal")?;
    if !signal.is_null() && !signal.is_undefined() {
        validate_abort_signal(scope, object.val(scope, c"signal")?, "options.signal")?;
    }
    Ok(options)
}

/// The kinds of request a generator takes.
mod request {
    pub(super) const NEXT: i32 = 0;
    pub(super) const RETURN: i32 = 1;
    pub(super) const THROW: i32 = 2;
}

/// The phases of a generator: before its first `next()`, suspended at a `yield`, and completed.
mod phase {
    pub(super) const START: i32 = 0;
    pub(super) const YIELDED: i32 = 1;
    pub(super) const DONE: i32 = 2;
}

/// The captures of a generator's methods: `[body, requests, running, phase, start, resume]`.
/// `requests` is the queue of `[kind, value, promise]` its `next`, `return` and `throw` calls
/// add. `body` is the state of the body, whose element 0 is this array.
mod generator {
    pub(super) const BODY: u32 = 0;
    pub(super) const REQUESTS: u32 = 1;
    pub(super) const RUNNING: u32 = 2;
    pub(super) const PHASE: u32 = 3;
    pub(super) const START: u32 = 4;
    pub(super) const RESUME: u32 = 5;
    pub(super) const COUNT: usize = 6;
}

/// The element of a body state that holds its generator's captures.
const GEN: u32 = 0;

/// A new async generator object. Its first `next()` runs `start` with the captures `[body]`. A
/// request after the body yielded runs `resume` with those captures and the arguments `(kind,
/// value)`, the kind of [`request`] and its argument. Each step of the body ends with
/// [`gen_yield`], [`gen_return`] or [`gen_throw`], and an exception `start` or `resume` throws
/// ends it as [`gen_throw`] does. The requests are queued and run one at a time, as an async
/// generator's are.
fn new_generator<'s>(
    scope: &'s Scope<'_>,
    body: &Captures<'_>,
    start: Callback,
    resume: Callback,
) -> Result<Object<'s>, ExnThrown> {
    use generator::*;
    let gen = new_state(scope, COUNT)?;
    gen.set(BODY, body.array)?;
    gen.set(REQUESTS, *js::Array::with_contents(scope, &[] as &[Value])?)?;
    gen.set(RUNNING, false)?;
    gen.set(PHASE, phase::START)?;
    gen.set(START, bind(scope, body, 0, start)?)?;
    gen.set(RESUME, bind(scope, body, 2, resume)?)?;
    body.set(GEN, gen.array)?;
    let object = Object::new_plain(scope)?;
    object.set_prototype(scope, js::class::get_async_iterator_prototype(scope)?)?;
    for (name, method) in [
        (c"next", gen_next as Callback),
        (c"return", gen_return_method),
        (c"throw", gen_throw_method),
    ] {
        let function = Function::new_callback(scope, name, 1, method, gen.array)?;
        object.set_property(scope, name, *function)?;
    }
    Ok(object)
}

/// The generator captures of the body state `body`.
fn gen_of<'s>(scope: &'s Scope<'_>, body: &Captures<'s>) -> Result<Captures<'s>, ExnThrown> {
    Captures::of(scope, body.get(GEN)?)
}

fn gen_next(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    gen_request(scope, payload, request::NEXT, args.get(0))
}

fn gen_return_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    gen_request(scope, payload, request::RETURN, args.get(0))
}

fn gen_throw_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    gen_request(scope, payload, request::THROW, args.get(0))
}

/// Queue a request, run it if the generator isn't running, and return its promise.
fn gen_request(
    scope: &Scope<'_>,
    payload: HandleValue<'_>,
    kind: i32,
    value: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let gen = Captures::of(scope, payload)?;
    let promise = js::Promise::new_pending(scope)?;
    let entry = js::Array::with_contents(
        scope,
        &[
            js::value::from_i32(kind),
            value.get(),
            promise.to_jsval_raw_throwing(scope)?,
        ],
    )?;
    array_push(
        scope,
        gen.obj(generator::REQUESTS)?,
        (*entry).to_jsval_throwing(scope)?,
    )?;
    gen_drain(scope, &gen)?;
    promise.to_jsval_raw_throwing(scope)
}

/// The first queued request, as `(kind, value, promise)`.
fn gen_front<'s>(
    scope: &'s Scope<'_>,
    gen: &Captures<'s>,
) -> Result<Option<(i32, HandleValue<'s>, js::Promise<'s>)>, ExnThrown> {
    let requests = gen.obj(generator::REQUESTS)?;
    if array_len(scope, requests)? == 0 {
        return Ok(None);
    }
    let entry = Captures::of(scope, requests.get_element(scope, 0)?)?;
    Ok(Some((
        entry.get(0)?.to_int32(),
        entry.get(1)?,
        promise_at(scope, &entry, 2)?,
    )))
}

/// Settle the first queued request with `value`, as a rejection or as `{ value, done }`, and
/// remove it.
fn gen_complete(
    scope: &Scope<'_>,
    gen: &Captures<'_>,
    reject: bool,
    value: HandleValue<'_>,
    done: bool,
) -> Result<(), ExnThrown> {
    let Some((_, _, promise)) = gen_front(scope, gen)? else {
        return Ok(());
    };
    array_shift(scope, gen.obj(generator::REQUESTS)?)?;
    if reject {
        promise.reject(scope, value)
    } else {
        let result = js::iteration::create_iter_result(scope, value, done)?;
        promise.resolve(scope, result)
    }
}

/// Run the queued requests until one leaves the generator running or none is left.
fn gen_drain(scope: &Scope<'_>, gen: &Captures<'_>) -> Result<(), ExnThrown> {
    use generator::*;
    loop {
        if gen.flag(RUNNING)? {
            return Ok(());
        }
        let Some((kind, value, _)) = gen_front(scope, gen)? else {
            return Ok(());
        };
        let current = gen.get(PHASE)?.to_int32();
        if current == phase::START && kind != request::NEXT {
            gen.set(PHASE, phase::DONE)?;
            continue;
        }
        if current == phase::DONE {
            match kind {
                request::NEXT => gen_complete(scope, gen, false, HandleValue::undefined(), true)?,
                request::THROW => gen_complete(scope, gen, true, value, true)?,
                _ => {
                    gen.set(RUNNING, true)?;
                    let fulfilled = closure(scope, 1, gen_returned, &[gen.array_value()?])?;
                    let rejected = closure(scope, 1, gen_return_failed, &[gen.array_value()?])?;
                    if await_value(scope, value, fulfilled, rejected).is_err() {
                        gen.set(RUNNING, false)?;
                        let error = take_exception(scope);
                        gen_complete(scope, gen, true, error, true)?;
                        continue;
                    }
                    return Ok(());
                }
            }
            continue;
        }
        gen.set(RUNNING, true)?;
        let stepped = if current == phase::START {
            call(scope, HandleValue::undefined(), gen.get(START)?, &[])
        } else {
            let kind = scope.root_value(js::value::from_i32(kind));
            call(
                scope,
                HandleValue::undefined(),
                gen.get(RESUME)?,
                &[kind, value],
            )
        };
        if stepped.is_err() {
            let error = take_exception(scope);
            gen_end(scope, gen, true, error)?;
        }
        return Ok(());
    }
}

/// The value of a `return(value)` request to a completed generator settled, whose captures are
/// `[generator]`.
fn gen_returned(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = Captures::of(scope, Captures::of(scope, payload)?.get(0)?)?;
    gen.set(generator::RUNNING, false)?;
    gen_complete(scope, &gen, false, args.get(0), true)?;
    gen_drain(scope, &gen)?;
    Ok(js::value::undefined())
}

fn gen_return_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = Captures::of(scope, Captures::of(scope, payload)?.get(0)?)?;
    gen.set(generator::RUNNING, false)?;
    gen_complete(scope, &gen, true, args.get(0), true)?;
    gen_drain(scope, &gen)?;
    Ok(js::value::undefined())
}

/// Complete the generator: fulfill the current request with `{ value, done: true }`, or reject
/// it with `value`.
fn gen_end(
    scope: &Scope<'_>,
    gen: &Captures<'_>,
    reject: bool,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    gen.set(generator::PHASE, phase::DONE)?;
    gen.set(generator::RUNNING, false)?;
    gen_complete(scope, gen, reject, value, true)?;
    gen_drain(scope, gen)
}

/// The body of the generator whose body state is `body` yields `value`: once it settles, as the
/// `await` of a `yield` does, fulfill the current request with `{ value, done: false }`. A
/// rejection resumes the body with a `throw` request.
fn gen_yield(
    scope: &Scope<'_>,
    body: &Captures<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let gen = gen_of(scope, body)?;
    let fulfilled = closure(scope, 1, gen_yielded, &[gen.array_value()?])?;
    let rejected = closure(scope, 1, gen_yield_failed, &[gen.array_value()?])?;
    await_value(scope, value, fulfilled, rejected)
}

fn gen_yielded(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = Captures::of(scope, Captures::of(scope, payload)?.get(0)?)?;
    gen.set(generator::PHASE, phase::YIELDED)?;
    gen.set(generator::RUNNING, false)?;
    gen_complete(scope, &gen, false, args.get(0), false)?;
    gen_drain(scope, &gen)?;
    Ok(js::value::undefined())
}

fn gen_yield_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = Captures::of(scope, Captures::of(scope, payload)?.get(0)?)?;
    let kind = scope.root_value(js::value::from_i32(request::THROW));
    let resume = gen.get(generator::RESUME)?;
    if call(
        scope,
        HandleValue::undefined(),
        resume,
        &[kind, args.get(0)],
    )
    .is_err()
    {
        let error = take_exception(scope);
        gen_end(scope, &gen, true, error)?;
    }
    Ok(js::value::undefined())
}

/// The body of the generator whose body state is `body` returns `value`: once it settles,
/// complete the generator with it.
fn gen_return(
    scope: &Scope<'_>,
    body: &Captures<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let gen = gen_of(scope, body)?;
    let fulfilled = closure(scope, 1, gen_return_settled, &[gen.array_value()?])?;
    let rejected = closure(scope, 1, gen_throw_settled, &[gen.array_value()?])?;
    await_value(scope, value, fulfilled, rejected)
}

fn gen_return_settled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = Captures::of(scope, Captures::of(scope, payload)?.get(0)?)?;
    gen_end(scope, &gen, false, args.get(0))?;
    Ok(js::value::undefined())
}

fn gen_throw_settled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = Captures::of(scope, Captures::of(scope, payload)?.get(0)?)?;
    gen_end(scope, &gen, true, args.get(0))?;
    Ok(js::value::undefined())
}

/// The body of the generator whose body state is `body` throws `error`.
fn gen_throw(
    scope: &Scope<'_>,
    body: &Captures<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    gen_end(scope, &gen_of(scope, body)?, true, error)
}

/// The ways a `for await` loop ends, the first argument of its end callback.
mod end {
    /// The iterator is done.
    pub(super) const DONE: i32 = 0;
    /// The iterator, or the loop body through [`super::loop_close`], threw the second argument.
    pub(super) const THROW: i32 = 1;
    /// [`super::loop_close`] without an error closed the iterator.
    pub(super) const CLOSED: i32 = 2;
}

/// The captures of a `for await` loop's reactions: `[iterator, next, sync, onValue, onEnd]`,
/// where `sync` is set for a sync iterator, whose values the loop awaits.
mod looping {
    pub(super) const ITERATOR: u32 = 0;
    pub(super) const NEXT: u32 = 1;
    pub(super) const SYNC: u32 = 2;
    pub(super) const ON_VALUE: u32 = 3;
    pub(super) const ON_END: u32 = 4;
    pub(super) const COUNT: usize = 5;
}

/// Start a `for await (const value of iterable)` loop: get the iterator of `iterable`, its
/// `Symbol.asyncIterator` method or else its `Symbol.iterator` method, and return the loop's
/// state. The loop calls `on_value(value)` for each value, which continues it with [`loop_next`]
/// or leaves it with [`loop_close`], and `on_end(kind, value)` once it ended, with a kind of
/// [`end`]. Nothing runs before the first [`loop_next`]. `what` names `iterable` in the
/// `TypeError` for a value that isn't iterable.
fn loop_open<'s>(
    scope: &'s Scope<'_>,
    iterable: HandleValue<'_>,
    what: &str,
    on_value: Object<'_>,
    on_end: Object<'_>,
) -> Result<Captures<'s>, ExnThrown> {
    use looping::*;
    if iterable.is_null() || iterable.is_undefined() {
        return Err(type_error(scope, &format!("{what} is not async iterable")));
    }
    let object = Object::from_value_coerce(scope, iterable)?;
    let symbol_method = |code: js::native::SymbolCode| -> Result<HandleValue<'_>, ExnThrown> {
        let key = js::symbol::get_well_known_key(scope, code);
        object.get_property_by_id(scope, scope.root_id(key))
    };
    let mut sync = false;
    let mut method = symbol_method(js::native::SymbolCode::asyncIterator)?;
    if method.is_null() || method.is_undefined() {
        sync = true;
        method = symbol_method(js::native::SymbolCode::iterator)?;
    }
    if !is_function(scope, method) {
        return Err(type_error(scope, &format!("{what} is not async iterable")));
    }
    let iterable = scope.root_value(iterable.get());
    let iterator = call(scope, iterable, method, &[])?;
    let Some(iterator_object) = as_object(scope, iterator) else {
        return Err(type_error(
            scope,
            "Result of the Symbol.asyncIterator method is not an object",
        ));
    };
    let next = iterator_object.val(scope, c"next")?;
    let state = new_state(scope, COUNT)?;
    state.set(ITERATOR, iterator)?;
    state.set(NEXT, next)?;
    state.set(SYNC, sync)?;
    state.set(ON_VALUE, on_value)?;
    state.set(ON_END, on_end)?;
    Ok(state)
}

/// End the loop with `kind` and `value`.
fn loop_end(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    kind: i32,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let kind = scope.root_value(js::value::from_i32(kind));
    let on_end = state.get(looping::ON_END)?;
    call(scope, HandleValue::undefined(), on_end, &[kind, value])?;
    Ok(())
}

/// End the loop with the pending exception.
fn loop_end_pending(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    let error = take_exception(scope);
    loop_end(scope, state, end::THROW, error)
}

/// Take the loop's next value: call the iterator's `next()`, and await its result.
fn loop_next(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    use looping::*;
    let iterator = state.get(ITERATOR)?;
    let Ok(result) = call(scope, iterator, state.get(NEXT)?, &[]) else {
        return loop_end_pending(scope, state);
    };
    if !state.flag(SYNC)? {
        let fulfilled = bind(scope, state, 1, loop_result)?;
        let rejected = bind(scope, state, 1, loop_failed)?;
        if await_value(scope, result, fulfilled, rejected).is_err() {
            return loop_end_pending(scope, state);
        }
        return Ok(());
    }
    let step = (|| -> Result<(), ExnThrown> {
        let Some(result) = as_object(scope, result) else {
            return Err(type_error(scope, "iterator result is not an object"));
        };
        let done = truthy(scope, result.val(scope, c"done")?);
        let value = result.val(scope, c"value")?;
        let fulfilled = if done {
            bind(scope, state, 1, loop_sync_done)?
        } else {
            bind(scope, state, 1, loop_value)?
        };
        let rejected = bind(scope, state, 1, loop_failed)?;
        await_value(scope, value, fulfilled, rejected)
    })();
    if step.is_err() {
        return loop_end_pending(scope, state);
    }
    Ok(())
}

/// An async iterator's `next()` fulfilled with `result`.
fn loop_result(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let step = (|| -> Result<Option<HandleValue<'_>>, ExnThrown> {
        let Some(result) = as_object(scope, args.get(0)) else {
            return Err(type_error(scope, "iterator result is not an object"));
        };
        if truthy(scope, result.val(scope, c"done")?) {
            return Ok(None);
        }
        Ok(Some(result.val(scope, c"value")?))
    })();
    match step {
        Ok(Some(value)) => {
            let on_value = state.get(looping::ON_VALUE)?;
            call(scope, HandleValue::undefined(), on_value, &[value])?;
        }
        Ok(None) => loop_end(scope, &state, end::DONE, HandleValue::undefined())?,
        Err(_) => loop_end_pending(scope, &state)?,
    }
    Ok(js::value::undefined())
}

/// The awaited value of a sync iterator's result.
fn loop_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let on_value = state.get(looping::ON_VALUE)?;
    call(scope, HandleValue::undefined(), on_value, &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// A sync iterator's result with `done` set, once its value settled.
fn loop_sync_done(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    loop_end(scope, &state, end::DONE, HandleValue::undefined())?;
    Ok(js::value::undefined())
}

fn loop_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    loop_end(scope, &state, end::THROW, args.get(0))?;
    Ok(js::value::undefined())
}

/// The captures of the reactions of [`loop_close`]: `[loop, hasError, error]`.
mod closing {
    pub(super) const LOOP: u32 = 0;
    pub(super) const HAS_ERROR: u32 = 1;
    pub(super) const ERROR: u32 = 2;
}

/// Leave the loop, as `break` does without `error` and `throw error` does with it: call the
/// iterator's `return()` if it has one, await its result, and end the loop. The loop ends with
/// `error` if given, and otherwise with an exception `return()` throws or its rejection.
fn loop_close(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let iterator = state.get(looping::ITERATOR)?;
    let finish = |thrown: Option<HandleValue<'_>>| -> Result<(), ExnThrown> {
        match (error, thrown) {
            (Some(error), _) => loop_end(scope, state, end::THROW, error),
            (None, Some(thrown)) => loop_end(scope, state, end::THROW, thrown),
            (None, None) => loop_end(scope, state, end::CLOSED, HandleValue::undefined()),
        }
    };
    let step = (|| -> Result<bool, ExnThrown> {
        let iterator_object = Object::from_value_coerce(scope, iterator)?;
        let method = iterator_object.val(scope, c"return")?;
        if method.is_null() || method.is_undefined() {
            return Ok(false);
        }
        if !is_function(scope, method) {
            return Err(type_error(scope, "iterator.return is not a function"));
        }
        let result = call(scope, iterator, method, &[])?;
        let captures = js::Array::with_contents(
            scope,
            &[
                state.array_value()?,
                js::value::from_bool(error.is_some()),
                error.map_or(js::value::undefined(), |error| error.get()),
            ],
        )?;
        let fulfilled = Function::new_callback(scope, c"", 1, loop_closed, *captures)?;
        let rejected = Function::new_callback(scope, c"", 1, loop_close_failed, *captures)?;
        await_value(scope, result, *fulfilled, *rejected)?;
        Ok(true)
    })();
    match step {
        Ok(true) => Ok(()),
        Ok(false) => finish(None),
        Err(_) => {
            let thrown = take_exception(scope);
            finish(Some(thrown))
        }
    }
}

/// The result of the iterator's `return()` settled.
fn loop_closed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use closing::*;
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(LOOP)?)?;
    if captures.flag(HAS_ERROR)? {
        loop_end(scope, &state, end::THROW, captures.get(ERROR)?)?;
    } else if !args.get(0).is_object() {
        let error = error_value(
            scope,
            NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_RETURN_VALUE",
                "iterator.return() did not return an object",
            ),
        )?;
        loop_end(scope, &state, end::THROW, error)?;
    } else {
        loop_end(scope, &state, end::CLOSED, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

fn loop_close_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use closing::*;
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(LOOP)?)?;
    let error = if captures.flag(HAS_ERROR)? {
        captures.get(ERROR)?
    } else {
        args.get(0)
    };
    loop_end(scope, &state, end::THROW, error)?;
    Ok(js::value::undefined())
}

/// The state of a `map` generator: `[generator, stream, fn, options, signal, signalOpt, queue,
/// next, resume, done, cnt, concurrency, highWaterMark, loop, eof, empty]`, Node's variables of
/// the same names. `next` and `resume` are the pending promises the body and the pump await, or
/// `undefined`. `eof` and `empty` are the objects for Node's `kEof` and `kEmpty`.
mod map {
    pub(super) const STREAM: u32 = 1;
    pub(super) const FN: u32 = 2;
    pub(super) const OPTIONS: u32 = 3;
    pub(super) const SIGNAL: u32 = 4;
    pub(super) const SIGNAL_OPT: u32 = 5;
    pub(super) const QUEUE: u32 = 6;
    pub(super) const NEXT: u32 = 7;
    pub(super) const RESUME: u32 = 8;
    pub(super) const DONE: u32 = 9;
    pub(super) const CNT: u32 = 10;
    pub(super) const CONCURRENCY: u32 = 11;
    pub(super) const HIGH_WATER_MARK: u32 = 12;
    pub(super) const LOOP: u32 = 13;
    pub(super) const EOF: u32 = 14;
    pub(super) const EMPTY: u32 = 15;
    pub(super) const COUNT: usize = 16;
}

/// `MathFloor(value)`: `Math.floor` of `value` converted to a number.
fn floor(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, ExnThrown> {
    use js::conversion::FromJSVal;
    Ok(f64::from_jsval(scope, value, ())
        .map_err(|_| ExnThrown)?
        .floor())
}

/// Node's `map(fn, options)`: the generator of `fn`'s results for the values of `stream`, with up
/// to `options.concurrency` calls of `fn` in flight and `options.highWaterMark` results waiting.
/// A result that is `empty` isn't yielded.
fn map_generator<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    function: HandleValue<'_>,
    options: HandleValue<'_>,
    empty: Option<Object<'_>>,
) -> Result<Object<'s>, Thrown> {
    use map::*;
    validate_function(scope, function, "fn")?;
    let options = scope.root_value(options.get());
    let options = validate_options(scope, options)?;
    let option = |name: &'static CStr| -> Result<HandleValue<'_>, ExnThrown> {
        match as_object(scope, options) {
            Some(options) => options.val(scope, name),
            None => Ok(HandleValue::undefined()),
        }
    };
    let mut concurrency = 1.0;
    let value = option(c"concurrency")?;
    if !value.is_null() && !value.is_undefined() {
        concurrency = floor(scope, option(c"concurrency")?)?;
    }
    let mut high_water_mark = concurrency - 1.0;
    let value = option(c"highWaterMark")?;
    if !value.is_null() && !value.is_undefined() {
        high_water_mark = floor(scope, option(c"highWaterMark")?)?;
    }
    let concurrency_value = scope.root_value(js::value::from_f64(concurrency));
    validate_integer(
        scope,
        concurrency_value,
        "options.concurrency",
        1.0,
        MAX_SAFE_INTEGER,
    )?;
    let high_water_mark_value = scope.root_value(js::value::from_f64(high_water_mark));
    validate_integer(
        scope,
        high_water_mark_value,
        "options.highWaterMark",
        0.0,
        MAX_SAFE_INTEGER,
    )?;
    high_water_mark += concurrency;

    let state = new_state(scope, COUNT)?;
    state.set(STREAM, stream)?;
    state.set(FN, function)?;
    state.set(OPTIONS, options)?;
    state.set(QUEUE, *js::Array::with_contents(scope, &[] as &[Value])?)?;
    state.set(DONE, false)?;
    state.set(CNT, 0)?;
    state.set(CONCURRENCY, concurrency)?;
    state.set(HIGH_WATER_MARK, high_water_mark)?;
    state.set(EOF, Object::new_plain(scope)?)?;
    let empty = match empty {
        Some(empty) => empty,
        None => Object::new_plain(scope)?,
    };
    state.set(EMPTY, empty)?;
    Ok(new_generator(scope, &state, map_start, map_resume)?)
}

/// The start of `map`'s body: create the signal, start the pump, and wait for the first result.
fn map_start(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use map::*;
    let state = state_of(scope, payload)?;
    let signals = js::Array::with_contents(scope, &[] as &[Value])?;
    if let Some(options) = as_object(scope, state.get(OPTIONS)?) {
        let signal = options.val(scope, c"signal")?;
        if truthy(scope, signal) {
            array_push(scope, *signals, signal)?;
        }
    }
    let abort_signal = scope
        .global()
        .obj(scope, c"AbortSignal")?
        .ok_or_else(|| type_error(scope, "AbortSignal is not defined"))?;
    let signal = call_method(
        scope,
        abort_signal,
        c"any",
        &[(*signals).to_jsval_throwing(scope)?],
    )?;
    state.set(SIGNAL, signal)?;
    let signal_opt = Object::new_plain(scope)?;
    signal_opt.set_property(scope, c"signal", signal)?;
    state.set(SIGNAL_OPT, signal_opt)?;
    pump_start(scope, &state)?;
    map_continue(scope, &state);
    Ok(js::value::undefined())
}

/// Resolve the pending promise at `index`, if any, and clear it: a call of Node's `next()` or
/// `resume()`.
fn wake(scope: &Scope<'_>, state: &Captures<'_>, index: u32) -> Result<(), ExnThrown> {
    let Some(promise) = as_object(scope, state.get(index)?) else {
        return Ok(());
    };
    state.set(index, js::value::undefined())?;
    if let Ok(promise) = promise.cast::<js::Promise>() {
        settle(scope, promise, false, HandleValue::undefined())?;
    }
    Ok(())
}

/// Store a new pending promise at `index`, and run `then` with the captures `[state]` once it is
/// resolved: `await new Promise((resolve) => { variable = resolve; })`.
fn wait(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    index: u32,
    then: Callback,
) -> Result<(), ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    state.set(index, *promise)?;
    promise.then(scope, Some(bind(scope, state, 1, then)?), None)?;
    Ok(())
}

/// The queue's length.
fn queue_len(scope: &Scope<'_>, state: &Captures<'_>) -> Result<f64, ExnThrown> {
    Ok(array_len(scope, state.obj(map::QUEUE)?)? as f64)
}

/// Node's `maybeResume()`: resume the pump if it waits and there is room.
fn maybe_resume(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    use map::*;
    if !state.get(RESUME)?.is_object() || state.flag(DONE)? {
        return Ok(());
    }
    if state.get(CNT)?.to_number() < state.get(CONCURRENCY)?.to_number()
        && queue_len(scope, state)? < state.get(HIGH_WATER_MARK)?.to_number()
    {
        wake(scope, state, RESUME)?;
    }
    Ok(())
}

/// Node's `afterItemProcessed()`.
fn after_item_processed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let count = state.get(map::CNT)?.to_number();
    state.set(map::CNT, count - 1.0)?;
    maybe_resume(scope, &state)?;
    Ok(js::value::undefined())
}

/// Node's `onCatch()`.
fn on_catch(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    state_of(scope, payload)?.set(map::DONE, true)?;
    after_item_processed(scope, args, payload)
}

/// Queue `promise`, the result for one value, after the reactions that count it as processed.
fn queue_result(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    promise: js::Promise<'_>,
) -> Result<(), ExnThrown> {
    let processed = bind(scope, state, 0, after_item_processed)?;
    let failed = bind(scope, state, 0, on_catch)?;
    promise.then(scope, Some(processed), Some(failed))?;
    array_push(
        scope,
        state.obj(map::QUEUE)?,
        promise.to_jsval_throwing(scope)?,
    )
}

/// Node's `pump()`: start the `for await` loop over the stream.
fn pump_start(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    use map::*;
    let on_value = bind(scope, state, 1, pump_value)?;
    let on_end = bind(scope, state, 2, pump_end)?;
    match loop_open(scope, state.get(STREAM)?, "stream", on_value, on_end) {
        Ok(lp) => {
            state.set(LOOP, lp.array)?;
            loop_next(scope, &lp)
        }
        Err(_) => {
            let error = take_exception(scope);
            pump_catch(scope, state, error)?;
            pump_finally(scope, state)
        }
    }
}

/// The loop of the pump.
fn pump_loop<'s>(scope: &'s Scope<'_>, state: &Captures<'s>) -> Result<Captures<'s>, ExnThrown> {
    Captures::of(scope, state.get(map::LOOP)?)
}

/// The body of the pump's loop, for the value `val`.
fn pump_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use map::*;
    let state = state_of(scope, payload)?;
    let lp = pump_loop(scope, &state)?;
    if state.flag(DONE)? {
        loop_close(scope, &lp, None)?;
        return Ok(js::value::undefined());
    }
    let signal = state.obj(SIGNAL)?;
    if truthy(scope, signal.val(scope, c"aborted")?) {
        let error = abort_error(scope, None)?;
        loop_close(scope, &lp, Some(error))?;
        return Ok(js::value::undefined());
    }
    let mapped = (|| -> Result<js::Promise<'_>, ExnThrown> {
        let value = call(
            scope,
            HandleValue::undefined(),
            state.get(FN)?,
            &[args.get(0), state.get(SIGNAL_OPT)?],
        )?;
        js::Promise::call_original_resolve(scope, value)
    })();
    let promise = match mapped {
        Ok(promise) => promise,
        Err(_) => {
            let error = take_exception(scope);
            js::Promise::new_rejected_with_error(scope, error)?
        }
    };
    let count = state.get(CNT)?.to_number();
    state.set(CNT, count + 1.0)?;
    queue_result(scope, &state, promise)?;
    wake(scope, &state, NEXT)?;
    if !state.flag(DONE)?
        && (queue_len(scope, &state)? >= state.get(HIGH_WATER_MARK)?.to_number()
            || state.get(CNT)?.to_number() >= state.get(CONCURRENCY)?.to_number())
    {
        wait(scope, &state, RESUME, pump_resumed)?;
    } else {
        loop_next(scope, &lp)?;
    }
    Ok(js::value::undefined())
}

fn pump_resumed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    loop_next(scope, &pump_loop(scope, &state)?)?;
    Ok(js::value::undefined())
}

/// The end of the pump's loop: queue `kEof` after the last value, or a rejection with the error.
fn pump_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    match args.get(0).to_int32() {
        end::DONE => array_push(scope, state.obj(map::QUEUE)?, state.get(map::EOF)?)?,
        end::THROW => pump_catch(scope, &state, args.get(1))?,
        _ => {}
    }
    pump_finally(scope, &state)?;
    Ok(js::value::undefined())
}

/// The `catch` block of the pump.
fn pump_catch(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let promise = js::Promise::new_rejected_with_error(scope, error)?;
    queue_result(scope, state, promise)
}

/// The `finally` block of the pump.
fn pump_finally(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    state.set(map::DONE, true)?;
    wake(scope, state, map::NEXT)
}

/// Continue `map`'s body at the top of its loop, and end the body with an exception the step
/// throws.
fn map_continue(scope: &Scope<'_>, state: &Captures<'_>) {
    if map_loop(scope, state).is_err() {
        let error = take_exception(scope);
        let _ = map_fail(scope, state, error);
    }
}

/// The top of `map`'s loop: await the first queued result, or wait for one.
fn map_loop(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    let queue = state.obj(map::QUEUE)?;
    if array_len(scope, queue)? == 0 {
        return wait(scope, state, map::NEXT, map_woken);
    }
    let front = queue.get_element(scope, 0)?;
    let fulfilled = bind(scope, state, 1, map_awaited)?;
    let rejected = bind(scope, state, 1, map_rejected)?;
    await_value(scope, front, fulfilled, rejected)
}

fn map_woken(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    map_continue(scope, &state_of(scope, payload)?);
    Ok(js::value::undefined())
}

/// The first queued result settled with `val`: return at `kEof`, and yield anything but
/// `kEmpty`.
fn map_awaited(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use map::*;
    let state = state_of(scope, payload)?;
    let step = (|| -> Result<(), ExnThrown> {
        let value = args.get(0);
        if same(scope, value, state.get(EOF)?)? {
            map_finally(scope, &state)?;
            return gen_return(scope, &state, HandleValue::undefined());
        }
        if truthy(scope, state.obj(SIGNAL)?.val(scope, c"aborted")?) {
            let error = abort_error(scope, None)?;
            return map_fail(scope, &state, error);
        }
        if same(scope, value, state.get(EMPTY)?)? {
            return map_after_yield(scope, &state);
        }
        gen_yield(scope, &state, value)
    })();
    if step.is_err() {
        let error = take_exception(scope);
        map_fail(scope, &state, error)?;
    }
    Ok(js::value::undefined())
}

fn map_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    map_fail(scope, &state_of(scope, payload)?, args.get(0))?;
    Ok(js::value::undefined())
}

/// After the body yielded: remove the result, resume the pump if there is room, and continue.
fn map_after_yield(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    array_shift(scope, state.obj(map::QUEUE)?)?;
    maybe_resume(scope, state)?;
    map_loop(scope, state)
}

/// A request to `map`'s body after it yielded.
fn map_resume(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    match args.get(0).to_int32() {
        request::NEXT => {
            if map_after_yield(scope, &state).is_err() {
                let error = take_exception(scope);
                map_fail(scope, &state, error)?;
            }
        }
        request::RETURN => {
            map_finally(scope, &state)?;
            gen_return(scope, &state, args.get(1))?;
        }
        _ => map_fail(scope, &state, args.get(1))?,
    }
    Ok(js::value::undefined())
}

/// `map`'s body throws `error`, after its `finally` block.
fn map_fail(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    map_finally(scope, state)?;
    gen_throw(scope, state, error)
}

/// The `finally` block of `map`'s body: stop the pump.
fn map_finally(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    state.set(map::DONE, true)?;
    wake(scope, state, map::RESUME)
}

/// `async function filterFn(value, options)` and `async function forEachFn(value, options)`,
/// whose captures are `[fn, empty, isForEach]`: call `fn`, and fulfill with `value` if `fn`'s
/// result is truthy and with `empty` otherwise, or always with `empty` for `forEach`.
fn filter_fn(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let value = args.get(0);
    let result = call(
        scope,
        HandleValue::undefined(),
        captures.get(0)?,
        &[value, args.get(1)],
    );
    let promise = match result {
        Ok(result) => js::Promise::call_original_resolve(scope, result),
        Err(error) => Err(error),
    };
    let promise = match promise {
        Ok(promise) => promise,
        Err(_) => {
            let error = take_exception(scope);
            return js::Promise::new_rejected_with_error(scope, error)?
                .to_jsval_raw_throwing(scope);
        }
    };
    let decide = closure(
        scope,
        1,
        filter_decide,
        &[value.get(), captures.get(1)?.get(), captures.get(2)?.get()],
    )?;
    promise
        .then(scope, Some(decide), None)?
        .to_jsval_raw_throwing(scope)
}

/// The result of `filterFn` once `fn`'s result settled: `value` if it is truthy, and `empty`
/// otherwise or for `forEach`. The captures are `[value, empty, isForEach]`.
fn filter_decide(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    if !captures.flag(2)? && truthy(scope, args.get(0)) {
        return Ok(captures.get(0)?.get());
    }
    Ok(captures.get(1)?.get())
}

/// Node's `filter(fn, options)`: `map` with `filterFn`.
fn filter_generator<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    function: HandleValue<'_>,
    options: HandleValue<'_>,
    for_each: bool,
) -> Result<Object<'s>, Thrown> {
    validate_function(scope, function, "fn")?;
    let empty = Object::new_plain(scope)?;
    let wrapped = closure(
        scope,
        2,
        filter_fn,
        &[
            function.get(),
            empty.to_jsval_raw_throwing(scope)?,
            js::value::from_bool(for_each),
        ],
    )?;
    map_generator(
        scope,
        stream,
        wrapped.to_jsval_throwing(scope)?,
        options,
        Some(empty),
    )
}

/// The state of a `drop` or `take` generator: `[generator, stream, options, number, loop, take,
/// returnValue]`, where `returnValue` is the value of a `return()` request that closes the loop.
mod skip {
    pub(super) const STREAM: u32 = 1;
    pub(super) const OPTIONS: u32 = 2;
    pub(super) const NUMBER: u32 = 3;
    pub(super) const LOOP: u32 = 4;
    pub(super) const TAKE: u32 = 5;
    pub(super) const RETURN_VALUE: u32 = 6;
    pub(super) const COUNT: usize = 7;
}

/// Node's `toIntegerOrInfinity(number)`: `Number(number)`, 0 for `NaN`, and `ERR_OUT_OF_RANGE`
/// for a negative number.
fn to_integer_or_infinity(scope: &Scope<'_>, number: HandleValue<'_>) -> Result<f64, Thrown> {
    let number_class = scope
        .global()
        .val(scope, c"Number")
        .map(|value| scope.root_value(value.get()))?;
    let number = call(scope, HandleValue::undefined(), number_class, &[number])?.to_number();
    if number.is_nan() {
        return Ok(0.0);
    }
    if number < 0.0 {
        return Err(
            NodeError::out_of_range("number", ">= 0", &out_of_range_received(number)).into(),
        );
    }
    Ok(number)
}

/// Node's `drop(number, options)` and `take(number, options)`.
fn skip_generator<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    number: HandleValue<'_>,
    options: HandleValue<'_>,
    take: bool,
) -> Result<Object<'s>, Thrown> {
    use skip::*;
    let options = scope.root_value(options.get());
    let options = validate_options(scope, options)?;
    let number = to_integer_or_infinity(scope, number)?;
    let state = new_state(scope, COUNT)?;
    state.set(STREAM, stream)?;
    state.set(OPTIONS, options)?;
    state.set(NUMBER, number)?;
    state.set(TAKE, take)?;
    Ok(new_generator(scope, &state, skip_start, skip_resume)?)
}

fn skip_start(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use skip::*;
    let state = state_of(scope, payload)?;
    if signal_aborted(scope, state.get(OPTIONS)?)? {
        let error = abort_error(scope, None)?;
        gen_throw(scope, &state, error)?;
        return Ok(js::value::undefined());
    }
    let on_value = bind(scope, &state, 1, skip_value)?;
    let on_end = bind(scope, &state, 2, skip_end)?;
    let lp = loop_open(scope, state.get(STREAM)?, "this", on_value, on_end)?;
    state.set(LOOP, lp.array)?;
    loop_next(scope, &lp)?;
    Ok(js::value::undefined())
}

/// The body of the loop of `drop` and `take`.
fn skip_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use skip::*;
    let state = state_of(scope, payload)?;
    let lp = Captures::of(scope, state.get(LOOP)?)?;
    let step = (|| -> Result<(), ExnThrown> {
        if signal_aborted(scope, state.get(OPTIONS)?)? {
            let error = abort_error(scope, None)?;
            return loop_close(scope, &lp, Some(error));
        }
        let number = state.get(NUMBER)?.to_number();
        state.set(NUMBER, number - 1.0)?;
        let take = state.flag(TAKE)?;
        if (!take && number <= 0.0) || (take && number > 0.0) {
            return gen_yield(scope, &state, args.get(0));
        }
        skip_continue(scope, &state, &lp)
    })();
    if step.is_err() {
        let error = take_exception(scope);
        loop_close(scope, &lp, Some(error))?;
    }
    Ok(js::value::undefined())
}

/// The end of an iteration of the loop of `drop` and `take`: `take` returns once it took the
/// number of values.
fn skip_continue(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    lp: &Captures<'_>,
) -> Result<(), ExnThrown> {
    use skip::*;
    if state.flag(TAKE)? && state.get(NUMBER)?.to_number() <= 0.0 {
        return loop_close(scope, lp, None);
    }
    loop_next(scope, lp)
}

fn skip_resume(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use skip::*;
    let state = state_of(scope, payload)?;
    let lp = Captures::of(scope, state.get(LOOP)?)?;
    match args.get(0).to_int32() {
        request::NEXT => skip_continue(scope, &state, &lp)?,
        request::RETURN => {
            state.set(RETURN_VALUE, args.get(1))?;
            loop_close(scope, &lp, None)?;
        }
        _ => loop_close(scope, &lp, Some(args.get(1)))?,
    }
    Ok(js::value::undefined())
}

fn skip_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    match args.get(0).to_int32() {
        end::THROW => gen_throw(scope, &state, args.get(1))?,
        end::CLOSED => gen_return(scope, &state, state.get(skip::RETURN_VALUE)?)?,
        _ => gen_return(scope, &state, HandleValue::undefined())?,
    }
    Ok(js::value::undefined())
}

/// The state of a `flatMap` generator: `[generator, values, outer, inner, returnValue]`, where
/// `values` is the `map` generator, `outer` the loop over it, `inner` the loop of the current
/// `yield*`, and `returnValue` the value of a `return()` request.
mod flat {
    pub(super) const VALUES: u32 = 1;
    pub(super) const OUTER: u32 = 2;
    pub(super) const INNER: u32 = 3;
    pub(super) const RETURN_VALUE: u32 = 4;
    pub(super) const COUNT: usize = 5;
}

/// Node's `flatMap(fn, options)`: the values of the iterables `map(fn, options)` yields.
fn flat_map_generator<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    function: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let values = map_generator(scope, stream, function, options, None)?;
    let state = new_state(scope, flat::COUNT)?;
    state.set(flat::VALUES, values)?;
    Ok(new_generator(scope, &state, flat_start, flat_resume)?)
}

fn flat_start(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let on_value = bind(scope, &state, 1, flat_outer_value)?;
    let on_end = bind(scope, &state, 2, flat_outer_end)?;
    let lp = loop_open(scope, state.get(flat::VALUES)?, "values", on_value, on_end)?;
    state.set(flat::OUTER, lp.array)?;
    loop_next(scope, &lp)?;
    Ok(js::value::undefined())
}

/// `yield* val` for a value of the `map` generator.
fn flat_outer_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use flat::*;
    let state = state_of(scope, payload)?;
    let opened = (|| -> Result<(), ExnThrown> {
        let on_value = bind(scope, &state, 1, flat_inner_value)?;
        let on_end = bind(scope, &state, 2, flat_inner_end)?;
        let lp = loop_open(scope, args.get(0), "val", on_value, on_end)?;
        state.set(INNER, lp.array)?;
        loop_next(scope, &lp)
    })();
    if opened.is_err() {
        let error = take_exception(scope);
        let outer = Captures::of(scope, state.get(OUTER)?)?;
        loop_close(scope, &outer, Some(error))?;
    }
    Ok(js::value::undefined())
}

fn flat_inner_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    gen_yield(scope, &state, args.get(0))?;
    Ok(js::value::undefined())
}

/// The end of a `yield*`: continue the loop over the `map` generator, or leave it.
fn flat_inner_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use flat::*;
    let state = state_of(scope, payload)?;
    state.set(INNER, js::value::undefined())?;
    let outer = Captures::of(scope, state.get(OUTER)?)?;
    match args.get(0).to_int32() {
        end::DONE => loop_next(scope, &outer)?,
        end::THROW => loop_close(scope, &outer, Some(args.get(1)))?,
        _ => loop_close(scope, &outer, None)?,
    }
    Ok(js::value::undefined())
}

fn flat_outer_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    match args.get(0).to_int32() {
        end::THROW => gen_throw(scope, &state, args.get(1))?,
        end::CLOSED => gen_return(scope, &state, state.get(flat::RETURN_VALUE)?)?,
        _ => gen_return(scope, &state, HandleValue::undefined())?,
    }
    Ok(js::value::undefined())
}

/// A request to `flatMap`'s body, which yielded from the current `yield*`.
fn flat_resume(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use flat::*;
    let state = state_of(scope, payload)?;
    let inner = Captures::of(scope, state.get(INNER)?)?;
    match args.get(0).to_int32() {
        request::NEXT => loop_next(scope, &inner)?,
        request::RETURN => {
            state.set(RETURN_VALUE, args.get(1))?;
            loop_close(scope, &inner, None)?;
        }
        _ => loop_close(scope, &inner, Some(args.get(1)))?,
    }
    Ok(js::value::undefined())
}

/// Run the body of a promise-returning operator, and return its promise, rejected with an
/// error the body throws.
fn promise_operator(
    scope: &Scope<'_>,
    body: impl FnOnce(js::Promise<'_>) -> Result<(), Thrown>,
) -> Result<Value, ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    if let Err(error) = body(promise) {
        error.throw(scope);
        let error = take_exception(scope);
        settle(scope, promise, true, error)?;
    }
    promise.to_jsval_raw_throwing(scope)
}

/// The state of a promise-returning operator: `[promise, loop, options, result, ...]`, with the
/// further elements of `reduce` and of `some` and `find`.
mod consume {
    pub(super) const PROMISE: u32 = 0;
    pub(super) const LOOP: u32 = 1;
    pub(super) const OPTIONS: u32 = 2;
    pub(super) const RESULT: u32 = 3;
    /// `reduce`'s `stream`, `some`'s and `find`'s `isFind`.
    pub(super) const EXTRA: u32 = 4;
    /// `reduce`'s `reducer`.
    pub(super) const REDUCER: u32 = 5;
    /// `reduce`'s `hasInitialValue`.
    pub(super) const HAS_INITIAL: u32 = 6;
    /// `reduce`'s `AbortController`.
    pub(super) const CONTROLLER: u32 = 7;
    /// `reduce`'s `gotAnyItemFromStream`.
    pub(super) const GOT_ANY: u32 = 8;
    pub(super) const COUNT: usize = 9;
}

/// Start the loop of a promise-returning operator over `iterable`, with the body `on_value` and
/// the end `on_end`.
fn consume_loop(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    iterable: HandleValue<'_>,
    on_value: Callback,
    on_end: Callback,
) -> Result<(), ExnThrown> {
    let on_value = bind(scope, state, 1, on_value)?;
    let on_end = bind(scope, state, 2, on_end)?;
    let lp = loop_open(scope, iterable, "this", on_value, on_end)?;
    state.set(consume::LOOP, lp.array)?;
    loop_next(scope, &lp)
}

/// Settle the operator's promise.
fn consume_settle(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    reject: bool,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    settle(
        scope,
        promise_at(scope, state, consume::PROMISE)?,
        reject,
        value,
    )
}

/// The loop of the operator.
fn consume_loop_of<'s>(
    scope: &'s Scope<'_>,
    state: &Captures<'s>,
) -> Result<Captures<'s>, ExnThrown> {
    Captures::of(scope, state.get(consume::LOOP)?)
}

/// Node's `toArray(options)`.
fn to_array(
    scope: &Scope<'_>,
    stream: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    promise_operator(scope, |promise| {
        let options = scope.root_value(options.get());
        let options = validate_options(scope, options)?;
        let state = new_state(scope, consume::COUNT)?;
        state.set(consume::PROMISE, *promise)?;
        state.set(consume::OPTIONS, options)?;
        state.set(
            consume::RESULT,
            *js::Array::with_contents(scope, &[] as &[Value])?,
        )?;
        consume_loop(scope, &state, stream, to_array_value, settle_end)?;
        Ok(())
    })
}

fn to_array_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use consume::*;
    let state = state_of(scope, payload)?;
    let lp = consume_loop_of(scope, &state)?;
    let step = (|| -> Result<bool, ExnThrown> {
        let options = state.get(OPTIONS)?;
        if signal_aborted(scope, options)? {
            let reason = signal_reason(scope, options)?;
            let error = abort_error(scope, Some(reason))?;
            loop_close(scope, &lp, Some(error))?;
            return Ok(false);
        }
        array_push(scope, state.obj(RESULT)?, args.get(0))?;
        Ok(true)
    })();
    match step {
        Ok(true) => loop_next(scope, &lp)?,
        Ok(false) => {}
        Err(_) => {
            let error = take_exception(scope);
            loop_close(scope, &lp, Some(error))?;
        }
    }
    Ok(js::value::undefined())
}

/// The end of a loop whose operator fulfills with its result: reject with the error, or fulfill
/// with `result`.
fn settle_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    if args.get(0).to_int32() == end::THROW {
        consume_settle(scope, &state, true, args.get(1))?;
    } else {
        consume_settle(scope, &state, false, state.get(consume::RESULT)?)?;
    }
    Ok(js::value::undefined())
}

/// Node's `forEach(fn, options)`: run the `map` generator of `forEachFn` to its end.
fn for_each(
    scope: &Scope<'_>,
    stream: HandleValue<'_>,
    function: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    promise_operator(scope, |promise| {
        let values = filter_generator(scope, stream, function, options, true)?;
        let state = new_state(scope, consume::COUNT)?;
        state.set(consume::PROMISE, *promise)?;
        consume_loop(
            scope,
            &state,
            values.to_jsval_throwing(scope)?,
            next_value,
            settle_end,
        )?;
        Ok(())
    })
}

/// A loop body that only continues the loop.
fn next_value(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    loop_next(scope, &consume_loop_of(scope, &state)?)?;
    Ok(js::value::undefined())
}

/// Node's `some(fn, options)`, or `find(fn, options)` if `find`: the first value of the `filter`
/// generator, which the promise fulfills with `true`, or with the value for `find`.
fn some_or_find(
    scope: &Scope<'_>,
    stream: HandleValue<'_>,
    function: HandleValue<'_>,
    options: HandleValue<'_>,
    find: bool,
) -> Result<Value, ExnThrown> {
    promise_operator(scope, |promise| {
        let values = filter_generator(scope, stream, function, options, false)?;
        let state = new_state(scope, consume::COUNT)?;
        state.set(consume::PROMISE, *promise)?;
        state.set(consume::EXTRA, find)?;
        if find {
            state.set(consume::RESULT, js::value::undefined())?;
        } else {
            state.set(consume::RESULT, false)?;
        }
        consume_loop(
            scope,
            &state,
            values.to_jsval_throwing(scope)?,
            first_value,
            settle_end,
        )?;
        Ok(())
    })
}

/// The body of the loop of `some` and `find`: record the result, and leave the loop.
fn first_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use consume::*;
    let state = state_of(scope, payload)?;
    if state.flag(EXTRA)? {
        state.set(RESULT, args.get(0))?;
    } else {
        state.set(RESULT, true)?;
    }
    loop_close(scope, &consume_loop_of(scope, &state)?, None)?;
    Ok(js::value::undefined())
}

/// `async (...args) => !(await fn(...args))`, whose captures are `[fn]`.
fn every_fn(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let function = Captures::of(scope, payload)?.get(0)?;
    let call_args: Vec<HandleValue<'_>> = (0..args.len()).map(|index| args.get(index)).collect();
    let result = call(scope, HandleValue::undefined(), function, &call_args)
        .and_then(|result| js::Promise::call_original_resolve(scope, result));
    let promise = match result {
        Ok(promise) => promise,
        Err(_) => {
            let error = take_exception(scope);
            return js::Promise::new_rejected_with_error(scope, error)?
                .to_jsval_raw_throwing(scope);
        }
    };
    let negate = Function::new_callback(scope, c"", 1, negate, ())?;
    promise
        .then(scope, Some(*negate), None)?
        .to_jsval_raw_throwing(scope)
}

fn noop(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// `!value`.
fn negate(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::from_bool(!truthy(scope, args.get(0))))
}

/// Node's `every(fn, options)`: `!(await some(async (...args) => !(await fn(...args))))`.
fn every(
    scope: &Scope<'_>,
    stream: HandleValue<'_>,
    function: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    if let Err(error) = validate_function(scope, function, "fn") {
        error.throw(scope);
        let error = take_exception(scope);
        return js::Promise::new_rejected_with_error(scope, error)?.to_jsval_raw_throwing(scope);
    }
    let wrapped = closure(scope, 0, every_fn, &[function.get()])?;
    let some = scope.root_value(some_or_find(
        scope,
        stream,
        wrapped.to_jsval_throwing(scope)?,
        options,
        false,
    )?);
    let some = js::Promise::call_original_resolve(scope, some)?;
    let negate = Function::new_callback(scope, c"", 1, negate, ())?;
    some.then(scope, Some(*negate), None)?
        .to_jsval_raw_throwing(scope)
}

/// `ERR_MISSING_ARGS` for a `reduce` of an empty stream without an initial value.
fn reduce_missing_initial_value() -> NodeError {
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_MISSING_ARGS",
        "Reduce of an empty stream requires an initial value",
    )
}

/// Node's `reduce(reducer, initialValue, options)`, where `initial` is `None` if the call had a
/// single argument.
fn reduce(
    scope: &Scope<'_>,
    stream: HandleValue<'_>,
    reducer: HandleValue<'_>,
    initial: Option<HandleValue<'_>>,
    options: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    use consume::*;
    promise_operator(scope, |promise| {
        validate_function(scope, reducer, "reducer")?;
        let options = scope.root_value(options.get());
        let options = validate_options(scope, options)?;
        let state = new_state(scope, COUNT)?;
        state.set(PROMISE, *promise)?;
        state.set(OPTIONS, options)?;
        state.set(EXTRA, stream)?;
        state.set(REDUCER, reducer)?;
        state.set(HAS_INITIAL, initial.is_some())?;
        state.set(RESULT, initial.unwrap_or(HandleValue::undefined()))?;
        state.set(GOT_ANY, false)?;
        if signal_aborted(scope, options)? {
            let reason = signal_reason(scope, options)?;
            let error = abort_error(scope, Some(reason))?;
            let stream_object = Object::from_value_coerce(scope, stream)?;
            let noop = Function::new_callback(scope, c"", 0, noop, ())?;
            listen(scope, stream_object, c"once", "error", *noop)?;
            let destroyed = call_method(scope, stream_object, c"destroy", &[error])?;
            let finished = super::eos::finished_promise(scope, destroyed, None)?;
            let rejected = closure(
                scope,
                0,
                reject_with,
                &[promise.to_jsval_raw_throwing(scope)?, error.get()],
            )?;
            let failed = closure(
                scope,
                1,
                reject_promise,
                &[promise.to_jsval_raw_throwing(scope)?],
            )?;
            finished.then(scope, Some(rejected), Some(failed))?;
            return Ok(());
        }
        let controller_class = scope
            .global()
            .obj(scope, c"AbortController")?
            .ok_or_else(|| type_error(scope, "AbortController is not defined"))?;
        let controller = Function::construct(scope, controller_class, &[] as &[HandleValue<'_>])?;
        state.set(CONTROLLER, controller)?;
        if let Some(options) = as_object(scope, options) {
            let signal = options.val(scope, c"signal")?;
            if let Some(signal) = as_object(scope, signal) {
                let listener = closure(
                    scope,
                    0,
                    abort_controller,
                    &[controller.to_jsval_raw_throwing(scope)?],
                )?;
                let listen_options = Object::new_plain(scope)?;
                listen_options.set_property(scope, c"once", true)?;
                let event = super::util::string(scope, "abort")?;
                call_method(
                    scope,
                    signal,
                    c"addEventListener",
                    &[
                        event,
                        listener.to_jsval_throwing(scope)?,
                        listen_options.to_jsval_throwing(scope)?,
                    ],
                )?;
            }
        }
        if consume_loop(scope, &state, stream, reduce_value, reduce_end).is_err() {
            let error = take_exception(scope);
            reduce_finally(scope, &state)?;
            consume_settle(scope, &state, true, error)?;
        }
        Ok(())
    })
}

/// `() => ac.abort()`, whose captures are `[ac]`.
fn abort_controller(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let controller = Captures::of(scope, payload)?.obj(0)?;
    call_method(scope, controller, c"abort", &[])?;
    Ok(js::value::undefined())
}

/// Reject the captured promise with the captured error, whose captures are `[promise, error]`.
fn reject_with(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    settle(
        scope,
        promise_at(scope, &captures, 0)?,
        true,
        captures.get(1)?,
    )?;
    Ok(js::value::undefined())
}

/// Reject the captured promise with the argument, whose captures are `[promise]`.
fn reject_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    settle(scope, promise_at(scope, &captures, 0)?, true, args.get(0))?;
    Ok(js::value::undefined())
}

/// The body of `reduce`'s loop.
fn reduce_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use consume::*;
    let state = state_of(scope, payload)?;
    let lp = consume_loop_of(scope, &state)?;
    let step = (|| -> Result<(), ExnThrown> {
        state.set(GOT_ANY, true)?;
        if signal_aborted(scope, state.get(OPTIONS)?)? {
            let error = abort_error(scope, None)?;
            return loop_close(scope, &lp, Some(error));
        }
        if !state.flag(HAS_INITIAL)? {
            state.set(RESULT, args.get(0))?;
            state.set(HAS_INITIAL, true)?;
            return loop_next(scope, &lp);
        }
        let controller = state.obj(CONTROLLER)?;
        let reducer_options = Object::new_plain(scope)?;
        reducer_options.set_property(scope, c"signal", controller.val(scope, c"signal")?)?;
        let result = call(
            scope,
            HandleValue::undefined(),
            state.get(REDUCER)?,
            &[
                state.get(RESULT)?,
                args.get(0),
                reducer_options.to_jsval_throwing(scope)?,
            ],
        )?;
        let fulfilled = bind(scope, &state, 1, reduce_reduced)?;
        let rejected = bind(scope, &state, 1, reduce_failed)?;
        await_value(scope, result, fulfilled, rejected)
    })();
    if step.is_err() {
        let error = take_exception(scope);
        loop_close(scope, &lp, Some(error))?;
    }
    Ok(js::value::undefined())
}

/// The reducer's result settled with the new accumulator.
fn reduce_reduced(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    state.set(consume::RESULT, args.get(0))?;
    loop_next(scope, &consume_loop_of(scope, &state)?)?;
    Ok(js::value::undefined())
}

fn reduce_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    loop_close(scope, &consume_loop_of(scope, &state)?, Some(args.get(0)))?;
    Ok(js::value::undefined())
}

/// The end of `reduce`'s loop.
fn reduce_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use consume::*;
    let state = state_of(scope, payload)?;
    reduce_finally(scope, &state)?;
    if args.get(0).to_int32() == end::THROW {
        consume_settle(scope, &state, true, args.get(1))?;
    } else if !state.flag(GOT_ANY)? && !state.flag(HAS_INITIAL)? {
        let error = error_value(scope, reduce_missing_initial_value())?;
        consume_settle(scope, &state, true, error)?;
    } else {
        consume_settle(scope, &state, false, state.get(RESULT)?)?;
    }
    Ok(js::value::undefined())
}

/// The `finally` block of `reduce`: abort the signal the reducer gets.
fn reduce_finally(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    let controller = state.obj(consume::CONTROLLER)?;
    call_method(scope, controller, c"abort", &[])?;
    Ok(())
}

/// `Readable.from(generator)`, the stream a stream-returning operator returns.
fn readable_from(
    scope: &Scope<'_>,
    generator: Result<Object<'_>, Thrown>,
) -> Result<Value, ExnThrown> {
    let generator = generator.map_err(|error| error.throw(scope))?;
    let generator = generator.to_jsval_throwing(scope)?;
    let readable = super::from::from(scope, generator, None).map_err(|error| error.throw(scope))?;
    readable.to_jsval_raw_throwing(scope)
}

fn map_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let generator = map_generator(scope, this, args.get(0), args.get(1), None);
    readable_from(scope, generator)
}

fn filter_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let generator = filter_generator(scope, this, args.get(0), args.get(1), false);
    readable_from(scope, generator)
}

fn flat_map_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let generator = flat_map_generator(scope, this, args.get(0), args.get(1));
    readable_from(scope, generator)
}

fn drop_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let generator = skip_generator(scope, this, args.get(0), args.get(1), false);
    readable_from(scope, generator)
}

fn take_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let generator = skip_generator(scope, this, args.get(0), args.get(1), true);
    readable_from(scope, generator)
}

fn every_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    every(scope, this, args.get(0), args.get(1))
}

fn for_each_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    for_each(scope, this, args.get(0), args.get(1))
}

fn reduce_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let initial = (args.len() > 1).then(|| args.get(1));
    reduce(scope, this, args.get(0), initial, args.get(2))
}

fn to_array_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    to_array(scope, this, args.get(0))
}

fn some_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    some_or_find(scope, this, args.get(0), args.get(1), false)
}

fn find_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    some_or_find(scope, this, args.get(0), args.get(1), true)
}

/// Define the operators on `Readable.prototype` as Node does: non-enumerable, with Node's names
/// and lengths, and in Node's order.
pub(crate) fn define_operators(scope: &Scope<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
    for (name, nargs, body) in [
        (c"drop", 1, drop_method as Callback),
        (c"filter", 2, filter_method),
        (c"flatMap", 2, flat_map_method),
        (c"map", 2, map_method),
        (c"take", 1, take_method),
        (c"every", 1, every_method),
        (c"forEach", 2, for_each_method),
        (c"reduce", 3, reduce_method),
        (c"toArray", 1, to_array_method),
        (c"some", 1, some_method),
        (c"find", 2, find_method),
    ] {
        let function = Function::new_callback(scope, name, nargs, body, ())?;
        proto.define_property(scope, name, *function, 0)?;
    }
    Ok(())
}
