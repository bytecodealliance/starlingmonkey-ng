// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Readable`: Node's `lib/internal/streams/readable.js`.

use std::ffi::CStr;

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{Callback, CallbackArgs};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::destroy::{self, error_or_destroy, error_or_destroy_with};
use super::util::{
    as_object, call, call_method, closure, emit, has_method, is_function, is_true, listen,
    listener_count, next_tick, required_obj, rust_string, same, states, string, take_exception,
    this_object, truthy, type_error, Captures, Props,
};
use crate::errors::{ErrorKind, NodeError, Thrown};

/// `stream._readableState`.
pub(crate) fn readable_state<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
) -> Result<Object<'s>, ExnThrown> {
    stream
        .obj(scope, c"_readableState")?
        .ok_or_else(|| js::error::TypeError("not a Readable".into()).throw(scope))
}

/// `Buffer.concat(parts)`.
fn concat_buffers<'s>(
    scope: &'s Scope<'_>,
    parts: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    let list = js::Array::with_contents(scope, parts)?;
    let list = (*list).to_jsval_throwing(scope)?;
    let buffer =
        crate::buffer::buffer::concat(scope, list, None).map_err(|error| error.throw(scope))?;
    buffer.to_jsval_throwing(scope)
}

/// Whether `value` is a `Buffer`.
pub(crate) fn is_buffer(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    crate::buffer::buffer::is_buffer(scope, value).map_err(|error| error.throw(scope))
}

/// Whether `value` is an `ArrayBufferView`.
pub(crate) fn is_array_buffer_view(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    as_object(scope, value).is_some_and(|object| js::ArrayBufferView::from_object(object).is_some())
}

/// `Buffer.from(value, ...args)`, with up to two further arguments.
pub(crate) fn buffer_from<'s>(
    scope: &'s Scope<'_>,
    args: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    let buffer =
        crate::buffer::buffer::from(scope, args[0], args.get(1).copied(), args.get(2).copied())
            .map_err(|error| error.throw(scope))?;
    buffer.to_jsval_throwing(scope)
}

/// A `Buffer` viewing the same memory as the `ArrayBufferView` `view`.
pub(crate) fn view_to_buffer<'s>(
    scope: &'s Scope<'_>,
    view: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let object =
        as_object(scope, view).ok_or_else(|| type_error(scope, "view is not an object"))?;
    let buffer = object.val(scope, c"buffer")?;
    let offset = object.val(scope, c"byteOffset")?;
    let length = object.val(scope, c"byteLength")?;
    buffer_from(scope, &[buffer, offset, length])
}

/// Whether `encoding` is an encoding `Buffer` supports.
pub(crate) fn is_encoding(scope: &Scope<'_>, encoding: HandleValue<'_>) -> Result<bool, ExnThrown> {
    Ok(crate::buffer::buffer::is_encoding(scope, encoding))
}

/// `ERR_UNKNOWN_ENCODING` for `encoding`.
pub(crate) fn unknown_encoding(scope: &Scope<'_>, encoding: HandleValue<'_>) -> NodeError {
    let name = js::JSString::from_value(scope, encoding)
        .and_then(|name| name.to_utf8(scope))
        .unwrap_or_default();
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_UNKNOWN_ENCODING",
        format!("Unknown encoding: {name}"),
    )
}

/// A new `StringDecoder` for `encoding`.
fn new_decoder<'s>(
    scope: &'s Scope<'_>,
    encoding: HandleValue<'s>,
) -> Result<Object<'s>, ExnThrown> {
    let namespace = core_runtime::module::builtin_namespace(scope, "node:string_decoder")?
        .ok_or_else(|| type_error(scope, "node:string_decoder is not available"))?;
    let ctor = required_obj(scope, namespace, c"StringDecoder")?;
    js::Function::construct(scope, ctor, &[encoding])
}

/// The length of `chunk`: a string's length in UTF-16 code units, or a buffer's in bytes.
pub(crate) fn chunk_length(scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<f64, ExnThrown> {
    if chunk.is_string() {
        return Ok(js::JSString::from_value(scope, chunk)?.len() as f64);
    }
    let Some(object) = as_object(scope, chunk) else {
        return Ok(0.0);
    };
    // A typed array's `length` is its element count, which is read directly unless the object has
    // an own `length` property shadowing the prototype's getter.
    if let Some(view) = js::ArrayBufferView::from_object(object) {
        if view.view_kind().is_typed_array() && !object.has_own_property(scope, c"length")? {
            return Ok(view.array_length() as f64);
        }
    }
    object.num(scope, c"length")
}

/// The length `chunk` adds to the buffered length of a stream in `object_mode` or not.
fn added_length(
    scope: &Scope<'_>,
    chunk: HandleValue<'_>,
    object_mode: bool,
) -> Result<f64, ExnThrown> {
    if object_mode {
        Ok(1.0)
    } else {
        chunk_length(scope, chunk)
    }
}

/// `ERR_INVALID_ARG_TYPE` for a chunk that isn't a string, `Buffer`, `TypedArray` or `DataView`.
pub(crate) fn invalid_chunk(scope: &Scope<'_>, chunk: HandleValue<'_>) -> NodeError {
    NodeError::invalid_arg_type(
        scope,
        "chunk",
        "of type string or an instance of Buffer, TypedArray, or DataView",
        chunk,
    )
}

/// Append `value` to the array `array`.
pub(crate) fn array_push(
    scope: &Scope<'_>,
    array: Object<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let length = array.num(scope, c"length")? as u32;
    array.set_element(scope, length, value)
}

#[core_runtime::jsfunction_class(name = "ReadableState")]
pub mod readable_state {
    use super::*;

    /// The state of a `Readable`, or of a `Duplex`'s readable side if `is_duplex`.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
        _stream: Option<HandleValue<'_>>,
        is_duplex: Option<bool>,
    ) -> Result<(), Thrown> {
        let options = options.and_then(|options| as_object(scope, options));
        let is_duplex = is_duplex.unwrap_or(false);
        let option = |name: &'static CStr| -> Result<HandleValue<'_>, ExnThrown> {
            match options {
                Some(options) => options.val(scope, name),
                None => Ok(HandleValue::undefined()),
            }
        };
        let object_mode = truthy(scope, option(c"objectMode")?)
            || (is_duplex && truthy(scope, option(c"readableObjectMode")?));
        this.set_flag(scope, c"objectMode", object_mode)?;
        let high_water_mark = match options {
            Some(options) => super::super::state::high_water_mark(
                scope,
                options,
                c"readableHighWaterMark",
                is_duplex,
                object_mode,
            )?,
            None => super::super::state::default_high_water_mark(false),
        };
        this.set_property(scope, c"highWaterMark", high_water_mark)?;
        this.set_property(
            scope,
            c"buffer",
            *js::Array::with_contents(scope, &[] as &[Value])?,
        )?;
        this.set_property(scope, c"bufferIndex", 0)?;
        this.set_property(scope, c"length", 0)?;
        this.set_property(
            scope,
            c"pipes",
            *js::Array::with_contents(scope, &[] as &[Value])?,
        )?;
        this.set_property(scope, c"flowing", js::value::null())?;
        for name in [
            c"ended",
            c"endEmitted",
            c"reading",
            c"needReadable",
            c"emittedReadable",
            c"readableListening",
            c"resumeScheduled",
            c"errorEmitted",
            c"destroyed",
            c"closed",
            c"closeEmitted",
            c"multiAwaitDrain",
            c"readingMore",
            c"dataEmitted",
            c"_dataListening",
        ] {
            this.set_flag(scope, name, false)?;
        }
        this.set_flag(scope, c"constructed", true)?;
        this.set_flag(scope, c"sync", true)?;
        let emit_close = option(c"emitClose")?;
        this.set_flag(
            scope,
            c"emitClose",
            !(emit_close.is_boolean() && !emit_close.to_boolean()),
        )?;
        let auto_destroy = option(c"autoDestroy")?;
        this.set_flag(
            scope,
            c"autoDestroy",
            !(auto_destroy.is_boolean() && !auto_destroy.to_boolean()),
        )?;
        this.set_property(scope, c"errored", js::value::null())?;
        this.set_property(scope, c"_paused", js::value::null())?;
        let default_encoding = option(c"defaultEncoding")?;
        let default_name = rust_string(scope, default_encoding);
        if default_encoding.is_null()
            || default_encoding.is_undefined()
            || matches!(default_name.as_deref(), Some("utf8" | "utf-8"))
        {
            this.set_property(scope, c"defaultEncoding", "utf8")?;
        } else if is_encoding(scope, default_encoding)? {
            this.set_property(scope, c"defaultEncoding", default_encoding)?;
        } else {
            return Err(unknown_encoding(scope, default_encoding).into());
        }
        this.set_property(scope, c"awaitDrainWriters", js::value::null())?;
        let encoding = option(c"encoding")?;
        if truthy(scope, encoding) {
            this.set_property(scope, c"decoder", new_decoder(scope, encoding)?)?;
            this.set_property(scope, c"encoding", encoding)?;
        } else {
            this.set_property(scope, c"decoder", js::value::null())?;
            this.set_property(scope, c"encoding", js::value::null())?;
        }
        Ok(())
    }

    /// Define the legacy `pipesCount` and `paused` accessors.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let pipes_count =
            Function::new_callback(scope, c"get pipesCount", 0, state_pipes_count, ())?;
        proto.define_accessor(scope, c"pipesCount", Some(*pipes_count), None, 0)?;
        let get_paused = Function::new_callback(scope, c"get paused", 0, state_get_paused, ())?;
        let set_paused = Function::new_callback(scope, c"set paused", 1, state_set_paused, ())?;
        proto.define_accessor(scope, c"paused", Some(*get_paused), Some(*set_paused), 0)
    }
}

/// `state.pipesCount`.
fn state_pipes_count(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = this_object(scope, &args)?;
    let pipes = required_obj(scope, state, c"pipes")?;
    Ok(js::value::from_f64(pipes.num(scope, c"length")?))
}

/// `state.paused`.
fn state_get_paused(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = this_object(scope, &args)?;
    Ok(js::value::from_bool(state.flag(scope, c"_paused")?))
}

/// `state.paused = value`.
fn state_set_paused(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = this_object(scope, &args)?;
    state.set_flag(scope, c"_paused", truthy(scope, args.get(0)))?;
    Ok(js::value::undefined())
}

/// Whether `state` is flowing: `state.flowing === true`.
fn flowing(scope: &Scope<'_>, state: Object<'_>) -> Result<bool, ExnThrown> {
    state.flag(scope, c"flowing")
}

/// Whether `state.flowing` was set to `false` explicitly: the stream was paused.
fn paused_flowing(scope: &Scope<'_>, state: Object<'_>) -> Result<bool, ExnThrown> {
    let value = state.get_property(scope, c"flowing")?;
    Ok(value.is_boolean() && !value.to_boolean())
}

/// Node's `canPushMore`.
fn can_push_more(scope: &Scope<'_>, state: Object<'_>) -> Result<bool, ExnThrown> {
    let length = state.num(scope, c"length")?;
    Ok(!state.flag(scope, c"ended")?
        && (length < state.num(scope, c"highWaterMark")? || length == 0.0))
}

/// Node's `readableAddChunk`, for `push` and `unshift` (if `add_to_front`).
fn readable_add_chunk(
    scope: &Scope<'_>,
    stream: Object<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
    add_to_front: bool,
) -> Result<bool, ExnThrown> {
    let state = readable_state(scope, stream)?;
    if chunk.is_null() {
        state.set_flag(scope, c"reading", false)?;
        on_eof_chunk(scope, stream, state)?;
        return Ok(false);
    }
    let object_mode = state.flag(scope, c"objectMode")?;
    let mut chunk = scope.root_value(chunk.get());
    let mut encoding = match encoding {
        Some(encoding) if truthy(scope, encoding) => scope.root_value(encoding.get()),
        _ => string(scope, "")?,
    };
    if !object_mode {
        if chunk.is_string() {
            if !truthy(scope, encoding) {
                encoding = state.val(scope, c"defaultEncoding")?;
            }
            let state_encoding = state.val(scope, c"encoding")?;
            if !same(scope, state_encoding, encoding)? {
                if add_to_front && truthy(scope, state_encoding) {
                    let buffer = buffer_from(scope, &[chunk, encoding])?;
                    let buffer = as_object(scope, buffer)
                        .ok_or_else(|| type_error(scope, "Buffer.from returned a primitive"))?;
                    chunk = call_method(scope, buffer, c"toString", &[state_encoding])?;
                } else {
                    chunk = buffer_from(scope, &[chunk, encoding])?;
                    encoding = string(scope, "")?;
                }
            }
        } else if is_buffer(scope, chunk)? {
            encoding = string(scope, "")?;
        } else if is_array_buffer_view(scope, chunk) {
            chunk = view_to_buffer(scope, chunk)?;
            encoding = string(scope, "")?;
        } else if !chunk.is_undefined() {
            error_or_destroy_with(scope, stream, invalid_chunk(scope, chunk), false)?;
            return Ok(false);
        }
    }
    let length = if chunk.is_undefined() {
        0.0
    } else {
        added_length(scope, chunk, object_mode)?
    };
    if object_mode || length > 0.0 {
        if add_to_front {
            if state.flag(scope, c"endEmitted")? {
                error_or_destroy_with(
                    scope,
                    stream,
                    NodeError::new(
                        ErrorKind::Error,
                        "ERR_STREAM_UNSHIFT_AFTER_END_EVENT",
                        "stream.unshift() after end event",
                    ),
                    false,
                )?;
            } else if state.flag(scope, c"destroyed")?
                || truthy(scope, state.get_property(scope, c"errored")?)
            {
                return Ok(false);
            } else {
                add_chunk(scope, stream, state, chunk, true)?;
            }
        } else if state.flag(scope, c"ended")? {
            error_or_destroy_with(
                scope,
                stream,
                NodeError::new(
                    ErrorKind::Error,
                    "ERR_STREAM_PUSH_AFTER_EOF",
                    "stream.push() after EOF",
                ),
                false,
            )?;
            return Ok(false);
        } else if state.flag(scope, c"destroyed")?
            || truthy(scope, state.get_property(scope, c"errored")?)
        {
            return Ok(false);
        } else {
            state.set_flag(scope, c"reading", false)?;
            let decoder = state.obj(scope, c"decoder")?;
            match decoder {
                Some(decoder) if !truthy(scope, encoding) => {
                    chunk = call_method(scope, decoder, c"write", &[chunk])?;
                    if object_mode || chunk_length(scope, chunk)? != 0.0 {
                        add_chunk(scope, stream, state, chunk, false)?;
                    } else {
                        maybe_read_more(scope, stream, state)?;
                    }
                }
                _ => add_chunk(scope, stream, state, chunk, false)?,
            }
        }
    } else if !add_to_front {
        state.set_flag(scope, c"reading", false)?;
        maybe_read_more(scope, stream, state)?;
    }
    can_push_more(scope, state)
}

/// Clear the writers the stream waits for `drain` from.
fn clear_await_drain(scope: &Scope<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    if state.flag(scope, c"multiAwaitDrain")? {
        if let Some(writers) = state.obj(scope, c"awaitDrainWriters")? {
            call_method(scope, writers, c"clear", &[])?;
        }
    } else {
        state.set_property(scope, c"awaitDrainWriters", js::value::null())?;
    }
    Ok(())
}

/// Node's `addChunk`: emit the chunk as `data` if the stream is flowing with an empty buffer, and
/// buffer it otherwise.
fn add_chunk(
    scope: &Scope<'_>,
    stream: Object<'_>,
    state: Object<'_>,
    chunk: HandleValue<'_>,
    add_to_front: bool,
) -> Result<(), ExnThrown> {
    if flowing(scope, state)?
        && !state.flag(scope, c"sync")?
        && state.flag(scope, c"_dataListening")?
        && state.num(scope, c"length")? == 0.0
    {
        clear_await_drain(scope, state)?;
        state.set_flag(scope, c"dataEmitted", true)?;
        emit(scope, stream, "data", &[chunk])?;
    } else {
        let object_mode = state.flag(scope, c"objectMode")?;
        let length = state.num(scope, c"length")? + added_length(scope, chunk, object_mode)?;
        state.set_property(scope, c"length", length)?;
        let buffer = required_obj(scope, state, c"buffer")?;
        if add_to_front {
            let index = state.num(scope, c"bufferIndex")?;
            if index > 0.0 {
                state.set_property(scope, c"bufferIndex", index - 1.0)?;
                buffer.set_element(scope, (index - 1.0) as u32, chunk)?;
            } else {
                call_method(scope, buffer, c"unshift", &[chunk])?;
            }
        } else {
            array_push(scope, buffer, chunk)?;
        }
        if state.flag(scope, c"needReadable")? {
            emit_readable(scope, stream)?;
        }
    }
    maybe_read_more(scope, stream, state)
}

/// Node's `onEofChunk`: end the stream, flushing the decoder.
fn on_eof_chunk(scope: &Scope<'_>, stream: Object<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    if state.flag(scope, c"ended")? {
        return Ok(());
    }
    if let Some(decoder) = state.obj(scope, c"decoder")? {
        let chunk = call_method(scope, decoder, c"end", &[])?;
        if !chunk.is_undefined() && !chunk.is_null() && chunk_length(scope, chunk)? > 0.0 {
            let buffer = required_obj(scope, state, c"buffer")?;
            array_push(scope, buffer, chunk)?;
            let object_mode = state.flag(scope, c"objectMode")?;
            let length = state.num(scope, c"length")? + added_length(scope, chunk, object_mode)?;
            state.set_property(scope, c"length", length)?;
        }
    }
    state.set_flag(scope, c"ended", true)?;
    if state.flag(scope, c"sync")? {
        emit_readable(scope, stream)
    } else {
        state.set_flag(scope, c"needReadable", false)?;
        state.set_flag(scope, c"emittedReadable", true)?;
        emit_readable_now(scope, stream)
    }
}

/// Node's `emitReadable`: emit `readable` in a microtask, once.
fn emit_readable(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    let state = readable_state(scope, stream)?;
    state.set_flag(scope, c"needReadable", false)?;
    if !state.flag(scope, c"emittedReadable")? {
        state.set_flag(scope, c"emittedReadable", true)?;
        next_tick(
            scope,
            emit_readable_tick,
            &[stream.to_jsval_raw_throwing(scope)?],
        )?;
    }
    Ok(())
}

fn emit_readable_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    emit_readable_now(scope, Captures::of(scope, payload)?.obj(0)?)?;
    Ok(js::value::undefined())
}

/// Node's `emitReadable_`.
fn emit_readable_now(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    let state = readable_state(scope, stream)?;
    let errored = truthy(scope, state.get_property(scope, c"errored")?);
    if !state.flag(scope, c"destroyed")?
        && !errored
        && (state.num(scope, c"length")? > 0.0 || state.flag(scope, c"ended")?)
    {
        emit(scope, stream, "readable", &[])?;
        state.set_flag(scope, c"emittedReadable", false)?;
    }
    if !flowing(scope, state)?
        && !state.flag(scope, c"ended")?
        && state.num(scope, c"length")? <= state.num(scope, c"highWaterMark")?
    {
        state.set_flag(scope, c"needReadable", true)?;
    }
    flow(scope, stream)
}

/// Node's `maybeReadMore`: read ahead in a microtask, once.
fn maybe_read_more(
    scope: &Scope<'_>,
    stream: Object<'_>,
    state: Object<'_>,
) -> Result<(), ExnThrown> {
    if !state.flag(scope, c"readingMore")? && state.flag(scope, c"constructed")? {
        state.set_flag(scope, c"readingMore", true)?;
        next_tick(
            scope,
            maybe_read_more_tick,
            &[stream.to_jsval_raw_throwing(scope)?],
        )?;
    }
    Ok(())
}

/// Node's `maybeReadMore_`: call `read(0)` while that fills the buffer and it's below the high
/// water mark, or empty and flowing.
fn maybe_read_more_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let state = readable_state(scope, stream)?;
    let zero = scope.root_value(js::value::from_f64(0.0));
    loop {
        if state.flag(scope, c"reading")? || state.flag(scope, c"ended")? {
            break;
        }
        let length = state.num(scope, c"length")?;
        if !(length < state.num(scope, c"highWaterMark")?
            || (flowing(scope, state)? && length == 0.0))
        {
            break;
        }
        call_method(scope, stream, c"read", &[zero])?;
        if length == state.num(scope, c"length")? {
            break;
        }
    }
    state.set_flag(scope, c"readingMore", false)?;
    Ok(js::value::undefined())
}

/// Node's `howMuchToRead`. `n` is NaN for "everything".
fn how_much_to_read(scope: &Scope<'_>, n: f64, state: Object<'_>) -> Result<f64, ExnThrown> {
    let length = state.num(scope, c"length")?;
    let ended = state.flag(scope, c"ended")?;
    if n <= 0.0 || (length == 0.0 && ended) {
        return Ok(0.0);
    }
    if state.flag(scope, c"objectMode")? {
        return Ok(1.0);
    }
    if n.is_nan() {
        if flowing(scope, state)? && length > 0.0 {
            let buffer = required_obj(scope, state, c"buffer")?;
            let first = buffer.get_element(scope, state.num(scope, c"bufferIndex")? as u32)?;
            return chunk_length(scope, first);
        }
        return Ok(length);
    }
    if n <= length {
        return Ok(n);
    }
    Ok(if ended { length } else { 0.0 })
}

/// Node's `computeNewHighWaterMark`: the next power of two at or above `n`.
fn compute_new_high_water_mark(scope: &Scope<'_>, n: f64) -> Result<f64, ExnThrown> {
    const MAX: f64 = 1073741824.0;
    if n > MAX {
        let received = crate::errors::out_of_range_received(n);
        return Err(NodeError::out_of_range("size", "<= 1GiB", &received).throw(scope));
    }
    Ok((n as u64).max(1).next_power_of_two() as f64)
}

/// JS's `parseInt(text, 10)`: the leading decimal integer after whitespace, or NaN.
fn parse_int(text: &str) -> f64 {
    let text =
        text.trim_start_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}');
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let Ok(value) = digits[..end].parse::<f64>() else {
        return f64::NAN;
    };
    if negative {
        -value
    } else {
        value
    }
}

/// `readable.read(n)`.
pub(crate) fn read<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    n: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let state = readable_state(scope, stream)?;
    let mut n = if n.is_undefined() {
        f64::NAN
    } else if n.is_number() && n.to_number().fract() == 0.0 {
        n.to_number()
    } else {
        parse_int(&js::JSString::from_value(scope, n)?.to_utf8(scope)?)
    };
    let n_orig = n;
    if n > state.num(scope, c"highWaterMark")? {
        let mark = compute_new_high_water_mark(scope, n)?;
        state.set_property(scope, c"highWaterMark", mark)?;
    }
    if n != 0.0 {
        state.set_flag(scope, c"emittedReadable", false)?;
    }
    let length = state.num(scope, c"length")?;
    let mark = state.num(scope, c"highWaterMark")?;
    let ended = state.flag(scope, c"ended")?;
    if n == 0.0
        && state.flag(scope, c"needReadable")?
        && ((if mark != 0.0 {
            length >= mark
        } else {
            length > 0.0
        }) || ended)
    {
        if length == 0.0 && ended {
            end_readable(scope, stream)?;
        } else {
            emit_readable(scope, stream)?;
        }
        return Ok(HandleValue::null());
    }
    n = how_much_to_read(scope, n, state)?;
    if n == 0.0 && state.flag(scope, c"ended")? {
        if state.num(scope, c"length")? == 0.0 {
            end_readable(scope, stream)?;
        }
        return Ok(HandleValue::null());
    }
    let mut do_read = state.flag(scope, c"needReadable")?;
    let length = state.num(scope, c"length")?;
    if length == 0.0 || length - n < state.num(scope, c"highWaterMark")? {
        do_read = true;
    }
    let blocked = state.flag(scope, c"reading")?
        || state.flag(scope, c"ended")?
        || state.flag(scope, c"destroyed")?
        || truthy(scope, state.get_property(scope, c"errored")?)
        || !state.flag(scope, c"constructed")?;
    if !blocked && do_read {
        state.set_flag(scope, c"reading", true)?;
        state.set_flag(scope, c"sync", true)?;
        if state.num(scope, c"length")? == 0.0 {
            state.set_flag(scope, c"needReadable", true)?;
        }
        let mark = state.val(scope, c"highWaterMark")?;
        if call_method(scope, stream, c"_read", &[mark]).is_err() {
            let error = take_exception(scope);
            error_or_destroy(scope, stream, error, false)?;
        }
        state.set_flag(scope, c"sync", false)?;
        if !state.flag(scope, c"reading")? {
            n = how_much_to_read(scope, n_orig, state)?;
        }
    }
    let ret = if n > 0.0 {
        from_list(scope, n, state)?
    } else {
        HandleValue::null()
    };
    if ret.is_null() {
        if state.num(scope, c"length")? <= state.num(scope, c"highWaterMark")? {
            state.set_flag(scope, c"needReadable", true)?;
        }
        n = 0.0;
    } else {
        let length = state.num(scope, c"length")? - n;
        state.set_property(scope, c"length", length)?;
        clear_await_drain(scope, state)?;
    }
    if state.num(scope, c"length")? == 0.0 {
        if !state.flag(scope, c"ended")? {
            state.set_flag(scope, c"needReadable", true)?;
        }
        let differs = !(n_orig == n || (n_orig.is_nan() && n.is_nan()));
        if differs && state.flag(scope, c"ended")? {
            end_readable(scope, stream)?;
        }
    }
    if !ret.is_null()
        && !state.flag(scope, c"errorEmitted")?
        && !state.flag(scope, c"closeEmitted")?
    {
        state.set_flag(scope, c"dataEmitted", true)?;
        emit(scope, stream, "data", &[ret])?;
    }
    Ok(ret)
}

/// Node's `fromList`: take `n` bytes, characters or one object from the buffer.
fn from_list<'s>(
    scope: &'s Scope<'_>,
    n: f64,
    state: Object<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    if state.num(scope, c"length")? == 0.0 {
        return Ok(HandleValue::null());
    }
    let buffer = required_obj(scope, state, c"buffer")?;
    let len = buffer.num(scope, c"length")? as u32;
    let mut index = state.num(scope, c"bufferIndex")? as u32;
    let total = state.num(scope, c"length")?;
    let decoding = state.obj(scope, c"decoder")?.is_some();
    let take = |index: &mut u32| -> Result<HandleValue<'s>, ExnThrown> {
        let value = buffer.get_element(scope, *index)?;
        buffer.set_element(scope, *index, js::value::null())?;
        *index += 1;
        Ok(value)
    };
    let ret;
    if state.flag(scope, c"objectMode")? {
        ret = take(&mut index)?;
    } else if n.is_nan() || n >= total {
        if decoding {
            let mut text = js::JSString::empty(scope);
            while index < len {
                let part = js::JSString::from_value(scope, take(&mut index)?)?;
                text = text.concat(scope, part)?;
            }
            ret = text.to_jsval_throwing(scope)?;
        } else if len - index == 1 {
            ret = take(&mut index)?;
        } else {
            let mut parts = Vec::new();
            while index < len {
                parts.push(take(&mut index)?);
            }
            ret = concat_buffers(scope, &parts)?;
        }
    } else {
        let first = buffer.get_element(scope, index)?;
        let first_length = chunk_length(scope, first)?;
        if n < first_length {
            let start = scope.root_value(js::value::from_f64(0.0));
            let end = scope.root_value(js::value::from_f64(n));
            ret = slice(scope, first, &[start, end])?;
            let rest = slice(scope, first, &[end])?;
            buffer.set_element(scope, index, rest)?;
        } else if n == first_length {
            ret = take(&mut index)?;
        } else {
            let mut remaining = n;
            let mut parts = Vec::new();
            while index < len && remaining > 0.0 {
                let data = buffer.get_element(scope, index)?;
                let data_length = chunk_length(scope, data)?;
                if remaining >= data_length {
                    parts.push(take(&mut index)?);
                    remaining -= data_length;
                } else {
                    let start = scope.root_value(js::value::from_f64(0.0));
                    let end = scope.root_value(js::value::from_f64(remaining));
                    parts.push(slice(scope, data, &[start, end])?);
                    let rest = slice(scope, data, &[end])?;
                    buffer.set_element(scope, index, rest)?;
                    remaining = 0.0;
                }
            }
            if decoding {
                let mut text = js::JSString::empty(scope);
                for part in parts {
                    text = text.concat(scope, js::JSString::from_value(scope, part)?)?;
                }
                ret = text.to_jsval_throwing(scope)?;
            } else {
                ret = concat_buffers(scope, &parts)?;
            }
        }
    }
    if index == len {
        buffer.set_property(scope, c"length", 0)?;
        state.set_property(scope, c"bufferIndex", 0)?;
    } else if index > 1024 {
        let start = scope.root_value(js::value::from_f64(0.0));
        let count = scope.root_value(js::value::from_f64(f64::from(index)));
        call_method(scope, buffer, c"splice", &[start, count])?;
        state.set_property(scope, c"bufferIndex", 0)?;
    } else {
        state.set_property(scope, c"bufferIndex", index)?;
    }
    Ok(ret)
}

/// `value.slice(...args)` for a string or `Buffer`.
fn slice<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    if value.is_string() {
        let text = js::JSString::from_value(scope, value)?;
        let start = (args[0].to_number() as usize).min(text.len());
        let end = args
            .get(1)
            .map_or(text.len(), |end| end.to_number() as usize)
            .clamp(start, text.len());
        return text
            .substring(scope, start, end - start)?
            .to_jsval_throwing(scope);
    }
    let object = as_object(scope, value)
        .ok_or_else(|| type_error(scope, "chunk.slice is not a function"))?;
    call_method(scope, object, c"slice", args)
}

/// Node's `endReadable`: emit `end` in a microtask.
fn end_readable(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    let state = readable_state(scope, stream)?;
    if !state.flag(scope, c"endEmitted")? {
        state.set_flag(scope, c"ended", true)?;
        next_tick(
            scope,
            end_readable_tick,
            &[stream.to_jsval_raw_throwing(scope)?],
        )?;
    }
    Ok(())
}

/// Node's `endReadableNT`: emit `end` unless data was unshifted, then end the writable side of a
/// duplex that doesn't allow half-open, or auto-destroy.
fn end_readable_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let state = readable_state(scope, stream)?;
    let errored = truthy(scope, state.get_property(scope, c"errored")?);
    if !errored
        && !state.flag(scope, c"closeEmitted")?
        && !state.flag(scope, c"endEmitted")?
        && state.num(scope, c"length")? == 0.0
    {
        state.set_flag(scope, c"endEmitted", true)?;
        emit(scope, stream, "end", &[])?;
        let allow_half_open = stream.get_property(scope, c"allowHalfOpen")?;
        if truthy(scope, stream.get_property(scope, c"writable")?)
            && allow_half_open.is_boolean()
            && !allow_half_open.to_boolean()
        {
            next_tick(
                scope,
                end_writable_tick,
                &[stream.to_jsval_raw_throwing(scope)?],
            )?;
        } else if state.flag(scope, c"autoDestroy")? {
            let auto_destroy = match stream.obj(scope, c"_writableState")? {
                None => true,
                Some(w) => {
                    let writable = w.get_property(scope, c"writable")?;
                    w.flag(scope, c"autoDestroy")?
                        && (w.flag(scope, c"finished")?
                            || (writable.is_boolean() && !writable.to_boolean()))
                }
            };
            if auto_destroy {
                call_method(scope, stream, c"destroy", &[])?;
            }
        }
    }
    Ok(js::value::undefined())
}

/// Node's `endWritableNT`.
fn end_writable_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let writable = truthy(scope, stream.get_property(scope, c"writable")?)
        && !truthy(scope, stream.get_property(scope, c"writableEnded")?)
        && !truthy(scope, stream.get_property(scope, c"destroyed")?);
    if writable {
        call_method(scope, stream, c"end", &[])?;
    }
    Ok(js::value::undefined())
}

/// Node's `flow`: read while flowing and reads return data.
fn flow(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    let state = readable_state(scope, stream)?;
    while flowing(scope, state)? {
        if call_method(scope, stream, c"read", &[])?.is_null() {
            break;
        }
    }
    Ok(())
}

/// `readable.resume()`.
pub(crate) fn resume<'s>(scope: &Scope<'_>, stream: Object<'s>) -> Result<Object<'s>, ExnThrown> {
    let state = readable_state(scope, stream)?;
    if !flowing(scope, state)? {
        let listening = state.flag(scope, c"readableListening")?;
        state.set_flag(scope, c"flowing", !listening)?;
        if !state.flag(scope, c"resumeScheduled")? {
            state.set_flag(scope, c"resumeScheduled", true)?;
            next_tick(scope, resume_tick, &[stream.to_jsval_raw_throwing(scope)?])?;
        }
    }
    state.set_flag(scope, c"_paused", false)?;
    Ok(stream)
}

/// Node's `resume_`.
fn resume_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let state = readable_state(scope, stream)?;
    let zero = scope.root_value(js::value::from_f64(0.0));
    if !state.flag(scope, c"reading")? {
        call_method(scope, stream, c"read", &[zero])?;
    }
    state.set_flag(scope, c"resumeScheduled", false)?;
    emit(scope, stream, "resume", &[])?;
    flow(scope, stream)?;
    if flowing(scope, state)? && !state.flag(scope, c"reading")? {
        call_method(scope, stream, c"read", &[zero])?;
    }
    Ok(js::value::undefined())
}

/// `readable.pause()`.
pub(crate) fn pause<'s>(scope: &Scope<'_>, stream: Object<'s>) -> Result<Object<'s>, ExnThrown> {
    let state = readable_state(scope, stream)?;
    if !paused_flowing(scope, state)? {
        state.set_flag(scope, c"flowing", false)?;
        emit(scope, stream, "pause", &[])?;
    }
    state.set_flag(scope, c"_paused", true)?;
    Ok(stream)
}

/// Node's `updateReadableListening`.
fn update_readable_listening(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let state = readable_state(scope, stream)?;
    let readable = string(scope, "readable")?;
    let listening = listener_count(scope, stream, readable)? > 0.0;
    state.set_flag(scope, c"readableListening", listening)?;
    let paused = state.get_property(scope, c"_paused")?;
    if paused.is_boolean() && !paused.to_boolean() && state.flag(scope, c"resumeScheduled")? {
        state.set_flag(scope, c"flowing", true)?;
    } else if state.flag(scope, c"_dataListening")? {
        call_method(scope, stream, c"resume", &[] as &[HandleValue<'_>])?;
    } else if !listening {
        state.set_property(scope, c"flowing", js::value::null())?;
    }
    Ok(js::value::undefined())
}

/// Node's `nReadingNextTick`.
fn read_zero_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let zero = scope.root_value(js::value::from_f64(0.0));
    call_method(scope, stream, c"read", &[zero])?;
    Ok(js::value::undefined())
}

/// `readable.on(event, listener)`: add the listener, and start flowing for `data` or reading for
/// `readable`.
pub(crate) fn on<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'s>,
    event: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let result = crate::events::add_listener(scope, stream, event, listener, false)?;
    let state = readable_state(scope, stream)?;
    match rust_string(scope, event).as_deref() {
        Some("data") => {
            state.set_flag(scope, c"_dataListening", true)?;
            let readable = string(scope, "readable")?;
            if listener_count(scope, stream, readable)? > 0.0 {
                state.set_flag(scope, c"readableListening", true)?;
            }
            if !paused_flowing(scope, state)? {
                call_method(scope, stream, c"resume", &[] as &[HandleValue<'_>])?;
            }
        }
        Some("readable")
            if !state.flag(scope, c"endEmitted")?
                && !state.flag(scope, c"readableListening")? =>
        {
            state.set_flag(scope, c"readableListening", true)?;
            state.set_flag(scope, c"needReadable", true)?;
            state.set_flag(scope, c"flowing", false)?;
            state.set_flag(scope, c"emittedReadable", false)?;
            if state.num(scope, c"length")? > 0.0 {
                emit_readable(scope, stream)?;
            } else if !state.flag(scope, c"reading")? {
                next_tick(
                    scope,
                    read_zero_tick,
                    &[stream.to_jsval_raw_throwing(scope)?],
                )?;
            }
        }
        _ => {}
    }
    Ok(result)
}

/// `readable.removeListener(event, listener)`.
pub(crate) fn remove_listener<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'s>,
    event: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let result = crate::events::remove_listener(scope, stream, event, listener)?;
    let state = readable_state(scope, stream)?;
    match rust_string(scope, event).as_deref() {
        Some("readable") => next_tick(
            scope,
            update_readable_listening,
            &[stream.to_jsval_raw_throwing(scope)?],
        )?,
        Some("data") if listener_count(scope, stream, event)? == 0.0 => {
            state.set_flag(scope, c"_dataListening", false)?;
        }
        _ => {}
    }
    Ok(result)
}

/// `readable.setEncoding(encoding)`.
pub(crate) fn set_encoding<'s>(
    scope: &Scope<'_>,
    stream: Object<'s>,
    encoding: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let state = readable_state(scope, stream)?;
    let decoder = new_decoder(scope, encoding)?;
    state.set_property(scope, c"decoder", decoder)?;
    state.set_property(
        scope,
        c"encoding",
        decoder.get_property(scope, c"encoding")?,
    )?;
    let buffer = required_obj(scope, state, c"buffer")?;
    let len = buffer.num(scope, c"length")? as u32;
    let mut content = js::JSString::empty(scope);
    for index in state.num(scope, c"bufferIndex")? as u32..len {
        let data = buffer.get_element(scope, index)?;
        let written = call_method(scope, decoder, c"write", &[data])?;
        content = content.concat(scope, js::JSString::from_value(scope, written)?)?;
    }
    if state.flag(scope, c"ended")? {
        let rest = call_method(scope, decoder, c"end", &[] as &[HandleValue<'_>])?;
        content = content.concat(scope, js::JSString::from_value(scope, rest)?)?;
    }
    buffer.set_property(scope, c"length", 0)?;
    state.set_property(scope, c"bufferIndex", 0)?;
    if !content.is_empty() {
        array_push(scope, buffer, content.to_jsval_throwing(scope)?)?;
    }
    state.set_property(scope, c"length", content.len() as f64)?;
    Ok(stream)
}

/// The listeners `pipe` adds, as captures: `[src, dest, ondata, onend, unpipe, onunpipe, onerror,
/// onclose, onfinish, ondrain, cleanedUp]`.
mod pipe_captures {
    pub(super) const SRC: u32 = 0;
    pub(super) const DEST: u32 = 1;
    pub(super) const ONDATA: u32 = 2;
    pub(super) const ONEND: u32 = 3;
    pub(super) const UNPIPE: u32 = 4;
    pub(super) const ONUNPIPE: u32 = 5;
    pub(super) const ONERROR: u32 = 6;
    pub(super) const ONCLOSE: u32 = 7;
    pub(super) const ONFINISH: u32 = 8;
    pub(super) const ONDRAIN: u32 = 9;
    pub(super) const CLEANED_UP: u32 = 10;
    pub(super) const COUNT: usize = 11;
}

/// Each `pipe` listener's captures are `[pipe]`, where `pipe` is the array [`pipe_captures`]
/// describes.
fn pipe_array<'s>(
    scope: &'s Scope<'_>,
    payload: HandleValue<'_>,
) -> Result<Captures<'s>, ExnThrown> {
    let outer = Captures::of(scope, payload)?;
    let inner = outer.get(0)?;
    Captures::of(scope, inner)
}

/// `readable.pipe(dest, options)`.
pub(crate) fn pipe<'s>(
    scope: &'s Scope<'_>,
    src: Object<'_>,
    dest: HandleValue<'s>,
    options: Option<HandleValue<'_>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    use pipe_captures::*;
    let state = readable_state(scope, src)?;
    let dest_object = as_object(scope, dest)
        .ok_or_else(|| js::error::TypeError("dest must be a stream".into()).throw(scope))?;
    let pipes = required_obj(scope, state, c"pipes")?;
    if pipes.num(scope, c"length")? == 1.0 && !state.flag(scope, c"multiAwaitDrain")? {
        state.set_flag(scope, c"multiAwaitDrain", true)?;
        let existing = state.val(scope, c"awaitDrainWriters")?;
        let set = js::Set::new(scope)?;
        if truthy(scope, existing) {
            set.add(scope, existing)?;
        }
        state.set_property(scope, c"awaitDrainWriters", set)?;
    }
    array_push(scope, pipes, dest)?;

    let pipe = js::Array::with_contents(scope, &[js::value::undefined(); COUNT])?;
    let pipe_value = scope.root_value((*pipe).to_jsval_raw_throwing(scope)?);
    let captures = Captures::of(scope, (*pipe).to_jsval_throwing(scope)?)?;
    captures.set(SRC, src)?;
    captures.set(DEST, dest)?;
    captures.set(CLEANED_UP, false)?;
    let make = |body: Callback, nargs: u32| closure(scope, nargs, body, &[pipe_value.get()]);
    let ondata = make(pipe_ondata, 1)?;
    let onend = make(pipe_onend, 0)?;
    let unpipe = make(pipe_unpipe, 0)?;
    let onunpipe = make(pipe_onunpipe, 2)?;
    let onerror = make(pipe_onerror, 1)?;
    let onclose = make(pipe_onclose, 0)?;
    let onfinish = make(pipe_onfinish, 0)?;
    for (index, function) in [
        (ONDATA, ondata),
        (ONEND, onend),
        (UNPIPE, unpipe),
        (ONUNPIPE, onunpipe),
        (ONERROR, onerror),
        (ONCLOSE, onclose),
        (ONFINISH, onfinish),
    ] {
        captures.set(index, function)?;
    }

    let end_option = options
        .and_then(|options| as_object(scope, options))
        .map(|options| options.get_property(scope, c"end"))
        .transpose()?;
    let stdout_or_stderr = {
        let process = scope.global().obj(scope, c"process")?;
        let mut matches = false;
        if let Some(process) = process {
            for name in [c"stdout", c"stderr"] {
                matches |= same(scope, process.val(scope, name)?, dest)?;
            }
        }
        matches
    };
    let do_end =
        !end_option.is_some_and(|end| end.is_boolean() && !end.to_boolean()) && !stdout_or_stderr;
    let end_fn = if do_end { onend } else { unpipe };
    if state.flag(scope, c"endEmitted")? {
        super::util::next_tick_call(scope, end_fn.to_jsval_throwing(scope)?, &[])?;
    } else {
        listen(scope, src, c"once", "end", end_fn)?;
    }
    listen(scope, dest_object, c"on", "unpipe", onunpipe)?;
    listen(scope, src, c"on", "data", ondata)?;
    prepend_listener(scope, dest_object, "error", onerror)?;
    listen(scope, dest_object, c"once", "close", onclose)?;
    listen(scope, dest_object, c"once", "finish", onfinish)?;
    emit(scope, dest_object, "pipe", &[src.to_jsval_throwing(scope)?])?;
    if is_true(dest_object.get_property(scope, c"writableNeedDrain")?) {
        pipe_pause(scope, &captures)?;
    } else if !flowing(scope, state)? {
        call_method(scope, src, c"resume", &[])?;
    }
    Ok(dest)
}

/// Node's `prependListener`.
pub(crate) fn prepend_listener(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    event: &str,
    listener: Object<'_>,
) -> Result<(), ExnThrown> {
    if has_method(scope, emitter, c"prependListener")? {
        listen(scope, emitter, c"prependListener", event, listener)
    } else {
        listen(scope, emitter, c"on", event, listener)
    }
}

fn pipe_ondata(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use pipe_captures::*;
    let captures = pipe_array(scope, payload)?;
    let dest = captures.obj(DEST)?;
    match call_method(scope, dest, c"write", &[args.get(0)]) {
        Ok(ret) => {
            if ret.is_boolean() && !ret.to_boolean() {
                pipe_pause(scope, &captures)?;
            }
        }
        Err(_) => {
            let error = take_exception(scope);
            call_method(scope, dest, c"destroy", &[error])?;
        }
    }
    Ok(js::value::undefined())
}

/// Node's `pause` inside `pipe`: record `dest` as awaited, pause the source, and listen for
/// `drain` on `dest`.
fn pipe_pause(scope: &Scope<'_>, captures: &Captures<'_>) -> Result<(), ExnThrown> {
    use pipe_captures::*;
    let src = captures.obj(SRC)?;
    let dest = captures.get(DEST)?;
    let state = readable_state(scope, src)?;
    if !captures.flag(CLEANED_UP)? {
        let pipes = required_obj(scope, state, c"pipes")?;
        let count = pipes.num(scope, c"length")?;
        if count == 1.0 && same(scope, pipes.get_element(scope, 0)?, dest)? {
            state.set_property(scope, c"awaitDrainWriters", dest)?;
            state.set_flag(scope, c"multiAwaitDrain", false)?;
        } else if count > 1.0 && truthy(scope, call_method(scope, pipes, c"includes", &[dest])?) {
            if let Some(writers) = state.obj(scope, c"awaitDrainWriters")? {
                call_method(scope, writers, c"add", &[dest])?;
            }
        }
        call_method(scope, src, c"pause", &[])?;
    }
    if captures.get(ONDRAIN)?.is_undefined() {
        let ondrain = closure(
            scope,
            0,
            pipe_ondrain,
            &[src.to_jsval_raw_throwing(scope)?, dest.get()],
        )?;
        captures.set(ONDRAIN, ondrain)?;
        listen(scope, captures.obj(DEST)?, c"on", "drain", ondrain)?;
    }
    Ok(())
}

/// Node's `pipeOnDrain`, whose captures are `[src, dest]`.
fn pipe_ondrain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let src = captures.obj(0)?;
    let dest = captures.get(1)?;
    let state = readable_state(scope, src)?;
    let writers = state.val(scope, c"awaitDrainWriters")?;
    if same(scope, writers, dest)? {
        state.set_property(scope, c"awaitDrainWriters", js::value::null())?;
    } else if state.flag(scope, c"multiAwaitDrain")? {
        if let Some(writers) = as_object(scope, writers) {
            call_method(scope, writers, c"delete", &[dest])?;
        }
    }
    let writers = state.val(scope, c"awaitDrainWriters")?;
    let empty = match as_object(scope, writers) {
        None => true,
        Some(set) if state.flag(scope, c"multiAwaitDrain")? => set.num(scope, c"size")? == 0.0,
        Some(_) => false,
    };
    if empty && state.flag(scope, c"_dataListening")? {
        call_method(scope, src, c"resume", &[])?;
    }
    Ok(js::value::undefined())
}

fn pipe_onend(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    call_method(
        scope,
        pipe_array(scope, payload)?.obj(pipe_captures::DEST)?,
        c"end",
        &[],
    )?;
    Ok(js::value::undefined())
}

fn pipe_unpipe(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = pipe_array(scope, payload)?;
    call_method(
        scope,
        captures.obj(pipe_captures::SRC)?,
        c"unpipe",
        &[captures.get(pipe_captures::DEST)?],
    )?;
    Ok(js::value::undefined())
}

fn pipe_onunpipe(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = pipe_array(scope, payload)?;
    if same(scope, args.get(0), captures.get(pipe_captures::SRC)?)? {
        if let Some(info) = as_object(scope, args.get(1)) {
            let unpiped = info.get_property(scope, c"hasUnpiped")?;
            if unpiped.is_boolean() && !unpiped.to_boolean() {
                info.set_property(scope, c"hasUnpiped", true)?;
                pipe_cleanup(scope, &captures)?;
            }
        }
    }
    Ok(js::value::undefined())
}

/// Node's `cleanup` inside `pipe`.
fn pipe_cleanup(scope: &Scope<'_>, captures: &Captures<'_>) -> Result<(), ExnThrown> {
    use pipe_captures::*;
    let src = captures.obj(SRC)?;
    let dest = captures.obj(DEST)?;
    let remove = |emitter: Object<'_>, event: &str, index: u32| -> Result<(), ExnThrown> {
        if let Some(listener) = as_object(scope, captures.get(index)?) {
            listen(scope, emitter, c"removeListener", event, listener)?;
        }
        Ok(())
    };
    remove(dest, "close", ONCLOSE)?;
    remove(dest, "finish", ONFINISH)?;
    remove(dest, "drain", ONDRAIN)?;
    remove(dest, "error", ONERROR)?;
    remove(dest, "unpipe", ONUNPIPE)?;
    remove(src, "end", ONEND)?;
    remove(src, "end", UNPIPE)?;
    remove(src, "data", ONDATA)?;
    captures.set(CLEANED_UP, true)?;
    let ondrain = captures.get(ONDRAIN)?;
    let state = readable_state(scope, src)?;
    if !ondrain.is_undefined() && truthy(scope, state.get_property(scope, c"awaitDrainWriters")?) {
        let needs = match dest.obj(scope, c"_writableState")? {
            None => true,
            Some(w) => w.flag(scope, c"needDrain")?,
        };
        if needs {
            call(scope, HandleValue::undefined(), ondrain, &[])?;
        }
    }
    Ok(())
}

fn pipe_onerror(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use pipe_captures::*;
    let captures = pipe_array(scope, payload)?;
    let dest = captures.obj(DEST)?;
    call_method(scope, captures.obj(SRC)?, c"unpipe", &[captures.get(DEST)?])?;
    if let Some(onerror) = as_object(scope, captures.get(ONERROR)?) {
        listen(scope, dest, c"removeListener", "error", onerror)?;
    }
    let error_event = string(scope, "error")?;
    if listener_count(scope, dest, error_event)? == 0.0 {
        let (readable, writable) = states(scope, dest)?;
        let error = args.get(0);
        match writable.or(readable) {
            Some(state) if !state.flag(scope, c"errorEmitted")? => {
                error_or_destroy(scope, dest, error, false)?
            }
            _ => {
                emit(scope, dest, "error", &[error])?;
            }
        }
    }
    Ok(js::value::undefined())
}

fn pipe_onclose(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = pipe_array(scope, payload)?;
    if let Some(onfinish) = as_object(scope, captures.get(pipe_captures::ONFINISH)?) {
        listen(
            scope,
            captures.obj(pipe_captures::DEST)?,
            c"removeListener",
            "finish",
            onfinish,
        )?;
    }
    pipe_unpipe(scope, args, payload)
}

fn pipe_onfinish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = pipe_array(scope, payload)?;
    if let Some(onclose) = as_object(scope, captures.get(pipe_captures::ONCLOSE)?) {
        listen(
            scope,
            captures.obj(pipe_captures::DEST)?,
            c"removeListener",
            "close",
            onclose,
        )?;
    }
    pipe_unpipe(scope, args, payload)
}

/// The `{ hasUnpiped: false }` argument of an `unpipe` event.
fn unpipe_info<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let info = Object::new_plain(scope)?;
    info.set_property(scope, c"hasUnpiped", false)?;
    info.to_jsval_throwing(scope)
}

/// `readable.unpipe(dest)`.
pub(crate) fn unpipe<'s>(
    scope: &Scope<'_>,
    stream: Object<'s>,
    dest: Option<HandleValue<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let state = readable_state(scope, stream)?;
    let pipes = required_obj(scope, state, c"pipes")?;
    if pipes.num(scope, c"length")? == 0.0 {
        return Ok(stream);
    }
    let stream_value = stream.to_jsval_throwing(scope)?;
    let dest = dest.filter(|dest| truthy(scope, *dest));
    let Some(dest) = dest else {
        let length = pipes.num(scope, c"length")? as u32;
        let dests: Vec<HandleValue<'_>> = (0..length)
            .map(|index| pipes.get_element(scope, index))
            .collect::<Result<_, _>>()?;
        state.set_property(
            scope,
            c"pipes",
            *js::Array::with_contents(scope, &[] as &[Value])?,
        )?;
        call_method(scope, stream, c"pause", &[] as &[HandleValue<'_>])?;
        for dest in dests {
            if let Some(dest) = as_object(scope, dest) {
                emit(scope, dest, "unpipe", &[stream_value, unpipe_info(scope)?])?;
            }
        }
        return Ok(stream);
    };
    let index = call_method(scope, pipes, c"indexOf", &[dest])?.to_number();
    if index < 0.0 {
        return Ok(stream);
    }
    let start = scope.root_value(js::value::from_f64(index));
    let one = scope.root_value(js::value::from_f64(1.0));
    call_method(scope, pipes, c"splice", &[start, one])?;
    if pipes.num(scope, c"length")? == 0.0 {
        call_method(scope, stream, c"pause", &[] as &[HandleValue<'_>])?;
    }
    if let Some(dest) = as_object(scope, dest) {
        emit(scope, dest, "unpipe", &[stream_value, unpipe_info(scope)?])?;
    }
    Ok(stream)
}

/// The captures of the async iterator's functions: `[stream, error, callback, cleanup, readable
/// listener, destroyOnReturn, done, pending, started]`. `error` is `undefined` while the stream
/// hasn't finished, `null` once it ended, and the error otherwise. `pending` is the promise of the
/// last `next()` call. `started` is set by the first `next()` call.
mod iterator_captures {
    pub(super) const STREAM: u32 = 0;
    pub(super) const ERROR: u32 = 1;
    pub(super) const CALLBACK: u32 = 2;
    pub(super) const CLEANUP: u32 = 3;
    pub(super) const LISTENER: u32 = 4;
    pub(super) const DESTROY_ON_RETURN: u32 = 5;
    pub(super) const DONE: u32 = 6;
    pub(super) const PENDING: u32 = 7;
    pub(super) const STARTED: u32 = 8;
    pub(super) const COUNT: usize = 9;
}

/// Node's `createAsyncIterator`, as an object with `next`, `return` and `throw`.
pub(crate) fn async_iterator<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    use iterator_captures::*;
    let options = match options.filter(|options| !options.is_undefined()) {
        Some(options) => Some(crate::errors::validate_object(scope, options, "options")?),
        None => None,
    };
    let destroy_on_return = match options {
        Some(options) => {
            let value = options.get_property(scope, c"destroyOnReturn")?;
            !(value.is_boolean() && !value.to_boolean())
        }
        None => true,
    };
    let state_array = js::Array::with_contents(scope, &[js::value::undefined(); COUNT])?;
    let captures = Captures::of(scope, (*state_array).to_jsval_throwing(scope)?)?;
    captures.set(STREAM, stream)?;
    captures.set(DESTROY_ON_RETURN, destroy_on_return)?;
    captures.set(DONE, false)?;
    captures.set(STARTED, false)?;

    let iterator = Object::new_plain(scope)?;
    iterator.set_prototype(scope, js::class::get_async_iterator_prototype(scope)?)?;
    for (name, body) in [
        (c"next", iterator_next as Callback),
        (c"return", iterator_return),
        (c"throw", iterator_throw),
    ] {
        let function = Function::new_callback(scope, name, 0, body, *state_array)?;
        iterator.set_property(scope, name, *function)?;
    }
    iterator.set_property(scope, c"stream", stream)?;
    Ok(iterator)
}

/// The start of Node's generator body: listen for `readable`, and record the end of the stream.
fn iterator_start(scope: &Scope<'_>, captures: &Captures<'_>) -> Result<(), Thrown> {
    use iterator_captures::*;
    captures.set(STARTED, true)?;
    let stream = captures.obj(STREAM)?;
    let listener = closure(scope, 0, iterator_on_readable, &[captures.array_value()?])?;
    captures.set(LISTENER, listener)?;
    listen(scope, stream, c"on", "readable", listener)?;
    let eos_callback = closure(scope, 1, iterator_on_end, &[captures.array_value()?])?;
    let eos_options = Object::new_plain(scope)?;
    eos_options.set_property(scope, c"writable", false)?;
    let cleanup = super::eos::eos(
        scope,
        stream.to_jsval_throwing(scope)?,
        Some(eos_options.to_jsval_throwing(scope)?),
        eos_callback.to_jsval_throwing(scope)?,
    )?;
    captures.set(CLEANUP, cleanup)?;
    Ok(())
}

/// Call the iterator's pending callback, if any, and clear it.
fn iterator_wake(scope: &Scope<'_>, captures: &Captures<'_>) -> Result<(), ExnThrown> {
    use iterator_captures::*;
    let callback = captures.get(CALLBACK)?;
    captures.set(CALLBACK, js::value::undefined())?;
    if is_function(scope, callback) {
        call(scope, HandleValue::undefined(), callback, &[])?;
    }
    Ok(())
}

fn iterator_on_readable(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    iterator_wake(
        scope,
        &Captures::of(scope, Captures::of(scope, payload)?.get(0)?)?,
    )?;
    Ok(js::value::undefined())
}

/// The `finished` callback of the iterator: record the error, or `null` for a clean end.
fn iterator_on_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use iterator_captures::*;
    let captures = Captures::of(scope, Captures::of(scope, payload)?.get(0)?)?;
    let error = args.get(0);
    if truthy(scope, error) {
        let combined = destroy::aggregate_two_errors(scope, captures.get(ERROR)?, error)?;
        captures.set(ERROR, combined)?;
    } else {
        captures.set(ERROR, js::value::null())?;
    }
    iterator_wake(scope, &captures)?;
    Ok(js::value::undefined())
}

/// The `finally` block of Node's generator: destroy the stream, or stop listening to it.
fn iterator_finish(scope: &Scope<'_>, captures: &Captures<'_>) -> Result<(), ExnThrown> {
    use iterator_captures::*;
    if captures.flag(DONE)? {
        return Ok(());
    }
    captures.set(DONE, true)?;
    let stream = captures.obj(STREAM)?;
    let error = captures.get(ERROR)?;
    let preserve_half_open_duplex = error.is_null()
        && is_true(stream.get_property(scope, c"allowHalfOpen")?)
        && is_true(stream.get_property(scope, c"writable")?)
        && !is_true(stream.get_property(scope, c"writableEnded")?);
    let auto_destroy = readable_state(scope, stream)?.flag(scope, c"autoDestroy")?;
    if (truthy(scope, error) || captures.flag(DESTROY_ON_RETURN)?)
        && (error.is_undefined() || auto_destroy)
        && !preserve_half_open_duplex
    {
        destroy::destroyer(scope, stream.to_jsval_throwing(scope)?, HandleValue::null())?;
    } else {
        if let Some(listener) = as_object(scope, captures.get(LISTENER)?) {
            listen(scope, stream, c"off", "readable", listener)?;
        }
        let cleanup = captures.get(CLEANUP)?;
        if is_function(scope, cleanup) {
            call(scope, HandleValue::undefined(), cleanup, &[])?;
        }
    }
    Ok(())
}

/// One step of the generator's loop, which settles `promise`: with the next chunk one microtask
/// later (the `await` of the generator's `yield`), with the end or the error right away, or, if no
/// chunk is available yet, once the stream signals more data or its end.
fn iterator_step(
    scope: &Scope<'_>,
    state: HandleValue<'_>,
    promise: js::Promise<'_>,
) -> Result<(), ExnThrown> {
    use iterator_captures::*;
    let captures = Captures::of(scope, state)?;
    let undefined_value = HandleValue::undefined();
    if captures.flag(DONE)? {
        let result = js::iteration::create_iter_result(scope, undefined_value, true)?;
        return promise.resolve(scope, result);
    }
    let stream = captures.obj(STREAM)?;
    let chunk = if truthy(scope, stream.get_property(scope, c"destroyed")?) {
        HandleValue::null()
    } else {
        call_method(scope, stream, c"read", &[] as &[HandleValue<'_>])?
    };
    if !chunk.is_null() {
        let awaited = js::Promise::call_original_resolve(scope, chunk)?;
        let fulfilled = closure(scope, 1, iterator_yield, &[promise])?;
        let rejected = closure(scope, 1, iterator_reject, &[promise])?;
        awaited.then(scope, Some(fulfilled), Some(rejected))?;
        return Ok(());
    }
    let error = captures.get(ERROR)?;
    if truthy(scope, error) {
        iterator_finish(scope, &captures)?;
        return promise.reject(scope, error);
    }
    if error.is_null() {
        iterator_finish(scope, &captures)?;
        let result = js::iteration::create_iter_result(scope, undefined_value, true)?;
        return promise.resolve(scope, result);
    }
    let resume = closure(
        scope,
        0,
        iterator_resume,
        &[state.get(), promise.to_jsval_raw_throwing(scope)?],
    )?;
    captures.set(CALLBACK, resume)
}

/// Fulfill the captured promise with `{ value, done: false }`, whose captures are `[promise]`.
fn iterator_yield(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = captured_promise(scope, &Captures::of(scope, payload)?, 0)?;
    let result = js::iteration::create_iter_result(scope, args.get(0), false)?;
    promise.resolve(scope, result)?;
    Ok(js::value::undefined())
}

/// Reject the captured promise with the argument, whose captures are `[promise]`.
fn iterator_reject(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = captured_promise(scope, &Captures::of(scope, payload)?, 0)?;
    promise.reject(scope, args.get(0))?;
    Ok(js::value::undefined())
}

/// The captured promise at `index`.
fn captured_promise<'s>(
    scope: &'s Scope<'_>,
    captures: &Captures<'s>,
    index: u32,
) -> Result<js::Promise<'s>, ExnThrown> {
    captures
        .obj(index)?
        .cast::<js::Promise>()
        .map_err(|_| type_error(scope, "not a promise"))
}

/// Run [`iterator_step`] for the captures `[state, promise]`, rejecting the promise with an
/// exception it throws.
fn iterator_step_captured(scope: &Scope<'_>, payload: HandleValue<'_>) -> Result<(), ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let promise = captured_promise(scope, &captures, 1)?;
    if iterator_step(scope, captures.get(0)?, promise).is_err() {
        let error = take_exception(scope);
        promise.reject(scope, error)?;
    }
    Ok(())
}

/// The function [`iterator_step`] stores in the `CALLBACK` capture when no chunk is available,
/// whose captures are `[state, promise]`: take the next step one microtask later, when Node's
/// generator resumes from awaiting the promise this callback resolves.
fn iterator_resume(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let job = Function::new_callback(scope, c"", 0, iterator_resumed, payload)?;
    js::jobs::queue_microtask(scope, &job)?;
    Ok(js::value::undefined())
}

/// The microtask [`iterator_resume`] queues, whose captures are `[state, promise]`.
fn iterator_resumed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    iterator_step_captured(scope, payload)?;
    Ok(js::value::undefined())
}

/// Run the step after the previous `next()` settled, whose captures are `[state, promise]`.
fn iterator_chained_step(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    iterator_step_captured(scope, payload)?;
    Ok(js::value::undefined())
}

fn iterator_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use iterator_captures::*;
    let captures = Captures::of(scope, payload)?;
    if !captures.flag(STARTED)? && !captures.flag(DONE)? {
        if let Err(error) = iterator_start(scope, &captures) {
            error.throw(scope);
            let error = take_exception(scope);
            return js::Promise::new_rejected_with_error(scope, error)?
                .to_jsval_raw_throwing(scope);
        }
    }
    let promise = js::Promise::new_pending(scope)?;
    let pending = captures.get(PENDING)?;
    match as_object(scope, pending).and_then(|pending| pending.cast::<js::Promise>().ok()) {
        Some(previous) if previous.is_pending() => {
            let step = closure(
                scope,
                0,
                iterator_chained_step,
                &[payload.get(), promise.to_jsval_raw_throwing(scope)?],
            )?;
            previous.then(scope, Some(step), Some(step))?;
        }
        _ => {
            if iterator_step(scope, payload, promise).is_err() {
                let error = take_exception(scope);
                promise.reject(scope, error)?;
            }
        }
    }
    captures.set(PENDING, *promise)?;
    promise.to_jsval_raw_throwing(scope)
}

/// `return(value)`: once `value` settles, as the generator's `await` of it does, run the
/// generator's `finally` block and fulfill with `{ value, done: true }`. If `value` rejects, the
/// generator records the rejection as its error, runs the `finally` block and rejects. For an
/// iterator whose `next()` was never called, the `finally` block doesn't run.
fn iterator_return(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use iterator_captures::*;
    let captures = Captures::of(scope, payload)?;
    if !captures.flag(STARTED)? {
        captures.set(DONE, true)?;
    }
    let promise = js::Promise::new_pending(scope)?;
    let shared = [payload, promise.to_jsval_throwing(scope)?];
    let fulfilled = closure(scope, 1, iterator_returned, &shared)?;
    let rejected = closure(scope, 1, iterator_return_rejected, &shared)?;
    js::Promise::call_original_resolve(scope, args.get(0))?.then(
        scope,
        Some(fulfilled),
        Some(rejected),
    )?;
    promise.to_jsval_raw_throwing(scope)
}

/// The fulfillment of `return(value)`'s value, whose captures are `[state, promise]`.
fn iterator_returned(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let promise = captured_promise(scope, &captures, 1)?;
    iterator_finish(scope, &Captures::of(scope, captures.get(0)?)?)?;
    let result = js::iteration::create_iter_result(scope, args.get(0), true)?;
    promise.resolve(scope, result)?;
    Ok(js::value::undefined())
}

/// The rejection of `return(value)`'s value, whose captures are `[state, promise]`.
fn iterator_return_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use iterator_captures::*;
    let captures = Captures::of(scope, payload)?;
    let promise = captured_promise(scope, &captures, 1)?;
    let state = Captures::of(scope, captures.get(0)?)?;
    if state.flag(DONE)? {
        promise.reject(scope, args.get(0))?;
        return Ok(js::value::undefined());
    }
    let combined = destroy::aggregate_two_errors(scope, state.get(ERROR)?, args.get(0))?;
    state.set(ERROR, combined)?;
    iterator_finish(scope, &state)?;
    promise.reject(scope, combined)?;
    Ok(js::value::undefined())
}

/// `throw(error)`. For an iterator whose `next()` was never called, it rejects with `error`
/// without running the generator's `finally` block.
fn iterator_throw(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use iterator_captures::*;
    let captures = Captures::of(scope, payload)?;
    if !captures.flag(STARTED)? {
        captures.set(DONE, true)?;
        return js::Promise::new_rejected_with_error(scope, args.get(0))?
            .to_jsval_raw_throwing(scope);
    }
    let combined = destroy::aggregate_two_errors(scope, captures.get(ERROR)?, args.get(0))?;
    captures.set(ERROR, combined)?;
    iterator_finish(scope, &captures)?;
    js::Promise::new_rejected_with_error(scope, combined)?.to_jsval_raw_throwing(scope)
}

/// The listeners `wrap` adds to the wrapped stream, whose captures are `[readable, wrapped,
/// paused]`.
fn wrap_ondata(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let readable = captures.obj(0)?;
    let wrapped = captures.obj(1)?;
    let pushed = call_method(scope, readable, c"push", &[args.get(0)])?;
    if !truthy(scope, pushed) && has_method(scope, wrapped, c"pause")? {
        captures.set(2, true)?;
        call_method(scope, wrapped, c"pause", &[])?;
    }
    Ok(js::value::undefined())
}

fn wrap_onend(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let readable = Captures::of(scope, payload)?.obj(0)?;
    call_method(scope, readable, c"push", &[HandleValue::null()])?;
    Ok(js::value::undefined())
}

fn wrap_onerror(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let readable = Captures::of(scope, payload)?.obj(0)?;
    error_or_destroy(scope, readable, args.get(0), false)?;
    Ok(js::value::undefined())
}

fn wrap_onclose(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let readable = Captures::of(scope, payload)?.obj(0)?;
    call_method(scope, readable, c"destroy", &[])?;
    Ok(js::value::undefined())
}

fn wrap_read(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let wrapped = captures.obj(1)?;
    if captures.flag(2)? && has_method(scope, wrapped, c"resume")? {
        captures.set(2, false)?;
        call_method(scope, wrapped, c"resume", &[])?;
    }
    Ok(js::value::undefined())
}

/// `Object.keys(object)`: the own enumerable string keys of `object`.
fn enumerable_keys<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let object_prototype = Object::new_plain(scope)?
        .get_prototype(scope)?
        .ok_or_else(|| type_error(scope, "Object.prototype is missing"))?;
    let is_enumerable = object_prototype.val(scope, c"propertyIsEnumerable")?;
    let this = object.to_jsval_throwing(scope)?;
    let mut keys = Vec::new();
    for key in object.own_keys(scope)? {
        if key.is_string() && truthy(scope, call(scope, this, is_enumerable, &[key])?) {
            keys.push(key);
        }
    }
    Ok(keys)
}

/// `readable.wrap(stream)`: read from an old-style stream.
pub(crate) fn wrap<'s>(
    scope: &Scope<'_>,
    readable: Object<'s>,
    wrapped: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let wrapped_object = as_object(scope, wrapped)
        .ok_or_else(|| js::error::TypeError("stream must be an object".into()).throw(scope))?;
    let payload = js::Array::with_contents(
        scope,
        &[
            readable.to_jsval_raw_throwing(scope)?,
            wrapped.get(),
            js::value::from_bool(false),
        ],
    )?;
    for (event, body) in [
        ("data", wrap_ondata as Callback),
        ("end", wrap_onend),
        ("error", wrap_onerror),
        ("close", wrap_onclose),
        ("destroy", wrap_onclose),
    ] {
        let listener = Function::new_callback(scope, c"", 1, body, *payload)?;
        listen(scope, wrapped_object, c"on", event, *listener)?;
    }
    let read = Function::new_callback(scope, c"_read", 0, wrap_read, *payload)?;
    readable.set_property(scope, c"_read", *read)?;
    for key in enumerable_keys(scope, wrapped_object)?.into_iter().skip(1) {
        let Some(name) = rust_string(scope, key) else {
            continue;
        };
        let Ok(name) = std::ffi::CString::new(name) else {
            continue;
        };
        let existing = readable.get_property_by_name(scope, &name)?;
        let method = wrapped_object.get_property_by_name(scope, &name)?;
        if existing.is_undefined() && is_function(scope, method) {
            let method_object = as_object(scope, method).expect("functions are objects");
            let bound = call_method(scope, method_object, c"bind", &[wrapped])?;
            readable.set_property_by_name(scope, &name, bound)?;
        }
    }
    Ok(readable)
}

/// The `destroy` option of a `Readable.wrap` stream, whose captures are `[src]`: destroy `src`
/// with the error, and call the callback with it.
fn wrap_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let src = Captures::of(scope, payload)?.get(0)?;
    destroy::destroyer(scope, src, args.get(0))?;
    call(scope, HandleValue::undefined(), args.get(1), &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// The static `Readable.wrap(src, options)`.
fn wrap_static<'s>(
    scope: &'s Scope<'_>,
    src: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let src_object = Object::from_value_coerce(scope, src)?;
    let mut object_mode = src_object.val(scope, c"readableObjectMode")?;
    if object_mode.is_null() || object_mode.is_undefined() {
        object_mode = src_object.val(scope, c"objectMode")?;
    }
    if object_mode.is_null() || object_mode.is_undefined() {
        object_mode = scope.root_value(js::value::from_bool(true));
    }
    let merged = Object::new_plain(scope)?;
    merged.set_property(scope, c"objectMode", object_mode)?;
    if let Some(options) = options.and_then(|options| as_object(scope, options)) {
        merged.assign(scope, options)?;
    }
    merged.set_property(
        scope,
        c"destroy",
        closure(scope, 2, wrap_destroy, &[src.get()])?,
    )?;
    let readable = Function::construct(scope, readable::class_constructor(scope)?, &[merged])?;
    wrap(scope, readable, src)
}

#[core_runtime::jsfunction_class(
    name = "Readable",
    extends = "super::super::legacy::stream",
    self_construct
)]
pub mod readable {
    use super::*;

    /// <https://nodejs.org/api/stream.html#new-streamreadableoptions>
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_readable(scope, this, options, false)?;
        super::super::legacy::stream::init(scope, this, options)?;
        super::finish_construction(scope, this)
    }

    /// <https://nodejs.org/api/stream.html#readabledestroyerror>
    pub fn destroy<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        error: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let error = error.unwrap_or(HandleValue::undefined());
        let callback = callback.unwrap_or(HandleValue::undefined());
        destroy::destroy(scope, this, error, callback)
    }

    /// Node's undocumented `readable._undestroy()`: reset both sides to their state before
    /// `destroy()`.
    #[method(name = "_undestroy")]
    pub fn undestroy(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        destroy::undestroy(scope, this)
    }

    /// <https://nodejs.org/api/stream.html#readable_destroyerr-callback>
    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        call(scope, HandleValue::undefined(), callback, &[error])?;
        Ok(())
    }

    /// <https://nodejs.org/api/stream.html#readablepushchunk-encoding>
    pub fn push(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<bool, ExnThrown> {
        readable_add_chunk(scope, this, chunk, encoding, false)
    }

    /// <https://nodejs.org/api/stream.html#readableunshiftchunk-encoding>
    pub fn unshift(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<bool, ExnThrown> {
        readable_add_chunk(scope, this, chunk, encoding, true)
    }

    /// <https://nodejs.org/api/stream.html#readableispaused>
    pub fn is_paused(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        let state = readable_state(scope, this)?;
        Ok(state.flag(scope, c"_paused")? || paused_flowing(scope, state)?)
    }

    /// <https://nodejs.org/api/stream.html#readablesetencodingencoding>
    pub fn set_encoding<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        encoding: HandleValue<'_>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::set_encoding(scope, this, encoding)
    }

    /// <https://nodejs.org/api/stream.html#readablereadsize>
    pub fn read<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        size: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let size = size.unwrap_or(HandleValue::undefined());
        super::read(scope, this, size)
    }

    /// <https://nodejs.org/api/stream.html#readable_readsize>: subclasses implement it.
    #[method(name = "_read")]
    pub fn read_impl() -> Result<(), NodeError> {
        Err(NodeError::not_implemented("_read()"))
    }

    /// <https://nodejs.org/api/stream.html#readablepipedestination-options>
    pub fn pipe<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        destination: HandleValue<'s>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        super::pipe(scope, this, destination, options)
    }

    /// <https://nodejs.org/api/stream.html#readableunpipedestination>
    pub fn unpipe<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        destination: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::unpipe(scope, this, destination)
    }

    /// <https://nodejs.org/api/events.html#emitteronEventname-listener>: also starts flowing for
    /// `data` and reading for `readable`.
    pub fn on<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        event: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::on(scope, this, event, listener)
    }

    /// <https://nodejs.org/api/events.html#emitteraddlistenereventname-listener>: `on`.
    pub fn add_listener<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        event: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::on(scope, this, event, listener)
    }

    /// <https://nodejs.org/api/events.html#emitterremovelistenereventname-listener>: also
    /// updates the `readable` and `data` listening state.
    pub fn remove_listener<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        event: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::remove_listener(scope, this, event, listener)
    }

    /// <https://nodejs.org/api/events.html#emitteroffeventname-listener>: `removeListener`.
    pub fn off<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        event: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::remove_listener(scope, this, event, listener)
    }

    /// <https://nodejs.org/api/events.html#emitterremovealllistenerseventname>: also updates the
    /// `readable` listening state.
    pub fn remove_all_listeners<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        event: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let ctor = crate::events::emitter_constructor(scope)?;
        let proto = required_obj(scope, ctor, c"prototype")?;
        let method = proto.val(scope, c"removeAllListeners")?;
        let args: Vec<HandleValue<'_>> = event.into_iter().collect();
        call(scope, this.to_jsval_throwing(scope)?, method, &args)?;
        let name = rust_string(scope, event.unwrap_or(HandleValue::undefined()));
        if event.is_none_or(|event| event.is_undefined()) || name.as_deref() == Some("readable") {
            next_tick(
                scope,
                update_readable_listening,
                &[this.to_jsval_raw_throwing(scope)?],
            )?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/stream.html#readableresume>
    pub fn resume<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        super::resume(scope, this)
    }

    /// <https://nodejs.org/api/stream.html#readablepause>
    pub fn pause<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        super::pause(scope, this)
    }

    /// <https://nodejs.org/api/stream.html#readablewrapstream>
    pub fn wrap<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        stream: HandleValue<'_>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::wrap(scope, this, stream)
    }

    /// <https://nodejs.org/api/stream.html#readableiteratoroptions>
    pub fn iterator<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::async_iterator(scope, this, options)
    }

    /// <https://nodejs.org/api/stream.html#streamreadablefromiterable-options>
    #[static_method]
    pub fn from<'s>(
        scope: &'s Scope<'_>,
        iterable: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::from::from(scope, iterable, options)
    }

    /// <https://nodejs.org/api/stream.html#streamreadablefromwebreadablestream-options>
    #[static_method]
    pub fn from_web<'s>(
        scope: &'s Scope<'_>,
        readable_stream: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::adapters::readable_from_web(scope, readable_stream, options)
    }

    /// <https://nodejs.org/api/stream.html#streamreadabletowebstreamreadable-options>
    #[static_method]
    pub fn to_web<'s>(
        scope: &'s Scope<'_>,
        stream_readable: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::adapters::readable_to_web(scope, stream_readable, options)
    }

    /// Node's static `Readable.wrap(src, options)`: a `Readable` with `options` that wraps the
    /// old-style stream `src`, and destroys it when destroyed. Its `objectMode` defaults to
    /// `src.readableObjectMode`, then `src.objectMode`, then `true`.
    #[static_method(name = "wrap")]
    pub fn wrap_stream<'s>(
        scope: &'s Scope<'_>,
        src: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::wrap_static(scope, src, options)
    }

    /// Define the accessors, `Symbol.asyncIterator`, the rejection handler, and `ReadableState`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        super::setup_readable(scope, ctor, proto)
    }
}

/// Create the `ReadableState` of `stream` and take the readable options: Node's
/// `new ReadableState()` and the option handling of the `Readable` and `Duplex` constructors.
pub(crate) fn init_readable(
    scope: &Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
    is_duplex: bool,
) -> Result<(), Thrown> {
    let state_ctor = readable_state::class_constructor(scope)?;
    let options_value = options.unwrap_or(HandleValue::undefined());
    let stream_value = stream.to_jsval_throwing(scope)?;
    let duplex_value = scope.root_value(js::value::from_bool(is_duplex));
    let state = js::Function::construct(
        scope,
        state_ctor,
        &[options_value, stream_value, duplex_value],
    )?;
    stream.set_property(scope, c"_readableState", state)?;
    if let Some(options) = options.and_then(|options| as_object(scope, options)) {
        for (option, method) in [
            (c"read", c"_read"),
            (c"destroy", c"_destroy"),
            (c"construct", c"_construct"),
        ] {
            let value = options.val(scope, option)?;
            if is_function(scope, value) {
                stream.set_property(scope, method, value)?;
            }
        }
        if !is_duplex {
            let signal = options.val(scope, c"signal")?;
            if truthy(scope, signal) {
                super::add_abort_signal(scope, signal, stream.to_jsval_throwing(scope)?)?;
            }
        }
    }
    Ok(())
}

/// Run the stream's `_construct`, if it has one, with a callback that calls [`on_constructed`]: the
/// end of the `Readable`, `Writable` and `Duplex` constructors.
pub(crate) fn finish_construction(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), Thrown> {
    let construct = stream.get_property(scope, c"_construct")?;
    if construct.is_null() || construct.is_undefined() {
        return Ok(());
    }
    let on_constructed = closure(
        scope,
        0,
        on_constructed,
        &[stream.to_jsval_raw_throwing(scope)?],
    )?;
    destroy::construct(scope, stream, on_constructed)?;
    Ok(())
}

/// The `kOnConstructed` of both states: start reading ahead if the readable side needs data, and
/// write what the writable side buffered.
fn on_constructed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let (readable, writable) = states(scope, stream)?;
    if let Some(state) = readable {
        if state.flag(scope, c"needReadable")? {
            maybe_read_more(scope, stream, state)?;
        }
    }
    if let Some(state) = writable {
        super::writable::on_constructed(scope, stream, state)?;
    }
    Ok(js::value::undefined())
}

/// Define a getter, and a setter if given, on `proto`. Each callback has one function object per
/// global, so defining the same callback on two prototypes defines the same function.
pub(crate) fn accessor(
    scope: &Scope<'_>,
    proto: Object<'_>,
    name: &CStr,
    getter: Callback,
    setter: Option<Callback>,
) -> Result<(), ExnThrown> {
    let shared = |body: Callback, nargs: u32| {
        js::class::get_or_init_shared_function(scope, body as usize, |scope| {
            Function::new_callback(scope, name, nargs, body, ())
        })
    };
    let get = shared(getter, 0)?;
    let set = setter.map(|setter| shared(setter, 1)).transpose()?;
    proto.define_accessor_by_name(scope, name, Some(*get), set.map(|set| *set), 0)
}

/// `this._readableState`, for the accessors.
fn this_state<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    match as_object(scope, scope.root_value(args.this())) {
        Some(this) => this.obj(scope, c"_readableState"),
        None => Ok(None),
    }
}

/// Define the accessor callback `$fn_name`, which returns `this._readableState[$field]`, or
/// `$default` if `this` has no readable state.
macro_rules! state_getter {
    ($fn_name:ident, $field:expr, $default:expr) => {
        fn $fn_name(
            scope: &Scope<'_>,
            args: CallbackArgs<'_>,
            _payload: HandleValue,
        ) -> Result<Value, ExnThrown> {
            match this_state(scope, &args)? {
                Some(state) => Ok(state.get_property(scope, $field)?.get()),
                None => Ok($default),
            }
        }
    };
}

state_getter!(
    get_readable_high_water_mark,
    c"highWaterMark",
    js::value::undefined()
);
state_getter!(get_readable_buffer, c"buffer", js::value::undefined());
state_getter!(get_readable_flowing, c"flowing", js::value::undefined());
state_getter!(get_readable_length, c"length", js::value::undefined());
state_getter!(
    get_readable_object_mode,
    c"objectMode",
    js::value::from_bool(false)
);
state_getter!(get_readable_encoding, c"encoding", js::value::null());
state_getter!(get_errored, c"errored", js::value::null());
state_getter!(get_closed, c"closed", js::value::from_bool(false));
state_getter!(get_destroyed, c"destroyed", js::value::from_bool(false));
state_getter!(
    get_readable_ended,
    c"endEmitted",
    js::value::from_bool(false)
);
state_getter!(
    get_readable_did_read,
    c"dataEmitted",
    js::value::from_bool(false)
);

fn get_readable(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(state) = this_state(scope, &args)? else {
        return Ok(js::value::from_bool(false));
    };
    let readable = state.get_property(scope, c"readable")?;
    let disabled = readable.is_boolean() && !readable.to_boolean();
    Ok(js::value::from_bool(
        !disabled
            && !state.flag(scope, c"destroyed")?
            && !state.flag(scope, c"errorEmitted")?
            && !state.flag(scope, c"endEmitted")?,
    ))
}

fn set_readable(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(state) = this_state(scope, &args)? {
        state.set_flag(scope, c"readable", truthy(scope, args.get(0)))?;
    }
    Ok(js::value::undefined())
}

fn set_readable_flowing(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(state) = this_state(scope, &args)? {
        state.set_property(scope, c"flowing", args.get(0))?;
    }
    Ok(js::value::undefined())
}

fn set_destroyed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(state) = this_state(scope, &args)? {
        state.set_property(scope, c"destroyed", args.get(0))?;
    }
    Ok(js::value::undefined())
}

fn get_readable_aborted(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(state) = this_state(scope, &args)? else {
        return Ok(js::value::from_bool(false));
    };
    let readable = state.get_property(scope, c"readable")?;
    let disabled = readable.is_boolean() && !readable.to_boolean();
    let failed =
        state.flag(scope, c"destroyed")? || truthy(scope, state.get_property(scope, c"errored")?);
    Ok(js::value::from_bool(
        !disabled && failed && !state.flag(scope, c"endEmitted")?,
    ))
}

/// `this[Symbol.asyncIterator]()`.
fn symbol_async_iterator(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = this_object(scope, &args)?;
    match async_iterator(scope, this, None) {
        Ok(iterator) => iterator.to_jsval_raw_throwing(scope),
        Err(error) => Err(error.throw(scope)),
    }
}

/// `this[EventEmitter.captureRejectionSymbol](error)`: destroy the stream with the error.
pub(crate) fn capture_rejection(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = this_object(scope, &args)?;
    call_method(scope, this, c"destroy", &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// `Readable.prototype[Symbol.asyncDispose]()`, or the `Writable` one if the payload is `true`:
/// destroy the stream with an `AbortError` unless it ended (or finished), and return a promise that
/// fulfills once `finished` reports the end. The promise rejects with an error `finished` reports
/// other than that `AbortError` (or, for `Writable`, other than any `AbortError`), or with an
/// exception `destroy` throws.
pub(crate) fn async_dispose(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let writable = is_true(payload);
    let promise = match async_dispose_start(scope, &args, writable) {
        Ok(promise) => promise,
        Err(_) => js::Promise::new_rejected_with_error(scope, take_exception(scope))?,
    };
    promise.to_jsval_raw_throwing(scope)
}

/// The body of [`async_dispose`].
fn async_dispose_start<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
    writable: bool,
) -> Result<js::Promise<'s>, ExnThrown> {
    let this = this_object(scope, args)?;
    let mut error = HandleValue::undefined();
    if !truthy(scope, this.get_property(scope, c"destroyed")?) {
        let done = if writable {
            c"writableFinished"
        } else {
            c"readableEnded"
        };
        error = if truthy(scope, this.get_property(scope, done)?) {
            HandleValue::null()
        } else {
            crate::errors::abort_error(scope, None)?
        };
        call_method(scope, this, c"destroy", &[error])?;
    }
    let closed = js::Promise::new_pending(scope)?;
    let callback = closure(
        scope,
        1,
        async_dispose_settle,
        &[
            closed.to_jsval_raw_throwing(scope)?,
            error.get(),
            js::value::from_bool(writable),
        ],
    )?;
    super::eos::eos(
        scope,
        this.to_jsval_throwing(scope)?,
        None,
        callback.to_jsval_throwing(scope)?,
    )
    .map_err(|error| error.throw(scope))?;
    let fulfilled = Function::new_callback(scope, c"", 0, return_undefined, ())?;
    closed.then(scope, Some(*fulfilled), None)
}

/// The `finished` callback of [`async_dispose`], whose captures are `[promise, error, writable]`.
fn async_dispose_settle(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let promise = captures
        .obj(0)?
        .cast::<js::Promise>()
        .map_err(|_| type_error(scope, "not a promise"))?;
    let error = args.get(0);
    let expected = if captures.flag(2)? {
        let name = match as_object(scope, error) {
            Some(error) => rust_string(scope, error.get_property(scope, c"name")?),
            None => None,
        };
        name.as_deref() == Some("AbortError")
    } else {
        same(scope, error, captures.get(1)?)?
    };
    if truthy(scope, error) && !expected {
        promise.reject(scope, error)?;
    } else {
        promise.resolve(scope, HandleValue::null())?;
    }
    Ok(js::value::undefined())
}

fn return_undefined(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// Define `proto[Symbol.asyncDispose]` as [`async_dispose`], for `Writable` if `writable`.
pub(crate) fn define_async_dispose(
    scope: &Scope<'_>,
    proto: Object<'_>,
    writable: bool,
) -> Result<(), ExnThrown> {
    let function = Function::new_callback(scope, c"", 0, async_dispose, writable)?;
    let key = js::symbol::get_well_known_key(scope, js::native::SymbolCode::asyncDispose);
    proto.set_property_by_id(scope, scope.root_id(key), *function)
}

/// Define the property keyed by `symbol` on `proto` as `function`.
pub(crate) fn define_symbol_method(
    scope: &Scope<'_>,
    proto: Object<'_>,
    symbol: HandleValue<'_>,
    name: &CStr,
    body: Callback,
) -> Result<(), ExnThrown> {
    let function = Function::new_callback(scope, name, 0, body, ())?;
    let key = scope.root_id(js::id::value_to_id(scope, symbol)?);
    proto.set_property_by_id(scope, key, (*function).to_jsval_throwing(scope)?)
}

/// The `Readable` class's [`readable::setup`].
fn setup_readable(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
    accessor(scope, proto, c"readable", get_readable, Some(set_readable))?;
    accessor(
        scope,
        proto,
        c"readableDidRead",
        get_readable_did_read,
        None,
    )?;
    accessor(scope, proto, c"readableAborted", get_readable_aborted, None)?;
    accessor(
        scope,
        proto,
        c"readableHighWaterMark",
        get_readable_high_water_mark,
        None,
    )?;
    accessor(scope, proto, c"readableBuffer", get_readable_buffer, None)?;
    accessor(
        scope,
        proto,
        c"readableFlowing",
        get_readable_flowing,
        Some(set_readable_flowing),
    )?;
    accessor(scope, proto, c"readableLength", get_readable_length, None)?;
    accessor(
        scope,
        proto,
        c"readableObjectMode",
        get_readable_object_mode,
        None,
    )?;
    accessor(
        scope,
        proto,
        c"readableEncoding",
        get_readable_encoding,
        None,
    )?;
    accessor(scope, proto, c"errored", get_errored, None)?;
    accessor(scope, proto, c"closed", get_closed, None)?;
    accessor(
        scope,
        proto,
        c"destroyed",
        get_destroyed,
        Some(set_destroyed),
    )?;
    accessor(scope, proto, c"readableEnded", get_readable_ended, None)?;
    let key = js::symbol::get_well_known_key(scope, js::native::SymbolCode::asyncIterator);
    let iterator = Function::new_callback(
        scope,
        c"[Symbol.asyncIterator]",
        0,
        symbol_async_iterator,
        (),
    )?;
    proto.set_property_by_id(
        scope,
        scope.root_id(key),
        (*iterator).to_jsval_throwing(scope)?,
    )?;
    let capture = crate::symbol(scope, "nodejs.rejection", true)?;
    define_symbol_method(scope, proto, capture, c"", capture_rejection)?;
    define_async_dispose(scope, proto, false)?;
    super::operators::define_operators(scope, proto)?;
    ctor.set_property(
        scope,
        c"ReadableState",
        readable_state::class_constructor(scope)?,
    )?;
    Ok(())
}
