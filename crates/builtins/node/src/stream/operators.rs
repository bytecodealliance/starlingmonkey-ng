// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The iterator helpers of `Readable.prototype`: `map`, `filter`, `flatMap`, `drop` and `take`,
//! which return a `Readable.from` stream, and `every`, `forEach`, `reduce`, `toArray`, `some` and
//! `find`, which return a promise. Node's `lib/internal/streams/operators.js`.
//!
//! Node writes these as async generators and async functions. Here a generator is a [`Generator`]
//! whose body is a state machine of promise reactions on a body state, and a `for await` loop is
//! a [`LoopState`] that calls back for each value and at the end.

use std::collections::VecDeque;
use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, Callback, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::iteration::{AsyncIteratorRecord, AsyncIteratorRecordImpl};
use js::native::Value;
use js::prelude::HandleValue;
use js::symbol::CachedSymbol;
use js::{Function, Object};
use web_globals::signals::abort_controller::{AbortController, AbortControllerImpl};
use web_globals::signals::{AbortSignal, AbortSignalImpl};

use crate::errors::{
    out_of_range_received, validate_abort_signal, validate_function, validate_integer,
    validate_plain_object, ErrorKind, NodeError, Thrown,
};

/// `Number.MAX_SAFE_INTEGER`, the default maximum of `validateInteger`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Node's `kEmpty`, the result of `filter`'s function for a value it skips.
static K_EMPTY: CachedSymbol = CachedSymbol::new(c"kEmpty");

/// `options.signal`, if `options` is an object with an `AbortSignal` there.
fn signal_of<'s>(
    scope: &'s Scope<'_>,
    options: HandleValue<'_>,
) -> Result<Option<AbortSignal<'s>>, ExnThrown> {
    let Ok(options) = Object::from_value(scope, options) else {
        return Ok(None);
    };
    let signal = options.get_property(scope, c"signal")?;
    Ok(Object::from_value(scope, signal)
        .ok()
        .and_then(|signal| signal.cast::<AbortSignal>().ok()))
}

/// `options?.signal?.aborted`, for `options` an object or `undefined`.
fn signal_aborted(scope: &Scope<'_>, options: HandleValue<'_>) -> Result<bool, ExnThrown> {
    Ok(signal_of(scope, options)?.is_some_and(|signal| signal.aborted()))
}

/// `options?.signal?.reason`.
fn signal_reason<'s>(
    scope: &'s Scope<'_>,
    options: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    Ok(match signal_of(scope, options)? {
        Some(signal) => signal.reason(scope),
        None => HandleValue::undefined(),
    })
}

/// The validation the operators share: `options` must be `null`, `undefined` or an object, and
/// `options.signal` must be `null`, `undefined` or an `AbortSignal`. Returns `options` as an
/// object, or `undefined`.
fn validate_options<'s>(
    scope: &'s Scope<'_>,
    options: HandleValue<'s>,
) -> Result<HandleValue<'s>, Thrown> {
    if options.is_null_or_undefined() {
        return Ok(HandleValue::undefined());
    }
    let object = validate_plain_object(scope, options, "options", false)?;
    let signal = object.get_property(scope, c"signal")?;
    if !signal.is_null_or_undefined() {
        validate_abort_signal(scope, signal, "options.signal")?;
    }
    Ok(options)
}

/// Run `fulfilled` or `rejected` once `Promise.resolve(value)` settles, as `await value` does.
fn await_value(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    fulfilled: Object<'_>,
    rejected: Object<'_>,
) -> Result<(), ExnThrown> {
    js::Promise::call_original_resolve(scope, value)?.add_reactions(
        scope,
        Some(fulfilled),
        Some(rejected),
    )
}

/// A reaction function running `body` with `payload`.
fn reaction<'s>(
    scope: &'s Scope<'_>,
    body: Callback,
    payload: impl ToJSVal<'s>,
) -> Result<Object<'s>, ExnThrown> {
    Ok(*Function::new_callback(scope, c"", 1, body, payload)?)
}

/// The kinds of request a generator takes.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Request {
    #[default]
    Next,
    Return,
    Throw,
}

/// The phases of a generator: before its first `next()`, suspended at a `yield`, and completed.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Start,
    Yielded,
    Done,
}

/// A queued `next`, `return` or `throw` call of a generator, with its argument and promise.
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct GenRequest {
    #[no_trace]
    kind: Request,
    value: Heap<Value>,
    promise: Heap<js::promise::Promise>,
}

/// The start of a generator's body, which gets the body state.
type BodyStart = fn(&Scope<'_>, Object<'_>) -> Result<(), ExnThrown>;

/// The continuation of a generator's body after it yielded, which gets the body state and the
/// request.
type BodyResume = fn(&Scope<'_>, Object<'_>, Request, HandleValue<'_>) -> Result<(), ExnThrown>;

/// An async generator: the body state, the queued requests, and the body's functions. The
/// requests are queued and run one at a time, as an async generator's are.
#[core_runtime::jsclass(hidden)]
pub(crate) struct Generator {
    body: Option<Heap<js::object::Object>>,
    requests: VecDeque<GenRequest>,
    #[no_trace]
    running: bool,
    #[no_trace]
    phase: Phase,
    #[no_trace]
    start: Option<BodyStart>,
    #[no_trace]
    resume: Option<BodyResume>,
}

#[core_runtime::jsmethods]
impl Generator {}

/// A new async generator object over the body state `body`. Its first `next()` runs `start`. A
/// request after the body yielded runs `resume`. Each step of the body ends with [`gen_yield`],
/// [`gen_return`] or [`gen_throw`], and an exception `start` or `resume` throws ends it as
/// [`gen_throw`] does.
fn new_generator<'s>(
    scope: &'s Scope<'_>,
    body: Object<'_>,
    start: BodyStart,
    resume: BodyResume,
) -> Result<(Generator<'s>, Object<'s>), ExnThrown> {
    let gen = js::class::create_instance_with::<GeneratorImpl>(scope, |_| GeneratorImpl {
        body: Some(Heap::from(body)),
        requests: VecDeque::new(),
        running: false,
        phase: Phase::Start,
        start: Some(start),
        resume: Some(resume),
    })?;
    let proto = js::class::get_async_iterator_prototype(scope)?;
    let object = Object::new_plain_with_proto(scope, proto)?;
    for (name, method) in [
        (c"next", gen_next as Callback),
        (c"return", gen_return_method),
        (c"throw", gen_throw_method),
    ] {
        let function = Function::new_callback(scope, name, 1, method, gen)?;
        object.set_property(scope, name, *function)?;
    }
    Ok((gen, object))
}

fn gen_next(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    gen_request(scope, payload, Request::Next, args.get(0))
}

fn gen_return_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    gen_request(scope, payload, Request::Return, args.get(0))
}

fn gen_throw_method(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    gen_request(scope, payload, Request::Throw, args.get(0))
}

/// Queue a request, run it if the generator isn't running, and return its promise.
fn gen_request(
    scope: &Scope<'_>,
    payload: HandleValue<'_>,
    kind: Request,
    value: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let gen = cast_payload::<Generator>(scope, payload);
    let promise = js::Promise::new_pending(scope)?;
    gen.data_mut().requests.push_back(GenRequest {
        kind,
        value: Heap::from(value.get()),
        promise: Heap::from(promise),
    });
    gen_drain(scope, gen)?;
    promise.to_jsval_raw_throwing(scope)
}

/// The first queued request, as `(kind, value)`.
fn gen_front<'s>(scope: &'s Scope<'_>, gen: Generator<'_>) -> Option<(Request, HandleValue<'s>)> {
    let data = gen.data();
    let request = data.requests.front()?;
    Some((request.kind, request.value.get(scope)))
}

/// Settle the first queued request with `value`, as a rejection or as `{ value, done }`, and
/// remove it.
fn gen_complete(
    scope: &Scope<'_>,
    gen: Generator<'_>,
    reject: bool,
    value: HandleValue<'_>,
    done: bool,
) -> Result<(), ExnThrown> {
    let promise = {
        let data = gen.data();
        let Some(request) = data.requests.front() else {
            return Ok(());
        };
        request.promise.get(scope)
    };
    drop(gen.data_mut().requests.pop_front());
    if reject {
        promise.reject(scope, value)
    } else {
        let result = js::iteration::create_iter_result(scope, value, done)?;
        promise.resolve(scope, result)
    }
}

/// The body state of `gen`.
fn gen_body<'s>(scope: &'s Scope<'_>, gen: Generator<'_>) -> Result<Object<'s>, ExnThrown> {
    gen.data().body.get(scope).ok_or(ExnThrown)
}

/// Run the queued requests until one leaves the generator running or none is left.
fn gen_drain(scope: &Scope<'_>, gen: Generator<'_>) -> Result<(), ExnThrown> {
    loop {
        if gen.data().running {
            return Ok(());
        }
        let Some((kind, value)) = gen_front(scope, gen) else {
            return Ok(());
        };
        let phase = gen.data().phase;
        if phase == Phase::Start && kind != Request::Next {
            gen.data_mut().phase = Phase::Done;
            continue;
        }
        if phase == Phase::Done {
            match kind {
                Request::Next => gen_complete(scope, gen, false, HandleValue::undefined(), true)?,
                Request::Throw => gen_complete(scope, gen, true, value, true)?,
                Request::Return => {
                    gen.data_mut().running = true;
                    let fulfilled = reaction(scope, gen_returned, gen)?;
                    let rejected = reaction(scope, gen_return_failed, gen)?;
                    if await_value(scope, value, fulfilled, rejected).is_err() {
                        gen.data_mut().running = false;
                        let error = js::exception::take_pending_or_undefined(scope);
                        gen_complete(scope, gen, true, error, true)?;
                        continue;
                    }
                    return Ok(());
                }
            }
            continue;
        }
        gen.data_mut().running = true;
        let body = gen_body(scope, gen)?;
        let (start, resume) = {
            let data = gen.data();
            (data.start, data.resume)
        };
        let stepped = match (phase, start, resume) {
            (Phase::Start, Some(start), _) => start(scope, body),
            (_, _, Some(resume)) => resume(scope, body, kind, value),
            _ => Ok(()),
        };
        if stepped.is_err() {
            let error = js::exception::take_pending_or_undefined(scope);
            gen_end(scope, gen, true, error)?;
        }
        return Ok(());
    }
}

/// The value of a `return(value)` request to a completed generator settled.
fn gen_returned(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = cast_payload::<Generator>(scope, payload);
    gen.data_mut().running = false;
    gen_complete(scope, gen, false, args.get(0), true)?;
    gen_drain(scope, gen)?;
    Ok(js::value::undefined())
}

fn gen_return_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = cast_payload::<Generator>(scope, payload);
    gen.data_mut().running = false;
    gen_complete(scope, gen, true, args.get(0), true)?;
    gen_drain(scope, gen)?;
    Ok(js::value::undefined())
}

/// Complete the generator: fulfill the current request with `{ value, done: true }`, or reject
/// it with `value`.
fn gen_end(
    scope: &Scope<'_>,
    gen: Generator<'_>,
    reject: bool,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    {
        let mut data = gen.data_mut();
        data.phase = Phase::Done;
        data.running = false;
    }
    gen_complete(scope, gen, reject, value, true)?;
    gen_drain(scope, gen)
}

/// The body of `gen` yields `value`: once it settles, as the `await` of a `yield` does, fulfill
/// the current request with `{ value, done: false }`. A rejection resumes the body with a `throw`
/// request.
fn gen_yield(
    scope: &Scope<'_>,
    gen: Generator<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let fulfilled = reaction(scope, gen_yielded, gen)?;
    let rejected = reaction(scope, gen_yield_failed, gen)?;
    await_value(scope, value, fulfilled, rejected)
}

fn gen_yielded(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = cast_payload::<Generator>(scope, payload);
    {
        let mut data = gen.data_mut();
        data.phase = Phase::Yielded;
        data.running = false;
    }
    gen_complete(scope, gen, false, args.get(0), false)?;
    gen_drain(scope, gen)?;
    Ok(js::value::undefined())
}

fn gen_yield_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let gen = cast_payload::<Generator>(scope, payload);
    let body = gen_body(scope, gen)?;
    let resume = gen.data().resume;
    let resumed = match resume {
        Some(resume) => resume(scope, body, Request::Throw, args.get(0)),
        None => Ok(()),
    };
    if resumed.is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        gen_end(scope, gen, true, error)?;
    }
    Ok(js::value::undefined())
}

/// The body of `gen` returns `value`: once it settles, complete the generator with it.
fn gen_return(
    scope: &Scope<'_>,
    gen: Generator<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let fulfilled = reaction(scope, gen_return_settled, gen)?;
    let rejected = reaction(scope, gen_throw_settled, gen)?;
    await_value(scope, value, fulfilled, rejected)
}

fn gen_return_settled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    gen_end(
        scope,
        cast_payload::<Generator>(scope, payload),
        false,
        args.get(0),
    )?;
    Ok(js::value::undefined())
}

fn gen_throw_settled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    gen_end(
        scope,
        cast_payload::<Generator>(scope, payload),
        true,
        args.get(0),
    )?;
    Ok(js::value::undefined())
}

/// The body of `gen` throws `error`.
fn gen_throw(
    scope: &Scope<'_>,
    gen: Generator<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    gen_end(scope, gen, true, error)
}

/// The ways a `for await` loop ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    /// The iterator is done.
    Done,
    /// The iterator, or the loop body through [`loop_close`], threw the value.
    Throw,
    /// [`loop_close`] without an error closed the iterator.
    Closed,
}

/// The body of a `for await` loop, which gets the loop's owner and the value.
type LoopValue = fn(&Scope<'_>, Object<'_>, HandleValue<'_>) -> Result<(), ExnThrown>;

/// The end of a `for await` loop, which gets the loop's owner, how it ended, and the error it
/// threw.
type LoopEnd = fn(&Scope<'_>, Object<'_>, End, HandleValue<'_>) -> Result<(), ExnThrown>;

/// A `for await` loop: the iterator record, the owner whose functions run for each value and at
/// the end, and the error a [`loop_close`] with an error ends it with.
#[core_runtime::jsclass(hidden)]
pub(crate) struct LoopState {
    record: Option<Heap<AsyncIteratorRecordImpl>>,
    owner: Option<Heap<js::object::Object>>,
    #[no_trace]
    on_value: Option<LoopValue>,
    #[no_trace]
    on_end: Option<LoopEnd>,
    close_error: Option<Heap<Value>>,
}

#[core_runtime::jsmethods]
impl LoopState {}

/// Start a `for await (const value of iterable)` loop: get the iterator of `iterable`, and return
/// the loop's state. The loop calls `on_value(owner, value)` for each value, which continues it
/// with [`loop_next`] or leaves it with [`loop_close`], and `on_end(owner, end, error)` once it
/// ended. Nothing runs before the first [`loop_next`].
fn loop_open<'s>(
    scope: &'s Scope<'_>,
    iterable: HandleValue<'_>,
    owner: Object<'_>,
    on_value: LoopValue,
    on_end: LoopEnd,
) -> Result<LoopState<'s>, ExnThrown> {
    let record = js::iteration::get_async_iterator(scope, iterable)?;
    js::class::create_instance_with::<LoopStateImpl>(scope, |_| LoopStateImpl {
        record: Some(Heap::from(record)),
        owner: Some(Heap::from(owner)),
        on_value: Some(on_value),
        on_end: Some(on_end),
        close_error: None,
    })
}

/// The iterator record of `state`.
fn loop_record<'s>(
    scope: &'s Scope<'_>,
    state: LoopState<'_>,
) -> Result<AsyncIteratorRecord<'s>, ExnThrown> {
    state.data().record.get(scope).ok_or(ExnThrown)
}

/// End the loop with `end` and `value`.
fn loop_end(
    scope: &Scope<'_>,
    state: LoopState<'_>,
    end: End,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let (owner, on_end) = {
        let data = state.data();
        (data.owner.get(scope), data.on_end)
    };
    match (owner, on_end) {
        (Some(owner), Some(on_end)) => on_end(scope, owner, end, value),
        _ => Ok(()),
    }
}

/// End the loop with the pending exception.
fn loop_end_pending(scope: &Scope<'_>, state: LoopState<'_>) -> Result<(), ExnThrown> {
    let error = js::exception::take_pending_or_undefined(scope);
    loop_end(scope, state, End::Throw, error)
}

/// Take the loop's next value: call the iterator's `next()`, and await its result.
fn loop_next(scope: &Scope<'_>, state: LoopState<'_>) -> Result<(), ExnThrown> {
    let step = (|| -> Result<(), ExnThrown> {
        let result = loop_record(scope, state)?.call_next(scope)?;
        let fulfilled = reaction(scope, loop_result, state)?;
        let rejected = reaction(scope, loop_failed, state)?;
        await_value(scope, result, fulfilled, rejected)
    })();
    if step.is_err() {
        return loop_end_pending(scope, state);
    }
    Ok(())
}

/// The iterator's `next()` fulfilled with `result`.
fn loop_result(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<LoopState>(scope, payload);
    let step = (|| -> Result<Option<HandleValue<'_>>, ExnThrown> {
        let result = js::iteration::iter_result_object(scope, args.get(0))?;
        if js::iteration::iter_result_done(scope, &result)? {
            return Ok(None);
        }
        Ok(Some(js::iteration::iter_result_value(scope, &result)?))
    })();
    match step {
        Ok(Some(value)) => {
            let (owner, on_value) = {
                let data = state.data();
                (data.owner.get(scope), data.on_value)
            };
            if let (Some(owner), Some(on_value)) = (owner, on_value) {
                on_value(scope, owner, value)?;
            }
        }
        Ok(None) => loop_end(scope, state, End::Done, HandleValue::undefined())?,
        Err(_) => loop_end_pending(scope, state)?,
    }
    Ok(js::value::undefined())
}

fn loop_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    loop_end(
        scope,
        cast_payload::<LoopState>(scope, payload),
        End::Throw,
        args.get(0),
    )?;
    Ok(js::value::undefined())
}

/// Leave the loop, as `break` does without `error` and `throw error` does with it: close the
/// iterator, as `AsyncIteratorClose` does, and end the loop. The loop ends with `error` if given,
/// and otherwise with an exception `return()` throws, its rejection, or `ERR_INVALID_RETURN_VALUE`
/// for a result that isn't an object.
fn loop_close(
    scope: &Scope<'_>,
    state: LoopState<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let record = loop_record(scope, state)?;
    let closing = match error {
        Some(error) => {
            state.data_mut().close_error = Some(Heap::from(error.get()));
            Ok(record.close_after_throw(scope))
        }
        None => record.call_return(scope),
    };
    match closing {
        Ok(Some(promise)) => {
            let fulfilled = reaction(scope, loop_closed, state)?;
            let rejected = reaction(scope, loop_close_failed, state)?;
            promise.add_reactions(scope, Some(fulfilled), Some(rejected))
        }
        Ok(None) => match error {
            Some(error) => loop_end(scope, state, End::Throw, error),
            None => loop_end(scope, state, End::Closed, HandleValue::undefined()),
        },
        Err(ExnThrown) => loop_end_pending(scope, state),
    }
}

/// The result of the iterator's `return()` settled.
fn loop_closed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<LoopState>(scope, payload);
    let close_error = state
        .data()
        .close_error
        .as_ref()
        .map(|error| error.get(scope));
    if let Some(error) = close_error {
        loop_end(scope, state, End::Throw, error)?;
    } else if !args.get(0).is_object() {
        let error = NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_RETURN_VALUE",
            "iterator.return() did not return an object",
        )
        .to_value(scope)?;
        loop_end(scope, state, End::Throw, error)?;
    } else {
        loop_end(scope, state, End::Closed, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

fn loop_close_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<LoopState>(scope, payload);
    let close_error = state
        .data()
        .close_error
        .as_ref()
        .map(|error| error.get(scope));
    let error = close_error.unwrap_or(args.get(0));
    loop_end(scope, state, End::Throw, error)?;
    Ok(js::value::undefined())
}

/// A result of `map`'s function in its queue, or the end of the stream.
#[js::must_root]
#[derive(core_runtime::Traceable)]
enum MapEntry {
    Result(Heap<js::promise::Promise>),
    Eof,
}

/// The state of a `map` generator: Node's variables of the same names. `next` and `resume` are the
/// pending promises the body and the pump await.
#[core_runtime::jsclass(hidden)]
pub(crate) struct MapState {
    gen: Option<Heap<GeneratorImpl>>,
    stream: Heap<Value>,
    function: Heap<Value>,
    options: Heap<Value>,
    signal: Option<Heap<AbortSignalImpl>>,
    signal_opt: Option<Heap<js::object::Object>>,
    queue: VecDeque<MapEntry>,
    next: Option<Heap<js::promise::Promise>>,
    resume: Option<Heap<js::promise::Promise>>,
    pump: Option<Heap<LoopStateImpl>>,
    #[no_trace]
    done: bool,
    #[no_trace]
    cnt: f64,
    #[no_trace]
    concurrency: f64,
    #[no_trace]
    high_water_mark: f64,
}

#[core_runtime::jsmethods]
impl MapState {}

/// `MathFloor(value)`: `Math.floor` of `value` converted to a number.
fn floor(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, ExnThrown> {
    Ok(f64::from_jsval(scope, value, ())
        .map_err(|_| ExnThrown)?
        .floor())
}

/// The generator of `map` for `stream`, `fn` and `options`, as an object.
fn map_generator<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    function: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    validate_function(scope, function, "fn")?;
    let options = scope.root_value(options.get());
    let options = validate_options(scope, options)?;
    let option = |name: &'static CStr| -> Result<HandleValue<'_>, ExnThrown> {
        match Object::from_value(scope, options).ok() {
            Some(options) => options.get_property(scope, name),
            None => Ok(HandleValue::undefined()),
        }
    };
    let mut concurrency = 1.0;
    let value = option(c"concurrency")?;
    if !value.is_null_or_undefined() {
        concurrency = floor(scope, option(c"concurrency")?)?;
    }
    let mut high_water_mark = concurrency - 1.0;
    let value = option(c"highWaterMark")?;
    if !value.is_null_or_undefined() {
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

    let state = js::class::create_instance_with::<MapStateImpl>(scope, |_| MapStateImpl {
        gen: None,
        stream: Heap::from(stream.get()),
        function: Heap::from(function.get()),
        options: Heap::from(options.get()),
        signal: None,
        signal_opt: None,
        queue: VecDeque::new(),
        next: None,
        resume: None,
        pump: None,
        done: false,
        cnt: 0.0,
        concurrency,
        high_water_mark,
    })?;
    let (gen, object) = new_generator(scope, *state, map_start, map_resume)?;
    state.data_mut().gen = Some(Heap::from(gen));
    Ok(object)
}

/// The [`MapState`] that is `body`.
fn map_state(body: Object<'_>) -> MapState<'_> {
    body.cast::<MapState>().expect("the body is a MapState")
}

/// The generator of `state`.
fn map_gen<'s>(scope: &'s Scope<'_>, state: MapState<'_>) -> Result<Generator<'s>, ExnThrown> {
    state.data().gen.get(scope).ok_or(ExnThrown)
}

/// The start of `map`'s body: create the signal, start the pump, and wait for the first result.
fn map_start(scope: &Scope<'_>, body: Object<'_>) -> Result<(), ExnThrown> {
    let state = map_state(body);
    let signals = js::Array::new(scope, 0)?;
    let options = state.data().options.get(scope);
    if let Ok(options) = Object::from_value(scope, options) {
        let signal = options.get_property(scope, c"signal")?;
        if js::conversion::to_boolean(signal) {
            signals.push(scope, signal)?;
        }
    }
    let signal = AbortSignalImpl::any(scope, signals.to_jsval_throwing(scope)?)?;
    let signal_opt = Object::new_plain(scope)?;
    signal_opt.set_property(scope, c"signal", *signal)?;
    {
        let mut data = state.data_mut();
        data.signal = Some(Heap::from(signal));
        data.signal_opt = Some(Heap::from(signal_opt));
    }
    pump_start(scope, state)?;
    map_continue(scope, state);
    Ok(())
}

/// Whether `state`'s signal aborted.
fn map_aborted(scope: &Scope<'_>, state: MapState<'_>) -> bool {
    state
        .data()
        .signal
        .get(scope)
        .is_some_and(|signal| signal.aborted())
}

/// The pending promise of a [`MapState`] that a call of Node's `next()` or `resume()` resolves.
type Slot = fn(&mut MapStateImpl) -> &mut Option<Heap<js::promise::Promise>>;

fn next_slot(data: &mut MapStateImpl) -> &mut Option<Heap<js::promise::Promise>> {
    &mut data.next
}

fn resume_slot(data: &mut MapStateImpl) -> &mut Option<Heap<js::promise::Promise>> {
    &mut data.resume
}

/// Resolve the pending promise `slot` selects, if any, and clear it: a call of Node's `next()` or
/// `resume()`.
fn wake(scope: &Scope<'_>, state: MapState<'_>, slot: Slot) -> Result<(), ExnThrown> {
    let promise = slot(&mut state.data_mut()).take_rooted(scope);
    if let Some(promise) = promise.filter(|promise| promise.is_pending()) {
        promise.resolve(scope, HandleValue::undefined())?;
    }
    Ok(())
}

/// Store a new pending promise in `slot`, and run `then` with `state` once it is resolved:
/// `await new Promise((resolve) => { variable = resolve; })`.
fn wait(
    scope: &Scope<'_>,
    state: MapState<'_>,
    slot: Slot,
    then: Callback,
) -> Result<(), ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    *slot(&mut state.data_mut()) = Some(Heap::from(promise));
    let fulfilled = reaction(scope, then, state)?;
    promise.add_reactions(scope, Some(fulfilled), None)
}

/// Node's `maybeResume()`: resume the pump if it waits and there is room.
fn maybe_resume(scope: &Scope<'_>, state: MapState<'_>) -> Result<(), ExnThrown> {
    let resume = {
        let data = state.data();
        data.resume.is_some()
            && !data.done
            && data.cnt < data.concurrency
            && (data.queue.len() as f64) < data.high_water_mark
    };
    if resume {
        wake(scope, state, resume_slot)?;
    }
    Ok(())
}

/// Node's `afterItemProcessed()`.
fn after_item_processed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<MapState>(scope, payload);
    state.data_mut().cnt -= 1.0;
    maybe_resume(scope, state)?;
    Ok(js::value::undefined())
}

/// Node's `onCatch()`.
fn on_catch(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    cast_payload::<MapState>(scope, payload).data_mut().done = true;
    after_item_processed(scope, args, payload)
}

/// Queue `promise`, the result for one value, after the reactions that count it as processed.
fn queue_result(
    scope: &Scope<'_>,
    state: MapState<'_>,
    promise: js::Promise<'_>,
) -> Result<(), ExnThrown> {
    let processed = reaction(scope, after_item_processed, state)?;
    let failed = reaction(scope, on_catch, state)?;
    promise.add_reactions(scope, Some(processed), Some(failed))?;
    state
        .data_mut()
        .queue
        .push_back(MapEntry::Result(Heap::from(promise)));
    Ok(())
}

/// Node's `pump()`: start the `for await` loop over the stream.
fn pump_start(scope: &Scope<'_>, state: MapState<'_>) -> Result<(), ExnThrown> {
    let stream = state.data().stream.get(scope);
    match loop_open(scope, stream, *state, pump_value, pump_end) {
        Ok(pump) => {
            state.data_mut().pump = Some(Heap::from(pump));
            loop_next(scope, pump)
        }
        Err(_) => {
            let error = js::exception::take_pending_or_undefined(scope);
            pump_catch(scope, state, error)?;
            pump_finally(scope, state)
        }
    }
}

/// The pump's loop.
fn map_pump<'s>(scope: &'s Scope<'_>, state: MapState<'_>) -> Result<LoopState<'s>, ExnThrown> {
    state.data().pump.get(scope).ok_or(ExnThrown)
}

/// The body of the pump's loop, for the value `val`.
fn pump_value(
    scope: &Scope<'_>,
    owner: Object<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = map_state(owner);
    let pump = map_pump(scope, state)?;
    if state.data().done {
        return loop_close(scope, pump, None);
    }
    if map_aborted(scope, state) {
        let error = crate::errors::abort_error(scope, None)?;
        return loop_close(scope, pump, Some(error));
    }
    let mapped = (|| -> Result<js::Promise<'_>, ExnThrown> {
        let (function, signal_opt) = {
            let data = state.data();
            (data.function.get(scope), data.signal_opt.get(scope))
        };
        let signal_opt = match signal_opt {
            Some(signal_opt) => scope.root_value(signal_opt.as_value()),
            None => HandleValue::undefined(),
        };
        let value = Function::call(
            scope,
            HandleValue::undefined(),
            function,
            &[value, signal_opt],
        )?;
        js::Promise::call_original_resolve(scope, value)
    })();
    let promise = match mapped {
        Ok(promise) => promise,
        Err(_) => {
            let error = js::exception::take_pending_or_undefined(scope);
            js::Promise::new_rejected_with_error(scope, error)?
        }
    };
    state.data_mut().cnt += 1.0;
    queue_result(scope, state, promise)?;
    wake(scope, state, next_slot)?;
    let wait_for_room = {
        let data = state.data();
        !data.done
            && ((data.queue.len() as f64) >= data.high_water_mark || data.cnt >= data.concurrency)
    };
    if wait_for_room {
        wait(scope, state, resume_slot, pump_resumed)
    } else {
        loop_next(scope, pump)
    }
}

fn pump_resumed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<MapState>(scope, payload);
    loop_next(scope, map_pump(scope, state)?)?;
    Ok(js::value::undefined())
}

/// The end of the pump's loop: queue the end after the last value, or a rejection with the error.
fn pump_end(
    scope: &Scope<'_>,
    owner: Object<'_>,
    end: End,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = map_state(owner);
    match end {
        End::Done => state.data_mut().queue.push_back(MapEntry::Eof),
        End::Throw => pump_catch(scope, state, error)?,
        End::Closed => {}
    }
    pump_finally(scope, state)
}

/// The `catch` block of the pump.
fn pump_catch(
    scope: &Scope<'_>,
    state: MapState<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let promise = js::Promise::new_rejected_with_error(scope, error)?;
    queue_result(scope, state, promise)
}

/// The `finally` block of the pump.
fn pump_finally(scope: &Scope<'_>, state: MapState<'_>) -> Result<(), ExnThrown> {
    state.data_mut().done = true;
    wake(scope, state, next_slot)
}

/// Continue `map`'s body at the top of its loop, and end the body with an exception the step
/// throws.
fn map_continue(scope: &Scope<'_>, state: MapState<'_>) {
    if map_loop(scope, state).is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        let _ = map_fail(scope, state, error);
    }
}

/// The top of `map`'s loop: await the first queued result, or wait for one.
fn map_loop(scope: &Scope<'_>, state: MapState<'_>) -> Result<(), ExnThrown> {
    enum Front<'r> {
        Empty,
        Result(js::Promise<'r>),
        Eof,
    }
    let front = match state.data().queue.front() {
        None => Front::Empty,
        Some(MapEntry::Result(promise)) => Front::Result(promise.get(scope)),
        Some(MapEntry::Eof) => Front::Eof,
    };
    match front {
        Front::Empty => wait(scope, state, next_slot, map_woken),
        Front::Result(promise) => {
            let fulfilled = reaction(scope, map_awaited, state)?;
            let rejected = reaction(scope, map_rejected, state)?;
            await_value(
                scope,
                promise.to_jsval_throwing(scope)?,
                fulfilled,
                rejected,
            )
        }
        Front::Eof => {
            let fulfilled = reaction(scope, map_eof, state)?;
            let rejected = reaction(scope, map_rejected, state)?;
            await_value(scope, HandleValue::undefined(), fulfilled, rejected)
        }
    }
}

fn map_woken(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    map_continue(scope, cast_payload::<MapState>(scope, payload));
    Ok(js::value::undefined())
}

/// The end of the stream reached the front of the queue: return.
fn map_eof(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<MapState>(scope, payload);
    let step = (|| -> Result<(), ExnThrown> {
        map_finally(scope, state)?;
        gen_return(scope, map_gen(scope, state)?, HandleValue::undefined())
    })();
    if step.is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        map_fail(scope, state, error)?;
    }
    Ok(js::value::undefined())
}

/// The first queued result settled with `val`: yield anything but `kEmpty`.
fn map_awaited(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<MapState>(scope, payload);
    let step = (|| -> Result<(), ExnThrown> {
        let value = args.get(0);
        if map_aborted(scope, state) {
            let error = crate::errors::abort_error(scope, None)?;
            return map_fail(scope, state, error);
        }
        if js::comparison::strictly_equal(scope, value, K_EMPTY.value(scope)?)? {
            return map_after_yield(scope, state);
        }
        gen_yield(scope, map_gen(scope, state)?, value)
    })();
    if step.is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        map_fail(scope, state, error)?;
    }
    Ok(js::value::undefined())
}

fn map_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    map_fail(scope, cast_payload::<MapState>(scope, payload), args.get(0))?;
    Ok(js::value::undefined())
}

/// After the body yielded: remove the result, resume the pump if there is room, and continue.
fn map_after_yield(scope: &Scope<'_>, state: MapState<'_>) -> Result<(), ExnThrown> {
    drop(state.data_mut().queue.pop_front());
    maybe_resume(scope, state)?;
    map_loop(scope, state)
}

/// A request to `map`'s body after it yielded.
fn map_resume(
    scope: &Scope<'_>,
    body: Object<'_>,
    kind: Request,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = map_state(body);
    match kind {
        Request::Next => {
            if map_after_yield(scope, state).is_err() {
                let error = js::exception::take_pending_or_undefined(scope);
                map_fail(scope, state, error)?;
            }
            Ok(())
        }
        Request::Return => {
            map_finally(scope, state)?;
            gen_return(scope, map_gen(scope, state)?, value)
        }
        Request::Throw => map_fail(scope, state, value),
    }
}

/// `map`'s body throws `error`, after its `finally` block.
fn map_fail(
    scope: &Scope<'_>,
    state: MapState<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    map_finally(scope, state)?;
    gen_throw(scope, map_gen(scope, state)?, error)
}

/// The `finally` block of `map`'s body: stop the pump.
fn map_finally(scope: &Scope<'_>, state: MapState<'_>) -> Result<(), ExnThrown> {
    state.data_mut().done = true;
    wake(scope, state, resume_slot)
}

/// The function `filterFn` or `forEachFn` wraps: `filter`'s or `forEach`'s `fn`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct FilterFn {
    function: Heap<Value>,
    #[no_trace]
    for_each: bool,
}

#[core_runtime::jsmethods]
impl FilterFn {}

/// `async function filterFn(value, options)` and `async function forEachFn(value, options)`,
/// whose payload is a [`FilterFn`]: call `fn`, and fulfill with `value` if `fn`'s result is truthy
/// and with `kEmpty` otherwise, or always with `kEmpty` for `forEach`.
fn filter_fn(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let filter = cast_payload::<FilterFn>(scope, payload);
    let (function, for_each) = {
        let data = filter.data();
        (data.function.get(scope), data.for_each)
    };
    let value = args.get(0);
    let promise = Function::call(
        scope,
        HandleValue::undefined(),
        function,
        &[value, args.get(1)],
    )
    .and_then(|result| js::Promise::call_original_resolve(scope, result));
    let promise = match promise {
        Ok(promise) => promise,
        Err(_) => {
            let error = js::exception::take_pending_or_undefined(scope);
            return js::Promise::new_rejected_with_error(scope, error)?
                .to_jsval_raw_throwing(scope);
        }
    };
    let decide = if for_each {
        js::class::get_or_init_shared_function(
            scope,
            empty_result as *const () as usize,
            |scope| Function::new_callback(scope, c"", 1, empty_result, ()),
        )?
    } else {
        Function::new_callback(scope, c"", 1, filter_decide, value)?
    };
    promise
        .then(scope, Some(*decide), None)?
        .to_jsval_raw_throwing(scope)
}

/// The result of `filterFn` once `fn`'s result settled, whose payload is the value: the value if
/// the result is truthy, and `kEmpty` otherwise.
fn filter_decide(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if js::conversion::to_boolean(args.get(0)) {
        return Ok(payload.get());
    }
    Ok(K_EMPTY.value(scope)?.get())
}

/// The result of `forEachFn` once `fn`'s result settled: `kEmpty`.
fn empty_result(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(K_EMPTY.value(scope)?.get())
}

/// Node's `filter(fn, options)`: `map` with `filterFn`, or with `forEachFn` for `forEach`.
fn filter_generator<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    function: HandleValue<'_>,
    options: HandleValue<'_>,
    for_each: bool,
) -> Result<Object<'s>, Thrown> {
    validate_function(scope, function, "fn")?;
    let filter = js::class::create_instance_with::<FilterFnImpl>(scope, |_| FilterFnImpl {
        function: Heap::from(function.get()),
        for_each,
    })?;
    let wrapped = Function::new_callback(scope, c"", 2, filter_fn, filter)?;
    map_generator(scope, stream, (*wrapped).to_jsval_throwing(scope)?, options)
}

/// The state of a `drop` or `take` generator: the stream, the options, the count, the loop, and
/// the value of a `return()` request that closes the loop.
#[core_runtime::jsclass(hidden)]
pub(crate) struct SkipState {
    gen: Option<Heap<GeneratorImpl>>,
    stream: Heap<Value>,
    options: Heap<Value>,
    over: Option<Heap<LoopStateImpl>>,
    return_value: Heap<Value>,
    #[no_trace]
    number: f64,
    #[no_trace]
    take: bool,
}

#[core_runtime::jsmethods]
impl SkipState {}

/// Node's `toIntegerOrInfinity(number)`: `Number(number)`, 0 for `NaN`, and `ERR_OUT_OF_RANGE`
/// for a negative number.
fn to_integer_or_infinity(scope: &Scope<'_>, number: HandleValue<'_>) -> Result<f64, Thrown> {
    let number = f64::from_jsval(scope, number, ()).map_err(|_| ExnThrown)?;
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
    let options = scope.root_value(options.get());
    let options = validate_options(scope, options)?;
    let number = to_integer_or_infinity(scope, number)?;
    let state = js::class::create_instance_with::<SkipStateImpl>(scope, |_| SkipStateImpl {
        gen: None,
        stream: Heap::from(stream.get()),
        options: Heap::from(options.get()),
        over: None,
        return_value: Heap::default(),
        number,
        take,
    })?;
    let (gen, object) = new_generator(scope, *state, skip_start, skip_resume)?;
    state.data_mut().gen = Some(Heap::from(gen));
    Ok(object)
}

/// The [`SkipState`] that is `body`.
fn skip_state(body: Object<'_>) -> SkipState<'_> {
    body.cast::<SkipState>().expect("the body is a SkipState")
}

/// The generator of `state`.
fn skip_gen<'s>(scope: &'s Scope<'_>, state: SkipState<'_>) -> Result<Generator<'s>, ExnThrown> {
    state.data().gen.get(scope).ok_or(ExnThrown)
}

/// The loop of `state`.
fn skip_loop<'s>(scope: &'s Scope<'_>, state: SkipState<'_>) -> Result<LoopState<'s>, ExnThrown> {
    state.data().over.get(scope).ok_or(ExnThrown)
}

fn skip_start(scope: &Scope<'_>, body: Object<'_>) -> Result<(), ExnThrown> {
    let state = skip_state(body);
    let (stream, options) = {
        let data = state.data();
        (data.stream.get(scope), data.options.get(scope))
    };
    if signal_aborted(scope, options)? {
        let error = crate::errors::abort_error(scope, None)?;
        return gen_throw(scope, skip_gen(scope, state)?, error);
    }
    let over = loop_open(scope, stream, body, skip_value, skip_end)?;
    state.data_mut().over = Some(Heap::from(over));
    loop_next(scope, over)
}

/// The body of the loop of `drop` and `take`.
fn skip_value(
    scope: &Scope<'_>,
    owner: Object<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = skip_state(owner);
    let over = skip_loop(scope, state)?;
    let step = (|| -> Result<(), ExnThrown> {
        let options = state.data().options.get(scope);
        if signal_aborted(scope, options)? {
            let error = crate::errors::abort_error(scope, None)?;
            return loop_close(scope, over, Some(error));
        }
        let (number, take) = {
            let mut data = state.data_mut();
            let number = data.number;
            data.number -= 1.0;
            (number, data.take)
        };
        if (!take && number <= 0.0) || (take && number > 0.0) {
            return gen_yield(scope, skip_gen(scope, state)?, value);
        }
        skip_continue(scope, state, over)
    })();
    if step.is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        loop_close(scope, over, Some(error))?;
    }
    Ok(())
}

/// The end of an iteration of the loop of `drop` and `take`: `take` returns once it took the
/// number of values.
fn skip_continue(
    scope: &Scope<'_>,
    state: SkipState<'_>,
    over: LoopState<'_>,
) -> Result<(), ExnThrown> {
    let finished = {
        let data = state.data();
        data.take && data.number <= 0.0
    };
    if finished {
        return loop_close(scope, over, None);
    }
    loop_next(scope, over)
}

fn skip_resume(
    scope: &Scope<'_>,
    body: Object<'_>,
    kind: Request,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = skip_state(body);
    let over = skip_loop(scope, state)?;
    match kind {
        Request::Next => skip_continue(scope, state, over),
        Request::Return => {
            state.data().return_value.set(value.get());
            loop_close(scope, over, None)
        }
        Request::Throw => loop_close(scope, over, Some(value)),
    }
}

fn skip_end(
    scope: &Scope<'_>,
    owner: Object<'_>,
    end: End,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = skip_state(owner);
    let gen = skip_gen(scope, state)?;
    match end {
        End::Throw => gen_throw(scope, gen, error),
        End::Closed => {
            let value = state.data().return_value.get(scope);
            gen_return(scope, gen, value)
        }
        End::Done => gen_return(scope, gen, HandleValue::undefined()),
    }
}

/// The state of a `flatMap` generator: the `map` generator, the loop over it, the loop of the
/// current `yield*`, and the value of a `return()` request.
#[core_runtime::jsclass(hidden)]
pub(crate) struct FlatState {
    gen: Option<Heap<GeneratorImpl>>,
    values: Heap<js::object::Object>,
    outer: Option<Heap<LoopStateImpl>>,
    inner: Option<Heap<LoopStateImpl>>,
    return_value: Heap<Value>,
}

#[core_runtime::jsmethods]
impl FlatState {}

/// Node's `flatMap(fn, options)`: the values of the iterables `map(fn, options)` yields.
fn flat_map_generator<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    function: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let values = map_generator(scope, stream, function, options)?;
    let state = js::class::create_instance_with::<FlatStateImpl>(scope, |_| FlatStateImpl {
        gen: None,
        values: Heap::from(values),
        outer: None,
        inner: None,
        return_value: Heap::default(),
    })?;
    let (gen, object) = new_generator(scope, *state, flat_start, flat_resume)?;
    state.data_mut().gen = Some(Heap::from(gen));
    Ok(object)
}

/// The [`FlatState`] that is `body`.
fn flat_state(body: Object<'_>) -> FlatState<'_> {
    body.cast::<FlatState>().expect("the body is a FlatState")
}

fn flat_gen<'s>(scope: &'s Scope<'_>, state: FlatState<'_>) -> Result<Generator<'s>, ExnThrown> {
    state.data().gen.get(scope).ok_or(ExnThrown)
}

fn flat_outer<'s>(scope: &'s Scope<'_>, state: FlatState<'_>) -> Result<LoopState<'s>, ExnThrown> {
    state.data().outer.get(scope).ok_or(ExnThrown)
}

fn flat_start(scope: &Scope<'_>, body: Object<'_>) -> Result<(), ExnThrown> {
    let state = flat_state(body);
    let values = state.data().values.get(scope);
    let outer = loop_open(
        scope,
        values.to_jsval_throwing(scope)?,
        body,
        flat_outer_value,
        flat_outer_end,
    )?;
    state.data_mut().outer = Some(Heap::from(outer));
    loop_next(scope, outer)
}

/// `yield* val` for a value of the `map` generator.
fn flat_outer_value(
    scope: &Scope<'_>,
    owner: Object<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = flat_state(owner);
    let opened = (|| -> Result<(), ExnThrown> {
        let inner = loop_open(scope, value, owner, flat_inner_value, flat_inner_end)?;
        state.data_mut().inner = Some(Heap::from(inner));
        loop_next(scope, inner)
    })();
    if opened.is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        loop_close(scope, flat_outer(scope, state)?, Some(error))?;
    }
    Ok(())
}

fn flat_inner_value(
    scope: &Scope<'_>,
    owner: Object<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    gen_yield(scope, flat_gen(scope, flat_state(owner))?, value)
}

/// The end of a `yield*`: continue the loop over the `map` generator, or leave it.
fn flat_inner_end(
    scope: &Scope<'_>,
    owner: Object<'_>,
    end: End,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = flat_state(owner);
    state.data_mut().inner = None;
    let outer = flat_outer(scope, state)?;
    match end {
        End::Done => loop_next(scope, outer),
        End::Throw => loop_close(scope, outer, Some(error)),
        End::Closed => loop_close(scope, outer, None),
    }
}

fn flat_outer_end(
    scope: &Scope<'_>,
    owner: Object<'_>,
    end: End,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = flat_state(owner);
    let gen = flat_gen(scope, state)?;
    match end {
        End::Throw => gen_throw(scope, gen, error),
        End::Closed => {
            let value = state.data().return_value.get(scope);
            gen_return(scope, gen, value)
        }
        End::Done => gen_return(scope, gen, HandleValue::undefined()),
    }
}

/// A request to `flatMap`'s body, which yielded from the current `yield*`.
fn flat_resume(
    scope: &Scope<'_>,
    body: Object<'_>,
    kind: Request,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = flat_state(body);
    let inner = state.data().inner.get(scope).ok_or(ExnThrown)?;
    match kind {
        Request::Next => loop_next(scope, inner),
        Request::Return => {
            state.data().return_value.set(value.get());
            loop_close(scope, inner, None)
        }
        Request::Throw => loop_close(scope, inner, Some(value)),
    }
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
        let error = js::exception::take_pending_or_undefined(scope);
        if promise.is_pending() {
            promise.reject(scope, error)?;
        }
    }
    promise.to_jsval_raw_throwing(scope)
}

/// What a promise-returning operator does with the values of its loop.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Consume {
    /// `toArray`: collect them.
    #[default]
    ToArray,
    /// `forEach`: run the loop to its end.
    ForEach,
    /// `some`: fulfill with `true` at the first one.
    Some,
    /// `find`: fulfill with the first one.
    Find,
    /// `reduce`: reduce them.
    Reduce,
}

/// The state of a promise-returning operator: its promise, its loop and options, and the result,
/// with the values `toArray` collects and the reducer and controller of `reduce`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ConsumeState {
    promise: Option<Heap<js::promise::Promise>>,
    over: Option<Heap<LoopStateImpl>>,
    options: Heap<Value>,
    result: Heap<Value>,
    values: Vec<Heap<Value>>,
    reducer: Heap<Value>,
    controller: Option<Heap<AbortControllerImpl>>,
    #[no_trace]
    kind: Consume,
    #[no_trace]
    has_initial: bool,
    #[no_trace]
    got_any: bool,
}

#[core_runtime::jsmethods]
impl ConsumeState {}

/// A new [`ConsumeState`] for `kind`, which settles `promise`.
fn new_consume<'s>(
    scope: &'s Scope<'_>,
    kind: Consume,
    promise: js::Promise<'_>,
    options: HandleValue<'_>,
    result: HandleValue<'_>,
) -> Result<ConsumeState<'s>, ExnThrown> {
    js::class::create_instance_with::<ConsumeStateImpl>(scope, |_| ConsumeStateImpl {
        promise: Some(Heap::from(promise)),
        over: None,
        options: Heap::from(options.get()),
        result: Heap::from(result.get()),
        values: Vec::new(),
        reducer: Heap::default(),
        controller: None,
        kind,
        has_initial: false,
        got_any: false,
    })
}

/// The [`ConsumeState`] that is `owner`.
fn consume_state(owner: Object<'_>) -> ConsumeState<'_> {
    owner
        .cast::<ConsumeState>()
        .expect("the owner is a ConsumeState")
}

/// The loop of `state`.
fn consume_loop_of<'s>(
    scope: &'s Scope<'_>,
    state: ConsumeState<'_>,
) -> Result<LoopState<'s>, ExnThrown> {
    state.data().over.get(scope).ok_or(ExnThrown)
}

/// Start the loop of a promise-returning operator over `iterable`.
fn consume_loop(
    scope: &Scope<'_>,
    state: ConsumeState<'_>,
    iterable: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let over = loop_open(scope, iterable, *state, consume_value, consume_end)?;
    state.data_mut().over = Some(Heap::from(over));
    loop_next(scope, over)
}

/// Settle the operator's promise.
fn consume_settle(
    scope: &Scope<'_>,
    state: ConsumeState<'_>,
    reject: bool,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let Some(promise) = state.data().promise.get(scope) else {
        return Ok(());
    };
    if !promise.is_pending() {
        return Ok(());
    }
    if reject {
        promise.reject(scope, value)
    } else {
        promise.resolve(scope, value)
    }
}

/// The body of the loop of a promise-returning operator.
fn consume_value(
    scope: &Scope<'_>,
    owner: Object<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = consume_state(owner);
    let over = consume_loop_of(scope, state)?;
    let kind = state.data().kind;
    match kind {
        Consume::ToArray => {
            let step = (|| -> Result<bool, ExnThrown> {
                let options = state.data().options.get(scope);
                if signal_aborted(scope, options)? {
                    let reason = signal_reason(scope, options)?;
                    let error = crate::errors::abort_error(scope, Some(reason))?;
                    loop_close(scope, over, Some(error))?;
                    return Ok(false);
                }
                state.data_mut().values.push(Heap::from(value.get()));
                Ok(true)
            })();
            match step {
                Ok(true) => loop_next(scope, over),
                Ok(false) => Ok(()),
                Err(_) => {
                    let error = js::exception::take_pending_or_undefined(scope);
                    loop_close(scope, over, Some(error))
                }
            }
        }
        Consume::ForEach => loop_next(scope, over),
        Consume::Some => {
            state.data().result.set(js::value::from_bool(true));
            loop_close(scope, over, None)
        }
        Consume::Find => {
            state.data().result.set(value.get());
            loop_close(scope, over, None)
        }
        Consume::Reduce => reduce_value(scope, state, over, value),
    }
}

/// The end of the loop of a promise-returning operator: reject with the error, or fulfill with
/// the result.
fn consume_end(
    scope: &Scope<'_>,
    owner: Object<'_>,
    end: End,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = consume_state(owner);
    let kind = state.data().kind;
    if kind == Consume::Reduce {
        reduce_finally(scope, state)?;
    }
    if end == End::Throw {
        return consume_settle(scope, state, true, error);
    }
    match kind {
        Consume::ToArray => {
            let values: Vec<HandleValue<'_>> = state
                .data()
                .values
                .iter()
                .map(|value| value.get(scope))
                .collect();
            let array = js::Array::with_contents(scope, &values)?;
            consume_settle(scope, state, false, array.to_jsval_throwing(scope)?)
        }
        Consume::Reduce => {
            let (got_any, has_initial) = {
                let data = state.data();
                (data.got_any, data.has_initial)
            };
            if !got_any && !has_initial {
                let error = reduce_missing_initial_value().to_value(scope)?;
                return consume_settle(scope, state, true, error);
            }
            let result = state.data().result.get(scope);
            consume_settle(scope, state, false, result)
        }
        Consume::ForEach | Consume::Some | Consume::Find => {
            let result = state.data().result.get(scope);
            consume_settle(scope, state, false, result)
        }
    }
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
        let state = new_consume(
            scope,
            Consume::ToArray,
            promise,
            options,
            HandleValue::undefined(),
        )?;
        consume_loop(scope, state, stream)?;
        Ok(())
    })
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
        let state = new_consume(
            scope,
            Consume::ForEach,
            promise,
            HandleValue::undefined(),
            HandleValue::undefined(),
        )?;
        consume_loop(scope, state, values.to_jsval_throwing(scope)?)?;
        Ok(())
    })
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
        let (kind, result) = if find {
            (Consume::Find, HandleValue::undefined())
        } else {
            (Consume::Some, scope.root_value(js::value::from_bool(false)))
        };
        let state = new_consume(scope, kind, promise, HandleValue::undefined(), result)?;
        consume_loop(scope, state, values.to_jsval_throwing(scope)?)?;
        Ok(())
    })
}

/// `async (...args) => !(await fn(...args))`, whose payload is `fn`.
fn every_fn(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let call_args: Vec<HandleValue<'_>> = (0..args.len()).map(|index| args.get(index)).collect();
    let result = Function::call(scope, HandleValue::undefined(), payload, &call_args)
        .and_then(|result| js::Promise::call_original_resolve(scope, result));
    let promise = match result {
        Ok(promise) => promise,
        Err(_) => {
            let error = js::exception::take_pending_or_undefined(scope);
            return js::Promise::new_rejected_with_error(scope, error)?
                .to_jsval_raw_throwing(scope);
        }
    };
    promise
        .then(scope, Some(negate_function(scope)?), None)?
        .to_jsval_raw_throwing(scope)
}

/// `!value`.
fn negate(
    _scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::from_bool(!js::conversion::to_boolean(
        args.get(0),
    )))
}

/// The shared function of [`negate`].
fn negate_function<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let function =
        js::class::get_or_init_shared_function(scope, negate as *const () as usize, |scope| {
            Function::new_callback(scope, c"", 1, negate, ())
        })?;
    Ok(*function)
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
        let error = js::exception::take_pending_or_undefined(scope);
        return js::Promise::new_rejected_with_error(scope, error)?.to_jsval_raw_throwing(scope);
    }
    let wrapped = Function::new_callback(scope, c"", 0, every_fn, function)?;
    let some = scope.root_value(some_or_find(
        scope,
        stream,
        (*wrapped).to_jsval_throwing(scope)?,
        options,
        false,
    )?);
    let some = js::Promise::call_original_resolve(scope, some)?;
    some.then(scope, Some(negate_function(scope)?), None)?
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
    promise_operator(scope, |promise| {
        validate_function(scope, reducer, "reducer")?;
        let options = scope.root_value(options.get());
        let options = validate_options(scope, options)?;
        let state = new_consume(
            scope,
            Consume::Reduce,
            promise,
            options,
            initial.unwrap_or(HandleValue::undefined()),
        )?;
        {
            let mut data = state.data_mut();
            data.reducer.set(reducer.get());
            data.has_initial = initial.is_some();
        }
        if signal_aborted(scope, options)? {
            let reason = signal_reason(scope, options)?;
            let error = crate::errors::abort_error(scope, Some(reason))?;
            state.data().result.set(error.get());
            let stream_object = Object::from_value_coerce(scope, stream)?;
            let noop = crate::support::noop_function(scope)?;
            crate::events::listen(scope, stream_object, c"once", c"error", noop)?;
            let destroyed =
                Function::call_by_name(scope, stream_object.handle(), c"destroy", &[error])?;
            let finished = super::eos::finished_promise(scope, destroyed, None)?;
            let rejected = reaction(scope, reduce_aborted, state)?;
            let failed = reaction(scope, reduce_aborted_failed, state)?;
            finished.add_reactions(scope, Some(rejected), Some(failed))?;
            return Ok(());
        }
        let controller = AbortController::new(scope)?;
        state.data_mut().controller = Some(Heap::from(controller));
        if let Some(signal) = signal_of(scope, options)? {
            let listener = Function::new_callback(scope, c"", 0, abort_controller, controller)?;
            let listen_options = Object::new_plain(scope)?;
            listen_options.set_property(scope, c"once", true)?;
            let event = js::class::get_or_init_atom(scope, c"abort")?;
            Function::call_by_name(
                scope,
                signal.handle(),
                c"addEventListener",
                &[
                    event,
                    (*listener).to_jsval_throwing(scope)?,
                    listen_options.to_jsval_throwing(scope)?,
                ],
            )?;
        }
        if consume_loop(scope, state, stream).is_err() {
            let error = js::exception::take_pending_or_undefined(scope);
            reduce_finally(scope, state)?;
            consume_settle(scope, state, true, error)?;
        }
        Ok(())
    })
}

/// `() => ac.abort()`, whose payload is the `AbortController`.
fn abort_controller(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let controller = cast_payload::<AbortController>(scope, payload);
    controller.js_abort(scope, HandleValue::undefined())?;
    Ok(js::value::undefined())
}

/// The stream of a `reduce` whose signal had aborted finished: reject with the `AbortError` in
/// the result.
fn reduce_aborted(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConsumeState>(scope, payload);
    let error = state.data().result.get(scope);
    consume_settle(scope, state, true, error)?;
    Ok(js::value::undefined())
}

/// The `finished` of a `reduce` whose signal had aborted rejected: reject with its reason.
fn reduce_aborted_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConsumeState>(scope, payload);
    consume_settle(scope, state, true, args.get(0))?;
    Ok(js::value::undefined())
}

/// The body of `reduce`'s loop.
fn reduce_value(
    scope: &Scope<'_>,
    state: ConsumeState<'_>,
    over: LoopState<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let step = (|| -> Result<(), ExnThrown> {
        state.data_mut().got_any = true;
        let options = state.data().options.get(scope);
        if signal_aborted(scope, options)? {
            let error = crate::errors::abort_error(scope, None)?;
            return loop_close(scope, over, Some(error));
        }
        if !state.data().has_initial {
            state.data().result.set(value.get());
            state.data_mut().has_initial = true;
            return loop_next(scope, over);
        }
        let (reducer, accumulator, controller) = {
            let data = state.data();
            (
                data.reducer.get(scope),
                data.result.get(scope),
                data.controller.get(scope),
            )
        };
        let reducer_options = Object::new_plain(scope)?;
        if let Some(controller) = controller {
            reducer_options.set_property(scope, c"signal", *controller.signal(scope))?;
        }
        let result = Function::call(
            scope,
            HandleValue::undefined(),
            reducer,
            &[
                accumulator,
                value,
                reducer_options.to_jsval_throwing(scope)?,
            ],
        )?;
        let fulfilled = reaction(scope, reduce_reduced, state)?;
        let rejected = reaction(scope, reduce_failed, state)?;
        await_value(scope, result, fulfilled, rejected)
    })();
    if step.is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        loop_close(scope, over, Some(error))?;
    }
    Ok(())
}

/// The reducer's result settled with the new accumulator.
fn reduce_reduced(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConsumeState>(scope, payload);
    state.data().result.set(args.get(0).get());
    loop_next(scope, consume_loop_of(scope, state)?)?;
    Ok(js::value::undefined())
}

fn reduce_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConsumeState>(scope, payload);
    loop_close(scope, consume_loop_of(scope, state)?, Some(args.get(0)))?;
    Ok(js::value::undefined())
}

/// The `finally` block of `reduce`: abort the signal the reducer gets.
fn reduce_finally(scope: &Scope<'_>, state: ConsumeState<'_>) -> Result<(), ExnThrown> {
    let controller = state.data().controller.get(scope);
    if let Some(controller) = controller {
        controller.js_abort(scope, HandleValue::undefined())?;
    }
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
    let generator = map_generator(scope, this, args.get(0), args.get(1));
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

/// Register the operators' internal classes for `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) {
    ConsumeState::add_to_global(scope, global);
    FilterFn::add_to_global(scope, global);
    FlatState::add_to_global(scope, global);
    Generator::add_to_global(scope, global);
    LoopState::add_to_global(scope, global);
    MapState::add_to_global(scope, global);
    SkipState::add_to_global(scope, global);
    AsyncIteratorRecord::add_to_global(scope, global);
}
