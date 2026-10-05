// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `fs.ReadStream` and `fs.WriteStream`, following Node.js v24's `lib/internal/fs/streams.js`.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs, EmptyArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::promises::{FileHandle, FileHandleImpl};
use super::{ops, run_callback, Prepared, Variant};
use crate::errors::{validate_integer, NodeError, Thrown};
use crate::stream::state::{StreamState, StreamStateImpl};
use crate::support::Props;

/// The payload of the `open` and `write` callbacks and of the `close` listener on a stream's
/// `FileHandle`: the stream, and the callback of its `_construct` or `_write`, or its `close`
/// method.
#[core_runtime::jsclass(hidden)]
pub(super) struct StreamRequest {
    stream: Heap<js::object::Object>,
    callback: Heap<Value>,
}

#[core_runtime::jsmethods]
impl StreamRequest {}

/// A new [`StreamRequest`] of `stream` with `callback`.
fn stream_request<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    callback: HandleValue<'_>,
) -> Result<StreamRequest<'s>, ExnThrown> {
    js::class::create_instance_with::<StreamRequestImpl>(scope, |_| StreamRequestImpl {
        stream: Heap::from(stream),
        callback: Heap::from(callback.get()),
    })
}

/// The payload of the `close` callback: the callback of `_destroy`, and the error the stream was
/// destroyed with.
#[core_runtime::jsclass(hidden)]
pub(super) struct CloseRequest {
    callback: Heap<Value>,
    error: Heap<Value>,
}

#[core_runtime::jsmethods]
impl CloseRequest {}

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
        auto_close.is_undefined() || js::conversion::to_boolean(auto_close),
    )?;
    Ok(copy)
}

/// The state of an `fs.ReadStream` or an `fs.WriteStream`: the state of the stream, the
/// `FileHandle` it reads or writes, which the stream closes, and whether a read or write is in
/// progress.
#[core_runtime::jsclass(hidden, extends = StreamState)]
pub(crate) struct FileStreamState {
    parent: StreamStateImpl,
    handle: Option<Heap<FileHandleImpl>>,
    #[no_trace]
    performing_io: bool,
    /// The `_destroy` that waits for the read or write in progress to finish.
    pending_close: Option<Heap<CloseRequestImpl>>,
}

#[core_runtime::jsmethods]
impl FileStreamState {}

/// The `close` listener on a stream's `FileHandle`, whose payload is a [`StreamRequest`]: call the
/// stream's `close` method.
fn handle_closed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = cast_payload::<StreamRequest>(scope, payload);
    let (stream, close) = {
        let data = request.data();
        (data.stream.get(scope), data.callback.get(scope))
    };
    Function::call(scope, stream, close, EmptyArgs)?;
    Ok(js::value::undefined())
}

/// Set the fields both stream classes share on `stream` from `path` and the `options` copy:
/// `fd`, `path`, `flags`, `mode`, `start` and `pos`. A `FileHandle` as `options.fd` is kept in
/// the stream's [`FileStreamState`], and as in Node.js's `importFd`, the stream references the
/// handle's descriptor and is closed when the handle emits `close`.
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
                let handle = Object::from_value(scope, *fd)
                    .map_err(|e| e.throw(scope))?
                    .cast::<FileHandle>()
                    .map_err(|e| js::error::TypeError(e.to_string()).throw(scope))?;
                read_stream_class::state_of(scope, stream)?
                    .data_mut()
                    .handle = Some(Heap::from(handle));
                handle.retain();
                let close = stream.get_property(scope, c"close")?;
                let request = stream_request(scope, stream, close)?;
                let listener = Function::new_callback(scope, c"", 0, handle_closed, request)?;
                let close = js::class::get_or_init_atom(scope, c"close")?;
                Function::call_by_name(
                    scope,
                    handle.handle(),
                    c"on",
                    &[close, (*listener).to_jsval_throwing(scope)?],
                )?;
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
        validate_integer(scope, start, "start", 0.0, crate::errors::MAX_SAFE_INTEGER)?;
    }
    stream.set_property(scope, c"start", start)?;
    stream.set_property(scope, c"pos", start)?;
    Ok(())
}

/// The `open` callback of `_construct`, whose payload is a [`StreamRequest`]: record the
/// descriptor, call the callback, and emit `open` and `ready`, or pass the error on.
fn opened(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = cast_payload::<StreamRequest>(scope, payload);
    let stream = request.data().stream.get(scope);
    let callback = request.data().callback.get(scope);
    let error = args.get(0);
    if !error.is_null() && !error.is_undefined() {
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        return Ok(js::value::undefined());
    }
    let fd = args.get(1);
    stream.set_property(scope, c"fd", fd)?;
    Function::call(scope, HandleValue::undefined(), callback, EmptyArgs)?;
    crate::events::emit(scope, stream, c"open", &[fd])?;
    crate::events::emit(scope, stream, c"ready", &[])?;
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
    let done = Function::new_callback(
        scope,
        c"",
        2,
        opened,
        stream_request(scope, this, callback)?,
    )?;
    run_callback(
        scope,
        "open",
        prepared,
        done.to_jsval_throwing(scope)?,
        true,
    )
}

/// The `close` callback of `_destroy`, whose payload is a [`CloseRequest`]: call the callback
/// with the close error, or else the destroy error.
fn closed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = cast_payload::<CloseRequest>(scope, payload);
    let close_error = args.get(0);
    let error = if close_error.is_null_or_undefined() {
        request.data().error.get(scope)
    } else {
        close_error
    };
    let callback = request.data().callback.get(scope);
    Function::call(scope, HandleValue::undefined(), callback, &[error])?;
    Ok(js::value::undefined())
}

/// The `_destroy` of both stream classes: once the read or write in progress, if there is one,
/// finished, close the `FileHandle` or descriptor, if there is one, and call `callback` with the
/// first error.
fn destroy(
    scope: &Scope<'_>,
    this: Object<'_>,
    error: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(), Thrown> {
    let state = read_stream_class::try_state_of(scope, this)?;
    if let Some(state) = state.filter(|state| state.data().performing_io) {
        let request =
            js::class::create_instance_with::<CloseRequestImpl>(scope, |_| CloseRequestImpl {
                callback: Heap::from(callback.get()),
                error: Heap::from(error.get()),
            })?;
        state.data_mut().pending_close = Some(Heap::from(request));
        return Ok(());
    }
    close(scope, this, error, callback)
}

/// Run the read or write `prepared` as `function` with the callback `done`, and record it as in
/// progress on `stream` until `done` runs. One that fails to start isn't recorded.
fn start_io(
    scope: &Scope<'_>,
    stream: Object<'_>,
    function: &str,
    prepared: Prepared,
    done: Function<'_>,
) -> Result<(), Thrown> {
    let state = read_stream_class::state_of(scope, stream)?;
    state.data_mut().performing_io = true;
    let started = run_callback(
        scope,
        function,
        prepared,
        done.to_jsval_throwing(scope)?,
        true,
    );
    if started.is_err() {
        state.data_mut().performing_io = false;
    }
    started
}

/// Record that the read or write in progress on `stream` finished. If the stream was destroyed,
/// run the `_destroy` that waited for it, with its error, or else `io_error`, and return `true`.
fn io_done(
    scope: &Scope<'_>,
    stream: Object<'_>,
    io_error: HandleValue<'_>,
) -> Result<bool, Thrown> {
    let state = read_stream_class::state_of(scope, stream)?;
    state.data_mut().performing_io = false;
    if !stream.flag(scope, c"destroyed")? {
        return Ok(false);
    }
    let pending = state
        .data_mut()
        .pending_close
        .take()
        .map(|request| request.take(scope));
    if let Some(request) = pending {
        let (error, callback) = {
            let data = request.data();
            (data.error.get(scope), data.callback.get(scope))
        };
        let error = if js::conversion::to_boolean(error) {
            error
        } else {
            io_error
        };
        close(scope, stream, error, callback)?;
    }
    Ok(true)
}

/// Close the `FileHandle` or descriptor of `this`, if there is one, and call `callback` with the
/// close error, or else `error`.
fn close(
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
    let request =
        js::class::create_instance_with::<CloseRequestImpl>(scope, |_| CloseRequestImpl {
            callback: Heap::from(callback.get()),
            error: Heap::from(error.get()),
        })?;
    let done = Function::new_callback(scope, c"", 1, closed, request)?;
    let handle = read_stream_class::try_state_of(scope, this)?
        .and_then(|state| state.data().handle.get(scope));
    if let Some(handle) = handle {
        // As Node.js's `FileHandleOperations.close`: release the stream's reference, then close
        // the handle.
        this.set_property(scope, c"fd", js::value::null())?;
        handle.release(scope)?;
        let closing = handle.close(scope)?;
        closing.add_reactions(scope, Some(*done), Some(*done))?;
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

/// The `read` callback of `_read`, whose payload is the stream: push the bytes read, or `null` at
/// the end of the file, or destroy the stream with the error. On a destroyed stream, run the
/// `_destroy` that waited for the read instead.
fn was_read(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<Object>(scope, payload);
    let error = args.get(0);
    if io_done(scope, stream, error).map_err(|thrown| thrown.throw(scope))? {
        return Ok(js::value::undefined());
    }
    if !error.is_null() && !error.is_undefined() {
        Function::call_by_name(scope, stream.handle(), c"destroy", &[error])?;
        return Ok(js::value::undefined());
    }
    let count = args.get(1).to_number();
    if count <= 0.0 {
        Function::call_by_name(scope, stream.handle(), c"push", &[HandleValue::null()])?;
        return Ok(js::value::undefined());
    }
    let total = stream.num(scope, c"bytesRead")?;
    stream.set_property(scope, c"bytesRead", total + count)?;
    let buffer = Object::from_value(scope, args.get(2)).map_err(|e| e.throw(scope))?;
    let zero = scope.root_value(js::value::from_f64(0.0));
    let end = scope.root_value(js::value::from_f64(count));
    let chunk = Function::call_by_name(scope, buffer.handle(), c"subarray", &[zero, end])?;
    Function::call_by_name(scope, stream.handle(), c"push", &[chunk])?;
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(
    name = "ReadStream",
    extends = "crate::stream::readable_class",
    state = "FileStreamState",
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
            Function::call_by_name(scope, this.handle(), c"push", &[HandleValue::null()])?;
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
        let done = Function::new_callback(scope, c"", 3, was_read, this)?;
        super::start_io(scope, this, "read", prepared, done)
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
        if let Some(callback) = callback.filter(|callback| js::function::is_callable(*callback)) {
            crate::stream::end_of_stream(scope, this.to_jsval_throwing(scope)?, None, callback)
                .map_err(|thrown| thrown.throw(scope))?;
        }
        Function::call_by_name(scope, this.handle(), c"destroy", js::function::EmptyArgs)?;
        Ok(())
    }

    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        super::define_pending(scope, proto)
    }
}

/// The `write` callback of `_write`, whose payload is a [`StreamRequest`]: count the bytes
/// written and call the callback, or call it with the error. On a destroyed stream, call the
/// callback with the error or else `ERR_STREAM_DESTROYED`, and run the `_destroy` that waited for
/// the write.
fn written(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = cast_payload::<StreamRequest>(scope, payload);
    let stream = request.data().stream.get(scope);
    let callback = request.data().callback.get(scope);
    let mut error = args.get(0);
    if stream.flag(scope, c"destroyed")? {
        if error.is_null_or_undefined() {
            error = crate::stream::stream_destroyed("write").to_value(scope)?;
        }
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        io_done(scope, stream, error).map_err(|thrown| thrown.throw(scope))?;
        return Ok(js::value::undefined());
    }
    read_stream_class::state_of(scope, stream)
        .map_err(|thrown| thrown.throw(scope))?
        .data_mut()
        .performing_io = false;
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
    state = "FileStreamState",
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
        let request = stream_request(scope, this, callback)?;
        let done = Function::new_callback(scope, c"", 2, written, request)?;
        super::start_io(scope, this, "write", prepared, done)
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
        if let Some(callback) = callback.filter(|callback| js::function::is_callable(*callback)) {
            if this.flag(scope, c"closed")? {
                crate::support::next_tick_call(scope, callback, &[])?;
                return Ok(());
            }
            Function::call_by_name(
                scope,
                this.handle(),
                c"on",
                &[js::class::get_or_init_atom(scope, c"close")?, callback],
            )?;
        }
        let auto_close = crate::stream::writable_auto_destroy(scope, this)?.unwrap_or(true);
        if !auto_close {
            let destroy = this.val(scope, c"destroy")?;
            Function::call_by_name(
                scope,
                this.handle(),
                c"on",
                &[js::class::get_or_init_atom(scope, c"finish")?, destroy],
            )?;
        }
        Function::call_by_name(scope, this.handle(), c"end", js::function::EmptyArgs)?;
        Ok(())
    }

    /// <https://nodejs.org/api/fs.html#writestreamdestroysoon>
    pub fn destroy_soon(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        Function::call_by_name(scope, this.handle(), c"end", js::function::EmptyArgs)?;
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
