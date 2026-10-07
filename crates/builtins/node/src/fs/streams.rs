// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `fs.ReadStream` and `fs.WriteStream`, following Node.js v24's `lib/internal/fs/streams.js`.

use js::conversion::{FromJSVal, ToJSVal};
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

/// The payload of the `open` and `write` callbacks, of the `close` listener on a stream's
/// `FileHandle` and of the `emit` that `_construct` installs for a replaced `open` method: the
/// stream, the callback of its `_construct` or `_write` or its `close` method, and for the
/// installed `emit`, the stream's previous `emit`.
#[core_runtime::jsclass(hidden)]
pub(super) struct StreamRequest {
    stream: Heap<js::object::Object>,
    callback: Heap<Value>,
    emit: Heap<Value>,
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
        emit: Heap::from(js::value::undefined()),
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
/// `FileHandle` it reads or writes, which the stream closes, whether a read or write is in
/// progress, and the values Node.js keeps in the stream's own properties, which accessors on the
/// prototypes read and assign (see [`Field`]).
#[core_runtime::jsclass(hidden, extends = StreamState)]
pub(crate) struct FileStreamState {
    parent: StreamStateImpl,
    handle: Option<Heap<FileHandleImpl>>,
    #[no_trace]
    performing_io: bool,
    /// The `_destroy` that waits for the read or write in progress to finish.
    pending_close: Option<Heap<CloseRequestImpl>>,
    fd: Heap<Value>,
    path: Heap<Value>,
    flags: Heap<Value>,
    mode: Heap<Value>,
    start: Heap<Value>,
    pos: Heap<Value>,
    end: Heap<Value>,
    /// `bytesRead` of a `ReadStream`, and `bytesWritten` of a `WriteStream`.
    transferred: Heap<Value>,
}

/// A value of [`FileStreamState`] that Node.js keeps in an own property of a file stream.
#[derive(Clone, Copy)]
enum Field {
    /// The descriptor: a number once the file is open, and `null` before and after.
    Fd,
    /// The path the stream opens, or `undefined` for a stream of a descriptor.
    Path,
    /// The flags the file is opened with.
    Flags,
    /// The mode the file is opened with.
    Mode,
    /// The `start` option.
    Start,
    /// The position of the next read or write, or `undefined` for the file's position.
    Pos,
    /// The position of the last byte a `ReadStream` reads.
    End,
    /// The count of bytes read or written.
    Transferred,
}

impl FileStreamStateImpl {
    fn field(&self, field: Field) -> &Heap<Value> {
        match field {
            Field::Fd => &self.fd,
            Field::Path => &self.path,
            Field::Flags => &self.flags,
            Field::Mode => &self.mode,
            Field::Start => &self.start,
            Field::Pos => &self.pos,
            Field::End => &self.end,
            Field::Transferred => &self.transferred,
        }
    }
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

/// The value of `field` of the file stream `stream`.
fn field_value<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    field: Field,
) -> Result<HandleValue<'s>, ExnThrown> {
    Ok(read_stream_class::state_of(scope, stream)?
        .data()
        .field(field)
        .get(scope))
}

/// Set `field` of the file stream `stream` to `value`.
fn set_field<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    field: Field,
    value: impl ToJSVal<'s>,
) -> Result<(), ExnThrown> {
    let value = value.to_jsval_throwing(scope)?;
    read_stream_class::state_of(scope, stream)?
        .data()
        .field(field)
        .set(value.get());
    Ok(())
}

/// Add `count` to the bytes `stream` read or wrote, as Node.js's `+=` adds it to a number.
fn add_transferred(scope: &Scope<'_>, stream: Object<'_>, count: f64) -> Result<(), ExnThrown> {
    let total = field_value(scope, stream, Field::Transferred)?;
    let total = f64::from_jsval_throwing(scope, total, ())? + count;
    set_field(scope, stream, Field::Transferred, total)
}

/// The accessors of the file stream prototypes, each a property name and the field it reads and
/// assigns.
const ACCESSORS: [(&std::ffi::CStr, Field); 9] = [
    (c"fd", Field::Fd),
    (c"path", Field::Path),
    (c"flags", Field::Flags),
    (c"mode", Field::Mode),
    (c"start", Field::Start),
    (c"end", Field::End),
    (c"pos", Field::Pos),
    (c"bytesRead", Field::Transferred),
    (c"bytesWritten", Field::Transferred),
];

/// The [`ACCESSORS`] of `ReadStream.prototype` and of `WriteStream.prototype`.
const READ_ACCESSORS: [usize; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
const WRITE_ACCESSORS: [usize; 7] = [0, 1, 2, 3, 4, 6, 8];

/// The getter of an accessor, whose payload is its index in [`ACCESSORS`]: the field of `this`,
/// or `undefined` for an object without a file stream's state.
fn get_accessor(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    let (_, field) = ACCESSORS[payload.to_number() as usize];
    let Some(state) = read_stream_class::try_state_of(scope, this)? else {
        return Ok(js::value::undefined());
    };
    let value = state.data().field(field).get(scope);
    Ok(value.get())
}

/// The setter of an accessor, whose payload is its index in [`ACCESSORS`]: set the field of
/// `this` to the value. An object without a file stream's state gets an own data property.
fn set_accessor(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    let (name, field) = ACCESSORS[payload.to_number() as usize];
    let value = args.get(0);
    match read_stream_class::try_state_of(scope, this)? {
        Some(state) => state.data().field(field).set(value.get()),
        None => {
            let attrs = js::class_spec::JSPROP_ENUMERATE as u32;
            this.define_property(scope, name, value, attrs)?;
        }
    }
    Ok(js::value::undefined())
}

/// Define the accessors `indices` of [`ACCESSORS`] on `prototype`, and the deprecated `open`
/// method, which `open` implements.
fn define_members(
    scope: &Scope<'_>,
    prototype: Object<'_>,
    indices: &[usize],
    open: js::function::Callback,
) -> Result<(), ExnThrown> {
    for &index in indices {
        let (name, _) = ACCESSORS[index];
        let payload = index as f64;
        let getter = Function::new_callback(scope, name, 0, get_accessor, payload)?;
        let setter = Function::new_callback(scope, name, 1, set_accessor, payload)?;
        let attrs = js::class_spec::JSPROP_ENUMERATE as u32;
        prototype.define_accessor(scope, name, Some(*getter), Some(*setter), attrs)?;
    }
    let open = crate::support::shared_named_function(scope, open, c"deprecated", 0)?;
    let attrs = js::class_spec::JSPROP_ENUMERATE as u32;
    prototype.define_property(scope, c"open", open, attrs)
}

/// `ReadStream.prototype.open()`, which does nothing but emit the deprecation warning DEP0135.
fn open_read(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = "ReadStream.prototype.open() is deprecated";
    crate::process::deprecate_once(scope, message, "DEP0135")?;
    Ok(js::value::undefined())
}

/// `WriteStream.prototype.open()`, which does nothing but emit the deprecation warning DEP0135.
fn open_write(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = "WriteStream.prototype.open() is deprecated";
    crate::process::deprecate_once(scope, message, "DEP0135")?;
    Ok(js::value::undefined())
}

/// Set the fields both stream classes share on `stream` from `path` and the `options` copy:
/// `fd`, `path`, `flags`, `mode`, `start` and `pos`. Returns `options.start`, which is `undefined`
/// or a validated integer. A `FileHandle` as `options.fd` is kept in the stream's
/// [`FileStreamState`], and as in Node.js's `importFd`, the stream references the handle's
/// descriptor and is closed when the handle emits `close`.
fn init_fields<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    path: HandleValue<'_>,
    options: Object<'_>,
    default_flags: &str,
) -> Result<HandleValue<'s>, Thrown> {
    let fd = options.val(scope, c"fd")?;
    if fd.is_null() || fd.is_undefined() {
        let path = super::path_arg(scope, path, "path")?;
        let path = js::JSString::from_str(scope, &path)?.to_jsval_throwing(scope)?;
        set_field(scope, stream, Field::Fd, HandleValue::null())?;
        set_field(scope, stream, Field::Path, path)?;
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
        set_field(scope, stream, Field::Fd, f64::from(fd))?;
        set_field(scope, stream, Field::Path, HandleValue::undefined())?;
    }
    let mut flags = options.val(scope, c"flags")?;
    if flags.is_undefined() {
        flags = js::JSString::from_str(scope, default_flags)?.to_jsval_throwing(scope)?;
    }
    set_field(scope, stream, Field::Flags, flags)?;
    let mode = options.val(scope, c"mode")?;
    if mode.is_undefined() {
        set_field(scope, stream, Field::Mode, f64::from(0o666))?;
    } else {
        set_field(scope, stream, Field::Mode, mode)?;
    }
    let start = options.val(scope, c"start")?;
    if !start.is_undefined() {
        validate_integer(scope, start, "start", 0.0, crate::errors::MAX_SAFE_INTEGER)?;
    }
    set_field(scope, stream, Field::Start, start)?;
    set_field(scope, stream, Field::Pos, start)?;
    Ok(start)
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
    set_field(scope, stream, Field::Fd, args.get(1))?;
    Function::call(scope, HandleValue::undefined(), callback, EmptyArgs)?;
    let fd = field_value(scope, stream, Field::Fd)?;
    crate::events::emit(scope, stream, c"open", &[fd])?;
    crate::events::emit(scope, stream, c"ready", &[])?;
    Ok(js::value::undefined())
}

/// The `emit` that `_construct` installs on a stream whose `open` method was replaced, whose
/// payload is a [`StreamRequest`]: an `open` event restores the stream's `emit`, calls the
/// callback of `_construct`, and is emitted. An `error` event restores the `emit` and calls the
/// callback with the error. Other events are emitted.
fn replaced_open_emit(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = cast_payload::<StreamRequest>(scope, payload);
    let (callback, emit) = {
        let data = request.data();
        (data.callback.get(scope), data.emit.get(scope))
    };
    let this = scope.root_value(args.this());
    let event = args.get(0);
    let is_open = crate::support::is_str(scope, event, c"open")?;
    if is_open || crate::support::is_str(scope, event, c"error")? {
        let this_object = Object::from_value(scope, *this).map_err(|e| e.throw(scope))?;
        this_object.set_property(scope, c"emit", emit)?;
        if !is_open {
            Function::call(scope, HandleValue::undefined(), callback, &[args.get(1)])?;
            return Ok(js::value::undefined());
        }
        Function::call(scope, HandleValue::undefined(), callback, EmptyArgs)?;
    }
    let values = args.rest(0);
    Function::call(scope, this, emit, &values)?;
    Ok(js::value::undefined())
}

/// The `_construct` of both stream classes. A stream with a descriptor is constructed at once. A
/// stream whose `open` method isn't the prototypes' is opened by calling it, with an `emit` that
/// completes the construction on the `open` or `error` event it emits, as in Node.js. Otherwise
/// the stream's path is opened.
fn construct(scope: &Scope<'_>, this: Object<'_>, callback: HandleValue<'_>) -> Result<(), Thrown> {
    if field_value(scope, this, Field::Fd)?.is_number() {
        Function::call(scope, HandleValue::undefined(), callback, EmptyArgs)?;
        return Ok(());
    }
    let open = this.get_property(scope, c"open")?;
    let originals = [
        crate::support::shared_named_function(scope, open_read, c"deprecated", 0)?,
        crate::support::shared_named_function(scope, open_write, c"deprecated", 0)?,
    ];
    let replaced = match Object::from_value(scope, *open) {
        Ok(open) => originals
            .iter()
            .all(|original| original.as_raw() != open.as_raw()),
        Err(_) => true,
    };
    if replaced {
        let request = stream_request(scope, this, callback)?;
        request
            .data()
            .emit
            .set(this.get_property(scope, c"emit")?.get());
        let emit = Function::new_callback(scope, c"", 0, replaced_open_emit, request)?;
        this.set_property(scope, c"emit", *emit)?;
        Function::call_by_name(scope, this.handle(), c"open", EmptyArgs)?;
        return Ok(());
    }
    let args = [
        field_value(scope, this, Field::Path)?,
        field_value(scope, this, Field::Flags)?,
        field_value(scope, this, Field::Mode)?,
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
    let fd = field_value(scope, this, Field::Fd)?;
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
        set_field(scope, this, Field::Fd, HandleValue::null())?;
        handle.release(scope)?;
        let closing = handle.close(scope)?;
        closing.add_reactions(scope, Some(*done), Some(*done))?;
        return Ok(());
    }
    let prepared = (ops::OPS[ops::index_of("close")].parse)(scope, &[fd], Variant::Callback)?;
    set_field(scope, this, Field::Fd, HandleValue::null())?;
    run_callback(
        scope,
        "close",
        prepared,
        done.to_jsval_throwing(scope)?,
        true,
    )
}

/// The getter of `pending`: whether the stream's descriptor is `null`, as it is before the file is
/// open and once it is closed.
fn pending(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this =
        Object::from_value(scope, scope.root_value(args.this())).map_err(|e| e.throw(scope))?;
    let fd = field_value(scope, this, Field::Fd)?;
    Ok(js::value::from_bool(fd.is_null()))
}

/// Define the `pending` getter, the accessors `indices` of [`ACCESSORS`] and the deprecated
/// `open` method implemented by `open` on `prototype`.
fn define_prototype(
    scope: &Scope<'_>,
    prototype: Object<'_>,
    indices: &[usize],
    open: js::function::Callback,
) -> Result<(), ExnThrown> {
    let getter = Function::new_callback(scope, c"pending", 0, pending, ())?;
    prototype.define_accessor(scope, c"pending", Some(*getter), None, 0)?;
    define_members(scope, prototype, indices, open)
}

/// The `read` callback of `_read`, whose payload is the stream and whose arguments are the error,
/// the count and a `Buffer` of the bytes read: advance the position, unless it is `undefined`, and
/// push the bytes, or `null` at the end of the file, or destroy the stream with the error. On a
/// destroyed stream, run the `_destroy` that waited for the read instead.
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
    let position = field_value(scope, stream, Field::Pos)?;
    if !position.is_undefined() {
        let position = f64::from_jsval_throwing(scope, position, ())? + count;
        set_field(scope, stream, Field::Pos, position)?;
    }
    add_transferred(scope, stream, count)?;
    Function::call_by_name(scope, stream.handle(), c"push", &[args.get(2)])?;
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
        let start = init_fields(scope, this, path, options, "r")?;
        let end = options.val(scope, c"end")?;
        // As in Node.js, `Infinity` is the one end that isn't a safe integer.
        let end = if end.is_undefined() || (end.is_number() && end.to_number() == f64::INFINITY) {
            f64::INFINITY
        } else {
            validate_integer(scope, end, "end", 0.0, crate::errors::MAX_SAFE_INTEGER)?
        };
        if start.is_number() && start.to_number() > end {
            let end = crate::errors::number_to_string(end);
            return Err(NodeError::out_of_range(
                "start",
                &format!("<= \"end\" (here: {end})"),
                &crate::errors::out_of_range_received(start.to_number()),
            )
            .into());
        }
        super::set_field(scope, this, Field::End, end)?;
        super::set_field(scope, this, Field::Transferred, 0.0)?;
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
        let position = super::field_value(scope, this, Field::Pos)?;
        let end = super::field_value(scope, this, Field::End)?;
        let end = f64::from_jsval_throwing(scope, end, ())?;
        let start = if position.is_undefined() {
            None
        } else {
            Some(f64::from_jsval_throwing(scope, position, ())?)
        };
        let limit = match start {
            None => {
                let bytes_read = super::field_value(scope, this, Field::Transferred)?;
                end - f64::from_jsval_throwing(scope, bytes_read, ())? + 1.0
            }
            Some(start) => end - start + 1.0,
        };
        // The size is `NaN` if `limit` is, as `Math.min` computes it.
        let size = if limit.is_nan() {
            limit
        } else {
            size.min(limit)
        };
        if size <= 0.0 {
            Function::call_by_name(scope, this.handle(), c"push", &[HandleValue::null()])?;
            return Ok(());
        }
        // Node.js allocates the buffer for the read here, which fails for a size of `NaN`.
        if size.is_nan() {
            let range = ">= 0 && <= 9007199254740991";
            return Err(NodeError::out_of_range("size", range, "NaN").into());
        }
        let fd = super::field_value(scope, this, Field::Fd)?;
        let prepared = ops::read_chunk(scope, fd, size as usize, position)?;
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
        super::define_prototype(scope, proto, &super::READ_ACCESSORS, super::open_read)
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
    add_transferred(scope, stream, args.get(1).to_number())?;
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
        super::set_field(scope, this, Field::Transferred, 0.0)?;
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
        let position = super::field_value(scope, this, Field::Pos)?;
        let args = [
            super::field_value(scope, this, Field::Fd)?,
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
            let position = position.to_number() + length as f64;
            super::set_field(scope, this, Field::Pos, position)?;
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
        super::define_prototype(scope, proto, &super::WRITE_ACCESSORS, super::open_write)
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
