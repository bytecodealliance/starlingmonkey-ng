// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Readable.toWeb`, `Readable.fromWeb`, `Writable.toWeb`, `Writable.fromWeb`, `Duplex.toWeb` and
//! `Duplex.fromWeb`: Node's `lib/internal/webstreams/adapters.js`, on the runtime's
//! `ReadableStream` and `WritableStream`.

use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, Callback, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};
use web_streams::queuing::QueuingStrategy;
use web_streams::readable::byte_stream_controller::ReadableByteStreamControllerImpl;
use web_streams::readable::default_controller::ReadableStreamDefaultControllerImpl;
use web_streams::readable::default_reader::DefaultReaderImpl;
use web_streams::readable::{
    DefaultReader, ReadableByteStreamController, ReadableStream, ReadableStreamDefaultController,
};
use web_streams::writable::default_controller::WritableStreamDefaultControllerImpl;
use web_streams::writable::default_writer::WritableStreamDefaultWriterImpl;
use web_streams::writable::{
    WritableStream, WritableStreamDefaultController, WritableStreamDefaultWriter,
};

use super::eos::{EosCallback, EosOptions, EosStateImpl};
use super::readable::{buffer_from, is_buffer, is_encoding};
use super::util::{is_destroyed, is_readable, is_writable, is_writable_ended, sides};
use crate::errors::{validate_plain_object, NodeError, Thrown};
use crate::support::{class_object, is_str, noop_function, rust_string};

/// Whether `value` is a `ReadableStream`, by its brand.
pub(crate) fn is_readable_stream(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    Object::from_value(scope, value).is_ok_and(|object| object.cast::<ReadableStream>().is_ok())
}

/// Whether `value` is a `WritableStream`, by its brand.
pub(crate) fn is_writable_stream(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    Object::from_value(scope, value).is_ok_and(|object| object.cast::<WritableStream>().is_ok())
}

/// `new Uint8Array(source)`: a copy of a `Buffer`, or a view of an `ArrayBuffer`.
fn new_uint8_array<'s>(
    scope: &'s Scope<'_>,
    source: HandleValue<'s>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let class = class_object(scope, js::class_spec::JSProtoKey::JSProto_Uint8Array)?;
    Function::construct(scope, class, &[source])?.to_jsval_throwing(scope)
}

/// `promise`, or a promise rejected with the pending exception if creating it threw.
fn or_rejected<'s>(
    scope: &'s Scope<'_>,
    promise: Result<js::Promise<'s>, ExnThrown>,
) -> Result<js::Promise<'s>, ExnThrown> {
    match promise {
        Ok(promise) => Ok(promise),
        Err(ExnThrown) => js::Promise::new_rejected_with_pending_error(scope),
    }
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
        Some(options) => validate_plain_object(scope, options, name, false),
        None => Ok(Object::new_plain(scope)?),
    }
}

/// `options[name]`, or `default` if it is `undefined`.
fn option_or<'s>(
    scope: &'s Scope<'_>,
    options: Object<'_>,
    name: &'static CStr,
    default: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let value = options.get_property(scope, name)?;
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
    let code = match Object::from_value(scope, cause).ok() {
        Some(object) => object.get_property(scope, c"code")?,
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
        let class = class_object(scope, js::class_spec::JSProtoKey::JSProto_TypeError)?;
        let error = Function::construct(
            scope,
            class,
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

/// The state of [`writable_to_web`]: the Node.js stream, the `WritableStream`'s controller, the
/// pending backpressure and close promises, and the `finished` it waits on.
#[core_runtime::jsclass(hidden)]
pub(crate) struct WritableToWebState {
    stream: Heap<js::object::Object>,
    controller: Option<Heap<WritableStreamDefaultControllerImpl>>,
    backpressure: Option<Heap<js::promise::Promise>>,
    closed: Option<Heap<js::promise::Promise>>,
    cleanup: Option<Heap<EosStateImpl>>,
}

#[core_runtime::jsmethods]
impl WritableToWebState {}

/// A new `WritableStream` that is closed.
fn closed_writable_stream<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let writable = WritableStream::new(scope, None, None)?;
    or_rejected(scope, writable.close(scope))?;
    Ok(*writable)
}

/// The underlying source or sink of an adapter: `methods` as functions with `state` as their
/// payload, on a null-prototype object.
fn algorithms<'s>(
    scope: &'s Scope<'_>,
    state: impl ToJSVal<'s> + Copy,
    methods: &[(&'static CStr, u32, Callback)],
) -> Result<Object<'s>, ExnThrown> {
    let object = Object::new_with_null_prototype(scope)?;
    for &(name, nargs, body) in methods {
        let function = Function::new_callback(scope, name, nargs, body, state)?;
        object.set_property(scope, name, *function)?;
    }
    Ok(object)
}

/// The `size` of the strategy of a `WritableStream` writing to a writable that isn't in object
/// mode: `chunk?.byteLength ?? chunk?.length ?? 1`.
fn chunk_size(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let chunk = args.get(0);
    if chunk.is_null_or_undefined() {
        return Ok(js::value::from_i32(1));
    }
    let object = Object::from_value_coerce(scope, chunk)?;
    for name in [c"byteLength", c"length"] {
        let value = object.get_property(scope, name)?;
        if !value.is_null_or_undefined() {
            return Ok(value.get());
        }
    }
    Ok(js::value::from_i32(1))
}

/// The `size` of a `ByteLengthQueuingStrategy`: `chunk.byteLength`.
fn byte_length_size(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let chunk = Object::from_value_coerce(scope, args.get(0))?;
    Ok(chunk.get_property(scope, c"byteLength")?.get())
}

/// The shared function running `body`.
fn shared_function<'s>(
    scope: &'s Scope<'_>,
    name: &CStr,
    body: Callback,
) -> Result<js::Callable<'s>, ExnThrown> {
    let function = js::class::get_or_init_shared_function(scope, body as usize, |scope| {
        Function::new_callback(scope, name, 1, body, ())
    })?;
    Ok(function.as_callable())
}

/// Node's `newWritableStreamFromStreamWritable`: a `WritableStream` writing to the Node.js
/// writable `stream`. Writes wait for `drain` once `stream.write()` returns `false`.
pub(crate) fn writable_to_web<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let candidate = Object::from_value(scope, stream)
        .ok()
        .filter(|_| js::conversion::to_boolean(stream));
    let stream_object = match candidate {
        Some(object)
            if crate::support::has_method(scope, object, c"write")?
                && crate::support::has_method(scope, object, c"on")? =>
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
        return Ok(closed_writable_stream(scope)?);
    }
    let high_water_mark = stream_object.get_property(scope, c"writableHighWaterMark")?;
    let high_water_mark = f64::from_jsval(scope, high_water_mark, ()).map_err(|_| ExnThrown)?;
    let object_mode =
        js::conversion::to_boolean(stream_object.get_property(scope, c"writableObjectMode")?);
    let strategy = QueuingStrategy {
        high_water_mark: Some(high_water_mark),
        size: if object_mode {
            None
        } else {
            Some(shared_function(scope, c"size", chunk_size)?)
        },
    };

    let state = js::class::create_instance_with::<WritableToWebStateImpl>(scope, |_| {
        WritableToWebStateImpl {
            stream: Heap::from(stream_object),
            controller: None,
            backpressure: None,
            closed: None,
            cleanup: None,
        }
    })?;
    let payload = state.to_jsval_throwing(scope)?;
    let cleanup = super::eos::eos_native(
        scope,
        stream_object,
        EosOptions::default(),
        EosCallback::Native(writable_to_web_finished, payload),
    )?;
    state.data_mut().cleanup = Some(Heap::from(cleanup));
    let on_drain = Function::new_callback(scope, c"onDrain", 0, writable_to_web_drain, state)?;
    crate::events::listen(scope, stream_object, c"on", c"drain", *on_drain)?;

    let sink = algorithms(
        scope,
        state,
        &[
            (c"start", 1, writable_to_web_start),
            (c"write", 1, writable_to_web_write),
            (c"abort", 1, writable_to_web_abort),
            (c"close", 0, writable_to_web_close),
        ],
    )?;
    let writable =
        WritableStream::new(scope, Some(sink.to_jsval_throwing(scope)?), Some(strategy))?;
    Ok(*writable)
}

/// The `finished` callback of [`writable_to_web`]: error the `WritableStream` with the stream's
/// error, or with an `AbortError` if the stream closed before `close()` ended it.
fn writable_to_web_finished(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WritableToWebState>(scope, payload);
    let error = if !args.is_empty() {
        args.get(0)
    } else {
        HandleValue::undefined()
    };
    let error = handle_known_internal_errors(scope, error)?;
    let (stream, cleanup) = {
        let data = state.data();
        (data.stream.get(scope), data.cleanup.get(scope))
    };
    if let Some(cleanup) = cleanup {
        super::eos::cleanup(scope, cleanup)?;
    }
    crate::events::listen(scope, stream, c"on", c"error", noop_function(scope)?)?;
    let (backpressure, closed) = {
        let mut data = state.data_mut();
        (data.backpressure.get(scope), data.closed.take_rooted(scope))
    };
    if !error.is_null_or_undefined() {
        if let Some(backpressure) = backpressure.filter(|promise| promise.is_pending()) {
            backpressure.reject(scope, error)?;
        }
        if let Some(closed) = closed.filter(|promise| promise.is_pending()) {
            closed.reject(scope, error)?;
        }
        let controller = state.data_mut().controller.take_rooted(scope);
        if let Some(controller) = controller {
            controller.error(scope, Some(error))?;
        }
        return Ok(js::value::undefined());
    }
    if let Some(closed) = closed {
        if closed.is_pending() {
            closed.resolve(scope, HandleValue::undefined())?;
        }
        return Ok(js::value::undefined());
    }
    let controller = state.data_mut().controller.take_rooted(scope);
    if let Some(controller) = controller {
        let error = crate::errors::abort_error(scope, None)?;
        controller.error(scope, Some(error))?;
    }
    Ok(js::value::undefined())
}

fn writable_to_web_drain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WritableToWebState>(scope, payload);
    let backpressure = state.data().backpressure.get(scope);
    if let Some(backpressure) = backpressure.filter(|promise| promise.is_pending()) {
        backpressure.resolve(scope, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

fn writable_to_web_start(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WritableToWebState>(scope, payload);
    let controller = Object::from_value(scope, args.get(0))
        .ok()
        .and_then(|controller| controller.cast::<WritableStreamDefaultController>().ok());
    state.data_mut().controller = controller.map(Heap::from);
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
    let state = cast_payload::<WritableToWebState>(scope, payload);
    let stream = state.data().stream.get(scope);
    let mut chunk = args.get(0);
    let object_mode =
        js::conversion::to_boolean(stream.get_property(scope, c"writableObjectMode")?);
    if !object_mode {
        if let Ok(object) = Object::from_value(scope, chunk) {
            if crate::inspect::is_array_buffer(object)
                || crate::inspect::is_shared_array_buffer(scope, object)?
            {
                chunk = new_uint8_array(scope, chunk)?;
            }
        }
    }
    let need_drain = js::conversion::to_boolean(stream.get_property(scope, c"writableNeedDrain")?);
    if !need_drain
        && js::conversion::to_boolean(Function::call_by_name(
            scope,
            stream.handle(),
            c"write",
            &[chunk],
        )?)
    {
        return Ok(js::value::undefined());
    }
    let backpressure = js::Promise::new_pending(scope)?;
    state.data_mut().backpressure = Some(Heap::from(backpressure));
    if !js::conversion::to_boolean(stream.get_property(scope, c"writableNeedDrain")?) {
        backpressure.resolve(scope, HandleValue::undefined())?;
    }
    let settled = Function::new_callback(scope, c"", 1, backpressure_settled, state)?;
    let failed = Function::new_callback(scope, c"", 1, backpressure_failed, state)?;
    let result = backpressure.then(scope, Some(*settled), Some(*failed))?;
    result.to_jsval_raw_throwing(scope)
}

/// The `finally` of the backpressure promise: clear it.
fn backpressure_settled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    cast_payload::<WritableToWebState>(scope, payload)
        .data_mut()
        .backpressure = None;
    Ok(js::value::undefined())
}

/// The `finally` of the backpressure promise after a rejection: clear it, and reject with the
/// reason.
fn backpressure_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    cast_payload::<WritableToWebState>(scope, payload)
        .data_mut()
        .backpressure = None;
    Err(js::exception::throw_value(scope, args.get(0)))
}

fn writable_to_web_abort(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<WritableToWebState>(scope, payload)
        .data()
        .stream
        .get(scope);
    super::destroy::destroyer(scope, stream.to_jsval_throwing(scope)?, args.get(0))?;
    Ok(js::value::undefined())
}

/// The sink's `close()`: end the stream, and return a promise that settles once it finished.
fn writable_to_web_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WritableToWebState>(scope, payload);
    let (stream, closing) = {
        let data = state.data();
        (data.stream.get(scope), data.closed.is_some())
    };
    if !closing && is_writable_ended(scope, stream.to_jsval_throwing(scope)?)? != Some(true) {
        let closed = js::Promise::new_pending(scope)?;
        state.data_mut().closed = Some(Heap::from(closed));
        Function::call_by_name(scope, stream.handle(), c"end", js::function::EmptyArgs)?;
        return closed.to_jsval_raw_throwing(scope);
    }
    state.data_mut().controller = None;
    js::Promise::new_resolved_with_value(scope, HandleValue::undefined())?
        .to_jsval_raw_throwing(scope)
}

/// The controller of a `ReadableStream`: a default controller, or a byte stream controller.
#[js::must_root]
#[derive(core_runtime::Traceable)]
enum Controller {
    Default(Heap<ReadableStreamDefaultControllerImpl>),
    Bytes(Heap<ReadableByteStreamControllerImpl>),
}

/// The rooted [`Controller`].
#[derive(Clone, Copy)]
enum RootedController<'s> {
    Default(ReadableStreamDefaultController<'s>),
    Bytes(ReadableByteStreamController<'s>),
}

/// The state of [`readable_to_web`]: the Node.js stream, the `ReadableStream`'s controller,
/// whether it was canceled, and the `finished` it waits on.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ReadableToWebState {
    stream: Heap<js::object::Object>,
    controller: Option<Controller>,
    cleanup: Option<Heap<EosStateImpl>>,
    #[no_trace]
    was_canceled: bool,
    #[no_trace]
    is_byob: bool,
    #[no_trace]
    object_mode: bool,
}

#[core_runtime::jsmethods]
impl ReadableToWebState {}

/// The controller of `state`'s `ReadableStream`.
fn controller_of<'s>(
    scope: &'s Scope<'_>,
    state: ReadableToWebState<'_>,
) -> Option<RootedController<'s>> {
    match &state.data().controller {
        Some(Controller::Default(controller)) => {
            Some(RootedController::Default(controller.get(scope)))
        }
        Some(Controller::Bytes(controller)) => Some(RootedController::Bytes(controller.get(scope))),
        None => None,
    }
}

/// `validateOneOf(type, 'options.type', ['bytes', undefined])`. Returns whether `type` is
/// `'bytes'`.
fn validate_type(scope: &Scope<'_>, kind: HandleValue<'_>) -> Result<bool, Thrown> {
    if kind.is_undefined() {
        return Ok(false);
    }
    if is_str(scope, kind, c"bytes")? {
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

/// An underlying source `{ type }`, with `type` `'bytes'` for a byte stream.
fn typed_source<'s>(scope: &'s Scope<'_>, kind: HandleValue<'_>) -> Result<Object<'s>, ExnThrown> {
    let source = Object::new_plain(scope)?;
    source.set_property(scope, c"type", kind)?;
    Ok(source)
}

/// Node's `newReadableStreamFromStreamReadable`: a `ReadableStream` reading from the Node.js
/// readable `stream`, a byte stream if `options.type` is `'bytes'`. `options.strategy` is its
/// queuing strategy, by default one with the stream's high water mark.
pub(crate) fn readable_to_web<'s>(
    scope: &'s Scope<'_>,
    stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let stream_object = Object::from_value(scope, stream).ok();
    let has_state = match stream_object {
        Some(object) => sides(scope, object)?.readable.is_some(),
        None => false,
    };
    let Some(stream_object) = stream_object.filter(|_| has_state) else {
        return Err(invalid_type(
            scope,
            "streamReadable",
            "an stream.Readable",
            stream,
        ));
    };
    let options = options_object(scope, options, "options")?;
    let kind = options.get_property(scope, c"type")?;
    let is_byob = validate_type(scope, kind)?;

    let readable = is_readable(scope, stream)? == Some(true);
    let object_mode =
        js::conversion::to_boolean(stream_object.get_property(scope, c"readableObjectMode")?);
    let state = js::class::create_instance_with::<ReadableToWebStateImpl>(scope, |_| {
        ReadableToWebStateImpl {
            stream: Heap::from(stream_object),
            controller: None,
            cleanup: None,
            was_canceled: false,
            is_byob,
            object_mode,
        }
    })?;
    let mut methods: Vec<(&'static CStr, u32, Callback)> = vec![
        (c"start", 1, readable_to_web_start),
        (c"cancel", 1, readable_to_web_cancel),
    ];
    let mut strategy = None;
    if readable {
        methods.push((c"pull", 0, readable_to_web_pull));
        let high_water_mark = stream_object.get_property(scope, c"readableHighWaterMark")?;
        let high_water_mark = f64::from_jsval(scope, high_water_mark, ()).map_err(|_| ExnThrown)?;
        let given = options.get_property(scope, c"strategy")?;
        strategy = Some(if is_byob {
            QueuingStrategy {
                high_water_mark: Some(high_water_mark),
                size: None,
            }
        } else if given.is_null_or_undefined() {
            QueuingStrategy {
                high_water_mark: Some(high_water_mark),
                size: if object_mode {
                    None
                } else {
                    Some(shared_function(scope, c"size", byte_length_size)?)
                },
            }
        } else {
            QueuingStrategy::from_jsval(scope, given, ()).map_err(|error| error.throw(scope))?
        });
    }
    let source = algorithms(scope, state, &methods)?;
    if is_byob {
        source.set_property(scope, c"type", "bytes")?;
    }
    let readable_stream =
        ReadableStream::new(scope, Some(source.to_jsval_throwing(scope)?), strategy)?;

    let payload = state.to_jsval_throwing(scope)?;
    let eos_options = EosOptions {
        writable: Some(false),
        synchronous: true,
        ..EosOptions::default()
    };
    let cleanup = super::eos::eos_native(
        scope,
        stream_object,
        eos_options,
        EosCallback::Native(readable_to_web_finished, payload),
    )?;
    state.data_mut().cleanup = Some(Heap::from(cleanup));

    if state.data().was_canceled {
        super::eos::cleanup(scope, cleanup)?;
    } else if readable {
        Function::call_by_name(
            scope,
            stream_object.handle(),
            c"pause",
            js::function::EmptyArgs,
        )?;
        let on_data = Function::new_callback(scope, c"onData", 1, readable_to_web_data, state)?;
        crate::events::listen(scope, stream_object, c"on", c"data", *on_data)?;
    }
    Ok(*readable_stream)
}

fn readable_to_web_start(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ReadableToWebState>(scope, payload);
    if let Ok(controller) = Object::from_value(scope, args.get(0)) {
        if let Ok(controller) = controller.cast::<ReadableStreamDefaultController>() {
            state.data_mut().controller = Some(Controller::Default(Heap::from(controller)));
        } else if let Ok(controller) = controller.cast::<ReadableByteStreamController>() {
            state.data_mut().controller = Some(Controller::Bytes(Heap::from(controller)));
        }
    }
    Ok(js::value::undefined())
}

fn readable_to_web_cancel(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ReadableToWebState>(scope, payload);
    state.data_mut().was_canceled = true;
    let stream = state.data().stream.get(scope);
    super::destroy::destroyer(scope, stream.to_jsval_throwing(scope)?, args.get(0))?;
    Ok(js::value::undefined())
}

fn readable_to_web_pull(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<ReadableToWebState>(scope, payload)
        .data()
        .stream
        .get(scope);
    Function::call_by_name(scope, stream.handle(), c"resume", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// The `data` listener of [`readable_to_web`]: enqueue the chunk, as a copy if it is a `Buffer`
/// outside object mode, and pause the stream once the queue is full.
fn readable_to_web_data(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ReadableToWebState>(scope, payload);
    let mut chunk = args.get(0);
    if is_buffer(scope, chunk)? && !state.data().object_mode {
        chunk = new_uint8_array(scope, chunk)?;
    }
    let desired = match controller_of(scope, state) {
        Some(RootedController::Default(controller)) => {
            controller.enqueue(scope, chunk)?;
            controller.desired_size(scope)
        }
        Some(RootedController::Bytes(controller)) => {
            let view = js::ArrayBufferView::from_value(scope, chunk).ok_or_else(|| {
                js::error::TypeError(String::from("chunk is not an ArrayBufferView")).throw(scope)
            })?;
            controller.enqueue(scope, view)?;
            controller.desired_size(scope)
        }
        None => {
            return Err(js::error::TypeError(String::from("controller is undefined")).throw(scope))
        }
    };
    if desired.is_none_or(|desired| desired <= 0.0) {
        let stream = state.data().stream.get(scope);
        Function::call_by_name(scope, stream.handle(), c"pause", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

/// The key of the property marking a stream that has the no-op `error` listener
/// [`readable_to_web`] adds.
static ERROR_SENTINEL_ATTACHED: js::symbol::CachedSymbol =
    js::symbol::CachedSymbol::new(c"kErrorSentinelAttached");

/// The `finished` callback of [`readable_to_web`]: error or close the `ReadableStream` unless it
/// was canceled.
fn readable_to_web_finished(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ReadableToWebState>(scope, payload);
    let error = if !args.is_empty() {
        args.get(0)
    } else {
        HandleValue::undefined()
    };
    let error = handle_known_internal_errors(scope, error)?;
    let (stream, cleanup) = {
        let data = state.data();
        (data.stream.get(scope), data.cleanup.get(scope))
    };
    if let Some(cleanup) = cleanup {
        super::eos::cleanup(scope, cleanup)?;
    }
    let key = ERROR_SENTINEL_ATTACHED.id(scope)?;
    if !stream.has_property_by_id(scope, key)? {
        crate::events::listen(scope, stream, c"on", c"error", noop_function(scope)?)?;
        stream.set_property_by_id(scope, key, true)?;
    }
    if state.data().was_canceled {
        return Ok(js::value::undefined());
    }
    state.data_mut().was_canceled = true;
    let controller = controller_of(scope, state);
    match controller {
        Some(RootedController::Default(controller)) => {
            if js::conversion::to_boolean(error) {
                controller.error(scope, Some(error))?;
            } else {
                controller.close(scope)?;
            }
        }
        Some(RootedController::Bytes(controller)) => {
            if js::conversion::to_boolean(error) {
                controller.error(scope, Some(error))?;
            } else {
                controller.close(scope)?;
                if state.data().is_byob {
                    if let Some(request) = controller.byob_request(scope) {
                        request.respond(scope, js::conversion::EnforceRange(0))?;
                    }
                }
            }
        }
        None => {
            return Err(js::error::TypeError(String::from("controller is undefined")).throw(scope))
        }
    }
    Ok(js::value::undefined())
}

/// The state of the `fromWeb` adapters: the Node.js stream they return, the writer and the reader
/// of the web streams, and whether those closed.
#[core_runtime::jsclass(hidden)]
pub(crate) struct FromWebState {
    stream: Option<Heap<js::object::Object>>,
    writer: Option<Heap<WritableStreamDefaultWriterImpl>>,
    reader: Option<Heap<DefaultReaderImpl>>,
    #[no_trace]
    writable_closed: bool,
    #[no_trace]
    readable_closed: bool,
    #[no_trace]
    decode_strings: bool,
    #[no_trace]
    object_mode: bool,
}

#[core_runtime::jsmethods]
impl FromWebState {}

/// The writer of `state`.
fn writer_of<'s>(
    scope: &'s Scope<'_>,
    state: FromWebState<'_>,
) -> Result<WritableStreamDefaultWriter<'s>, ExnThrown> {
    state
        .data()
        .writer
        .get(scope)
        .ok_or_else(|| js::error::TypeError(String::from("no writer")).throw(scope))
}

/// The reader of `state`.
fn reader_of<'s>(
    scope: &'s Scope<'_>,
    state: FromWebState<'_>,
) -> Result<DefaultReader<'s>, ExnThrown> {
    state
        .data()
        .reader
        .get(scope)
        .ok_or_else(|| js::error::TypeError(String::from("no reader")).throw(scope))
}

/// The Node.js stream of `state`.
fn stream_of<'s>(scope: &'s Scope<'_>, state: FromWebState<'_>) -> HandleValue<'s> {
    match state.data().stream.get(scope) {
        Some(stream) => scope.root_value(stream.as_value()),
        None => HandleValue::undefined(),
    }
}

/// The options of the Node.js stream a `fromWeb` adapter creates: the properties in `values`, and
/// the functions in `methods` with `state` as their payload.
fn stream_options<'s>(
    scope: &'s Scope<'_>,
    state: FromWebState<'_>,
    values: &[(&'static CStr, HandleValue<'_>)],
    methods: &[(&'static CStr, u32, Callback)],
) -> Result<HandleValue<'s>, ExnThrown> {
    let options = Object::new_plain(scope)?;
    for &(name, value) in values {
        options.set_property(scope, name, value)?;
    }
    for &(name, nargs, body) in methods {
        let function = Function::new_callback(scope, name, nargs, body, state)?;
        options.set_property(scope, name, *function)?;
    }
    options.to_jsval_throwing(scope)
}

/// A new [`FromWebState`].
fn new_from_web_state<'s>(
    scope: &'s Scope<'_>,
    writer: Option<WritableStreamDefaultWriter<'_>>,
    reader: Option<DefaultReader<'_>>,
    decode_strings: bool,
    object_mode: bool,
) -> Result<FromWebState<'s>, ExnThrown> {
    js::class::create_instance_with::<FromWebStateImpl>(scope, |_| FromWebStateImpl {
        stream: None,
        writer: writer.map(Heap::from),
        reader: reader.map(Heap::from),
        writable_closed: false,
        readable_closed: false,
        decode_strings,
        object_mode,
    })
}

/// The writer of `writable_stream`, which locks it: `writableStream.getWriter()`.
fn get_writer<'s>(
    scope: &'s Scope<'_>,
    writable_stream: HandleValue<'_>,
) -> Result<WritableStreamDefaultWriter<'s>, ExnThrown> {
    let stream = Object::from_value(scope, writable_stream)
        .expect("streams are objects")
        .cast::<WritableStream>()
        .expect("the stream is a WritableStream");
    stream.get_writer(scope)
}

/// The reader of `readable_stream`, which locks it: `readableStream.getReader()`.
fn get_reader<'s>(
    scope: &'s Scope<'_>,
    readable_stream: HandleValue<'_>,
) -> Result<DefaultReader<'s>, ExnThrown> {
    let stream = Object::from_value(scope, readable_stream)
        .expect("streams are objects")
        .cast::<ReadableStream>()
        .expect("the stream is a ReadableStream");
    web_streams::readable::native_read::acquire_native_reader(scope, &stream)
}

/// Node's `newStreamWritableFromWritableStream`: a Node.js `Writable` writing to the
/// `WritableStream` `writable_stream` through a writer it locks.
pub(crate) fn writable_from_web<'s>(
    scope: &'s Scope<'_>,
    writable_stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    if !is_writable_stream(scope, writable_stream) {
        return Err(invalid_type(
            scope,
            "writableStream",
            "an instance of WritableStream",
            writable_stream,
        ));
    }
    let options = options_object(scope, options, "options")?;
    let high_water_mark = options.get_property(scope, c"highWaterMark")?;
    let decode_strings = option_or(scope, options, c"decodeStrings", true)?;
    let object_mode = option_or(scope, options, c"objectMode", false)?;
    let signal = options.get_property(scope, c"signal")?;
    let object_mode_flag =
        crate::errors::validate_boolean(scope, object_mode, "options.objectMode")?;
    let decode_strings_flag =
        crate::errors::validate_boolean(scope, decode_strings, "options.decodeStrings")?;

    let writer = get_writer(scope, writable_stream)?;
    let state = new_from_web_state(
        scope,
        Some(writer),
        None,
        decode_strings_flag,
        object_mode_flag,
    )?;
    let stream_options = stream_options(
        scope,
        state,
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
    state.data_mut().stream = Some(Heap::from(writable));
    watch_writer_closed(scope, state)?;
    Ok(writable)
}

/// Node's `newStreamReadableFromReadableStream`: a Node.js `Readable` reading from the
/// `ReadableStream` `readable_stream` through a reader it locks.
pub(crate) fn readable_from_web<'s>(
    scope: &'s Scope<'_>,
    readable_stream: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    if !is_readable_stream(scope, readable_stream) {
        return Err(invalid_type(
            scope,
            "readableStream",
            "an instance of ReadableStream",
            readable_stream,
        ));
    }
    let options = options_object(scope, options, "options")?;
    let high_water_mark = options.get_property(scope, c"highWaterMark")?;
    let encoding = options.get_property(scope, c"encoding")?;
    let object_mode = option_or(scope, options, c"objectMode", false)?;
    let signal = options.get_property(scope, c"signal")?;
    validate_encoding(scope, encoding)?;
    crate::errors::validate_boolean(scope, object_mode, "options.objectMode")?;

    let reader = get_reader(scope, readable_stream)?;
    let state = new_from_web_state(scope, None, Some(reader), false, false)?;
    let stream_options = stream_options(
        scope,
        state,
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
    state.data_mut().stream = Some(Heap::from(readable));
    watch_reader_closed(scope, state)?;
    Ok(readable)
}

/// Node's `newStreamDuplexFromReadableWritablePair`: a Node.js `Duplex` reading from
/// `pair.readable` and writing to `pair.writable`.
pub(crate) fn duplex_from_web<'s>(
    scope: &'s Scope<'_>,
    pair: Option<HandleValue<'_>>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let pair = options_object(scope, pair, "pair")?;
    let readable_stream = pair.get_property(scope, c"readable")?;
    let writable_stream = pair.get_property(scope, c"writable")?;
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
    let encoding = options.get_property(scope, c"encoding")?;
    let decode_strings = option_or(scope, options, c"decodeStrings", true)?;
    let high_water_mark = options.get_property(scope, c"highWaterMark")?;
    let signal = options.get_property(scope, c"signal")?;
    let object_mode_flag =
        crate::errors::validate_boolean(scope, object_mode, "options.objectMode")?;
    validate_encoding(scope, encoding)?;

    let writer = get_writer(scope, writable_stream)?;
    let reader = get_reader(scope, readable_stream)?;
    let state = new_from_web_state(
        scope,
        Some(writer),
        Some(reader),
        js::conversion::to_boolean(decode_strings),
        object_mode_flag,
    )?;
    let stream_options = stream_options(
        scope,
        state,
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
    state.data_mut().stream = Some(Heap::from(duplex));
    watch_writer_closed(scope, state)?;
    watch_reader_closed(scope, state)?;
    Ok(duplex)
}

/// `promise.then(fulfilled, rejected)`, with `state` as the reactions' payload.
fn react<'s>(
    scope: &'s Scope<'_>,
    promise: js::Promise<'_>,
    state: impl ToJSVal<'s> + Copy,
    fulfilled: Callback,
    rejected: Callback,
) -> Result<(), ExnThrown> {
    let fulfilled = Function::new_callback(scope, c"", 1, fulfilled, state)?;
    let rejected = Function::new_callback(scope, c"", 1, rejected, state)?;
    promise.then(scope, Some(*fulfilled), Some(*rejected))?;
    Ok(())
}

/// `writer.closed.then(...)`: destroy the stream with `ERR_STREAM_PREMATURE_CLOSE` if the
/// `WritableStream` closes before the stream ended, or with the error it errors with.
fn watch_writer_closed(scope: &Scope<'_>, state: FromWebState<'_>) -> Result<(), ExnThrown> {
    let closed = writer_of(scope, state)?.closed(scope);
    react(scope, closed, state, writer_closed, web_stream_failed)
}

/// `reader.closed.then(...)`: destroy the stream with the error the `ReadableStream` errors
/// with.
fn watch_reader_closed(scope: &Scope<'_>, state: FromWebState<'_>) -> Result<(), ExnThrown> {
    let closed = reader_of(scope, state)?.closed(scope);
    react(scope, closed, state, reader_closed, web_stream_failed)
}

fn writer_closed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    state.data_mut().writable_closed = true;
    let stream = stream_of(scope, state);
    if is_writable_ended(scope, stream)? != Some(true) {
        let error = super::eos::premature_close().to_value(scope)?;
        super::destroy::destroyer(scope, stream, error)?;
    }
    Ok(js::value::undefined())
}

fn reader_closed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    cast_payload::<FromWebState>(scope, payload)
        .data_mut()
        .readable_closed = true;
    Ok(js::value::undefined())
}

/// The rejection of `writer.closed` or `reader.closed`: mark both sides closed, and destroy the
/// stream with the error.
fn web_stream_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    {
        let mut data = state.data_mut();
        data.writable_closed = true;
        data.readable_closed = true;
    }
    super::destroy::destroyer(scope, stream_of(scope, state), args.get(0))?;
    Ok(js::value::undefined())
}

/// How [`done`] handles an exception the callback of a `fromWeb` stream's method throws.
#[derive(Clone, Copy, Default)]
enum OnThrow {
    /// Destroy the stream with it.
    #[default]
    Destroy,
    /// Destroy the stream with it in a tick.
    DestroyInTick,
}

/// A call of a `fromWeb` stream's `write`, `writev` or `final`: the adapter state, the chunk or
/// chunks to write, the callback, and how an exception the callback throws is handled.
#[core_runtime::jsclass(hidden)]
pub(crate) struct WebWrite {
    state: Heap<FromWebStateImpl>,
    chunk: Heap<Value>,
    callback: Heap<Value>,
    #[no_trace]
    on_throw: OnThrow,
}

#[core_runtime::jsmethods]
impl WebWrite {}

/// A new [`WebWrite`].
fn new_web_write<'s>(
    scope: &'s Scope<'_>,
    state: FromWebState<'_>,
    chunk: HandleValue<'_>,
    callback: HandleValue<'_>,
    on_throw: OnThrow,
) -> Result<WebWrite<'s>, ExnThrown> {
    js::class::create_instance_with::<WebWriteImpl>(scope, |_| WebWriteImpl {
        state: Heap::from(state),
        chunk: Heap::from(chunk.get()),
        callback: Heap::from(callback.get()),
        on_throw,
    })
}

fn done_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    done(scope, cast_payload::<WebWrite>(scope, payload), None)
}

fn done_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    done(
        scope,
        cast_payload::<WebWrite>(scope, payload),
        Some(args.get(0)),
    )
}

/// Node's `done(error)` of the `fromWeb` streams' methods.
fn done(
    scope: &Scope<'_>,
    write: WebWrite<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<Value, ExnThrown> {
    let (state, callback, on_throw) = {
        let data = write.data();
        (
            data.state.get(scope),
            data.callback.get(scope),
            data.on_throw,
        )
    };
    let args: Vec<HandleValue<'_>> = error.into_iter().collect();
    if Function::call(scope, HandleValue::undefined(), callback, &args).is_err() {
        let thrown = js::exception::take_pending_or_undefined(scope);
        let stream = stream_of(scope, state);
        match on_throw {
            OnThrow::Destroy => super::destroy::destroyer(scope, stream, thrown)?,
            OnThrow::DestroyInTick => {
                js::jobs::queue_native_tick(scope, destroy_in_tick, stream, &[thrown])?
            }
        }
    }
    Ok(js::value::undefined())
}

/// `process.nextTick(() => destroy(stream, error))`, whose payload is the stream and whose
/// argument is the error.
fn destroy_in_tick(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    super::destroy::destroyer(scope, payload, args.get(0))?;
    Ok(js::value::undefined())
}

/// `writer.ready.then(after).then(done, done)`, with `write` as the payload of `after` and of
/// `done`.
fn after_ready(scope: &Scope<'_>, write: WebWrite<'_>, after: Callback) -> Result<(), ExnThrown> {
    let state = write.data().state.get(scope);
    let ready = writer_of(scope, state)?.ready(scope);
    let after = Function::new_callback(scope, c"", 1, after, write)?;
    let written = ready.then(scope, Some(*after), None)?;
    react(scope, written, write, done_fulfilled, done_rejected)
}

/// The `writev(chunks, callback)` of the `fromWeb` streams: write each chunk once the writer is
/// ready.
fn web_writev(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    let write = new_web_write(
        scope,
        state,
        args.get(0),
        args.get(1),
        OnThrow::DestroyInTick,
    )?;
    after_ready(scope, write, write_all)?;
    Ok(js::value::undefined())
}

/// `SafePromiseAllReturnVoid(chunks, (data) => writer.write(data.chunk))`, whose payload is the
/// [`WebWrite`].
fn write_all(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let write = cast_payload::<WebWrite>(scope, payload);
    let (state, chunks) = {
        let data = write.data();
        (data.state.get(scope), data.chunk.get(scope))
    };
    let writer = writer_of(scope, state)?;
    let chunks = Object::from_value_coerce(scope, chunks)?;
    let length = chunks.get_property(scope, c"length")?;
    let length = f64::from_jsval(scope, length, ()).map_err(|_| ExnThrown)? as u32;
    let mut writes = Vec::new();
    for index in 0..length {
        let data = chunks.get_element(scope, index)?;
        let chunk = Object::from_value_coerce(scope, data)?.get_property(scope, c"chunk")?;
        writes.push(or_rejected(scope, writer.write(scope, Some(chunk)))?);
    }
    let handles: Vec<_> = writes.iter().map(|promise| promise.handle()).collect();
    let all = js::Promise::wait_for_all_from(scope, &handles)?;
    let fulfilled = noop_function(scope)?;
    all.then(scope, Some(fulfilled), None)?
        .to_jsval_raw_throwing(scope)
}

/// The `write(chunk, encoding, callback)` of the `fromWeb` streams: write `chunk` once the writer
/// is ready, as a `Uint8Array` if it is a string to decode.
fn web_write(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    let mut chunk = args.get(0);
    let (decode_strings, object_mode) = {
        let data = state.data();
        (data.decode_strings, data.object_mode)
    };
    if chunk.is_string() && decode_strings && !object_mode {
        let buffer = buffer_from(scope, &[chunk, args.get(1)])?;
        let buffer = js::ArrayBufferView::from_value(scope, buffer).expect("buffers are views");
        let view = js::Uint8Array::with_buffer(
            scope,
            buffer.viewed_buffer(scope)?,
            buffer.byte_offset(),
            buffer.byte_length(),
        )?;
        chunk = scope.root_value(view.as_value());
    }
    let write = new_web_write(scope, state, chunk, args.get(2), OnThrow::Destroy)?;
    after_ready(scope, write, write_one)?;
    Ok(js::value::undefined())
}

/// `writer.write(chunk)`, whose payload is the [`WebWrite`].
fn write_one(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let write = cast_payload::<WebWrite>(scope, payload);
    let (state, chunk) = {
        let data = write.data();
        (data.state.get(scope), data.chunk.get(scope))
    };
    let writer = writer_of(scope, state)?;
    or_rejected(scope, writer.write(scope, Some(chunk)))?.to_jsval_raw_throwing(scope)
}

/// The `final(callback)` of the `fromWeb` streams: close the writer unless the `WritableStream`
/// closed.
fn web_final(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    if !state.data().writable_closed {
        let closed = or_rejected(scope, writer_of(scope, state)?.close(scope))?;
        let write = new_web_write(
            scope,
            state,
            HandleValue::undefined(),
            args.get(0),
            OnThrow::DestroyInTick,
        )?;
        react(scope, closed, write, done_fulfilled, done_rejected)?;
    }
    Ok(js::value::undefined())
}

/// The `read()` of the `fromWeb` streams: push the reader's next chunk, or the end.
fn web_read(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    let read = or_rejected(scope, reader_of(scope, state)?.read(scope))?;
    react(scope, read, state, web_read_result, web_read_failed)?;
    Ok(js::value::undefined())
}

fn web_read_result(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    let stream = Object::from_value(scope, stream_of(scope, state))
        .map_err(|_| js::error::TypeError(String::from("no stream")).throw(scope))?;
    let result = Object::from_value_coerce(scope, args.get(0))?;
    if js::conversion::to_boolean(result.get_property(scope, c"done")?) {
        Function::call_by_name(scope, stream.handle(), c"push", &[HandleValue::null()])?;
    } else {
        Function::call_by_name(
            scope,
            stream.handle(),
            c"push",
            &[result.get_property(scope, c"value")?],
        )?;
    }
    Ok(js::value::undefined())
}

fn web_read_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    super::destroy::destroyer(scope, stream_of(scope, state), args.get(0))?;
    Ok(js::value::undefined())
}

/// The state of the `destroy(error, callback)` of the `fromWeb` streams: the callback and the
/// error.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DestroyDone {
    callback: Heap<Value>,
    error: Heap<Value>,
}

#[core_runtime::jsmethods]
impl DestroyDone {}

/// The function calling `callback` with `error`, for the promise of `destroy(error, callback)`.
fn destroy_done_function<'s>(
    scope: &'s Scope<'_>,
    callback: HandleValue<'_>,
    error: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let state = js::class::create_instance_with::<DestroyDoneImpl>(scope, |_| DestroyDoneImpl {
        callback: Heap::from(callback.get()),
        error: Heap::from(error.get()),
    })?;
    Ok(*Function::new_callback(scope, c"", 0, destroy_done, state)?)
}

/// Node's `done()` of the `fromWeb` streams' `destroy`, whose payload is a [`DestroyDone`]: call
/// the callback with the error, and throw an exception it throws in a tick.
fn destroy_done(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<DestroyDone>(scope, payload);
    let (callback, error) = {
        let data = state.data();
        (data.callback.get(scope), data.error.get(scope))
    };
    if Function::call(scope, HandleValue::undefined(), callback, &[error]).is_err() {
        let thrown = js::exception::take_pending_or_undefined(scope);
        js::jobs::queue_native_tick(scope, crate::support::throw_in_tick, thrown, &[])?;
    }
    Ok(js::value::undefined())
}

/// Call `done` with no arguments.
fn call_done(scope: &Scope<'_>, done: Object<'_>) -> Result<(), ExnThrown> {
    Function::call(
        scope,
        HandleValue::undefined(),
        done.to_jsval_throwing(scope)?,
        js::function::EmptyArgs,
    )?;
    Ok(())
}

/// The `destroy(error, callback)` of [`writable_from_web`]: abort the writer with `error`, or
/// close it without one, unless the `WritableStream` closed.
fn writable_from_web_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    let (error, callback) = (args.get(0), args.get(1));
    let done = destroy_done_function(scope, callback, error)?;
    if !state.data().writable_closed {
        let writer = writer_of(scope, state)?;
        let closed = if error.is_null_or_undefined() {
            writer.close(scope)
        } else {
            writer.abort(scope, error)
        };
        or_rejected(scope, closed)?.then(scope, Some(done), Some(done))?;
        return Ok(js::value::undefined());
    }
    call_done(scope, done)?;
    Ok(js::value::undefined())
}

/// The `destroy(error, callback)` of [`readable_from_web`]: cancel the reader with `error` unless
/// the `ReadableStream` closed.
fn readable_from_web_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    let (error, callback) = (args.get(0), args.get(1));
    let done = destroy_done_function(scope, callback, error)?;
    if !state.data().readable_closed {
        let canceled = reader_of(scope, state)?.cancel(scope, Some(error));
        or_rejected(scope, canceled)?.then(scope, Some(done), Some(done))?;
        return Ok(js::value::undefined());
    }
    call_done(scope, done)?;
    Ok(js::value::undefined())
}

/// The `destroy(error, callback)` of [`duplex_from_web`]: abort the writer and cancel the reader
/// with `error`, each unless its stream closed.
fn duplex_from_web_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FromWebState>(scope, payload);
    let (error, callback) = (args.get(0), args.get(1));
    let done = destroy_done_function(scope, callback, error)?;
    let (writable_closed, readable_closed) = {
        let data = state.data();
        (data.writable_closed, data.readable_closed)
    };
    if writable_closed && readable_closed {
        call_done(scope, done)?;
        return Ok(js::value::undefined());
    }
    let writer = if writable_closed {
        js::Promise::new_resolved_with_value(scope, HandleValue::undefined())?
    } else {
        let aborted = or_rejected(scope, writer_of(scope, state)?.abort(scope, error))?;
        js::Promise::call_original_resolve(scope, aborted)?
    };
    let reader = if readable_closed {
        js::Promise::new_resolved_with_value(scope, HandleValue::undefined())?
    } else {
        let canceled = or_rejected(scope, reader_of(scope, state)?.cancel(scope, Some(error)))?;
        js::Promise::call_original_resolve(scope, canceled)?
    };
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
    let has_states = match Object::from_value(scope, duplex) {
        Ok(object) => {
            let sides = sides(scope, object)?;
            sides.readable.is_some() && sides.writable.is_some()
        }
        Err(_) => false,
    };
    if !has_states {
        return Err(invalid_type(scope, "duplex", "an stream.Duplex", duplex));
    }
    let options = options_object(scope, options, "options")?;
    let mut kind = options.get_property(scope, c"readableType")?;
    if kind.is_null_or_undefined() {
        kind = options.get_property(scope, c"type")?;
    }
    let readable_options = Object::new_with_null_prototype(scope)?;
    readable_options.set_property(scope, c"type", kind)?;
    let readable_options_value = readable_options.to_jsval_throwing(scope)?;
    let canceled_readable = || -> Result<Object<'_>, ExnThrown> {
        let source = typed_source(scope, kind)?.to_jsval_throwing(scope)?;
        let readable = ReadableStream::new(scope, Some(source), None)?;
        or_rejected(scope, readable.cancel(scope, None))?;
        Ok(*readable)
    };

    let pair = Object::new_plain(scope)?;
    if is_destroyed(scope, duplex)? == Some(true) {
        let writable = closed_writable_stream(scope)?;
        let readable = canceled_readable()?;
        pair.set_property(scope, c"readable", readable)?;
        pair.set_property(scope, c"writable", writable)?;
        return Ok(pair);
    }

    let writable = if is_writable(scope, duplex)? == Some(true) {
        writable_to_web(scope, duplex)?
    } else {
        closed_writable_stream(scope)?
    };
    let readable = if is_readable(scope, duplex)? == Some(true) {
        readable_to_web(scope, duplex, Some(readable_options_value))?
    } else {
        canceled_readable()?
    };
    pair.set_property(scope, c"writable", writable)?;
    pair.set_property(scope, c"readable", readable)?;
    Ok(pair)
}
