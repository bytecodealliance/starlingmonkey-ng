// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `events.once` and `events.on`, which wait for events on an `EventEmitter` or `EventTarget`.
//! See <https://nodejs.org/api/events.html#eventsonceemitter-name-options>.
//!
//! The listeners, iterators and promises share their state through a [`OnceWait`] or an
//! [`OnIteration`], which each native function holds as its payload.

use std::collections::VecDeque;

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{
    abort_error, validate_abort_signal, validate_plain_object, NodeError, Thrown, MAX_SAFE_INTEGER,
};

/// Add `listener` for `name` to `target`, with its `once` or `on` method if it has `on`, and with
/// `addEventListener` otherwise. This ports Node's `eventTargetAgnosticAddListener`.
fn add_listener(
    scope: &Scope<'_>,
    target: Object<'_>,
    name: HandleValue<'_>,
    listener: HandleValue<'_>,
    once: bool,
) -> Result<(), Thrown> {
    if crate::support::has_method(scope, target, c"on")? {
        let add = if once { c"once" } else { c"on" };
        Function::call_by_name(scope, target.handle(), add, &[name, listener])?;
        return Ok(());
    }
    if crate::support::has_method(scope, target, c"addEventListener")? {
        let options = Object::new_plain(scope)?;
        options.set_property(scope, c"once", once)?;
        let options = options.to_jsval_throwing(scope)?;
        Function::call_by_name(
            scope,
            target.handle(),
            c"addEventListener",
            &[name, listener, options],
        )?;
        return Ok(());
    }
    let target = target.to_jsval_throwing(scope)?;
    Err(not_an_emitter(scope, target))
}

/// Node's `ERR_INVALID_ARG_TYPE` for an `emitter` argument that is neither an `EventEmitter` nor
/// an `EventTarget`.
fn not_an_emitter(scope: &Scope<'_>, emitter: HandleValue<'_>) -> Thrown {
    NodeError::invalid_arg_type(scope, "emitter", "an instance of EventEmitter", emitter).into()
}

/// `emitter` as an object to add listeners to. `null` and `undefined` throw the `TypeError` of a
/// property read, and other primitives `ERR_INVALID_ARG_TYPE`, as Node's
/// `eventTargetAgnosticAddListener` reads `emitter.on` first.
fn emitter_object<'s>(
    scope: &'s Scope<'_>,
    emitter: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    if emitter.is_null_or_undefined() {
        // `from_value_coerce` throws a `TypeError` for `null` and `undefined`.
        return Ok(Object::from_value_coerce(scope, emitter)?);
    }
    Object::from_value(scope, *emitter).map_err(|_| not_an_emitter(scope, emitter))
}

/// Remove `listener` for `name` from `target`, with `removeListener` or `removeEventListener`.
fn remove_listener(
    scope: &Scope<'_>,
    target: Object<'_>,
    name: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let remove = if crate::support::has_method(scope, target, c"removeListener")? {
        c"removeListener"
    } else if crate::support::has_method(scope, target, c"removeEventListener")? {
        c"removeEventListener"
    } else {
        return Ok(());
    };
    Function::call_by_name(scope, target.handle(), remove, &[name, listener])?;
    Ok(())
}

/// The `AbortError` for an abort of `signal`, with its reason as the cause.
fn abort_error_for<'s>(
    scope: &'s Scope<'_>,
    signal: Object<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let reason = signal.get_property(scope, c"reason")?;
    abort_error(scope, Some(reason))
}

/// `object`, or `undefined` if it is `None`.
fn value_or_undefined<'s>(
    scope: &'s Scope<'_>,
    object: Option<Object<'s>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    match object {
        Some(object) => object.to_jsval_throwing(scope),
        None => Ok(scope.root_value(js::value::undefined())),
    }
}

/// `signal`, or a `TypeError` if it is `None`.
fn required_signal<'s>(
    scope: &Scope<'_>,
    signal: Option<Object<'s>>,
) -> Result<Object<'s>, ExnThrown> {
    signal.ok_or_else(|| js::error::TypeError("no signal".into()).throw(scope))
}

/// The payload of the listeners `once` adds: the promise `once` returned, the emitter and the
/// event name, the listeners, and the signal, if `once` got one.
#[core_runtime::jsclass(hidden)]
pub(crate) struct OnceWait {
    promise: Heap<js::promise::Promise>,
    emitter: Heap<js::object::Object>,
    name: Heap<Value>,
    resolver: Option<Heap<js::object::Object>>,
    error_listener: Option<Heap<js::object::Object>>,
    abort_listener: Option<Heap<js::object::Object>>,
    signal: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl OnceWait {}

/// Remove the listeners `once` added for `wait`.
fn once_cleanup(scope: &Scope<'_>, wait: OnceWait<'_>) -> Result<(), ExnThrown> {
    let (emitter, name, resolver, error_listener, abort_listener, signal) = {
        let data = wait.data();
        (
            data.emitter.get(scope),
            data.name.get(scope),
            data.resolver.get(scope),
            data.error_listener.get(scope),
            data.abort_listener.get(scope),
            data.signal.get(scope),
        )
    };
    remove_listener(scope, emitter, name, value_or_undefined(scope, resolver)?)?;
    let error = js::class::get_or_init_atom(scope, c"error")?;
    remove_listener(
        scope,
        emitter,
        error,
        value_or_undefined(scope, error_listener)?,
    )?;
    if let Some(signal) = signal {
        let abort = js::class::get_or_init_atom(scope, c"abort")?;
        remove_listener(
            scope,
            signal,
            abort,
            value_or_undefined(scope, abort_listener)?,
        )?;
    }
    Ok(())
}

/// The promise `once` returned for `wait`.
fn once_promise<'s>(scope: &'s Scope<'_>, wait: OnceWait<'_>) -> js::Promise<'s> {
    wait.data().promise.get(scope)
}

/// The listener `once` adds for the event, whose payload is a [`OnceWait`]: resolve with the
/// event's arguments.
fn once_resolver(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let wait = cast_payload::<OnceWait>(scope, payload);
    once_cleanup(scope, wait)?;
    let values: Vec<HandleValue<'_>> = args.rest(0);
    let values = js::Array::with_contents(scope, &values)?;
    once_promise(scope, wait).resolve(scope, *values)?;
    Ok(js::value::undefined())
}

/// The listener `once` adds for `error`, whose payload is a [`OnceWait`]: reject with the error.
fn once_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let wait = cast_payload::<OnceWait>(scope, payload);
    once_cleanup(scope, wait)?;
    once_promise(scope, wait).reject(scope, args.get(0))?;
    Ok(js::value::undefined())
}

/// The listener `once` adds for the signal's `abort`, whose payload is a [`OnceWait`]: reject
/// with an `AbortError`.
fn once_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let wait = cast_payload::<OnceWait>(scope, payload);
    once_cleanup(scope, wait)?;
    let signal = wait.data().signal.get(scope);
    let signal = required_signal(scope, signal)?;
    let error = abort_error_for(scope, signal)?;
    once_promise(scope, wait).reject(scope, error)?;
    Ok(js::value::undefined())
}

/// `options.signal`, validated, or `None` if there is none.
fn read_signal<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Option<Object<'s>>, Thrown> {
    let Some(options) = options.filter(|options| !options.is_undefined()) else {
        return Ok(None);
    };
    let options = validate_plain_object(scope, options, "options", false)?;
    let signal = options.get_property(scope, c"signal")?;
    let signal = scope.root_value(signal.get());
    validate_abort_signal(scope, signal, "options.signal")?;
    let signal = Object::from_value(scope, signal).ok();
    if let Some(signal) = signal {
        if crate::support::is_true(signal.get_property(scope, c"aborted")?) {
            js::exception::set_pending(
                scope,
                abort_error_for(scope, signal)?,
                js::native::ExceptionStackBehavior::Capture,
            );
            return Err(Thrown::Pending);
        }
    }
    Ok(signal)
}

/// `events.once(emitter, name, options)`. Errors in the arguments reject the returned promise.
pub fn once<'s>(
    scope: &'s Scope<'_>,
    emitter: HandleValue<'_>,
    name: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    let started = (|| -> Result<(), Thrown> {
        let signal = read_signal(scope, options)?;
        let emitter = emitter_object(scope, emitter)?;
        let wait = js::class::create_instance_with::<OnceWaitImpl>(scope, |_| OnceWaitImpl {
            promise: Heap::from(promise),
            emitter: Heap::from(emitter),
            name: Heap::from(name.get()),
            resolver: None,
            error_listener: None,
            abort_listener: None,
            signal: None,
        })?;
        let resolver = Function::new_callback(scope, c"", 0, once_resolver, wait)?;
        let error_listener = Function::new_callback(scope, c"", 1, once_error, wait)?;
        let abort_listener = Function::new_callback(scope, c"", 0, once_abort, wait)?;
        {
            let mut data = wait.data_mut();
            data.resolver.set(*resolver);
            data.error_listener.set(*error_listener);
            data.abort_listener.set(*abort_listener);
        }
        add_listener(
            scope,
            emitter,
            name,
            (*resolver).to_jsval_throwing(scope)?,
            true,
        )?;
        let is_error = crate::support::is_str(scope, name, c"error")?;
        if !is_error && crate::support::has_method(scope, emitter, c"once")? {
            let error = js::class::get_or_init_atom(scope, c"error")?;
            add_listener(
                scope,
                emitter,
                error,
                (*error_listener).to_jsval_throwing(scope)?,
                true,
            )?;
        }
        if let Some(signal) = signal {
            wait.data_mut().signal.set(signal);
            let abort = js::class::get_or_init_atom(scope, c"abort")?;
            add_listener(
                scope,
                signal,
                abort,
                (*abort_listener).to_jsval_throwing(scope)?,
                true,
            )?;
        }
        Ok(())
    })();
    if let Err(error) = started {
        error.throw(scope);
        promise.reject_with_pending(scope)?;
    }
    Ok(promise)
}

/// A listener [`on_add`] added, for [`on_close`] to remove.
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct AddedListener {
    target: Heap<js::object::Object>,
    name: Heap<Value>,
    listener: Heap<js::object::Object>,
}

/// The payload of the iterator methods and the listeners of `events.on`: the emitter, the queued
/// events, pending `next()` promises and added listeners, and the iteration's state.
#[core_runtime::jsclass(hidden)]
pub(crate) struct OnIteration {
    emitter: Heap<js::object::Object>,
    /// The values of the events no `next()` took yet: arrays of their arguments, or their first
    /// arguments with `first_event_param`.
    queue: VecDeque<Heap<Value>>,
    /// The promises of the pending `next()` calls.
    pending: VecDeque<Heap<js::promise::Promise>>,
    added: Vec<AddedListener>,
    signal: Option<Heap<js::object::Object>>,
    /// The error the next `next()` rejects with, if `has_error` is set.
    error: Heap<Value>,
    #[no_trace]
    has_error: bool,
    #[no_trace]
    high_water_mark: f64,
    #[no_trace]
    low_water_mark: f64,
    #[no_trace]
    paused: bool,
    #[no_trace]
    finished: bool,
    /// Whether the iterator yields the first argument of each event instead of an array of all
    /// of them, as Node.js's `kFirstEventParam` option selects.
    #[no_trace]
    first_event_param: bool,
}

#[core_runtime::jsmethods]
impl OnIteration {}

/// Remove `on`'s listeners for `state`, mark its iterator finished and not paused, and resolve the
/// pending `next()` promises as done. Returns a promise resolved as done. This ports Node's
/// `closeHandler`.
fn on_close<'s>(
    scope: &'s Scope<'_>,
    state: OnIteration<'_>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let added: Vec<_> = {
        let mut data = state.data_mut();
        data.finished = true;
        data.paused = false;
        let added = data
            .added
            .iter()
            .map(|entry| {
                (
                    entry.target.get(scope),
                    entry.name.get(scope),
                    entry.listener.get(scope),
                )
            })
            .collect();
        data.added.clear();
        added
    };
    for (target, name, listener) in added {
        remove_listener(scope, target, name, listener.to_jsval_throwing(scope)?)?;
    }
    let undefined = scope.root_value(js::value::undefined());
    let pending: Vec<_> = {
        let mut data = state.data_mut();
        let pending = data
            .pending
            .iter()
            .map(|promise| promise.get(scope))
            .collect();
        data.pending.clear();
        pending
    };
    for pending in pending {
        pending.resolve(
            scope,
            js::iteration::create_iter_result(scope, undefined, true)?,
        )?;
    }
    js::Promise::new_resolved_with_value(
        scope,
        js::iteration::create_iter_result(scope, undefined, true)?,
    )
}

/// Remove and return the oldest pending `next()` promise of `state`, if there is one.
fn shift_pending<'s>(scope: &'s Scope<'_>, state: OnIteration<'_>) -> Option<js::Promise<'s>> {
    let mut data = state.data_mut();
    let pending = data.pending.front()?.get(scope);
    drop(data.pending.pop_front());
    Some(pending)
}

/// The listener `on` adds for the event, whose payload is an [`OnIteration`]: resolve the oldest
/// pending `next()` with the event's value, or queue it, pausing the emitter once more than
/// `highWaterMark` are queued.
fn on_event(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<OnIteration>(scope, payload);
    let first_event_param = state.data().first_event_param;
    let value = if first_event_param {
        args.get(0)
    } else {
        let values: Vec<HandleValue<'_>> = args.rest(0);
        js::Array::with_contents(scope, &values)?.to_jsval_throwing(scope)?
    };
    if let Some(pending) = shift_pending(scope, state) {
        pending.resolve(
            scope,
            js::iteration::create_iter_result(scope, value, false)?,
        )?;
        return Ok(js::value::undefined());
    }
    let (queued, paused, high_water_mark, emitter) = {
        let mut data = state.data_mut();
        data.queue.push_back(Heap::from(value.get()));
        (
            data.queue.len() as f64,
            data.paused,
            data.high_water_mark,
            data.emitter.get(scope),
        )
    };
    if !paused && queued > high_water_mark {
        if crate::support::has_method(scope, emitter, c"pause")? {
            Function::call_by_name(scope, emitter.handle(), c"pause", js::function::EmptyArgs)?;
        }
        state.data_mut().paused = true;
    }
    Ok(js::value::undefined())
}

/// Reject the oldest pending `next()` with `error`, or keep it for the next `next()`, then close
/// the iterator. This ports Node's `errorHandler`.
fn on_fail(
    scope: &Scope<'_>,
    state: OnIteration<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    match shift_pending(scope, state) {
        Some(pending) => {
            pending.reject(scope, error)?;
        }
        None => {
            let mut data = state.data_mut();
            data.error.set(error.get());
            data.has_error = true;
        }
    }
    on_close(scope, state)?;
    Ok(())
}

/// The listener `on` adds for `error`, whose payload is an [`OnIteration`].
fn on_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    on_fail(
        scope,
        cast_payload::<OnIteration>(scope, payload),
        args.get(0),
    )?;
    Ok(js::value::undefined())
}

/// The listener `on` adds for the `close` option's events, whose payload is an [`OnIteration`].
fn on_close_event(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    on_close(scope, cast_payload::<OnIteration>(scope, payload))?;
    Ok(js::value::undefined())
}

/// The listener `on` adds for the signal's `abort`, whose payload is an [`OnIteration`].
fn on_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<OnIteration>(scope, payload);
    let signal = state.data().signal.get(scope);
    let signal = required_signal(scope, signal)?;
    let error = abort_error_for(scope, signal)?;
    on_fail(scope, state, error)?;
    Ok(js::value::undefined())
}

/// The iterator's `next()`, whose payload is an [`OnIteration`]: the oldest queued event, else
/// the kept error, else done once the iterator is finished, else a promise for the next event.
fn on_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<OnIteration>(scope, payload);
    let next = {
        let mut data = state.data_mut();
        data.queue.pop_front().map(|values| {
            (
                values.get(scope),
                data.queue.len() as f64,
                data.paused,
                data.low_water_mark,
                data.emitter.get(scope),
            )
        })
    };
    if let Some((values, queued, paused, low_water_mark, emitter)) = next {
        if paused && queued < low_water_mark {
            if crate::support::has_method(scope, emitter, c"resume")? {
                Function::call_by_name(
                    scope,
                    emitter.handle(),
                    c"resume",
                    js::function::EmptyArgs,
                )?;
            }
            state.data_mut().paused = false;
        }
        let result = js::iteration::create_iter_result(scope, values, false)?;
        return js::Promise::new_resolved_with_value(scope, result)?.to_jsval_raw_throwing(scope);
    }
    let (has_error, error, finished) = {
        let data = state.data();
        (data.has_error, data.error.get(scope), data.finished)
    };
    if has_error {
        {
            let mut data = state.data_mut();
            data.has_error = false;
            data.error.set(js::value::undefined());
        }
        return js::Promise::new_rejected_with_error(scope, error)?.to_jsval_raw_throwing(scope);
    }
    if finished {
        return on_close(scope, state)?.to_jsval_raw_throwing(scope);
    }
    let pending = js::Promise::new_pending(scope)?;
    state.data_mut().pending.push_back(Heap::from(pending));
    pending.to_jsval_raw_throwing(scope)
}

/// The iterator's `return()`, whose payload is an [`OnIteration`].
fn on_return(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    on_close(scope, cast_payload::<OnIteration>(scope, payload))?.to_jsval_raw_throwing(scope)
}

/// The iterator's `throw(error)`, whose payload is an [`OnIteration`]: fail the iterator with
/// `error`, which must be an `Error`.
fn on_throw(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let error = args.get(0);
    if !js::exception::is_error_object(scope, error) {
        return Err(NodeError::invalid_arg_type(
            scope,
            "EventEmitter.AsyncIterator",
            "an instance of Error",
            error,
        )
        .throw(scope));
    }
    on_fail(scope, cast_payload::<OnIteration>(scope, payload), error)?;
    Ok(js::value::undefined())
}

/// Add `listener` for `name` to `target` for `on`'s iterator, and record it in `state` for
/// [`on_close`] to remove.
fn on_add(
    scope: &Scope<'_>,
    state: OnIteration<'_>,
    target: Object<'_>,
    name: HandleValue<'_>,
    listener: Function<'_>,
) -> Result<(), Thrown> {
    add_listener(
        scope,
        target,
        name,
        (*listener).to_jsval_throwing(scope)?,
        false,
    )?;
    state.data_mut().added.push(AddedListener {
        target: Heap::from(target),
        name: Heap::from(name.get()),
        listener: Heap::from(*listener),
    });
    Ok(())
}

/// The water mark option under the first of `names` that isn't `undefined` or `null`, or
/// `default`, validated as an integer of at least 1 and reported as `options.<names[0]>`.
fn water_mark_option(
    scope: &Scope<'_>,
    options: Option<Object<'_>>,
    names: [&'static std::ffi::CStr; 2],
    default: f64,
) -> Result<f64, Thrown> {
    let Some(options) = options else {
        return Ok(default);
    };
    for name in names {
        let value = options.get_property(scope, name)?;
        if !value.is_null() && !value.is_undefined() {
            let label = format!("options.{}", names[0].to_string_lossy());
            return crate::errors::validate_integer(scope, value, &label, 1.0, MAX_SAFE_INTEGER);
        }
    }
    Ok(default)
}

/// `events.on(emitter, name, options)`: an async iterator over the arguments of each `name` event.
pub fn on<'s>(
    scope: &'s Scope<'_>,
    emitter: HandleValue<'_>,
    name: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let signal = read_signal(scope, options)?;
    let options = options
        .filter(|options| !options.is_undefined())
        .and_then(|options| Object::from_value(scope, scope.root_value(options.get())).ok());
    let high_water_mark = water_mark_option(
        scope,
        options,
        [c"highWaterMark", c"highWatermark"],
        MAX_SAFE_INTEGER,
    )?;
    let low_water_mark =
        water_mark_option(scope, options, [c"lowWaterMark", c"lowWatermark"], 1.0)?;
    let emitter = emitter_object(scope, emitter)?;
    let settings = OnSettings {
        high_water_mark,
        low_water_mark,
        signal,
        first_event_param: false,
    };
    on_iterator(scope, emitter, name, settings, |scope| {
        let Some(options) = options else {
            return Ok(Vec::new());
        };
        let close = options.get_property(scope, c"close")?;
        if !close.is_object() {
            return Ok(Vec::new());
        }
        Ok(crate::support::list_from_array_like(scope, close)?)
    })
}

/// The validated options of an `events.on` iterator.
pub(crate) struct OnSettings<'a> {
    pub(crate) high_water_mark: f64,
    pub(crate) low_water_mark: f64,
    pub(crate) signal: Option<Object<'a>>,
    /// Whether the iterator yields the first argument of each event, as Node.js's
    /// `kFirstEventParam` option selects, instead of an array of all of them.
    pub(crate) first_event_param: bool,
}

/// The iterator of `events.on(emitter, name, options)` with the options `settings`, which ends at
/// each event that `close_events` returns. `close_events` is called after the listeners for `name`
/// and `error` are added, when Node.js reads `options.close`.
pub(crate) fn on_iterator<'s>(
    scope: &'s Scope<'_>,
    emitter: Object<'_>,
    name: HandleValue<'_>,
    settings: OnSettings<'_>,
    close_events: impl FnOnce(&'s Scope<'_>) -> Result<Vec<HandleValue<'s>>, Thrown>,
) -> Result<Object<'s>, Thrown> {
    let OnSettings {
        high_water_mark,
        low_water_mark,
        signal,
        first_event_param,
    } = settings;
    let state = js::class::create_instance_with::<OnIterationImpl>(scope, |_| OnIterationImpl {
        emitter: Heap::from(emitter),
        queue: VecDeque::new(),
        pending: VecDeque::new(),
        added: Vec::new(),
        signal: signal.map(Heap::from),
        error: Heap::from(js::value::undefined()),
        has_error: false,
        high_water_mark,
        low_water_mark,
        paused: false,
        finished: false,
        first_event_param,
    })?;

    let proto = js::class::get_async_iterator_prototype(scope)?;
    let iterator = Object::new_plain_with_proto(scope, proto)?;
    for (key, body) in [
        (c"next", on_next as js::function::Callback),
        (c"return", on_return),
        (c"throw", on_throw),
    ] {
        let function = Function::new_callback(scope, key, 0, body, state)?;
        iterator.set_property(scope, key, *function)?;
    }

    on_add(
        scope,
        state,
        emitter,
        name,
        Function::new_callback(scope, c"", 0, on_event, state)?,
    )?;
    let is_error = crate::support::is_str(scope, name, c"error")?;
    if !is_error && crate::support::has_method(scope, emitter, c"on")? {
        let error = js::class::get_or_init_atom(scope, c"error")?;
        on_add(
            scope,
            state,
            emitter,
            error,
            Function::new_callback(scope, c"", 1, on_error, state)?,
        )?;
    }
    for event in close_events(scope)? {
        on_add(
            scope,
            state,
            emitter,
            event,
            Function::new_callback(scope, c"", 0, on_close_event, state)?,
        )?;
    }
    if let Some(signal) = signal {
        let abort = js::class::get_or_init_atom(scope, c"abort")?;
        on_add(
            scope,
            state,
            signal,
            abort,
            Function::new_callback(scope, c"", 0, on_abort, state)?,
        )?;
    }
    Ok(iterator)
}
