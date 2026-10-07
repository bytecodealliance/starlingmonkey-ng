// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Writable`: Node's `lib/internal/streams/writable.js`.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, Callback, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::destroy::{
    self, error_or_destroy, error_or_destroy_with, multiple_callback, stream_destroyed,
};
use super::readable::{
    accessor, buffer_from, chunk_length, invalid_chunk, is_buffer, is_encoding, unknown_encoding,
    view_to_buffer,
};
use super::state::{
    AfterWriteTickInfo, AfterWriteTickInfoImpl, BufferedWrite, ReadableSide, StreamState,
    WritablePart, WritableSide, R, W,
};
use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::support::{is_str, rust_string};

/// Node's `resetBuffer`.
fn reset_buffer(side: WritableSide<'_>) {
    let mut part = side.get_mut();
    part.buffered.clear();
    part.flags.insert(W::ALL_BUFFERS | W::ALL_NOOP);
}

/// Give `holder`'s state a writable part for `options` whose `onwrite` writes to `stream`: Node's
/// `new WritableState()`, of a `Duplex`'s writable side if `is_duplex`.
pub(crate) fn init_writable_part(
    scope: &Scope<'_>,
    holder: StreamState<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
    is_duplex: bool,
) -> Result<(), Thrown> {
    let options = options.and_then(|options| Object::from_value(scope, *options).ok());
    let option = |name: &'static std::ffi::CStr| -> Result<HandleValue<'_>, ExnThrown> {
        match options {
            Some(options) => options.get_property(scope, name),
            None => Ok(HandleValue::undefined()),
        }
    };
    let mut flags = W::SYNC | W::CONSTRUCTED | W::EMIT_CLOSE | W::AUTO_DESTROY;
    let object_mode = js::conversion::to_boolean(option(c"objectMode")?)
        || (is_duplex && js::conversion::to_boolean(option(c"writableObjectMode")?));
    flags.set(W::OBJECT_MODE, object_mode);
    let high_water_mark = match options {
        Some(options) => super::state::high_water_mark(
            scope,
            options,
            c"writableHighWaterMark",
            is_duplex,
            object_mode,
        )?,
        None => super::state::default_high_water_mark(false),
    };
    flags.set(
        W::DECODE_STRINGS,
        !crate::support::is_false(option(c"decodeStrings")?),
    );
    flags.set(
        W::EMIT_CLOSE,
        !crate::support::is_false(option(c"emitClose")?),
    );
    flags.set(
        W::AUTO_DESTROY,
        !crate::support::is_false(option(c"autoDestroy")?),
    );
    let default_encoding = option(c"defaultEncoding")?;
    let default_encoding = if default_encoding.is_null_or_undefined() {
        None
    } else if is_encoding(scope, default_encoding)? {
        super::state::default_encoding_field(scope, default_encoding)?
    } else {
        return Err(unknown_encoding(scope, default_encoding));
    };
    let onwrite = Function::new_callback(scope, c"", 1, onwrite, stream)?;
    flags.insert(W::ALL_BUFFERS | W::ALL_NOOP);
    holder.data_mut().writable = Some(WritablePart {
        flags,
        writable: None,
        high_water_mark,
        length: 0.0,
        corked: 0.0,
        writelen: 0.0,
        pendingcb: 0.0,
        errored: None,
        default_encoding: default_encoding.map(|value| Heap::from(value.get())),
        writecb: None,
        onwrite: Heap::from(*onwrite),
        after_write_tick_info: None,
        buffered: Default::default(),
        on_finished: Vec::new(),
    });
    Ok(())
}

/// The writable side's `kOnConstructed`: start writing what was buffered, and finish if ending.
pub(crate) fn on_constructed(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: WritableSide<'_>,
) -> Result<(), ExnThrown> {
    if !side.has(W::WRITING) {
        clear_buffer(scope, stream, side)?;
    }
    if side.has(W::ENDING) {
        finish_maybe(scope, stream, side, false)?;
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
    let side = WritableSide::of(scope, stream)?;
    let callback = js::function::is_callable(callback).then(|| scope.root_value(callback.get()));
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
    if !side.has(W::OBJECT_MODE) {
        if !js::conversion::to_boolean(encoding) {
            encoding = side.default_encoding(scope)?;
        } else if !is_str(scope, encoding, c"buffer")? && !is_encoding(scope, encoding)? {
            return Err(unknown_encoding(scope, encoding));
        }
        if chunk.is_string() {
            if is_str(scope, encoding, c"buffer")? {
                return Err(unknown_encoding(scope, encoding));
            }
            if side.has(W::DECODE_STRINGS) {
                chunk = buffer_from(scope, &[chunk, encoding])?;
                encoding = js::class::get_or_init_atom(scope, c"buffer")?;
            }
        } else if is_buffer(scope, chunk)? {
            encoding = js::class::get_or_init_atom(scope, c"buffer")?;
        } else if js::ArrayBufferView::from_value(scope, chunk).is_some() {
            chunk = view_to_buffer(scope, chunk)?;
            encoding = js::class::get_or_init_atom(scope, c"buffer")?;
        } else {
            return Err(invalid_chunk(scope, chunk).into());
        }
    }
    let error = if side.has(W::ENDING) {
        Some(NodeError::new(
            ErrorKind::Error,
            "ERR_STREAM_WRITE_AFTER_END",
            "write after end",
        ))
    } else if side.has(W::DESTROYED) {
        Some(stream_destroyed("write"))
    } else {
        None
    };
    if let Some(error) = error {
        let error = error.to_value(scope)?;
        if let Some(callback) = callback {
            js::jobs::queue_tick(scope, callback, &[error])?;
        }
        error_or_destroy(scope, stream, error, true)?;
        return Ok(Err(error));
    }
    side.add_pending(1.0);
    Ok(Ok(write_or_buffer(
        scope, stream, side, chunk, encoding, callback,
    )?))
}

/// The callback `_write` gets.
fn onwrite_function<'s>(scope: &'s Scope<'_>, side: WritableSide<'_>) -> HandleValue<'s> {
    let onwrite = side.get().onwrite.get(scope);
    scope.root_value(onwrite.as_value())
}

/// Node's `writeOrBuffer`.
fn write_or_buffer(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: WritableSide<'_>,
    chunk: HandleValue<'_>,
    encoding: HandleValue<'_>,
    callback: Option<HandleValue<'_>>,
) -> Result<bool, ExnThrown> {
    let len = if side.has(W::OBJECT_MODE) {
        1.0
    } else {
        chunk_length(scope, chunk)?
    };
    side.get_mut().length += len;
    let buffer = {
        let part = side.get();
        part.flags.contains(W::WRITING)
            || part.errored.is_some()
            || part.corked > 0.0
            || !part.flags.contains(W::CONSTRUCTED)
    };
    if buffer {
        let is_buffer_encoding = is_str(scope, encoding, c"buffer")?;
        let mut part = side.get_mut();
        part.buffered.push_back(BufferedWrite {
            chunk: Heap::from(chunk.get()),
            encoding: Heap::from(encoding.get()),
            callback: Heap::from(
                callback.map_or(js::value::undefined(), |callback| callback.get()),
            ),
        });
        if !is_buffer_encoding {
            part.flags.remove(W::ALL_BUFFERS);
        }
        if callback.is_some() {
            part.flags.remove(W::ALL_NOOP);
        }
    } else {
        {
            let mut part = side.get_mut();
            part.writelen = len;
            if let Some(callback) = callback {
                part.writecb = Some(Heap::from(callback.get()));
            }
            part.flags.insert(W::WRITING | W::SYNC | W::EXPECT_WRITE_CB);
        }
        let onwrite = onwrite_function(scope, side);
        Function::call_by_name(
            scope,
            stream.handle(),
            c"_write",
            &[chunk, encoding, onwrite],
        )?;
        side.set(W::SYNC, false);
    }
    let part = side.get();
    let ret = part.length < part.high_water_mark || part.length == 0.0;
    drop(part);
    if !ret {
        side.set(W::NEED_DRAIN, true);
    }
    let part = side.get();
    Ok(ret && !part.flags.contains(W::DESTROYED) && part.errored.is_none())
}

/// Node's `doWrite`.
#[allow(clippy::too_many_arguments)]
fn do_write(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: WritableSide<'_>,
    writev: bool,
    len: f64,
    chunk: HandleValue<'_>,
    encoding: HandleValue<'_>,
    callback: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    {
        let mut part = side.get_mut();
        part.writelen = len;
        if let Some(callback) = callback {
            part.writecb = Some(Heap::from(callback.get()));
        }
        part.flags.insert(W::WRITING | W::SYNC | W::EXPECT_WRITE_CB);
    }
    let onwrite = onwrite_function(scope, side);
    if side.has(W::DESTROYED) {
        let error = stream_destroyed("write").to_value(scope)?;
        Function::call(scope, HandleValue::undefined(), onwrite, &[error])?;
    } else if writev {
        Function::call_by_name(scope, stream.handle(), c"_writev", &[chunk, onwrite])?;
    } else {
        Function::call_by_name(
            scope,
            stream.handle(),
            c"_write",
            &[chunk, encoding, onwrite],
        )?;
    }
    side.set(W::SYNC, false);
    Ok(())
}

/// Call `callback` with `args`, unless it is `undefined`, Node's `nop`.
fn call_back(
    scope: &Scope<'_>,
    callback: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    if !callback.is_undefined() {
        Function::call(scope, HandleValue::undefined(), callback, args)?;
    }
    Ok(())
}

/// Node's `onwriteError`.
fn onwrite_error(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: WritableSide<'_>,
    error: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    side.add_pending(-1.0);
    call_back(scope, callback, &[error])?;
    error_buffer(scope, side)?;
    error_or_destroy(scope, stream, error, false)
}

/// The tick of Node's `onwriteError`, whose payload is the stream and whose arguments are the
/// error and the write callback.
fn onwrite_error_tick(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = WritableSide::of(scope, stream)?;
    onwrite_error(scope, stream, side, args.get(0), args.get(1))?;
    Ok(js::value::undefined())
}

/// Node's `onwrite`, the callback `_write` gets, whose payload is the stream.
fn onwrite(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = WritableSide::of(scope, stream)?;
    if !side.has(W::EXPECT_WRITE_CB) {
        error_or_destroy_with(scope, stream, multiple_callback(), false)?;
        return Ok(js::value::undefined());
    }
    let (sync, callback) = {
        let mut part = side.get_mut();
        let sync = part.flags.contains(W::SYNC);
        let callback = part.writecb.as_ref().map(|callback| callback.get(scope));
        part.writecb = None;
        part.flags.remove(W::WRITING | W::EXPECT_WRITE_CB);
        part.length -= part.writelen;
        part.writelen = 0.0;
        (sync, callback.unwrap_or(HandleValue::undefined()))
    };
    let error = args.get(0);
    if js::conversion::to_boolean(error) {
        if !side.is_errored() {
            side.set_errored(error);
        }
        if let Some(readable) = ReadableSide::of_state(side.0) {
            if !readable.is_errored() {
                readable.set_errored(error);
            }
        }
        if sync {
            js::jobs::queue_native_tick(scope, onwrite_error_tick, stream, &[error, callback])?;
        } else {
            onwrite_error(scope, stream, side, error, callback)?;
        }
        return Ok(js::value::undefined());
    }
    if side.has_buffered() {
        clear_buffer(scope, stream, side)?;
    }
    if !sync {
        after_write(scope, stream, side, 1.0, callback)?;
        return Ok(js::value::undefined());
    }
    let (need_drain, object_mode) = {
        let part = side.get();
        (
            part.flags.contains(W::NEED_DRAIN) && part.length == 0.0,
            part.flags.contains(W::OBJECT_MODE),
        )
    };
    // Node.js computes `state[kState] & kDestroyed !== 0`, which tests the object mode bit, so this
    // tests `objectMode` instead of `destroyed`.
    let need_tick = need_drain || object_mode || !callback.is_undefined();
    if callback.is_undefined() {
        if !side.has(W::AFTER_WRITE_PENDING) && need_tick {
            js::jobs::queue_native_tick(scope, after_write_tick_noop, stream, &[])?;
            side.set(W::AFTER_WRITE_PENDING, true);
        } else {
            side.add_pending(-1.0);
            if side.has(W::ENDING) {
                finish_maybe(scope, stream, side, true)?;
            }
        }
        return Ok(js::value::undefined());
    }
    let info = side.get().after_write_tick_info.get(scope);
    let same = match info {
        Some(info) => {
            let previous = info.data().callback.get(scope);
            js::comparison::strictly_equal(scope, previous, callback)?
        }
        None => false,
    };
    if let (true, Some(info)) = (same, info) {
        info.data_mut().count += 1.0;
    } else if need_tick {
        let info = js::class::create_instance_with::<AfterWriteTickInfoImpl>(scope, |_| {
            AfterWriteTickInfoImpl {
                stream: Heap::from(stream),
                callback: Heap::from(callback.get()),
                count: 1.0,
            }
        })?;
        side.get_mut().after_write_tick_info = Some(Heap::from(info));
        js::jobs::queue_native_tick(scope, after_write_tick, info, &[])?;
        side.set(W::AFTER_WRITE_PENDING, true);
    } else {
        side.add_pending(-1.0);
        if side.has(W::ENDING) {
            finish_maybe(scope, stream, side, true)?;
        }
    }
    Ok(js::value::undefined())
}

/// Node's `afterWrite` for a write without a callback, whose payload is the stream.
fn after_write_tick_noop(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = WritableSide::of(scope, stream)?;
    after_write(scope, stream, side, 1.0, HandleValue::undefined())?;
    Ok(js::value::undefined())
}

/// Node's `afterWriteTick`, whose payload is an [`AfterWriteTickInfo`].
fn after_write_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let info = cast_payload::<AfterWriteTickInfo>(scope, payload);
    let (stream, callback, count) = {
        let data = info.data();
        (data.stream.get(scope), data.callback.get(scope), data.count)
    };
    let side = WritableSide::of(scope, stream)?;
    side.get_mut().after_write_tick_info = None;
    after_write(scope, stream, side, count, callback)?;
    Ok(js::value::undefined())
}

/// Node's `afterWrite`: emit `drain` if it's needed, and call the write callback `count` times.
fn after_write(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: WritableSide<'_>,
    count: f64,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    side.set(W::AFTER_WRITE_PENDING, false);
    let need_drain = {
        let part = side.get();
        (part.flags & (W::ENDING | W::NEED_DRAIN | W::DESTROYED)) == W::NEED_DRAIN
            && part.length == 0.0
    };
    if need_drain {
        side.set(W::NEED_DRAIN, false);
        crate::events::emit(scope, stream, c"drain", &[])?;
    }
    let null = HandleValue::null();
    for _ in 0..count as u64 {
        side.add_pending(-1.0);
        call_back(scope, callback, &[null])?;
    }
    if side.has(W::DESTROYED) {
        error_buffer(scope, side)?;
    }
    if side.has(W::ENDING) {
        finish_maybe(scope, stream, side, true)?;
    }
    Ok(())
}

/// The error buffered writes and end callbacks fail with: the state's error, or
/// `ERR_STREAM_DESTROYED` for `method`.
fn buffer_error<'s>(
    scope: &'s Scope<'_>,
    side: WritableSide<'_>,
    method: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    match side.errored(scope) {
        Some(error) => Ok(error),
        None => stream_destroyed(method).to_value(scope),
    }
}

/// Node's `errorBuffer`: fail every buffered write and end callback.
fn error_buffer(scope: &Scope<'_>, side: WritableSide<'_>) -> Result<(), ExnThrown> {
    if side.has(W::WRITING) {
        return Ok(());
    }
    let object_mode = side.has(W::OBJECT_MODE);
    let count = side.get().buffered.len();
    for index in 0..count {
        let entry = {
            let part = side.get();
            part.buffered
                .get(index)
                .map(|entry| (entry.chunk.get(scope), entry.callback.get(scope)))
        };
        let Some((chunk, callback)) = entry else {
            break;
        };
        let len = if object_mode {
            1.0
        } else {
            chunk_length(scope, chunk)?
        };
        side.get_mut().length -= len;
        if !callback.is_undefined() {
            let error = buffer_error(scope, side, "write")?;
            Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        }
    }
    let error = buffer_error(scope, side, "end")?;
    call_finished_callbacks(scope, side, error)?;
    reset_buffer(side);
    Ok(())
}

/// The tick of Node's `errorBuffer`, whose payload is the object holding the state.
fn error_buffer_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<StreamState>(scope, payload);
    if let Some(side) = WritableSide::of_state(state) {
        error_buffer(scope, side)?;
    }
    Ok(js::value::undefined())
}

/// The callbacks of the writes a `_writev` call got, which its callback calls.
#[core_runtime::jsclass(hidden)]
pub(crate) struct WritevCallbacks {
    callbacks: Vec<Heap<Value>>,
}

#[core_runtime::jsmethods]
impl WritevCallbacks {}

/// The callback of a `_writev` of several buffered writes, whose payload is a
/// [`WritevCallbacks`]: call each write's callback.
fn writev_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let callbacks = cast_payload::<WritevCallbacks>(scope, payload);
    let callbacks: Vec<HandleValue<'_>> = callbacks
        .data()
        .callbacks
        .iter()
        .map(|callback| callback.get(scope))
        .collect();
    for callback in callbacks {
        call_back(scope, callback, &[args.get(0)])?;
    }
    Ok(js::value::undefined())
}

/// Node's `clearBuffer`: write the buffered writes, all at once with `_writev` if the stream has
/// one.
fn clear_buffer(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: WritableSide<'_>,
) -> Result<(), ExnThrown> {
    let (blocked, count, object_mode, all_noop, all_buffers) = {
        let part = side.get();
        (
            part.flags.intersects(W::DESTROYED | W::BUFFER_PROCESSING)
                || part.corked > 0.0
                || part.buffered.is_empty()
                || !part.flags.contains(W::CONSTRUCTED),
            part.buffered.len(),
            part.flags.contains(W::OBJECT_MODE),
            part.flags.contains(W::ALL_NOOP),
            part.flags.contains(W::ALL_BUFFERS),
        )
    };
    if blocked {
        return Ok(());
    }
    side.set(W::BUFFER_PROCESSING, true);
    let writev = stream.get_property(scope, c"_writev")?;
    if count > 1 && js::conversion::to_boolean(writev) {
        side.add_pending(-((count - 1) as f64));
        let callback = if all_noop {
            None
        } else {
            let callbacks = js::class::create_instance_with::<WritevCallbacksImpl>(scope, |_| {
                WritevCallbacksImpl {
                    callbacks: Vec::new(),
                }
            })?;
            for index in 0..count {
                let callback = {
                    let part = side.get();
                    part.buffered
                        .get(index)
                        .map(|entry| entry.callback.get(scope))
                };
                if let Some(callback) = callback {
                    callbacks
                        .data_mut()
                        .callbacks
                        .push(Heap::from(callback.get()));
                }
            }
            let function = Function::new_callback(scope, c"", 1, writev_callback, callbacks)?;
            Some(scope.root_value((*function).as_value()))
        };
        let chunks = super::state::buffered_array(scope, side)?;
        chunks.set_property(scope, c"allBuffers", all_buffers)?;
        let length = side.length();
        let empty = js::class::get_or_init_atom(scope, c"")?;
        let chunks = chunks.to_jsval_throwing(scope)?;
        do_write(scope, stream, side, true, length, chunks, empty, callback)?;
        reset_buffer(side);
    } else {
        loop {
            let entry = {
                let part = side.get();
                part.buffered.front().map(|entry| {
                    (
                        entry.chunk.get(scope),
                        entry.encoding.get(scope),
                        entry.callback.get(scope),
                    )
                })
            };
            let Some((chunk, encoding, callback)) = entry else {
                break;
            };
            drop(side.get_mut().buffered.pop_front());
            let len = if object_mode {
                1.0
            } else {
                chunk_length(scope, chunk)?
            };
            let callback = (!callback.is_undefined()).then_some(callback);
            do_write(scope, stream, side, false, len, chunk, encoding, callback)?;
            if side.get().buffered.is_empty() || side.has(W::WRITING) {
                break;
            }
        }
        if !side.has_buffered() {
            reset_buffer(side);
        }
    }
    side.set(W::BUFFER_PROCESSING, false);
    Ok(())
}

/// Node's `needFinish`.
fn need_finish(side: WritableSide<'_>) -> bool {
    let part = side.get();
    part.flags.contains(W::ENDING | W::CONSTRUCTED)
        && !part.flags.intersects(
            W::DESTROYED | W::FINISHED | W::WRITING | W::ERROR_EMITTED | W::CLOSE_EMITTED,
        )
        && part.errored.is_none()
        && part.buffered.is_empty()
        && part.length == 0.0
}

/// Node's `onFinish`, the callback `_final` gets, whose payload is the stream.
fn on_final(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = WritableSide::of(scope, stream)?;
    let error = args.get(0);
    if side.has(W::PREFINISHED) {
        if !error.is_null_or_undefined() {
            error_or_destroy(scope, stream, error, false)?;
        } else {
            error_or_destroy_with(scope, stream, multiple_callback(), false)?;
        }
        return Ok(js::value::undefined());
    }
    side.add_pending(-1.0);
    if js::conversion::to_boolean(error) {
        call_finished_callbacks(scope, side, error)?;
        error_or_destroy(scope, stream, error, side.has(W::SYNC))?;
    } else if need_finish(side) {
        side.set(W::PREFINISHED, true);
        crate::events::emit(scope, stream, c"prefinish", &[])?;
        side.add_pending(1.0);
        js::jobs::queue_native_tick(scope, finish_tick, stream, &[])?;
    }
    Ok(js::value::undefined())
}

/// Node's `prefinish`: call `_final`, or emit `prefinish` if there is none.
fn prefinish(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: WritableSide<'_>,
) -> Result<(), ExnThrown> {
    if side.any(W::PREFINISHED | W::FINAL_CALLED) {
        return Ok(());
    }
    let final_method = stream.get_property(scope, c"_final")?;
    if js::function::is_callable(final_method) && !side.has(W::DESTROYED) {
        side.set(W::FINAL_CALLED | W::SYNC, true);
        side.add_pending(1.0);
        let callback = Function::new_callback(scope, c"", 1, on_final, stream)?;
        let callback = (*callback).to_jsval_throwing(scope)?;
        if Function::call_by_name(scope, stream.handle(), c"_final", &[callback]).is_err() {
            let error = js::exception::take_pending_or_undefined(scope);
            Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        }
        side.set(W::SYNC, false);
    } else {
        side.set(W::FINAL_CALLED | W::PREFINISHED, true);
        crate::events::emit(scope, stream, c"prefinish", &[])?;
    }
    Ok(())
}

/// Node's `finishMaybe`.
fn finish_maybe(
    scope: &Scope<'_>,
    stream: Object<'_>,
    side: WritableSide<'_>,
    sync: bool,
) -> Result<(), ExnThrown> {
    if !need_finish(side) {
        return Ok(());
    }
    prefinish(scope, stream, side)?;
    if side.get().pendingcb == 0.0 {
        if sync {
            side.add_pending(1.0);
            js::jobs::queue_native_tick(scope, finish_if_needed_tick, stream, &[])?;
        } else if need_finish(side) {
            side.add_pending(1.0);
            finish(scope, stream, side)?;
        }
    }
    Ok(())
}

fn finish_if_needed_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = WritableSide::of(scope, stream)?;
    if need_finish(side) {
        finish(scope, stream, side)?;
    } else {
        side.add_pending(-1.0);
    }
    Ok(js::value::undefined())
}

fn finish_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let side = WritableSide::of(scope, stream)?;
    finish(scope, stream, side)?;
    Ok(js::value::undefined())
}

/// Node's `finish`: emit `finish`, and auto-destroy once the readable side, if any, ended too.
fn finish(scope: &Scope<'_>, stream: Object<'_>, side: WritableSide<'_>) -> Result<(), ExnThrown> {
    side.add_pending(-1.0);
    side.set(W::FINISHED, true);
    call_finished_callbacks(scope, side, HandleValue::null())?;
    crate::events::emit(scope, stream, c"finish", &[])?;
    if side.has(W::AUTO_DESTROY) {
        let auto_destroy = match ReadableSide::of_state(side.0) {
            None => true,
            Some(r) => r.has(R::AUTO_DESTROY) && (r.has(R::END_EMITTED) || r.disabled()),
        };
        if auto_destroy {
            Function::call_by_name(scope, stream.handle(), c"destroy", js::function::EmptyArgs)?;
        }
    }
    Ok(())
}

/// Node's `callFinishedCallbacks`: call each `end()` callback with `error`.
fn call_finished_callbacks(
    scope: &Scope<'_>,
    side: WritableSide<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let callbacks: Vec<HandleValue<'_>> = side
        .get()
        .on_finished
        .iter()
        .map(|callback| callback.get(scope))
        .collect();
    side.get_mut().on_finished.clear();
    for callback in callbacks {
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
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
        Some(encoding) if js::function::is_callable(encoding) => (HandleValue::null(), encoding),
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
    let side = WritableSide::of(scope, stream)?;
    let mut chunk = chunk.unwrap_or(HandleValue::undefined());
    let mut encoding = encoding.unwrap_or(HandleValue::undefined());
    let mut callback = callback.unwrap_or(HandleValue::undefined());
    if js::function::is_callable(chunk) {
        callback = chunk;
        chunk = HandleValue::null();
        encoding = HandleValue::null();
    } else if js::function::is_callable(encoding) {
        callback = encoding;
        encoding = HandleValue::null();
    }
    let mut error = None;
    if !chunk.is_null_or_undefined() {
        if let Err(failed) = write_chunk(scope, stream, chunk, encoding, HandleValue::undefined())?
        {
            error = Some(failed);
        }
    }
    if side.corked() {
        side.get_mut().corked = 1.0;
        Function::call_by_name(scope, stream.handle(), c"uncork", js::function::EmptyArgs)?;
    }
    if error.is_some() {
    } else if !side.has(W::ENDING) && !side.is_errored() {
        side.set(W::ENDING, true);
        finish_maybe(scope, stream, side, true)?;
        side.set(W::ENDED, true);
    } else if side.has(W::FINISHED) {
        error = Some(
            NodeError::new(
                ErrorKind::Error,
                "ERR_STREAM_ALREADY_FINISHED",
                "Cannot call end after a stream was finished",
            )
            .to_value(scope)?,
        );
    } else if side.has(W::DESTROYED) {
        error = Some(stream_destroyed("end").to_value(scope)?);
    }
    if js::function::is_callable(callback) {
        if let Some(error) = error {
            js::jobs::queue_tick(scope, callback, &[error])?;
        } else if let Some(errored) = side.errored(scope) {
            js::jobs::queue_tick(scope, callback, &[errored])?;
        } else if side.has(W::FINISHED) {
            js::jobs::queue_tick(scope, callback, &[HandleValue::null()])?;
        } else {
            side.get_mut().on_finished.push(Heap::from(callback.get()));
        }
    }
    Ok(stream)
}

#[core_runtime::jsfunction_class(
    name = "Writable",
    extends = "super::super::legacy::stream",
    state = "super::super::state::StreamState",
    invalid_this = "crate::errors::invalid_this",
    self_construct
)]
pub mod writable {
    use super::*;

    /// <https://nodejs.org/api/stream.html#new-streamwritableoptions>
    #[constructor(length = 1)]
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
    #[method(length = 3)]
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
    #[method(length = 3)]
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
        WritableSide::of(scope, this)?.get_mut().corked += 1.0;
        Ok(())
    }

    /// <https://nodejs.org/api/stream.html#writableuncork>
    pub fn uncork(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let side = WritableSide::of(scope, this)?;
        if side.corked() {
            side.get_mut().corked -= 1.0;
            if !side.has(W::WRITING) {
                clear_buffer(scope, this, side)?;
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
            Some(name) => js::conversion::ToJSVal::to_jsval_throwing(&name.to_lowercase(), scope)?,
            None => scope.root_value(encoding.get()),
        };
        if !is_encoding(scope, encoding)? {
            return Err(unknown_encoding(scope, encoding));
        }
        let side = WritableSide::of(scope, this)?;
        side.get_mut().default_encoding =
            super::super::state::default_encoding_field(scope, encoding)?
                .map(|value| Heap::from(value.get()));
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
        if !js::conversion::to_boolean(writev) {
            return Err(NodeError::not_implemented("_write()").into());
        }
        let entry = Object::new_plain(scope)?;
        entry.set_property(scope, c"chunk", chunk)?;
        entry.set_property(scope, c"encoding", encoding)?;
        let list = js::Array::with_contents(scope, &[entry.to_jsval_raw_throwing(scope)?])?;
        Function::call(
            scope,
            this.to_jsval_throwing(scope)?,
            writev,
            &[(*list).to_jsval_throwing(scope)?, callback],
        )?;
        Ok(())
    }

    /// <https://nodejs.org/api/stream.html#writabledestroyerror>
    #[method(length = 2)]
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
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
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
    #[static_method(length = 2)]
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

/// `writable.destroy(error, callback)`, which also fails the buffered writes in a tick.
pub(crate) fn destroy_writable<'s>(
    scope: &Scope<'_>,
    stream: Object<'s>,
    error: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    if let Some(side) = WritableSide::try_of(scope, stream)? {
        let pending = {
            let part = side.get();
            (!part.buffered.is_empty() || !part.on_finished.is_empty())
                && !part.flags.contains(W::DESTROYED)
        };
        if pending {
            js::jobs::queue_native_tick(scope, error_buffer_tick, side.0, &[])?;
        }
    }
    let error = error.unwrap_or(HandleValue::undefined());
    let callback = callback.unwrap_or(HandleValue::undefined());
    destroy::destroy(scope, stream, error, callback)
}

/// Create the writable part of the state of `stream` and its `_writableState` view, and take the
/// writable options.
pub(crate) fn init_writable(
    scope: &Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
    is_duplex: bool,
) -> Result<(), Thrown> {
    let state = writable::state_of(scope, stream)?;
    init_writable_part(scope, state, stream, options, is_duplex)?;
    let view = super::state::new_writable_view(scope, state)?;
    state.data_mut().writable_view = Some(Heap::from(*view));
    stream.set_property(scope, c"_writableState", view)?;
    let options = options.and_then(|options| Object::from_value(scope, options).ok());
    // The `Duplex` constructor assigns the methods of both sides itself.
    if let Some(options) = options.filter(|_| !is_duplex) {
        for (option, method) in [
            (c"write", c"_write"),
            (c"writev", c"_writev"),
            (c"destroy", c"_destroy"),
            (c"final", c"_final"),
            (c"construct", c"_construct"),
        ] {
            let value = options.get_property(scope, option)?;
            if js::function::is_callable(value) {
                stream.set_property(scope, method, value)?;
            }
        }
        let signal = options.get_property(scope, c"signal")?;
        if js::conversion::to_boolean(signal) {
            super::add_abort_signal(scope, signal, stream.to_jsval_throwing(scope)?)?;
        }
    }
    Ok(())
}

/// The writable part of the state of an accessor's `this`, if it has one.
fn this_side<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
) -> Result<Option<WritableSide<'s>>, ExnThrown> {
    match Object::from_value(scope, args.this()) {
        Ok(this) => WritableSide::try_of(scope, this),
        Err(_) => Ok(None),
    }
}

/// A getter of `Writable.prototype`: `body` of the writable part of `this`'s state, or `default`
/// without one.
fn writable_getter(
    scope: &Scope<'_>,
    args: &CallbackArgs<'_>,
    default: impl FnOnce() -> Value,
    body: impl FnOnce(&WritablePart) -> Value,
) -> Result<Value, ExnThrown> {
    Ok(match this_side(scope, args)? {
        Some(side) => body(&side.get()),
        None => default(),
    })
}

/// Define a `Writable.prototype` getter named `$fn_name` whose value is `$body` of the writable
/// part, or `$default` without one.
macro_rules! writable_getter {
    ($fn_name:ident, $default:expr, |$part:ident| $body:expr) => {
        fn $fn_name(
            scope: &Scope<'_>,
            args: CallbackArgs<'_>,
            _payload: HandleValue,
        ) -> Result<Value, ExnThrown> {
            writable_getter(scope, &args, || $default, |$part| $body)
        }
    };
}

writable_getter!(get_closed, js::value::from_bool(false), |part| {
    js::value::from_bool(part.flags.contains(W::CLOSED))
});
writable_getter!(get_destroyed, js::value::from_bool(false), |part| {
    js::value::from_bool(part.flags.contains(W::DESTROYED))
});
writable_getter!(get_writable, js::value::from_bool(false), |part| {
    js::value::from_bool(part.is_writable())
});
writable_getter!(get_writable_finished, js::value::from_bool(false), |part| {
    js::value::from_bool(part.flags.contains(W::FINISHED))
});
writable_getter!(
    get_writable_object_mode,
    js::value::from_bool(false),
    |part| { js::value::from_bool(part.flags.contains(W::OBJECT_MODE)) }
);
writable_getter!(get_writable_ended, js::value::from_bool(false), |part| {
    js::value::from_bool(part.flags.contains(W::ENDING))
});
writable_getter!(
    get_writable_need_drain,
    js::value::from_bool(false),
    |part| {
        js::value::from_bool(
            (part.flags & (W::DESTROYED | W::ENDING | W::NEED_DRAIN)) == W::NEED_DRAIN,
        )
    }
);
writable_getter!(
    get_writable_high_water_mark,
    js::value::undefined(),
    |part| { js::value::from_f64(part.high_water_mark) }
);
writable_getter!(get_writable_corked, js::value::from_i32(0), |part| {
    js::value::from_f64(part.corked)
});
writable_getter!(get_writable_length, js::value::undefined(), |part| {
    js::value::from_f64(part.length)
});
writable_getter!(get_writable_aborted, js::value::from_bool(false), |part| {
    js::value::from_bool(
        part.writable != Some(false)
            && (part.flags.contains(W::DESTROYED) || part.errored.is_some())
            && !part.flags.contains(W::FINISHED),
    )
});

fn get_writable_buffer(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(match this_side(scope, &args)? {
        Some(side) => super::state::buffered_array(scope, side)?.as_value(),
        None => js::value::undefined(),
    })
}

fn get_errored(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(match this_side(scope, &args)? {
        Some(side) => side
            .errored(scope)
            .map_or(js::value::null(), |error| error.get()),
        None => js::value::null(),
    })
}

fn set_destroyed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(side) = this_side(scope, &args)? {
        side.set(W::DESTROYED, js::conversion::to_boolean(args.get(0)));
    }
    Ok(js::value::undefined())
}

pub(crate) fn set_writable(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(side) = this_side(scope, &args)? {
        side.get_mut().writable = Some(js::conversion::to_boolean(args.get(0)));
    }
    Ok(js::value::undefined())
}

/// `Writable[Symbol.hasInstance](object)`: an ordinary instance, or, for `Writable` itself, any
/// object whose state has a writable part, such as a `Duplex`.
fn has_instance(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Ok(ctor) = Object::from_value(scope, args.this()) else {
        return Ok(js::value::from_bool(false));
    };
    if ctor.ordinary_has_instance(scope, args.get(0))? {
        return Ok(js::value::from_bool(true));
    }
    let writable_ctor = writable::class_constructor(scope)?;
    if ctor.as_raw() != writable_ctor.as_raw() {
        return Ok(js::value::from_bool(false));
    }
    let Ok(candidate) = Object::from_value(scope, args.get(0)) else {
        return Ok(js::value::from_bool(false));
    };
    Ok(js::value::from_bool(
        WritableSide::try_of(scope, candidate)?.is_some(),
    ))
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
    let capture = crate::events::REJECTION.value(scope)?;
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
        super::state::WritableState::class_constructor(scope)?,
    )?;
    Ok(())
}
