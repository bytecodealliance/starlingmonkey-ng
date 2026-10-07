// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:events`. See <https://nodejs.org/api/events.html>.
//!
//! An emitter's state lives in JS properties, as in Node's `lib/events.js`: `_events`, a
//! null-prototype object mapping each event name to its listener or an array of its listeners,
//! `_eventsCount`, and `_maxListeners`. Any object can therefore be an emitter, including one the
//! prototype's methods were copied onto.

use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::{HandleId, Value};
use js::object::{PropertyTemplate, PropertyWriter, TemplateKey};
use js::prelude::HandleValue;
use js::symbol::CachedSymbol;
use js::{Function, Object};

use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};
use crate::support::as_array;

js::instance_local! {
    /// `EventEmitter.defaultMaxListeners`: the number of listeners per event above which adding
    /// one prints a warning, unless an emitter sets its own limit.
    static DEFAULT_MAX_LISTENERS: std::cell::Cell<f64> = const { std::cell::Cell::new(10.0) };
}

/// `kCapture`, the symbol an emitter's own property records whether it captures rejections under.
static K_CAPTURE: CachedSymbol = CachedSymbol::new(c"kCapture");

/// `shapeMode`, the symbol an emitter's own property records whether it had its own `_events`
/// before initialization under.
static SHAPE_MODE: CachedSymbol = CachedSymbol::new(c"shapeMode");

/// The own properties that initialization creates on an emitter without `_events`, in Node's
/// order.
pub(crate) static EMITTER_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"_events"),
    TemplateKey::Name(c"_eventsCount"),
    TemplateKey::Symbol(&SHAPE_MODE),
    TemplateKey::Name(c"_maxListeners"),
    TemplateKey::Symbol(&K_CAPTURE),
]);

/// Initialize `emitter`, which `properties` writes to and this crate created without `_events`, as
/// `EventEmitter.init()` does without options: with no listeners. It reads `_maxListeners`
/// through the prototype chain, and `kCapture` from `EventEmitter.prototype`. Once `node:domain`
/// is loaded, `emitter.domain` is defined first.
pub(crate) fn init_new_emitter<'w>(
    scope: &'w Scope<'_>,
    emitter: Object<'_>,
    properties: &mut PropertyWriter<'w>,
) -> Result<(), ExnThrown> {
    if crate::domain::in_use() {
        crate::domain::init_emitter(scope, emitter)?;
    }
    properties.continue_with(scope, &EMITTER_PROPERTIES)?;
    properties.write(scope, c"_events", Object::new_with_null_prototype(scope)?)?;
    properties.write(scope, c"_eventsCount", 0)?;
    properties.write(scope, &SHAPE_MODE, false)?;
    let max = properties.get(scope, c"_maxListeners")?;
    if bool::from_jsval(scope, max, ()).unwrap_or(false) {
        properties.skip(c"_maxListeners");
    } else {
        properties.write(scope, c"_maxListeners", js::value::undefined())?;
    }
    let proto = emitter_constructor(scope)?.get_property(scope, c"prototype")?;
    let proto = Object::from_value(scope, proto).map_err(|e| e.throw(scope))?;
    let capture = proto.get_property_by_id(scope, capture_symbol(scope)?)?;
    properties.write(scope, &K_CAPTURE, capture)
}

/// The symbol of the listener limit `setMaxListeners` stores on an `EventTarget`.
static MAX_EVENT_TARGET_LISTENERS: CachedSymbol =
    CachedSymbol::new(c"events.maxEventTargetListeners");

/// `EventEmitter.errorMonitor`.
pub(crate) static ERROR_MONITOR: CachedSymbol = CachedSymbol::new(c"events.errorMonitor");

/// `EventEmitter.captureRejectionSymbol`, `Symbol.for('nodejs.rejection')`.
pub(crate) static REJECTION: CachedSymbol = CachedSymbol::registered(c"nodejs.rejection");

/// Set `emitter`'s `shapeMode` property.
fn set_shape_mode(scope: &Scope<'_>, emitter: Object<'_>, value: bool) -> Result<(), ExnThrown> {
    emitter.set_property_by_id(scope, SHAPE_MODE.id(scope)?, value)
}

/// `kCapture`, the symbol an emitter's own property records whether it captures rejections under.
fn capture_symbol<'s>(scope: &'s Scope<'_>) -> Result<HandleId<'s>, ExnThrown> {
    K_CAPTURE.id(scope)
}

/// Whether `emitter` captures rejections of the promises its listeners return.
fn captures_rejections(scope: &Scope<'_>, emitter: Object<'_>) -> Result<bool, ExnThrown> {
    let key = capture_symbol(scope)?;
    Ok(crate::support::is_true(
        emitter.get_property_by_id(scope, key)?,
    ))
}

/// The emission whose listener returned a promise that [`add_catch`] watches: the emitter, the
/// event name, and the event's arguments.
#[core_runtime::jsclass(hidden)]
pub(crate) struct RejectionState {
    emitter: Heap<js::object::Object>,
    name: Heap<Value>,
    args: Vec<Heap<Value>>,
}

#[core_runtime::jsmethods]
impl RejectionState {}

/// Call `emitter.emit('error', error)`.
fn emit_error(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let name = js::class::get_or_init_atom(scope, c"error")?;
    call_emit(scope, emitter, &[name, error])
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
        if !js::function::is_callable(then) {
            return Ok(());
        }
        let state =
            js::class::create_instance_with::<RejectionStateImpl>(scope, |_| RejectionStateImpl {
                emitter: Heap::from(emitter),
                name: Heap::from(name.get()),
                args: args.iter().map(|arg| Heap::from(arg.get())).collect(),
            })?;
        let on_rejected = Function::new_callback(scope, c"", 1, rejected, state)?;
        Function::call(
            scope,
            promise,
            then,
            &[
                HandleValue::undefined(),
                (*on_rejected).to_jsval_throwing(scope)?,
            ],
        )?;
        Ok(())
    })();
    if caught.is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        emit_error(scope, emitter, error)?;
    }
    Ok(())
}

/// The rejection handler [`add_catch`] attaches, whose payload is a [`RejectionState`]: queue
/// Node.js's `emitUnhandledRejectionOrErr` with the reason as a tick.
fn rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    js::jobs::queue_native_tick(scope, emit_rejection, payload, &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// Node.js's `emitUnhandledRejectionOrErr`, whose payload is a [`RejectionState`] and whose
/// argument is the rejection reason: call the emitter's `Symbol.for('nodejs.rejection')` method
/// with the reason, the event name and its arguments, or else emit the reason as an `error` event
/// without capturing rejections.
fn emit_rejection(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = js::function::cast_payload::<RejectionState>(scope, payload);
    let reason = args.get(0);
    let (emitter, name, event_args) = {
        let data = state.data();
        let event_args: Vec<HandleValue<'_>> = data.args.iter().map(|arg| arg.get(scope)).collect();
        (data.emitter.get(scope), data.name.get(scope), event_args)
    };
    let rejection_key = REJECTION.id(scope)?;
    let handler = emitter.get_property_by_id(scope, rejection_key)?;
    if js::function::is_callable(handler) {
        let mut call_args = vec![reason, name];
        call_args.extend(event_args);
        Function::call(scope, emitter, handler, &call_args)?;
        return Ok(js::value::undefined());
    }
    let key = capture_symbol(scope)?;
    let previous = emitter.get_property_by_id(scope, key)?;
    emitter.set_property_by_id(scope, key, false)?;
    let result = emit_error(scope, emitter, reason);
    emitter.set_property_by_id(scope, key, previous)?;
    result?;
    Ok(js::value::undefined())
}

/// Emit `event` on `emitter` with `args`, through its `emit` method. Returns whether `event` had
/// listeners.
pub(crate) fn emit(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    event: &'static CStr,
    args: &[HandleValue<'_>],
) -> Result<bool, ExnThrown> {
    let mut all = Vec::with_capacity(args.len() + 1);
    all.push(js::class::get_or_init_atom(scope, event)?);
    all.extend_from_slice(args);
    let result = Function::call_by_name(scope, emitter.handle(), c"emit", &all)?;
    Ok(js::conversion::to_boolean(result))
}

/// Call the method `method` of `emitter` with `event` and `listener`, such as `on` or
/// `removeListener`.
pub(crate) fn listen(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    method: &'static CStr,
    event: &'static CStr,
    listener: Object<'_>,
) -> Result<(), ExnThrown> {
    let event = js::class::get_or_init_atom(scope, event)?;
    let listener = listener.to_jsval_throwing(scope)?;
    Function::call_by_name(scope, emitter.handle(), method, &[event, listener])?;
    Ok(())
}

/// `emitter.listenerCount(event)`.
pub(crate) fn listener_count(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    event: HandleValue<'_>,
) -> Result<f64, ExnThrown> {
    let count = Function::call_by_name(scope, emitter.handle(), c"listenerCount", &[event])?;
    Ok(if count.is_number() {
        count.to_number()
    } else {
        0.0
    })
}

/// The property key of `name`, an event name.
fn event_key<'s>(scope: &'s Scope<'_>, name: HandleValue<'_>) -> Result<HandleId<'s>, ExnThrown> {
    let id = js::id::value_to_id(scope, name)?;
    Ok(scope.root_id(id))
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
    f64::from_jsval_throwing(scope, count, ())
}

/// Reset `emitter` to having no listeners.
fn reset_events(scope: &Scope<'_>, emitter: Object<'_>) -> Result<(), ExnThrown> {
    emitter.set_property(scope, c"_events", Object::new_with_null_prototype(scope)?)?;
    emitter.set_property(scope, c"_eventsCount", 0)
}

/// `emitter._events`, created if it doesn't exist yet.
fn ensure_events<'s>(scope: &'s Scope<'_>, emitter: Object<'_>) -> Result<Object<'s>, ExnThrown> {
    if let Some(events) = events_object(scope, emitter)? {
        return Ok(events);
    }
    let events = Object::new_with_null_prototype(scope)?;
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
    if !stored.is_object() {
        return Ok(Vec::new());
    }
    match as_array(scope, stored) {
        Some(array) => array.to_vec(scope),
        None => Ok(vec![stored]),
    }
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

/// Whether `a` and `b` are the same value, by `===`.
fn same_value(scope: &Scope<'_>, a: HandleValue<'_>, b: HandleValue<'_>) -> bool {
    js::comparison::strictly_equal(scope, a, b).unwrap_or(false)
}

/// The `EventEmitter` constructor of the current realm.
pub fn emitter_constructor<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    event_emitter::class_constructor(scope)
}

/// Node.js's `_getMaxListeners`: the listener limit of `emitter`, its `_maxListeners`, which can be
/// any value, or `EventEmitter.defaultMaxListeners` if that is `undefined`.
fn max_listeners<'s>(
    scope: &'s Scope<'_>,
    emitter: Object<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let own = emitter.get_property(scope, c"_maxListeners")?;
    if !own.is_undefined() {
        return Ok(own);
    }
    Ok(scope.root_value(js::value::from_f64(DEFAULT_MAX_LISTENERS.get())))
}

/// The getter of `EventEmitter.defaultMaxListeners`.
fn get_default_max_listeners(
    _: &Scope<'_>,
    _: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::from_f64(DEFAULT_MAX_LISTENERS.get()))
}

/// The setter of `EventEmitter.defaultMaxListeners`, which validates the value as
/// `validateNumber(value, 'defaultMaxListeners', 0)` does.
fn set_default_max_listeners(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let value = crate::errors::validate_number(scope, args.get(0), "defaultMaxListeners", 0.0)
        .map_err(|error| error.throw(scope))?;
    DEFAULT_MAX_LISTENERS.set(value);
    Ok(js::value::undefined())
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
    let array = match as_array(scope, existing) {
        Some(array) => {
            if prepend {
                array.unshift(scope, listener)?;
            } else {
                array.push(scope, listener)?;
            }
            array
        }
        None => {
            let pair = if prepend {
                [listener, existing]
            } else {
                [existing, listener]
            };
            let array = js::Array::with_contents(scope, &pair)?;
            events.set_property_by_id(scope, key, *array)?;
            array
        }
    };

    // Warn once per event about a possible leak.
    let limit = f64::from_jsval_throwing(scope, max_listeners(scope, emitter)?, ())?;
    let count = array.length(scope)?;
    if limit > 0.0
        && f64::from(count) > limit
        && !crate::support::is_true(array.get_property(scope, c"warned")?)
    {
        array.set_property(scope, c"warned", true)?;
        warn_max_listeners(scope, emitter, name, count, limit)?;
    }
    Ok(emitter)
}

/// Print the `MaxListenersExceededWarning` for `count` listeners of the event `event`, through
/// `process.emitWarning` if `process` exists, and to stderr otherwise. The warning is an `Error`
/// with the `emitter`, the event as `type`, and the `count`.
fn warn_max_listeners(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    event: HandleValue<'_>,
    count: u32,
    limit: f64,
) -> Result<(), ExnThrown> {
    let name = string_of(scope, event)?;
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
    let process = crate::process::process_value(scope)?;
    let Ok(process) = Object::from_value(scope, process) else {
        return crate::process::emit_warning(scope, &message, "MaxListenersExceededWarning");
    };
    let error_class = crate::stack_trace::error_constructor(scope)?;
    let message = js::JSString::from_str(scope, &message)?.to_jsval_throwing(scope)?;
    let warning = Function::construct(scope, error_class, &[message])?;
    warning.set_property(scope, c"name", "MaxListenersExceededWarning")?;
    warning.set_property(scope, c"emitter", emitter)?;
    warning.set_property(scope, c"type", event)?;
    warning.set_property(scope, c"count", count)?;
    Function::call_by_name(
        scope,
        process.handle(),
        c"emitWarning",
        &[warning.to_jsval_throwing(scope)?],
    )?;
    Ok(())
}

/// `String(value)`, which gives a symbol's description where `ToString` throws.
fn string_of(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    let string = crate::support::class_object(scope, js::class_spec::JSProtoKey::JSProto_String)?;
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
    let array = as_array(scope, stored);
    let matches = |candidate: HandleValue<'_>| {
        same_value(scope, candidate, listener)
            || unwrap_listener(scope, candidate)
                .is_ok_and(|original| same_value(scope, original, listener))
    };
    // A single listener is reported unwrapped, and one of several as `listener` was given.
    let reported = if let Some(array) = array {
        let listeners = array.to_vec(scope)?;
        let Some(position) = listeners.iter().rposition(|candidate| matches(*candidate)) else {
            return Ok(emitter);
        };
        array.remove(scope, position as u32, 1)?;
        if array.length(scope)? == 1 {
            let only = array.get_element(scope, 0)?;
            events.set_property_by_id(scope, key, only)?;
        }
        listener
    } else {
        if !matches(stored) {
            return Ok(emitter);
        }
        let count = events_count(scope, emitter)? - 1.0;
        emitter.set_property(scope, c"_eventsCount", count)?;
        let shape_mode = SHAPE_MODE.id(scope)?;
        if crate::support::is_true(emitter.get_property_by_id(scope, shape_mode)?) {
            events.set_property_by_id(scope, key, js::value::undefined())?;
        } else if count == 0.0 {
            reset_events(scope, emitter)?;
        } else {
            events.delete_property_by_id(scope, key)?;
        }
        unwrap_listener(scope, stored)?
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
            let key = ERROR_MONITOR.id(scope)?;
            if !events.get_property_by_id(scope, key)?.is_undefined() {
                let mut monitor_args = vec![ERROR_MONITOR.value(scope)?];
                monitor_args.extend_from_slice(args);
                call_emit(scope, emitter, &monitor_args)?;
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
        js::exception::throw_value(scope, exception);
    }
    let ExnThrown = thrown;
    Thrown::Pending
}

/// The state of a `once` wrapper, Node's `onceWrapper` `this`: the emitter, the event name, the
/// listener, the wrapper, and whether it fired.
#[core_runtime::jsclass(hidden)]
pub(crate) struct OnceState {
    target: Heap<js::object::Object>,
    name: Heap<Value>,
    listener: Heap<Value>,
    wrapper: Option<Heap<js::object::Object>>,
    #[no_trace]
    fired: bool,
}

#[core_runtime::jsmethods]
impl OnceState {}

/// The body of a `once` wrapper, whose payload is a [`OnceState`]: remove the wrapper, then call
/// the listener once.
fn once_wrapper(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = js::function::cast_payload::<OnceState>(scope, payload);
    if state.data().fired {
        return Ok(js::value::undefined());
    }
    let (target, name, wrapper) = {
        let data = state.data();
        (
            data.target.get(scope),
            data.name.get(scope),
            data.wrapper.get(scope),
        )
    };
    let wrapper = match wrapper {
        Some(wrapper) => scope.root_value(wrapper.as_value()),
        None => HandleValue::undefined(),
    };
    Function::call_by_name(scope, target.handle(), c"removeListener", &[name, wrapper])?;
    state.data_mut().fired = true;
    let listener = state.data().listener.get(scope);
    let call_args: Vec<_> = (0..args.len()).map(|index| args.get(index)).collect();
    let result = Function::call(scope, target, listener, &call_args)?;
    Ok(result.get())
}

/// A wrapper for `listener` that removes itself from `target`'s `name` listeners before calling
/// it, with the listener as its `listener` property. Node's `_onceWrap`.
fn once_wrap<'s>(
    scope: &'s Scope<'_>,
    target: Object<'_>,
    name: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let state = js::class::create_instance_with::<OnceStateImpl>(scope, |_| OnceStateImpl {
        target: Heap::from(target),
        name: Heap::from(name.get()),
        listener: Heap::from(listener.get()),
        wrapper: None,
        fired: false,
    })?;
    let wrapper = Function::new_callback(scope, c"wrapper", 0, once_wrapper, state)?;
    state.data_mut().wrapper = Some(Heap::from(*wrapper));
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
    /// makes it emit rejections of the promises its listeners return as `error` events. Once
    /// `node:domain` is loaded, `this.domain` is defined first.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if crate::domain::in_use() {
            crate::domain::init_emitter(scope, this)?;
        }
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

    /// `EventEmitter.init(options)`: initialize `this` as an emitter, as the constructor does.
    #[static_method(name = "init", length = 1)]
    pub fn init_static(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        init(scope, this, options)
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
    #[method(length = 1)]
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
                if crate::support::is_str(scope, key, c"removeListener")? {
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

    /// <https://nodejs.org/api/events.html#emitteremiteventname-args>: once `node:domain` is
    /// loaded, the emitter's domain handles the event as in
    /// <https://nodejs.org/api/domain.html#explicit-binding>.
    #[method(length = 1)]
    pub fn emit(
        scope: &Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        if crate::domain::in_use() {
            if let Some(emitted) = crate::domain::emit(scope, this, name, &args)? {
                return Ok(emitted);
            }
        }
        emit_event(scope, this, name, &args)
    }

    /// <https://nodejs.org/api/events.html#emittersetmaxlistenersn>
    pub fn set_max_listeners<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        n: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        crate::errors::validate_number(scope, n, "setMaxListeners", 0.0)?;
        this.set_property(scope, c"_maxListeners", n)?;
        Ok(this)
    }

    /// <https://nodejs.org/api/events.html#emittergetmaxlisteners>
    pub fn get_max_listeners<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
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
    #[method(length = 2)]
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

    /// <https://nodejs.org/api/events.html#emitter-eventnames>: keys whose value is `undefined`,
    /// which `removeListener` leaves on an emitter in shape mode, are skipped.
    pub fn event_names<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<js::Array<'s>, Thrown> {
        let mut names = Vec::new();
        if let Some(events) = events_object(scope, this)? {
            if events_count(scope, this)? > 0.0 {
                for key in events.own_keys(scope)? {
                    let id = event_key(scope, key)?;
                    if !events.get_property_by_id(scope, id)?.is_undefined() {
                        names.push(key);
                    }
                }
            }
        }
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
        let getter = Function::new_callback(scope, c"get", 0, get_default_max_listeners, ())?;
        let setter = Function::new_callback(scope, c"set", 1, set_default_max_listeners, ())?;
        ctor.define_accessor(
            scope,
            c"defaultMaxListeners",
            Some(*getter),
            Some(*setter),
            (js::class_spec::JSPROP_ENUMERATE | js::class_spec::JSPROP_PERMANENT) as u32,
        )?;
        ctor.set_property(scope, c"errorMonitor", ERROR_MONITOR.value(scope)?)?;
        ctor.set_property(scope, c"captureRejectionSymbol", REJECTION.value(scope)?)?;
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
    let Some(prototype) = js::class::get_prototype_object_for::<
        web_globals::events::event_target::EventTargetImpl,
    >(scope) else {
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

/// The callbacks of the listeners of `target`, an object [`is_event_target`] accepts, for the
/// event type `name`. An object that inherits from `EventTarget.prototype` without being an
/// `EventTarget` has none, and a symbol `name` has none.
fn event_target_listeners<'s>(
    scope: &'s Scope<'_>,
    target: HandleValue<'_>,
    name: HandleValue<'_>,
) -> Result<Vec<Object<'s>>, ExnThrown> {
    let Ok(target) = web_globals::events::event_target::EventTarget::from_jsval(scope, target, ())
    else {
        return Ok(Vec::new());
    };
    if name.is_symbol() {
        return Ok(Vec::new());
    }
    let name = js::JSString::from_value(scope, name)?.to_utf8(scope)?;
    Ok(target.listeners_for(scope, &name))
}

/// The key of the listener limit `setMaxListeners` stores on an `EventTarget`.
fn event_target_limit_key<'s>(scope: &'s Scope<'_>) -> Result<HandleId<'s>, ExnThrown> {
    MAX_EVENT_TARGET_LISTENERS.id(scope)
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
    mirror = "errorMonitor, captureRejectionSymbol, captureRejections, defaultMaxListeners, usingDomains, init"
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
        if is_event_target(scope, emitter)? {
            let listeners = event_target_listeners(scope, emitter, name)?;
            return Ok(js::Array::with_contents(scope, &listeners)?.to_jsval_throwing(scope)?);
        }
        Err(not_an_emitter(scope, "emitter", emitter))
    }

    /// <https://nodejs.org/api/events.html#eventsgetmaxlistenersemitterortarget>
    pub fn get_max_listeners<'s>(
        scope: &'s Scope<'_>,
        emitter: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        if let Some(object) = with_method(scope, emitter, c"getMaxListeners")? {
            return Ok(max_listeners(scope, object)?);
        }
        if is_event_target(scope, emitter)? {
            let target = Object::from_value(scope, *emitter)?;
            let limit = target.get_property_by_id(scope, event_target_limit_key(scope)?)?;
            if limit.is_number() {
                return Ok(limit);
            }
            return Ok(scope.root_value(js::value::from_f64(DEFAULT_MAX_LISTENERS.get())));
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
        let n = match n.filter(|n| !n.is_undefined()) {
            Some(n) => n,
            None => scope.root_value(js::value::from_f64(DEFAULT_MAX_LISTENERS.get())),
        };
        let number = crate::errors::validate_number(scope, n, "setMaxListeners", 0.0)?;
        if targets.is_empty() {
            DEFAULT_MAX_LISTENERS.set(number);
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
    pub fn listener_count<'s>(
        scope: &'s Scope<'_>,
        emitter: HandleValue<'_>,
        name: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        if let Some(object) = with_method(scope, emitter, c"listenerCount")? {
            let count = Function::call_by_name(scope, object.handle(), c"listenerCount", &[name])?;
            return Ok(scope.root_value(count.get()));
        }
        if is_event_target(scope, emitter)? {
            let count = event_target_listeners(scope, emitter, name)?.len() as f64;
            return Ok(scope.root_value(js::value::from_f64(count)));
        }
        Err(not_an_emitter(scope, "emitter", emitter))
    }
}
