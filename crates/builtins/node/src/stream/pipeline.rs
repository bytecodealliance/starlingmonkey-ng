// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `pipeline`: Node's `lib/internal/streams/pipeline.js`, for chains of Node streams, iterables and
//! functions. An async iterable a function stage returns becomes a stream through
//! `Readable.from`. Web streams aren't accepted.

use std::collections::VecDeque;

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::eos::{eos_native, premature_close, EosCallback, EosOptions, EosStateImpl};
use super::util::{
    is_iterable, is_node_stream, is_readable, is_readable_finished, is_readable_node_stream, sides,
    Field,
};
use crate::errors::{validate_abort_signal, validate_function, ErrorKind, NodeError, Thrown};
use crate::support::{is_str, rust_string};
use web_globals::signals::abort_controller::{AbortController, AbortControllerImpl};

/// A cleanup the pipeline runs for its last stream once it finished without an error: an `eos`
/// cleanup, or the removal of its `error` listener from a stream.
#[js::must_root]
#[derive(core_runtime::Traceable)]
enum Cleanup {
    Eos(Heap<EosStateImpl>),
    ErrorListener {
        stream: Heap<js::object::Object>,
        listener: Heap<js::object::Object>,
    },
}

/// The state of a `pipeline` call: Node's `pipelineImpl` variables.
#[core_runtime::jsclass(hidden)]
pub(crate) struct PipelineState {
    /// The callback, unless the pipeline settles `promise`.
    callback: Heap<Value>,
    promise: Option<Heap<js::promise::Promise>>,
    error: Heap<Value>,
    value: Heap<Value>,
    #[no_trace]
    finish_count: f64,
    destroys: VecDeque<Heap<DestroyerStateImpl>>,
    last_cleanups: Vec<Cleanup>,
    #[no_trace]
    called: bool,
    controller: Option<Heap<AbortControllerImpl>>,
    outer_signal: Option<Heap<js::object::Object>>,
    abort_listener: Option<Heap<js::object::Object>>,
    /// Node's `finish`, as a function.
    finish_fn: Option<Heap<js::object::Object>>,
    /// The `error` listener the pipeline adds to each stream.
    error_fn: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl PipelineState {}

/// Node's `finishImpl`: record the first error, destroy the streams once there is an error or the
/// pipeline is done, and call the callback once it is done.
fn finish_impl(
    scope: &Scope<'_>,
    state: PipelineState<'_>,
    error: HandleValue<'_>,
    done: bool,
) -> Result<(), ExnThrown> {
    let recorded = state.data().error.get(scope);
    if js::conversion::to_boolean(error) {
        let replaceable = match Object::from_value(scope, recorded).ok() {
            _ if !js::conversion::to_boolean(recorded) => true,
            Some(recorded) => {
                is_str(
                    scope,
                    recorded.get_property(scope, c"code")?,
                    c"ERR_STREAM_PREMATURE_CLOSE",
                )? || is_str(scope, recorded.get_property(scope, c"name")?, c"AbortError")?
            }
            None => false,
        };
        if replaceable {
            state.data().error.set(error.get());
        }
    }
    let error = state.data().error.get(scope);
    if !js::conversion::to_boolean(error) && !done {
        return Ok(());
    }
    loop {
        let destroyer = {
            let mut data = state.data_mut();
            let destroyer = data.destroys.front().map(|destroyer| destroyer.get(scope));
            drop(data.destroys.pop_front());
            destroyer
        };
        let Some(destroyer) = destroyer else {
            break;
        };
        destroy_stream(scope, destroyer, error)?;
    }
    let (signal, listener, controller) = {
        let data = state.data();
        (
            data.outer_signal.get(scope),
            data.abort_listener.get(scope),
            data.controller.get(scope),
        )
    };
    if let (Some(signal), Some(listener)) = (signal, listener) {
        crate::events::listen(scope, signal, c"removeEventListener", c"abort", listener)?;
    }
    if let Some(controller) = controller {
        controller.js_abort(scope, HandleValue::undefined())?;
    }
    if done && !state.data().called {
        state.data_mut().called = true;
        if !js::conversion::to_boolean(error) {
            run_last_cleanups(scope, state)?;
        }
        let error = if js::conversion::to_boolean(error) {
            error
        } else {
            HandleValue::undefined()
        };
        let (callback, promise, value) = {
            let data = state.data();
            (
                data.callback.get(scope),
                data.promise.get(scope),
                data.value.get(scope),
            )
        };
        match promise {
            Some(promise) => {
                js::jobs::queue_native_tick(scope, settle, promise, &[error, value])?;
            }
            None => js::jobs::queue_tick(scope, callback, &[error, value])?,
        }
    }
    Ok(())
}

/// Run the cleanups of the last stream.
fn run_last_cleanups(scope: &Scope<'_>, state: PipelineState<'_>) -> Result<(), ExnThrown> {
    let count = state.data().last_cleanups.len();
    for index in 0..count {
        enum Rooted<'r> {
            Eos(super::eos::EosState<'r>),
            ErrorListener(Object<'r>, Object<'r>),
        }
        let cleanup = {
            let data = state.data();
            match &data.last_cleanups[index] {
                Cleanup::Eos(eos) => Rooted::Eos(eos.get(scope)),
                Cleanup::ErrorListener { stream, listener } => {
                    Rooted::ErrorListener(stream.get(scope), listener.get(scope))
                }
            }
        };
        match cleanup {
            Rooted::Eos(eos) => super::eos::cleanup(scope, eos)?,
            Rooted::ErrorListener(stream, listener) => {
                crate::events::listen(scope, stream, c"removeListener", c"error", listener)?
            }
        }
    }
    Ok(())
}

/// Node's `finish`, whose payload is the [`PipelineState`] and whose argument is the error.
fn finish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipelineState>(scope, payload);
    state.data_mut().finish_count -= 1.0;
    let done = state.data().finish_count == 0.0;
    finish_impl(scope, state, args.get(0), done)?;
    Ok(js::value::undefined())
}

/// The outer signal's abort listener, whose payload is the [`PipelineState`].
fn abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipelineState>(scope, payload);
    let signal = state.data().outer_signal.get(scope);
    let reason = match signal {
        Some(signal) => Some(signal.get_property(scope, c"reason")?),
        None => None,
    };
    let error = crate::errors::abort_error(scope, reason)?;
    finish_impl(scope, state, error, true)?;
    Ok(js::value::undefined())
}

/// The state of Node's `destroyer`: the stream, and whether it finished.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DestroyerState {
    stream: Heap<js::object::Object>,
    #[no_trace]
    finished: bool,
}

#[core_runtime::jsmethods]
impl DestroyerState {}

/// The listener for `close` of [`destroyer`], whose payload is a [`DestroyerState`].
fn mark_finished(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    cast_payload::<DestroyerState>(scope, payload)
        .data_mut()
        .finished = true;
    Ok(js::value::undefined())
}

/// The `finished` callback of [`destroyer`], whose payload is a [`DestroyerState`].
fn mark_finished_unless_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let error = !args.is_empty() && js::conversion::to_boolean(args.get(0));
    cast_payload::<DestroyerState>(scope, payload)
        .data_mut()
        .finished = !error;
    Ok(js::value::undefined())
}

/// The `destroy` of Node's `destroyer`: destroy the stream unless it finished.
fn destroy_stream(
    scope: &Scope<'_>,
    destroyer: DestroyerState<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if destroyer.data().finished {
        return Ok(());
    }
    destroyer.data_mut().finished = true;
    let error = if js::conversion::to_boolean(error) {
        scope.root_value(error.get())
    } else {
        super::destroy::stream_destroyed("pipe").to_value(scope)?
    };
    let stream = destroyer.data().stream.get(scope);
    super::destroy::destroyer(scope, stream.to_jsval_throwing(scope)?, error)
}

/// Node's `destroyer`: the state of the destruction of `stream` unless it finished, and the state
/// of the `finished` it uses.
fn destroyer<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    reading: bool,
    writing: bool,
) -> Result<(DestroyerState<'s>, super::eos::EosState<'s>), Thrown> {
    let state =
        js::class::create_instance_with::<DestroyerStateImpl>(scope, |_| DestroyerStateImpl {
            stream: Heap::from(stream),
            finished: false,
        })?;
    let on_close = Function::new_callback(scope, c"", 0, mark_finished, state)?;
    crate::events::listen(scope, stream, c"on", c"close", *on_close)?;
    let options = EosOptions {
        readable: Some(reading),
        writable: Some(writing),
        ..EosOptions::default()
    };
    let payload = state.to_jsval_throwing(scope)?;
    let cleanup = eos_native(
        scope,
        stream,
        options,
        EosCallback::Native(mark_finished_unless_error, payload),
    )?;
    Ok((state, cleanup))
}

/// The error listener the pipeline adds to each stream, whose payload is the [`PipelineState`]:
/// record errors other than aborts and premature closes.
fn on_stream_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let error = args.get(0);
    if let Ok(object) = Object::from_value(scope, error) {
        let name = rust_string(scope, object.get_property(scope, c"name")?);
        let code = rust_string(scope, object.get_property(scope, c"code")?);
        if name.as_deref() == Some("AbortError")
            || code.as_deref() == Some("ERR_STREAM_PREMATURE_CLOSE")
        {
            return Ok(js::value::undefined());
        }
    } else if !js::conversion::to_boolean(error) {
        return Ok(js::value::undefined());
    }
    finish_impl(
        scope,
        cast_payload::<PipelineState>(scope, payload),
        error,
        false,
    )?;
    Ok(js::value::undefined())
}

/// The state of Node's `pipe` of `src` into `dst`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct PipeLinkState {
    pipeline: Heap<PipelineStateImpl>,
    src: Heap<js::object::Object>,
    dst: Heap<js::object::Object>,
    /// Whether `src` ended `dst`.
    #[no_trace]
    ended: bool,
}

#[core_runtime::jsmethods]
impl PipeLinkState {}

/// The `close` listener [`pipe`] adds to `dst`, whose payload is a [`PipeLinkState`]: record a
/// premature close if `dst` closed before `src` ended.
fn pipe_dst_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let link = cast_payload::<PipeLinkState>(scope, payload);
    if !link.data().ended {
        let error = premature_close().to_value(scope)?;
        let pipeline = link.data().pipeline.get(scope);
        finish_impl(scope, pipeline, error, false)?;
    }
    Ok(js::value::undefined())
}

/// The function that ends `dst` once `src` ended, whose payload is a [`PipeLinkState`].
fn pipe_end(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let link = cast_payload::<PipeLinkState>(scope, payload);
    link.data_mut().ended = true;
    let dst = link.data().dst.get(scope);
    Function::call_by_name(scope, dst.handle(), c"end", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// The `finished` callback for the source of [`pipe`], whose payload is a [`PipeLinkState`]:
/// finish once the source ended, also after a premature close of a source that ended cleanly.
fn pipe_src_finished(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let link = cast_payload::<PipeLinkState>(scope, payload);
    let (pipeline, src) = {
        let data = link.data();
        (data.pipeline.get(scope), data.src.get(scope))
    };
    let error = if !args.is_empty() {
        args.get(0)
    } else {
        HandleValue::undefined()
    };
    let premature = Object::from_value(scope, error)
        .ok()
        .map(|error| error.get_property(scope, c"code"))
        .transpose()?
        .is_some_and(|code| {
            rust_string(scope, code).as_deref() == Some("ERR_STREAM_PREMATURE_CLOSE")
        });
    let clean_end = match sides(scope, src)?.readable {
        Some(r) => {
            r.truthy(scope, Field::Ended)?
                && !r.truthy(scope, Field::Errored)?
                && !r.truthy(scope, Field::ErrorEmitted)?
        }
        None => false,
    };
    if js::conversion::to_boolean(error) && premature && clean_end {
        let finish_fn = pipeline.data().finish_fn.get(scope).ok_or(ExnThrown)?;
        crate::events::listen(scope, src, c"once", c"end", finish_fn)?;
        crate::events::listen(scope, src, c"once", c"error", finish_fn)?;
    } else {
        finish_now(scope, pipeline, Some(error))?;
    }
    Ok(js::value::undefined())
}

/// Node's `pipe`: pipe `src` into `dst` and finish the pipeline's two counts once both have
/// finished. Returns the state of the `finished` on `dst`.
fn pipe<'s>(
    scope: &'s Scope<'_>,
    state: PipelineState<'_>,
    src: Object<'_>,
    dst: Object<'_>,
    end: bool,
) -> Result<super::eos::EosState<'s>, Thrown> {
    let link =
        js::class::create_instance_with::<PipeLinkStateImpl>(scope, |_| PipeLinkStateImpl {
            pipeline: Heap::from(state),
            src: Heap::from(src),
            dst: Heap::from(dst),
            ended: false,
        })?;
    let on_close = Function::new_callback(scope, c"", 0, pipe_dst_close, link)?;
    crate::events::listen(scope, dst, c"on", c"close", *on_close)?;
    let options = Object::new_plain(scope)?;
    options.set_property(scope, c"end", false)?;
    Function::call_by_name(
        scope,
        src.handle(),
        c"pipe",
        &[
            dst.to_jsval_throwing(scope)?,
            options.to_jsval_throwing(scope)?,
        ],
    )?;
    if end {
        let end_fn = Function::new_callback(scope, c"", 0, pipe_end, link)?;
        if is_readable_finished(scope, src.to_jsval_throwing(scope)?, true)? == Some(true) {
            js::jobs::queue_tick(scope, (*end_fn).to_jsval_throwing(scope)?, &[])?;
        } else {
            crate::events::listen(scope, src, c"once", c"end", *end_fn)?;
        }
    } else {
        finish_now(scope, state, None)?;
    }
    let link_payload = link.to_jsval_throwing(scope)?;
    eos_native(
        scope,
        src,
        EosOptions {
            readable: Some(true),
            writable: Some(false),
            ..EosOptions::default()
        },
        EosCallback::Native(pipe_src_finished, link_payload),
    )?;
    let payload = state.to_jsval_throwing(scope)?;
    eos_native(
        scope,
        dst,
        EosOptions {
            readable: Some(false),
            writable: Some(true),
            ..EosOptions::default()
        },
        EosCallback::Native(finish, payload),
    )
}

/// Call Node's `finish` with `error`.
fn finish_now(
    scope: &Scope<'_>,
    state: PipelineState<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let args: Vec<HandleValue<'_>> = error.into_iter().collect();
    finish(
        scope,
        CallbackArgs::from_values(&args),
        state.to_jsval_throwing(scope)?,
    )?;
    Ok(())
}

/// The state of a final function stage that returned a promise: the pipeline, the `PassThrough`
/// the pipeline returns, and whether to end it.
#[core_runtime::jsclass(hidden)]
pub(crate) struct FinalStageState {
    pipeline: Heap<PipelineStateImpl>,
    pt: Heap<js::object::Object>,
    #[no_trace]
    end: bool,
}

#[core_runtime::jsmethods]
impl FinalStageState {}

/// The fulfillment of a final function stage's promise, whose payload is a [`FinalStageState`].
fn final_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stage = cast_payload::<FinalStageState>(scope, payload);
    let (pipeline, pt, end) = {
        let data = stage.data();
        (data.pipeline.get(scope), data.pt.get(scope), data.end)
    };
    let value = args.get(0);
    pipeline.data().value.set(value.get());
    if !value.is_null_or_undefined() {
        Function::call_by_name(scope, pt.handle(), c"write", &[value])?;
    }
    if end {
        Function::call_by_name(scope, pt.handle(), c"end", js::function::EmptyArgs)?;
    }
    js::jobs::queue_native_tick(scope, finish, pipeline, &[])?;
    Ok(js::value::undefined())
}

/// The rejection of a final function stage's promise, whose payload is a [`FinalStageState`].
fn final_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stage = cast_payload::<FinalStageState>(scope, payload);
    let (pipeline, pt) = {
        let data = stage.data();
        (data.pipeline.get(scope), data.pt.get(scope))
    };
    Function::call_by_name(scope, pt.handle(), c"destroy", &[args.get(0)])?;
    js::jobs::queue_native_tick(scope, finish, pipeline, &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// `ERR_INVALID_RETURN_VALUE`.
fn invalid_return(
    scope: &Scope<'_>,
    expected: &str,
    name: &str,
    value: HandleValue<'_>,
) -> Result<NodeError, ExnThrown> {
    let received = determine_specific_type(scope, value)?;
    Ok(NodeError::new(
        ErrorKind::TypeError,
        "ERR_INVALID_RETURN_VALUE",
        format!(
            "Expected {expected} to be returned from the \"{name}\" function but got {received}."
        ),
    ))
}

/// Node's `determineSpecificType`: a description of `value` for error messages, such as
/// `type number (5)` or `an instance of Object`.
fn determine_specific_type(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    let text = |value: HandleValue<'_>| -> Result<String, ExnThrown> {
        js::JSString::from_value(scope, value.get())?.to_utf8(scope)
    };
    let inspect = |depth: Option<f64>| -> Result<String, ExnThrown> {
        let options = crate::inspect::InspectOptions {
            depth,
            ..crate::inspect::InspectOptions::current(scope)?
        };
        crate::inspect::inspect(scope, value, options)
    };
    if value.is_null() {
        return Ok("null".into());
    }
    if value.is_undefined() {
        return Ok("undefined".into());
    }
    if let Ok(object) = Object::from_value(scope, value) {
        if object.is_callable() {
            return Ok(format!(
                "function {}",
                text(object.get_property(scope, c"name")?)?
            ));
        }
        let constructor = object.get_property(scope, c"constructor")?;
        if let Ok(constructor) = Object::from_value(scope, constructor) {
            if constructor.has_property(scope, c"name")? {
                let name = constructor.get_property(scope, c"name")?;
                return Ok(format!("an instance of {}", text(name)?));
            }
        }
        return inspect(Some(-1.0));
    }
    if value.is_number() {
        let number = value.to_number();
        if number == 0.0 && number.is_sign_negative() {
            return Ok("type number (-0)".into());
        }
        return Ok(format!("type number ({})", text(value)?));
    }
    if value.is_boolean() {
        return Ok(format!("type boolean ({})", value.to_boolean()));
    }
    if value.is_symbol() {
        return Ok(format!("type symbol ({})", inspect(Some(2.0))?));
    }
    if value.is_bigint() {
        return Ok(format!("type bigint ({}n)", text(value)?));
    }
    let units: Vec<u16> = text(value)?.encode_utf16().collect();
    let mut string = String::from_utf16_lossy(&units);
    if units.len() > 28 {
        string = format!("{}...", String::from_utf16_lossy(&units[..25]));
    }
    if !string.contains('\'') {
        return Ok(format!("type string ('{string}')"));
    }
    let quoted = js::JSString::from_str(scope, &string)?;
    let quoted = js::json::stringify(scope, quoted.to_jsval_throwing(scope)?, None, None)?;
    Ok(format!("type string ({})", quoted.unwrap_or_default()))
}

/// Node's `makeAsyncIterable`: `value` if it is iterable, an async iterator over it if it is a
/// readable stream, and `ERR_INVALID_ARG_TYPE` otherwise.
fn make_async_iterable<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'s>,
) -> Result<HandleValue<'s>, Thrown> {
    if is_iterable(scope, value, true)? {
        return Ok(value);
    }
    if is_readable_node_stream(scope, value, false)? {
        let stream = Object::from_value(scope, value).expect("streams are objects");
        let iterator = super::readable::async_iterator(scope, stream, None)?;
        return Ok(iterator.to_jsval_throwing(scope)?);
    }
    Err(NodeError::invalid_arg_type(
        scope,
        "val",
        "an instance of Readable, Iterable, or AsyncIterable",
        value,
    )
    .into())
}

/// `Readable.from(value)` as a stream.
fn readable_from<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    object_mode: bool,
) -> Result<Object<'s>, Thrown> {
    let options = Object::new_plain(scope)?;
    options.set_property(scope, c"objectMode", object_mode)?;
    super::from::from(scope, value, Some(options.to_jsval_throwing(scope)?))
}

/// The `{ signal }` options a function stage gets.
fn signal_options<'s>(
    scope: &'s Scope<'_>,
    signal: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let options = Object::new_plain(scope)?;
    options.set_property(scope, c"signal", signal)?;
    options.to_jsval_throwing(scope)
}

/// Node's `pipelineImpl`, which settles `promise` or calls `callback` once the pipeline finished.
/// Returns the last stream.
fn pipeline_impl<'s>(
    scope: &'s Scope<'_>,
    mut streams: Vec<HandleValue<'s>>,
    callback: HandleValue<'_>,
    promise: Option<js::Promise<'_>>,
    options: Option<Object<'_>>,
) -> Result<HandleValue<'s>, Thrown> {
    if streams.len() == 1 {
        if let Some(list) = Object::from_value(scope, streams[0])
            .ok()
            .and_then(|list| list.cast::<js::Array>().ok())
        {
            streams = list.to_vec(scope)?;
        }
    }
    if streams.len() < 2 {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_MISSING_ARGS",
            "The \"streams\" argument must be specified",
        )
        .into());
    }
    let outer_signal = match options {
        Some(options) => options.get_property(scope, c"signal")?,
        None => HandleValue::undefined(),
    };
    validate_abort_signal(scope, outer_signal, "options.signal")?;
    let end_option = match options {
        Some(options) => options.get_property(scope, c"end")?,
        None => HandleValue::undefined(),
    };

    let state =
        js::class::create_instance_with::<PipelineStateImpl>(scope, |_| PipelineStateImpl {
            callback: Heap::from(callback.get()),
            promise: promise.map(Heap::from),
            error: Heap::default(),
            value: Heap::default(),
            finish_count: 0.0,
            destroys: VecDeque::new(),
            last_cleanups: Vec::new(),
            called: false,
            controller: None,
            outer_signal: None,
            abort_listener: None,
            finish_fn: None,
            error_fn: None,
        })?;
    let finish_fn = Function::new_callback(scope, c"finish", 1, finish, state)?;
    let error_fn = Function::new_callback(scope, c"onError", 1, on_stream_error, state)?;
    let controller = AbortController::new(scope)?;
    let signal = scope.root_value(controller.signal(scope).as_value());
    {
        let mut data = state.data_mut();
        data.finish_fn = Some(Heap::from(*finish_fn));
        data.error_fn = Some(Heap::from(*error_fn));
        data.controller = Some(Heap::from(controller));
    }
    if let Ok(outer) = Object::from_value(scope, outer_signal) {
        let listener = Function::new_callback(scope, c"abort", 0, abort, state)?;
        {
            let mut data = state.data_mut();
            data.outer_signal = Some(Heap::from(outer));
            data.abort_listener = Some(Heap::from(*listener));
        }
        crate::events::listen(scope, outer, c"addEventListener", c"abort", *listener)?;
    }
    let add_finishes = |count: f64| state.data_mut().finish_count += count;
    let add_destroyer = |destroyer: DestroyerState<'_>| {
        state.data_mut().destroys.push_back(Heap::from(destroyer));
    };
    let add_eos_cleanup = |eos: super::eos::EosState<'_>| {
        state
            .data_mut()
            .last_cleanups
            .push(Cleanup::Eos(Heap::from(eos)));
    };
    let count = streams.len();
    let mut ret: HandleValue<'s> = HandleValue::undefined();
    for (index, stream) in streams.iter().copied().enumerate() {
        let reading = index < count - 1;
        let writing = index > 0;
        let is_last = index == count - 1;
        let end = reading || !(end_option.is_boolean() && !end_option.to_boolean());
        if is_node_stream(scope, stream)? {
            let stream_object = Object::from_value(scope, stream).expect("streams are objects");
            if let Some(next) = streams
                .get(index + 1)
                .and_then(|next| Object::from_value(scope, *next).ok())
            {
                if js::conversion::to_boolean(next.get_property(scope, c"closed")?)
                    || js::conversion::to_boolean(next.get_property(scope, c"destroyed")?)
                {
                    return Err(NodeError::new(
                        ErrorKind::Error,
                        "ERR_STREAM_UNABLE_TO_PIPE",
                        "Cannot pipe to a closed or destroyed stream",
                    )
                    .into());
                }
            }
            if end {
                let (destroyer, cleanup) = destroyer(scope, stream_object, reading, writing)?;
                add_destroyer(destroyer);
                if is_readable(scope, stream)? == Some(true) && is_last {
                    add_eos_cleanup(cleanup);
                }
            }
            crate::events::listen(scope, stream_object, c"on", c"error", *error_fn)?;
            if is_readable(scope, stream)? == Some(true) && is_last {
                state.data_mut().last_cleanups.push(Cleanup::ErrorListener {
                    stream: Heap::from(stream_object),
                    listener: Heap::from(*error_fn),
                });
            }
        }
        if index == 0 {
            if js::function::is_callable(stream) {
                let options = signal_options(scope, signal)?;
                ret = Function::call(scope, HandleValue::undefined(), stream, &[options])?;
                if !is_iterable(scope, ret, true)? {
                    return Err(invalid_return(
                        scope,
                        "Iterable, AsyncIterable or Stream",
                        "source",
                        ret,
                    )?
                    .into());
                }
            } else if is_iterable(scope, stream, true)?
                || is_readable_node_stream(scope, stream, false)?
            {
                ret = stream;
            } else {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "source",
                    "an instance of Stream, Iterable, or AsyncIterable",
                    stream,
                )
                .into());
            }
            continue;
        }
        if js::function::is_callable(stream) {
            let source = make_async_iterable(scope, ret)?;
            let options = signal_options(scope, signal)?;
            let result =
                Function::call(scope, HandleValue::undefined(), stream, &[source, options])?;
            if reading {
                if !is_iterable(scope, result, false)? {
                    return Err(invalid_return(
                        scope,
                        "AsyncIterable",
                        &format!("transform[{}]", index - 1),
                        result,
                    )?
                    .into());
                }
                ret = result;
                continue;
            }
            let pass_through = super::duplex::pass_through::class_constructor(scope)?;
            let pt_options = Object::new_plain(scope)?;
            pt_options.set_property(scope, c"objectMode", true)?;
            let pt =
                Function::construct(scope, pass_through, &[pt_options.to_jsval_throwing(scope)?])?;
            let then = match Object::from_value(scope, result).ok() {
                Some(result) => result.get_property(scope, c"then")?,
                None => HandleValue::undefined(),
            };
            if js::function::is_callable(then) {
                add_finishes(1.0);
                let stage = js::class::create_instance_with::<FinalStageStateImpl>(scope, |_| {
                    FinalStageStateImpl {
                        pipeline: Heap::from(state),
                        pt: Heap::from(pt),
                        end,
                    }
                })?;
                let on_value = Function::new_callback(scope, c"", 1, final_value, stage)?;
                let on_error = Function::new_callback(scope, c"", 1, final_error, stage)?;
                Function::call(
                    scope,
                    result,
                    then,
                    &[
                        (*on_value).to_jsval_throwing(scope)?,
                        (*on_error).to_jsval_throwing(scope)?,
                    ],
                )?;
            } else if is_iterable(scope, result, false)? {
                add_finishes(2.0);
                let source = readable_from(scope, result, true)?;
                pipe(scope, state, source, pt, end)?;
            } else {
                return Err(invalid_return(
                    scope,
                    "AsyncIterable or Promise",
                    "destination",
                    result,
                )?
                .into());
            }
            ret = pt.to_jsval_throwing(scope)?;
            let (destroyer, cleanup) = destroyer(scope, pt, false, true)?;
            add_destroyer(destroyer);
            if is_last {
                add_eos_cleanup(cleanup);
            }
            continue;
        }
        if is_node_stream(scope, stream)? {
            let stream_object = Object::from_value(scope, stream).expect("streams are objects");
            if is_readable_node_stream(scope, ret, false)? {
                add_finishes(2.0);
                let source = Object::from_value(scope, ret).expect("streams are objects");
                let cleanup = pipe(scope, state, source, stream_object, end)?;
                if is_readable(scope, stream)? == Some(true) && is_last {
                    add_eos_cleanup(cleanup);
                }
            } else if is_iterable(scope, ret, true)? {
                add_finishes(2.0);
                let source = readable_from(scope, ret, true)?;
                pipe(scope, state, source, stream_object, end)?;
            } else {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "val",
                    "an instance of Readable, Iterable, or AsyncIterable",
                    ret,
                )
                .into());
            }
            ret = stream;
            continue;
        }
        return Err(NodeError::invalid_arg_type(
            scope,
            "streams",
            "an instance of Stream or a function",
            stream,
        )
        .into());
    }
    let aborted = |value: HandleValue<'_>| -> Result<bool, ExnThrown> {
        Ok(match Object::from_value(scope, value).ok() {
            Some(signal) => js::conversion::to_boolean(signal.get_property(scope, c"aborted")?),
            None => false,
        })
    };
    if aborted(signal)? || aborted(outer_signal)? {
        js::jobs::queue_native_tick(scope, abort, state, &[])?;
    }
    Ok(ret)
}

/// `stream.pipeline(...streams, callback)`.
pub(crate) fn pipeline<'s>(
    scope: &'s Scope<'_>,
    mut streams: Vec<HandleValue<'s>>,
) -> Result<HandleValue<'s>, Thrown> {
    let callback = streams.pop().unwrap_or(HandleValue::undefined());
    validate_function(scope, callback, "streams[stream.length - 1]")?;
    pipeline_impl(scope, streams, callback, None, None)
}

/// The tick settling the promise of `stream/promises`' `pipeline`, whose payload is the promise
/// and whose arguments are the error and the value.
fn settle(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = cast_payload::<js::Promise>(scope, payload);
    if js::conversion::to_boolean(args.get(0)) {
        promise.reject(scope, args.get(0))?;
    } else {
        promise.resolve(scope, args.get(1))?;
    }
    Ok(js::value::undefined())
}

/// `stream/promises`' `pipeline(...streams, options)`.
pub(crate) fn pipeline_promise<'s>(
    scope: &'s Scope<'_>,
    mut streams: Vec<HandleValue<'s>>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    let mut options = None;
    if let Some(last) = streams.last().copied() {
        if let Ok(object) = Object::from_value(scope, last) {
            if !is_node_stream(scope, last)?
                && !is_iterable(scope, last, true)?
                && !js::function::is_callable(last)
            {
                options = Some(object);
                streams.pop();
            }
        }
    }
    if let Err(error) = pipeline_impl(
        scope,
        streams,
        HandleValue::undefined(),
        Some(promise),
        options,
    ) {
        error.throw(scope);
        promise.reject_with_pending(scope)?;
    }
    Ok(promise)
}
