// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Destroying and constructing streams: Node's `lib/internal/streams/destroy.js`.

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use super::util::{
    as_object, call, call_method, closure, emit, error_value, has_method, is_destroyed,
    is_finished, is_function, listener_count, next_tick, states, take_exception, truthy,
    type_error, Captures, Props,
};
use crate::errors::{ErrorKind, NodeError};

/// The event a stream emits once a `destroy()` called before its construction finished can run.
pub(crate) fn destroy_event<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    crate::symbol(scope, "nodejs.stream.kDestroy", true)
}

/// The event a stream emits once its `_construct` succeeded.
pub(crate) fn construct_event<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    crate::symbol(scope, "nodejs.stream.kConstruct", true)
}

/// Record `error`, if it is truthy, as the error of each state that has none yet.
fn check_error(
    scope: &Scope<'_>,
    error: HandleValue<'_>,
    writable: Option<Object<'_>>,
    readable: Option<Object<'_>>,
) -> Result<(), ExnThrown> {
    if !truthy(scope, error) {
        return Ok(());
    }
    for state in [writable, readable].into_iter().flatten() {
        if !truthy(scope, state.get_property(scope, c"errored")?) {
            state.set_property(scope, c"errored", error)?;
        }
    }
    Ok(())
}

/// `ERR_MULTIPLE_CALLBACK`.
pub(crate) fn multiple_callback() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_MULTIPLE_CALLBACK",
        "Callback called multiple times",
    )
}

/// `ERR_STREAM_DESTROYED` for `method`.
pub(crate) fn stream_destroyed(method: &str) -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_STREAM_DESTROYED",
        format!("Cannot call {method} after a stream was destroyed"),
    )
}

/// Node's `aggregateTwoErrors`: if both are truthy and differ, `outer` with `inner` appended to its
/// `errors` array if it has one, or else a new `AggregateError` of both with `outer`'s `message`
/// and `code`. Otherwise `inner` if it is truthy, and `outer` if not.
pub(crate) fn aggregate_two_errors<'s>(
    scope: &'s Scope<'_>,
    inner: HandleValue<'_>,
    outer: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    if truthy(scope, inner) && truthy(scope, outer) && !super::util::same(scope, inner, outer)? {
        let outer_object = as_object(scope, outer);
        if let Some(errors) = outer_object
            .map(|outer| outer.obj(scope, c"errors"))
            .transpose()?
            .flatten()
            .filter(|errors| js::Array::is_array(scope, errors.handle()).unwrap_or(false))
        {
            call_method(scope, errors, c"push", &[inner])?;
            return Ok(scope.root_value(outer.get()));
        }
        let ctor = scope
            .global()
            .obj(scope, c"AggregateError")?
            .ok_or_else(|| type_error(scope, "AggregateError is not a constructor"))?;
        let (message, code) = match outer_object {
            Some(outer) => (outer.val(scope, c"message")?, outer.val(scope, c"code")?),
            None => (HandleValue::undefined(), HandleValue::undefined()),
        };
        let list = js::Array::with_contents(scope, &[outer.get(), inner.get()])?;
        let list = (*list).to_jsval_throwing(scope)?;
        let aggregate = js::Function::construct(scope, ctor, &[list, message])?;
        aggregate.set_property(scope, c"code", code)?;
        return aggregate.to_jsval_throwing(scope);
    }
    Ok(scope.root_value(if truthy(scope, inner) {
        inner.get()
    } else {
        outer.get()
    }))
}

/// `stream.destroy(err, cb)`, the `destroy` of `Readable` and `Writable`.
pub(crate) fn destroy<'s>(
    scope: &Scope<'_>,
    stream: Object<'s>,
    error: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let (readable, writable) = states(scope, stream)?;
    let destroyed = |state: Option<Object<'_>>| -> Result<bool, ExnThrown> {
        Ok(match state {
            Some(state) => state.flag(scope, c"destroyed")?,
            None => false,
        })
    };
    if destroyed(writable)? || destroyed(readable)? {
        if is_function(scope, callback) {
            call(scope, HandleValue::undefined(), callback, &[])?;
        }
        return Ok(stream);
    }
    check_error(scope, error, writable, readable)?;
    for state in [writable, readable].into_iter().flatten() {
        state.set_flag(scope, c"destroyed", true)?;
    }
    let state = writable
        .or(readable)
        .ok_or_else(|| type_error(scope, "not a stream"))?;
    if !state.flag(scope, c"constructed")? {
        let deferred = closure(
            scope,
            1,
            destroy_after_construct,
            &[
                stream.to_jsval_raw_throwing(scope)?,
                error.get(),
                callback.get(),
            ],
        )?;
        let event = destroy_event(scope)?;
        call_method(
            scope,
            stream,
            c"once",
            &[event, deferred.to_jsval_throwing(scope)?],
        )?;
    } else {
        destroy_now(scope, stream, error, callback)?;
    }
    Ok(stream)
}

/// The listener for the destroy event, whose captures are `[stream, err, cb]`: destroy the stream
/// with the construction error and `err` combined.
fn destroy_after_construct(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let error = aggregate_two_errors(scope, args.get(0), captures.get(1)?)?;
    destroy_now(scope, captures.obj(0)?, error, captures.get(2)?)?;
    Ok(js::value::undefined())
}

/// Node's `_destroy`: call the stream's `_destroy` with a callback that marks it closed, calls
/// `callback`, and emits `error` and `close` in a microtask.
fn destroy_now(
    scope: &Scope<'_>,
    stream: Object<'_>,
    error: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let on_destroy = closure(
        scope,
        1,
        on_destroy,
        &[
            stream.to_jsval_raw_throwing(scope)?,
            callback.get(),
            js::value::from_bool(false),
        ],
    )?;
    let error = if truthy(scope, error) {
        error
    } else {
        HandleValue::null()
    };
    let on_destroy_value = on_destroy.to_jsval_throwing(scope)?;
    if call_method(scope, stream, c"_destroy", &[error, on_destroy_value]).is_err() {
        let thrown = take_exception(scope);
        call(scope, HandleValue::undefined(), on_destroy_value, &[thrown])?;
    }
    Ok(())
}

/// The callback `_destroy` gets, whose captures are `[stream, cb, called]`.
fn on_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    if captures.flag(2)? {
        return Ok(js::value::undefined());
    }
    captures.set(2, true)?;
    let stream = captures.obj(0)?;
    let error = args.get(0);
    let (readable, writable) = states(scope, stream)?;
    check_error(scope, error, writable, readable)?;
    for state in [writable, readable].into_iter().flatten() {
        state.set_flag(scope, c"closed", true)?;
    }
    let callback = captures.get(1)?;
    if is_function(scope, callback) {
        call(scope, HandleValue::undefined(), callback, &[error])?;
    }
    if truthy(scope, error) {
        next_tick(
            scope,
            emit_error_close_tick,
            &[stream.to_jsval_raw_throwing(scope)?, error.get()],
        )?;
    } else {
        next_tick(
            scope,
            emit_close_tick,
            &[stream.to_jsval_raw_throwing(scope)?],
        )?;
    }
    Ok(js::value::undefined())
}

/// Emit `error` and then `close` on the captured stream.
fn emit_error_close_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let stream = captures.obj(0)?;
    emit_error(scope, stream, captures.get(1)?)?;
    emit_close(scope, stream)?;
    Ok(js::value::undefined())
}

/// Emit `close` on the captured stream.
fn emit_close_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    emit_close(scope, Captures::of(scope, payload)?.obj(0)?)?;
    Ok(js::value::undefined())
}

/// Emit `error` on the captured stream.
fn emit_error_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    emit_error(scope, captures.obj(0)?, captures.get(1)?)?;
    Ok(js::value::undefined())
}

/// Node's `emitCloseNT`: mark `close` emitted, and emit it if either side has `emitClose` set.
fn emit_close(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    let (readable, writable) = states(scope, stream)?;
    let mut wanted = false;
    for state in [writable, readable].into_iter().flatten() {
        state.set_flag(scope, c"closeEmitted", true)?;
        wanted |= state.flag(scope, c"emitClose")?;
    }
    if wanted {
        emit(scope, stream, "close", &[])?;
    }
    Ok(())
}

/// Node's `emitErrorNT`: emit `error` unless either side emitted one already.
fn emit_error(
    scope: &Scope<'_>,
    stream: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let (readable, writable) = states(scope, stream)?;
    for state in [writable, readable].into_iter().flatten() {
        if state.flag(scope, c"errorEmitted")? {
            return Ok(());
        }
    }
    for state in [writable, readable].into_iter().flatten() {
        state.set_flag(scope, c"errorEmitted", true)?;
    }
    emit(scope, stream, "error", &[error])?;
    Ok(())
}

/// `stream._undestroy()`: reset both sides to their state before `destroy()`.
pub(crate) fn undestroy(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    let (readable, writable) = states(scope, stream)?;
    if let Some(r) = readable {
        let disabled = r.get_property(scope, c"readable")?;
        let disabled = disabled.is_boolean() && !disabled.to_boolean();
        for (name, value) in [
            (c"constructed", true),
            (c"closed", false),
            (c"closeEmitted", false),
            (c"destroyed", false),
            (c"errorEmitted", false),
            (c"reading", false),
            (c"ended", disabled),
            (c"endEmitted", disabled),
        ] {
            r.set_flag(scope, name, value)?;
        }
        r.set_property(scope, c"errored", js::value::null())?;
    }
    if let Some(w) = writable {
        let disabled = w.get_property(scope, c"writable")?;
        let disabled = disabled.is_boolean() && !disabled.to_boolean();
        for (name, value) in [
            (c"constructed", true),
            (c"destroyed", false),
            (c"closed", false),
            (c"closeEmitted", false),
            (c"errorEmitted", false),
            (c"finalCalled", false),
            (c"prefinished", false),
            (c"ended", disabled),
            (c"ending", disabled),
            (c"finished", disabled),
        ] {
            w.set_flag(scope, name, value)?;
        }
        w.set_property(scope, c"errored", js::value::null())?;
    }
    Ok(())
}

/// Node's `errorOrDestroy`: destroy `stream` with `error` if either side auto-destroys, and
/// otherwise record the error and emit it, in a microtask if `sync`.
pub(crate) fn error_or_destroy(
    scope: &Scope<'_>,
    stream: Object<'_>,
    error: HandleValue<'_>,
    sync: bool,
) -> Result<(), ExnThrown> {
    let (readable, writable) = states(scope, stream)?;
    for state in [writable, readable].into_iter().flatten() {
        if state.flag(scope, c"destroyed")? {
            return Ok(());
        }
    }
    let mut auto_destroy = false;
    for state in [readable, writable].into_iter().flatten() {
        auto_destroy |= state.flag(scope, c"autoDestroy")?;
    }
    if auto_destroy {
        call_method(scope, stream, c"destroy", &[error])?;
    } else if truthy(scope, error) {
        for state in [writable, readable].into_iter().flatten() {
            if !truthy(scope, state.get_property(scope, c"errored")?) {
                state.set_property(scope, c"errored", error)?;
            }
        }
        if sync {
            next_tick(
                scope,
                emit_error_tick,
                &[stream.to_jsval_raw_throwing(scope)?, error.get()],
            )?;
        } else {
            emit_error(scope, stream, error)?;
        }
    }
    Ok(())
}

/// [`error_or_destroy`] with a Node error.
pub(crate) fn error_or_destroy_with(
    scope: &Scope<'_>,
    stream: Object<'_>,
    error: NodeError,
    sync: bool,
) -> Result<(), ExnThrown> {
    let error = error_value(scope, error)?;
    error_or_destroy(scope, stream, error, sync)
}

/// Node's `construct`: if the stream has a `_construct`, mark both sides unconstructed, and run it
/// in a microtask, calling `on_constructed` once it succeeds.
pub(crate) fn construct(
    scope: &Scope<'_>,
    stream: Object<'_>,
    on_constructed: Object<'_>,
) -> Result<(), ExnThrown> {
    if !has_method(scope, stream, c"_construct")? {
        return Ok(());
    }
    let (readable, writable) = states(scope, stream)?;
    for state in [readable, writable].into_iter().flatten() {
        state.set_flag(scope, c"constructed", false)?;
    }
    let event = construct_event(scope)?;
    call_method(
        scope,
        stream,
        c"once",
        &[event, on_constructed.to_jsval_throwing(scope)?],
    )?;
    if listener_count(scope, stream, event)? > 1.0 {
        return Ok(());
    }
    next_tick(
        scope,
        construct_tick,
        &[stream.to_jsval_raw_throwing(scope)?],
    )
}

/// Node's `constructNT`: call `_construct` with a callback that settles the construction in a
/// microtask.
fn construct_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let on_construct = closure(
        scope,
        1,
        on_construct,
        &[
            stream.to_jsval_raw_throwing(scope)?,
            js::value::from_bool(false),
        ],
    )?;
    let deferred = closure(
        scope,
        1,
        defer_construct,
        &[on_construct.to_jsval_raw_throwing(scope)?],
    )?;
    let deferred = deferred.to_jsval_throwing(scope)?;
    if call_method(scope, stream, c"_construct", &[deferred]).is_err() {
        let thrown = take_exception(scope);
        next_tick(
            scope,
            call_on_construct,
            &[on_construct.to_jsval_raw_throwing(scope)?, thrown.get()],
        )?;
    }
    Ok(js::value::undefined())
}

/// The callback `_construct` gets, whose captures are `[onConstruct]`: settle in a microtask.
fn defer_construct(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let on_construct = Captures::of(scope, payload)?.get(0)?;
    next_tick(
        scope,
        call_on_construct,
        &[on_construct.get(), args.get(0).get()],
    )?;
    Ok(js::value::undefined())
}

/// Call the captured `onConstruct` with the captured error.
fn call_on_construct(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    call(
        scope,
        HandleValue::undefined(),
        captures.get(0)?,
        &[captures.get(1)?],
    )?;
    Ok(js::value::undefined())
}

/// Node's `onConstruct`, whose captures are `[stream, called]`.
fn on_construct(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let stream = captures.obj(0)?;
    let error = args.get(0);
    if captures.flag(1)? {
        if truthy(scope, error) {
            error_or_destroy(scope, stream, error, false)?;
        } else {
            error_or_destroy_with(scope, stream, multiple_callback(), false)?;
        }
        return Ok(js::value::undefined());
    }
    captures.set(1, true)?;
    let (readable, writable) = states(scope, stream)?;
    for state in [readable, writable].into_iter().flatten() {
        state.set_flag(scope, c"constructed", true)?;
    }
    let state = writable
        .or(readable)
        .ok_or_else(|| type_error(scope, "not a stream"))?;
    if state.flag(scope, c"destroyed")? {
        let event = destroy_event(scope)?;
        call_method(scope, stream, c"emit", &[event, error])?;
    } else if truthy(scope, error) {
        error_or_destroy(scope, stream, error, true)?;
    } else {
        let event = construct_event(scope)?;
        call_method(scope, stream, c"emit", &[event])?;
    }
    Ok(js::value::undefined())
}

/// Emit `close` on the captured legacy stream.
fn emit_close_legacy(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    emit(scope, Captures::of(scope, payload)?.obj(0)?, "close", &[])?;
    Ok(js::value::undefined())
}

/// Emit `error` and then, in a microtask, `close` on the captured legacy stream.
fn emit_error_close_legacy(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let stream = captures.obj(0)?;
    emit(scope, stream, "error", &[captures.get(1)?])?;
    next_tick(
        scope,
        emit_close_legacy,
        &[stream.to_jsval_raw_throwing(scope)?],
    )?;
    Ok(js::value::undefined())
}

/// Node's `isServerRequest`: whether `stream` has boolean `_consuming` and `_dumped` properties
/// and no `req.upgradeOrConnect`.
fn is_server_request(scope: &Scope<'_>, stream: Object<'_>) -> Result<bool, ExnThrown> {
    if !stream.get_property(scope, c"_consuming")?.is_boolean()
        || !stream.get_property(scope, c"_dumped")?.is_boolean()
    {
        return Ok(false);
    }
    Ok(match stream.obj(scope, c"req")? {
        Some(request) => request
            .get_property(scope, c"upgradeOrConnect")?
            .is_undefined(),
        None => true,
    })
}

/// Node's `destroyer`: destroy `stream` with `error`, or with an `AbortError` if it has no error and
/// hasn't finished, through whichever of `abort`, `destroy` and `close` it has. A server request is
/// destroyed after its `socket` is set to `null`.
pub(crate) fn destroyer(
    scope: &Scope<'_>,
    stream: HandleValue<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let Some(object) = as_object(scope, stream) else {
        return Ok(());
    };
    if is_destroyed(scope, stream)? == Some(true) {
        return Ok(());
    }
    let mut error = scope.root_value(error.get());
    if !truthy(scope, error) && is_finished(scope, stream)? != Some(true) {
        error = crate::errors::abort_error(scope, None)?;
    }
    let is_request = |object: Option<Object<'_>>| -> Result<bool, ExnThrown> {
        let Some(object) = object else {
            return Ok(false);
        };
        Ok(truthy(scope, object.get_property(scope, c"setHeader")?)
            && has_method(scope, object, c"abort")?)
    };
    let request = object.obj(scope, c"req")?;
    if is_server_request(scope, object)? {
        object.set_property(scope, c"socket", HandleValue::null())?;
        call_method(scope, object, c"destroy", &[error])?;
    } else if is_request(Some(object))? {
        call_method(scope, object, c"abort", &[] as &[HandleValue<'_>])?;
    } else if is_request(request)? {
        let request = request.expect("requests are objects");
        call_method(scope, request, c"abort", &[] as &[HandleValue<'_>])?;
    } else if has_method(scope, object, c"destroy")? {
        call_method(scope, object, c"destroy", &[error])?;
    } else if has_method(scope, object, c"close")? {
        call_method(scope, object, c"close", &[])?;
    } else if truthy(scope, error) {
        next_tick(
            scope,
            emit_error_close_legacy,
            &[object.to_jsval_raw_throwing(scope)?, error.get()],
        )?;
    } else {
        next_tick(
            scope,
            emit_close_legacy,
            &[object.to_jsval_raw_throwing(scope)?],
        )?;
    }
    if !truthy(scope, object.get_property(scope, c"destroyed")?) {
        let marker = crate::symbol(scope, "nodejs.stream.destroyed", true)?;
        let key = scope.root_id(js::id::value_to_id(scope, marker)?);
        object.set_property_by_id(scope, key, true)?;
    }
    Ok(())
}
