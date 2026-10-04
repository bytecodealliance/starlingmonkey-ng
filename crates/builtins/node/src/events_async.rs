// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `events.once` and `events.on`, which wait for events on an `EventEmitter` or `EventTarget`.
//! See <https://nodejs.org/api/events.html#eventsonceemitter-name-options>.
//!
//! The listeners, iterators and promises share their state through a plain object each native
//! function holds as its payload.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{abort_error, validate_abort_signal, validate_object, NodeError, Thrown};

/// Whether `value` is `true`.
fn flag(value: HandleValue<'_>) -> bool {
    value.is_boolean() && value.to_boolean()
}

/// The value of `object[name]` if it is a function.
fn method<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let value = object.get_property(scope, name)?;
    let value = scope.root_value(value.get());
    let callable = Object::from_value(scope, value).is_ok_and(|function| function.is_callable());
    Ok(callable.then_some(value))
}

/// Add `listener` for `name` to `target`, with its `once` or `on` method if it has `on`, and with
/// `addEventListener` otherwise. This ports Node's `eventTargetAgnosticAddListener`.
fn add_listener(
    scope: &Scope<'_>,
    target: Object<'_>,
    name: HandleValue<'_>,
    listener: HandleValue<'_>,
    once: bool,
) -> Result<(), Thrown> {
    if method(scope, target, c"on")?.is_some() {
        let add = if once { c"once" } else { c"on" };
        Function::call_by_name(scope, target.handle(), add, &[name, listener])?;
        return Ok(());
    }
    if method(scope, target, c"addEventListener")?.is_some() {
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
    Err(NodeError::invalid_arg_type(
        scope,
        "emitter",
        "an instance of EventEmitter or EventTarget",
        target,
    )
    .into())
}

/// Remove `listener` for `name` from `target`, with `removeListener` or `removeEventListener`.
fn remove_listener(
    scope: &Scope<'_>,
    target: Object<'_>,
    name: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let remove = if method(scope, target, c"removeListener")?.is_some() {
        c"removeListener"
    } else if method(scope, target, c"removeEventListener")?.is_some() {
        c"removeEventListener"
    } else {
        return Ok(());
    };
    Function::call_by_name(scope, target.handle(), remove, &[name, listener])?;
    Ok(())
}

/// The string `name` as a JS value.
fn string<'s>(scope: &'s Scope<'_>, name: &str) -> Result<HandleValue<'s>, ExnThrown> {
    js::JSString::from_str(scope, name)?.to_jsval_throwing(scope)
}

/// The `AbortError` for an abort of `signal`, with its reason as the cause.
fn abort_error_for<'s>(
    scope: &'s Scope<'_>,
    signal: Object<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let reason = signal.get_property(scope, c"reason")?;
    abort_error(scope, Some(reason))
}

/// The payload of a native function as an object.
fn state_of<'s>(scope: &'s Scope<'_>, payload: HandleValue<'_>) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, scope.root_value(payload.get())).map_err(|e| e.throw(scope))
}

/// `state[name]` as an object.
fn object_at<'s>(
    scope: &'s Scope<'_>,
    state: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Object<'s>, ExnThrown> {
    let value = state.get_property(scope, name)?;
    Object::from_value(scope, scope.root_value(value.get())).map_err(|e| e.throw(scope))
}

/// `state.signal` as an object, if `once` or `on` got one.
fn signal_of<'s>(scope: &'s Scope<'_>, state: Object<'_>) -> Result<Option<Object<'s>>, ExnThrown> {
    let signal = state.get_property(scope, c"signal")?;
    Ok(Object::from_value(scope, scope.root_value(signal.get())).ok())
}

/// Remove the listeners `once` added for its `state`.
fn once_cleanup(scope: &Scope<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    let emitter = object_at(scope, state, c"emitter")?;
    let name = state.get_property(scope, c"name")?;
    remove_listener(
        scope,
        emitter,
        name,
        state.get_property(scope, c"resolver")?,
    )?;
    let error = string(scope, "error")?;
    remove_listener(
        scope,
        emitter,
        error,
        state.get_property(scope, c"errorListener")?,
    )?;
    if let Some(signal) = signal_of(scope, state)? {
        let abort = string(scope, "abort")?;
        remove_listener(
            scope,
            signal,
            abort,
            state.get_property(scope, c"abortListener")?,
        )?;
    }
    Ok(())
}

/// The promise `once` returned for `state`.
fn once_promise<'s>(scope: &'s Scope<'_>, state: Object<'_>) -> Result<js::Promise<'s>, ExnThrown> {
    object_at(scope, state, c"promise")?
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))
}

/// The listener `once` adds for the event: resolve with the event's arguments.
fn once_resolver(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    once_cleanup(scope, state)?;
    let values: Vec<HandleValue<'_>> = (0..args.len()).map(|index| args.get(index)).collect();
    let values = js::Array::with_contents(scope, &values)?;
    once_promise(scope, state)?.resolve(scope, *values)?;
    Ok(js::value::undefined())
}

/// The listener `once` adds for `error`: reject with the error.
fn once_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    once_cleanup(scope, state)?;
    once_promise(scope, state)?.reject(scope, args.get(0))?;
    Ok(js::value::undefined())
}

/// The listener `once` adds for the signal's `abort`: reject with an `AbortError`.
fn once_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    once_cleanup(scope, state)?;
    let signal = object_at(scope, state, c"signal")?;
    let error = abort_error_for(scope, signal)?;
    once_promise(scope, state)?.reject(scope, error)?;
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
    let options = validate_object(scope, options, "options")?;
    let signal = options.get_property(scope, c"signal")?;
    let signal = scope.root_value(signal.get());
    validate_abort_signal(scope, signal, "options.signal")?;
    let signal = Object::from_value(scope, signal).ok();
    if let Some(signal) = signal {
        if flag(signal.get_property(scope, c"aborted")?) {
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
        let emitter = validate_object(scope, emitter, "emitter")?;
        let state = Object::new_plain(scope)?;
        state.set_property(scope, c"promise", *promise)?;
        state.set_property(scope, c"emitter", emitter)?;
        state.set_property(scope, c"name", name)?;
        let resolver = Function::new_callback(scope, c"", 0, once_resolver, state)?;
        let error_listener = Function::new_callback(scope, c"", 1, once_error, state)?;
        let abort_listener = Function::new_callback(scope, c"", 0, once_abort, state)?;
        state.set_property(scope, c"resolver", *resolver)?;
        state.set_property(scope, c"errorListener", *error_listener)?;
        state.set_property(scope, c"abortListener", *abort_listener)?;
        add_listener(
            scope,
            emitter,
            name,
            (*resolver).to_jsval_throwing(scope)?,
            true,
        )?;
        let is_error = name.is_string()
            && String::from_jsval(scope, name, ()).is_ok_and(|name| name == "error");
        if !is_error && method(scope, emitter, c"once")?.is_some() {
            let error = string(scope, "error")?;
            add_listener(
                scope,
                emitter,
                error,
                (*error_listener).to_jsval_throwing(scope)?,
                true,
            )?;
        }
        if let Some(signal) = signal {
            state.set_property(scope, c"signal", signal)?;
            let abort = string(scope, "abort")?;
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

/// Remove `on`'s listeners for `state`, mark its iterator finished and not paused, and resolve the
/// pending `next()` promises as done. Returns a promise resolved as done. This ports Node's
/// `closeHandler`.
fn on_close<'s>(scope: &'s Scope<'_>, state: Object<'_>) -> Result<js::Promise<'s>, ExnThrown> {
    let added = elements(scope, state.get_property(scope, c"added")?)?;
    for entry in added {
        let entry = Object::from_value(scope, entry).map_err(|e| e.throw(scope))?;
        let target =
            Object::from_value(scope, entry.get_element(scope, 0)?).map_err(|e| e.throw(scope))?;
        remove_listener(
            scope,
            target,
            entry.get_element(scope, 1)?,
            entry.get_element(scope, 2)?,
        )?;
    }
    state.set_property(
        scope,
        c"added",
        *js::Array::with_contents(scope, &[] as &[Value])?,
    )?;
    state.set_property(scope, c"finished", true)?;
    state.set_property(scope, c"paused", false)?;
    let undefined = scope.root_value(js::value::undefined());
    for pending in take_all(scope, state, c"pending")? {
        let pending = Object::from_value(scope, pending)
            .map_err(|e| e.throw(scope))?
            .cast::<js::Promise>()
            .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
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

/// The elements of `array`, an array object.
fn elements<'s>(
    scope: &'s Scope<'_>,
    array: HandleValue<'_>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let Ok(array) = Object::from_value(scope, scope.root_value(array.get())) else {
        return Ok(Vec::new());
    };
    let length = array.get_property(scope, c"length")?;
    let length = if length.is_number() {
        length.to_number() as u32
    } else {
        0
    };
    (0..length)
        .map(|index| array.get_element(scope, index))
        .collect()
}

/// The `length` of the array `array`, or 0 if it isn't a number.
fn length_of(scope: &Scope<'_>, array: Object<'_>) -> Result<u32, ExnThrown> {
    let length = array.get_property(scope, c"length")?;
    Ok(if length.is_number() {
        length.to_number() as u32
    } else {
        0
    })
}

/// Append `value` to the array `state[name]`, and return its new length.
fn push(
    scope: &Scope<'_>,
    state: Object<'_>,
    name: &'static std::ffi::CStr,
    value: HandleValue<'_>,
) -> Result<u32, ExnThrown> {
    let array = object_at(scope, state, name)?;
    Function::call_by_name(scope, array.handle(), c"push", &[value])?;
    length_of(scope, array)
}

/// Remove and return the first element of the array `state[name]`, if it has one.
fn shift<'s>(
    scope: &'s Scope<'_>,
    state: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let array = object_at(scope, state, name)?;
    if length_of(scope, array)? == 0 {
        return Ok(None);
    }
    let first = Function::call_by_name(scope, array.handle(), c"shift", js::function::EmptyArgs)?;
    Ok(Some(scope.root_value(first.get())))
}

/// Remove and return every element of the array `state[name]`.
fn take_all<'s>(
    scope: &'s Scope<'_>,
    state: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let values = elements(scope, state.get_property(scope, name)?)?;
    state.set_property_by_name(
        scope,
        name,
        *js::Array::with_contents(scope, &[] as &[Value])?,
    )?;
    Ok(values)
}

/// `state.highWaterMark` or `state.lowWaterMark`.
fn water_mark(
    scope: &Scope<'_>,
    state: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<f64, ExnThrown> {
    Ok(state.get_property(scope, name)?.to_number())
}

/// The listener `on` adds for the event: resolve the oldest pending `next()` with the event's
/// arguments, or queue them, pausing the emitter once more than `highWaterMark` are queued.
fn on_event(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let values: Vec<HandleValue<'_>> = (0..args.len()).map(|index| args.get(index)).collect();
    let values = js::Array::with_contents(scope, &values)?;
    if let Some(pending) = shift(scope, state, c"pending")? {
        let pending = Object::from_value(scope, pending)
            .map_err(|e| e.throw(scope))?
            .cast::<js::Promise>()
            .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
        pending.resolve(
            scope,
            js::iteration::create_iter_result(scope, *values, false)?,
        )?;
        return Ok(js::value::undefined());
    }
    let queued = f64::from(push(
        scope,
        state,
        c"queue",
        (*values).to_jsval_throwing(scope)?,
    )?);
    let paused = flag(state.get_property(scope, c"paused")?);
    if !paused && queued > water_mark(scope, state, c"highWaterMark")? {
        let emitter = object_at(scope, state, c"emitter")?;
        if method(scope, emitter, c"pause")?.is_some() {
            Function::call_by_name(scope, emitter.handle(), c"pause", js::function::EmptyArgs)?;
        }
        state.set_property(scope, c"paused", true)?;
    }
    Ok(js::value::undefined())
}

/// Reject the oldest pending `next()` with `error`, or keep it for the next `next()`, then close
/// the iterator. This ports Node's `errorHandler`.
fn on_fail(scope: &Scope<'_>, state: Object<'_>, error: HandleValue<'_>) -> Result<(), ExnThrown> {
    match shift(scope, state, c"pending")? {
        Some(pending) => {
            Object::from_value(scope, pending)
                .map_err(|e| e.throw(scope))?
                .cast::<js::Promise>()
                .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?
                .reject(scope, error)?;
        }
        None => {
            state.set_property(scope, c"error", error)?;
            state.set_property(scope, c"hasError", true)?;
        }
    }
    on_close(scope, state)?;
    Ok(())
}

/// The listener `on` adds for `error`.
fn on_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    on_fail(scope, state_of(scope, payload)?, args.get(0))?;
    Ok(js::value::undefined())
}

/// The listener `on` adds for the `close` option's events.
fn on_close_event(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    on_close(scope, state_of(scope, payload)?)?;
    Ok(js::value::undefined())
}

/// The listener `on` adds for the signal's `abort`.
fn on_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let signal = object_at(scope, state, c"signal")?;
    let error = abort_error_for(scope, signal)?;
    on_fail(scope, state, error)?;
    Ok(js::value::undefined())
}

/// The iterator's `next()`: the oldest queued event, else the kept error, else done once the
/// iterator is finished, else a promise for the next event.
fn on_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    if let Some(values) = shift(scope, state, c"queue")? {
        let queued = f64::from(length_of(scope, object_at(scope, state, c"queue")?)?);
        if flag(state.get_property(scope, c"paused")?)
            && queued < water_mark(scope, state, c"lowWaterMark")?
        {
            let emitter = object_at(scope, state, c"emitter")?;
            if method(scope, emitter, c"resume")?.is_some() {
                Function::call_by_name(
                    scope,
                    emitter.handle(),
                    c"resume",
                    js::function::EmptyArgs,
                )?;
            }
            state.set_property(scope, c"paused", false)?;
        }
        let result = js::iteration::create_iter_result(scope, values, false)?;
        return js::Promise::new_resolved_with_value(scope, result)?.to_jsval_raw_throwing(scope);
    }
    if flag(state.get_property(scope, c"hasError")?) {
        let error = state.get_property(scope, c"error")?;
        let error = scope.root_value(error.get());
        state.set_property(scope, c"hasError", false)?;
        state.set_property(scope, c"error", js::value::undefined())?;
        return js::Promise::new_rejected_with_error(scope, error)?.to_jsval_raw_throwing(scope);
    }
    if flag(state.get_property(scope, c"finished")?) {
        return on_close(scope, state)?.to_jsval_raw_throwing(scope);
    }
    let pending = js::Promise::new_pending(scope)?;
    push(scope, state, c"pending", pending.to_jsval_throwing(scope)?)?;
    pending.to_jsval_raw_throwing(scope)
}

/// The iterator's `return()`.
fn on_return(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    on_close(scope, state_of(scope, payload)?)?.to_jsval_raw_throwing(scope)
}

/// The iterator's `throw(error)`: fail the iterator with `error`, which must be an `Error`.
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
    on_fail(scope, state_of(scope, payload)?, error)?;
    Ok(js::value::undefined())
}

/// Add `listener` for `name` to `target` for `on`'s iterator, and record it in `state.added` for
/// [`on_close`] to remove.
fn on_add(
    scope: &Scope<'_>,
    state: Object<'_>,
    target: Object<'_>,
    name: HandleValue<'_>,
    listener: Function<'_>,
) -> Result<(), Thrown> {
    let listener = (*listener).to_jsval_throwing(scope)?;
    add_listener(scope, target, name, listener, false)?;
    let entry = js::Array::with_contents(
        scope,
        &[
            target.to_jsval_raw_throwing(scope)?,
            name.get(),
            listener.get(),
        ],
    )?;
    push(scope, state, c"added", (*entry).to_jsval_throwing(scope)?)?;
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
    const MAX_SAFE_INTEGER: f64 = 9007199254740991.0;
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
    let emitter = validate_object(scope, emitter, "emitter")?;
    const MAX_SAFE_INTEGER: f64 = 9007199254740991.0;
    let high = water_mark_option(
        scope,
        options,
        [c"highWaterMark", c"highWatermark"],
        MAX_SAFE_INTEGER,
    )?;
    let low = water_mark_option(scope, options, [c"lowWaterMark", c"lowWatermark"], 1.0)?;

    let state = Object::new_plain(scope)?;
    let empty = || js::Array::with_contents(scope, &[] as &[Value]);
    state.set_property(scope, c"emitter", emitter)?;
    state.set_property(scope, c"queue", *empty()?)?;
    state.set_property(scope, c"pending", *empty()?)?;
    state.set_property(scope, c"added", *empty()?)?;
    state.set_property(scope, c"highWaterMark", high)?;
    state.set_property(scope, c"lowWaterMark", low)?;
    if let Some(signal) = signal {
        state.set_property(scope, c"signal", signal)?;
    }

    let proto = js::class::get_async_iterator_prototype(scope)?;
    let iterator = Object::new_plain(scope)?;
    iterator.set_prototype(scope, proto)?;
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
    let is_error =
        name.is_string() && String::from_jsval(scope, name, ()).is_ok_and(|name| name == "error");
    if !is_error && method(scope, emitter, c"on")?.is_some() {
        let error = string(scope, "error")?;
        on_add(
            scope,
            state,
            emitter,
            error,
            Function::new_callback(scope, c"", 1, on_error, state)?,
        )?;
    }
    if let Some(options) = options {
        let close = options.get_property(scope, c"close")?;
        for event in elements(scope, close)? {
            on_add(
                scope,
                state,
                emitter,
                event,
                Function::new_callback(scope, c"", 0, on_close_event, state)?,
            )?;
        }
    }
    if let Some(signal) = signal {
        let abort = string(scope, "abort")?;
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
