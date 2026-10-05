// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Destroying and constructing streams: Node's `lib/internal/streams/destroy.js`.

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use super::state::{ReadableSide, WritableSide, R, W};
use super::util::{is_destroyed, is_finished, sides, Field, Side};
use crate::errors::{ErrorKind, NodeError};
use crate::events::listener_count;
use crate::support::has_method;

/// Node's private `kDestroy`, the event a stream emits once a `destroy()` called before its
/// construction finished can run.
static K_DESTROY: js::symbol::CachedSymbol = js::symbol::CachedSymbol::new(c"kDestroy");

/// Node's private `kConstruct`, the event a stream emits once its `_construct` succeeded.
static K_CONSTRUCT: js::symbol::CachedSymbol = js::symbol::CachedSymbol::new(c"kConstruct");

/// The readable and writable parts of the state of `stream`, if it has one.
fn parts<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'s>,
) -> Result<(Option<ReadableSide<'s>>, Option<WritableSide<'s>>), ExnThrown> {
    Ok(match super::state::try_state(scope, stream)? {
        Some(state) => (ReadableSide::of_state(state), WritableSide::of_state(state)),
        None => (None, None),
    })
}

/// Record `error`, if it is truthy, as the error of each part that has none yet.
fn check_error(
    error: HandleValue<'_>,
    readable: Option<ReadableSide<'_>>,
    writable: Option<WritableSide<'_>>,
) {
    if !js::conversion::to_boolean(error) {
        return;
    }
    if let Some(w) = writable.filter(|w| !w.is_errored()) {
        w.set_errored(error);
    }
    if let Some(r) = readable.filter(|r| !r.is_errored()) {
        r.set_errored(error);
    }
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
    if js::conversion::to_boolean(inner)
        && js::conversion::to_boolean(outer)
        && !js::comparison::strictly_equal(scope, inner, outer)?
    {
        let outer_object = Object::from_value(scope, outer).ok();
        if let Some(outer) = outer_object {
            let errors = outer.get_property(scope, c"errors")?;
            if let Some(errors) = Object::from_value(scope, errors)
                .ok()
                .and_then(|errors| errors.cast::<js::Array>().ok())
            {
                errors.push(scope, inner)?;
                return Ok(scope.root_value(outer.as_value()));
            }
        }
        let key = js::class_spec::JSProtoKey::JSProto_AggregateError;
        let ctor =
            Object::from_handle(js::class::get_class_object(scope, key)?).ok_or(ExnThrown)?;
        let (message, code) = match outer_object {
            Some(outer) => (
                outer.get_property(scope, c"message")?,
                outer.get_property(scope, c"code")?,
            ),
            None => (HandleValue::undefined(), HandleValue::undefined()),
        };
        let list = js::Array::with_contents(scope, &[outer.get(), inner.get()])?;
        let list = (*list).to_jsval_throwing(scope)?;
        let aggregate = js::Function::construct(scope, ctor, &[list, message])?;
        aggregate.set_property(scope, c"code", code)?;
        return aggregate.to_jsval_throwing(scope);
    }
    Ok(scope.root_value(if js::conversion::to_boolean(inner) {
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
    let (readable, writable) = parts(scope, stream)?;
    if writable.is_some_and(|w| w.has(W::DESTROYED))
        || readable.is_some_and(|r| r.has(R::DESTROYED))
    {
        if js::function::is_callable(callback) {
            js::Function::call(
                scope,
                HandleValue::undefined(),
                callback,
                js::function::EmptyArgs,
            )?;
        }
        return Ok(stream);
    }
    check_error(error, readable, writable);
    if let Some(w) = writable {
        w.set(W::DESTROYED, true);
    }
    if let Some(r) = readable {
        r.set(R::DESTROYED, true);
    }
    let constructed = match (writable, readable) {
        (Some(w), _) => w.has(W::CONSTRUCTED),
        (None, Some(r)) => r.has(R::CONSTRUCTED),
        (None, None) => true,
    };
    if !constructed {
        let request = new_destroy_request(scope, stream, error, callback)?;
        let deferred = js::Function::new_callback(scope, c"", 1, destroy_after_construct, request)?;
        js::Function::call_by_name(
            scope,
            stream.handle(),
            c"once",
            &[
                K_DESTROY.value(scope)?,
                (*deferred).to_jsval_throwing(scope)?,
            ],
        )?;
    } else {
        destroy_now(scope, stream, error, callback)?;
    }
    Ok(stream)
}

/// A call of a stream's `_destroy`: the stream, the error it destroys the stream with, the
/// callback of `destroy()`, and whether `_destroy` called back.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DestroyRequest {
    stream: Heap<js::object::Object>,
    error: Heap<Value>,
    callback: Heap<Value>,
    #[no_trace]
    called: bool,
}

#[core_runtime::jsmethods]
impl DestroyRequest {}

/// A new [`DestroyRequest`].
fn new_destroy_request<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    error: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<DestroyRequest<'s>, ExnThrown> {
    js::class::create_instance_with::<DestroyRequestImpl>(scope, |_| DestroyRequestImpl {
        stream: Heap::from(stream),
        error: Heap::from(error.get()),
        callback: Heap::from(callback.get()),
        called: false,
    })
}

/// The listener for the destroy event, whose payload is a [`DestroyRequest`]: destroy the stream
/// with the construction error and the request's error combined.
fn destroy_after_construct(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = cast_payload::<DestroyRequest>(scope, payload);
    let (stream, error, callback) = {
        let data = request.data();
        (
            data.stream.get(scope),
            data.error.get(scope),
            data.callback.get(scope),
        )
    };
    let error = aggregate_two_errors(scope, args.get(0), error)?;
    destroy_now(scope, stream, error, callback)?;
    Ok(js::value::undefined())
}

/// Node's `_destroy`: call the stream's `_destroy` with a callback that marks it closed, calls
/// `callback`, and emits `error` and `close` in a tick.
fn destroy_now(
    scope: &Scope<'_>,
    stream: Object<'_>,
    error: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let request = new_destroy_request(scope, stream, error, callback)?;
    let on_destroy = js::Function::new_callback(scope, c"", 1, on_destroy, request)?;
    let error = if js::conversion::to_boolean(error) {
        error
    } else {
        HandleValue::null()
    };
    let on_destroy_value = (*on_destroy).to_jsval_throwing(scope)?;
    if js::Function::call_by_name(
        scope,
        stream.handle(),
        c"_destroy",
        &[error, on_destroy_value],
    )
    .is_err()
    {
        let thrown = js::exception::take_pending_or_undefined(scope);
        js::Function::call(scope, HandleValue::undefined(), on_destroy_value, &[thrown])?;
    }
    Ok(())
}

/// The callback `_destroy` gets, whose payload is a [`DestroyRequest`].
fn on_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = cast_payload::<DestroyRequest>(scope, payload);
    if request.data().called {
        return Ok(js::value::undefined());
    }
    request.data_mut().called = true;
    let (stream, callback) = {
        let data = request.data();
        (data.stream.get(scope), data.callback.get(scope))
    };
    let error = args.get(0);
    let (readable, writable) = parts(scope, stream)?;
    check_error(error, readable, writable);
    if let Some(w) = writable {
        w.set(W::CLOSED, true);
    }
    if let Some(r) = readable {
        r.set(R::CLOSED, true);
    }
    if js::function::is_callable(callback) {
        js::Function::call(scope, HandleValue::undefined(), callback, &[error])?;
    }
    if js::conversion::to_boolean(error) {
        js::jobs::queue_native_tick(scope, emit_error_close_tick, stream, &[error])?;
    } else {
        js::jobs::queue_native_tick(scope, emit_close_tick, stream, &[])?;
    }
    Ok(js::value::undefined())
}

/// Node's `emitErrorCloseNT`, whose payload is the stream and whose argument is the error.
fn emit_error_close_tick(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    emit_error(scope, stream, args.get(0))?;
    emit_close(scope, stream)?;
    Ok(js::value::undefined())
}

/// Node's `emitCloseNT`, whose payload is the stream.
fn emit_close_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    emit_close(scope, cast_payload::<Object>(scope, payload))?;
    Ok(js::value::undefined())
}

/// Node's `emitErrorNT`, whose payload is the stream and whose argument is the error.
fn emit_error_tick(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    emit_error(scope, cast_payload::<Object>(scope, payload), args.get(0))?;
    Ok(js::value::undefined())
}

/// Node's `emitCloseNT`: mark `close` emitted, and emit it if either side has `emitClose` set.
fn emit_close(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    let (readable, writable) = parts(scope, stream)?;
    let mut wanted = false;
    if let Some(w) = writable {
        w.set(W::CLOSE_EMITTED, true);
        wanted |= w.has(W::EMIT_CLOSE);
    }
    if let Some(r) = readable {
        r.set(R::CLOSE_EMITTED, true);
        wanted |= r.has(R::EMIT_CLOSE);
    }
    if wanted {
        crate::events::emit(scope, stream, c"close", &[])?;
    }
    Ok(())
}

/// Node's `emitErrorNT`: emit `error` unless either side emitted one already.
fn emit_error(
    scope: &Scope<'_>,
    stream: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let (readable, writable) = parts(scope, stream)?;
    if writable.is_some_and(|w| w.has(W::ERROR_EMITTED))
        || readable.is_some_and(|r| r.has(R::ERROR_EMITTED))
    {
        return Ok(());
    }
    if let Some(w) = writable {
        w.set(W::ERROR_EMITTED, true);
    }
    if let Some(r) = readable {
        r.set(R::ERROR_EMITTED, true);
    }
    crate::events::emit(scope, stream, c"error", &[error])?;
    Ok(())
}

/// `stream._undestroy()`: reset both sides to their state before `destroy()`.
pub(crate) fn undestroy(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    let (readable, writable) = parts(scope, stream)?;
    if let Some(r) = readable {
        let disabled = r.disabled();
        let mut part = r.get_mut();
        part.flags.insert(R::CONSTRUCTED);
        part.flags
            .remove(R::CLOSED | R::CLOSE_EMITTED | R::DESTROYED | R::ERROR_EMITTED | R::READING);
        part.flags.set(R::ENDED | R::END_EMITTED, disabled);
        part.errored = None;
    }
    if let Some(w) = writable {
        let disabled = w.disabled();
        let mut part = w.get_mut();
        part.flags.insert(W::CONSTRUCTED);
        part.flags.remove(
            W::DESTROYED
                | W::CLOSED
                | W::CLOSE_EMITTED
                | W::ERROR_EMITTED
                | W::FINAL_CALLED
                | W::PREFINISHED,
        );
        part.flags.set(W::ENDED | W::ENDING | W::FINISHED, disabled);
        part.errored = None;
    }
    Ok(())
}

/// Node's `errorOrDestroy`: destroy `stream` with `error` if either side auto-destroys, and
/// otherwise record the error and emit it, in a tick if `sync`. The state of a stream of another
/// implementation never auto-destroys, as Node's reads of its bits find none.
pub(crate) fn error_or_destroy(
    scope: &Scope<'_>,
    stream: Object<'_>,
    error: HandleValue<'_>,
    sync: bool,
) -> Result<(), ExnThrown> {
    let sides = sides(scope, stream)?;
    for side in [sides.writable, sides.readable].into_iter().flatten() {
        if side.truthy(scope, Field::Destroyed)? {
            return Ok(());
        }
    }
    let auto_destroy =
        [sides.readable, sides.writable]
            .into_iter()
            .flatten()
            .any(|side| match side {
                Side::Readable(r) => r.has(R::AUTO_DESTROY),
                Side::Writable(w) => w.has(W::AUTO_DESTROY),
                Side::Foreign(_) => false,
            });
    if auto_destroy {
        js::Function::call_by_name(scope, stream.handle(), c"destroy", &[error])?;
    } else if js::conversion::to_boolean(error) {
        for side in [sides.writable, sides.readable].into_iter().flatten() {
            match side {
                Side::Readable(r) if !r.is_errored() => r.set_errored(error),
                Side::Writable(w) if !w.is_errored() => w.set_errored(error),
                Side::Foreign(state) => state.set_property(scope, c"errored", error)?,
                _ => {}
            }
        }
        if sync {
            js::jobs::queue_native_tick(scope, emit_error_tick, stream, &[error])?;
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
    let error = error.to_value(scope)?;
    error_or_destroy(scope, stream, error, sync)
}

/// Node's `construct`: if the stream has a `_construct`, mark both sides unconstructed, and run it
/// in a tick, calling `on_constructed` once it succeeds.
pub(crate) fn construct(
    scope: &Scope<'_>,
    stream: Object<'_>,
    on_constructed: Object<'_>,
) -> Result<(), ExnThrown> {
    if !has_method(scope, stream, c"_construct")? {
        return Ok(());
    }
    let (readable, writable) = parts(scope, stream)?;
    if let Some(r) = readable {
        r.set(R::CONSTRUCTED, false);
    }
    if let Some(w) = writable {
        w.set(W::CONSTRUCTED, false);
    }
    let event = K_CONSTRUCT.value(scope)?;
    js::Function::call_by_name(
        scope,
        stream.handle(),
        c"once",
        &[event, on_constructed.to_jsval_throwing(scope)?],
    )?;
    if listener_count(scope, stream, event)? > 1.0 {
        return Ok(());
    }
    js::jobs::queue_native_tick(scope, construct_tick, stream, &[])
}

/// The construction of a stream: the stream, and whether its `_construct` called back.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ConstructState {
    stream: Heap<js::object::Object>,
    #[no_trace]
    called: bool,
}

#[core_runtime::jsmethods]
impl ConstructState {}

/// Node's `constructNT`, whose payload is the stream: call `_construct` with a callback that
/// settles the construction in a tick.
fn construct_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let state =
        js::class::create_instance_with::<ConstructStateImpl>(scope, |_| ConstructStateImpl {
            stream: Heap::from(stream),
            called: false,
        })?;
    let deferred = js::Function::new_callback(scope, c"", 1, defer_construct, state)?;
    let deferred = (*deferred).to_jsval_throwing(scope)?;
    if js::Function::call_by_name(scope, stream.handle(), c"_construct", &[deferred]).is_err() {
        let thrown = js::exception::take_pending_or_undefined(scope);
        js::jobs::queue_native_tick(scope, on_construct, state, &[thrown])?;
    }
    Ok(js::value::undefined())
}

/// The callback `_construct` gets, whose payload is a [`ConstructState`]: settle in a tick.
fn defer_construct(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    js::jobs::queue_native_tick(scope, on_construct, payload, &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// Node's `onConstruct`, whose payload is a [`ConstructState`] and whose argument is the error.
fn on_construct(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConstructState>(scope, payload);
    let stream = state.data().stream.get(scope);
    let error = args.get(0);
    if state.data().called {
        if error.is_null_or_undefined() {
            error_or_destroy_with(scope, stream, multiple_callback(), false)?;
        } else {
            error_or_destroy(scope, stream, error, false)?;
        }
        return Ok(js::value::undefined());
    }
    state.data_mut().called = true;
    let (readable, writable) = parts(scope, stream)?;
    if let Some(r) = readable {
        r.set(R::CONSTRUCTED, true);
    }
    if let Some(w) = writable {
        w.set(W::CONSTRUCTED, true);
    }
    let destroyed = match (writable, readable) {
        (Some(w), _) => w.has(W::DESTROYED),
        (None, Some(r)) => r.has(R::DESTROYED),
        (None, None) => false,
    };
    if destroyed {
        let event = K_DESTROY.value(scope)?;
        js::Function::call_by_name(scope, stream.handle(), c"emit", &[event, error])?;
    } else if js::conversion::to_boolean(error) {
        error_or_destroy(scope, stream, error, true)?;
    } else {
        let event = K_CONSTRUCT.value(scope)?;
        js::Function::call_by_name(scope, stream.handle(), c"emit", &[event])?;
    }
    Ok(js::value::undefined())
}

/// Node's `emitCloseLegacy`, whose payload is the stream.
fn emit_close_legacy(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    crate::events::emit(scope, cast_payload::<Object>(scope, payload), c"close", &[])?;
    Ok(js::value::undefined())
}

/// Node's `emitErrorCloseLegacy`, whose payload is the stream and whose argument is the error:
/// emit `error` and then, in a tick, `close`.
fn emit_error_close_legacy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    crate::events::emit(scope, stream, c"error", &[args.get(0)])?;
    js::jobs::queue_native_tick(scope, emit_close_legacy, stream, &[])?;
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
    let request = stream.get_property(scope, c"req")?;
    Ok(match Object::from_value(scope, request) {
        Ok(request) => request
            .get_property(scope, c"upgradeOrConnect")?
            .is_undefined(),
        Err(_) => true,
    })
}

/// Node's `isRequest`: whether `stream` has a truthy `setHeader` and an `abort` method.
pub(crate) fn is_request(scope: &Scope<'_>, stream: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(
        js::conversion::to_boolean(stream.get_property(scope, c"setHeader")?)
            && has_method(scope, stream, c"abort")?,
    )
}

/// Node's `destroyer`: destroy `stream` with `error`, or with an `AbortError` if it has no error and
/// hasn't finished, through whichever of `abort`, `destroy` and `close` it has. A server request is
/// destroyed after its `socket` is set to `null`.
pub(crate) fn destroyer(
    scope: &Scope<'_>,
    stream: HandleValue<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let Ok(object) = Object::from_value(scope, stream) else {
        return Ok(());
    };
    if is_destroyed(scope, stream)? == Some(true) {
        return Ok(());
    }
    let mut error = scope.root_value(error.get());
    if !js::conversion::to_boolean(error) && is_finished(scope, stream)? != Some(true) {
        error = crate::errors::abort_error(scope, None)?;
    }
    let request = object.get_property(scope, c"req")?;
    let request = Object::from_value(scope, request).ok();
    if is_server_request(scope, object)? {
        object.set_property(scope, c"socket", HandleValue::null())?;
        js::Function::call_by_name(scope, object.handle(), c"destroy", &[error])?;
    } else if is_request(scope, object)? {
        js::Function::call_by_name(scope, object.handle(), c"abort", js::function::EmptyArgs)?;
    } else if let Some(request) =
        request.filter(|request| is_request(scope, *request).unwrap_or(false))
    {
        js::Function::call_by_name(scope, request.handle(), c"abort", js::function::EmptyArgs)?;
    } else if has_method(scope, object, c"destroy")? {
        js::Function::call_by_name(scope, object.handle(), c"destroy", &[error])?;
    } else if has_method(scope, object, c"close")? {
        js::Function::call_by_name(scope, object.handle(), c"close", js::function::EmptyArgs)?;
    } else if js::conversion::to_boolean(error) {
        js::jobs::queue_native_tick(scope, emit_error_close_legacy, object, &[error])?;
    } else {
        js::jobs::queue_native_tick(scope, emit_close_legacy, object, &[])?;
    }
    if !js::conversion::to_boolean(object.get_property(scope, c"destroyed")?) {
        let key = super::util::STREAM_DESTROYED.id(scope)?;
        object.set_property_by_id(scope, key, true)?;
    }
    Ok(())
}
