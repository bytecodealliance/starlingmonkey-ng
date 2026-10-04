// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Readable.toWeb`, `Readable.fromWeb`, `Writable.toWeb`, `Writable.fromWeb`, `Duplex.toWeb` and
//! `Duplex.fromWeb`: Node's `lib/internal/webstreams/adapters.js`. The web streams are the
//! runtime's `ReadableStream` and `WritableStream` globals, used through their JS API.

use std::ffi::CStr;

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::{Callback, CallbackArgs};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::readable::{buffer_from, is_buffer, is_encoding};
use super::util::{
    as_object, call, call_method, closure, error_value, has_method, is_destroyed, is_readable,
    is_writable, is_writable_ended, listen, next_tick, rust_string, take_exception, truthy,
    type_error, Captures, Props,
};
use crate::errors::{validate_object, NodeError, Thrown};

/// The global constructor `name`.
fn web_class<'s>(scope: &'s Scope<'_>, name: &'static CStr) -> Result<Object<'s>, ExnThrown> {
    scope.global().obj(scope, name)?.ok_or_else(|| {
        let name = name.to_str().unwrap_or_default();
        type_error(scope, &format!("{name} is not defined"))
    })
}

/// `new globalThis[name](...args)`.
fn construct<'s>(
    scope: &'s Scope<'_>,
    name: &'static CStr,
    args: &[HandleValue<'_>],
) -> Result<Object<'s>, ExnThrown> {
    let args: Vec<HandleValue<'s>> = args.iter().map(|arg| scope.root_value(arg.get())).collect();
    Function::construct(scope, web_class(scope, name)?, &args)
}

/// Whether `value` is a `ReadableStream`, by its brand.
pub(crate) fn is_readable_stream(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    as_object(scope, value).is_some_and(|object| {
        object
            .cast::<web_streams::readable::readable_stream::ReadableStream>()
            .is_ok()
    })
}

/// Whether `value` is a `WritableStream`, by its brand.
pub(crate) fn is_writable_stream(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    as_object(scope, value).is_some_and(|object| {
        object
            .cast::<web_streams::writable::writable_stream::WritableStream>()
            .is_ok()
    })
}

/// Whether `value` is `null` or `undefined`.
fn is_nullish(value: HandleValue<'_>) -> bool {
    value.is_null() || value.is_undefined()
}

/// A plain object without a prototype, as Node's `{ __proto__: null }`.
fn null_prototype_object<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let object = Object::new_plain(scope)?;
    object.set_prototype(scope, js::prelude::HandleObject::null())?;
    Ok(object)
}

/// A function that does nothing, shared by all callers in a global.
fn noop_function<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let function =
        js::class::get_or_init_shared_function(scope, noop as *const () as usize, |scope| {
            Function::new_callback(scope, c"", 0, noop, ())
        })?;
    Ok(*function)
}

fn noop(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// A new state array of `count` `undefined` values, the captures of the functions an adapter
/// creates.
fn new_state<'s>(scope: &'s Scope<'_>, count: usize) -> Result<Captures<'s>, ExnThrown> {
    let array = js::Array::new(scope, count)?;
    for index in 0..count as u32 {
        array.set_element(scope, index, HandleValue::undefined())?;
    }
    Captures::of(scope, (*array).to_jsval_throwing(scope)?)
}

/// The state of a function whose captures are `[state, ...]`.
fn state_of<'s>(scope: &'s Scope<'_>, payload: HandleValue<'_>) -> Result<Captures<'s>, ExnThrown> {
    Captures::of(scope, Captures::of(scope, payload)?.get(0)?)
}

/// A function running `body` with the captures `[state]`.
fn method<'s>(
    scope: &'s Scope<'_>,
    state: &Captures<'_>,
    name: &CStr,
    nargs: u32,
    body: Callback,
) -> Result<Object<'s>, ExnThrown> {
    Ok(*Function::new_callback(
        scope,
        name,
        nargs,
        body,
        *js::Array::with_contents(scope, &[state.array_value()?])?,
    )?)
}

/// The promise at `index` of `state`, if it holds one.
fn promise_at<'s>(
    scope: &'s Scope<'_>,
    state: &Captures<'s>,
    index: u32,
) -> Result<Option<js::Promise<'s>>, ExnThrown> {
    Ok(as_object(scope, state.get(index)?).and_then(|object| object.cast::<js::Promise>().ok()))
}

/// Resolve or reject `promise` with `value` unless it settled, as the functions of
/// `Promise.withResolvers()` do.
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

/// `Promise.resolve(value).then(fulfilled, rejected)`.
fn then<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    fulfilled: Option<Object<'_>>,
    rejected: Option<Object<'_>>,
) -> Result<js::Promise<'s>, ExnThrown> {
    js::Promise::call_original_resolve(scope, scope.root_value(value.get()))?
        .then(scope, fulfilled, rejected)
}

/// `ERR_INVALID_ARG_TYPE` for the argument `name`, which isn't `expected`.
fn invalid_type(scope: &Scope<'_>, name: &str, expected: &str, value: HandleValue<'_>) -> Thrown {
    NodeError::invalid_arg_type(scope, name, expected, value).into()
}

/// The `options` argument as an object: `{}` for `undefined`, and `ERR_INVALID_ARG_TYPE` for
/// a value `validateObject` rejects.
fn options_object<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'_>>,
    name: &str,
) -> Result<Object<'s>, Thrown> {
    match options.filter(|options| !options.is_undefined()) {
        Some(options) => Ok(validate_object(scope, options, name)?),
        None => Ok(Object::new_plain(scope)?),
    }
}

/// `validateBoolean(value, name)`.
fn validate_boolean(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<bool, Thrown> {
    if !value.is_boolean() {
        return Err(invalid_type(scope, name, "of type boolean", value));
    }
    Ok(value.to_boolean())
}

/// `options[name]`, or `default` if it is `undefined`.
fn option_or<'s>(
    scope: &'s Scope<'_>,
    options: Object<'_>,
    name: &'static CStr,
    default: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let value = options.val(scope, name)?;
    Ok(if value.is_undefined() {
        scope.root_value(js::value::from_bool(default))
    } else {
        value
    })
}

/// `ERR_INVALID_ARG_VALUE` unless `encoding` is `undefined` or an encoding `Buffer` supports.
fn validate_encoding(scope: &Scope<'_>, encoding: HandleValue<'_>) -> Result<(), Thrown> {
    if !encoding.is_undefined() && !is_encoding(scope, encoding)? {
        return Err(NodeError::invalid_arg_value(
            scope,
            "options.encoding",
            encoding,
            "is invalid",
        )
        .into());
    }
    Ok(())
}

/// Node's `handleKnownInternalErrors`: an `AbortError` caused by a premature close, a `TypeError`
/// with the `code` of a zlib or Brotli failure caused by it, or else `cause` itself.
fn handle_known_internal_errors<'s>(
    scope: &'s Scope<'_>,
    cause: HandleValue<'s>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let code = match as_object(scope, cause) {
        Some(object) => object.val(scope, c"code")?,
        None => return Ok(cause),
    };
    let Some(code_text) = rust_string(scope, code) else {
        return Ok(cause);
    };
    if code_text == "ERR_STREAM_PREMATURE_CLOSE" {
        return crate::errors::abort_error(scope, Some(cause));
    }
    if crate::zlib::is_failure_code(&code_text) || code_text.starts_with("ERR__ERROR_") {
        let options = Object::new_plain(scope)?;
        options.set_property(scope, c"cause", cause)?;
        let error = construct(
            scope,
            c"TypeError",
            &[HandleValue::undefined(), options.to_jsval_throwing(scope)?],
        )?;
        error.define_property(
            scope,
            c"code",
            code,
            js::class_spec::JSPROP_ENUMERATE as u32,
        )?;
        return error.to_jsval_throwing(scope);
    }
    Ok(cause)
}

/// Call `callback(error)`, or with no argument if `error` is `None`.
fn call_back(
    scope: &Scope<'_>,
    callback: HandleValue<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let args: Vec<HandleValue<'_>> = error.into_iter().collect();
    call(scope, HandleValue::undefined(), callback, &args)?;
    Ok(())
}

/// The captures of the functions [`writable_to_web`] creates: `[stream, controller,
/// backpressurePromise, closed, cleanup]`, where `backpressurePromise` and `closed` are pending
/// promises or `undefined`.
mod to_writable {
    pub(super) const STREAM: u32 = 0;
    pub(super) const CONTROLLER: u32 = 1;
    pub(super) const BACKPRESSURE: u32 = 2;
    pub(super) const CLOSED: u32 = 3;
    pub(super) const CLEANUP: u32 = 4;
    pub(super) const COUNT: usize = 5;
}

/// Node's `newWritableStreamFromStreamWritable`: a `WritableStream` writing to the Node.js
/// writable `stream`. Writes wait for `drain` once `stream.write()` returns `false`.
pub(crate) fn writable_to_web<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    use to_writable::*;
    let candidate = as_object(scope, stream).filter(|_| truthy(scope, stream));
    let stream_object = match candidate {
        Some(object)
            if has_method(scope, object, c"write")? && has_method(scope, object, c"on")? =>
        {
            object
        }
        _ => {
            return Err(invalid_type(
                scope,
                "streamWritable",
                "an stream.Writable",
                stream,
            ))
        }
    };
    if is_destroyed(scope, stream)? == Some(true) || is_writable(scope, stream)? != Some(true) {
        let writable = construct(scope, c"WritableStream", &[])?;
        call_method(scope, writable, c"close", &[])?;
        return Ok(writable);
    }
    let high_water_mark = stream_object.val(scope, c"writableHighWaterMark")?;
    let strategy = if truthy(scope, stream_object.val(scope, c"writableObjectMode")?) {
        let init = Object::new_plain(scope)?;
        init.set_property(scope, c"highWaterMark", high_water_mark)?;
        construct(
            scope,
            c"CountQueuingStrategy",
            &[init.to_jsval_throwing(scope)?],
        )?
    } else {
        let strategy = Object::new_plain(scope)?;
        strategy.set_property(scope, c"highWaterMark", high_water_mark)?;
        let size = Function::new_callback(scope, c"size", 1, chunk_size, ())?;
        strategy.set_property(scope, c"size", *size)?;
        strategy
    };

    let state = new_state(scope, COUNT)?;
    state.set(STREAM, stream_object)?;
    let on_end = method(scope, &state, c"", 1, writable_to_web_finished)?;
    let cleanup = super::eos::eos(scope, stream, None, on_end.to_jsval_throwing(scope)?)?;
    state.set(CLEANUP, cleanup)?;
    let on_drain = method(scope, &state, c"onDrain", 0, writable_to_web_drain)?;
    listen(scope, stream_object, c"on", "drain", on_drain)?;

    let sink = Object::new_plain(scope)?;
    for (name, nargs, body) in [
        (c"start", 1, writable_to_web_start as Callback),
        (c"write", 1, writable_to_web_write),
        (c"abort", 1, writable_to_web_abort),
        (c"close", 0, writable_to_web_close),
    ] {
        sink.set_property(scope, name, method(scope, &state, name, nargs, body)?)?;
    }
    Ok(construct(
        scope,
        c"WritableStream",
        &[
            sink.to_jsval_throwing(scope)?,
            strategy.to_jsval_throwing(scope)?,
        ],
    )?)
}

/// The `size` of the strategy of a `WritableStream` writing to a writable that isn't in object
/// mode: `chunk?.byteLength ?? chunk?.length ?? 1`.
fn chunk_size(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let chunk = args.get(0);
    if is_nullish(chunk) {
        return Ok(js::value::from_i32(1));
    }
    let object = Object::from_value_coerce(scope, chunk)?;
    for name in [c"byteLength", c"length"] {
        let value = object.val(scope, name)?;
        if !is_nullish(value) {
            return Ok(value.get());
        }
    }
    Ok(js::value::from_i32(1))
}

/// The `finished` callback of [`writable_to_web`]: error the `WritableStream` with the stream's
/// error, or with an `AbortError` if the stream closed before `close()` ended it.
fn writable_to_web_finished(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use to_writable::*;
    let state = state_of(scope, payload)?;
    let error = handle_known_internal_errors(scope, args.get(0))?;
    call(scope, HandleValue::undefined(), state.get(CLEANUP)?, &[])?;
    let stream = state.obj(STREAM)?;
    listen(scope, stream, c"on", "error", noop_function(scope)?)?;
    let closed = promise_at(scope, &state, CLOSED)?;
    let controller = as_object(scope, state.get(CONTROLLER)?);
    if !is_nullish(error) {
        if let Some(backpressure) = promise_at(scope, &state, BACKPRESSURE)? {
            settle(scope, backpressure, true, error)?;
        }
        if let Some(closed) = closed {
            settle(scope, closed, true, error)?;
            state.set(CLOSED, js::value::undefined())?;
        }
        if let Some(controller) = controller {
            call_method(scope, controller, c"error", &[error])?;
        }
        state.set(CONTROLLER, js::value::undefined())?;
        return Ok(js::value::undefined());
    }
    if let Some(closed) = closed {
        settle(scope, closed, false, HandleValue::undefined())?;
        state.set(CLOSED, js::value::undefined())?;
        return Ok(js::value::undefined());
    }
    if let Some(controller) = controller {
        let error = crate::errors::abort_error(scope, None)?;
        call_method(scope, controller, c"error", &[error])?;
    }
    state.set(CONTROLLER, js::value::undefined())?;
    Ok(js::value::undefined())
}

fn writable_to_web_drain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    if let Some(backpressure) = promise_at(scope, &state, to_writable::BACKPRESSURE)? {
        settle(scope, backpressure, false, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

fn writable_to_web_start(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    state_of(scope, payload)?.set(to_writable::CONTROLLER, args.get(0))?;
    Ok(js::value::undefined())
}

/// The sink's `write(chunk)`: write `chunk` to the stream, as a `Uint8Array` if it is an
/// `ArrayBuffer` and the stream isn't in object mode. Returns a promise that fulfills once the
/// stream drains if it needs to.
fn writable_to_web_write(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use to_writable::*;
    let state = state_of(scope, payload)?;
    let stream = state.obj(STREAM)?;
    let mut chunk = args.get(0);
    let object_mode = truthy(scope, stream.val(scope, c"writableObjectMode")?);
    if !object_mode {
        if let Some(object) = as_object(scope, chunk) {
            if crate::inspect::is_array_buffer(object)
                || crate::inspect::is_shared_array_buffer(scope, object)?
            {
                chunk = construct(scope, c"Uint8Array", &[chunk])?.to_jsval_throwing(scope)?;
            }
        }
    }
    let need_drain = truthy(scope, stream.val(scope, c"writableNeedDrain")?);
    if !need_drain && truthy(scope, call_method(scope, stream, c"write", &[chunk])?) {
        return Ok(js::value::undefined());
    }
    let backpressure = js::Promise::new_pending(scope)?;
    state.set(BACKPRESSURE, *backpressure)?;
    if !truthy(scope, stream.val(scope, c"writableNeedDrain")?) {
        backpressure.resolve(scope, HandleValue::undefined())?;
    }
    let result = js::Promise::new_pending(scope)?;
    let captures = [state.array, *result];
    let fulfilled = closure(scope, 1, backpressure_settled, &captures)?;
    let rejected = closure(scope, 1, backpressure_failed, &captures)?;
    backpressure.then(scope, Some(fulfilled), Some(rejected))?;
    result.to_jsval_raw_throwing(scope)
}

/// The `finally` of the backpressure promise, whose captures are `[state, result]`: clear it, and
/// fulfill `result`.
fn backpressure_settled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    state.set(to_writable::BACKPRESSURE, js::value::undefined())?;
    if let Some(result) = promise_at(scope, &captures, 1)? {
        result.resolve(scope, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

/// The `finally` of the backpressure promise after a rejection, whose captures are `[state,
/// result]`: clear it, and reject `result` with the reason.
fn backpressure_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    state.set(to_writable::BACKPRESSURE, js::value::undefined())?;
    if let Some(result) = promise_at(scope, &captures, 1)? {
        result.reject(scope, args.get(0))?;
    }
    Ok(js::value::undefined())
}

fn writable_to_web_abort(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let stream = state.get(to_writable::STREAM)?;
    super::destroy::destroyer(scope, stream, args.get(0))?;
    Ok(js::value::undefined())
}

/// The sink's `close()`: end the stream, and return a promise that settles once it finished.
fn writable_to_web_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use to_writable::*;
    let state = state_of(scope, payload)?;
    let stream = state.get(STREAM)?;
    if state.get(CLOSED)?.is_undefined() && is_writable_ended(scope, stream)? != Some(true) {
        let closed = js::Promise::new_pending(scope)?;
        state.set(CLOSED, *closed)?;
        call_method(scope, state.obj(STREAM)?, c"end", &[])?;
        return closed.to_jsval_raw_throwing(scope);
    }
    state.set(CONTROLLER, js::value::undefined())?;
    js::Promise::new_resolved_with_value(scope, HandleValue::undefined())?
        .to_jsval_raw_throwing(scope)
}

/// The captures of the functions [`readable_to_web`] creates: `[stream, controller, wasCanceled,
/// cleanup, isBYOB, objectMode]`.
mod to_readable {
    pub(super) const STREAM: u32 = 0;
    pub(super) const CONTROLLER: u32 = 1;
    pub(super) const WAS_CANCELED: u32 = 2;
    pub(super) const CLEANUP: u32 = 3;
    pub(super) const IS_BYOB: u32 = 4;
    pub(super) const OBJECT_MODE: u32 = 5;
    pub(super) const COUNT: usize = 6;
}

/// The key of the property marking a stream that has the no-op `error` listener
/// [`readable_to_web`] adds.
fn error_sentinel_key<'s>(scope: &'s Scope<'_>) -> Result<js::native::HandleId<'s>, ExnThrown> {
    let symbol = crate::symbol(scope, "nodejs.webstreams.kErrorSentinelAttached", true)?;
    Ok(scope.root_id(js::id::value_to_id(scope, symbol)?))
}

/// Node's `newReadableStreamFromStreamReadable`: a `ReadableStream` reading from the Node.js
/// readable `stream`, a byte stream if `options.type` is `'bytes'`. `options.strategy` is its
/// queuing strategy, by default one with the stream's high water mark.
pub(crate) fn readable_to_web<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    use to_readable::*;
    let readable_state = match as_object(scope, stream) {
        Some(object) => object.val(scope, c"_readableState")?,
        None => HandleValue::undefined(),
    };
    if !readable_state.is_object() && !readable_state.is_null() {
        return Err(invalid_type(
            scope,
            "streamReadable",
            "an stream.Readable",
            stream,
        ));
    }
    let stream_object = as_object(scope, stream).expect("streams are objects");
    let options = options_object(scope, options, "options")?;
    let kind = options.val(scope, c"type")?;
    let is_byob = validate_type(scope, kind)?;

    let state = new_state(scope, COUNT)?;
    state.set(STREAM, stream_object)?;
    state.set(WAS_CANCELED, false)?;
    state.set(CLEANUP, noop_function(scope)?)?;
    state.set(IS_BYOB, is_byob)?;
    let source = null_prototype_object(scope)?;
    if is_byob {
        source.set_property(scope, c"type", "bytes")?;
    } else {
        source.set_property(scope, c"type", HandleValue::undefined())?;
    }
    source.set_property(
        scope,
        c"start",
        method(scope, &state, c"start", 1, readable_to_web_start)?,
    )?;
    source.set_property(
        scope,
        c"cancel",
        method(scope, &state, c"cancel", 1, readable_to_web_cancel)?,
    )?;

    let readable = is_readable(scope, stream)? == Some(true);
    let object_mode = stream_object.val(scope, c"readableObjectMode")?;
    state.set(OBJECT_MODE, truthy(scope, object_mode))?;
    let mut strategy = HandleValue::undefined();
    if readable {
        source.set_property(
            scope,
            c"pull",
            method(scope, &state, c"pull", 0, readable_to_web_pull)?,
        )?;
        let high_water_mark = stream_object.val(scope, c"readableHighWaterMark")?;
        let init = Object::new_plain(scope)?;
        init.set_property(scope, c"highWaterMark", high_water_mark)?;
        strategy = if is_byob {
            init.to_jsval_throwing(scope)?
        } else {
            let given = options.val(scope, c"strategy")?;
            if is_nullish(given) {
                let class = if truthy(scope, object_mode) {
                    c"CountQueuingStrategy"
                } else {
                    c"ByteLengthQueuingStrategy"
                };
                construct(scope, class, &[init.to_jsval_throwing(scope)?])?
                    .to_jsval_throwing(scope)?
            } else {
                given
            }
        };
    }
    let readable_stream = construct(
        scope,
        c"ReadableStream",
        &[source.to_jsval_throwing(scope)?, strategy],
    )?;

    let eos_options = null_prototype_object(scope)?;
    eos_options.set_property(scope, c"writable", false)?;
    let on_end = method(scope, &state, c"", 1, readable_to_web_finished)?;
    let cleanup = super::eos::eos_synchronous(
        scope,
        stream,
        Some(eos_options.to_jsval_throwing(scope)?),
        on_end.to_jsval_throwing(scope)?,
    )?;
    state.set(CLEANUP, cleanup)?;

    if state.flag(WAS_CANCELED)? {
        call(scope, HandleValue::undefined(), cleanup, &[])?;
    } else if readable {
        call_method(scope, stream_object, c"pause", &[])?;
        let on_data = method(scope, &state, c"onData", 1, readable_to_web_data)?;
        listen(scope, stream_object, c"on", "data", on_data)?;
    }
    Ok(readable_stream)
}

/// `validateOneOf(type, 'options.type', ['bytes', undefined])`. Returns whether `type` is
/// `'bytes'`.
fn validate_type(scope: &Scope<'_>, kind: HandleValue<'_>) -> Result<bool, Thrown> {
    if kind.is_undefined() {
        return Ok(false);
    }
    if super::util::is_str(scope, kind, c"bytes")? {
        return Ok(true);
    }
    Err(NodeError::invalid_arg_value(
        scope,
        "options.type",
        kind,
        "must be one of: 'bytes', undefined",
    )
    .into())
}

fn readable_to_web_start(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    state_of(scope, payload)?.set(to_readable::CONTROLLER, args.get(0))?;
    Ok(js::value::undefined())
}

fn readable_to_web_cancel(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    state.set(to_readable::WAS_CANCELED, true)?;
    let stream = state.get(to_readable::STREAM)?;
    super::destroy::destroyer(scope, stream, args.get(0))?;
    Ok(js::value::undefined())
}

fn readable_to_web_pull(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = state_of(scope, payload)?.obj(to_readable::STREAM)?;
    call_method(scope, stream, c"resume", &[])?;
    Ok(js::value::undefined())
}

/// The `data` listener of [`readable_to_web`]: enqueue the chunk, as a copy if it is a `Buffer`
/// outside object mode, and pause the stream once the queue is full.
fn readable_to_web_data(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use to_readable::*;
    let state = state_of(scope, payload)?;
    let mut chunk = args.get(0);
    if is_buffer(scope, chunk)? && !state.flag(OBJECT_MODE)? {
        chunk = construct(scope, c"Uint8Array", &[chunk])?.to_jsval_throwing(scope)?;
    }
    let controller = state.obj(CONTROLLER)?;
    call_method(scope, controller, c"enqueue", &[chunk])?;
    let desired = controller.val(scope, c"desiredSize")?;
    let full = if desired.is_number() {
        desired.to_number() <= 0.0
    } else {
        desired.is_null()
    };
    if full {
        call_method(scope, state.obj(STREAM)?, c"pause", &[])?;
    }
    Ok(js::value::undefined())
}

/// The `finished` callback of [`readable_to_web`]: error or close the `ReadableStream` unless it
/// was canceled.
fn readable_to_web_finished(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use to_readable::*;
    let state = state_of(scope, payload)?;
    let error = handle_known_internal_errors(scope, args.get(0))?;
    call(scope, HandleValue::undefined(), state.get(CLEANUP)?, &[])?;
    let stream = state.obj(STREAM)?;
    let key = error_sentinel_key(scope)?;
    if !stream.has_property_by_id(scope, key)? {
        listen(scope, stream, c"on", "error", noop_function(scope)?)?;
        stream.set_property_by_id(scope, key, true)?;
    }
    if state.flag(WAS_CANCELED)? {
        return Ok(js::value::undefined());
    }
    state.set(WAS_CANCELED, true)?;
    let controller = state.obj(CONTROLLER)?;
    if truthy(scope, error) {
        call_method(scope, controller, c"error", &[error])?;
        return Ok(js::value::undefined());
    }
    call_method(scope, controller, c"close", &[])?;
    if state.flag(IS_BYOB)? {
        if let Some(request) = controller.obj(scope, c"byobRequest")? {
            let zero = scope.root_value(js::value::from_i32(0));
            call_method(scope, request, c"respond", &[zero])?;
        }
    }
    Ok(js::value::undefined())
}

/// The captures of the functions the `fromWeb` adapters create: `[stream, writer, reader,
/// writableClosed, readableClosed, decodeStrings, objectMode]`, where `stream` is the Node.js
/// stream the adapter returns.
mod from_web {
    pub(super) const STREAM: u32 = 0;
    pub(super) const WRITER: u32 = 1;
    pub(super) const READER: u32 = 2;
    pub(super) const WRITABLE_CLOSED: u32 = 3;
    pub(super) const READABLE_CLOSED: u32 = 4;
    pub(super) const DECODE_STRINGS: u32 = 5;
    pub(super) const OBJECT_MODE: u32 = 6;
    pub(super) const COUNT: usize = 7;
}

/// The options of the Node.js stream a `fromWeb` adapter creates: the properties in `values`, and
/// the functions in `methods` with the captures `[state]`.
fn stream_options<'s>(
    scope: &'s Scope<'_>,
    state: &Captures<'_>,
    values: &[(&'static CStr, HandleValue<'_>)],
    methods: &[(&'static CStr, u32, Callback)],
) -> Result<HandleValue<'s>, ExnThrown> {
    let options = Object::new_plain(scope)?;
    for &(name, value) in values {
        options.set_property(scope, name, value)?;
    }
    for &(name, nargs, body) in methods {
        options.set_property(scope, name, method(scope, state, name, nargs, body)?)?;
    }
    options.to_jsval_throwing(scope)
}

/// Node's `newStreamWritableFromWritableStream`: a Node.js `Writable` writing to the
/// `WritableStream` `writable_stream` through a writer it locks.
pub(crate) fn writable_from_web<'s>(
    scope: &'s Scope<'_>,
    writable_stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    use from_web::*;
    if !is_writable_stream(scope, writable_stream) {
        return Err(invalid_type(
            scope,
            "writableStream",
            "an instance of WritableStream",
            writable_stream,
        ));
    }
    let options = options_object(scope, options, "options")?;
    let high_water_mark = options.val(scope, c"highWaterMark")?;
    let decode_strings = option_or(scope, options, c"decodeStrings", true)?;
    let object_mode = option_or(scope, options, c"objectMode", false)?;
    let signal = options.val(scope, c"signal")?;
    let object_mode_flag = validate_boolean(scope, object_mode, "options.objectMode")?;
    let decode_strings_flag = validate_boolean(scope, decode_strings, "options.decodeStrings")?;

    let source = as_object(scope, writable_stream).expect("streams are objects");
    let writer = call_method(scope, source, c"getWriter", &[])?;
    let state = new_state(scope, COUNT)?;
    state.set(WRITER, writer)?;
    state.set(WRITABLE_CLOSED, false)?;
    state.set(READABLE_CLOSED, false)?;
    state.set(DECODE_STRINGS, decode_strings_flag)?;
    state.set(OBJECT_MODE, object_mode_flag)?;
    let stream_options = stream_options(
        scope,
        &state,
        &[
            (c"highWaterMark", high_water_mark),
            (c"objectMode", object_mode),
            (c"decodeStrings", decode_strings),
            (c"signal", signal),
        ],
        &[
            (c"writev", 2, web_writev),
            (c"write", 3, web_write),
            (c"destroy", 2, writable_from_web_destroy),
            (c"final", 1, web_final),
        ],
    )?;
    let ctor = super::writable::writable::class_constructor(scope)?;
    let writable = Function::construct(scope, ctor, &[stream_options])?;
    state.set(STREAM, writable)?;
    watch_writer_closed(scope, &state)?;
    Ok(writable)
}

/// Node's `newStreamReadableFromReadableStream`: a Node.js `Readable` reading from the
/// `ReadableStream` `readable_stream` through a reader it locks.
pub(crate) fn readable_from_web<'s>(
    scope: &'s Scope<'_>,
    readable_stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    use from_web::*;
    if !is_readable_stream(scope, readable_stream) {
        return Err(invalid_type(
            scope,
            "readableStream",
            "an instance of ReadableStream",
            readable_stream,
        ));
    }
    let options = options_object(scope, options, "options")?;
    let high_water_mark = options.val(scope, c"highWaterMark")?;
    let encoding = options.val(scope, c"encoding")?;
    let object_mode = option_or(scope, options, c"objectMode", false)?;
    let signal = options.val(scope, c"signal")?;
    validate_encoding(scope, encoding)?;
    validate_boolean(scope, object_mode, "options.objectMode")?;

    let source = as_object(scope, readable_stream).expect("streams are objects");
    let reader = call_method(scope, source, c"getReader", &[])?;
    let state = new_state(scope, COUNT)?;
    state.set(READER, reader)?;
    state.set(WRITABLE_CLOSED, false)?;
    state.set(READABLE_CLOSED, false)?;
    let stream_options = stream_options(
        scope,
        &state,
        &[
            (c"objectMode", object_mode),
            (c"highWaterMark", high_water_mark),
            (c"encoding", encoding),
            (c"signal", signal),
        ],
        &[
            (c"read", 0, web_read),
            (c"destroy", 2, readable_from_web_destroy),
        ],
    )?;
    let ctor = super::readable::readable::class_constructor(scope)?;
    let readable = Function::construct(scope, ctor, &[stream_options])?;
    state.set(STREAM, readable)?;
    watch_reader_closed(scope, &state)?;
    Ok(readable)
}

/// Node's `newStreamDuplexFromReadableWritablePair`: a Node.js `Duplex` reading from
/// `pair.readable` and writing to `pair.writable`.
pub(crate) fn duplex_from_web<'s>(
    scope: &'s Scope<'_>,
    pair: Option<HandleValue<'_>>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    use from_web::*;
    let pair = options_object(scope, pair, "pair")?;
    let readable_stream = pair.val(scope, c"readable")?;
    let writable_stream = pair.val(scope, c"writable")?;
    if !is_readable_stream(scope, readable_stream) {
        return Err(invalid_type(
            scope,
            "pair.readable",
            "an instance of ReadableStream",
            readable_stream,
        ));
    }
    if !is_writable_stream(scope, writable_stream) {
        return Err(invalid_type(
            scope,
            "pair.writable",
            "an instance of WritableStream",
            writable_stream,
        ));
    }
    let options = options_object(scope, options, "options")?;
    let allow_half_open = option_or(scope, options, c"allowHalfOpen", false)?;
    let object_mode = option_or(scope, options, c"objectMode", false)?;
    let encoding = options.val(scope, c"encoding")?;
    let decode_strings = option_or(scope, options, c"decodeStrings", true)?;
    let high_water_mark = options.val(scope, c"highWaterMark")?;
    let signal = options.val(scope, c"signal")?;
    let object_mode_flag = validate_boolean(scope, object_mode, "options.objectMode")?;
    validate_encoding(scope, encoding)?;

    let writable_object = as_object(scope, writable_stream).expect("streams are objects");
    let readable_object = as_object(scope, readable_stream).expect("streams are objects");
    let writer = call_method(scope, writable_object, c"getWriter", &[])?;
    let reader = call_method(scope, readable_object, c"getReader", &[])?;
    let state = new_state(scope, COUNT)?;
    state.set(WRITER, writer)?;
    state.set(READER, reader)?;
    state.set(WRITABLE_CLOSED, false)?;
    state.set(READABLE_CLOSED, false)?;
    state.set(DECODE_STRINGS, truthy(scope, decode_strings))?;
    state.set(OBJECT_MODE, object_mode_flag)?;
    let stream_options = stream_options(
        scope,
        &state,
        &[
            (c"allowHalfOpen", allow_half_open),
            (c"highWaterMark", high_water_mark),
            (c"objectMode", object_mode),
            (c"encoding", encoding),
            (c"decodeStrings", decode_strings),
            (c"signal", signal),
        ],
        &[
            (c"writev", 2, web_writev),
            (c"write", 3, web_write),
            (c"final", 1, web_final),
            (c"read", 0, web_read),
            (c"destroy", 2, duplex_from_web_destroy),
        ],
    )?;
    let ctor = super::duplex::duplex::class_constructor(scope)?;
    let duplex = Function::construct(scope, ctor, &[stream_options])?;
    state.set(STREAM, duplex)?;
    watch_writer_closed(scope, &state)?;
    watch_reader_closed(scope, &state)?;
    Ok(duplex)
}

/// `writer.closed.then(...)`: destroy the stream with `ERR_STREAM_PREMATURE_CLOSE` if the
/// `WritableStream` closes before the stream ended, or with the error it errors with.
fn watch_writer_closed(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    let writer = state.obj(from_web::WRITER)?;
    let closed = writer.val(scope, c"closed")?;
    let fulfilled = method(scope, state, c"", 0, writer_closed)?;
    let rejected = method(scope, state, c"", 1, web_stream_failed)?;
    then(scope, closed, Some(fulfilled), Some(rejected))?;
    Ok(())
}

/// `reader.closed.then(...)`: destroy the stream with the error the `ReadableStream` errors
/// with.
fn watch_reader_closed(scope: &Scope<'_>, state: &Captures<'_>) -> Result<(), ExnThrown> {
    let reader = state.obj(from_web::READER)?;
    let closed = reader.val(scope, c"closed")?;
    let fulfilled = method(scope, state, c"", 0, reader_closed)?;
    let rejected = method(scope, state, c"", 1, web_stream_failed)?;
    then(scope, closed, Some(fulfilled), Some(rejected))?;
    Ok(())
}

fn writer_closed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use from_web::*;
    let state = state_of(scope, payload)?;
    state.set(WRITABLE_CLOSED, true)?;
    let stream = state.get(STREAM)?;
    if is_writable_ended(scope, stream)? != Some(true) {
        let error = error_value(scope, super::eos::premature_close())?;
        super::destroy::destroyer(scope, stream, error)?;
    }
    Ok(js::value::undefined())
}

fn reader_closed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    state_of(scope, payload)?.set(from_web::READABLE_CLOSED, true)?;
    Ok(js::value::undefined())
}

/// The rejection of `writer.closed` or `reader.closed`: mark both sides closed, and destroy the
/// stream with the error.
fn web_stream_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use from_web::*;
    let state = state_of(scope, payload)?;
    state.set(WRITABLE_CLOSED, true)?;
    state.set(READABLE_CLOSED, true)?;
    super::destroy::destroyer(scope, state.get(STREAM)?, args.get(0))?;
    Ok(js::value::undefined())
}

/// How [`done`] handles an exception the callback of a `fromWeb` stream's method throws.
#[derive(Clone, Copy)]
enum OnThrow {
    /// Destroy the stream with it.
    Destroy,
    /// Destroy the stream with it in a tick.
    DestroyInTick,
    /// Throw it in a tick.
    ThrowInTick,
}

impl OnThrow {
    fn code(self) -> i32 {
        self as i32
    }

    fn of(value: HandleValue<'_>) -> Self {
        match value.to_int32() {
            0 => OnThrow::Destroy,
            1 => OnThrow::DestroyInTick,
            _ => OnThrow::ThrowInTick,
        }
    }
}

/// Promise reactions that call `callback`: with no error once the promise fulfills, and with the
/// reason once it rejects. An exception `callback` throws is handled according to `on_throw`.
fn done_reactions<'s>(
    scope: &'s Scope<'_>,
    state: &Captures<'_>,
    callback: HandleValue<'_>,
    on_throw: OnThrow,
) -> Result<(Object<'s>, Object<'s>), ExnThrown> {
    let captures = js::Array::with_contents(
        scope,
        &[
            state.array_value()?,
            callback.get(),
            js::value::from_i32(on_throw.code()),
        ],
    )?;
    let fulfilled = Function::new_callback(scope, c"", 1, done_fulfilled, *captures)?;
    let rejected = Function::new_callback(scope, c"", 1, done_rejected, *captures)?;
    Ok((*fulfilled, *rejected))
}

fn done_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    done(scope, payload, None)
}

fn done_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    done(scope, payload, Some(args.get(0)))
}

/// Node's `done(error)` of the `fromWeb` streams' methods, whose captures are `[state, callback,
/// onThrow]`.
fn done(
    scope: &Scope<'_>,
    payload: HandleValue<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    if call_back(scope, captures.get(1)?, error).is_err() {
        let thrown = take_exception(scope);
        let stream = state.get(from_web::STREAM)?;
        match OnThrow::of(captures.get(2)?) {
            OnThrow::Destroy => super::destroy::destroyer(scope, stream, thrown)?,
            OnThrow::DestroyInTick => {
                next_tick(scope, destroy_in_tick, &[stream.get(), thrown.get()])?
            }
            OnThrow::ThrowInTick => next_tick(scope, throw_in_tick, &[thrown.get()])?,
        }
    }
    Ok(js::value::undefined())
}

/// `process.nextTick(() => destroy(stream, error))`, whose captures are `[stream, error]`.
fn destroy_in_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    super::destroy::destroyer(scope, captures.get(0)?, captures.get(1)?)?;
    Ok(js::value::undefined())
}

/// `process.nextTick(() => { throw error; })`, whose captures are `[error]`.
fn throw_in_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let error = Captures::of(scope, payload)?.get(0)?;
    Err(js::exception::set_pending(
        scope,
        error,
        js::native::ExceptionStackBehavior::Capture,
    ))
}

/// `writer.ready.then(after).then(done, done)`, with the captures `[state, value]` for `after`.
fn after_ready(
    scope: &Scope<'_>,
    state: &Captures<'_>,
    after: Callback,
    value: HandleValue<'_>,
    callback: HandleValue<'_>,
    on_throw: OnThrow,
) -> Result<(), ExnThrown> {
    let writer = state.obj(from_web::WRITER)?;
    let ready = writer.val(scope, c"ready")?;
    let after = closure(
        scope,
        1,
        after,
        &[state.array.to_jsval_throwing(scope)?, value],
    )?;
    let written = then(scope, ready, Some(after), None)?;
    let (fulfilled, rejected) = done_reactions(scope, state, callback, on_throw)?;
    written.then(scope, Some(fulfilled), Some(rejected))?;
    Ok(())
}

/// The `writev(chunks, callback)` of the `fromWeb` streams: write each chunk once the writer is
/// ready.
fn web_writev(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    after_ready(
        scope,
        &state,
        write_all,
        args.get(0),
        args.get(1),
        OnThrow::DestroyInTick,
    )?;
    Ok(js::value::undefined())
}

/// `SafePromiseAllReturnVoid(chunks, (data) => writer.write(data.chunk))`, whose captures are
/// `[state, chunks]`.
fn write_all(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    let writer = state.obj(from_web::WRITER)?;
    let chunks = captures.obj(1)?;
    let mut writes = Vec::new();
    for index in 0..chunks.num(scope, c"length")? as u32 {
        let data = chunks.get_element(scope, index)?;
        let chunk = Object::from_value_coerce(scope, data)?.val(scope, c"chunk")?;
        let written = call_method(scope, writer, c"write", &[chunk])?;
        writes.push(js::Promise::call_original_resolve(scope, written)?);
    }
    let handles: Vec<_> = writes.iter().map(|promise| promise.handle()).collect();
    let all = js::Promise::wait_for_all_from(scope, &handles)?;
    let fulfilled = Function::new_callback(scope, c"", 0, noop, ())?;
    all.then(scope, Some(*fulfilled), None)?
        .to_jsval_raw_throwing(scope)
}

/// The `write(chunk, encoding, callback)` of the `fromWeb` streams: write `chunk` once the writer
/// is ready, as a `Uint8Array` if it is a string to decode.
fn web_write(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use from_web::*;
    let state = state_of(scope, payload)?;
    let mut chunk = args.get(0);
    if chunk.is_string() && state.flag(DECODE_STRINGS)? && !state.flag(OBJECT_MODE)? {
        let buffer = buffer_from(scope, &[chunk, args.get(1)])?;
        let buffer = as_object(scope, buffer).expect("buffers are objects");
        let view = construct(
            scope,
            c"Uint8Array",
            &[
                buffer.val(scope, c"buffer")?,
                buffer.val(scope, c"byteOffset")?,
                buffer.val(scope, c"byteLength")?,
            ],
        )?;
        chunk = view.to_jsval_throwing(scope)?;
    }
    after_ready(
        scope,
        &state,
        write_one,
        chunk,
        args.get(2),
        OnThrow::Destroy,
    )?;
    Ok(js::value::undefined())
}

/// `writer.write(chunk)`, whose captures are `[state, chunk]`.
fn write_one(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    let writer = state.obj(from_web::WRITER)?;
    Ok(call_method(scope, writer, c"write", &[captures.get(1)?])?.get())
}

/// The `final(callback)` of the `fromWeb` streams: close the writer unless the `WritableStream`
/// closed.
fn web_final(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use from_web::*;
    let state = state_of(scope, payload)?;
    if !state.flag(WRITABLE_CLOSED)? {
        let closed = call_method(scope, state.obj(WRITER)?, c"close", &[])?;
        let (fulfilled, rejected) =
            done_reactions(scope, &state, args.get(0), OnThrow::DestroyInTick)?;
        then(scope, closed, Some(fulfilled), Some(rejected))?;
    }
    Ok(js::value::undefined())
}

/// The `read()` of the `fromWeb` streams: push the reader's next chunk, or the end.
fn web_read(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let read = call_method(scope, state.obj(from_web::READER)?, c"read", &[])?;
    let fulfilled = method(scope, &state, c"", 1, web_read_result)?;
    let rejected = method(scope, &state, c"", 1, web_read_failed)?;
    then(scope, read, Some(fulfilled), Some(rejected))?;
    Ok(js::value::undefined())
}

fn web_read_result(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    let stream = state.obj(from_web::STREAM)?;
    let result = Object::from_value_coerce(scope, args.get(0))?;
    if truthy(scope, result.val(scope, c"done")?) {
        call_method(scope, stream, c"push", &[HandleValue::null()])?;
    } else {
        call_method(scope, stream, c"push", &[result.val(scope, c"value")?])?;
    }
    Ok(js::value::undefined())
}

fn web_read_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = state_of(scope, payload)?;
    super::destroy::destroyer(scope, state.get(from_web::STREAM)?, args.get(0))?;
    Ok(js::value::undefined())
}

/// A function that calls the destroy callback with `error`, for the promise of
/// `destroy(error, callback)`.
fn destroy_done_reactions<'s>(
    scope: &'s Scope<'_>,
    callback: HandleValue<'_>,
    error: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    closure(scope, 0, destroy_done, &[callback.get(), error.get()])
}

/// Node's `done()` of the `fromWeb` streams' `destroy`, whose captures are `[callback, error]`:
/// call the callback with the error, and throw an exception it throws in a tick.
fn destroy_done(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    if call_back(scope, captures.get(0)?, Some(captures.get(1)?)).is_err() {
        let thrown = take_exception(scope);
        next_tick(scope, throw_in_tick, &[thrown.get()])?;
    }
    Ok(js::value::undefined())
}

/// The `destroy(error, callback)` of [`writable_from_web`]: abort the writer with `error`, or
/// close it without one, unless the `WritableStream` closed.
fn writable_from_web_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use from_web::*;
    let state = state_of(scope, payload)?;
    let (error, callback) = (args.get(0), args.get(1));
    let done = destroy_done_reactions(scope, callback, error)?;
    if !state.flag(WRITABLE_CLOSED)? {
        let writer = state.obj(WRITER)?;
        let closed = if is_nullish(error) {
            call_method(scope, writer, c"close", &[])?
        } else {
            call_method(scope, writer, c"abort", &[error])?
        };
        then(scope, closed, Some(done), Some(done))?;
        return Ok(js::value::undefined());
    }
    call(
        scope,
        HandleValue::undefined(),
        done.to_jsval_throwing(scope)?,
        &[],
    )?;
    Ok(js::value::undefined())
}

/// The `destroy(error, callback)` of [`readable_from_web`]: cancel the reader with `error` unless
/// the `ReadableStream` closed.
fn readable_from_web_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use from_web::*;
    let state = state_of(scope, payload)?;
    let (error, callback) = (args.get(0), args.get(1));
    let done = destroy_done_reactions(scope, callback, error)?;
    if !state.flag(READABLE_CLOSED)? {
        let canceled = call_method(scope, state.obj(READER)?, c"cancel", &[error])?;
        then(scope, canceled, Some(done), Some(done))?;
        return Ok(js::value::undefined());
    }
    call(
        scope,
        HandleValue::undefined(),
        done.to_jsval_throwing(scope)?,
        &[],
    )?;
    Ok(js::value::undefined())
}

/// The `destroy(error, callback)` of [`duplex_from_web`]: abort the writer and cancel the reader
/// with `error`, each unless its stream closed.
fn duplex_from_web_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use from_web::*;
    let state = state_of(scope, payload)?;
    let (error, callback) = (args.get(0), args.get(1));
    let done = destroy_done_reactions(scope, callback, error)?;
    let writable_closed = state.flag(WRITABLE_CLOSED)?;
    let readable_closed = state.flag(READABLE_CLOSED)?;
    if writable_closed && readable_closed {
        call(
            scope,
            HandleValue::undefined(),
            done.to_jsval_throwing(scope)?,
            &[],
        )?;
        return Ok(js::value::undefined());
    }
    let side =
        |closed: bool, owner: u32, name: &'static CStr| -> Result<js::Promise<'_>, ExnThrown> {
            if closed {
                return js::Promise::new_resolved_with_value(scope, HandleValue::undefined());
            }
            let result = call_method(scope, state.obj(owner)?, name, &[error])?;
            js::Promise::call_original_resolve(scope, result)
        };
    let writer = side(writable_closed, WRITER, c"abort")?;
    let reader = side(readable_closed, READER, c"cancel")?;
    let all = js::Promise::wait_for_all_from(scope, &[writer.handle(), reader.handle()])?;
    all.then(scope, Some(done), Some(done))?;
    Ok(js::value::undefined())
}

/// Node's `newReadableWritablePairFromDuplex`: `{ readable, writable }` web streams reading from
/// and writing to `duplex`. A side that isn't readable or writable is a closed or canceled
/// stream, and both are for a destroyed `duplex`. `options.readableType`, or the deprecated
/// `options.type`, is the `type` of the `ReadableStream`.
pub(crate) fn duplex_to_web<'s>(
    scope: &'s Scope<'_>,
    duplex: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let has_state = |name: &'static CStr| -> Result<bool, ExnThrown> {
        let value = match as_object(scope, duplex) {
            Some(object) => object.val(scope, name)?,
            None => return Ok(false),
        };
        Ok(value.is_object() || value.is_null())
    };
    if !has_state(c"_writableState")? || !has_state(c"_readableState")? {
        return Err(invalid_type(scope, "duplex", "an stream.Duplex", duplex));
    }
    let options = options_object(scope, options, "options")?;
    let readable_options = null_prototype_object(scope)?;
    let mut kind = options.val(scope, c"readableType")?;
    if is_nullish(kind) {
        kind = options.val(scope, c"type")?;
    }
    readable_options.set_property(scope, c"type", kind)?;
    let readable_options_value = readable_options.to_jsval_throwing(scope)?;
    let empty_source = || -> Result<HandleValue<'_>, ExnThrown> {
        let source = Object::new_plain(scope)?;
        source.set_property(scope, c"type", kind)?;
        source.to_jsval_throwing(scope)
    };

    let pair = Object::new_plain(scope)?;
    if is_destroyed(scope, duplex)? == Some(true) {
        let writable = construct(scope, c"WritableStream", &[])?;
        let readable = construct(scope, c"ReadableStream", &[empty_source()?])?;
        call_method(scope, writable, c"close", &[])?;
        call_method(scope, readable, c"cancel", &[])?;
        pair.set_property(scope, c"readable", readable)?;
        pair.set_property(scope, c"writable", writable)?;
        return Ok(pair);
    }

    let writable = if is_writable(scope, duplex)? == Some(true) {
        writable_to_web(scope, duplex)?
    } else {
        construct(scope, c"WritableStream", &[])?
    };
    if is_writable(scope, duplex)? != Some(true) {
        call_method(scope, writable, c"close", &[])?;
    }
    let readable = if is_readable(scope, duplex)? == Some(true) {
        readable_to_web(scope, duplex, Some(readable_options_value))?
    } else {
        construct(scope, c"ReadableStream", &[empty_source()?])?
    };
    if is_readable(scope, duplex)? != Some(true) {
        call_method(scope, readable, c"cancel", &[])?;
    }
    pair.set_property(scope, c"writable", writable)?;
    pair.set_property(scope, c"readable", readable)?;
    Ok(pair)
}
