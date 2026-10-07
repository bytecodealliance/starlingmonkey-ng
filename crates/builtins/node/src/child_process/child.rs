// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `ChildProcess`: an `EventEmitter` for a child process, whose standard streams are `net.Socket`s
//! over pipes. Ported from Node.js's `lib/internal/child_process.js`.
//!
//! A child's state is a [`ChildState`], which holds the process until it exits, the streams, and
//! the counts of the `close` events `close` waits for. The own properties Node.js defines, such as
//! `pid`, `exitCode` and `stdout`, are written as the state changes, and never read back.

use std::time::Duration;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};
use platform::child::{ExitStatus, Spawned};
use platform::fs::FsError;

use super::options::{self, StdioEntry};
use crate::errors::{validate_string, NodeError, Thrown};
use crate::support::spawn_cancellable_with_ref;

/// The state of a `ChildProcess`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ChildState {
    /// The process, from its spawn until its exit.
    #[no_trace]
    process: Option<platform::child::Child>,
    /// The exit code, once known.
    #[no_trace]
    exit_code: Option<i32>,
    /// The error of a spawn that failed, or of waiting for the exit.
    #[no_trace]
    spawn_error: Option<FsError>,
    /// The signal the `timeout` and `signal` options kill the process with, or `None` for
    /// `SIGTERM`.
    #[no_trace]
    kill_signal: Option<i32>,
    /// The name of the signal that ended the process.
    #[no_trace]
    signal_code: Option<&'static str>,
    #[no_trace]
    killed: bool,
    /// The number of `close` events of the streams and the channel, and the exit, that `close`
    /// waits for, and the number that happened.
    #[no_trace]
    closes_needed: u32,
    #[no_trace]
    closes_got: u32,
    /// Whether waiting for the exit keeps the event loop alive: unset by `unref` and set again by
    /// `ref`.
    #[no_trace]
    referenced: js::promise::FutureRef,
    /// An interest in the event loop the child was spawned in, held from the spawn until the exit
    /// while the child is referenced, so a served request's loop keeps running to reap the child
    /// and emit `exit`.
    #[no_trace]
    interest: Option<core_runtime::event_loop::InterestHandle>,
    /// The ID of the timer of the `timeout` option, while it is pending.
    #[no_trace]
    timer: Option<u64>,
    /// The program and the arguments the process was spawned with, for errors.
    #[no_trace]
    spawnfile: Option<String>,
    spawnargs: Option<Heap<js::object::Object>>,
    stdin: Option<Heap<js::object::Object>>,
    /// The socket of each descriptor that is a pipe.
    stdio: Vec<Option<Heap<js::object::Object>>>,
    /// The `signal` option and its `abort` listener, while the process runs.
    abort: Option<AbortLink>,
}

/// An `AbortSignal` and the listener that kills a child process when it aborts.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct AbortLink {
    signal: Heap<js::object::Object>,
    listener: Heap<js::object::Object>,
}

#[core_runtime::jsmethods]
impl ChildState {}

/// The state of `child`.
pub(super) fn state_of<'s>(
    scope: &'s Scope<'_>,
    child: Object<'s>,
) -> Result<ChildState<'s>, ExnThrown> {
    child_process_class::state_of(scope, child)
}

/// Node.js's `ErrnoException(err, syscall)` for `error`: an `Error` with the message
/// `syscall CODE`, and `errno`, `code` and `syscall`.
pub(super) fn errno_exception<'s>(
    scope: &'s Scope<'_>,
    error: FsError,
    syscall: &str,
) -> Result<Object<'s>, ExnThrown> {
    let message = format!("{syscall} {}", error.code);
    let value = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, &message)?;
    let object = Object::from_value(scope, *value).map_err(|e| e.throw(scope))?;
    object.set_property(scope, c"errno", error.errno)?;
    object.set_property(scope, c"code", error.code)?;
    object.set_property(scope, c"syscall", syscall)?;
    Ok(object)
}

/// A new array of the elements of the array `array` after the first.
pub(super) fn rest_of<'s>(
    scope: &'s Scope<'_>,
    array: Option<Object<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let elements = match array {
        Some(array) => crate::support::array_elements(scope, array.to_jsval_throwing(scope)?)?
            .unwrap_or_default(),
        None => Vec::new(),
    };
    Ok(*js::Array::with_contents(
        scope,
        elements.get(1..).unwrap_or_default(),
    )?)
}

/// Whether `file` names this runtime's executable.
fn is_runtime(file: &str) -> bool {
    static RUNTIME: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::canonicalize(path).ok())
    });
    let Some(runtime) = runtime else {
        return false;
    };
    std::path::Path::new(file).components().count() > 1
        && std::fs::canonicalize(file).is_ok_and(|path| &path == runtime)
}

/// The runtime's options for Node.js compatibility that its child processes get: `--node-compat`,
/// `--allow-node-runtime-flags`, and `--node-http-listen` if this process has it.
fn runtime_options() -> Vec<&'static str> {
    let mut options = vec!["--node-compat", "--allow-node-runtime-flags"];
    if crate::runtime_flags::get().http_listen {
        options.push("--node-http-listen");
    }
    options
}

/// The environment variable whose options, separated by spaces, this runtime's native command line
/// adds after the program name.
const NODE_OPTIONS_VARIABLE: &str = "STARLINGMONKEY_NODE_OPTIONS";

/// Set [`NODE_OPTIONS_VARIABLE`] in this process's environment to [`runtime_options`], so this
/// runtime started by a child process that inherits the environment, also through a shell or
/// another program, has the same options. Does nothing on wasm, where the environment is fixed.
/// Must be called at startup, after [`crate::runtime_flags::set`] and before other threads start.
pub fn set_runtime_options_variable() {
    platform::process::set_env_var(NODE_OPTIONS_VARIABLE, &runtime_options().join(" "));
}

/// The arguments of a child process that runs this runtime, `args` with [`runtime_options`] added
/// after the program name, each unless `args` has it already. Other programs keep `args`.
pub(super) fn runtime_args(file: &str, mut args: Vec<String>) -> Vec<String> {
    if !is_runtime(file) {
        return args;
    }
    let options = runtime_options();
    let at = 1.min(args.len());
    for option in options.into_iter().rev() {
        if !args.iter().any(|arg| arg == option) {
            args.insert(at, option.to_string());
        }
    }
    args
}

/// The strings of the array `value`, converted as `String(element)` converts them.
fn strings(scope: &Scope<'_>, values: &[HandleValue<'_>]) -> Result<Vec<String>, ExnThrown> {
    values
        .iter()
        .map(|value| crate::errors::js_string(scope, *value))
        .collect()
}

/// The errors of a spawn that `ChildProcess.spawn` emits as an `error` event in the tick queue
/// instead of throwing them. `ENOSYS` is among them, for hosts that can't spawn processes.
const EMITTED_SPAWN_ERRORS: [&str; 6] =
    ["EACCES", "EAGAIN", "EMFILE", "ENFILE", "ENOENT", "ENOSYS"];

/// `ChildProcess.prototype.spawn(options)`, as Node.js's: spawn the process `options.file`
/// with the arguments `options.args`, the environment `options.envPairs`, the working directory
/// `options.cwd` and the descriptors `options.stdio`, and create a socket for each descriptor
/// that is a pipe. Returns 0, or the negated error number of a spawn error that is emitted
/// instead of thrown. The spawn is published on the `child_process.spawn` tracing channel's
/// `start`, `end` and `error` channels.
pub(super) fn spawn<'s>(
    scope: &'s Scope<'_>,
    child: Object<'s>,
    options: HandleValue<'_>,
) -> Result<f64, Thrown> {
    let options = crate::errors::validate_plain_object(scope, options, "options", false)?;
    let mut stdio = options.get_property(scope, c"stdio")?;
    if !js::conversion::to_boolean(stdio) {
        stdio = "pipe".to_jsval_throwing(scope)?;
    }
    let entries = options::valid_stdio(scope, stdio, false)?;
    let serialization = options.get_property(scope, c"serialization")?;
    let serialization = if serialization.is_undefined() {
        "json".to_string()
    } else {
        let name = crate::support::rust_string(scope, serialization);
        match name.as_deref() {
            Some(name @ ("json" | "advanced")) => name.to_string(),
            _ => {
                return Err(NodeError::invalid_arg_value(
                    scope,
                    "options.serialization",
                    serialization,
                    "must be one of: undefined, 'json', 'advanced'",
                )
                .into())
            }
        }
    };
    let ipc = entries.iter().position(|entry| *entry == StdioEntry::Ipc);
    if ipc.is_some() && serialization == "advanced" {
        return Err(NodeError::not_implemented("advanced serialization").into());
    }
    let env_pairs = options.get_property(scope, c"envPairs")?;
    let mut env = match crate::support::array_elements(scope, env_pairs)? {
        Some(pairs) => Some(strings(scope, &pairs)?),
        None if ipc.is_some() && !env_pairs.is_undefined() => {
            options::validate_array(scope, env_pairs, "options.envPairs")?;
            None
        }
        None if ipc.is_some() => Some(Vec::new()),
        None => None,
    };
    if let (Some(fd), Some(env)) = (ipc, env.as_mut()) {
        env.push(format!("NODE_CHANNEL_FD={fd}"));
        env.push(format!("NODE_CHANNEL_SERIALIZATION_MODE={serialization}"));
    }
    let file = options.get_property(scope, c"file")?;
    let file = validate_string(scope, file, "options.file")?;
    child.set_property(scope, c"spawnfile", file.as_str())?;
    let args = options.get_property(scope, c"args")?;
    let (spawnargs, args) = if args.is_undefined() {
        (
            *js::Array::with_contents(scope, &[] as &[HandleValue<'_>])?,
            Vec::new(),
        )
    } else {
        let elements = options::validate_array(scope, args, "options.args")?;
        let array = Object::from_value(scope, *args).map_err(|e| e.throw(scope))?;
        (array, strings(scope, &elements)?)
    };
    child.set_property(scope, c"spawnargs", spawnargs)?;
    let cwd = options::cwd(scope, options.get_property(scope, c"cwd")?);
    let detached = js::conversion::to_boolean(options.get_property(scope, c"detached")?);
    let id = |name| -> Result<Option<u32>, ExnThrown> {
        let value = options.get_property(scope, name)?;
        Ok(options::is_int32(value).then(|| value.to_number() as i32 as u32))
    };
    let (uid, gid) = (id(c"uid")?, id(c"gid")?);
    let (descriptors, bad_fd) = options::host_stdio(scope, &entries)?;
    {
        let state = state_of(scope, child)?;
        let mut data = state.data_mut();
        data.spawnfile = Some(file.clone());
        data.spawnargs = Some(Heap::from(spawnargs));
    }
    let spawn_options = platform::child::Options {
        file: file.clone(),
        args: runtime_args(&file, args),
        cwd,
        env: env.map(|pairs| {
            pairs
                .into_iter()
                .map(|pair| match pair.split_once('=') {
                    Some((key, value)) => (key.to_string(), value.to_string()),
                    None => (pair, String::new()),
                })
                .collect()
        }),
        stdio: descriptors,
        detached,
        uid,
        gid,
    };
    if let Some(channel) = subscribed(scope, "tracing:child_process.spawn:start")? {
        let message = Object::new_plain(scope)?;
        message.set_property(scope, c"process", child)?;
        message.set_property(scope, c"options", options)?;
        channel.publish(scope, message.to_jsval_throwing(scope)?)?;
    }
    let (process, mut pipes) = if bad_fd {
        (Err(FsError::from_code("EBADF")), Vec::new())
    } else {
        let Spawned { child, pipes } = platform::child::spawn(&spawn_options);
        (child.map_err(|error| FsError::from_io(&error)), pipes)
    };
    if let Some(channel) = subscribed(scope, "tracing:child_process.spawn:end")? {
        if process.is_ok() {
            let message = Object::new_plain(scope)?;
            message.set_property(scope, c"process", child)?;
            channel.publish(scope, message.to_jsval_throwing(scope)?)?;
        }
    }
    if let (Err(error), Some(channel)) = (
        &process,
        subscribed(scope, "tracing:child_process.spawn:error")?,
    ) {
        let message = Object::new_plain(scope)?;
        message.set_property(scope, c"process", child)?;
        message.set_property(scope, c"error", errno_exception(scope, *error, "spawn")?)?;
        channel.publish(scope, message.to_jsval_throwing(scope)?)?;
    }
    let mut result = 0.0;
    let mut running = None;
    let pid = match process {
        Ok(process) => {
            let pid = process.pid();
            running = Some(process);
            js::jobs::queue_native_tick(scope, spawn_tick, child, &[])?;
            Some(pid)
        }
        Err(error) if !EMITTED_SPAWN_ERRORS.contains(&error.code) => {
            child.set_property(scope, c"_handle", HandleValue::null())?;
            let error = errno_exception(scope, error, "spawn")?;
            return Err(throw_object(scope, error));
        }
        Err(error) => {
            state_of(scope, child)?.data_mut().spawn_error = Some(error);
            js::jobs::queue_native_tick(scope, error_tick, child, &[])?;
            if matches!(error.code, "EMFILE" | "ENFILE") {
                return Ok(f64::from(error.errno));
            }
            result = f64::from(error.errno);
            None
        }
    };
    match pid {
        Some(pid) => child.set_property(scope, c"pid", f64::from(pid))?,
        None => child.set_property(scope, c"pid", HandleValue::undefined())?,
    }
    // A socket whose pipe the child got stops reading, so the child gets all of its input.
    if let Ok(given) = Object::from_value(scope, *stdio) {
        for (index, entry) in entries.iter().enumerate() {
            if let StdioEntry::Wrap(_) = entry {
                let stream = given.get_element(scope, index as u32)?;
                if let Ok(stream) = Object::from_value(scope, *stream) {
                    crate::net::socket::use_as_stdio(scope, stream)?;
                }
            }
        }
    }
    let mut channel = None;
    let mut sockets: Vec<Option<(Object<'s>, bool)>> = Vec::new();
    if sockets.try_reserve_exact(entries.len()).is_err() {
        js::exception::report_out_of_memory(scope);
        return Err(ExnThrown.into());
    }
    for (index, entry) in entries.iter().enumerate() {
        let pipe = pipes.get_mut(index).and_then(Option::take);
        match entry {
            StdioEntry::Ipc => {
                state_of(scope, child)?.data_mut().closes_needed += 1;
                channel = Some(pipe);
                sockets.push(None);
            }
            StdioEntry::Pipe => {
                let attached = pipe.is_some();
                let socket = create_socket(scope, index > 0, pipe)?;
                if index > 0 {
                    state_of(scope, child)?.data_mut().closes_needed += 1;
                    let on_close = Function::new_callback(scope, c"", 0, stdio_closed, child)?;
                    crate::events::listen(scope, socket, c"on", c"close", *on_close)?;
                }
                sockets.push(Some((socket, attached)));
            }
            _ => sockets.push(None),
        }
    }
    let closes_needed = state_of(scope, child)?.data().closes_needed;
    child.set_property(scope, c"_closesNeeded", closes_needed)?;
    let stream = |index: usize| -> Result<HandleValue<'s>, ExnThrown> {
        match sockets.get(index).copied().flatten() {
            Some((socket, _)) => socket.to_jsval_throwing(scope),
            None => Ok(HandleValue::null()),
        }
    };
    child.set_property(scope, c"stdin", stream(0)?)?;
    child.set_property(scope, c"stdout", stream(1)?)?;
    child.set_property(scope, c"stderr", stream(2)?)?;
    let all: Vec<HandleValue<'s>> = (0..sockets.len()).map(&stream).collect::<Result<_, _>>()?;
    child.set_property(scope, c"stdio", js::Array::with_contents(scope, &all)?)?;
    {
        let state = state_of(scope, child)?;
        let mut data = state.data_mut();
        data.stdin = sockets
            .first()
            .copied()
            .flatten()
            .map(|(socket, _)| Heap::from(socket));
        data.stdio = sockets
            .iter()
            .map(|socket| socket.map(|(socket, _)| Heap::from(socket)))
            .collect();
    }
    for (index, socket) in sockets.iter().enumerate() {
        let Some((socket, attached)) = *socket else {
            continue;
        };
        if !attached {
            // A socket without a pipe, as where processes can't be spawned, closes at once.
            Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
        } else if index > 0 {
            crate::net::socket::read_zero(scope, socket)?;
        }
    }
    if let Some(pipe) = channel {
        super::ipc::setup(scope, child, pipe, true)?;
    }
    // Waiting for the exit starts after the reads, so output the child wrote before exiting is
    // emitted before `exit` when both are ready.
    if let Some(process) = running {
        wait_for_exit(scope, child, process)?;
    }
    Ok(result)
}

/// The diagnostics channel `name`, if it has subscribers.
fn subscribed<'s>(
    scope: &'s Scope<'_>,
    name: &str,
) -> Result<Option<crate::diagnostics_channel::Channel<'s>>, ExnThrown> {
    let name = name.to_jsval_throwing(scope)?;
    let channel = crate::diagnostics_channel::module::channel(scope, name)
        .map_err(|thrown| thrown.throw(scope))?;
    Ok(channel.has_subscribers().then_some(channel))
}

/// Set `error` as the pending exception, and return [`Thrown::Pending`].
pub(super) fn throw_object(scope: &Scope<'_>, error: Object<'_>) -> Thrown {
    if let Ok(value) = error.to_jsval_throwing(scope) {
        js::exception::set_pending(
            scope,
            value,
            js::native::ExceptionStackBehavior::DoNotCapture,
        );
    }
    Thrown::Pending
}

/// A `net.Socket` for a standard stream of a child process, readable unless it is the child's
/// standard input, over `pipe`.
fn create_socket<'s>(
    scope: &'s Scope<'_>,
    readable: bool,
    pipe: Option<platform::child::Pipe>,
) -> Result<Object<'s>, ExnThrown> {
    let options = Object::new_plain(scope)?;
    options.set_property(scope, c"readable", readable)?;
    let class = crate::net::socket::socket_class::class_constructor(scope)?;
    let socket = Function::construct(scope, class, &[options.to_jsval_throwing(scope)?])?;
    if let Some(pipe) = pipe {
        crate::net::socket::attach_pipe(scope, socket, pipe)?;
    }
    Ok(socket)
}

/// Wait for `process`, the process of `child`, to exit, and keep it in the state of `child` until
/// then.
fn wait_for_exit(
    scope: &Scope<'_>,
    child: Object<'_>,
    process: platform::child::Child,
) -> Result<(), ExnThrown> {
    let exit = process.exit();
    let state = state_of(scope, child)?;
    let referenced = state.data().referenced.clone();
    {
        let mut data = state.data_mut();
        data.process = Some(process);
        if referenced.is_referenced() {
            data.interest = crate::fs::hold_event_loop();
        }
    }
    spawn_cancellable_with_ref(
        scope,
        child,
        |_| async move { Some(exit.await) },
        |scope, child, result: std::io::Result<ExitStatus>| {
            let state = state_of(scope, child)?;
            let mut data = state.data_mut();
            data.process = None;
            data.interest = None;
            match result {
                Ok(status) => match status.signal.and_then(crate::os::signal_name) {
                    Some(name) => data.signal_code = Some(name),
                    None => data.exit_code = Some(status.code.unwrap_or(0)),
                },
                Err(error) => data.spawn_error = Some(FsError::from_io(&error)),
            }
            Ok(HandleValue::undefined())
        },
        |scope, child, _| on_exit(scope, child),
        referenced,
    )?;
    Ok(())
}

/// The tick callback whose payload is a child process that spawned: emit `spawn`.
fn spawn_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let child = js::function::cast_payload::<js::Object>(scope, payload);
    crate::events::emit(scope, child, c"spawn", &[])?;
    Ok(js::value::undefined())
}

/// The tick callback whose payload is a child process that failed to spawn, as Node.js's
/// `onErrorNT`: handle the failure as the exit.
fn error_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let child = js::function::cast_payload::<js::Object>(scope, payload);
    on_exit(scope, child)?;
    Ok(js::value::undefined())
}

/// The exit of `child`, or the failure to spawn it, as Node.js's `onexit`: record the exit code
/// or the signal, destroy the standard input, and emit `exit`, or `error` for a failed spawn. The
/// unconsumed standard streams start flowing in the next tick, so they end and close.
fn on_exit(scope: &Scope<'_>, child: Object<'_>) -> Result<(), ExnThrown> {
    let state = state_of(scope, child)?;
    let (exit_code, signal_code, spawn_error, spawnfile, spawnargs, stdin) = {
        let mut data = state.data_mut();
        data.process = None;
        let stdin = data.stdin.as_ref().map(|stdin| stdin.get(scope));
        let spawnargs = data.spawnargs.as_ref().map(|args| args.get(scope));
        (
            data.exit_code,
            data.signal_code,
            data.spawn_error,
            data.spawnfile.clone(),
            spawnargs,
            stdin,
        )
    };
    match (signal_code, spawn_error) {
        (Some(signal), _) => child.set_property(scope, c"signalCode", signal)?,
        (None, Some(error)) => child.set_property(scope, c"exitCode", error.errno)?,
        (None, None) => child.set_property(scope, c"exitCode", exit_code.unwrap_or(0))?,
    }
    if let Some(stdin) = stdin {
        Function::call_by_name(scope, stdin.handle(), c"destroy", js::function::EmptyArgs)?;
    }
    child.set_property(scope, c"_handle", HandleValue::null())?;
    if let Some(error) = spawn_error {
        let syscall = match &spawnfile {
            Some(file) => format!("spawn {file}"),
            None => "spawn".to_string(),
        };
        let error = errno_exception(scope, error, &syscall)?;
        if let Some(file) = &spawnfile {
            error.set_property(scope, c"path", file.as_str())?;
        }
        error.set_property(scope, c"spawnargs", rest_of(scope, spawnargs)?)?;
        crate::events::emit(scope, child, c"error", &[error.to_jsval_throwing(scope)?])?;
    } else {
        stop_watching(scope, &state)?;
        let (code, signal) = exit_values(scope, &state)?;
        crate::events::emit(scope, child, c"exit", &[code, signal])?;
    }
    js::jobs::queue_native_tick(scope, flush_stdio, child, &[])?;
    maybe_close(scope, child)
}

/// The exit code and the signal of the child process of `state`, each `null` if unknown, as
/// `exit` and `close` give them.
fn exit_values<'s>(
    scope: &'s Scope<'_>,
    state: &ChildState<'_>,
) -> Result<(HandleValue<'s>, HandleValue<'s>), ExnThrown> {
    let (exit_code, signal_code, spawn_error) = {
        let data = state.data();
        (data.exit_code, data.signal_code, data.spawn_error)
    };
    let code = match (signal_code, spawn_error, exit_code) {
        (Some(_), _, _) => HandleValue::null(),
        (None, Some(error), _) => scope.root_value(js::value::from_f64(f64::from(error.errno))),
        (None, None, Some(code)) => scope.root_value(js::value::from_f64(f64::from(code))),
        (None, None, None) => HandleValue::null(),
    };
    let signal = match signal_code {
        Some(signal) => signal.to_jsval_throwing(scope)?,
        None => HandleValue::null(),
    };
    Ok((code, signal))
}

/// Clear the `timeout` option's timer and remove the `signal` option's listener of the child
/// process of `state`.
fn stop_watching(scope: &Scope<'_>, state: &ChildState<'_>) -> Result<(), ExnThrown> {
    let timer = state.data_mut().timer.take();
    if let Some(id) = timer {
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.clear_js_timer(id)
        });
    }
    let abort = {
        let mut data = state.data_mut();
        let abort = data
            .abort
            .as_ref()
            .map(|abort| (abort.signal.get(scope), abort.listener.get(scope)));
        data.abort = None;
        abort
    };
    if let Some((signal, listener)) = abort {
        crate::events::listen(scope, signal, c"removeEventListener", c"abort", listener)?;
    }
    Ok(())
}

/// The `close` listener of a standard stream, whose payload is the child process.
fn stdio_closed(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let child = js::function::cast_payload::<js::Object>(scope, payload);
    maybe_close(scope, child)?;
    Ok(js::value::undefined())
}

/// Count one of the `close` events `child` waits for, as Node.js's `maybeClose`, and emit
/// `close` with the exit code and the signal once all happened.
pub(super) fn maybe_close(scope: &Scope<'_>, child: Object<'_>) -> Result<(), ExnThrown> {
    let state = state_of(scope, child)?;
    let (got, needed) = {
        let mut data = state.data_mut();
        data.closes_got += 1;
        (data.closes_got, data.closes_needed)
    };
    child.set_property(scope, c"_closesGot", got)?;
    if got == needed {
        let (code, signal) = exit_values(scope, &state)?;
        crate::events::emit(scope, child, c"close", &[code, signal])?;
    }
    Ok(())
}

/// The tick callback whose payload is a child process that exited, as Node.js's `flushStdio`:
/// resume each standard stream that is still readable, so it reads to its end, unless another
/// child got its pipe.
fn flush_stdio(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let child = js::function::cast_payload::<js::Object>(scope, payload);
    let streams: Vec<Object<'_>> = {
        let state = state_of(scope, child)?;
        let data = state.data();
        data.stdio
            .iter()
            .flatten()
            .map(|stream| stream.get(scope))
            .collect()
    };
    for stream in streams {
        if crate::net::socket::used_as_stdio(scope, stream)? {
            continue;
        }
        if js::conversion::to_boolean(stream.get_property(scope, c"readable")?) {
            Function::call_by_name(scope, stream.handle(), c"resume", js::function::EmptyArgs)?;
        }
    }
    Ok(js::value::undefined())
}

/// `subprocess.kill(signal)`: send `signal`, `SIGTERM` by default, to the child process of
/// `child`. Returns whether the signal was sent, and records that in `killed`.
pub(super) fn kill(
    scope: &Scope<'_>,
    child: Object<'_>,
    signal: Option<HandleValue<'_>>,
) -> Result<bool, Thrown> {
    let number = match signal {
        Some(signal) if signal.is_number() && signal.to_number() == 0.0 => 0,
        Some(signal) if !signal.is_undefined() => options::signal(scope, signal)?,
        _ => crate::os::signal_number("SIGTERM").unwrap_or(15),
    };
    let sent = {
        let state = state_of(scope, child)?;
        let data = state.data();
        data.process.as_ref().map(|process| process.kill(number))
    };
    match sent {
        None => Ok(false),
        Some(Ok(())) => {
            state_of(scope, child)?.data_mut().killed = true;
            child.set_property(scope, c"killed", true)?;
            Ok(true)
        }
        Some(Err(error)) => {
            let error = FsError::from_io(&error);
            match error.code {
                "ESRCH" => Ok(false),
                "EINVAL" | "ENOSYS" => {
                    Err(throw_object(scope, errno_exception(scope, error, "kill")?))
                }
                _ => {
                    let error = errno_exception(scope, error, "kill")?;
                    crate::events::emit(
                        scope,
                        child,
                        c"error",
                        &[error.to_jsval_throwing(scope)?],
                    )?;
                    Ok(false)
                }
            }
        }
    }
}

/// Whether `kill` sent a signal to the child process of `child`.
pub(super) fn killed(scope: &Scope<'_>, child: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(state_of(scope, child)?.data().killed)
}

/// Call `child.kill(signal)` once `timeout` milliseconds passed, unless the child exited before.
pub(super) fn kill_after(
    scope: &Scope<'_>,
    child: Object<'_>,
    timeout: f64,
    signal: Option<i32>,
) -> Result<(), ExnThrown> {
    let callback = crate::support::shared_function(scope, timeout_kill, 1)?;
    let callable = js::Callable::from_jsval(scope, callback.to_jsval_throwing(scope)?, ())
        .map_err(|e| e.throw(scope))?;
    let signal = match signal {
        Some(signal) => scope.root_value(js::value::from_f64(f64::from(signal))),
        None => HandleValue::undefined(),
    };
    let id = core_runtime::event_loop::timer::start_timer(
        scope,
        callable,
        child.to_jsval_throwing(scope)?,
        &[signal],
        Duration::from_millis(timeout as u64),
        false,
        None,
    )
    .ok_or_else(|| core_runtime::event_loop::throw_no_event_loop(scope, "child_process.spawn"))?;
    state_of(scope, child)?.data_mut().timer = Some(id);
    Ok(())
}

/// The callback of the `timeout` option's timer, whose `this` is the child process and whose
/// argument is the kill signal: call `kill`, and emit what it throws as an `error` event.
fn timeout_kill(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let child = crate::support::this_object(scope, &args)?;
    state_of(scope, child)?.data_mut().timer = None;
    let signal = args.get(0);
    if Function::call_by_name(scope, child.handle(), c"kill", &[signal]).is_err() {
        let error = js::exception::take_pending_or_undefined(scope);
        crate::events::emit(scope, child, c"error", &[error])?;
    }
    Ok(js::value::undefined())
}

/// Kill `child` with `signal` once the `AbortSignal` `abort` aborts, or in the tick queue if it
/// aborted, as Node.js's `spawn` does with its `signal` option.
pub(super) fn kill_on_abort(
    scope: &Scope<'_>,
    child: Object<'_>,
    abort: Object<'_>,
    signal: Option<i32>,
) -> Result<(), ExnThrown> {
    state_of(scope, child)?.data_mut().kill_signal = signal;
    if js::conversion::to_boolean(abort.get_property(scope, c"aborted")?) {
        let abort = abort.to_jsval_throwing(scope)?;
        return js::jobs::queue_native_tick(scope, abort_kill, child, &[abort]);
    }
    let listener = Function::new_callback(scope, c"onAbortListener", 0, abort_kill, child)?;
    crate::events::listen(scope, abort, c"addEventListener", c"abort", *listener)?;
    state_of(scope, child)?.data_mut().abort = Some(AbortLink {
        signal: Heap::from(abort),
        listener: Heap::from(*listener),
    });
    Ok(())
}

/// The `abort` listener of the `signal` option, whose payload is the child process, as Node.js's
/// `abortChildProcess`: kill the child, and emit an `AbortError` whose cause is the signal's
/// reason if a signal was sent, or the error `kill` threw. The signal is the state's, or the
/// argument of the tick of a signal that aborted before the spawn.
fn abort_kill(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let child = js::function::cast_payload::<js::Object>(scope, payload);
    let (signal, abort) = {
        let state = state_of(scope, child)?;
        let data = state.data();
        let abort = data.abort.as_ref().map(|abort| abort.signal.get(scope));
        (data.kill_signal, abort)
    };
    let abort = abort.or_else(|| Object::from_value(scope, *args.get(0)).ok());
    let reason = match abort {
        Some(abort) => abort.get_property(scope, c"reason")?,
        None => HandleValue::undefined(),
    };
    let signal = match signal {
        Some(signal) => scope.root_value(js::value::from_f64(f64::from(signal))),
        None => HandleValue::undefined(),
    };
    match Function::call_by_name(scope, child.handle(), c"kill", &[signal]) {
        Ok(sent) if js::conversion::to_boolean(sent) => {
            let error = crate::errors::abort_error(scope, Some(reason))?;
            crate::events::emit(scope, child, c"error", &[error])?;
        }
        Ok(_) => {}
        Err(_) => {
            let error = js::exception::take_pending_or_undefined(scope);
            crate::events::emit(scope, child, c"error", &[error])?;
        }
    }
    Ok(js::value::undefined())
}

/// `subprocess[Symbol.dispose]()`: kill the child process unless `kill` sent it a signal.
fn dispose(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let child = crate::support::this_object(scope, &args)?;
    if !killed(scope, child)? {
        Function::call_by_name(scope, child.handle(), c"kill", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(
    name = "ChildProcess",
    extends = "crate::events::event_emitter",
    state = "ChildState",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod child_process_class {
    use super::*;

    /// <https://nodejs.org/api/child_process.html#class-childprocess>: a child process that isn't
    /// spawned yet.
    #[constructor(length = 0)]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)?;
        state_of(scope, this)?.data_mut().closes_needed = 1;
        this.set_property(scope, c"_closesNeeded", 1)?;
        this.set_property(scope, c"_closesGot", 0)?;
        this.set_property(scope, c"connected", false)?;
        this.set_property(scope, c"signalCode", HandleValue::null())?;
        this.set_property(scope, c"exitCode", HandleValue::null())?;
        this.set_property(scope, c"killed", false)?;
        this.set_property(scope, c"spawnfile", HandleValue::null())?;
        this.set_property(scope, c"_handle", Object::new_plain(scope)?)?;
        if let Some(channel) = super::subscribed(scope, "child_process")? {
            let message = Object::new_plain(scope)?;
            message.set_property(scope, c"process", this)?;
            channel.publish(scope, message.to_jsval_throwing(scope)?)?;
        }
        Ok(())
    }

    /// Node.js's `ChildProcess.prototype.spawn(options)`, which `child_process.spawn` calls with
    /// the options it normalized: see [`super::spawn`].
    #[method(name = "spawn", length = 1)]
    pub fn spawn_method(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: HandleValue<'_>,
    ) -> Result<f64, Thrown> {
        super::spawn(scope, this, options)
    }

    /// <https://nodejs.org/api/child_process.html#subprocesskillsignal>: signals are sent with
    /// `kill(2)`. A signal name or number that `os.constants.signals` lacks throws
    /// `ERR_UNKNOWN_SIGNAL`.
    #[method(length = 1)]
    pub fn kill(
        scope: &Scope<'_>,
        this: Object<'_>,
        signal: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        super::kill(scope, this, signal)
    }

    /// <https://nodejs.org/api/child_process.html#subprocessref>: waiting for the child to exit
    /// keeps the event loop alive again.
    #[method(name = "ref")]
    pub fn ref_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        let mut data = state.data_mut();
        data.referenced.set_referenced(true);
        if data.process.is_some() && data.interest.is_none() {
            data.interest = crate::fs::hold_event_loop();
        }
        Ok(())
    }

    /// <https://nodejs.org/api/child_process.html#subprocessunref>: waiting for the child to exit
    /// doesn't keep the event loop alive. Its pipes still do until they are unreferenced.
    pub fn unref(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        let mut data = state.data_mut();
        data.referenced.set_referenced(false);
        data.interest = None;
        Ok(())
    }

    /// Define `Symbol.dispose`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let dispose = Function::new_callback(scope, c"[Symbol.dispose]", 0, super::dispose, ())?;
        js::class::define_well_known_symbol_property(
            scope,
            proto,
            js::native::SymbolCode::dispose,
            *dispose,
        )
    }
}
