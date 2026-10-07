// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Readable`: Node's `lib/internal/streams/readable.js`.

use std::ffi::CStr;

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, Callback, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyTemplate, PropertyWriter, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};

use super::destroy::{self, error_or_destroy, error_or_destroy_with};
use super::eos::{EosCallback, EosOptions};
use super::state::{
    AwaitDrainWriters, DrainWriters, ReadablePart, ReadableSide, StreamState, WritableSide, R, W,
};
use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::events::{listen, listener_count};
use crate::support::{has_method, is_str, is_true, rust_string, this_object};

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
    let object = Object::from_value(scope, view)
        .map_err(|_| js::error::TypeError(String::from("view is not an object")).throw(scope))?;
    let buffer = object.get_property(scope, c"buffer")?;
    let offset = object.get_property(scope, c"byteOffset")?;
    let length = object.get_property(scope, c"byteLength")?;
    buffer_from(scope, &[buffer, offset, length])
}

/// Whether `encoding` is an encoding `Buffer` supports.
pub(crate) fn is_encoding(scope: &Scope<'_>, encoding: HandleValue<'_>) -> Result<bool, ExnThrown> {
    Ok(crate::buffer::buffer::is_encoding(scope, encoding))
}

pub(crate) use crate::buffer::unknown_encoding;

/// A new `StringDecoder` for `encoding`.
fn new_decoder<'s>(
    scope: &'s Scope<'_>,
    encoding: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    crate::string_decoder::new_decoder(scope, encoding).map_err(|error| error.throw(scope))
}

/// The length of `chunk`: a string's length in UTF-16 code units, or a buffer's in bytes.
pub(crate) fn chunk_length(scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<f64, ExnThrown> {
    if chunk.is_string() {
        return Ok(js::JSString::from_value(scope, chunk)?.len() as f64);
    }
    let Ok(object) = Object::from_value(scope, chunk) else {
        return Ok(0.0);
    };
    // A typed array's `length` is its element count, which is read directly unless the object has
    // an own `length` property shadowing the prototype's getter.
    if let Some(view) = js::ArrayBufferView::from_object(object) {
        if view.view_kind().is_typed_array() && !object.has_own_property(scope, c"length")? {
            return Ok(view.array_length() as f64);
        }
    }
    let length = object.get_property(scope, c"length")?;
    Ok(if length.is_number() {
        length.to_number()
    } else {
        0.0
    })
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

/// `options[name]`, or `undefined` without options.
fn option<'s>(
    scope: &'s Scope<'_>,
    options: Option<Object<'_>>,
    name: &'static CStr,
) -> Result<HandleValue<'s>, ExnThrown> {
    match options {
        Some(options) => options.get_property(scope, name),
        None => Ok(HandleValue::undefined()),
    }
}

/// Give `holder`'s state a readable part for `options`: Node's `new ReadableState()`, of a
/// `Duplex`'s readable side if `is_duplex`.
pub(crate) fn init_readable_part(
    scope: &Scope<'_>,
    holder: StreamState<'_>,
    options: Option<HandleValue<'_>>,
    is_duplex: bool,
) -> Result<(), Thrown> {
    let options = options.and_then(|options| Object::from_value(scope, *options).ok());
    let mut flags = R::EMIT_CLOSE | R::AUTO_DESTROY | R::CONSTRUCTED | R::SYNC;
    let object_mode = js::conversion::to_boolean(option(scope, options, c"objectMode")?)
        || (is_duplex
            && js::conversion::to_boolean(option(scope, options, c"readableObjectMode")?));
    flags.set(R::OBJECT_MODE, object_mode);
    let high_water_mark = match options {
        Some(options) => super::state::high_water_mark(
            scope,
            options,
            c"readableHighWaterMark",
            is_duplex,
            object_mode,
        )?,
        None => super::state::default_high_water_mark(false),
    };
    flags.set(
        R::EMIT_CLOSE,
        !crate::support::is_false(option(scope, options, c"emitClose")?),
    );
    flags.set(
        R::AUTO_DESTROY,
        !crate::support::is_false(option(scope, options, c"autoDestroy")?),
    );
    let default_encoding = option(scope, options, c"defaultEncoding")?;
    let default_encoding = if default_encoding.is_null_or_undefined() {
        None
    } else if is_encoding(scope, default_encoding)? {
        super::state::default_encoding_field(scope, default_encoding)?
    } else {
        return Err(unknown_encoding(scope, default_encoding));
    };
    let encoding = option(scope, options, c"encoding")?;
    let (decoder, encoding) = if js::conversion::to_boolean(encoding) {
        (Some(new_decoder(scope, encoding)?), Some(encoding))
    } else {
        (None, None)
    };
    holder.data_mut().readable = Some(ReadablePart {
        flags,
        flowing: None,
        paused: None,
        high_water_mark,
        length: 0.0,
        buffer_index: 0.0,
        buffer: Vec::new(),
        pipes: Vec::new(),
        await_drain_writers: AwaitDrainWriters::Value(Heap::from(js::value::null())),
        errored: None,
        default_encoding: default_encoding.map(|value| Heap::from(value.get())),
        decoder: decoder.map(Heap::from),
        encoding: encoding.map(|encoding| Heap::from(encoding.get())),
        readable: Heap::default(),
    });
    Ok(())
}

/// Whether the stream is flowing: `state.flowing === true`.
fn flowing(side: ReadableSide<'_>) -> bool {
    side.get().flowing == Some(true)
}

/// Node's `canPushMore`.
fn can_push_more(side: ReadableSide<'_>) -> bool {
    let part = side.get();
    !part.flags.contains(R::ENDED) && (part.length < part.high_water_mark || part.length == 0.0)
}

/// Node's `readableAddChunk`, for `push` and `unshift` (if `add_to_front`).
fn readable_add_chunk(
    scope: &Scope<'_>,
    stream: Object<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
    add_to_front: bool,
) -> Result<bool, ExnThrown> {
    let side = ReadableSide::of(scope, stream)?;
    if chunk.is_null() {
        side.set(R::READING, false);
        on_eof_chunk(scope, stream, side)?;
        return Ok(false);
    }
    let object_mode = side.has(R::OBJECT_MODE);
    let mut chunk = scope.root_value(chunk.get());
    let mut encoding = match encoding {
        Some(encoding) if js::conversion::to_boolean(encoding) => scope.root_value(encoding.get()),
        _ => js::class::get_or_init_atom(scope, c"")?,
    };
    if !object_mode {
        if chunk.is_string() {
            if !js::conversion::to_boolean(encoding) {
                encoding = side.default_encoding(scope)?;
            }
            let state_encoding = side.encoding(scope).unwrap_or(HandleValue::null());
            if !js::comparison::strictly_equal(scope, state_encoding, encoding)? {
                if add_to_front && js::conversion::to_boolean(state_encoding) {
                    let buffer = buffer_from(scope, &[chunk, encoding])?;
                    let buffer = Object::from_value(scope, buffer).map_err(|_| {
                        js::error::TypeError(String::from("Buffer.from returned a primitive"))
                            .throw(scope)
                    })?;
                    chunk = Function::call_by_name(
                        scope,
                        buffer.handle(),
                        c"toString",
                        &[state_encoding],
                    )?;
                } else {
                    chunk = buffer_from(scope, &[chunk, encoding])?;
                    encoding = js::class::get_or_init_atom(scope, c"")?;
                }
            }
        } else if is_buffer(scope, chunk)? {
            encoding = js::class::get_or_init_atom(scope, c"")?;
        } else if js::ArrayBufferView::from_value(scope, chunk).is_some() {
            chunk = view_to_buffer(scope, chunk)?;
            encoding = js::class::get_or_init_atom(scope, c"")?;
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
            if side.has(R::END_EMITTED) {
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
            } else if side.has(R::DESTROYED) || side.is_errored() {
                return Ok(false);
            } else {
                add_chunk(scope, stream, side, chunk, true)?;
            }
        } else if side.has(R::ENDED) {
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
        } else if side.has(R::DESTROYED) || side.is_errored() {
            return Ok(false);
        } else {
            side.set(R::READING, false);
            match side.decoder(scope) {
                Some(decoder) if !js::conversion::to_boolean(encoding) => {
                    chunk = Function::call_by_name(scope, decoder.handle(), c"write", &[chunk])?;
                    if object_mode || chunk_length(scope, chunk)? != 0.0 {
                        add_chunk(scope, stream, side, chunk, false)?;
                    } else {
                        maybe_read_more(scope, stream, side)?;
                    }
                }
                _ => add_chunk(scope, stream, side, chunk, false)?,
            }
        }
    } else if !add_to_front {
        side.set(R::READING, false);
        maybe_read_more(scope, stream, side)?;
    }
    Ok(can_push_more(side))
}

/// The index of the first of `values` that is `value`, compared with `===`.
fn position(
    scope: &Scope<'_>,
    values: &[HandleValue<'_>],
    value: HandleValue<'_>,
) -> Result<Option<usize>, ExnThrown> {
    for (index, candidate) in values.iter().enumerate() {
        if js::comparison::strictly_equal(scope, *candidate, value)? {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// Clear the writers the stream waits for `drain` from.
fn clear_await_drain(side: ReadableSide<'_>) {
    if side.has(R::MULTI_AWAIT_DRAIN) {
        if let AwaitDrainWriters::Set(writers) = &mut side.get_mut().await_drain_writers {
            writers.clear();
        }
    } else {
        side.set_await_drain_writers(DrainWriters::Value(HandleValue::null()));
    }
}

/// Node's `addChunk`: emit the chunk as `data` if the stream is flowing with an empty buffer, and
/// buffer it otherwise.
fn add_chunk(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: ReadableSide<'_>,
    chunk: HandleValue<'_>,
    add_to_front: bool,
) -> Result<(), ExnThrown> {
    let emit_now = {
        let part = side.get();
        part.flowing == Some(true)
            && !part.flags.contains(R::SYNC)
            && part.flags.contains(R::DATA_LISTENING)
            && part.length == 0.0
    };
    if emit_now {
        clear_await_drain(side);
        side.set(R::DATA_EMITTED, true);
        crate::events::emit(scope, stream, c"data", &[chunk])?;
    } else {
        let added = added_length(scope, chunk, side.has(R::OBJECT_MODE))?;
        let mut part = side.get_mut();
        part.length += added;
        let index = part.buffer_index as usize;
        if add_to_front && index > 0 && index <= part.buffer.len() {
            part.buffer_index = (index - 1) as f64;
            part.buffer[index - 1].set(chunk.get());
        } else if add_to_front {
            part.buffer.insert(0, Heap::from(chunk.get()));
        } else {
            part.buffer.push(Heap::from(chunk.get()));
        }
        drop(part);
        if side.has(R::NEED_READABLE) {
            emit_readable(scope, stream, side)?;
        }
    }
    maybe_read_more(scope, stream, side)
}

/// Node's `onEofChunk`: end the stream, flushing the decoder.
fn on_eof_chunk(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: ReadableSide<'_>,
) -> Result<(), ExnThrown> {
    if side.has(R::ENDED) {
        return Ok(());
    }
    if let Some(decoder) = side.decoder(scope) {
        let chunk =
            Function::call_by_name(scope, decoder.handle(), c"end", js::function::EmptyArgs)?;
        if !chunk.is_null_or_undefined() && chunk_length(scope, chunk)? > 0.0 {
            side.get_mut().buffer.push(Heap::from(chunk.get()));
            let added = added_length(scope, chunk, side.has(R::OBJECT_MODE))?;
            side.get_mut().length += added;
        }
    }
    side.set(R::ENDED, true);
    if side.has(R::SYNC) {
        emit_readable(scope, stream, side)
    } else {
        side.set(R::NEED_READABLE, false);
        side.set(R::EMITTED_READABLE, true);
        emit_readable_now(scope, stream, side)
    }
}

/// Queue `body` in the tick queue with `stream` as its payload.
fn tick(scope: &Scope<'_>, body: Callback, stream: Object<'_>) -> Result<(), ExnThrown> {
    js::jobs::queue_native_tick(scope, body, stream, &[])
}

/// Node's `emitReadable`: emit `readable` in a tick, once.
fn emit_readable(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: ReadableSide<'_>,
) -> Result<(), ExnThrown> {
    side.set(R::NEED_READABLE, false);
    if !side.has(R::EMITTED_READABLE) {
        side.set(R::EMITTED_READABLE, true);
        tick(scope, emit_readable_tick, stream)?;
    }
    Ok(())
}

fn emit_readable_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = ReadableSide::of(scope, stream)?;
    emit_readable_now(scope, stream, side)?;
    Ok(js::value::undefined())
}

/// Node's `emitReadable_`.
fn emit_readable_now(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: ReadableSide<'_>,
) -> Result<(), ExnThrown> {
    let emit = {
        let part = side.get();
        !part.flags.contains(R::DESTROYED)
            && part.errored.is_none()
            && (part.length > 0.0 || part.flags.contains(R::ENDED))
    };
    if emit {
        crate::events::emit(scope, stream, c"readable", &[])?;
        side.set(R::EMITTED_READABLE, false);
    }
    let need_readable = {
        let part = side.get();
        part.flowing != Some(true)
            && !part.flags.contains(R::ENDED)
            && part.length <= part.high_water_mark
    };
    if need_readable {
        side.set(R::NEED_READABLE, true);
    }
    flow(scope, stream, side)
}

/// Node's `maybeReadMore`: read ahead in a tick, once.
fn maybe_read_more(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: ReadableSide<'_>,
) -> Result<(), ExnThrown> {
    if !side.has(R::READING_MORE) && side.has(R::CONSTRUCTED) {
        side.set(R::READING_MORE, true);
        tick(scope, maybe_read_more_tick, stream)?;
    }
    Ok(())
}

/// The value `0`, rooted.
fn zero<'s>(scope: &'s Scope<'_>) -> HandleValue<'s> {
    scope.root_value(js::value::from_i32(0))
}

/// Node's `maybeReadMore_`: call `read(0)` while that fills the buffer and it's below the high
/// water mark, or empty and flowing.
fn maybe_read_more_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = ReadableSide::of(scope, stream)?;
    let zero = zero(scope);
    loop {
        let (length, more) = {
            let part = side.get();
            let length = part.length;
            let more = !part.flags.intersects(R::READING | R::ENDED)
                && (length < part.high_water_mark || (part.flowing == Some(true) && length == 0.0));
            (length, more)
        };
        if !more {
            break;
        }
        Function::call_by_name(scope, stream.handle(), c"read", &[zero])?;
        if length == side.length() {
            break;
        }
    }
    side.set(R::READING_MORE, false);
    Ok(js::value::undefined())
}

/// Node's `howMuchToRead`. `n` is NaN for "everything".
fn how_much_to_read(scope: &Scope<'_>, n: f64, side: ReadableSide<'_>) -> Result<f64, ExnThrown> {
    let (length, ended, object_mode, flowing) = {
        let part = side.get();
        (
            part.length,
            part.flags.contains(R::ENDED),
            part.flags.contains(R::OBJECT_MODE),
            part.flowing == Some(true),
        )
    };
    if n <= 0.0 || (length == 0.0 && ended) {
        return Ok(0.0);
    }
    if object_mode {
        return Ok(1.0);
    }
    if n.is_nan() {
        if flowing && length > 0.0 {
            let index = side.get().buffer_index as usize;
            return chunk_length(scope, side.chunk(scope, index));
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
    let side = ReadableSide::of(scope, stream)?;
    let mut n = if n.is_undefined() {
        f64::NAN
    } else if n.is_number() && n.to_number().fract() == 0.0 {
        n.to_number()
    } else {
        parse_int(&js::JSString::from_value(scope, n)?.to_utf8(scope)?)
    };
    let n_orig = n;
    if n > side.high_water_mark() {
        let mark = compute_new_high_water_mark(scope, n)?;
        side.get_mut().high_water_mark = mark;
    }
    if n != 0.0 {
        side.set(R::EMITTED_READABLE, false);
    }
    let (length, mark, ended, need_readable) = {
        let part = side.get();
        (
            part.length,
            part.high_water_mark,
            part.flags.contains(R::ENDED),
            part.flags.contains(R::NEED_READABLE),
        )
    };
    if n == 0.0
        && need_readable
        && ((if mark != 0.0 {
            length >= mark
        } else {
            length > 0.0
        }) || ended)
    {
        if length == 0.0 && ended {
            end_readable(scope, stream, side)?;
        } else {
            emit_readable(scope, stream, side)?;
        }
        return Ok(HandleValue::null());
    }
    n = how_much_to_read(scope, n, side)?;
    if n == 0.0 && side.has(R::ENDED) {
        if side.length() == 0.0 {
            end_readable(scope, stream, side)?;
        }
        return Ok(HandleValue::null());
    }
    let (do_read, blocked) = {
        let part = side.get();
        let do_read = part.flags.contains(R::NEED_READABLE)
            || part.length == 0.0
            || part.length - n < part.high_water_mark;
        let blocked = part.flags.intersects(R::READING | R::ENDED | R::DESTROYED)
            || part.errored.is_some()
            || !part.flags.contains(R::CONSTRUCTED);
        (do_read, blocked)
    };
    if !blocked && do_read {
        side.set(R::READING | R::SYNC, true);
        if side.length() == 0.0 {
            side.set(R::NEED_READABLE, true);
        }
        let mark = scope.root_value(js::value::from_f64(side.high_water_mark()));
        if Function::call_by_name(scope, stream.handle(), c"_read", &[mark]).is_err() {
            let error = js::exception::take_pending_or_undefined(scope);
            error_or_destroy(scope, stream, error, false)?;
        }
        side.set(R::SYNC, false);
        if !side.has(R::READING) {
            n = how_much_to_read(scope, n_orig, side)?;
        }
    }
    let ret = if n > 0.0 {
        from_list(scope, n, side)?
    } else {
        HandleValue::null()
    };
    if ret.is_null() {
        if side.length() <= side.high_water_mark() {
            side.set(R::NEED_READABLE, true);
        }
        n = 0.0;
    } else {
        side.get_mut().length -= n;
        clear_await_drain(side);
    }
    if side.length() == 0.0 {
        if !side.has(R::ENDED) {
            side.set(R::NEED_READABLE, true);
        }
        let differs = !(n_orig == n || (n_orig.is_nan() && n.is_nan()));
        if differs && side.has(R::ENDED) {
            end_readable(scope, stream, side)?;
        }
    }
    if !ret.is_null() && !side.any(R::ERROR_EMITTED | R::CLOSE_EMITTED) {
        side.set(R::DATA_EMITTED, true);
        crate::events::emit(scope, stream, c"data", &[ret])?;
    }
    Ok(ret)
}

/// Node's `fromList`: take `n` bytes, characters or one object from the buffer.
fn from_list<'s>(
    scope: &'s Scope<'_>,
    n: f64,
    side: ReadableSide<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let (total, mut index, object_mode, decoding) = {
        let part = side.get();
        (
            part.length,
            part.buffer_index as usize,
            part.flags.contains(R::OBJECT_MODE),
            part.decoder.is_some(),
        )
    };
    if total == 0.0 {
        return Ok(HandleValue::null());
    }
    let len = side.buffer_len();
    let take = |index: &mut usize| -> HandleValue<'s> {
        let value = side.chunk(scope, *index);
        side.set_chunk(*index, HandleValue::null());
        *index += 1;
        value
    };
    let ret;
    if object_mode {
        ret = take(&mut index);
    } else if n.is_nan() || n >= total {
        if decoding {
            let mut text = js::JSString::empty(scope);
            while index < len {
                let part = js::JSString::from_value(scope, take(&mut index))?;
                text = text.concat(scope, part)?;
            }
            ret = text.to_jsval_throwing(scope)?;
        } else if len.saturating_sub(index) == 1 {
            ret = take(&mut index);
        } else {
            let mut parts = Vec::new();
            while index < len {
                parts.push(take(&mut index));
            }
            ret = concat_buffers(scope, &parts)?;
        }
    } else {
        let first = side.chunk(scope, index);
        let first_length = chunk_length(scope, first)?;
        if n < first_length {
            ret = slice(scope, first, 0.0, Some(n))?;
            let rest = slice(scope, first, n, None)?;
            side.set_chunk(index, rest);
        } else if n == first_length {
            ret = take(&mut index);
        } else {
            let mut remaining = n;
            let mut parts = Vec::new();
            while index < len && remaining > 0.0 {
                let data = side.chunk(scope, index);
                let data_length = chunk_length(scope, data)?;
                if remaining >= data_length {
                    parts.push(take(&mut index));
                    remaining -= data_length;
                } else {
                    parts.push(slice(scope, data, 0.0, Some(remaining))?);
                    let rest = slice(scope, data, remaining, None)?;
                    side.set_chunk(index, rest);
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
    let mut part = side.get_mut();
    if index == len {
        part.buffer.clear();
        part.buffer_index = 0.0;
    } else if index > 1024 {
        let taken = index.min(part.buffer.len());
        part.buffer.drain(..taken);
        part.buffer_index = 0.0;
    } else {
        part.buffer_index = index as f64;
    }
    Ok(ret)
}

/// `value.slice(start, end)` for a string or `Buffer`.
fn slice<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    start: f64,
    end: Option<f64>,
) -> Result<HandleValue<'s>, ExnThrown> {
    if value.is_string() {
        let text = js::JSString::from_value(scope, value)?;
        let start = (start as usize).min(text.len());
        let end = end
            .map_or(text.len(), |end| end as usize)
            .clamp(start, text.len());
        return text
            .substring(scope, start, end - start)?
            .to_jsval_throwing(scope);
    }
    let object = Object::from_value(scope, value).map_err(|_| {
        js::error::TypeError(String::from("chunk.slice is not a function")).throw(scope)
    })?;
    let start = scope.root_value(js::value::from_f64(start));
    match end {
        Some(end) => {
            let end = scope.root_value(js::value::from_f64(end));
            Function::call_by_name(scope, object.handle(), c"slice", &[start, end])
        }
        None => Function::call_by_name(scope, object.handle(), c"slice", &[start]),
    }
}

/// Node's `endReadable`: emit `end` in a tick.
fn end_readable(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: ReadableSide<'_>,
) -> Result<(), ExnThrown> {
    if !side.has(R::END_EMITTED) {
        side.set(R::ENDED, true);
        tick(scope, end_readable_tick, stream)?;
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
    let stream = cast_payload::<Object>(scope, payload);
    let side = ReadableSide::of(scope, stream)?;
    let emit = {
        let part = side.get();
        part.errored.is_none()
            && !part.flags.intersects(R::CLOSE_EMITTED | R::END_EMITTED)
            && part.length == 0.0
    };
    if !emit {
        return Ok(js::value::undefined());
    }
    side.set(R::END_EMITTED, true);
    crate::events::emit(scope, stream, c"end", &[])?;
    let allow_half_open = stream.get_property(scope, c"allowHalfOpen")?;
    if js::conversion::to_boolean(stream.get_property(scope, c"writable")?)
        && allow_half_open.is_boolean()
        && !allow_half_open.to_boolean()
    {
        tick(scope, end_writable_tick, stream)?;
    } else if side.has(R::AUTO_DESTROY) {
        let auto_destroy = match WritableSide::of_state(side.0) {
            None => true,
            Some(w) => {
                let part = w.get();
                part.flags.contains(W::AUTO_DESTROY)
                    && (part.flags.contains(W::FINISHED) || part.writable == Some(false))
            }
        };
        if auto_destroy {
            Function::call_by_name(scope, stream.handle(), c"destroy", js::function::EmptyArgs)?;
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
    let stream = cast_payload::<Object>(scope, payload);
    let writable = js::conversion::to_boolean(stream.get_property(scope, c"writable")?)
        && !js::conversion::to_boolean(stream.get_property(scope, c"writableEnded")?)
        && !js::conversion::to_boolean(stream.get_property(scope, c"destroyed")?);
    if writable {
        Function::call_by_name(scope, stream.handle(), c"end", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

/// Node's `flow`: read while flowing and reads return data.
fn flow(scope: &Scope<'_>, stream: Object<'_>, side: ReadableSide<'_>) -> Result<(), ExnThrown> {
    while flowing(side) {
        if Function::call_by_name(scope, stream.handle(), c"read", js::function::EmptyArgs)?
            .is_null()
        {
            break;
        }
    }
    Ok(())
}

/// `readable.resume()`.
pub(crate) fn resume<'s>(scope: &Scope<'_>, stream: Object<'s>) -> Result<Object<'s>, ExnThrown> {
    let side = ReadableSide::of(scope, stream)?;
    if !flowing(side) {
        let listening = side.has(R::READABLE_LISTENING);
        side.get_mut().flowing = Some(!listening);
        if !side.has(R::RESUME_SCHEDULED) {
            side.set(R::RESUME_SCHEDULED, true);
            tick(scope, resume_tick, stream)?;
        }
    }
    side.get_mut().paused = Some(false);
    Ok(stream)
}

/// Node's `resume_`.
fn resume_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = ReadableSide::of(scope, stream)?;
    let zero = zero(scope);
    if !side.has(R::READING) {
        Function::call_by_name(scope, stream.handle(), c"read", &[zero])?;
    }
    side.set(R::RESUME_SCHEDULED, false);
    crate::events::emit(scope, stream, c"resume", &[])?;
    flow(scope, stream, side)?;
    if flowing(side) && !side.has(R::READING) {
        Function::call_by_name(scope, stream.handle(), c"read", &[zero])?;
    }
    Ok(js::value::undefined())
}

/// `readable.pause()`.
pub(crate) fn pause<'s>(scope: &Scope<'_>, stream: Object<'s>) -> Result<Object<'s>, ExnThrown> {
    let side = ReadableSide::of(scope, stream)?;
    if side.get().flowing != Some(false) {
        side.get_mut().flowing = Some(false);
        crate::events::emit(scope, stream, c"pause", &[])?;
    }
    side.get_mut().paused = Some(true);
    Ok(stream)
}

/// Node's `updateReadableListening`.
fn update_readable_listening(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = ReadableSide::of(scope, stream)?;
    let readable = js::class::get_or_init_atom(scope, c"readable")?;
    let listening = listener_count(scope, stream, readable)? > 0.0;
    side.set(R::READABLE_LISTENING, listening);
    let (paused, resume_scheduled, data_listening) = {
        let part = side.get();
        (
            part.paused,
            part.flags.contains(R::RESUME_SCHEDULED),
            part.flags.contains(R::DATA_LISTENING),
        )
    };
    if paused == Some(false) && resume_scheduled {
        side.get_mut().flowing = Some(true);
    } else if data_listening {
        Function::call_by_name(scope, stream.handle(), c"resume", js::function::EmptyArgs)?;
    } else if !listening {
        side.get_mut().flowing = None;
    }
    Ok(js::value::undefined())
}

/// Node's `nReadingNextTick`.
fn read_zero_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    Function::call_by_name(scope, stream.handle(), c"read", &[zero(scope)])?;
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
    let side = ReadableSide::of(scope, stream)?;
    if is_str(scope, event, c"data")? {
        side.set(R::DATA_LISTENING, true);
        let readable = js::class::get_or_init_atom(scope, c"readable")?;
        if listener_count(scope, stream, readable)? > 0.0 {
            side.set(R::READABLE_LISTENING, true);
        }
        if side.get().flowing != Some(false) {
            Function::call_by_name(scope, stream.handle(), c"resume", js::function::EmptyArgs)?;
        }
    } else if !side.any(R::END_EMITTED | R::READABLE_LISTENING)
        && is_str(scope, event, c"readable")?
    {
        side.set(R::READABLE_LISTENING | R::NEED_READABLE, true);
        side.get_mut().flowing = Some(false);
        side.set(R::EMITTED_READABLE, false);
        if side.length() > 0.0 {
            emit_readable(scope, stream, side)?;
        } else if !side.has(R::READING) {
            tick(scope, read_zero_tick, stream)?;
        }
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
    let side = ReadableSide::of(scope, stream)?;
    if is_str(scope, event, c"readable")? {
        tick(scope, update_readable_listening, stream)?;
    } else if is_str(scope, event, c"data")? && listener_count(scope, stream, event)? == 0.0 {
        side.set(R::DATA_LISTENING, false);
    }
    Ok(result)
}

/// `readable.setEncoding(encoding)`.
pub(crate) fn set_encoding<'s>(
    scope: &Scope<'_>,
    stream: Object<'s>,
    encoding: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let side = ReadableSide::of(scope, stream)?;
    let decoder = new_decoder(scope, encoding)?;
    let decoder_encoding = decoder.get_property(scope, c"encoding")?;
    {
        let mut part = side.get_mut();
        part.decoder = Some(Heap::from(decoder));
        part.encoding = js::conversion::to_boolean(decoder_encoding)
            .then(|| Heap::from(decoder_encoding.get()));
    }
    let mut content = js::JSString::empty(scope);
    let mut index = side.get().buffer_index as usize;
    while index < side.buffer_len() {
        let data = side.chunk(scope, index);
        let written = Function::call_by_name(scope, decoder.handle(), c"write", &[data])?;
        content = content.concat(scope, js::JSString::from_value(scope, written)?)?;
        index += 1;
    }
    if side.has(R::ENDED) {
        let rest =
            Function::call_by_name(scope, decoder.handle(), c"end", js::function::EmptyArgs)?;
        content = content.concat(scope, js::JSString::from_value(scope, rest)?)?;
    }
    let content_value = content.to_jsval_throwing(scope)?;
    let mut part = side.get_mut();
    part.buffer.clear();
    part.buffer_index = 0.0;
    if !content.is_empty() {
        part.buffer.push(Heap::from(content_value.get()));
    }
    part.length = content.len() as f64;
    drop(part);
    Ok(stream)
}

/// The state of a `pipe`: the source and destination, and the listeners it adds.
#[core_runtime::jsclass(hidden)]
pub(crate) struct PipeState {
    src: Heap<js::object::Object>,
    dest: Heap<js::object::Object>,
    ondata: Heap<js::object::Object>,
    onend: Heap<js::object::Object>,
    unpipe: Heap<js::object::Object>,
    onunpipe: Heap<js::object::Object>,
    onerror: Heap<js::object::Object>,
    onclose: Heap<js::object::Object>,
    onfinish: Heap<js::object::Object>,
    /// The `drain` listener, which the first backpressure adds.
    ondrain: Option<Heap<js::object::Object>>,
    #[no_trace]
    cleaned_up: bool,
}

#[core_runtime::jsmethods]
impl PipeState {
    fn new(&self, scope: &Scope<'_>, src: Object<'_>, dest: Object<'_>) -> Result<(), ExnThrown> {
        let listener =
            |body: Callback, nargs: u32| Function::new_callback(scope, c"", nargs, body, self);
        let data = self.data();
        data.src.set(src);
        data.dest.set(dest);
        data.ondata.set(*listener(pipe_ondata, 1)?);
        data.onend.set(*listener(pipe_onend, 0)?);
        data.unpipe.set(*listener(pipe_unpipe, 0)?);
        data.onunpipe.set(*listener(pipe_onunpipe, 2)?);
        data.onerror.set(*listener(pipe_onerror, 1)?);
        data.onclose.set(*listener(pipe_onclose, 0)?);
        data.onfinish.set(*listener(pipe_onfinish, 0)?);
        Ok(())
    }
}

/// `readable.pipe(dest, options)`.
pub(crate) fn pipe<'s>(
    scope: &'s Scope<'_>,
    src: Object<'_>,
    dest: HandleValue<'s>,
    options: Option<HandleValue<'_>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let side = ReadableSide::of(scope, src)?;
    let dest_object = Object::from_value(scope, dest)
        .map_err(|_| js::error::TypeError("dest must be a stream".into()).throw(scope))?;
    if side.get().pipes.len() == 1 && !side.has(R::MULTI_AWAIT_DRAIN) {
        side.set(R::MULTI_AWAIT_DRAIN, true);
        let writers = match side.await_drain_writers(scope) {
            DrainWriters::Value(writer) if js::conversion::to_boolean(writer) => vec![writer],
            DrainWriters::Value(_) => Vec::new(),
            DrainWriters::Set(writers) => writers,
        };
        side.set_await_drain_writers(DrainWriters::Set(writers));
    }
    side.get_mut().pipes.push(Heap::from(dest.get()));

    let state = PipeState::new(scope, src, dest_object)?;
    let end_option = options
        .and_then(|options| Object::from_value(scope, options).ok())
        .map(|options| options.get_property(scope, c"end"))
        .transpose()?;
    let stdout_or_stderr = {
        let process = crate::process::process_value(scope)?;
        let mut matches = false;
        if let Ok(process) = Object::from_value(scope, process) {
            for name in [c"stdout", c"stderr"] {
                let stdio = process.get_property(scope, name)?;
                matches |= js::comparison::strictly_equal(scope, stdio, dest)?;
            }
        }
        matches
    };
    let do_end =
        !end_option.is_some_and(|end| end.is_boolean() && !end.to_boolean()) && !stdout_or_stderr;
    let (ondata, onend, unpipe, onunpipe, onerror, onclose, onfinish) = {
        let data = state.data();
        (
            data.ondata.get(scope),
            data.onend.get(scope),
            data.unpipe.get(scope),
            data.onunpipe.get(scope),
            data.onerror.get(scope),
            data.onclose.get(scope),
            data.onfinish.get(scope),
        )
    };
    let end_fn = if do_end { onend } else { unpipe };
    if side.has(R::END_EMITTED) {
        js::jobs::queue_tick(scope, end_fn.to_jsval_throwing(scope)?, &[])?;
    } else {
        listen(scope, src, c"once", c"end", end_fn)?;
    }
    listen(scope, dest_object, c"on", c"unpipe", onunpipe)?;
    listen(scope, src, c"on", c"data", ondata)?;
    prepend_listener(scope, dest_object, c"error", onerror)?;
    listen(scope, dest_object, c"once", c"close", onclose)?;
    listen(scope, dest_object, c"once", c"finish", onfinish)?;
    crate::events::emit(
        scope,
        dest_object,
        c"pipe",
        &[src.to_jsval_throwing(scope)?],
    )?;
    if is_true(dest_object.get_property(scope, c"writableNeedDrain")?) {
        pipe_pause(scope, state)?;
    } else if !flowing(side) {
        Function::call_by_name(scope, src.handle(), c"resume", js::function::EmptyArgs)?;
    }
    Ok(dest)
}

/// Node's `prependListener`.
pub(crate) fn prepend_listener(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    event: &'static CStr,
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
    let state = cast_payload::<PipeState>(scope, payload);
    let dest = state.data().dest.get(scope);
    match Function::call_by_name(scope, dest.handle(), c"write", &[args.get(0)]) {
        Ok(ret) => {
            if ret.is_boolean() && !ret.to_boolean() {
                pipe_pause(scope, state)?;
            }
        }
        Err(_) => {
            let error = js::exception::take_pending_or_undefined(scope);
            Function::call_by_name(scope, dest.handle(), c"destroy", &[error])?;
        }
    }
    Ok(js::value::undefined())
}

/// Node's `pause` inside `pipe`: record `dest` as awaited, pause the source, and listen for
/// `drain` on `dest`.
fn pipe_pause(scope: &Scope<'_>, state: PipeState<'_>) -> Result<(), ExnThrown> {
    let (src, dest, cleaned_up) = {
        let data = state.data();
        (data.src.get(scope), data.dest.get(scope), data.cleaned_up)
    };
    let dest_value = dest.to_jsval_throwing(scope)?;
    if !cleaned_up {
        let side = ReadableSide::of(scope, src)?;
        let pipes = side.pipes(scope);
        if pipes.len() == 1 && js::comparison::strictly_equal(scope, pipes[0], dest_value)? {
            side.set_await_drain_writers(DrainWriters::Value(dest_value));
            side.set(R::MULTI_AWAIT_DRAIN, false);
        } else if pipes.len() > 1 && position(scope, &pipes, dest_value)?.is_some() {
            if let DrainWriters::Set(mut writers) = side.await_drain_writers(scope) {
                if position(scope, &writers, dest_value)?.is_none() {
                    writers.push(dest_value);
                    side.set_await_drain_writers(DrainWriters::Set(writers));
                }
            }
        }
        Function::call_by_name(scope, src.handle(), c"pause", js::function::EmptyArgs)?;
    }
    if state.data().ondrain.is_none() {
        let ondrain = Function::new_callback(scope, c"", 0, pipe_ondrain, state)?;
        state.data_mut().ondrain = Some(Heap::from(*ondrain));
        listen(scope, dest, c"on", c"drain", *ondrain)?;
    }
    Ok(())
}

/// Node's `pipeOnDrain`.
fn pipe_ondrain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    let (src, dest) = {
        let data = state.data();
        (data.src.get(scope), data.dest.get(scope))
    };
    let dest = dest.to_jsval_throwing(scope)?;
    let side = ReadableSide::of(scope, src)?;
    match side.await_drain_writers(scope) {
        DrainWriters::Value(writer) => {
            if js::comparison::strictly_equal(scope, writer, dest)? {
                side.set_await_drain_writers(DrainWriters::Value(HandleValue::null()));
            }
        }
        DrainWriters::Set(mut writers) => {
            if side.has(R::MULTI_AWAIT_DRAIN) {
                if let Some(index) = position(scope, &writers, dest)? {
                    writers.remove(index);
                    side.set_await_drain_writers(DrainWriters::Set(writers));
                }
            }
        }
    }
    let empty = match side.await_drain_writers(scope) {
        DrainWriters::Value(writer) => !js::conversion::to_boolean(writer),
        DrainWriters::Set(writers) => writers.is_empty(),
    };
    if empty && side.has(R::DATA_LISTENING) {
        Function::call_by_name(scope, src.handle(), c"resume", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

fn pipe_onend(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let dest = cast_payload::<PipeState>(scope, payload)
        .data()
        .dest
        .get(scope);
    Function::call_by_name(scope, dest.handle(), c"end", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// `src.unpipe(dest)`.
fn pipe_unpipe_now(scope: &Scope<'_>, state: PipeState<'_>) -> Result<(), ExnThrown> {
    let (src, dest) = {
        let data = state.data();
        (data.src.get(scope), data.dest.get(scope))
    };
    Function::call_by_name(
        scope,
        src.handle(),
        c"unpipe",
        &[dest.to_jsval_throwing(scope)?],
    )?;
    Ok(())
}

fn pipe_unpipe(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    pipe_unpipe_now(scope, cast_payload::<PipeState>(scope, payload))?;
    Ok(js::value::undefined())
}

fn pipe_onunpipe(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    let src = state.data().src.get(scope);
    if js::comparison::strictly_equal(scope, args.get(0), src.to_jsval_throwing(scope)?)? {
        if let Ok(info) = Object::from_value(scope, args.get(1)) {
            let unpiped = info.get_property(scope, c"hasUnpiped")?;
            if unpiped.is_boolean() && !unpiped.to_boolean() {
                info.set_property(scope, c"hasUnpiped", true)?;
                pipe_cleanup(scope, state)?;
            }
        }
    }
    Ok(js::value::undefined())
}

/// Node's `cleanup` inside `pipe`.
fn pipe_cleanup(scope: &Scope<'_>, state: PipeState<'_>) -> Result<(), ExnThrown> {
    let (src, dest, ondata, onend, unpipe, onunpipe, onerror, onclose, onfinish, ondrain) = {
        let data = state.data();
        (
            data.src.get(scope),
            data.dest.get(scope),
            data.ondata.get(scope),
            data.onend.get(scope),
            data.unpipe.get(scope),
            data.onunpipe.get(scope),
            data.onerror.get(scope),
            data.onclose.get(scope),
            data.onfinish.get(scope),
            data.ondrain.get(scope),
        )
    };
    let remove = |emitter: Object<'_>, event: &'static CStr, listener: Object<'_>| {
        listen(scope, emitter, c"removeListener", event, listener)
    };
    remove(dest, c"close", onclose)?;
    remove(dest, c"finish", onfinish)?;
    if let Some(ondrain) = ondrain {
        remove(dest, c"drain", ondrain)?;
    }
    remove(dest, c"error", onerror)?;
    remove(dest, c"unpipe", onunpipe)?;
    remove(src, c"end", onend)?;
    remove(src, c"end", unpipe)?;
    remove(src, c"data", ondata)?;
    state.data_mut().cleaned_up = true;
    let Some(ondrain) = ondrain else {
        return Ok(());
    };
    let side = ReadableSide::of(scope, src)?;
    let awaiting = match side.await_drain_writers(scope) {
        DrainWriters::Value(writer) => js::conversion::to_boolean(writer),
        DrainWriters::Set(_) => true,
    };
    if awaiting {
        let needs = match super::util::sides(scope, dest)?.writable {
            None => true,
            Some(writable) => writable.need_drain(scope)?,
        };
        if needs {
            Function::call(
                scope,
                HandleValue::undefined(),
                ondrain.to_jsval_throwing(scope)?,
                js::function::EmptyArgs,
            )?;
        }
    }
    Ok(())
}

fn pipe_onerror(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    pipe_unpipe_now(scope, state)?;
    let (dest, onerror) = {
        let data = state.data();
        (data.dest.get(scope), data.onerror.get(scope))
    };
    listen(scope, dest, c"removeListener", c"error", onerror)?;
    let error_event = js::class::get_or_init_atom(scope, c"error")?;
    if listener_count(scope, dest, error_event)? == 0.0 {
        let sides = super::util::sides(scope, dest)?;
        let error = args.get(0);
        match sides.writable.or(sides.readable) {
            Some(side) if !side.error_emitted(scope)? => {
                error_or_destroy(scope, dest, error, false)?
            }
            _ => {
                crate::events::emit(scope, dest, c"error", &[error])?;
            }
        }
    }
    Ok(js::value::undefined())
}

fn pipe_onclose(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    let (dest, onfinish) = {
        let data = state.data();
        (data.dest.get(scope), data.onfinish.get(scope))
    };
    listen(scope, dest, c"removeListener", c"finish", onfinish)?;
    pipe_unpipe_now(scope, state)?;
    Ok(js::value::undefined())
}

fn pipe_onfinish(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PipeState>(scope, payload);
    let (dest, onclose) = {
        let data = state.data();
        (data.dest.get(scope), data.onclose.get(scope))
    };
    listen(scope, dest, c"removeListener", c"close", onclose)?;
    pipe_unpipe_now(scope, state)?;
    Ok(js::value::undefined())
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
    let side = ReadableSide::of(scope, stream)?;
    let pipes = side.pipes(scope);
    if pipes.is_empty() {
        return Ok(stream);
    }
    let stream_value = stream.to_jsval_throwing(scope)?;
    let dest = dest.filter(|dest| js::conversion::to_boolean(*dest));
    let Some(dest) = dest else {
        side.get_mut().pipes.clear();
        Function::call_by_name(scope, stream.handle(), c"pause", js::function::EmptyArgs)?;
        for dest in pipes {
            if let Ok(dest) = Object::from_value(scope, dest) {
                crate::events::emit(scope, dest, c"unpipe", &[stream_value, unpipe_info(scope)?])?;
            }
        }
        return Ok(stream);
    };
    let Some(index) = position(scope, &pipes, dest)? else {
        return Ok(stream);
    };
    let remaining = {
        let mut part = side.get_mut();
        if index < part.pipes.len() {
            part.pipes.remove(index);
        }
        part.pipes.len()
    };
    if remaining == 0 {
        Function::call_by_name(scope, stream.handle(), c"pause", js::function::EmptyArgs)?;
    }
    if let Ok(dest) = Object::from_value(scope, dest) {
        crate::events::emit(scope, dest, c"unpipe", &[stream_value, unpipe_info(scope)?])?;
    }
    Ok(stream)
}

/// The state of an async iterator over a readable: Node's `createAsyncIterator` generator.
#[core_runtime::jsclass(hidden)]
pub(crate) struct IteratorState {
    stream: Heap<js::object::Object>,
    /// `undefined` while the stream hasn't finished, `null` once it ended, and the error
    /// otherwise.
    error: Heap<Value>,
    /// The promise of a step that waits for the stream to signal more data or its end.
    waiting: Option<Heap<js::promise::Promise>>,
    /// The promise of the step that a signal resumed, which a microtask takes.
    resuming: Option<Heap<js::promise::Promise>>,
    /// The microtask that runs the resumed step.
    resume_job: Option<Heap<js::function::Function>>,
    /// The cleanup of the `finished` the generator runs.
    cleanup: Option<Heap<EosStateImpl>>,
    /// The `readable` listener.
    listener: Option<Heap<js::object::Object>>,
    /// The promise of the last `next()` call.
    pending: Option<Heap<js::promise::Promise>>,
    /// The promise of the step that awaits a chunk, which [`iterator_yield`] or
    /// [`iterator_reject`] settles.
    yielding: Option<Heap<js::promise::Promise>>,
    /// The reactions to the chunk a step awaits, created on first use.
    on_chunk: Option<(Heap<js::object::Object>, Heap<js::object::Object>)>,
    #[no_trace]
    destroy_on_return: bool,
    #[no_trace]
    done: bool,
    /// Set by the first `next()` call.
    #[no_trace]
    started: bool,
}

#[core_runtime::jsmethods]
impl IteratorState {}

use super::eos::EosStateImpl;

/// A step of an async iterator that runs once a promise settles: the state and the promise the
/// step settles.
#[core_runtime::jsclass(hidden)]
pub(crate) struct IteratorStep {
    state: Heap<IteratorStateImpl>,
    promise: Heap<js::promise::Promise>,
}

#[core_runtime::jsmethods]
impl IteratorStep {}

/// A new [`IteratorStep`] that settles `promise`.
fn iterator_step_of<'s>(
    scope: &'s Scope<'_>,
    state: IteratorState<'_>,
    promise: js::Promise<'_>,
) -> Result<IteratorStep<'s>, ExnThrown> {
    js::class::create_instance_with::<IteratorStepImpl>(scope, |_| IteratorStepImpl {
        state: Heap::from(state),
        promise: Heap::from(promise),
    })
}

/// Node's `streamToAsyncIterator`: `createAsyncIterator` of `stream`, as an object with `next`,
/// `return` and `throw`. A stream without a `read` method is iterated through
/// `Readable.wrap(stream, { objectMode: true })`.
pub(crate) fn async_iterator<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let options = match options.filter(|options| !options.is_undefined()) {
        Some(options) => Some(crate::errors::validate_plain_object(
            scope, options, "options", false,
        )?),
        None => None,
    };
    let stream = if has_method(scope, stream, c"read")? {
        stream
    } else {
        let wrap_options = Object::new_plain(scope)?;
        wrap_options.set_property(scope, c"objectMode", true)?;
        let stream = stream.to_jsval_throwing(scope)?;
        let wrap_options = wrap_options.to_jsval_throwing(scope)?;
        wrap_static(scope, stream, Some(wrap_options))?
    };
    let destroy_on_return = match options {
        Some(options) => {
            !crate::support::is_false(options.get_property(scope, c"destroyOnReturn")?)
        }
        None => true,
    };
    let state =
        js::class::create_instance_with::<IteratorStateImpl>(scope, |_| IteratorStateImpl {
            stream: Heap::from(stream),
            error: Heap::default(),
            waiting: None,
            resuming: None,
            resume_job: None,
            cleanup: None,
            listener: None,
            pending: None,
            yielding: None,
            on_chunk: None,
            destroy_on_return,
            done: false,
            started: false,
        })?;
    let iterator =
        Object::new_plain_with_proto(scope, js::class::get_async_iterator_prototype(scope)?)?;
    for (name, body) in [
        (c"next", iterator_next as Callback),
        (c"return", iterator_return),
        (c"throw", iterator_throw),
    ] {
        let function = Function::new_callback(scope, name, 0, body, state)?;
        iterator.set_property(scope, name, *function)?;
    }
    iterator.set_property(scope, c"stream", stream)?;
    Ok(iterator)
}

/// The start of Node's generator body: listen for `readable`, and record the end of the stream.
fn iterator_start(scope: &Scope<'_>, state: IteratorState<'_>) -> Result<(), Thrown> {
    state.data_mut().started = true;
    let stream = state.data().stream.get(scope);
    let listener = Function::new_callback(scope, c"", 0, iterator_on_readable, state)?;
    state.data_mut().listener = Some(Heap::from(*listener));
    listen(scope, stream, c"on", c"readable", *listener)?;
    let options = EosOptions {
        writable: Some(false),
        ..EosOptions::default()
    };
    let payload = state.to_jsval_throwing(scope)?;
    let cleanup = super::eos::eos_native(
        scope,
        stream,
        options,
        EosCallback::Native(iterator_on_end, payload),
    )?;
    state.data_mut().cleanup = Some(Heap::from(cleanup));
    Ok(())
}

/// Resume the step that waits for the stream, if any, one microtask later, when Node's generator
/// resumes from awaiting the promise its callback resolves.
fn iterator_wake(scope: &Scope<'_>, state: IteratorState<'_>) -> Result<(), ExnThrown> {
    let Some(waiting) = state.data_mut().waiting.take_rooted(scope) else {
        return Ok(());
    };
    state.data_mut().resuming = Some(Heap::from(waiting));
    let job = state.data().resume_job.get(scope);
    let job = match job {
        Some(job) => job,
        None => {
            let job = Function::new_callback(scope, c"", 0, iterator_resumed, state)?;
            state.data_mut().resume_job = Some(Heap::from(job));
            job
        }
    };
    js::jobs::queue_microtask(scope, &job)
}

fn iterator_on_readable(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    iterator_wake(scope, cast_payload::<IteratorState>(scope, payload))?;
    Ok(js::value::undefined())
}

/// The `finished` callback of the iterator: record the error, or `null` for a clean end.
fn iterator_on_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<IteratorState>(scope, payload);
    let error = args.get(0);
    if js::conversion::to_boolean(error) {
        let previous = state.data().error.get(scope);
        let combined = destroy::aggregate_two_errors(scope, previous, error)?;
        state.data().error.set(combined.get());
    } else {
        state.data().error.set(js::value::null());
    }
    iterator_wake(scope, state)?;
    Ok(js::value::undefined())
}

/// The `finally` block of Node's generator: destroy the stream, or stop listening to it.
fn iterator_finish(scope: &Scope<'_>, state: IteratorState<'_>) -> Result<(), ExnThrown> {
    if state.data().done {
        return Ok(());
    }
    state.data_mut().done = true;
    let (stream, error, destroy_on_return) = {
        let data = state.data();
        (
            data.stream.get(scope),
            data.error.get(scope),
            data.destroy_on_return,
        )
    };
    let preserve_half_open_duplex = error.is_null()
        && is_true(stream.get_property(scope, c"allowHalfOpen")?)
        && is_true(stream.get_property(scope, c"writable")?)
        && !is_true(stream.get_property(scope, c"writableEnded")?);
    // `autoDestroy` is read only for an error, as Node's condition reads it, and a stream without
    // a readable state doesn't destroy itself.
    let auto_destroy = |scope: &Scope<'_>| -> Result<bool, ExnThrown> {
        Ok(super::readable_auto_destroy(scope, stream)?.unwrap_or(false))
    };
    if (js::conversion::to_boolean(error) || destroy_on_return)
        && (error.is_undefined() || auto_destroy(scope)?)
        && !preserve_half_open_duplex
    {
        destroy::destroyer(scope, stream.to_jsval_throwing(scope)?, HandleValue::null())?;
    } else {
        let (listener, cleanup) = {
            let data = state.data();
            (data.listener.get(scope), data.cleanup.get(scope))
        };
        if let Some(listener) = listener {
            listen(scope, stream, c"off", c"readable", listener)?;
        }
        if let Some(cleanup) = cleanup {
            super::eos::cleanup(scope, cleanup)?;
        }
    }
    Ok(())
}

/// One step of the generator's loop, which settles `promise`: with the next chunk one microtask
/// later (the `await` of the generator's `yield`), with the end or the error right away, or, if no
/// chunk is available yet, once the stream signals more data or its end.
fn iterator_step(
    scope: &Scope<'_>,
    state: IteratorState<'_>,
    promise: js::Promise<'_>,
) -> Result<(), ExnThrown> {
    let undefined_value = HandleValue::undefined();
    if state.data().done {
        let result = js::iteration::create_iter_result(scope, undefined_value, true)?;
        return promise.resolve(scope, result);
    }
    let stream = state.data().stream.get(scope);
    let chunk = if js::conversion::to_boolean(stream.get_property(scope, c"destroyed")?) {
        HandleValue::null()
    } else {
        Function::call_by_name(scope, stream.handle(), c"read", js::function::EmptyArgs)?
    };
    if !chunk.is_null() {
        let awaited = js::Promise::call_original_resolve(scope, chunk)?;
        let on_chunk = state
            .data()
            .on_chunk
            .as_ref()
            .map(|(fulfilled, rejected)| (fulfilled.get(scope), rejected.get(scope)));
        let (fulfilled, rejected) = match on_chunk {
            Some(reactions) => reactions,
            None => {
                let fulfilled = *Function::new_callback(scope, c"", 1, iterator_yield, state)?;
                let rejected = *Function::new_callback(scope, c"", 1, iterator_reject, state)?;
                state.data_mut().on_chunk = Some((Heap::from(fulfilled), Heap::from(rejected)));
                (fulfilled, rejected)
            }
        };
        state.data_mut().yielding = Some(Heap::from(promise));
        awaited.add_reactions(scope, Some(fulfilled), Some(rejected))?;
        return Ok(());
    }
    let error = state.data().error.get(scope);
    if js::conversion::to_boolean(error) {
        iterator_finish(scope, state)?;
        return promise.reject(scope, error);
    }
    if error.is_null() {
        iterator_finish(scope, state)?;
        let result = js::iteration::create_iter_result(scope, undefined_value, true)?;
        return promise.resolve(scope, result);
    }
    state.data_mut().waiting = Some(Heap::from(promise));
    Ok(())
}

/// Fulfill the promise of the step that awaits a chunk with `{ value, done: false }`. The payload
/// is the [`IteratorState`].
fn iterator_yield(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<IteratorState>(scope, payload);
    let promise = state.data_mut().yielding.take_rooted(scope);
    if let Some(promise) = promise {
        let result = js::iteration::create_iter_result(scope, args.get(0), false)?;
        promise.resolve(scope, result)?;
    }
    Ok(js::value::undefined())
}

/// Reject the promise of the step that awaits a chunk with the argument. The payload is the
/// [`IteratorState`].
fn iterator_reject(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<IteratorState>(scope, payload);
    let promise = state.data_mut().yielding.take_rooted(scope);
    if let Some(promise) = promise {
        promise.reject(scope, args.get(0))?;
    }
    Ok(js::value::undefined())
}

/// Run [`iterator_step`], rejecting `promise` with an exception it throws.
fn iterator_step_or_reject(
    scope: &Scope<'_>,
    state: IteratorState<'_>,
    promise: js::Promise<'_>,
) -> Result<(), ExnThrown> {
    if iterator_step(scope, state, promise).is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        promise.reject(scope, error)?;
    }
    Ok(())
}

/// The microtask [`iterator_wake`] queues: take the resumed step.
fn iterator_resumed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<IteratorState>(scope, payload);
    let resuming = state.data_mut().resuming.take_rooted(scope);
    if let Some(promise) = resuming {
        iterator_step_or_reject(scope, state, promise)?;
    }
    Ok(js::value::undefined())
}

/// Run the step after the previous `next()` settled, whose payload is an [`IteratorStep`].
fn iterator_chained_step(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let step = cast_payload::<IteratorStep>(scope, payload);
    let (state, promise) = {
        let data = step.data();
        (data.state.get(scope), data.promise.get(scope))
    };
    iterator_step_or_reject(scope, state, promise)?;
    Ok(js::value::undefined())
}

fn iterator_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<IteratorState>(scope, payload);
    let (started, done) = {
        let data = state.data();
        (data.started, data.done)
    };
    if !started && !done {
        if let Err(error) = iterator_start(scope, state) {
            error.throw(scope);
            let error = js::exception::take_pending_or_undefined(scope);
            return js::Promise::new_rejected_with_error(scope, error)?
                .to_jsval_raw_throwing(scope);
        }
    }
    let promise = js::Promise::new_pending(scope)?;
    let previous = state.data().pending.get(scope);
    match previous {
        Some(previous) if previous.is_pending() => {
            let step = iterator_step_of(scope, state, promise)?;
            let reaction = Function::new_callback(scope, c"", 0, iterator_chained_step, step)?;
            previous.then(scope, Some(*reaction), Some(*reaction))?;
        }
        _ => iterator_step_or_reject(scope, state, promise)?,
    }
    state.data_mut().pending = Some(Heap::from(promise));
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
    let state = cast_payload::<IteratorState>(scope, payload);
    if !state.data().started {
        state.data_mut().done = true;
    }
    let promise = js::Promise::new_pending(scope)?;
    let step = iterator_step_of(scope, state, promise)?;
    let fulfilled = Function::new_callback(scope, c"", 1, iterator_returned, step)?;
    let rejected = Function::new_callback(scope, c"", 1, iterator_return_rejected, step)?;
    js::Promise::call_original_resolve(scope, args.get(0))?.then(
        scope,
        Some(*fulfilled),
        Some(*rejected),
    )?;
    promise.to_jsval_raw_throwing(scope)
}

/// The fulfillment of `return(value)`'s value, whose payload is an [`IteratorStep`].
fn iterator_returned(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let step = cast_payload::<IteratorStep>(scope, payload);
    let (state, promise) = {
        let data = step.data();
        (data.state.get(scope), data.promise.get(scope))
    };
    iterator_finish(scope, state)?;
    let result = js::iteration::create_iter_result(scope, args.get(0), true)?;
    promise.resolve(scope, result)?;
    Ok(js::value::undefined())
}

/// The rejection of `return(value)`'s value, whose payload is an [`IteratorStep`].
fn iterator_return_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let step = cast_payload::<IteratorStep>(scope, payload);
    let (state, promise) = {
        let data = step.data();
        (data.state.get(scope), data.promise.get(scope))
    };
    if state.data().done {
        promise.reject(scope, args.get(0))?;
        return Ok(js::value::undefined());
    }
    let previous = state.data().error.get(scope);
    let combined = destroy::aggregate_two_errors(scope, previous, args.get(0))?;
    state.data().error.set(combined.get());
    iterator_finish(scope, state)?;
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
    let state = cast_payload::<IteratorState>(scope, payload);
    if !state.data().started {
        state.data_mut().done = true;
        return js::Promise::new_rejected_with_error(scope, args.get(0))?
            .to_jsval_raw_throwing(scope);
    }
    let previous = state.data().error.get(scope);
    let combined = destroy::aggregate_two_errors(scope, previous, args.get(0))?;
    state.data().error.set(combined.get());
    iterator_finish(scope, state)?;
    js::Promise::new_rejected_with_error(scope, combined)?.to_jsval_raw_throwing(scope)
}

/// The state of `readable.wrap(stream)`: the readable, the wrapped old-style stream, and whether
/// the wrap paused it.
#[core_runtime::jsclass(hidden)]
pub(crate) struct WrapState {
    readable: Heap<js::object::Object>,
    wrapped: Heap<js::object::Object>,
    #[no_trace]
    paused: bool,
}

#[core_runtime::jsmethods]
impl WrapState {}

fn wrap_ondata(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WrapState>(scope, payload);
    let (readable, wrapped) = {
        let data = state.data();
        (data.readable.get(scope), data.wrapped.get(scope))
    };
    let pushed = Function::call_by_name(scope, readable.handle(), c"push", &[args.get(0)])?;
    if !js::conversion::to_boolean(pushed) && has_method(scope, wrapped, c"pause")? {
        state.data_mut().paused = true;
        Function::call_by_name(scope, wrapped.handle(), c"pause", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

fn wrap_onend(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let readable = cast_payload::<WrapState>(scope, payload)
        .data()
        .readable
        .get(scope);
    Function::call_by_name(scope, readable.handle(), c"push", &[HandleValue::null()])?;
    Ok(js::value::undefined())
}

fn wrap_onerror(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let readable = cast_payload::<WrapState>(scope, payload)
        .data()
        .readable
        .get(scope);
    error_or_destroy(scope, readable, args.get(0), false)?;
    Ok(js::value::undefined())
}

fn wrap_onclose(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let readable = cast_payload::<WrapState>(scope, payload)
        .data()
        .readable
        .get(scope);
    Function::call_by_name(
        scope,
        readable.handle(),
        c"destroy",
        js::function::EmptyArgs,
    )?;
    Ok(js::value::undefined())
}

fn wrap_read(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WrapState>(scope, payload);
    let wrapped = state.data().wrapped.get(scope);
    if state.data().paused && has_method(scope, wrapped, c"resume")? {
        state.data_mut().paused = false;
        Function::call_by_name(scope, wrapped.handle(), c"resume", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

/// `readable.wrap(stream)`: read from an old-style stream.
pub(crate) fn wrap<'s>(
    scope: &Scope<'_>,
    readable: Object<'s>,
    wrapped: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let wrapped_object = Object::from_value(scope, wrapped)
        .map_err(|_| js::error::TypeError("stream must be an object".into()).throw(scope))?;
    let state = js::class::create_instance_with::<WrapStateImpl>(scope, |_| WrapStateImpl {
        readable: Heap::from(readable),
        wrapped: Heap::from(wrapped_object),
        paused: false,
    })?;
    for (event, body) in [
        (c"data", wrap_ondata as Callback),
        (c"end", wrap_onend),
        (c"error", wrap_onerror),
        (c"close", wrap_onclose),
        (c"destroy", wrap_onclose),
    ] {
        let listener = Function::new_callback(scope, c"", 1, body, state)?;
        listen(scope, wrapped_object, c"on", event, *listener)?;
    }
    let read = Function::new_callback(scope, c"_read", 0, wrap_read, state)?;
    readable.set_property(scope, c"_read", *read)?;
    for key in wrapped_object.own_enumerable_keys(scope)? {
        let Some(name) = rust_string(scope, key) else {
            continue;
        };
        let Ok(name) = std::ffi::CString::new(name) else {
            continue;
        };
        let existing = readable.get_property_by_name(scope, &name)?;
        let method = wrapped_object.get_property_by_name(scope, &name)?;
        if existing.is_undefined() && js::function::is_callable(method) {
            let method_object = Object::from_value(scope, method).expect("functions are objects");
            let bound = Function::call_by_name(scope, method_object.handle(), c"bind", &[wrapped])?;
            readable.set_property_by_name(scope, &name, bound)?;
        }
    }
    Ok(readable)
}

/// The `destroy` option of a `Readable.wrap` stream, whose payload is the wrapped stream: destroy
/// it with the error, and call the callback with it.
fn wrap_destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    destroy::destroyer(scope, payload, args.get(0))?;
    Function::call(scope, HandleValue::undefined(), args.get(1), &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// The static `Readable.wrap(src, options)`.
fn wrap_static<'s>(
    scope: &'s Scope<'_>,
    src: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let src_object = Object::from_value_coerce(scope, src)?;
    let mut object_mode = src_object.get_property(scope, c"readableObjectMode")?;
    if object_mode.is_null_or_undefined() {
        object_mode = src_object.get_property(scope, c"objectMode")?;
    }
    if object_mode.is_null_or_undefined() {
        object_mode = scope.root_value(js::value::from_bool(true));
    }
    let merged = Object::new_plain(scope)?;
    merged.set_property(scope, c"objectMode", object_mode)?;
    if let Some(options) = options.and_then(|options| Object::from_value(scope, options).ok()) {
        merged.assign(scope, options)?;
    }
    let destroy = Function::new_callback(scope, c"destroy", 2, wrap_destroy, src)?;
    merged.set_property(scope, c"destroy", *destroy)?;
    let readable = Function::construct(scope, readable::class_constructor(scope)?, &[merged])?;
    wrap(scope, readable, src)
}

#[core_runtime::jsfunction_class(
    name = "Readable",
    extends = "super::super::legacy::stream",
    state = "super::super::state::StreamState",
    invalid_this = "crate::errors::invalid_this",
    self_construct
)]
pub mod readable {
    use super::*;

    /// <https://nodejs.org/api/stream.html#new-streamreadableoptions>
    #[constructor(length = 1)]
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
    #[method(length = 2)]
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
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        Ok(())
    }

    /// <https://nodejs.org/api/stream.html#readablepushchunk-encoding>
    #[method(length = 2)]
    pub fn push(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<bool, ExnThrown> {
        readable_add_chunk(scope, this, chunk, encoding, false)
    }

    /// <https://nodejs.org/api/stream.html#readableunshiftchunk-encoding>
    #[method(length = 2)]
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
        let side = ReadableSide::of(scope, this)?;
        let part = side.get();
        Ok(part.paused == Some(true) || part.flowing == Some(false))
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
    #[method(length = 1)]
    pub fn read<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        size: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let size = size.unwrap_or(HandleValue::undefined());
        super::read(scope, this, size)
    }

    /// <https://nodejs.org/api/stream.html#readable_readsize>: subclasses implement it.
    #[method(name = "_read", length = 1)]
    pub fn read_impl() -> Result<(), NodeError> {
        Err(NodeError::not_implemented("_read()"))
    }

    /// <https://nodejs.org/api/stream.html#readablepipedestination-options>
    #[method(length = 2)]
    pub fn pipe<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        destination: HandleValue<'s>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        super::pipe(scope, this, destination, options)
    }

    /// <https://nodejs.org/api/stream.html#readableunpipedestination>
    #[method(length = 1)]
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
    #[method(length = 1)]
    pub fn remove_all_listeners<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        event: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        crate::events::event_emitter::remove_all_listeners(scope, this, event)?;
        let event = event.unwrap_or(HandleValue::undefined());
        if event.is_undefined() || is_str(scope, event, c"readable")? {
            super::tick(scope, update_readable_listening, this)?;
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
    #[method(length = 1)]
    pub fn iterator<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::async_iterator(scope, this, options)
    }

    /// <https://nodejs.org/api/stream.html#streamreadablefromiterable-options>
    #[static_method(length = 2)]
    pub fn from<'s>(
        scope: &'s Scope<'_>,
        iterable: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::from::from(scope, iterable, options)
    }

    /// <https://nodejs.org/api/stream.html#streamreadablefromwebreadablestream-options>
    #[static_method(length = 2)]
    pub fn from_web<'s>(
        scope: &'s Scope<'_>,
        readable_stream: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::adapters::readable_from_web(scope, readable_stream, options)
    }

    /// <https://nodejs.org/api/stream.html#streamreadabletowebstreamreadable-options>
    #[static_method(length = 2)]
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
    #[static_method(name = "wrap", length = 2)]
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

/// Create the readable part of the state of `stream` and its `_readableState` view, and take the
/// readable options: Node's `new ReadableState()` and the option handling of the `Readable` and
/// `Duplex` constructors.
pub(crate) fn init_readable(
    scope: &Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
    is_duplex: bool,
) -> Result<(), Thrown> {
    init_readable_with(scope, stream, options, is_duplex, None)
}

/// The own property of a readable stream that [`init_new_readable`] creates before those of the
/// emitter.
pub(crate) static READABLE_PROPERTIES: PropertyTemplate =
    PropertyTemplate::new(&[TemplateKey::Name(c"_readableState")]);

/// Initialize `stream`, which this crate created and `properties` writes to, as the `Readable`
/// constructor does for `options`, or for `{ highWaterMark: high_water_mark }` if
/// `high_water_mark` is a valid high water mark. It has no `_events` yet, so its emitter is
/// initialized with [`crate::events::init_new_emitter`].
pub(crate) fn init_new_readable<'w>(
    scope: &'w Scope<'_>,
    stream: Object<'w>,
    properties: &mut PropertyWriter<'w>,
    options: Option<HandleValue<'_>>,
    high_water_mark: Option<f64>,
) -> Result<(), Thrown> {
    match high_water_mark {
        Some(mark) => {
            init_readable_with(scope, stream, None, false, Some(properties))?;
            ReadableSide::of(scope, stream)?.get_mut().high_water_mark = mark;
        }
        None => init_readable_with(scope, stream, options, false, Some(properties))?,
    }
    crate::events::init_new_emitter(scope, stream, properties)?;
    if properties.get(scope, c"_construct")?.is_null_or_undefined() {
        return Ok(());
    }
    properties.flush(scope)?;
    finish_construction(scope, stream)
}

/// Run [`init_readable`], creating `_readableState` with `properties`, or by assignment without
/// it.
fn init_readable_with<'w>(
    scope: &'w Scope<'_>,
    stream: Object<'w>,
    options: Option<HandleValue<'_>>,
    is_duplex: bool,
    mut properties: Option<&mut PropertyWriter<'w>>,
) -> Result<(), Thrown> {
    let state = readable::state_of(scope, stream)?;
    init_readable_part(scope, state, options, is_duplex)?;
    let view = super::state::new_readable_view(scope, state)?;
    match properties.as_deref_mut() {
        Some(properties) => {
            properties.continue_with(scope, &READABLE_PROPERTIES)?;
            properties.write(scope, c"_readableState", view)?;
        }
        None => stream.set_property(scope, c"_readableState", view)?,
    }
    let options = options.and_then(|options| Object::from_value(scope, options).ok());
    // The `Duplex` constructor assigns the methods of both sides itself.
    if let Some(options) = options.filter(|_| !is_duplex) {
        for (option, method) in [
            (c"read", c"_read"),
            (c"destroy", c"_destroy"),
            (c"construct", c"_construct"),
        ] {
            let value = options.get_property(scope, option)?;
            if js::function::is_callable(value) {
                if let Some(properties) = properties.as_deref_mut() {
                    properties.flush(scope)?;
                }
                stream.set_property(scope, method, value)?;
            }
        }
        let signal = options.get_property(scope, c"signal")?;
        if js::conversion::to_boolean(signal) {
            if let Some(properties) = properties {
                properties.flush(scope)?;
            }
            super::add_abort_signal(scope, signal, stream.to_jsval_throwing(scope)?)?;
        }
    }
    Ok(())
}

/// Run the stream's `_construct`, if it has one, with a callback that calls [`on_constructed`]: the
/// end of the `Readable`, `Writable` and `Duplex` constructors.
pub(crate) fn finish_construction(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), Thrown> {
    let construct = stream.get_property(scope, c"_construct")?;
    if construct.is_null_or_undefined() {
        return Ok(());
    }
    let on_constructed = Function::new_callback(scope, c"", 0, on_constructed, stream)?;
    destroy::construct(scope, stream, *on_constructed)?;
    Ok(())
}

/// The `kOnConstructed` of both states, whose payload is the stream: start reading ahead if the
/// readable side needs data, and write what the writable side buffered.
fn on_constructed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let Some(state) = super::state::try_state(scope, stream)? else {
        return Ok(js::value::undefined());
    };
    if let Some(side) = ReadableSide::of_state(state) {
        if side.has(R::NEED_READABLE) {
            maybe_read_more(scope, stream, side)?;
        }
    }
    if let Some(side) = WritableSide::of_state(state) {
        super::writable::on_constructed(scope, stream, side)?;
    }
    Ok(js::value::undefined())
}

/// Define a getter, and a setter if given, on `proto`. Each callback has one function object per
/// global, so defining the same callback on two prototypes defines the same function.
pub(crate) fn accessor(
    scope: &Scope<'_>,
    proto: Object<'_>,
    name: &'static CStr,
    getter: Callback,
    setter: Option<Callback>,
) -> Result<(), ExnThrown> {
    let shared = |body: Callback, nargs: u32| {
        crate::support::shared_named_function(scope, body, name, nargs)
    };
    let get = shared(getter, 0)?;
    let set = setter.map(|setter| shared(setter, 1)).transpose()?;
    proto.define_accessor_by_name(scope, name, Some(get), set, 0)
}

/// The readable part of the state of an accessor's `this`, if it has one.
fn this_side<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
) -> Result<Option<ReadableSide<'s>>, ExnThrown> {
    match Object::from_value(scope, args.this()) {
        Ok(this) => ReadableSide::try_of(scope, this),
        Err(_) => Ok(None),
    }
}

/// A getter of `Readable.prototype`: `body` of the readable part of `this`'s state, or `default`
/// without one.
fn readable_getter(
    scope: &Scope<'_>,
    args: &CallbackArgs<'_>,
    default: impl FnOnce() -> Value,
    body: impl FnOnce(&ReadablePart) -> Value,
) -> Result<Value, ExnThrown> {
    Ok(match this_side(scope, args)? {
        Some(side) => body(&side.get()),
        None => default(),
    })
}

fn get_readable_high_water_mark(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    readable_getter(scope, &args, js::value::undefined, |part| {
        js::value::from_f64(part.high_water_mark)
    })
}

fn get_readable_buffer(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(match this_side(scope, &args)? {
        Some(side) => side.buffer_array(scope)?.as_value(),
        None => js::value::undefined(),
    })
}

fn get_readable_flowing(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    readable_getter(scope, &args, js::value::undefined, |part| {
        match part.flowing {
            Some(flowing) => js::value::from_bool(flowing),
            None => js::value::null(),
        }
    })
}

fn get_readable_length(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    readable_getter(scope, &args, js::value::undefined, |part| {
        js::value::from_f64(part.length)
    })
}

fn get_readable_object_mode(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    readable_getter(
        scope,
        &args,
        || js::value::from_bool(false),
        |part| js::value::from_bool(part.flags.contains(R::OBJECT_MODE)),
    )
}

fn get_readable_encoding(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(match this_side(scope, &args)? {
        Some(side) => side.encoding(scope).map_or(js::value::null(), |e| e.get()),
        None => js::value::null(),
    })
}

fn get_errored(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(match this_side(scope, &args)? {
        Some(side) => side.errored(scope).map_or(js::value::null(), |e| e.get()),
        None => js::value::null(),
    })
}

fn get_closed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    readable_getter(
        scope,
        &args,
        || js::value::from_bool(false),
        |part| js::value::from_bool(part.flags.contains(R::CLOSED)),
    )
}

fn get_destroyed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    readable_getter(
        scope,
        &args,
        || js::value::from_bool(false),
        |part| js::value::from_bool(part.flags.contains(R::DESTROYED)),
    )
}

fn get_readable_ended(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    readable_getter(
        scope,
        &args,
        || js::value::from_bool(false),
        |part| js::value::from_bool(part.flags.contains(R::END_EMITTED)),
    )
}

fn get_readable_did_read(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    readable_getter(
        scope,
        &args,
        || js::value::from_bool(false),
        |part| js::value::from_bool(part.flags.contains(R::DATA_EMITTED)),
    )
}

fn get_readable(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(side) = this_side(scope, &args)? else {
        return Ok(js::value::from_bool(false));
    };
    let readable = side.get().is_readable();
    Ok(js::value::from_bool(readable))
}

fn set_readable(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(side) = this_side(scope, &args)? {
        let readable = js::conversion::to_boolean(args.get(0));
        side.get().readable.set(js::value::from_bool(readable));
    }
    Ok(js::value::undefined())
}

fn set_readable_flowing(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(side) = this_side(scope, &args)? {
        let value = args.get(0);
        side.get_mut().flowing =
            (!value.is_null_or_undefined()).then(|| js::conversion::to_boolean(value));
    }
    Ok(js::value::undefined())
}

fn set_destroyed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(side) = this_side(scope, &args)? {
        side.set(R::DESTROYED, js::conversion::to_boolean(args.get(0)));
    }
    Ok(js::value::undefined())
}

fn get_readable_aborted(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(side) = this_side(scope, &args)? else {
        return Ok(js::value::from_bool(false));
    };
    let failed = side.has(R::DESTROYED) || side.is_errored();
    Ok(js::value::from_bool(
        !side.disabled() && failed && !side.has(R::END_EMITTED),
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
    Function::call_by_name(scope, this.handle(), c"destroy", &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// The state of a `Symbol.asyncDispose` call: the promise it returned, and the error it
/// destroyed the stream with.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DisposeState {
    promise: Heap<js::promise::Promise>,
    error: Heap<Value>,
    #[no_trace]
    writable: bool,
}

#[core_runtime::jsmethods]
impl DisposeState {}

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
        Err(_) => js::Promise::new_rejected_with_error(
            scope,
            js::exception::take_pending_or_undefined(scope),
        )?,
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
    if !js::conversion::to_boolean(this.get_property(scope, c"destroyed")?) {
        let done = if writable {
            c"writableFinished"
        } else {
            c"readableEnded"
        };
        error = if js::conversion::to_boolean(this.get_property(scope, done)?) {
            HandleValue::null()
        } else {
            crate::errors::abort_error(scope, None)?
        };
        Function::call_by_name(scope, this.handle(), c"destroy", &[error])?;
    }
    let closed = js::Promise::new_pending(scope)?;
    let state = js::class::create_instance_with::<DisposeStateImpl>(scope, |_| DisposeStateImpl {
        promise: Heap::from(closed),
        error: Heap::from(error.get()),
        writable,
    })?;
    let payload = state.to_jsval_throwing(scope)?;
    super::eos::eos_native(
        scope,
        this,
        EosOptions::default(),
        EosCallback::Native(async_dispose_settle, payload),
    )
    .map_err(|error| error.throw(scope))?;
    closed.then(scope, Some(crate::support::nop(scope)?), None)
}

/// The `finished` callback of [`async_dispose`], whose payload is a [`DisposeState`].
fn async_dispose_settle(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<DisposeState>(scope, payload);
    let (promise, expected_error, writable) = {
        let data = state.data();
        (
            data.promise.get(scope),
            data.error.get(scope),
            data.writable,
        )
    };
    let error = args.get(0);
    let expected = if writable {
        let name = match Object::from_value(scope, error).ok() {
            Some(error) => rust_string(scope, error.get_property(scope, c"name")?),
            None => None,
        };
        name.as_deref() == Some("AbortError")
    } else {
        js::comparison::strictly_equal(scope, error, expected_error)?
    };
    if js::conversion::to_boolean(error) && !expected {
        promise.reject(scope, error)?;
    } else {
        promise.resolve(scope, HandleValue::null())?;
    }
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
    let capture = crate::events::REJECTION.value(scope)?;
    define_symbol_method(scope, proto, capture, c"", capture_rejection)?;
    define_async_dispose(scope, proto, false)?;
    super::operators::define_operators(scope, proto)?;
    ctor.set_property(
        scope,
        c"ReadableState",
        super::state::ReadableState::class_constructor(scope)?,
    )?;
    Ok(())
}
