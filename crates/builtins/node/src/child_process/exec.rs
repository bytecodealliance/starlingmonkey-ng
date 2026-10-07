// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `exec` and `execFile`, which collect a child process's output and call back with it, and their
//! `util.promisify.custom` versions. Ported from Node.js's `lib/child_process.js`.

use std::time::Duration;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::options;
use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};

/// Node.js's default `maxBuffer`, 1 MiB.
pub(super) const MAX_BUFFER: f64 = 1024.0 * 1024.0;

/// The state of an `execFile`: the child process, the callback, and the output collected so far.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ExecState {
    child: Option<Heap<js::object::Object>>,
    callback: Option<Heap<js::object::Object>>,
    /// The encoding of the output, or `None` for `Buffer`s.
    #[no_trace]
    encoding: Option<String>,
    /// The chunks of standard output and of standard error.
    stdout: Vec<Heap<Value>>,
    stderr: Vec<Heap<Value>>,
    /// The bytes of standard output and of standard error.
    #[no_trace]
    lengths: [f64; 2],
    #[no_trace]
    max_buffer: f64,
    #[no_trace]
    kill_signal: Option<i32>,
    /// Whether the output limit or the timeout killed the child.
    #[no_trace]
    killed: bool,
    #[no_trace]
    exited: bool,
    /// The ID of the timeout's timer, while it is pending.
    #[no_trace]
    timer: Option<u64>,
    /// The error the callback gets.
    error: Option<Heap<Value>>,
    /// The command: the file, followed by the arguments.
    #[no_trace]
    file: String,
    args: Vec<Heap<Value>>,
}

#[core_runtime::jsmethods]
impl ExecState {}

/// Node.js's `normalizeExecFileArgs`: the arguments, options and callback of `execFile(file,
/// args, options, callback)`, where `args` and `options` can each be left out.
pub(super) fn normalize_exec_file_args<'s>(
    scope: &'s Scope<'_>,
    args: HandleValue<'_>,
    options: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(HandleValue<'s>, Option<Object<'s>>, HandleValue<'s>), Thrown> {
    let mut args = scope.root_value(args.get());
    let mut options = scope.root_value(options.get());
    let mut callback = scope.root_value(callback.get());
    if js::Array::is_array_value(scope, args)? {
    } else if args.is_object() && !js::function::is_callable(args) {
        callback = options;
        options = args;
        args = HandleValue::null();
    } else if js::function::is_callable(args) {
        callback = args;
        options = HandleValue::null();
        args = HandleValue::null();
    }
    if args.is_null_or_undefined() {
        args =
            js::Array::with_contents(scope, &[] as &[HandleValue<'_>])?.to_jsval_throwing(scope)?;
    }
    let options = if js::function::is_callable(options) {
        callback = options;
        None
    } else if !options.is_null_or_undefined() {
        Some(crate::errors::validate_plain_object(
            scope, options, "options", false,
        )?)
    } else {
        None
    };
    if !callback.is_null_or_undefined() {
        validate_function(scope, callback, "callback")?;
    }
    if let Some(options) = options {
        let argv0 = options.get_property(scope, c"argv0")?;
        if !argv0.is_null_or_undefined() {
            crate::errors::validate_string(scope, argv0, "options.argv0")?;
            options::null_check(scope, argv0, "options.argv0")?;
        }
    }
    Ok((args, options, callback))
}

/// `child_process.execFile(file, args, options, callback)`, as Node.js's `execFile`: spawn
/// `file`, collect its standard output and standard error in `options.encoding`, and call
/// `callback` with the error, the output and the error output once the child closes. Output
/// beyond `options.maxBuffer` bytes per stream kills the child, and so does `options.timeout`.
pub(super) fn exec_file<'s>(
    scope: &'s Scope<'_>,
    file: HandleValue<'_>,
    args: HandleValue<'_>,
    options: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let (args, given, callback) = normalize_exec_file_args(scope, args, options, callback)?;
    let options = Object::new_with_null_prototype(scope)?;
    options.set_property(scope, c"encoding", "utf8")?;
    options.set_property(scope, c"timeout", 0)?;
    options.set_property(scope, c"maxBuffer", MAX_BUFFER)?;
    options.set_property(scope, c"killSignal", "SIGTERM")?;
    options.set_property(scope, c"cwd", HandleValue::null())?;
    options.set_property(scope, c"env", HandleValue::null())?;
    options.set_property(scope, c"shell", false)?;
    if let Some(given) = given {
        options.assign(scope, given)?;
    }
    let timeout = options::timeout(scope, options.get_property(scope, c"timeout")?)?;
    let max_buffer = options::max_buffer(scope, options.get_property(scope, c"maxBuffer")?)?;
    let kill_signal = options::kill_signal(scope, options.get_property(scope, c"killSignal")?)?;
    let spawn_options = Object::new_plain(scope)?;
    for name in [c"cwd", c"env", c"gid", c"shell", c"signal", c"uid"] {
        spawn_options.set_property(scope, name, options.get_property(scope, name)?)?;
    }
    for name in [c"windowsHide", c"windowsVerbatimArguments"] {
        let value = js::conversion::to_boolean(options.get_property(scope, name)?);
        spawn_options.set_property(scope, name, value)?;
    }
    let child = super::spawn(scope, file, args, spawn_options.to_jsval_throwing(scope)?)?;
    let encoding = options.get_property(scope, c"encoding")?;
    let encoding = crate::support::rust_string(scope, encoding)
        .filter(|name| name != "buffer" && crate::buffer::Encoding::parse(name).is_some());
    let file = crate::errors::js_string(scope, file)?;
    let args = crate::support::array_elements(scope, args)?.unwrap_or_default();
    let callback = Object::from_value(scope, *callback).ok();
    let state = js::class::create_instance_with::<ExecStateImpl>(scope, |_| ExecStateImpl {
        child: Some(Heap::from(child)),
        callback: callback.map(Heap::from),
        encoding: encoding.clone(),
        stdout: Vec::new(),
        stderr: Vec::new(),
        lengths: [0.0, 0.0],
        max_buffer: max_buffer.unwrap_or(MAX_BUFFER),
        kill_signal,
        killed: false,
        exited: false,
        timer: None,
        error: None,
        file,
        args: args.iter().map(|arg| Heap::from(arg.get())).collect(),
    })?;
    if let Some(timeout) = timeout.filter(|timeout| *timeout > 0.0) {
        let callback = Function::new_callback(scope, c"delayedKill", 0, delayed_kill, state)?;
        let callable = js::Callable::from_jsval(scope, callback.to_jsval_throwing(scope)?, ())
            .map_err(|e| e.throw(scope))?;
        let id = core_runtime::event_loop::timer::start_timer(
            scope,
            callable,
            HandleValue::undefined(),
            &[],
            Duration::from_millis(timeout as u64),
            false,
            None,
        )
        .ok_or_else(|| core_runtime::event_loop::throw_no_event_loop(scope, "execFile"))?;
        state.data_mut().timer = Some(id);
    }
    for (name, listener_name, body) in [
        (
            c"stdout",
            c"onChildStdout",
            on_child_stdout as js::function::Callback,
        ),
        (c"stderr", c"onChildStderr", on_child_stderr),
    ] {
        let Ok(stream) = Object::from_value(scope, *child.get_property(scope, name)?) else {
            continue;
        };
        if let Some(encoding) = &encoding {
            let encoding = encoding.as_str().to_jsval_throwing(scope)?;
            Function::call_by_name(scope, stream.handle(), c"setEncoding", &[encoding])?;
        }
        let listener = Function::new_callback(scope, listener_name, 1, body, state)?;
        crate::events::listen(scope, stream, c"on", c"data", *listener)?;
    }
    let exit = Function::new_callback(scope, c"exithandler", 2, exit_handler, state)?;
    crate::events::listen(scope, child, c"addListener", c"close", *exit)?;
    let error = Function::new_callback(scope, c"errorhandler", 1, error_handler, state)?;
    crate::events::listen(scope, child, c"addListener", c"error", *error)?;
    Ok(child)
}

/// The child process of the `execFile` of `state`, and its stream `name`, if it has one.
fn stream<'s>(
    scope: &'s Scope<'_>,
    state: &ExecState<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let child = state.data().child.as_ref().map(|child| child.get(scope));
    let Some(child) = child else {
        return Ok(None);
    };
    Ok(Object::from_value(scope, *child.get_property(scope, name)?).ok())
}

/// Destroy the standard output and standard error of the child of `state`.
fn destroy_streams(scope: &Scope<'_>, state: &ExecState<'_>) -> Result<(), ExnThrown> {
    for name in [c"stdout", c"stderr"] {
        if let Some(stream) = stream(scope, state, name)? {
            Function::call_by_name(scope, stream.handle(), c"destroy", js::function::EmptyArgs)?;
        }
    }
    Ok(())
}

/// The `kill` of Node.js's `execFile`: destroy the streams, and kill the child with the kill
/// signal. An exception of `kill` becomes the error, and the callback is called with it.
fn kill(scope: &Scope<'_>, state: ExecState<'_>) -> Result<(), ExnThrown> {
    destroy_streams(scope, &state)?;
    let (child, signal) = {
        let mut data = state.data_mut();
        data.killed = true;
        (
            data.child.as_ref().map(|child| child.get(scope)),
            data.kill_signal,
        )
    };
    let Some(child) = child else {
        return Ok(());
    };
    let signal = match signal {
        Some(signal) => scope.root_value(js::value::from_f64(f64::from(signal))),
        None => HandleValue::undefined(),
    };
    if Function::call_by_name(scope, child.handle(), c"kill", &[signal]).is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        state.data_mut().error = Some(Heap::from(error.get()));
        exit(
            scope,
            state,
            HandleValue::undefined(),
            HandleValue::undefined(),
        )?;
    }
    Ok(())
}

/// The timeout's timer callback, whose payload is the [`ExecState`].
fn delayed_kill(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = js::function::cast_payload::<ExecState>(scope, payload);
    kill(scope, state)?;
    state.data_mut().timer = None;
    Ok(js::value::undefined())
}

/// The `data` listener of standard output, whose payload is the [`ExecState`].
fn on_child_stdout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    collect(
        scope,
        js::function::cast_payload::<ExecState>(scope, payload),
        0,
        args.get(0),
    )
}

/// The `data` listener of standard error, whose payload is the [`ExecState`].
fn on_child_stderr(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    collect(
        scope,
        js::function::cast_payload::<ExecState>(scope, payload),
        1,
        args.get(0),
    )
}

/// Collect `chunk` of the stream `index`, 0 for standard output and 1 for standard error. A chunk
/// that takes the stream beyond `maxBuffer` bytes is cut to the limit, and kills the child with
/// `ERR_CHILD_PROCESS_STDIO_MAXBUFFER`.
fn collect(
    scope: &Scope<'_>,
    state: ExecState<'_>,
    index: usize,
    chunk: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let max_buffer = state.data().max_buffer;
    let push = |chunk: HandleValue<'_>| {
        let mut data = state.data_mut();
        let chunks = if index == 0 {
            &mut data.stdout
        } else {
            &mut data.stderr
        };
        chunks.push(Heap::from(chunk.get()));
    };
    if max_buffer == f64::INFINITY {
        push(chunk);
        return Ok(js::value::undefined());
    }
    let name = if index == 0 { c"stdout" } else { c"stderr" };
    let encoding = match stream(scope, &state, name)? {
        Some(stream) => stream.get_property(scope, c"readableEncoding")?,
        None => HandleValue::null(),
    };
    let length = if js::conversion::to_boolean(encoding) {
        crate::buffer::buffer::byte_length(scope, chunk, Some(encoding))
            .map_err(|thrown| thrown.throw(scope))?
    } else {
        let length = Object::from_value(scope, *chunk)
            .map(|chunk| chunk.get_property(scope, c"length"))
            .unwrap_or(Ok(HandleValue::undefined()))?;
        if length.is_number() {
            length.to_number()
        } else {
            0.0
        }
    };
    let total = {
        let mut data = state.data_mut();
        data.lengths[index] += length;
        data.lengths[index]
    };
    if total <= max_buffer {
        push(chunk);
        return Ok(js::value::undefined());
    }
    let kept = max_buffer - (total - length);
    let cut = if chunk.is_string() {
        let text = js::JSString::from_value(scope, chunk)?;
        let kept = (kept.max(0.0) as usize).min(text.len());
        text.substring(scope, 0, kept)?.to_jsval_throwing(scope)?
    } else {
        let chunk = Object::from_value(scope, *chunk).map_err(|e| e.throw(scope))?;
        let kept = scope.root_value(js::value::from_f64(kept));
        let zero = scope.root_value(js::value::from_f64(0.0));
        Function::call_by_name(scope, chunk.handle(), c"slice", &[zero, kept])?
    };
    push(cut);
    let label = if index == 0 { "stdout" } else { "stderr" };
    let error = NodeError::new(
        ErrorKind::RangeError,
        "ERR_CHILD_PROCESS_STDIO_MAXBUFFER",
        format!("{label} maxBuffer length exceeded"),
    )
    .to_value(scope)?;
    state.data_mut().error = Some(Heap::from(error.get()));
    kill(scope, state)?;
    Ok(js::value::undefined())
}

/// The `close` listener of the child, whose payload is the [`ExecState`] and whose arguments
/// are the exit code and the signal, as Node.js's `exithandler`.
fn exit_handler(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = js::function::cast_payload::<ExecState>(scope, payload);
    exit(scope, state, args.get(0), args.get(1))?;
    Ok(js::value::undefined())
}

/// The `error` listener of the child, whose payload is the [`ExecState`], as Node.js's
/// `errorhandler`: the error becomes the callback's, and the streams are destroyed.
fn error_handler(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = js::function::cast_payload::<ExecState>(scope, payload);
    state.data_mut().error = Some(Heap::from(args.get(0).get()));
    destroy_streams(scope, &state)?;
    exit(
        scope,
        state,
        HandleValue::undefined(),
        HandleValue::undefined(),
    )?;
    Ok(js::value::undefined())
}

/// The output `chunks` joined: a string for an encoding, and a `Buffer` otherwise.
fn joined<'s>(
    scope: &'s Scope<'_>,
    chunks: &[HandleValue<'_>],
    strings: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    if strings {
        let mut text = js::JSString::from_str(scope, "")?;
        for chunk in chunks {
            let chunk = js::JSString::from_value(scope, *chunk)?;
            text = text.concat(scope, chunk)?;
        }
        return text.to_jsval_throwing(scope);
    }
    let list = js::Array::with_contents(scope, chunks)?.to_jsval_throwing(scope)?;
    crate::buffer::buffer::concat(scope, list, None)
        .map_err(|thrown| thrown.throw(scope))?
        .to_jsval_throwing(scope)
}

/// Node.js's `exithandler`: once, clear the timeout, and call the callback with `null`, the
/// output and the error output after an exit with code 0, and otherwise with the error, which is
/// "Command failed" with `code`, `killed`, `signal` and `cmd` unless an error came before.
fn exit(
    scope: &Scope<'_>,
    state: ExecState<'_>,
    code: HandleValue<'_>,
    signal: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let (timer, callback) = {
        let mut data = state.data_mut();
        if data.exited {
            return Ok(());
        }
        data.exited = true;
        let callback = data.callback.as_ref().map(|callback| callback.get(scope));
        (data.timer.take(), callback)
    };
    if let Some(id) = timer {
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.clear_js_timer(id)
        });
    }
    let Some(callback) = callback else {
        return Ok(());
    };
    let (encoding, stdout, stderr, error, file, args, child, killed) = {
        let data = state.data();
        let root = |chunks: &[Heap<Value>]| -> Vec<HandleValue<'_>> {
            chunks.iter().map(|chunk| chunk.get(scope)).collect()
        };
        (
            data.encoding.is_some(),
            root(&data.stdout),
            root(&data.stderr),
            data.error.as_ref().map(|error| error.get(scope)),
            data.file.clone(),
            root(&data.args),
            data.child.as_ref().map(|child| child.get(scope)),
            data.killed,
        )
    };
    let decoded = |name| -> Result<bool, ExnThrown> {
        if encoding {
            return Ok(true);
        }
        let Some(child) = child else {
            return Ok(false);
        };
        let Ok(stream) = Object::from_value(scope, *child.get_property(scope, name)?) else {
            return Ok(false);
        };
        Ok(js::conversion::to_boolean(
            stream.get_property(scope, c"readableEncoding")?,
        ))
    };
    let stdout = joined(scope, &stdout, decoded(c"stdout")?)?;
    let stderr = joined(scope, &stderr, decoded(c"stderr")?)?;
    let success =
        error.is_none() && code.is_number() && code.to_number() == 0.0 && signal.is_null();
    if success {
        Function::call(
            scope,
            HandleValue::undefined(),
            callback,
            &[HandleValue::null(), stdout, stderr],
        )?;
        return Ok(());
    }
    // `Array.prototype.join` joins `null` and `undefined` as empty strings.
    let mut cmd = file;
    for arg in &args {
        cmd.push(' ');
        if !arg.is_null_or_undefined() {
            cmd.push_str(&crate::errors::js_string(scope, *arg)?);
        }
    }
    let error = match error {
        Some(error) => error,
        None => {
            let message = format!(
                "Command failed: {cmd}\n{}",
                crate::errors::js_string(scope, stderr)?
            );
            let error = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, &message)?;
            let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
            let code = if code.is_number() && code.to_number() < 0.0 {
                let name = crate::util::module::get_system_error_name(scope, code)
                    .map_err(|thrown| thrown.throw(scope))?;
                name.as_str().to_jsval_throwing(scope)?
            } else {
                code
            };
            object.set_property(scope, c"code", code)?;
            let child_killed = match child {
                Some(child) => super::child::killed(scope, child)?,
                None => false,
            };
            object.set_property(scope, c"killed", child_killed || killed)?;
            object.set_property(scope, c"signal", signal)?;
            error
        }
    };
    if let Ok(object) = Object::from_value(scope, *error) {
        object.set_property(scope, c"cmd", cmd.as_str())?;
    }
    Function::call(
        scope,
        HandleValue::undefined(),
        callback,
        &[error, stdout, stderr],
    )?;
    Ok(())
}

/// `child_process.exec(command, options, callback)`, as Node.js's `exec`: `execFile` of
/// `command` in a shell, `options.shell` or `/bin/sh`.
pub(super) fn exec<'s>(
    scope: &'s Scope<'_>,
    command: HandleValue<'_>,
    options: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    crate::errors::validate_string(scope, command, "command")?;
    options::null_check(scope, command, "command")?;
    let (options, callback) = if js::function::is_callable(options) {
        (HandleValue::undefined(), options)
    } else {
        (options, callback)
    };
    let copy = Object::new_with_null_prototype(scope)?;
    if let Ok(options) = Object::from_value(scope, *options) {
        copy.assign(scope, options)?;
    }
    let shell = copy.get_property(scope, c"shell")?;
    if !shell.is_string() {
        copy.set_property(scope, c"shell", true)?;
    }
    let copy = copy.to_jsval_throwing(scope)?;
    exec_file(scope, command, copy, callback, HandleValue::undefined())
}

/// `exec(command, options, callback)`.
fn exec_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let child =
        exec(scope, args.get(0), args.get(1), args.get(2)).map_err(|thrown| thrown.throw(scope))?;
    Ok(child.as_value())
}

/// `execFile(file, args, options, callback)`.
fn exec_file_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let child = exec_file(scope, args.get(0), args.get(1), args.get(2), args.get(3))
        .map_err(|thrown| thrown.throw(scope))?;
    Ok(child.as_value())
}

/// The `util.promisify.custom` version of `exec` or `execFile`, whose payload is `true` for
/// `execFile`, as Node.js's `customPromiseExecFunction` creates it: a promise of
/// `{ stdout, stderr }`, rejected with the error, to which `stdout` and `stderr` are added. The
/// promise's `child` is the child process.
fn promisified(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    let settle = Function::new_callback(scope, c"", 3, settle_promise, promise)?;
    // The callback follows the given arguments, as in `orig(...args, callback)`.
    let mut values: Vec<HandleValue<'_>> = args.rest(0);
    values.push((*settle).to_jsval_throwing(scope)?);
    let at = |index: usize| {
        values
            .get(index)
            .copied()
            .unwrap_or(HandleValue::undefined())
    };
    let child = if payload.to_boolean() {
        exec_file(scope, at(0), at(1), at(2), at(3))
    } else {
        exec(scope, at(0), at(1), at(2))
    };
    let child = child.map_err(|thrown| thrown.throw(scope))?;
    promise.set_property(scope, c"child", child)?;
    Ok(promise.as_value())
}

/// The callback of a promisified `exec` or `execFile`, whose payload is the promise.
fn settle_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = js::function::cast_payload::<js::Promise>(scope, payload);
    let (error, stdout, stderr) = (args.get(0), args.get(1), args.get(2));
    if !error.is_null() {
        if let Ok(error) = Object::from_value(scope, *error) {
            error.set_property(scope, c"stdout", stdout)?;
            error.set_property(scope, c"stderr", stderr)?;
        }
        promise.reject(scope, error)?;
    } else {
        let result = Object::new_plain(scope)?;
        result.set_property(scope, c"stdout", stdout)?;
        result.set_property(scope, c"stderr", stderr)?;
        promise.resolve(scope, result)?;
    }
    Ok(js::value::undefined())
}

/// `exec` and `execFile`, each with its `util.promisify.custom` version as a non-enumerable,
/// read-only property.
pub(super) fn exports<'s>(
    scope: &'s Scope<'_>,
) -> Result<Vec<(&'static str, HandleValue<'s>)>, ExnThrown> {
    let mut exports = Vec::new();
    for (name, c_name, length, body, file) in [
        (
            "exec",
            c"exec",
            3,
            exec_callback as js::function::Callback,
            false,
        ),
        ("execFile", c"execFile", 4, exec_file_callback, true),
    ] {
        let function = Function::new_callback(scope, c_name, length, body, ())?;
        let custom = Function::new_callback(scope, c_name, 0, promisified, file)?;
        let id = crate::util::PROMISIFY_CUSTOM.id(scope)?;
        let attrs = js::class_spec::JSPROP_READONLY | js::class_spec::JSPROP_PERMANENT;
        function.define_value_by_id(scope, id, *custom, attrs as u32)?;
        exports.push((name, (*function).to_jsval_throwing(scope)?));
    }
    Ok(exports)
}
