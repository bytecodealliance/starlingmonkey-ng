// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:events`. See <https://nodejs.org/api/events.html>.
//!
//! An emitter's state lives in JS properties, as in Node's `lib/events.js`: `_events`, a
//! null-prototype object mapping each event name to its listener or an array of its listeners,
//! `_eventsCount`, and `_maxListeners`. Any object can therefore be an emitter, including one the
//! prototype's methods were copied onto.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::{HandleId, HandleObject, Value};
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};

/// The number of listeners per event above which adding one prints a warning, unless an emitter
/// sets its own limit.
const DEFAULT_MAX_LISTENERS: f64 = 10.0;

/// The key of the object holding the `kCapture` symbol in the global's shared objects.
static CAPTURE_KEY: u8 = 0;

/// The object holding the symbols of an emitter's own state properties: `kCapture`, whether it
/// captures rejections, and `shapeMode`, whether it had its own `_events` before initialization.
/// It also holds the symbol of the listener limit `setMaxListeners` stores on an `EventTarget`.
fn state_symbols<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::get_or_init_shared_object(scope, &CAPTURE_KEY as *const u8 as usize, |scope| {
        let holder = Object::new_plain(scope)?;
        holder.set_property(scope, c"capture", crate::symbol(scope, "kCapture", false)?)?;
        holder.set_property(
            scope,
            c"shapeMode",
            crate::symbol(scope, "shapeMode", false)?,
        )?;
        let limit = crate::symbol(scope, "events.maxEventTargetListeners", false)?;
        holder.set_property(scope, c"maxEventTargetListeners", limit)?;
        Ok(holder)
    })
}

/// Set `emitter`'s `shapeMode` property.
fn set_shape_mode(scope: &Scope<'_>, emitter: Object<'_>, value: bool) -> Result<(), ExnThrown> {
    let key = event_key(
        scope,
        state_symbols(scope)?.get_property(scope, c"shapeMode")?,
    )?;
    emitter.set_property_by_id(scope, key, value)
}

/// `kCapture`, the symbol an emitter's own property records whether it captures rejections under.
fn capture_symbol<'s>(scope: &'s Scope<'_>) -> Result<HandleId<'s>, ExnThrown> {
    event_key(
        scope,
        state_symbols(scope)?.get_property(scope, c"capture")?,
    )
}

/// Whether `emitter` captures rejections of the promises its listeners return.
fn captures_rejections(scope: &Scope<'_>, emitter: Object<'_>) -> Result<bool, ExnThrown> {
    let key = capture_symbol(scope)?;
    Ok(is_true(emitter.get_property_by_id(scope, key)?))
}

/// Node.js's `addCatch`: if `emitter` captures rejections, emit a rejection of `result`, the value
/// a listener for `name` returned, as an `error` event in a later tick, or pass it to the
/// emitter's `Symbol.for('nodejs.rejection')` method.
fn add_catch(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    result: HandleValue<'_>,
    name: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), Thrown> {
    if !captures_rejections(scope, emitter)? {
        return Ok(());
    }
    let Ok(promise) = Object::from_value(scope, *result) else {
        return Ok(());
    };
    let caught = (|| -> Result<(), ExnThrown> {
        let then = promise.get_property(scope, c"then")?;
        if !Object::from_value(scope, then).is_ok_and(|then| then.is_callable()) {
            return Ok(());
        }
        let mut state = vec![emitter.to_jsval_throwing(scope)?, name];
        state.extend_from_slice(args);
        let state = js::Array::with_contents(scope, &state)?;
        let on_rejected = Function::new_callback(scope, c"", 1, rejected, *state)?;
        let undefined = scope.root_value(js::value::undefined());
        Function::call(
            scope,
            promise,
            then,
            &[undefined, on_rejected.to_jsval_throwing(scope)?],
        )?;
        Ok(())
    })();
    if caught.is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        let error_name = scope.root_value(js::JSString::from_str(scope, "error")?.as_value());
        emit_event(scope, emitter, error_name, &[error])?;
    }
    Ok(())
}

/// The rejection handler [`add_catch`] attaches, whose payload is `[emitter, name, ...args]`:
/// queue Node.js's `emitUnhandledRejectionOrErr` with the reason as a tick.
fn rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    state.set_property(scope, c"reason", args.get(0))?;
    let emit = Function::new_callback(
        scope,
        c"emitUnhandledRejectionOrErr",
        0,
        emit_rejection,
        state,
    )?;
    let process = Object::from_value(scope, scope.global().get_property(scope, c"process")?)
        .map_err(|e| e.throw(scope))?;
    Function::call_by_name(
        scope,
        process.handle(),
        c"nextTick",
        &[emit.to_jsval_throwing(scope)?],
    )?;
    Ok(js::value::undefined())
}

/// Node.js's `emitUnhandledRejectionOrErr`, whose payload is `[emitter, name, ...args]` with the
/// rejection `reason`: call the emitter's `Symbol.for('nodejs.rejection')` method with the reason,
/// the event name and its arguments, or else emit the reason as an `error` event without
/// capturing rejections.
fn emit_rejection(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let emitter =
        Object::from_value(scope, state.get_element(scope, 0)?).map_err(|e| e.throw(scope))?;
    let reason = state.get_property(scope, c"reason")?;
    let length = array_length(scope, state)?;
    let rejection_key = event_key(scope, crate::symbol(scope, "nodejs.rejection", true)?)?;
    let handler = emitter.get_property_by_id(scope, rejection_key)?;
    if Object::from_value(scope, handler).is_ok_and(|handler| handler.is_callable()) {
        let mut args = vec![reason];
        for index in 1..length {
            args.push(state.get_element(scope, index)?);
        }
        Function::call(scope, emitter, handler, &args)?;
        return Ok(js::value::undefined());
    }
    let key = capture_symbol(scope)?;
    let previous = emitter.get_property_by_id(scope, key)?;
    emitter.set_property_by_id(scope, key, false)?;
    let error_name = scope.root_value(js::JSString::from_str(scope, "error")?.as_value());
    let result = emit_event(scope, emitter, error_name, &[reason]);
    emitter.set_property_by_id(scope, key, previous)?;
    result.map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::undefined())
}

/// The property key of `name`, an event name.
fn event_key<'s>(scope: &'s Scope<'_>, name: HandleValue<'_>) -> Result<HandleId<'s>, ExnThrown> {
    let id = js::id::value_to_id(scope, name)?;
    Ok(scope.root_id(id))
}

/// A new object with no prototype, like `Object.create(null)`.
fn null_prototype_object<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let object = Object::new_plain(scope)?;
    object.set_prototype(scope, HandleObject::null())?;
    Ok(object)
}

/// `emitter._events`, or `None` if it isn't an object.
fn events_object<'s>(
    scope: &'s Scope<'_>,
    emitter: Object<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let events = emitter.get_property(scope, c"_events")?;
    Ok(Object::from_value(scope, events).ok())
}

/// `emitter._eventsCount`, as a number.
fn events_count(scope: &Scope<'_>, emitter: Object<'_>) -> Result<f64, ExnThrown> {
    let count = emitter.get_property(scope, c"_eventsCount")?;
    Ok(f64::from_jsval(scope, count, ()).unwrap_or(0.0))
}

/// Reset `emitter` to having no listeners.
fn reset_events(scope: &Scope<'_>, emitter: Object<'_>) -> Result<(), ExnThrown> {
    emitter.set_property(scope, c"_events", null_prototype_object(scope)?)?;
    emitter.set_property(scope, c"_eventsCount", 0)
}

/// `emitter._events`, created if it doesn't exist yet.
fn ensure_events<'s>(scope: &'s Scope<'_>, emitter: Object<'_>) -> Result<Object<'s>, ExnThrown> {
    if let Some(events) = events_object(scope, emitter)? {
        return Ok(events);
    }
    let events = null_prototype_object(scope)?;
    emitter.set_property(scope, c"_events", events)?;
    emitter.set_property(scope, c"_eventsCount", 0)?;
    Ok(events)
}

/// The listeners stored under one event name: `undefined`, a single function, or an array.
fn listener_values<'s>(
    scope: &'s Scope<'_>,
    stored: HandleValue<'s>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    if stored.is_undefined() {
        return Ok(Vec::new());
    }
    let Ok(object) = Object::from_value(scope, stored) else {
        return Ok(Vec::new());
    };
    if !js::Array::is_array(scope, object.handle())? {
        return Ok(vec![stored]);
    }
    let length = array_length(scope, object)?;
    (0..length)
        .map(|index| object.get_element(scope, index))
        .collect()
}

/// The `length` of the array `array`.
fn array_length(scope: &Scope<'_>, array: Object<'_>) -> Result<u32, ExnThrown> {
    let length = array.get_property(scope, c"length")?;
    Ok(f64::from_jsval(scope, length, ()).map_err(|_| ExnThrown)? as u32)
}

/// The listener a `once` wrapper calls: `wrapper`'s `listener` property if it is a `once` wrapper, and
/// `wrapper` itself otherwise.
fn unwrap_listener<'s>(
    scope: &'s Scope<'_>,
    wrapper: HandleValue<'s>,
) -> Result<HandleValue<'s>, ExnThrown> {
    if let Ok(object) = Object::from_value(scope, wrapper) {
        let original = object.get_property(scope, c"listener")?;
        if !original.is_undefined() {
            return Ok(original);
        }
    }
    Ok(wrapper)
}

/// Whether `value` is `true`.
fn is_true(value: HandleValue<'_>) -> bool {
    value.is_boolean() && value.to_boolean()
}

/// Whether `a` and `b` are the same value, by `===`.
fn same_value(scope: &Scope<'_>, a: HandleValue<'_>, b: HandleValue<'_>) -> bool {
    js::comparison::strictly_equal(scope, a, b).unwrap_or(false)
}

/// The `EventEmitter` constructor of the current realm.
pub fn emitter_constructor<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    event_emitter::class_constructor(scope)
}

/// The listener limit of `emitter`: its `_maxListeners`, or `EventEmitter.defaultMaxListeners`.
fn max_listeners(scope: &Scope<'_>, emitter: Object<'_>) -> Result<f64, ExnThrown> {
    let own = emitter.get_property(scope, c"_maxListeners")?;
    if !own.is_undefined() {
        return Ok(f64::from_jsval(scope, own, ()).unwrap_or(DEFAULT_MAX_LISTENERS));
    }
    let ctor = emitter_constructor(scope)?;
    let default = ctor.get_property(scope, c"defaultMaxListeners")?;
    Ok(f64::from_jsval(scope, default, ()).unwrap_or(DEFAULT_MAX_LISTENERS))
}

/// Call `emitter.emit(...args)` through the method, so a subclass's override runs.
fn call_emit(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    Function::call_by_name(scope, emitter.handle(), c"emit", args)?;
    Ok(())
}

/// Add `listener` for `name` to `emitter`, at the front if `prepend`. Node's `_addListener`.
pub fn add_listener<'s>(
    scope: &'s Scope<'_>,
    emitter: Object<'s>,
    name: HandleValue<'_>,
    listener: HandleValue<'_>,
    prepend: bool,
) -> Result<Object<'s>, Thrown> {
    validate_function(scope, listener, "listener")?;
    let mut events = ensure_events(scope, emitter)?;
    // To avoid recursion in the case that `name === "newListener"`, the listener is added after
    // the event is emitted.
    if !events.get_property(scope, c"newListener")?.is_undefined() {
        let new_listener =
            scope.root_value(js::JSString::from_str(scope, "newListener")?.as_value());
        let original = unwrap_listener(scope, listener)?;
        call_emit(scope, emitter, &[new_listener, name, original])?;
        events = ensure_events(scope, emitter)?;
    }
    let key = event_key(scope, name)?;
    let existing = events.get_property_by_id(scope, key)?;
    if existing.is_undefined() {
        events.set_property_by_id(scope, key, listener)?;
        let count = events_count(scope, emitter)?;
        emitter.set_property(scope, c"_eventsCount", count + 1.0)?;
        return Ok(emitter);
    }
    // A second listener turns the stored function into an array, which later ones are added to.
    let array = match Object::from_value(scope, existing)
        .ok()
        .filter(|existing| js::Array::is_array(scope, existing.handle()).unwrap_or(false))
    {
        Some(array) => {
            let method = if prepend { c"unshift" } else { c"push" };
            Function::call_by_name(scope, array.handle(), method, &[listener])?;
            array
        }
        None => {
            let pair = if prepend {
                [listener, existing]
            } else {
                [existing, listener]
            };
            let array = *js::Array::with_contents(scope, &pair)?;
            events.set_property_by_id(scope, key, array)?;
            array
        }
    };

    // Warn once per event about a possible leak.
    let limit = max_listeners(scope, emitter)?;
    let count = array_length(scope, array)?;
    if limit > 0.0 && f64::from(count) > limit && !is_true(array.get_property(scope, c"warned")?) {
        array.set_property(scope, c"warned", true)?;
        warn_max_listeners(scope, emitter, name, count, limit)?;
    }
    Ok(emitter)
}

/// Print the `MaxListenersExceededWarning` for `count` listeners of `name`, through
/// `process.emitWarning` if `process` exists, and to stderr otherwise.
fn warn_max_listeners(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    name: HandleValue<'_>,
    count: u32,
    limit: f64,
) -> Result<(), ExnThrown> {
    let name = string_of(scope, name)?;
    let constructor = emitter.get_property(scope, c"constructor")?;
    let class = match Object::from_value(scope, constructor) {
        Ok(constructor) => {
            js::JSString::from_value(scope, *constructor.get_property(scope, c"name")?)?
                .to_utf8(scope)?
        }
        Err(_) => "Object".into(),
    };
    let message = format!(
        "Possible EventEmitter memory leak detected. {count} {name} listeners added to [{class}]. \
         MaxListeners is {limit}. Use emitter.setMaxListeners() to increase limit"
    );
    crate::process::emit_warning(scope, &message, "MaxListenersExceededWarning")
}

/// `String(value)`, which gives a symbol's description where `ToString` throws.
fn string_of(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    let string = scope.global().get_property(scope, c"String")?;
    let result = Function::call(scope, js::value::undefined(), string, &[value])?;
    js::JSString::from_value(scope, *result)?.to_utf8(scope)
}

/// Remove `listener` for `name` from `emitter`. Node's `removeListener`.
pub fn remove_listener<'s>(
    scope: &'s Scope<'_>,
    emitter: Object<'s>,
    name: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    validate_function(scope, listener, "listener")?;
    let Some(events) = events_object(scope, emitter)? else {
        return Ok(emitter);
    };
    let key = event_key(scope, name)?;
    let stored = events.get_property_by_id(scope, key)?;
    if stored.is_undefined() {
        return Ok(emitter);
    }
    let is_array = Object::from_value(scope, stored)
        .is_ok_and(|stored| js::Array::is_array(scope, stored.handle()).unwrap_or(false));
    let matches = |candidate: HandleValue<'_>| {
        same_value(scope, candidate, listener)
            || unwrap_listener(scope, candidate)
                .is_ok_and(|original| same_value(scope, original, listener))
    };
    // A single listener is reported unwrapped, and one of several as `listener` was given.
    let reported = if !is_array {
        if !matches(stored) {
            return Ok(emitter);
        }
        let count = events_count(scope, emitter)? - 1.0;
        emitter.set_property(scope, c"_eventsCount", count)?;
        let shape_mode = event_key(
            scope,
            state_symbols(scope)?.get_property(scope, c"shapeMode")?,
        )?;
        if is_true(emitter.get_property_by_id(scope, shape_mode)?) {
            events.set_property_by_id(scope, key, js::value::undefined())?;
        } else if count == 0.0 {
            reset_events(scope, emitter)?;
        } else {
            events.delete_property_by_id(scope, key)?;
        }
        unwrap_listener(scope, stored)?
    } else {
        let listeners = listener_values(scope, stored)?;
        let Some(position) = listeners.iter().rposition(|candidate| matches(*candidate)) else {
            return Ok(emitter);
        };
        let remaining: Vec<_> = listeners
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != position)
            .map(|(_, value)| *value)
            .collect();
        if remaining.len() == 1 {
            events.set_property_by_id(scope, key, remaining[0])?;
        } else {
            let array = js::Array::with_contents(scope, &remaining)?;
            events.set_property_by_id(scope, key, *array)?;
        }
        listener
    };
    // Whether to emit depends on the events object from before a reset.
    if !events
        .get_property(scope, c"removeListener")?
        .is_undefined()
    {
        let event = scope.root_value(js::JSString::from_str(scope, "removeListener")?.as_value());
        call_emit(scope, emitter, &[event, name, reported])?;
    }
    Ok(emitter)
}

/// Call every listener for `name` on `emitter` with `args`, and return whether there were any.
/// An `error` event without listeners throws its first argument. Node's `emit`.
pub fn emit_event(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    name: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<bool, Thrown> {
    let is_error = name.is_string()
        && js::JSString::from_value(scope, *name)
            .and_then(|name| name.equals_ascii(scope, c"error"))
            .unwrap_or(false);
    let events = events_object(scope, emitter)?;
    if is_error {
        if let Some(events) = events {
            let monitor = emitter_constructor(scope)?.get_property(scope, c"errorMonitor")?;
            if monitor.is_symbol() {
                let key = event_key(scope, monitor)?;
                if !events.get_property_by_id(scope, key)?.is_undefined() {
                    emit_event(scope, emitter, monitor, args)?;
                }
            }
        }
    }
    let stored = match events {
        Some(events) => {
            let key = event_key(scope, name)?;
            events.get_property_by_id(scope, key)?
        }
        None => scope.root_value(js::value::undefined()),
    };
    let listeners = listener_values(scope, stored)?;
    if listeners.is_empty() {
        if is_error {
            return Err(unhandled_error(scope, args.first().copied()));
        }
        return Ok(false);
    }
    for listener in listeners {
        let result = Function::call(scope, emitter, listener, args)?;
        if !result.is_undefined() && !result.is_null() {
            add_catch(scope, emitter, result, name, args)?;
        }
    }
    Ok(true)
}

/// The error an `error` event without listeners throws: `error` itself if it is an `Error`, and
/// Node.js's `getUnhandledErrorException`: the error an `error` event without listeners throws,
/// `error` itself if it is an `Error`, and otherwise `ERR_UNHANDLED_ERROR` with `error` inspected
/// in its message and as its `context` property.
fn unhandled_error(scope: &Scope<'_>, error: Option<HandleValue<'_>>) -> Thrown {
    let error = error.unwrap_or_else(|| scope.root_value(js::value::undefined()));
    if js::exception::is_error_object(scope, error) {
        js::exception::set_pending(scope, error, js::native::ExceptionStackBehavior::Capture);
        return Thrown::Pending;
    }
    let description = match crate::inspect::inspect(scope, error, Default::default()) {
        Ok(description) => description,
        Err(ExnThrown) => {
            js::exception::clear(scope);
            string_of(scope, error).unwrap_or_default()
        }
    };
    let thrown = NodeError::new(
        ErrorKind::Error,
        "ERR_UNHANDLED_ERROR",
        format!("Unhandled error. ({description})"),
    )
    .throw(scope);
    if let Some(exception) = js::exception::take_pending(scope) {
        let exception = scope.root_value(exception.get());
        if let Ok(object) = Object::from_value(scope, exception) {
            if object.set_property(scope, c"context", error).is_err() {
                return Thrown::Pending;
            }
        }
        js::exception::set_pending(
            scope,
            exception,
            js::native::ExceptionStackBehavior::DoNotCapture,
        );
    }
    let ExnThrown = thrown;
    Thrown::Pending
}

/// The body of a `once` wrapper: remove the wrapper, then call the listener once. The payload is
/// the wrapper's state object, holding `target`, `type`, `listener`, `wrapper` and `fired`.
fn once_wrapper(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    if is_true(state.get_property(scope, c"fired")?) {
        return Ok(js::value::undefined());
    }
    state.set_property(scope, c"fired", true)?;
    let target = Object::from_value(scope, state.get_property(scope, c"target")?)
        .map_err(|e| e.throw(scope))?;
    let name = state.get_property(scope, c"type")?;
    let wrapper = state.get_property(scope, c"wrapper")?;
    Function::call_by_name(scope, target.handle(), c"removeListener", &[name, wrapper])?;
    let listener = state.get_property(scope, c"listener")?;
    let call_args: Vec<_> = (0..args.len()).map(|index| args.get(index)).collect();
    let result = Function::call(scope, target, listener, &call_args)?;
    Ok(result.get())
}

/// A wrapper for `listener` that removes itself from `target`'s `name` listeners before calling
/// it. Node's `_onceWrap`.
fn once_wrap<'s>(
    scope: &'s Scope<'_>,
    target: Object<'_>,
    name: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let state = Object::new_plain(scope)?;
    state.set_property(scope, c"fired", false)?;
    state.set_property(scope, c"target", target)?;
    state.set_property(scope, c"type", name)?;
    state.set_property(scope, c"listener", listener)?;
    let wrapper = Function::new_callback(scope, c"bound onceWrapper", 0, once_wrapper, state)?;
    state.set_property(scope, c"wrapper", *wrapper)?;
    wrapper.set_property(scope, c"listener", listener)?;
    (*wrapper).to_jsval_throwing(scope)
}

/// The listeners for `name` on `emitter`, with `once` wrappers unwrapped if `unwrap`.
fn listeners_of<'s>(
    scope: &'s Scope<'_>,
    emitter: Object<'_>,
    name: HandleValue<'_>,
    unwrap: bool,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let Some(events) = events_object(scope, emitter)? else {
        return Ok(Vec::new());
    };
    let key = event_key(scope, name)?;
    let listeners = listener_values(scope, events.get_property_by_id(scope, key)?)?;
    if !unwrap {
        return Ok(listeners);
    }
    listeners
        .into_iter()
        .map(|listener| unwrap_listener(scope, listener))
        .collect()
}

#[core_runtime::jsfunction_class(name = "EventEmitter")]
pub mod event_emitter {
    use super::*;

    /// <https://nodejs.org/api/events.html#class-eventemitter>: initialize `this` as an emitter
    /// with no listeners, unless it already has its own `_events`. `options.captureRejections`
    /// makes it emit rejections of the promises its listeners return as `error` events.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let events = this.get_property(scope, c"_events")?;
        let inherited = this
            .get_prototype(scope)?
            .map(|proto| proto.get_property(scope, c"_events"))
            .transpose()?;
        let shared = inherited.is_some_and(|inherited| same_value(scope, inherited, events));
        let reset = events.is_undefined() || shared;
        if reset {
            reset_events(scope, this)?;
        }
        set_shape_mode(scope, this, !reset)?;
        let max = this.get_property(scope, c"_maxListeners")?;
        if !bool::from_jsval(scope, max, ()).unwrap_or(false) {
            this.set_property(scope, c"_maxListeners", js::value::undefined())?;
        }
        let capture = match options.and_then(|options| Object::from_value(scope, *options).ok()) {
            Some(options) => options.get_property(scope, c"captureRejections")?,
            None => scope.root_value(js::value::undefined()),
        };
        let key = capture_symbol(scope)?;
        if bool::from_jsval(scope, capture, ()).unwrap_or(false) {
            if !capture.is_boolean() {
                let name = "options.captureRejections";
                return Err(
                    NodeError::invalid_arg_type(scope, name, "of type boolean", capture).into(),
                );
            }
            this.set_property_by_id(scope, key, true)?;
        } else {
            let proto = emitter_constructor(scope)?.get_property(scope, c"prototype")?;
            let proto = Object::from_value(scope, proto)?;
            this.set_property_by_id(scope, key, proto.get_property_by_id(scope, key)?)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/events.html#emitteronEventname-listener>
    pub fn on<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::add_listener(scope, this, name, listener, false)
    }

    /// <https://nodejs.org/api/events.html#emitteraddlistenereventname-listener>
    pub fn add_listener<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::add_listener(scope, this, name, listener, false)
    }

    /// <https://nodejs.org/api/events.html#emitterprependlistenereventname-listener>
    pub fn prepend_listener<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::add_listener(scope, this, name, listener, true)
    }

    /// <https://nodejs.org/api/events.html#emitteronceeventname-listener>
    pub fn once<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        validate_function(scope, listener, "listener")?;
        let wrapper = once_wrap(scope, this, name, listener)?;
        Function::call_by_name(scope, this.handle(), c"on", &[name, wrapper])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/events.html#emitterprependoncelistenereventname-listener>
    pub fn prepend_once_listener<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        validate_function(scope, listener, "listener")?;
        let wrapper = once_wrap(scope, this, name, listener)?;
        Function::call_by_name(scope, this.handle(), c"prependListener", &[name, wrapper])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/events.html#emitterremovelistenereventname-listener>
    pub fn remove_listener<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::remove_listener(scope, this, name, listener)
    }

    /// <https://nodejs.org/api/events.html#emitteroffeventname-listener>
    pub fn off<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::remove_listener(scope, this, name, listener)
    }

    /// <https://nodejs.org/api/events.html#emitterremovealllistenerseventname>
    pub fn remove_all_listeners<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        name: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        // An explicit `undefined` names the event "undefined".
        let Some(events) = events_object(scope, this)? else {
            return Ok(this);
        };
        // Without `removeListener` listeners, nothing observes the removal.
        if events
            .get_property(scope, c"removeListener")?
            .is_undefined()
        {
            match name {
                None => reset_events(scope, this)?,
                Some(name) => {
                    let key = event_key(scope, name)?;
                    if !events.get_property_by_id(scope, key)?.is_undefined() {
                        let count = events_count(scope, this)? - 1.0;
                        if count == 0.0 {
                            reset_events(scope, this)?;
                        } else {
                            this.set_property(scope, c"_eventsCount", count)?;
                            events.delete_property_by_id(scope, key)?;
                        }
                    }
                }
            }
            set_shape_mode(scope, this, false)?;
            return Ok(this);
        }
        let Some(name) = name else {
            // Remove `removeListener` last, so its listeners see every other removal.
            for key in events.own_keys(scope)? {
                if key.is_string()
                    && String::from_jsval(scope, key, ()).is_ok_and(|key| key == "removeListener")
                {
                    continue;
                }
                Function::call_by_name(scope, this.handle(), c"removeAllListeners", &[key])?;
            }
            let remove_listener =
                scope.root_value(js::JSString::from_str(scope, "removeListener")?.as_value());
            Function::call_by_name(
                scope,
                this.handle(),
                c"removeAllListeners",
                &[remove_listener],
            )?;
            reset_events(scope, this)?;
            set_shape_mode(scope, this, false)?;
            return Ok(this);
        };
        let key = event_key(scope, name)?;
        let listeners = listener_values(scope, events.get_property_by_id(scope, key)?)?;
        for listener in listeners.into_iter().rev() {
            Function::call_by_name(scope, this.handle(), c"removeListener", &[name, listener])?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/events.html#emitteremiteventname-args>
    pub fn emit(
        scope: &Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        emit_event(scope, this, name, &args)
    }

    /// <https://nodejs.org/api/events.html#emittersetmaxlistenersn>
    pub fn set_max_listeners<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        n: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        if !n.is_number() {
            return Err(
                NodeError::invalid_arg_type(scope, "setMaxListeners", "of type number", n).into(),
            );
        }
        let number = n.to_number();
        if number.is_nan() || number < 0.0 {
            return Err(
                NodeError::out_of_range("setMaxListeners", ">= 0", &number.to_string()).into(),
            );
        }
        this.set_property(scope, c"_maxListeners", n)?;
        Ok(this)
    }

    /// <https://nodejs.org/api/events.html#emittergetmaxlisteners>
    pub fn get_max_listeners(scope: &Scope<'_>, this: Object<'_>) -> Result<f64, Thrown> {
        Ok(max_listeners(scope, this)?)
    }

    /// <https://nodejs.org/api/events.html#emitterlistenerseventname>
    pub fn listeners<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
    ) -> Result<js::Array<'s>, Thrown> {
        let listeners = listeners_of(scope, this, name, true)?;
        Ok(js::Array::with_contents(scope, &listeners)?)
    }

    /// <https://nodejs.org/api/events.html#emitterrawlistenerseventname>
    pub fn raw_listeners<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
    ) -> Result<js::Array<'s>, Thrown> {
        let listeners = listeners_of(scope, this, name, false)?;
        Ok(js::Array::with_contents(scope, &listeners)?)
    }

    /// <https://nodejs.org/api/events.html#emitterlistenercounteventname-listener>
    pub fn listener_count(
        scope: &Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
        listener: Option<HandleValue<'_>>,
    ) -> Result<u32, Thrown> {
        let listeners = listeners_of(scope, this, name, true)?;
        Ok(match listener.filter(|listener| !listener.is_undefined()) {
            Some(listener) => listeners
                .iter()
                .filter(|candidate| same_value(scope, **candidate, listener))
                .count() as u32,
            None => listeners.len() as u32,
        })
    }

    /// <https://nodejs.org/api/events.html#emitter-eventnames>
    pub fn event_names<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<js::Array<'s>, Thrown> {
        let names = match events_object(scope, this)? {
            Some(events) if events_count(scope, this)? > 0.0 => events.own_keys(scope)?,
            _ => Vec::new(),
        };
        Ok(js::Array::with_contents(scope, &names)?)
    }

    /// Define the static members: `defaultMaxListeners`, `errorMonitor`,
    /// `captureRejectionSymbol`, `captureRejections`, `usingDomains`, and `EventEmitter` itself.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        proto.set_property_by_id(scope, capture_symbol(scope)?, false)?;
        // The prototype's defaults for emitters that `init` didn't initialize.
        proto.set_property(scope, c"_events", js::value::undefined())?;
        proto.set_property(scope, c"_eventsCount", 0)?;
        proto.set_property(scope, c"_maxListeners", js::value::undefined())?;
        // `on` and `off` are the same functions as `addListener` and `removeListener`.
        let add_listener = proto.get_property(scope, c"addListener")?;
        proto.define_property(
            scope,
            c"on",
            add_listener,
            js::class_spec::JSPROP_ENUMERATE as u32,
        )?;
        let remove_listener = proto.get_property(scope, c"removeListener")?;
        proto.define_property(
            scope,
            c"off",
            remove_listener,
            js::class_spec::JSPROP_ENUMERATE as u32,
        )?;
        ctor.set_property(scope, c"defaultMaxListeners", DEFAULT_MAX_LISTENERS)?;
        let monitor = crate::symbol(scope, "events.errorMonitor", false)?;
        ctor.set_property(scope, c"errorMonitor", monitor)?;
        let capture = crate::symbol(scope, "nodejs.rejection", true)?;
        ctor.set_property(scope, c"captureRejectionSymbol", capture)?;
        ctor.set_property(scope, c"captureRejections", false)?;
        ctor.set_property(scope, c"usingDomains", false)?;
        ctor.set_property(scope, c"EventEmitter", ctor)?;
        Ok(())
    }
}

/// Whether `value` has `EventTarget.prototype` on its prototype chain.
fn is_event_target(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    let Ok(object) = Object::from_value(scope, *value) else {
        return Ok(false);
    };
    let event_target = scope.global().get_property(scope, c"EventTarget")?;
    let Ok(event_target) = Object::from_value(scope, event_target) else {
        return Ok(false);
    };
    let Ok(prototype) = Object::from_value(scope, event_target.get_property(scope, c"prototype")?)
    else {
        return Ok(false);
    };
    let mut current = object.get_prototype(scope)?;
    while let Some(candidate) = current {
        if candidate.as_raw() == prototype.as_raw() {
            return Ok(true);
        }
        current = candidate.get_prototype(scope)?;
    }
    Ok(false)
}

/// The key of the listener limit `setMaxListeners` stores on an `EventTarget`.
fn event_target_limit_key<'s>(scope: &'s Scope<'_>) -> Result<HandleId<'s>, ExnThrown> {
    let symbol = state_symbols(scope)?.get_property(scope, c"maxEventTargetListeners")?;
    event_key(scope, symbol)
}

/// `ERR_INVALID_ARG_TYPE` for an argument `name` that is neither an `EventEmitter` nor an
/// `EventTarget`.
fn not_an_emitter(scope: &Scope<'_>, name: &str, value: HandleValue<'_>) -> Thrown {
    let expected = "an instance of EventEmitter or EventTarget";
    NodeError::invalid_arg_type(scope, name, expected, value).into()
}

/// `value` as an object, if it has a method `name`.
fn with_method<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let Ok(object) = Object::from_value(scope, *value) else {
        return Ok(None);
    };
    let method = object.get_property(scope, name)?;
    Ok(Object::from_value(scope, method)
        .is_ok_and(|method| method.is_callable())
        .then_some(object))
}

#[core_runtime::jsmodule(
    node_args,
    name = "node:events",
    default = "EventEmitter",
    mirror = "errorMonitor, captureRejectionSymbol, captureRejections, defaultMaxListeners, usingDomains"
)]
pub mod module {
    use super::*;

    pub use super::event_emitter as EventEmitter;

    /// <https://nodejs.org/api/events.html#eventsgeteventlistenersemitterortarget-eventname>
    pub fn get_event_listeners<'s>(
        scope: &'s Scope<'_>,
        emitter: HandleValue<'_>,
        name: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        if let Some(object) = with_method(scope, emitter, c"listeners")? {
            let listeners = Function::call_by_name(scope, object.handle(), c"listeners", &[name])?;
            return Ok(scope.root_value(listeners.get()));
        }
        Err(not_an_emitter(scope, "emitter", emitter))
    }

    /// <https://nodejs.org/api/events.html#eventsgetmaxlistenersemitterortarget>
    pub fn get_max_listeners(scope: &Scope<'_>, emitter: HandleValue<'_>) -> Result<f64, Thrown> {
        if let Some(object) = with_method(scope, emitter, c"getMaxListeners")? {
            return Ok(max_listeners(scope, object)?);
        }
        if is_event_target(scope, emitter)? {
            let target = Object::from_value(scope, *emitter)?;
            let limit = target.get_property_by_id(scope, event_target_limit_key(scope)?)?;
            if limit.is_number() {
                return Ok(limit.to_number());
            }
            let ctor = emitter_constructor(scope)?;
            return Ok(ctor
                .get_property(scope, c"defaultMaxListeners")?
                .to_number());
        }
        Err(not_an_emitter(scope, "emitter", emitter))
    }

    /// <https://nodejs.org/api/events.html#eventssetmaxlistenersn-eventtargets>: without targets,
    /// sets `EventEmitter.defaultMaxListeners`. `n` defaults to the current default.
    pub fn set_max_listeners(
        scope: &Scope<'_>,
        n: Option<HandleValue<'_>>,
        targets: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let ctor = emitter_constructor(scope)?;
        let n = match n.filter(|n| !n.is_undefined()) {
            Some(n) => n,
            None => ctor.get_property(scope, c"defaultMaxListeners")?,
        };
        if !n.is_number() {
            let error = NodeError::invalid_arg_type(scope, "setMaxListeners", "of type number", n);
            return Err(error.into());
        }
        if n.to_number().is_nan() || n.to_number() < 0.0 {
            let received = n.to_number().to_string();
            return Err(NodeError::out_of_range("setMaxListeners", ">= 0", &received).into());
        }
        if targets.is_empty() {
            ctor.set_property(scope, c"defaultMaxListeners", n)?;
            return Ok(());
        }
        for target in targets.iter() {
            if is_event_target(scope, *target)? {
                let target = Object::from_value(scope, **target)?;
                target.set_property_by_id(scope, event_target_limit_key(scope)?, n)?;
            } else if let Some(object) = with_method(scope, *target, c"setMaxListeners")? {
                Function::call_by_name(scope, object.handle(), c"setMaxListeners", &[n])?;
            } else {
                return Err(not_an_emitter(scope, "eventTargets", *target));
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/events.html#eventsonceemitter-name-options>
    pub fn once<'s>(
        scope: &'s Scope<'_>,
        emitter: HandleValue<'_>,
        name: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        crate::events_async::once(scope, emitter, name, options)
    }

    /// <https://nodejs.org/api/events.html#eventsonemitter-eventname-options>: the
    /// `highWaterMark` and `lowWaterMark` options call the emitter's `pause` and `resume` if it has
    /// them.
    pub fn on<'s>(
        scope: &'s Scope<'_>,
        emitter: HandleValue<'_>,
        name: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        crate::events_async::on(scope, emitter, name, options)
    }

    /// <https://nodejs.org/api/events.html#eventslistenercountemitter-eventname>
    pub fn listener_count(
        scope: &Scope<'_>,
        emitter: HandleValue<'_>,
        name: HandleValue<'_>,
    ) -> Result<u32, Thrown> {
        if let Some(object) = with_method(scope, emitter, c"listenerCount")? {
            let count = Function::call_by_name(scope, object.handle(), c"listenerCount", &[name])?;
            return Ok(count.to_number() as u32);
        }
        let object = crate::errors::validate_object(scope, emitter, "emitter")?;
        Ok(listeners_of(scope, object, name, true)?.len() as u32)
    }
}
