// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Helpers the stream implementation shares: typed access to the state objects, closures over
//! captured values, `process.nextTick` scheduling, and the predicates of Node's
//! `lib/internal/streams/utils.js`.

use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::Callback;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::NodeError;

/// Typed access to the properties of a stream or state object.
pub(crate) trait Props {
    /// Whether `self[name]` is `true`.
    fn flag(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<bool, ExnThrown>;
    /// Set `self[name]` to `value`.
    fn set_flag(
        &self,
        scope: &Scope<'_>,
        name: &'static CStr,
        value: bool,
    ) -> Result<(), ExnThrown>;
    /// `self[name]` if it is a number, and 0 otherwise.
    fn num(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<f64, ExnThrown>;
    /// `self[name]` as an object, if it is one.
    fn obj<'s>(
        &self,
        scope: &'s Scope<'_>,
        name: &'static CStr,
    ) -> Result<Option<Object<'s>>, ExnThrown>;
    /// `self[name]`.
    fn val<'s>(
        &self,
        scope: &'s Scope<'_>,
        name: &'static CStr,
    ) -> Result<HandleValue<'s>, ExnThrown>;
}

impl Props for Object<'_> {
    fn flag(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<bool, ExnThrown> {
        Ok(is_true(self.get_property(scope, name)?))
    }

    fn set_flag(
        &self,
        scope: &Scope<'_>,
        name: &'static CStr,
        value: bool,
    ) -> Result<(), ExnThrown> {
        self.set_property(scope, name, value)
    }

    fn num(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<f64, ExnThrown> {
        let value = self.get_property(scope, name)?;
        Ok(if value.is_number() {
            value.to_number()
        } else {
            0.0
        })
    }

    fn obj<'s>(
        &self,
        scope: &'s Scope<'_>,
        name: &'static CStr,
    ) -> Result<Option<Object<'s>>, ExnThrown> {
        let value = self.get_property(scope, name)?;
        Ok(as_object(scope, value))
    }

    fn val<'s>(
        &self,
        scope: &'s Scope<'_>,
        name: &'static CStr,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        self.get_property(scope, name)
    }
}

/// Throw a `TypeError` with `message`.
pub(crate) fn type_error(scope: &Scope<'_>, message: &str) -> ExnThrown {
    js::error::TypeError(message.into()).throw(scope)
}

/// The `this` of a callback as an object, or a `TypeError` if it isn't one.
pub(crate) fn this_object<'s>(
    scope: &'s Scope<'_>,
    args: &js::function::CallbackArgs<'_>,
) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, args.this())
        .map_err(|_| type_error(scope, "the receiver is not an object"))
}

/// `object[name]` as an object, or a `TypeError` if it isn't one.
pub(crate) fn required_obj<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    name: &'static CStr,
) -> Result<Object<'s>, ExnThrown> {
    object.obj(scope, name)?.ok_or_else(|| {
        let name = name.to_str().unwrap_or_default();
        type_error(scope, &format!("{name} is not an object"))
    })
}

/// Whether `value` is `true`.
pub(crate) fn is_true(value: HandleValue<'_>) -> bool {
    value.is_boolean() && value.to_boolean()
}

/// Whether `value` is truthy, as JS's `!!value`.
pub(crate) fn truthy(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    bool::from_jsval(scope, value, ()).unwrap_or(false)
}

/// `value` as an object, if it is one.
pub(crate) fn as_object<'s>(scope: &'s Scope<'_>, value: HandleValue<'_>) -> Option<Object<'s>> {
    Object::from_value(scope, value.get()).ok()
}

/// Whether `value` is a function.
pub(crate) fn is_function(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    Object::from_value(scope, value).is_ok_and(|object| object.is_callable())
}

/// Whether `object[name]` is a function.
pub(crate) fn has_method(
    scope: &Scope<'_>,
    object: Object<'_>,
    name: &'static CStr,
) -> Result<bool, ExnThrown> {
    Ok(is_function(scope, object.get_property(scope, name)?))
}

/// The string `text` as a JS value.
pub(crate) fn string<'s>(scope: &'s Scope<'_>, text: &str) -> Result<HandleValue<'s>, ExnThrown> {
    js::JSString::from_str(scope, text)?.to_jsval_throwing(scope)
}

/// Whether `value` is the string `text`.
pub(crate) fn is_str(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    text: &CStr,
) -> Result<bool, ExnThrown> {
    if !value.is_string() {
        return Ok(false);
    }
    js::JSString::from_value(scope, value.get())?.equals_ascii(scope, text)
}

/// `value` as a Rust string, if it is a string.
pub(crate) fn rust_string(scope: &Scope<'_>, value: HandleValue<'_>) -> Option<String> {
    value
        .is_string()
        .then(|| String::from_jsval(scope, value, ()).ok())
        .flatten()
}

/// `a === b`.
pub(crate) fn same(
    scope: &Scope<'_>,
    a: HandleValue<'_>,
    b: HandleValue<'_>,
) -> Result<bool, ExnThrown> {
    js::comparison::strictly_equal(scope, a, b)
}

/// Call the method `name` of `object` with `args`.
pub(crate) fn call_method<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    name: &'static CStr,
    args: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    let result = Function::call_by_name(scope, object.handle(), name, args)?;
    Ok(scope.root_value(result.get()))
}

/// Call `callee` with `this` and `args`.
pub(crate) fn call<'s>(
    scope: &'s Scope<'_>,
    this: HandleValue<'_>,
    callee: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    let result = Function::call(scope, this, callee, args)?;
    Ok(scope.root_value(result.get()))
}

/// Emit `event` on `emitter` with `args`. Returns whether it had listeners.
pub(crate) fn emit(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    event: &str,
    args: &[HandleValue<'_>],
) -> Result<bool, ExnThrown> {
    let mut all = Vec::with_capacity(args.len() + 1);
    all.push(string(scope, event)?);
    all.extend_from_slice(args);
    Ok(truthy(scope, call_method(scope, emitter, c"emit", &all)?))
}

/// Call the method `name` of `emitter` with the event `event` and `listener`, such as `on` or
/// `removeListener`.
pub(crate) fn listen(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    name: &'static CStr,
    event: &str,
    listener: Object<'_>,
) -> Result<(), ExnThrown> {
    let event = string(scope, event)?;
    let listener = listener.to_jsval_throwing(scope)?;
    call_method(scope, emitter, name, &[event, listener])?;
    Ok(())
}

/// `emitter.listenerCount(event)`.
pub(crate) fn listener_count(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    event: HandleValue<'_>,
) -> Result<f64, ExnThrown> {
    let count = call_method(scope, emitter, c"listenerCount", &[event])?;
    Ok(if count.is_number() {
        count.to_number()
    } else {
        0.0
    })
}

/// The values a closure captured, in the array that is its payload. Closures update a captured
/// value by setting it.
pub(crate) struct Captures<'s> {
    scope: &'s Scope<'s>,
    pub(crate) array: Object<'s>,
}

impl<'s> Captures<'s> {
    /// The captures of the closure `payload` is the payload of.
    pub(crate) fn of(scope: &'s Scope<'s>, payload: HandleValue<'_>) -> Result<Self, ExnThrown> {
        let array = as_object(scope, payload)
            .ok_or_else(|| js::error::TypeError("not a closure".into()).throw(scope))?;
        Ok(Captures { scope, array })
    }

    /// The captured value at `index`.
    pub(crate) fn get(&self, index: u32) -> Result<HandleValue<'s>, ExnThrown> {
        self.array.get_element(self.scope, index)
    }

    /// The captured object at `index`.
    pub(crate) fn obj(&self, index: u32) -> Result<Object<'s>, ExnThrown> {
        as_object(self.scope, self.get(index)?)
            .ok_or_else(|| js::error::TypeError("not an object".into()).throw(self.scope))
    }

    /// Whether the captured value at `index` is `true`.
    pub(crate) fn flag(&self, index: u32) -> Result<bool, ExnThrown> {
        Ok(is_true(self.get(index)?))
    }

    /// The array of captured values.
    pub(crate) fn array_value(&self) -> Result<Value, ExnThrown> {
        self.array.to_jsval_raw_throwing(self.scope)
    }

    /// Replace the captured value at `index`.
    pub(crate) fn set(&self, index: u32, value: impl ToJSVal<'s>) -> Result<(), ExnThrown> {
        let value = value.to_jsval_throwing(self.scope)?;
        self.array.set_element(self.scope, index, value)
    }
}

/// A function running `body` with `captures` as its payload.
pub(crate) fn closure<'s>(
    scope: &'s Scope<'_>,
    nargs: u32,
    body: Callback,
    captures: &[impl ToJSVal<'s>],
) -> Result<Object<'s>, ExnThrown> {
    let payload = js::Array::with_contents(scope, captures)?;
    Ok(*Function::new_callback(scope, c"", nargs, body, *payload)?)
}

/// Run `body` with `captures` as its payload in the tick queue, as `process.nextTick` does.
pub(crate) fn next_tick(
    scope: &Scope<'_>,
    body: Callback,
    captures: &[Value],
) -> Result<(), ExnThrown> {
    let function = closure(scope, 0, body, captures)?;
    js::jobs::queue_tick(scope, function.to_jsval_throwing(scope)?, &[])
}

/// Call `callback` with `args` in the tick queue, if it is a function.
pub(crate) fn next_tick_call(
    scope: &Scope<'_>,
    callback: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    if !is_function(scope, callback) {
        return Ok(());
    }
    js::jobs::queue_tick(scope, callback, args)
}

/// `error` as a JS value instead of a thrown exception.
pub(crate) fn error_value<'s>(
    scope: &'s Scope<'_>,
    error: NodeError,
) -> Result<HandleValue<'s>, ExnThrown> {
    error.throw(scope);
    js::exception::take_pending(scope).ok_or(ExnThrown)
}

/// Take the pending exception.
pub(crate) fn take_exception<'s>(scope: &'s Scope<'_>) -> HandleValue<'s> {
    js::exception::take_pending_or_undefined(scope)
}

/// `stream._readableState` and `stream._writableState`, if they are objects.
pub(crate) fn states<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
) -> Result<(Option<Object<'s>>, Option<Object<'s>>), ExnThrown> {
    Ok((
        stream.obj(scope, c"_readableState")?,
        stream.obj(scope, c"_writableState")?,
    ))
}

/// Node's `isReadableNodeStream`.
pub(crate) fn is_readable_node_stream(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    strict: bool,
) -> Result<bool, ExnThrown> {
    let Some(stream) = as_object(scope, value) else {
        return Ok(false);
    };
    if !has_method(scope, stream, c"pipe")? || !has_method(scope, stream, c"on")? {
        return Ok(false);
    }
    if strict && (!has_method(scope, stream, c"pause")? || !has_method(scope, stream, c"resume")?) {
        return Ok(false);
    }
    let (readable, writable) = states(scope, stream)?;
    if writable.is_some() {
        let Some(readable) = readable else {
            return Ok(false);
        };
        let flag = readable.get_property(scope, c"readable")?;
        if flag.is_boolean() && !flag.to_boolean() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Node's `isWritableNodeStream`.
pub(crate) fn is_writable_node_stream(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<bool, ExnThrown> {
    let Some(stream) = as_object(scope, value) else {
        return Ok(false);
    };
    if !has_method(scope, stream, c"write")? || !has_method(scope, stream, c"on")? {
        return Ok(false);
    }
    let (readable, writable) = states(scope, stream)?;
    if readable.is_some() {
        if let Some(writable) = writable {
            let flag = writable.get_property(scope, c"writable")?;
            if flag.is_boolean() && !flag.to_boolean() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Node's `isNodeStream`.
pub(crate) fn is_node_stream(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    let Some(stream) = as_object(scope, value) else {
        return Ok(false);
    };
    let (readable, writable) = states(scope, stream)?;
    if readable.is_some() || writable.is_some() {
        return Ok(true);
    }
    let on = has_method(scope, stream, c"on")?;
    Ok(on && (has_method(scope, stream, c"write")? || has_method(scope, stream, c"pipe")?))
}

/// Node's `isDestroyed`, with `None` for a value that isn't a stream.
pub(crate) fn is_destroyed(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    if !is_node_stream(scope, value)? {
        return Ok(None);
    }
    let stream = as_object(scope, value).expect("streams are objects");
    if truthy(scope, stream.get_property(scope, c"destroyed")?) {
        return Ok(Some(true));
    }
    let marker = crate::symbol(scope, "nodejs.stream.destroyed", true)?;
    let key = scope.root_id(js::id::value_to_id(scope, marker)?);
    if truthy(scope, stream.get_property_by_id(scope, key)?) {
        return Ok(Some(true));
    }
    let (readable, writable) = states(scope, stream)?;
    match writable.or(readable) {
        Some(state) => Ok(Some(state.flag(scope, c"destroyed")?)),
        None => Ok(Some(false)),
    }
}

/// Node's `isWritableEnded`.
pub(crate) fn is_writable_ended(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    if !is_writable_node_stream(scope, value)? {
        return Ok(None);
    }
    let stream = as_object(scope, value).expect("streams are objects");
    if is_true(stream.get_property(scope, c"writableEnded")?) {
        return Ok(Some(true));
    }
    let Some(state) = stream.obj(scope, c"_writableState")? else {
        return Ok(None);
    };
    if truthy(scope, state.get_property(scope, c"errored")?) {
        return Ok(Some(false));
    }
    let ended = state.get_property(scope, c"ended")?;
    Ok(ended.is_boolean().then(|| ended.to_boolean()))
}

/// Node's `isWritableFinished`.
pub(crate) fn is_writable_finished(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    strict: bool,
) -> Result<Option<bool>, ExnThrown> {
    if !is_writable_node_stream(scope, value)? {
        return Ok(None);
    }
    let stream = as_object(scope, value).expect("streams are objects");
    if is_true(stream.get_property(scope, c"writableFinished")?) {
        return Ok(Some(true));
    }
    let Some(state) = stream.obj(scope, c"_writableState")? else {
        return Ok(None);
    };
    if truthy(scope, state.get_property(scope, c"errored")?) {
        return Ok(Some(false));
    }
    let finished = state.get_property(scope, c"finished")?;
    if !finished.is_boolean() {
        return Ok(None);
    }
    Ok(Some(
        finished.to_boolean()
            || (!strict && state.flag(scope, c"ended")? && state.num(scope, c"length")? == 0.0),
    ))
}

/// Node's `isReadableFinished`.
pub(crate) fn is_readable_finished(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    strict: bool,
) -> Result<Option<bool>, ExnThrown> {
    if !is_readable_node_stream(scope, value, false)? {
        return Ok(None);
    }
    let stream = as_object(scope, value).expect("streams are objects");
    let Some(state) = stream.obj(scope, c"_readableState")? else {
        return Ok(None);
    };
    if truthy(scope, state.get_property(scope, c"errored")?) {
        return Ok(Some(false));
    }
    let end_emitted = state.get_property(scope, c"endEmitted")?;
    if !end_emitted.is_boolean() {
        return Ok(None);
    }
    Ok(Some(
        end_emitted.to_boolean()
            || (!strict && state.flag(scope, c"ended")? && state.num(scope, c"length")? == 0.0),
    ))
}

/// Node's `isReadable`.
pub(crate) fn is_readable(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    let Some(stream) = as_object(scope, value) else {
        return Ok(None);
    };
    let readable = stream.get_property(scope, c"readable")?;
    if !readable.is_boolean() {
        return Ok(None);
    }
    if is_destroyed(scope, value)? == Some(true) {
        return Ok(Some(false));
    }
    Ok(Some(
        is_readable_node_stream(scope, value, false)?
            && readable.to_boolean()
            && is_readable_finished(scope, value, true)? != Some(true),
    ))
}

/// Node's `isWritable`.
pub(crate) fn is_writable(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    let Some(stream) = as_object(scope, value) else {
        return Ok(None);
    };
    let writable = stream.get_property(scope, c"writable")?;
    if !writable.is_boolean() {
        return Ok(None);
    }
    if is_destroyed(scope, value)? == Some(true) {
        return Ok(Some(false));
    }
    Ok(Some(
        is_writable_node_stream(scope, value)?
            && writable.to_boolean()
            && is_writable_ended(scope, value)? != Some(true),
    ))
}

/// Node's `isFinished`, for both sides of a stream.
pub(crate) fn is_finished(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    if !is_node_stream(scope, value)? {
        return Ok(None);
    }
    if is_destroyed(scope, value)? == Some(true) {
        return Ok(Some(true));
    }
    if is_readable(scope, value)? == Some(true) || is_writable(scope, value)? == Some(true) {
        return Ok(Some(false));
    }
    Ok(Some(true))
}

/// Node's `isWritableErrored` or `isReadableErrored`: the error of the side whose state is
/// `state_name`, or `None`.
pub(crate) fn side_errored<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    property: &'static CStr,
    state_name: &'static CStr,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let direct = stream.val(scope, property)?;
    if truthy(scope, direct) {
        return Ok(Some(direct));
    }
    let Some(state) = stream.obj(scope, state_name)? else {
        return Ok(None);
    };
    let errored = state.val(scope, c"errored")?;
    Ok((!errored.is_null() && !errored.is_undefined()).then_some(errored))
}

/// Node's `isClosed`.
pub(crate) fn is_closed(scope: &Scope<'_>, stream: Object<'_>) -> Result<Option<bool>, ExnThrown> {
    let closed = stream.get_property(scope, c"closed")?;
    if closed.is_boolean() {
        return Ok(Some(closed.to_boolean()));
    }
    let (readable, writable) = states(scope, stream)?;
    let side = |state: Option<Object<'_>>| -> Result<Option<bool>, ExnThrown> {
        let Some(state) = state else {
            return Ok(None);
        };
        let closed = state.get_property(scope, c"closed")?;
        Ok(closed.is_boolean().then(|| closed.to_boolean()))
    };
    let (w, r) = (side(writable)?, side(readable)?);
    if w.is_some() || r.is_some() {
        return Ok(Some(w == Some(true) || r == Some(true)));
    }
    Ok(None)
}

/// Node's `willEmitClose`.
pub(crate) fn will_emit_close(scope: &Scope<'_>, stream: Object<'_>) -> Result<bool, ExnThrown> {
    let (readable, writable) = states(scope, stream)?;
    let Some(state) = writable.or(readable) else {
        return Ok(false);
    };
    let closed = state.get_property(scope, c"closed")?;
    Ok(state.flag(scope, c"autoDestroy")?
        && state.flag(scope, c"emitClose")?
        && closed.is_boolean()
        && !closed.to_boolean())
}

/// Whether `value` has a `Symbol.asyncIterator` method, or a `Symbol.iterator` method if
/// `include_sync`.
pub(crate) fn is_iterable(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    include_sync: bool,
) -> Result<bool, ExnThrown> {
    if value.is_null() || value.is_undefined() {
        return Ok(false);
    }
    let Some(object) = as_object(scope, value) else {
        return Ok(include_sync && value.is_string());
    };
    let codes = if include_sync {
        &[
            js::native::SymbolCode::asyncIterator,
            js::native::SymbolCode::iterator,
        ][..]
    } else {
        &[js::native::SymbolCode::asyncIterator][..]
    };
    for &code in codes {
        let key = js::symbol::get_well_known_key(scope, code);
        if is_function(scope, object.get_property_by_id(scope, scope.root_id(key))?) {
            return Ok(true);
        }
    }
    Ok(false)
}
