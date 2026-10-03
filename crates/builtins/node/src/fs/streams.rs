// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `fs.ReadStream` and `fs.WriteStream`, following Node.js v24's `lib/internal/fs/streams.js`.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{CallbackArgs, EmptyArgs};
use js::gc::scope::Scope;
use js::native::{HandleId, Value};
use js::prelude::HandleValue;
use js::{Function, Object};

use super::{ops, run_callback, Variant};
use crate::errors::{validate_integer, NodeError, Thrown};
use crate::stream::{call_method, closure, is_function, string, truthy, Captures, Props};

/// A copy of the stream options `options`, which may be an encoding name, for the stream base
/// class: with `highWaterMark` defaulting to `high_water_mark`, and `autoDestroy` following
/// `autoClose`.
fn base_options<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'_>>,
    high_water_mark: Option<f64>,
) -> Result<Object<'s>, ExnThrown> {
    let copy = Object::new_plain(scope)?;
    match options {
        Some(options) if options.is_string() => copy.set_property(scope, c"encoding", options)?,
        Some(options) if options.is_object() => {
            let options = Object::from_value(scope, *options).map_err(|e| e.throw(scope))?;
            copy.assign(scope, options)?;
        }
        _ => {}
    }
    if let Some(mark) = high_water_mark {
        if copy.val(scope, c"highWaterMark")?.is_undefined() {
            copy.set_property(scope, c"highWaterMark", mark)?;
        }
    }
    let auto_close = copy.val(scope, c"autoClose")?;
    copy.set_property(
        scope,
        c"autoDestroy",
        auto_close.is_undefined() || truthy(scope, auto_close),
    )?;
    Ok(copy)
}

/// The key of the `FileHandle` a stream reads or writes, which the stream closes.
fn handle_key<'s>(scope: &'s Scope<'_>) -> Result<HandleId<'s>, ExnThrown> {
    let symbol = crate::symbol(scope, "nodejs.fs.stream.handle", true)?;
    Ok(scope.root_id(js::id::value_to_id(scope, symbol)?))
}

/// Set the fields both stream classes share on `stream` from `path` and the `options` copy:
/// `fd`, `path`, `flags`, `mode`, `start` and `pos`. A `FileHandle` as `options.fd` is kept
/// under [`handle_key`].
fn init_fields(
    scope: &Scope<'_>,
    stream: Object<'_>,
    path: HandleValue<'_>,
    options: Object<'_>,
    default_flags: &str,
) -> Result<(), Thrown> {
    let fd = options.val(scope, c"fd")?;
    if fd.is_null() || fd.is_undefined() {
        let path = super::path_arg(scope, path, "path")?;
        stream.set_property(scope, c"fd", js::value::null())?;
        stream.set_property(scope, c"path", path.as_str())?;
    } else {
        let fd = match super::promises::file_handle_fd(scope, fd)? {
            Some(handle_fd) => {
                stream.set_property_by_id(scope, handle_key(scope)?, fd)?;
                handle_fd
            }
            None => super::fd_arg(scope, fd)?,
        };
        stream.set_property(scope, c"fd", fd)?;
        stream.set_property(scope, c"path", js::value::undefined())?;
    }
    let flags = options.val(scope, c"flags")?;
    if flags.is_undefined() {
        stream.set_property(scope, c"flags", default_flags)?;
    } else {
        stream.set_property(scope, c"flags", flags)?;
    }
    let mode = options.val(scope, c"mode")?;
    if mode.is_undefined() {
        stream.set_property(scope, c"mode", 0o666)?;
    } else {
        stream.set_property(scope, c"mode", mode)?;
    }
    let start = options.val(scope, c"start")?;
    if !start.is_undefined() {
        validate_integer(scope, start, "start", 0.0, 9007199254740991.0)?;
    }
    stream.set_property(scope, c"start", start)?;
    stream.set_property(scope, c"pos", start)?;
    Ok(())
}

/// The `open` callback of `_construct`, whose captures are `[stream, callback]`: record the
/// descriptor, call the callback, and emit `open` and `ready`, or pass the error on.
fn opened(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let stream = captures.obj(0)?;
    let callback = captures.get(1)?;
    let error = args.get(0);
    if !error.is_null() && !error.is_undefined() {
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        return Ok(js::value::undefined());
    }
    let fd = args.get(1);
    stream.set_property(scope, c"fd", fd)?;
    Function::call(scope, HandleValue::undefined(), callback, EmptyArgs)?;
    call_method(scope, stream, c"emit", &[string(scope, "open")?, fd])?;
    call_method(scope, stream, c"emit", &[string(scope, "ready")?])?;
    Ok(js::value::undefined())
}

/// The `_construct` of both stream classes: open `this.path` unless `this.fd` is a descriptor.
fn construct(scope: &Scope<'_>, this: Object<'_>, callback: HandleValue<'_>) -> Result<(), Thrown> {
    if this.val(scope, c"fd")?.is_number() {
        Function::call(scope, HandleValue::undefined(), callback, EmptyArgs)?;
        return Ok(());
    }
    let args = [
        this.val(scope, c"path")?,
        this.val(scope, c"flags")?,
        this.val(scope, c"mode")?,
    ];
    let prepared = (ops::OPS[ops::index_of("open")].parse)(scope, &args, Variant::Callback)?;
    let done = closure(
        scope,
        2,
        opened,
        &[this.to_jsval_raw_throwing(scope)?, callback.get()],
    )?;
    run_callback(
        scope,
        "open",
        prepared,
        done.to_jsval_throwing(scope)?,
        true,
    )
}

/// The `close` callback of `_destroy`, whose captures are `[callback, error]`: call the callback
/// with the close error, or else the destroy error.
fn closed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let close_error = args.get(0);
    let error = if close_error.is_null() || close_error.is_undefined() {
        captures.get(1)?
    } else {
        close_error
    };
    Function::call(scope, HandleValue::undefined(), captures.get(0)?, &[error])?;
    Ok(js::value::undefined())
}

/// The `_destroy` of both stream classes: close the `FileHandle` or descriptor, if there is one,
/// and call `callback` with the first error.
fn destroy(
    scope: &Scope<'_>,
    this: Object<'_>,
    error: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(), Thrown> {
    let fd = this.val(scope, c"fd")?;
    if !fd.is_number() {
        Function::call(scope, js::value::undefined(), callback, &[error])?;
        return Ok(());
    }
    let done = closure(scope, 1, closed, &[callback.get(), error.get()])?;
    let handle = this.get_property_by_id(scope, handle_key(scope)?)?;
    if let Ok(handle) = Object::from_value(scope, *handle) {
        this.set_property(scope, c"fd", js::value::null())?;
        let closing = call_method(scope, handle, c"close", &[])?;
        let closing = Object::from_value(scope, *closing).map_err(|e| e.throw(scope))?;
        let done = done.to_jsval_throwing(scope)?;
        call_method(scope, closing, c"then", &[done, done])?;
        return Ok(());
    }
    let prepared = (ops::OPS[ops::index_of("close")].parse)(scope, &[fd], Variant::Callback)?;
    this.set_property(scope, c"fd", js::value::null())?;
    run_callback(
        scope,
        "close",
        prepared,
        done.to_jsval_throwing(scope)?,
        true,
    )
}

/// The getter of `pending`: whether the file isn't open yet.
fn pending(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this =
        Object::from_value(scope, scope.root_value(args.this())).map_err(|e| e.throw(scope))?;
    Ok(js::value::from_bool(this.val(scope, c"fd")?.is_null()))
}

/// Define the `pending` getter on `prototype`.
fn define_pending(scope: &Scope<'_>, prototype: Object<'_>) -> Result<(), ExnThrown> {
    let getter = Function::new_callback(scope, c"pending", 0, pending, ())?;
    prototype.define_accessor(scope, c"pending", Some(*getter), None, 0)
}

/// The `read` callback of `_read`, whose captures are `[stream]`: push the bytes read, or `null`
/// at the end of the file, or destroy the stream with the error.
fn was_read(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let error = args.get(0);
    if !error.is_null() && !error.is_undefined() {
        call_method(scope, stream, c"destroy", &[error])?;
        return Ok(js::value::undefined());
    }
    let count = args.get(1).to_number();
    if count <= 0.0 {
        call_method(scope, stream, c"push", &[HandleValue::null()])?;
        return Ok(js::value::undefined());
    }
    let total = stream.num(scope, c"bytesRead")?;
    stream.set_property(scope, c"bytesRead", total + count)?;
    let buffer = Object::from_value(scope, args.get(2)).map_err(|e| e.throw(scope))?;
    let zero = scope.root_value(js::value::from_f64(0.0));
    let end = scope.root_value(js::value::from_f64(count));
    let chunk = call_method(scope, buffer, c"subarray", &[zero, end])?;
    call_method(scope, stream, c"push", &[chunk])?;
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(
    name = "ReadStream",
    extends = "crate::stream::readable_class",
    self_construct
)]
pub mod read_stream_class {
    use super::*;

    /// <https://nodejs.org/api/fs.html#class-fsreadstream>: a stream of the file `path`, or of
    /// `options.fd`, from `options.start` through `options.end`.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        path: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let options = base_options(scope, options, Some(64.0 * 1024.0))?;
        init_fields(scope, this, path, options, "r")?;
        let end = options.val(scope, c"end")?;
        let end = if end.is_undefined() {
            f64::INFINITY
        } else {
            validate_integer(scope, end, "end", 0.0, f64::INFINITY)?
        };
        let start = this.val(scope, c"start")?;
        if start.is_number() && start.to_number() > end {
            return Err(NodeError::out_of_range(
                "start",
                &format!("<= \"end\" (here: {end})"),
                &start.to_number().to_string(),
            )
            .into());
        }
        this.set_property(scope, c"end", end)?;
        this.set_property(scope, c"bytesRead", 0)?;
        crate::stream::readable_class::init(scope, this, Some(options.to_jsval_throwing(scope)?))
    }

    #[method(name = "_construct")]
    pub fn construct_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        super::construct(scope, this, callback)
    }

    #[method(name = "_read")]
    pub fn read_impl(scope: &Scope<'_>, this: Object<'_>, size: f64) -> Result<(), Thrown> {
        let position = this.val(scope, c"pos")?;
        let end = this.num(scope, c"end")?;
        let size = if position.is_undefined() {
            size.min(end - this.num(scope, c"bytesRead")? + 1.0)
        } else {
            size.min(end - position.to_number() + 1.0)
        };
        if size <= 0.0 {
            call_method(scope, this, c"push", &[HandleValue::null()])?;
            return Ok(());
        }
        let buffer =
            crate::buffer::new_buffer(scope, &vec![0; size as usize])?.to_jsval_throwing(scope)?;
        let args = [
            this.val(scope, c"fd")?,
            buffer,
            scope.root_value(js::value::from_f64(0.0)),
            scope.root_value(js::value::from_f64(size)),
            if position.is_number() {
                position
            } else {
                HandleValue::null()
            },
        ];
        let prepared = (ops::OPS[ops::index_of("read")].parse)(scope, &args, Variant::Callback)?;
        if position.is_number() {
            this.set_property(scope, c"pos", position.to_number() + size)?;
        }
        let done = closure(scope, 3, was_read, &[this.to_jsval_raw_throwing(scope)?])?;
        run_callback(
            scope,
            "read",
            prepared,
            done.to_jsval_throwing(scope)?,
            true,
        )
    }

    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        super::destroy(scope, this, error, callback)
    }

    /// <https://nodejs.org/api/fs.html#readstreamclosecallback>
    pub fn close(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        if let Some(callback) = callback.filter(|callback| is_function(scope, *callback)) {
            crate::stream::end_of_stream(scope, this.to_jsval_throwing(scope)?, None, callback)
                .map_err(|thrown| thrown.throw(scope))?;
        }
        call_method(scope, this, c"destroy", &[])?;
        Ok(())
    }

    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        super::define_pending(scope, proto)
    }
}

/// The `write` callback of `_write`, whose captures are `[stream, callback]`: count the bytes
/// written and call the callback, or call it with the error.
fn written(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let stream = captures.obj(0)?;
    let callback = captures.get(1)?;
    let error = args.get(0);
    if !error.is_null() && !error.is_undefined() {
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        return Ok(js::value::undefined());
    }
    let total = stream.num(scope, c"bytesWritten")?;
    stream.set_property(scope, c"bytesWritten", total + args.get(1).to_number())?;
    Function::call(scope, HandleValue::undefined(), callback, EmptyArgs)?;
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(
    name = "WriteStream",
    extends = "crate::stream::writable_class",
    self_construct
)]
pub mod write_stream_class {
    use super::*;

    /// <https://nodejs.org/api/fs.html#class-fswritestream>: a stream to the file `path`, or to
    /// `options.fd`, from `options.start`.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        path: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let options = base_options(scope, options, None)?;
        options.set_property(scope, c"decodeStrings", true)?;
        init_fields(scope, this, path, options, "w")?;
        this.set_property(scope, c"bytesWritten", 0)?;
        crate::stream::writable_class::init(scope, this, Some(options.to_jsval_throwing(scope)?))
    }

    #[method(name = "_construct")]
    pub fn construct_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        super::construct(scope, this, callback)
    }

    #[method(name = "_write")]
    pub fn write_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        _encoding: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let position = this.val(scope, c"pos")?;
        let args = [
            this.val(scope, c"fd")?,
            chunk,
            HandleValue::null(),
            HandleValue::null(),
            if position.is_number() {
                position
            } else {
                HandleValue::null()
            },
        ];
        let prepared = (ops::OPS[ops::index_of("write")].parse)(scope, &args, Variant::Callback)?;
        if position.is_number() {
            let length = Object::from_value(scope, *chunk)
                .ok()
                .and_then(js::ArrayBufferView::from_object)
                .map_or(0, |view| view.byte_length());
            this.set_property(scope, c"pos", position.to_number() + length as f64)?;
        }
        let done = closure(
            scope,
            2,
            written,
            &[this.to_jsval_raw_throwing(scope)?, callback.get()],
        )?;
        run_callback(
            scope,
            "write",
            prepared,
            done.to_jsval_throwing(scope)?,
            true,
        )
    }

    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        super::destroy(scope, this, error, callback)
    }

    /// <https://nodejs.org/api/fs.html#writestreamclosecallback>: end the stream, and call
    /// `callback` once it closed.
    pub fn close(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        if let Some(callback) = callback.filter(|callback| is_function(scope, *callback)) {
            if this.flag(scope, c"closed")? {
                crate::stream::next_tick_call(scope, callback, &[])?;
                return Ok(());
            }
            call_method(scope, this, c"on", &[string(scope, "close")?, callback])?;
        }
        let state = this.obj(scope, c"_writableState")?;
        let auto_close = match state {
            Some(state) => state.flag(scope, c"autoDestroy")?,
            None => true,
        };
        if !auto_close {
            let destroy = this.val(scope, c"destroy")?;
            call_method(scope, this, c"on", &[string(scope, "finish")?, destroy])?;
        }
        call_method(scope, this, c"end", &[])?;
        Ok(())
    }

    /// <https://nodejs.org/api/fs.html#writestreamdestroysoon>
    pub fn destroy_soon(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        call_method(scope, this, c"end", &[])?;
        Ok(())
    }

    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        super::define_pending(scope, proto)
    }
}

/// A new stream of `class` for the `FileHandle` `handle`, with `options`. The stream closes the
/// handle.
pub(super) fn with_fd_stream<'s>(
    scope: &'s Scope<'_>,
    class: Object<'s>,
    handle: Object<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let copy = base_options(scope, options, None)?;
    copy.set_property(scope, c"fd", handle)?;
    Function::construct(
        scope,
        class,
        &[HandleValue::undefined(), copy.to_jsval_throwing(scope)?],
    )
}
