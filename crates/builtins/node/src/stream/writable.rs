// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Writable`: Node's `lib/internal/streams/writable.js`.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{Callback, CallbackArgs};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::destroy::{
    self, error_or_destroy, error_or_destroy_with, multiple_callback, stream_destroyed,
};
use super::readable::{
    accessor, array_push, buffer_from, chunk_length, invalid_chunk, is_array_buffer_view,
    is_buffer, is_encoding, unknown_encoding, view_to_buffer,
};
use super::util::{
    as_object, call, call_method, closure, emit, error_value, is_function, is_str, next_tick,
    next_tick_call, required_obj, rust_string, take_exception, this_object, truthy, type_error,
    Captures, Props,
};
use crate::errors::{ErrorKind, NodeError, Thrown};

/// `stream._writableState`.
pub(crate) fn writable_state<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
) -> Result<Object<'s>, ExnThrown> {
    stream
        .obj(scope, c"_writableState")?
        .ok_or_else(|| js::error::TypeError("not a Writable".into()).throw(scope))
}

/// Whether the state holds buffered writes: Node's `kBuffered`.
fn has_buffered(scope: &Scope<'_>, state: Object<'_>) -> Result<bool, ExnThrown> {
    let buffered = required_obj(scope, state, c"buffered")?;
    Ok(buffered.num(scope, c"length")? > 0.0)
}

/// Whether the stream is corked: Node's `kCorked`.
fn is_corked(scope: &Scope<'_>, state: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(state.num(scope, c"corked")? > 0.0)
}

/// Node's `resetBuffer`.
fn reset_buffer(scope: &Scope<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    state.set_property(
        scope,
        c"buffered",
        *js::Array::with_contents(scope, &[] as &[Value])?,
    )?;
    state.set_property(scope, c"bufferedIndex", 0)?;
    state.set_flag(scope, c"allBuffers", true)?;
    state.set_flag(scope, c"allNoop", true)
}

#[core_runtime::jsfunction_class(name = "WritableState")]
pub mod writable_state {
    use super::*;

    /// The state of a `Writable`, or of a `Duplex`'s writable side if `is_duplex`.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
        stream: Option<HandleValue<'_>>,
        is_duplex: Option<bool>,
    ) -> Result<(), Thrown> {
        let options = options.and_then(|options| as_object(scope, options));
        let is_duplex = is_duplex.unwrap_or(false);
        let option = |name: &'static std::ffi::CStr| -> Result<HandleValue<'_>, ExnThrown> {
            match options {
                Some(options) => options.val(scope, name),
                None => Ok(HandleValue::undefined()),
            }
        };
        let object_mode = truthy(scope, option(c"objectMode")?)
            || (is_duplex && truthy(scope, option(c"writableObjectMode")?));
        this.set_flag(scope, c"objectMode", object_mode)?;
        let high_water_mark = match options {
            Some(options) => super::super::state::high_water_mark(
                scope,
                options,
                c"writableHighWaterMark",
                is_duplex,
                object_mode,
            )?,
            None => super::super::state::default_high_water_mark(false),
        };
        this.set_property(scope, c"highWaterMark", high_water_mark)?;
        for name in [
            c"finalCalled",
            c"needDrain",
            c"ending",
            c"ended",
            c"finished",
            c"destroyed",
            c"writing",
            c"bufferProcessing",
            c"prefinished",
            c"errorEmitted",
            c"closed",
            c"closeEmitted",
            c"afterWritePending",
            c"_expectWriteCb",
        ] {
            this.set_flag(scope, name, false)?;
        }
        this.set_flag(scope, c"sync", true)?;
        this.set_flag(scope, c"constructed", true)?;
        let decode = option(c"decodeStrings")?;
        this.set_flag(
            scope,
            c"decodeStrings",
            !(decode.is_boolean() && !decode.to_boolean()),
        )?;
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
        this.set_property(scope, c"errored", js::value::null())?;
        this.set_property(scope, c"length", 0)?;
        this.set_property(scope, c"corked", 0)?;
        let stream = stream.unwrap_or(HandleValue::undefined());
        let onwrite = closure(scope, 1, super::onwrite, &[stream.get()])?;
        this.set_property(scope, c"onwrite", onwrite)?;
        this.set_property(scope, c"writecb", js::value::null())?;
        this.set_property(scope, c"writelen", 0)?;
        this.set_property(scope, c"afterWriteTickInfo", js::value::null())?;
        this.set_property(scope, c"_onFinished", js::value::null())?;
        reset_buffer(scope, this)?;
        this.set_property(scope, c"pendingcb", 0)?;
        Ok(())
    }

    /// <https://nodejs.org/api/stream.html#writablewritablebuffer>: the buffered writes.
    pub fn get_buffer<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let buffered = required_obj(scope, this, c"buffered")?;
        let index = this.val(scope, c"bufferedIndex")?;
        call_method(scope, buffered, c"slice", &[index])
    }

    /// Define `bufferedRequestCount`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        accessor(
            scope,
            proto,
            c"bufferedRequestCount",
            super::state_buffered_request_count,
            None,
        )
    }
}

/// `state.bufferedRequestCount`.
fn state_buffered_request_count(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = this_object(scope, &args)?;
    let buffered = required_obj(scope, state, c"buffered")?;
    Ok(js::value::from_f64(
        buffered.num(scope, c"length")? - state.num(scope, c"bufferedIndex")?,
    ))
}

/// The writable side's `kOnConstructed`: start writing what was buffered, and finish if ending.
pub(crate) fn on_constructed(
    scope: &Scope<'_>,
    stream: Object<'_>,
    state: Object<'_>,
) -> Result<(), ExnThrown> {
    if !state.flag(scope, c"writing")? {
        clear_buffer(scope, stream, state)?;
    }
    if state.flag(scope, c"ending")? {
        finish_maybe(scope, stream, state, false)?;
    }
    Ok(())
}

/// Node's `_write`: check and convert `chunk`, then write or buffer it. Returns whether the stream
/// can take more, or the error the write failed with.
fn write_chunk<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    chunk: HandleValue<'_>,
    encoding: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<Result<bool, HandleValue<'s>>, Thrown> {
    let state = writable_state(scope, stream)?;
    let callback = if is_function(scope, callback) {
        scope.root_value(callback.get())
    } else {
        HandleValue::undefined()
    };
    if chunk.is_null() {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_STREAM_NULL_VALUES",
            "May not write null values to stream",
        )
        .into());
    }
    let mut chunk = scope.root_value(chunk.get());
    let mut encoding = scope.root_value(encoding.get());
    if !state.flag(scope, c"objectMode")? {
        if !truthy(scope, encoding) {
            encoding = state.val(scope, c"defaultEncoding")?;
        } else if !is_str(scope, encoding, c"buffer")? && !is_encoding(scope, encoding)? {
            return Err(unknown_encoding(scope, encoding).into());
        }
        if chunk.is_string() {
            if is_str(scope, encoding, c"buffer")? {
                return Err(unknown_encoding(scope, encoding).into());
            }
            if state.flag(scope, c"decodeStrings")? {
                chunk = buffer_from(scope, &[chunk, encoding])?;
                encoding = super::util::string(scope, "buffer")?;
            }
        } else if is_buffer(scope, chunk)? {
            encoding = super::util::string(scope, "buffer")?;
        } else if is_array_buffer_view(scope, chunk) {
            chunk = view_to_buffer(scope, chunk)?;
            encoding = super::util::string(scope, "buffer")?;
        } else {
            return Err(invalid_chunk(scope, chunk).into());
        }
    }
    let error = if state.flag(scope, c"ending")? {
        Some(NodeError::new(
            ErrorKind::Error,
            "ERR_STREAM_WRITE_AFTER_END",
            "write after end",
        ))
    } else if state.flag(scope, c"destroyed")? {
        Some(stream_destroyed("write"))
    } else {
        None
    };
    if let Some(error) = error {
        let error = error_value(scope, error)?;
        next_tick_call(scope, callback, &[error])?;
        error_or_destroy(scope, stream, error, true)?;
        return Ok(Err(error));
    }
    let pending = state.num(scope, c"pendingcb")? + 1.0;
    state.set_property(scope, c"pendingcb", pending)?;
    Ok(Ok(write_or_buffer(
        scope, stream, state, chunk, encoding, callback,
    )?))
}

/// Node's `writeOrBuffer`.
fn write_or_buffer(
    scope: &Scope<'_>,
    stream: Object<'_>,
    state: Object<'_>,
    chunk: HandleValue<'_>,
    encoding: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<bool, ExnThrown> {
    let len = if state.flag(scope, c"objectMode")? {
        1.0
    } else {
        chunk_length(scope, chunk)?
    };
    let length = state.num(scope, c"length")? + len;
    state.set_property(scope, c"length", length)?;
    let errored = truthy(scope, state.get_property(scope, c"errored")?);
    if state.flag(scope, c"writing")?
        || errored
        || is_corked(scope, state)?
        || !state.flag(scope, c"constructed")?
    {
        let entry = Object::new_plain(scope)?;
        entry.set_property(scope, c"chunk", chunk)?;
        entry.set_property(scope, c"encoding", encoding)?;
        entry.set_property(scope, c"callback", callback)?;
        let buffered = required_obj(scope, state, c"buffered")?;
        array_push(scope, buffered, entry.to_jsval_throwing(scope)?)?;
        if !is_str(scope, encoding, c"buffer")? {
            state.set_flag(scope, c"allBuffers", false)?;
        }
        if !callback.is_undefined() {
            state.set_flag(scope, c"allNoop", false)?;
        }
    } else {
        state.set_property(scope, c"writelen", len)?;
        if !callback.is_undefined() {
            state.set_property(scope, c"writecb", callback)?;
        }
        state.set_flag(scope, c"writing", true)?;
        state.set_flag(scope, c"sync", true)?;
        state.set_flag(scope, c"_expectWriteCb", true)?;
        let onwrite = state.val(scope, c"onwrite")?;
        call_method(scope, stream, c"_write", &[chunk, encoding, onwrite])?;
        state.set_flag(scope, c"sync", false)?;
    }
    let length = state.num(scope, c"length")?;
    let ret = length < state.num(scope, c"highWaterMark")? || length == 0.0;
    if !ret {
        state.set_flag(scope, c"needDrain", true)?;
    }
    let errored = truthy(scope, state.get_property(scope, c"errored")?);
    Ok(ret && !state.flag(scope, c"destroyed")? && !errored)
}

/// Node's `doWrite`.
fn do_write(
    scope: &Scope<'_>,
    stream: Object<'_>,
    state: Object<'_>,
    writev: bool,
    len: f64,
    chunk: HandleValue<'_>,
    encoding: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    state.set_property(scope, c"writelen", len)?;
    if !callback.is_undefined() {
        state.set_property(scope, c"writecb", callback)?;
    }
    state.set_flag(scope, c"writing", true)?;
    state.set_flag(scope, c"sync", true)?;
    state.set_flag(scope, c"_expectWriteCb", true)?;
    let onwrite = state.val(scope, c"onwrite")?;
    if state.flag(scope, c"destroyed")? {
        let error = error_value(scope, stream_destroyed("write"))?;
        call(scope, HandleValue::undefined(), onwrite, &[error])?;
    } else if writev {
        call_method(scope, stream, c"_writev", &[chunk, onwrite])?;
    } else {
        call_method(scope, stream, c"_write", &[chunk, encoding, onwrite])?;
    }
    state.set_flag(scope, c"sync", false)
}

/// Node's `onwriteError`.
fn onwrite_error(
    scope: &Scope<'_>,
    stream: Object<'_>,
    state: Object<'_>,
    error: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let pending = state.num(scope, c"pendingcb")? - 1.0;
    state.set_property(scope, c"pendingcb", pending)?;
    if is_function(scope, callback) {
        call(scope, HandleValue::undefined(), callback, &[error])?;
    }
    error_buffer(scope, state)?;
    error_or_destroy(scope, stream, error, false)
}

fn onwrite_error_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let stream = captures.obj(0)?;
    onwrite_error(
        scope,
        stream,
        writable_state(scope, stream)?,
        captures.get(1)?,
        captures.get(2)?,
    )?;
    Ok(js::value::undefined())
}

/// Node's `onwrite`, the callback `_write` gets, whose captures are `[stream]`.
fn onwrite(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let state = writable_state(scope, stream)?;
    if !state.flag(scope, c"_expectWriteCb")? {
        error_or_destroy_with(scope, stream, multiple_callback(), false)?;
        return Ok(js::value::undefined());
    }
    let sync = state.flag(scope, c"sync")?;
    let callback = state.val(scope, c"writecb")?;
    let callback = if is_function(scope, callback) {
        callback
    } else {
        HandleValue::undefined()
    };
    state.set_property(scope, c"writecb", js::value::null())?;
    state.set_flag(scope, c"writing", false)?;
    state.set_flag(scope, c"_expectWriteCb", false)?;
    let length = state.num(scope, c"length")? - state.num(scope, c"writelen")?;
    state.set_property(scope, c"length", length)?;
    state.set_property(scope, c"writelen", 0)?;
    let error = args.get(0);
    if truthy(scope, error) {
        if !truthy(scope, state.get_property(scope, c"errored")?) {
            state.set_property(scope, c"errored", error)?;
        }
        if let Some(readable) = stream.obj(scope, c"_readableState")? {
            if !truthy(scope, readable.get_property(scope, c"errored")?) {
                readable.set_property(scope, c"errored", error)?;
            }
        }
        if sync {
            next_tick(
                scope,
                onwrite_error_tick,
                &[
                    stream.to_jsval_raw_throwing(scope)?,
                    error.get(),
                    callback.get(),
                ],
            )?;
        } else {
            onwrite_error(scope, stream, state, error, callback)?;
        }
        return Ok(js::value::undefined());
    }
    if has_buffered(scope, state)? {
        clear_buffer(scope, stream, state)?;
    }
    if !sync {
        after_write(scope, stream, state, 1.0, callback)?;
        return Ok(js::value::undefined());
    }
    let need_drain = state.flag(scope, c"needDrain")? && state.num(scope, c"length")? == 0.0;
    // Node.js computes `state[kState] & kDestroyed !== 0`, which tests the object mode bit, so this
    // tests `objectMode` instead of `destroyed`.
    let need_tick = need_drain || state.flag(scope, c"objectMode")? || !callback.is_undefined();
    let tick_info = state.obj(scope, c"afterWriteTickInfo")?;
    if callback.is_undefined() {
        if !state.flag(scope, c"afterWritePending")? && need_tick {
            next_tick(
                scope,
                after_write_tick_noop,
                &[stream.to_jsval_raw_throwing(scope)?],
            )?;
            state.set_flag(scope, c"afterWritePending", true)?;
        } else {
            let pending = state.num(scope, c"pendingcb")? - 1.0;
            state.set_property(scope, c"pendingcb", pending)?;
            if state.flag(scope, c"ending")? {
                finish_maybe(scope, stream, state, true)?;
            }
        }
    } else if let Some(info) = match tick_info {
        Some(info) if super::util::same(scope, info.val(scope, c"cb")?, callback)? => Some(info),
        _ => None,
    } {
        let count = info.num(scope, c"count")? + 1.0;
        info.set_property(scope, c"count", count)?;
    } else if need_tick {
        let info = Object::new_plain(scope)?;
        info.set_property(scope, c"count", 1)?;
        info.set_property(scope, c"cb", callback)?;
        info.set_property(scope, c"stream", stream)?;
        state.set_property(scope, c"afterWriteTickInfo", info)?;
        next_tick(
            scope,
            after_write_tick,
            &[info.to_jsval_raw_throwing(scope)?],
        )?;
        state.set_flag(scope, c"afterWritePending", true)?;
    } else {
        let pending = state.num(scope, c"pendingcb")? - 1.0;
        state.set_property(scope, c"pendingcb", pending)?;
        if state.flag(scope, c"ending")? {
            finish_maybe(scope, stream, state, true)?;
        }
    }
    Ok(js::value::undefined())
}

fn after_write_tick_noop(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    after_write(
        scope,
        stream,
        writable_state(scope, stream)?,
        1.0,
        HandleValue::undefined(),
    )?;
    Ok(js::value::undefined())
}

/// Node's `afterWriteTick`, whose captures are `[info]`.
fn after_write_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let info = Captures::of(scope, payload)?.obj(0)?;
    let stream = required_obj(scope, info, c"stream")?;
    let state = writable_state(scope, stream)?;
    state.set_property(scope, c"afterWriteTickInfo", js::value::null())?;
    after_write(
        scope,
        stream,
        state,
        info.num(scope, c"count")?,
        info.val(scope, c"cb")?,
    )?;
    Ok(js::value::undefined())
}

/// Node's `afterWrite`: emit `drain` if it's needed, and call the write callback `count` times.
fn after_write(
    scope: &Scope<'_>,
    stream: Object<'_>,
    state: Object<'_>,
    count: f64,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    state.set_flag(scope, c"afterWritePending", false)?;
    let need_drain = !state.flag(scope, c"ending")?
        && state.flag(scope, c"needDrain")?
        && !state.flag(scope, c"destroyed")?
        && state.num(scope, c"length")? == 0.0;
    if need_drain {
        state.set_flag(scope, c"needDrain", false)?;
        emit(scope, stream, "drain", &[])?;
    }
    let null = HandleValue::null();
    for _ in 0..count as u64 {
        let pending = state.num(scope, c"pendingcb")? - 1.0;
        state.set_property(scope, c"pendingcb", pending)?;
        if is_function(scope, callback) {
            call(scope, HandleValue::undefined(), callback, &[null])?;
        }
    }
    if state.flag(scope, c"destroyed")? {
        error_buffer(scope, state)?;
    }
    if state.flag(scope, c"ending")? {
        finish_maybe(scope, stream, state, true)?;
    }
    Ok(())
}

/// The error buffered writes and end callbacks fail with: the state's error, or
/// `ERR_STREAM_DESTROYED` for `method`.
fn buffer_error<'s>(
    scope: &'s Scope<'_>,
    state: Object<'_>,
    method: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    let errored = state.val(scope, c"errored")?;
    if errored.is_null() || errored.is_undefined() {
        error_value(scope, stream_destroyed(method))
    } else {
        Ok(errored)
    }
}

/// Node's `errorBuffer`: fail every buffered write and end callback.
fn error_buffer(scope: &Scope<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    if state.flag(scope, c"writing")? {
        return Ok(());
    }
    if has_buffered(scope, state)? {
        let buffered = required_obj(scope, state, c"buffered")?;
        let object_mode = state.flag(scope, c"objectMode")?;
        for index in
            state.num(scope, c"bufferedIndex")? as u32..buffered.num(scope, c"length")? as u32
        {
            let Some(entry) = as_object(scope, buffered.get_element(scope, index)?) else {
                continue;
            };
            let chunk = entry.val(scope, c"chunk")?;
            let len = if object_mode {
                1.0
            } else {
                chunk_length(scope, chunk)?
            };
            let length = state.num(scope, c"length")? - len;
            state.set_property(scope, c"length", length)?;
            let callback = entry.val(scope, c"callback")?;
            if is_function(scope, callback) {
                let error = buffer_error(scope, state, "write")?;
                call(scope, HandleValue::undefined(), callback, &[error])?;
            }
        }
    }
    let error = buffer_error(scope, state, "end")?;
    call_finished_callbacks(scope, state, error)?;
    reset_buffer(scope, state)
}

fn error_buffer_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    error_buffer(scope, Captures::of(scope, payload)?.obj(0)?)?;
    Ok(js::value::undefined())
}

/// The callback of a `_writev` of several buffered writes, whose captures are `[buffered, start]`:
/// call each write's callback.
fn writev_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let buffered = captures.obj(0)?;
    let start = captures.get(1)?.to_number() as u32;
    for index in start..buffered.num(scope, c"length")? as u32 {
        if let Some(entry) = as_object(scope, buffered.get_element(scope, index)?) {
            let callback = entry.val(scope, c"callback")?;
            if is_function(scope, callback) {
                call(scope, HandleValue::undefined(), callback, &[args.get(0)])?;
            }
        }
    }
    Ok(js::value::undefined())
}

/// Node's `clearBuffer`: write the buffered writes, all at once with `_writev` if the stream has
/// one.
fn clear_buffer(scope: &Scope<'_>, stream: Object<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    if state.flag(scope, c"destroyed")?
        || state.flag(scope, c"bufferProcessing")?
        || is_corked(scope, state)?
        || !has_buffered(scope, state)?
        || !state.flag(scope, c"constructed")?
    {
        return Ok(());
    }
    let object_mode = state.flag(scope, c"objectMode")?;
    let buffered = required_obj(scope, state, c"buffered")?;
    let start = state.num(scope, c"bufferedIndex")?;
    let count = buffered.num(scope, c"length")? - start;
    if count == 0.0 {
        return Ok(());
    }
    let start = start as u32;
    let mut index = start;
    state.set_flag(scope, c"bufferProcessing", true)?;
    let writev = stream.get_property(scope, c"_writev")?;
    if count > 1.0 && is_function(scope, writev) {
        let pending = state.num(scope, c"pendingcb")? - (count - 1.0);
        state.set_property(scope, c"pendingcb", pending)?;
        let callback = if state.flag(scope, c"allNoop")? {
            HandleValue::undefined()
        } else {
            closure(
                scope,
                1,
                writev_callback,
                &[
                    buffered.to_jsval_raw_throwing(scope)?,
                    js::value::from_f64(f64::from(index)),
                ],
            )?
            .to_jsval_throwing(scope)?
        };
        let chunks = if state.flag(scope, c"allNoop")? && index == 0 {
            buffered.to_jsval_throwing(scope)?
        } else {
            let start_value = scope.root_value(js::value::from_f64(f64::from(index)));
            call_method(scope, buffered, c"slice", &[start_value])?
        };
        if let Some(chunks) = as_object(scope, chunks) {
            chunks.set_flag(scope, c"allBuffers", state.flag(scope, c"allBuffers")?)?;
        }
        let length = state.num(scope, c"length")?;
        let empty = super::util::string(scope, "")?;
        do_write(scope, stream, state, true, length, chunks, empty, callback)?;
        reset_buffer(scope, state)?;
    } else {
        loop {
            let entry = buffered.get_element(scope, index)?;
            let entry = as_object(scope, entry).ok_or_else(|| {
                let kind = if entry.is_null() { "null" } else { "undefined" };
                type_error(
                    scope,
                    &format!(
                        "Cannot destructure property 'chunk' of 'buffered[i]' as it is {kind}."
                    ),
                )
            })?;
            buffered.set_element(scope, index, js::value::null())?;
            index += 1;
            let chunk = entry.val(scope, c"chunk")?;
            let encoding = entry.val(scope, c"encoding")?;
            let callback = entry.val(scope, c"callback")?;
            let len = if object_mode {
                1.0
            } else {
                chunk_length(scope, chunk)?
            };
            do_write(scope, stream, state, false, len, chunk, encoding, callback)?;
            if index >= buffered.num(scope, c"length")? as u32 || state.flag(scope, c"writing")? {
                break;
            }
        }
        let total = buffered.num(scope, c"length")? as u32;
        if index == total {
            reset_buffer(scope, state)?;
        } else if index > 256 {
            let zero = scope.root_value(js::value::from_f64(0.0));
            let count = scope.root_value(js::value::from_f64(f64::from(index)));
            call_method(scope, buffered, c"splice", &[zero, count])?;
            state.set_property(scope, c"bufferedIndex", 0)?;
        } else {
            state.set_property(scope, c"bufferedIndex", index)?;
        }
    }
    state.set_flag(scope, c"bufferProcessing", false)
}

/// Node's `needFinish`.
fn need_finish(scope: &Scope<'_>, state: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(state.flag(scope, c"ending")?
        && state.flag(scope, c"constructed")?
        && !state.flag(scope, c"destroyed")?
        && !state.flag(scope, c"finished")?
        && !state.flag(scope, c"writing")?
        && !state.flag(scope, c"errorEmitted")?
        && !state.flag(scope, c"closeEmitted")?
        && !truthy(scope, state.get_property(scope, c"errored")?)
        && !has_buffered(scope, state)?
        && state.num(scope, c"length")? == 0.0)
}

/// Node's `onFinish`, the callback `_final` gets, whose captures are `[stream]`.
fn on_final(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let state = writable_state(scope, stream)?;
    let error = args.get(0);
    if state.flag(scope, c"prefinished")? {
        if !error.is_null() && !error.is_undefined() {
            error_or_destroy(scope, stream, error, false)?;
        } else {
            error_or_destroy_with(scope, stream, multiple_callback(), false)?;
        }
        return Ok(js::value::undefined());
    }
    let pending = state.num(scope, c"pendingcb")? - 1.0;
    state.set_property(scope, c"pendingcb", pending)?;
    if truthy(scope, error) {
        call_finished_callbacks(scope, state, error)?;
        error_or_destroy(scope, stream, error, state.flag(scope, c"sync")?)?;
    } else if need_finish(scope, state)? {
        state.set_flag(scope, c"prefinished", true)?;
        emit(scope, stream, "prefinish", &[])?;
        let pending = state.num(scope, c"pendingcb")? + 1.0;
        state.set_property(scope, c"pendingcb", pending)?;
        next_tick(scope, finish_tick, &[stream.to_jsval_raw_throwing(scope)?])?;
    }
    Ok(js::value::undefined())
}

/// Node's `prefinish`: call `_final`, or emit `prefinish` if there is none.
fn prefinish(scope: &Scope<'_>, stream: Object<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    if state.flag(scope, c"prefinished")? || state.flag(scope, c"finalCalled")? {
        return Ok(());
    }
    let final_method = stream.get_property(scope, c"_final")?;
    if is_function(scope, final_method) && !state.flag(scope, c"destroyed")? {
        state.set_flag(scope, c"finalCalled", true)?;
        state.set_flag(scope, c"sync", true)?;
        let pending = state.num(scope, c"pendingcb")? + 1.0;
        state.set_property(scope, c"pendingcb", pending)?;
        let callback = closure(scope, 1, on_final, &[stream.to_jsval_raw_throwing(scope)?])?;
        let callback = callback.to_jsval_throwing(scope)?;
        if call_method(scope, stream, c"_final", &[callback]).is_err() {
            let error = take_exception(scope);
            call(scope, HandleValue::undefined(), callback, &[error])?;
        }
        state.set_flag(scope, c"sync", false)?;
    } else {
        state.set_flag(scope, c"finalCalled", true)?;
        state.set_flag(scope, c"prefinished", true)?;
        emit(scope, stream, "prefinish", &[])?;
    }
    Ok(())
}

/// Node's `finishMaybe`.
fn finish_maybe(
    scope: &Scope<'_>,
    stream: Object<'_>,
    state: Object<'_>,
    sync: bool,
) -> Result<(), ExnThrown> {
    if !need_finish(scope, state)? {
        return Ok(());
    }
    prefinish(scope, stream, state)?;
    if state.num(scope, c"pendingcb")? == 0.0 {
        if sync {
            let pending = state.num(scope, c"pendingcb")? + 1.0;
            state.set_property(scope, c"pendingcb", pending)?;
            next_tick(
                scope,
                finish_if_needed_tick,
                &[stream.to_jsval_raw_throwing(scope)?],
            )?;
        } else if need_finish(scope, state)? {
            let pending = state.num(scope, c"pendingcb")? + 1.0;
            state.set_property(scope, c"pendingcb", pending)?;
            finish(scope, stream, state)?;
        }
    }
    Ok(())
}

fn finish_if_needed_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let state = writable_state(scope, stream)?;
    if need_finish(scope, state)? {
        finish(scope, stream, state)?;
    } else {
        let pending = state.num(scope, c"pendingcb")? - 1.0;
        state.set_property(scope, c"pendingcb", pending)?;
    }
    Ok(js::value::undefined())
}

fn finish_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    finish(scope, stream, writable_state(scope, stream)?)?;
    Ok(js::value::undefined())
}

/// Node's `finish`: emit `finish`, and auto-destroy once the readable side, if any, ended too.
fn finish(scope: &Scope<'_>, stream: Object<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    let pending = state.num(scope, c"pendingcb")? - 1.0;
    state.set_property(scope, c"pendingcb", pending)?;
    state.set_flag(scope, c"finished", true)?;
    call_finished_callbacks(scope, state, HandleValue::null())?;
    emit(scope, stream, "finish", &[])?;
    if state.flag(scope, c"autoDestroy")? {
        let auto_destroy = match stream.obj(scope, c"_readableState")? {
            None => true,
            Some(r) => {
                let readable = r.get_property(scope, c"readable")?;
                r.flag(scope, c"autoDestroy")?
                    && (r.flag(scope, c"endEmitted")?
                        || (readable.is_boolean() && !readable.to_boolean()))
            }
        };
        if auto_destroy {
            call_method(scope, stream, c"destroy", &[])?;
        }
    }
    Ok(())
}

/// Node's `callFinishedCallbacks`: call each `end()` callback with `error`.
fn call_finished_callbacks(
    scope: &Scope<'_>,
    state: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let Some(callbacks) = state.obj(scope, c"_onFinished")? else {
        return Ok(());
    };
    state.set_property(scope, c"_onFinished", js::value::null())?;
    for index in 0..callbacks.num(scope, c"length")? as u32 {
        let callback = callbacks.get_element(scope, index)?;
        call(scope, HandleValue::undefined(), callback, &[error])?;
    }
    Ok(())
}

/// `writable.write(chunk, encoding, callback)`.
pub(crate) fn write(
    scope: &Scope<'_>,
    stream: Object<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
) -> Result<bool, Thrown> {
    let (encoding, callback) = match encoding {
        Some(encoding) if is_function(scope, encoding) => (HandleValue::null(), encoding),
        _ => (
            encoding.unwrap_or(HandleValue::undefined()),
            callback.unwrap_or(HandleValue::undefined()),
        ),
    };
    Ok(matches!(
        write_chunk(scope, stream, chunk, encoding, callback)?,
        Ok(true)
    ))
}

/// `writable.end(chunk, encoding, callback)`.
pub(crate) fn end<'s>(
    scope: &Scope<'_>,
    stream: Object<'s>,
    chunk: Option<HandleValue<'_>>,
    encoding: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let state = writable_state(scope, stream)?;
    let mut chunk = chunk.unwrap_or(HandleValue::undefined());
    let mut encoding = encoding.unwrap_or(HandleValue::undefined());
    let mut callback = callback.unwrap_or(HandleValue::undefined());
    if is_function(scope, chunk) {
        callback = chunk;
        chunk = HandleValue::null();
        encoding = HandleValue::null();
    } else if is_function(scope, encoding) {
        callback = encoding;
        encoding = HandleValue::null();
    }
    let mut error = None;
    if !chunk.is_null() && !chunk.is_undefined() {
        if let Err(failed) = write_chunk(scope, stream, chunk, encoding, HandleValue::undefined())?
        {
            error = Some(failed);
        }
    }
    if is_corked(scope, state)? {
        state.set_property(scope, c"corked", 1)?;
        call_method(scope, stream, c"uncork", &[])?;
    }
    let errored = truthy(scope, state.get_property(scope, c"errored")?);
    if error.is_some() {
    } else if !state.flag(scope, c"ending")? && !errored {
        state.set_flag(scope, c"ending", true)?;
        finish_maybe(scope, stream, state, true)?;
        state.set_flag(scope, c"ended", true)?;
    } else if state.flag(scope, c"finished")? {
        error = Some(error_value(
            scope,
            NodeError::new(
                ErrorKind::Error,
                "ERR_STREAM_ALREADY_FINISHED",
                "Cannot call end after a stream was finished",
            ),
        )?);
    } else if state.flag(scope, c"destroyed")? {
        error = Some(error_value(scope, stream_destroyed("end"))?);
    }
    if is_function(scope, callback) {
        if let Some(error) = error {
            next_tick_call(scope, callback, &[error])?;
        } else if truthy(scope, state.get_property(scope, c"errored")?) {
            let errored = state.val(scope, c"errored")?;
            next_tick_call(scope, callback, &[errored])?;
        } else if state.flag(scope, c"finished")? {
            next_tick_call(scope, callback, &[HandleValue::null()])?;
        } else {
            let callbacks = match state.obj(scope, c"_onFinished")? {
                Some(callbacks) => callbacks,
                None => {
                    let callbacks = js::Array::with_contents(scope, &[] as &[Value])?;
                    state.set_property(scope, c"_onFinished", *callbacks)?;
                    *callbacks
                }
            };
            array_push(scope, callbacks, callback)?;
        }
    }
    Ok(stream)
}

#[core_runtime::jsfunction_class(
    name = "Writable",
    extends = "super::super::legacy::stream",
    self_construct
)]
pub mod writable {
    use super::*;

    /// <https://nodejs.org/api/stream.html#new-streamwritableoptions>
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_writable(scope, this, options, false)?;
        super::super::legacy::stream::init(scope, this, options)?;
        super::super::readable::finish_construction(scope, this)
    }

    /// <https://nodejs.org/api/stream.html#writablewritechunk-encoding-callback>
    pub fn write(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        super::write(scope, this, chunk, encoding, callback)
    }

    /// <https://nodejs.org/api/stream.html#writableendchunk-encoding-callback>
    pub fn end<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        chunk: Option<HandleValue<'_>>,
        encoding: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::end(scope, this, chunk, encoding, callback)
    }

    /// <https://nodejs.org/api/stream.html#writablecork>
    pub fn cork(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = writable_state(scope, this)?;
        let corked = state.num(scope, c"corked")? + 1.0;
        state.set_property(scope, c"corked", corked)
    }

    /// <https://nodejs.org/api/stream.html#writableuncork>
    pub fn uncork(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = writable_state(scope, this)?;
        let corked = state.num(scope, c"corked")?;
        if corked > 0.0 {
            state.set_property(scope, c"corked", corked - 1.0)?;
            if !state.flag(scope, c"writing")? {
                clear_buffer(scope, this, state)?;
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/stream.html#writablesetdefaultencodingencoding>
    pub fn set_default_encoding<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        encoding: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let encoding = match rust_string(scope, encoding) {
            Some(name) => super::super::util::string(scope, &name.to_lowercase())?,
            None => scope.root_value(encoding.get()),
        };
        if !is_encoding(scope, encoding)? {
            return Err(unknown_encoding(scope, encoding).into());
        }
        writable_state(scope, this)?.set_property(scope, c"defaultEncoding", encoding)?;
        Ok(this)
    }

    #[method(name = "_write")]
    pub fn write_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let writev = this.get_property(scope, c"_writev")?;
        if !is_function(scope, writev) {
            return Err(NodeError::not_implemented("_write()").into());
        }
        let entry = Object::new_plain(scope)?;
        entry.set_property(scope, c"chunk", chunk)?;
        entry.set_property(scope, c"encoding", encoding)?;
        let list = js::Array::with_contents(scope, &[entry.to_jsval_raw_throwing(scope)?])?;
        call(
            scope,
            this.to_jsval_throwing(scope)?,
            writev,
            &[(*list).to_jsval_throwing(scope)?, callback],
        )?;
        Ok(())
    }

    /// <https://nodejs.org/api/stream.html#writabledestroyerror>
    pub fn destroy<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        error: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        super::destroy_writable(scope, this, error, callback)
    }

    #[method(name = "_undestroy")]
    pub fn undestroy(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        destroy::undestroy(scope, this)
    }

    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        call(scope, HandleValue::undefined(), callback, &[error])?;
        Ok(())
    }

    /// `Writable` can't be piped from.
    pub fn pipe(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        error_or_destroy_with(
            scope,
            this,
            NodeError::new(
                ErrorKind::Error,
                "ERR_STREAM_CANNOT_PIPE",
                "Cannot pipe, not readable",
            ),
            false,
        )
    }

    /// <https://nodejs.org/api/stream.html#streamwritablefromwebwritablestream-options>
    #[static_method]
    pub fn from_web<'s>(
        scope: &'s Scope<'_>,
        writable_stream: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::adapters::writable_from_web(scope, writable_stream, options)
    }

    /// <https://nodejs.org/api/stream.html#streamwritabletowebstreamwritable>
    #[static_method]
    pub fn to_web<'s>(
        scope: &'s Scope<'_>,
        stream_writable: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::adapters::writable_to_web(scope, stream_writable)
    }

    /// Define the accessors, `_writev`, `Symbol.hasInstance`, the rejection handler, and
    /// `WritableState`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        super::setup_writable(scope, ctor, proto)
    }
}

/// `writable.destroy(error, callback)`, which also fails the buffered writes in a microtask.
pub(crate) fn destroy_writable<'s>(
    scope: &Scope<'_>,
    stream: Object<'s>,
    error: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let state = writable_state(scope, stream)?;
    if (has_buffered(scope, state)? || state.obj(scope, c"_onFinished")?.is_some())
        && !state.flag(scope, c"destroyed")?
    {
        next_tick(
            scope,
            error_buffer_tick,
            &[state.to_jsval_raw_throwing(scope)?],
        )?;
    }
    let error = error.unwrap_or(HandleValue::undefined());
    let callback = callback.unwrap_or(HandleValue::undefined());
    destroy::destroy(scope, stream, error, callback)
}

/// Create the `WritableState` of `stream` and take the writable options.
pub(crate) fn init_writable(
    scope: &Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
    is_duplex: bool,
) -> Result<(), Thrown> {
    let state_ctor = writable_state::class_constructor(scope)?;
    let options_value = options.unwrap_or(HandleValue::undefined());
    let stream_value = stream.to_jsval_throwing(scope)?;
    let duplex_value = scope.root_value(js::value::from_bool(is_duplex));
    let state = js::Function::construct(
        scope,
        state_ctor,
        &[options_value, stream_value, duplex_value],
    )?;
    stream.set_property(scope, c"_writableState", state)?;
    if let Some(options) = options.and_then(|options| as_object(scope, options)) {
        for (option, method) in [
            (c"write", c"_write"),
            (c"writev", c"_writev"),
            (c"destroy", c"_destroy"),
            (c"final", c"_final"),
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

/// `this._writableState`, for the accessors.
fn this_state<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    match as_object(scope, scope.root_value(args.this())) {
        Some(this) => this.obj(scope, c"_writableState"),
        None => Ok(None),
    }
}

/// A getter returning `$body` evaluated on the writable state, or `$default` without one.
macro_rules! writable_getter {
    ($fn_name:ident, $default:expr, |$scope:ident, $state:ident| $body:expr) => {
        pub(crate) fn $fn_name(
            $scope: &Scope<'_>,
            args: CallbackArgs<'_>,
            _payload: HandleValue,
        ) -> Result<Value, ExnThrown> {
            match this_state($scope, &args)? {
                Some($state) => $body,
                None => Ok($default),
            }
        }
    };
}

writable_getter!(get_closed, js::value::from_bool(false), |scope, state| Ok(
    js::value::from_bool(state.flag(scope, c"closed")?)
));
writable_getter!(
    get_destroyed,
    js::value::from_bool(false),
    |scope, state| Ok(js::value::from_bool(state.flag(scope, c"destroyed")?))
);
writable_getter!(get_writable, js::value::from_bool(false), |scope, state| {
    let writable = state.get_property(scope, c"writable")?;
    let disabled = writable.is_boolean() && !writable.to_boolean();
    Ok(js::value::from_bool(
        !disabled
            && !state.flag(scope, c"ending")?
            && !state.flag(scope, c"ended")?
            && !state.flag(scope, c"destroyed")?
            && !truthy(scope, state.get_property(scope, c"errored")?),
    ))
});
writable_getter!(
    get_writable_finished,
    js::value::from_bool(false),
    |scope, state| Ok(js::value::from_bool(state.flag(scope, c"finished")?))
);
writable_getter!(
    get_writable_object_mode,
    js::value::from_bool(false),
    |scope, state| Ok(js::value::from_bool(state.flag(scope, c"objectMode")?))
);
writable_getter!(
    get_writable_buffer,
    js::value::undefined(),
    |scope, state| {
        let buffered = required_obj(scope, state, c"buffered")?;
        let index = state.val(scope, c"bufferedIndex")?;
        Ok(call_method(scope, buffered, c"slice", &[index])?.get())
    }
);
writable_getter!(
    get_writable_ended,
    js::value::from_bool(false),
    |scope, state| Ok(js::value::from_bool(state.flag(scope, c"ending")?))
);
writable_getter!(
    get_writable_need_drain,
    js::value::from_bool(false),
    |scope, state| Ok(js::value::from_bool(
        !state.flag(scope, c"destroyed")?
            && !state.flag(scope, c"ending")?
            && state.flag(scope, c"needDrain")?
    ))
);
writable_getter!(
    get_writable_high_water_mark,
    js::value::undefined(),
    |scope, state| Ok(state.get_property(scope, c"highWaterMark")?.get())
);
writable_getter!(
    get_writable_corked,
    js::value::from_f64(0.0),
    |scope, state| Ok(state.get_property(scope, c"corked")?.get())
);
writable_getter!(
    get_writable_length,
    js::value::undefined(),
    |scope, state| Ok(state.get_property(scope, c"length")?.get())
);
writable_getter!(get_errored, js::value::null(), |scope, state| Ok(state
    .get_property(scope, c"errored")?
    .get()));
writable_getter!(
    get_writable_aborted,
    js::value::from_bool(false),
    |scope, state| {
        let writable = state.get_property(scope, c"writable")?;
        let disabled = writable.is_boolean() && !writable.to_boolean();
        let failed = state.flag(scope, c"destroyed")?
            || truthy(scope, state.get_property(scope, c"errored")?);
        Ok(js::value::from_bool(
            !disabled && failed && !state.flag(scope, c"finished")?,
        ))
    }
);

fn set_destroyed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(state) = this_state(scope, &args)? {
        state.set_flag(scope, c"destroyed", truthy(scope, args.get(0)))?;
    }
    Ok(js::value::undefined())
}

pub(crate) fn set_writable(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(state) = this_state(scope, &args)? {
        state.set_flag(scope, c"writable", truthy(scope, args.get(0)))?;
    }
    Ok(js::value::undefined())
}

/// `Writable[Symbol.hasInstance](object)`: an ordinary instance, or, for `Writable` itself, any
/// object whose `_writableState` is a `WritableState`, such as a `Duplex`.
fn has_instance(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(ctor) = Object::from_value(scope, args.this())
        .ok()
        .filter(|ctor| ctor.is_callable())
    else {
        return Ok(js::value::from_bool(false));
    };
    let Some(candidate) = as_object(scope, args.get(0)) else {
        return Ok(js::value::from_bool(false));
    };
    let proto = ctor.obj(scope, c"prototype")?;
    let mut current = candidate.get_prototype(scope)?;
    while let Some(object) = current {
        if proto.is_some_and(|proto| proto.as_raw() == object.as_raw()) {
            return Ok(js::value::from_bool(true));
        }
        current = object.get_prototype(scope)?;
    }
    let writable_ctor = writable::class_constructor(scope)?;
    if ctor.as_raw() != writable_ctor.as_raw() {
        return Ok(js::value::from_bool(false));
    }
    let Some(state) = candidate.obj(scope, c"_writableState")? else {
        return Ok(js::value::from_bool(false));
    };
    let state_proto = required_obj(
        scope,
        writable_state::class_constructor(scope)?,
        c"prototype",
    )?;
    let mut current = state.get_prototype(scope)?;
    while let Some(object) = current {
        if object.as_raw() == state_proto.as_raw() {
            return Ok(js::value::from_bool(true));
        }
        current = object.get_prototype(scope)?;
    }
    Ok(js::value::from_bool(false))
}

/// The accessors of `Writable.prototype`: each name with its getter and setter.
pub(crate) fn accessors() -> [(&'static std::ffi::CStr, Callback, Option<Callback>); 13] {
    [
        (c"closed", get_closed, None),
        (c"destroyed", get_destroyed, Some(set_destroyed)),
        (c"writable", get_writable, Some(set_writable)),
        (c"writableFinished", get_writable_finished, None),
        (c"writableObjectMode", get_writable_object_mode, None),
        (c"writableBuffer", get_writable_buffer, None),
        (c"writableEnded", get_writable_ended, None),
        (c"writableNeedDrain", get_writable_need_drain, None),
        (c"writableHighWaterMark", get_writable_high_water_mark, None),
        (c"writableCorked", get_writable_corked, None),
        (c"writableLength", get_writable_length, None),
        (c"errored", get_errored, None),
        (c"writableAborted", get_writable_aborted, None),
    ]
}

/// The `Writable` class's [`writable::setup`].
fn setup_writable(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
    for (name, getter, setter) in accessors() {
        accessor(scope, proto, name, getter, setter)?;
    }
    proto.set_property(scope, c"_writev", js::value::null())?;
    let key = js::symbol::get_well_known_key(scope, js::native::SymbolCode::hasInstance);
    let function = Function::new_callback(scope, c"[Symbol.hasInstance]", 1, has_instance, ())?;
    let attrs = js::class_spec::JSPROP_PERMANENT | js::class_spec::JSPROP_READONLY;
    ctor.define_value_by_id(scope, scope.root_id(key), *function, attrs.into())?;
    let capture = crate::symbol(scope, "nodejs.rejection", true)?;
    super::readable::define_symbol_method(
        scope,
        proto,
        capture,
        c"",
        super::readable::capture_rejection,
    )?;
    super::readable::define_async_dispose(scope, proto, true)?;
    ctor.set_property(
        scope,
        c"WritableState",
        writable_state::class_constructor(scope)?,
    )?;
    Ok(())
}
