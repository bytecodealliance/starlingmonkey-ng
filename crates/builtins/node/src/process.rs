// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:process` and the `process` global. See <https://nodejs.org/api/process.html>.
//!
//! `process` is an `EventEmitter`. Its functions come from the [`functions`] namespace, and the
//! global initializer [`install`] adds the data properties and makes it an emitter.
//!
//! `process.env`, `process.argv` and `process.execPath` are accessors that read the host's
//! environment, arguments and program path on first use after the runtime starts or resumes from
//! a Wizer snapshot, so a snapshot doesn't keep the ones it was taken with. Assigning one replaces
//! it until the next resume. `process.env` and `process.argv` are proxies that read the host's
//! values again on their first use after a resume, so `node:process`' `env` and `argv` exports
//! stay current as well. Values assigned to `process.env`'s properties are converted to strings.

use std::cell::Cell;
use std::ffi::CStr;
use std::io::{IsTerminal, Write};

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_function, validate_string, ErrorKind, NodeError, Thrown};

/// The Node.js version whose API the runtime targets, which `process.version` reports.
pub const NODE_VERSION: &str = "24.21.0";

/// The current working directory, as `process.cwd()` returns it: the directory relative paths
/// resolve against, natively and in WASI's emulation of one.
pub fn cwd() -> String {
    std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into())
}

/// `process.platform`: the host's platform natively, and `linux` on wasm, whose WASI paths are
/// POSIX paths.
pub fn platform() -> &'static str {
    if cfg!(target_arch = "wasm32") {
        return "linux";
    }
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        os => os,
    }
}

/// `process.arch`: `wasm32` on wasm, and the host's architecture in Node's naming natively.
pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        arch => arch,
    }
}

/// `process.pid`: the process ID natively, and 1 on wasm, where an instance has none.
fn pid() -> u32 {
    if cfg!(target_arch = "wasm32") {
        1
    } else {
        std::process::id()
    }
}

js::instance_local! {
    /// Incremented each time an instance resumes from a snapshot, which makes `process.env` and
    /// `process.argv` read the host's values again.
    static RESUME_GENERATION: Cell<u32> = const { Cell::new(0) };
    /// The monotonic clock reading `process.uptime()` counts from.
    static START_NS: Cell<Option<u64>> = const { Cell::new(None) };
}

/// The resume fixup registered by [`crate::register`].
pub(crate) fn on_resume() {
    RESUME_GENERATION.with(|generation| generation.set(generation.get() + 1));
    START_NS.with(|start| start.set(Some(platform::clock::monotonic_ns())));
}

/// Emit a process warning with `message` and the warning name `kind`, through
/// `process.emitWarning` if the global has a `process`, and to stderr otherwise.
pub fn emit_warning(scope: &Scope<'_>, message: &str, kind: &str) -> Result<(), ExnThrown> {
    let process = scope.global().get_property(scope, c"process")?;
    if let Ok(process) = Object::from_value(scope, process) {
        let emit = process.get_property(scope, c"emitWarning")?;
        if Object::from_value(scope, emit).is_ok_and(|emit| emit.is_callable()) {
            Function::call(scope, process, emit, &[message, kind])?;
            return Ok(());
        }
    }
    eprintln!("({kind}) {message}");
    Ok(())
}

/// Emit the deprecation warning `code` with `message` through `process.emitWarning`.
pub(crate) fn deprecation_warning(
    scope: &Scope<'_>,
    message: &str,
    code: &str,
) -> Result<(), ExnThrown> {
    let process = scope.global().get_property(scope, c"process")?;
    let Ok(process) = Object::from_value(scope, process) else {
        return Ok(());
    };
    let emit = process.get_property(scope, c"emitWarning")?;
    if Object::from_value(scope, emit).is_ok_and(|emit| emit.is_callable()) {
        Function::call(scope, process, emit, &[message, "DeprecationWarning", code])?;
    }
    Ok(())
}

/// Whether the innermost frame of the JS stack is in a `node_modules` directory.
pub(crate) fn called_from_node_modules(scope: &Scope<'_>) -> Result<bool, ExnThrown> {
    let error_class = scope.global().get_property(scope, c"Error")?;
    let Ok(error_class) = Object::from_value(scope, error_class) else {
        return Ok(false);
    };
    let error = Function::construct(scope, error_class, js::function::EmptyArgs)?;
    let stack =
        String::from_jsval(scope, error.get_property(scope, c"stack")?, ()).unwrap_or_default();
    Ok(stack
        .lines()
        .find(|line| !line.is_empty())
        .is_some_and(|line| line.contains("/node_modules/")))
}

/// The global's `process` object.
fn global_process<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let process = scope.global().get_property(scope, c"process")?;
    Object::from_value(scope, process).map_err(|e| e.throw(scope))
}

/// A `process.hrtime()` reading `[seconds, nanoseconds]`, relative to `previous` if given.
fn hrtime_pair(scope: &Scope<'_>, previous: Option<HandleValue<'_>>) -> Result<(f64, f64), Thrown> {
    let mut now = platform::clock::monotonic_ns();
    if let Some(previous) = previous.filter(|previous| !previous.is_undefined()) {
        let previous = Object::from_value(scope, *previous).map_err(|_| {
            NodeError::invalid_arg_type(scope, "time", "an instance of Array", previous)
        })?;
        let seconds = f64::from_jsval(scope, previous.get_element(scope, 0)?, ()).unwrap_or(0.0);
        let nanos = f64::from_jsval(scope, previous.get_element(scope, 1)?, ()).unwrap_or(0.0);
        now = now.saturating_sub((seconds * 1e9 + nanos) as u64);
    }
    Ok(((now / 1_000_000_000) as f64, (now % 1_000_000_000) as f64))
}

/// `process.hrtime.bigint()`: the monotonic clock reading in nanoseconds.
fn hrtime_bigint(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let nanos = js::bigint::from_u64(scope, platform::clock::monotonic_ns())?;
    Ok(js::value::from_bigint(nanos))
}

/// Emit `exit` on `process` with `process.exitCode`, or 0 if it isn't set, and set
/// `process._exiting`. Does nothing if `process._exiting` is already set. Returns the code to exit
/// with: `process.exitCode` after the listeners ran, or 0 if it isn't set. If a listener threw,
/// prints the exception to stderr and returns `process.exitCode`, or 1 if it isn't set.
fn emit_exit(scope: &Scope<'_>, process: Object<'_>) -> u8 {
    let emitted = (|| -> Result<(), ExnThrown> {
        if is_exiting(scope, process)? {
            return Ok(());
        }
        process.set_property(scope, c"_exiting", true)?;
        let code = exit_code_if_set(scope, process).unwrap_or(0);
        let exit = scope.root_value(js::JSString::from_str(scope, "exit")?.as_value());
        let code = scope.root_value(js::value::from_i32(code.into()));
        Function::call_by_name(scope, process.handle(), c"emit", &[exit, code])?;
        Ok(())
    })();
    if emitted.is_err() {
        print_uncaught(scope);
        return exit_code_if_set(scope, process).unwrap_or(1);
    }
    exit_code_if_set(scope, process).unwrap_or(0)
}

/// Whether `process._exiting` is `true`.
fn is_exiting(scope: &Scope<'_>, process: Object<'_>) -> Result<bool, ExnThrown> {
    let exiting = process.get_property(scope, c"_exiting")?;
    Ok(exiting.is_boolean() && exiting.to_boolean())
}

/// `process.exitCode` as an exit code, or `None` if it isn't set to an integer.
fn exit_code_if_set(scope: &Scope<'_>, process: Object<'_>) -> Option<u8> {
    let value = process.get_property(scope, c"exitCode").ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    let code = exit_code(scope, process, Some(value));
    js::exception::clear(scope);
    code.ok()
}

/// Print the pending exception to stderr, as `Uncaught` followed by its message and stack.
fn print_uncaught(scope: &Scope<'_>) {
    if let Some(error) = js::exception::take_pending(scope) {
        eprintln!("{}", uncaught_text(scope, error));
    }
}

/// The text Node.js prints for the uncaught exception `error`: `util.inspect` of an object, which
/// shows an error's name, code, message, stack and other own properties, followed by the Node.js
/// version, or `Uncaught` and the inspected value for a primitive.
fn uncaught_text(scope: &Scope<'_>, error: HandleValue<'_>) -> String {
    let inspected = crate::inspect::InspectOptions::current(scope)
        .and_then(|options| crate::inspect::inspect(scope, error, options));
    let inspected = match inspected {
        Ok(text) => text,
        Err(_) => {
            js::exception::clear(scope);
            crate::errors::describe_value(scope, error)
        }
    };
    if !error.is_object() {
        return format!("Uncaught {inspected}");
    }
    let version = global_process(scope)
        .and_then(|process| process.get_property(scope, c"version"))
        .ok()
        .and_then(|version| String::from_jsval(scope, version, ()).ok())
        .unwrap_or_default();
    format!("{inspected}\n\nNode.js {version}")
}

/// Flush the standard streams, and exit with `code`.
fn flush_and_exit(code: u8) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    platform::process::exit(code)
}

/// Exit after emitting `exit` on `process`, with the code [`emit_exit`] returns.
fn exit_with(scope: &Scope<'_>, process: Object<'_>) -> ! {
    flush_and_exit(emit_exit(scope, process))
}

/// The `before_exit` lifecycle hook: emit `beforeExit` with `process.exitCode`.
fn before_exit(scope: &Scope<'_>) {
    let Ok(process) = global_process(scope) else {
        return;
    };
    let code = exit_code(scope, process, None).unwrap_or(0);
    let emitted = (|| -> Result<(), ExnThrown> {
        let event = scope.root_value(js::JSString::from_str(scope, "beforeExit")?.as_value());
        let code = scope.root_value(js::value::from_i32(code.into()));
        Function::call_by_name(scope, process.handle(), c"emit", &[event, code])?;
        Ok(())
    })();
    if emitted.is_err() {
        uncaught(scope, js::exception::take_pending_or_undefined(scope), None);
    }
}

/// The `exit` lifecycle hook: emit `exit`, and end the process if the exit code isn't 0.
fn exit(scope: &Scope<'_>) {
    let Ok(process) = global_process(scope) else {
        return;
    };
    let code = emit_exit(scope, process);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    if code != 0 {
        platform::process::exit(code);
    }
}

/// Whether `process` has listeners for `event`.
fn has_listeners(scope: &Scope<'_>, process: Object<'_>, event: &str) -> Result<bool, ExnThrown> {
    let event = scope.root_value(js::JSString::from_str(scope, event)?.as_value());
    let count = Function::call_by_name(scope, process.handle(), c"listenerCount", &[event])?;
    Ok(count.to_number() > 0.0)
}

/// Emit `event` on `process` with `args`. Returns whether a listener threw.
fn emit(scope: &Scope<'_>, process: Object<'_>, event: &str, args: &[HandleValue<'_>]) -> bool {
    let emitted = (|| -> Result<(), ExnThrown> {
        let mut values = vec![scope.root_value(js::JSString::from_str(scope, event)?.as_value())];
        values.extend(args.iter().map(|arg| scope.root_value(arg.get())));
        Function::call_by_name(scope, process.handle(), c"emit", &values)?;
        Ok(())
    })();
    emitted.is_err()
}

/// The `rejection_handled` lifecycle hook, which does nothing.
fn rejection_handled(_scope: &Scope<'_>, _promise: js::Promise<'_>, _id: u64) {}

/// The `uncaught` lifecycle hook: emit `unhandledRejection` for a rejection if something listens,
/// or else call the `process.setUncaughtExceptionCaptureCallback` callback if one is set, or else
/// emit `uncaughtException` if something listens, and return `true`. If that listener or callback
/// throws, print its exception and exit with code 7. If nothing listens and the runtime runs a script as a
/// command, emit `exit` with code 1, print the error and exit, as Node.js does. In other runs,
/// return `false`, and the runtime reports the error.
///
/// With `--abort-on-uncaught-exception` and no capture callback, an exception aborts the process
/// after its message and stack frames are printed, without emitting `uncaughtException`, and a
/// rejection nothing handles aborts it after it is printed.
fn uncaught(
    scope: &Scope<'_>,
    error: HandleValue<'_>,
    rejection: Option<core_runtime::lifecycle::Rejection<'_>>,
) -> bool {
    let promise = rejection.map(|rejection| rejection.promise);
    let Ok(process) = global_process(scope) else {
        return false;
    };
    let abort = crate::runtime_flags::get().abort_on_uncaught_exception
        && capture_callback(scope).is_none();
    if abort && promise.is_none() {
        abort_uncaught(scope, error);
    }
    let threw = match promise {
        Some(promise) if has_listeners(scope, process, "unhandledRejection").unwrap_or(false) => {
            Some(emit(
                scope,
                process,
                "unhandledRejection",
                &[error, promise],
            ))
        }
        _ if capture_callback(scope).is_some() => {
            let callback = capture_callback(scope).expect("checked above");
            let undefined = scope.root_value(js::value::undefined());
            Some(Function::call(scope, undefined, callback, &[error]).is_err())
        }
        _ if has_listeners(scope, process, "uncaughtException").unwrap_or(false) => {
            let origin = if promise.is_some() {
                "unhandledRejection"
            } else {
                "uncaughtException"
            };
            match js::JSString::from_str(scope, origin) {
                Ok(origin) => {
                    let origin = scope.root_value(origin.as_value());
                    Some(emit(scope, process, "uncaughtException", &[error, origin]))
                }
                Err(_) => Some(true),
            }
        }
        _ => None,
    };
    match threw {
        Some(false) => true,
        Some(true) => {
            print_uncaught(scope);
            flush_and_exit(7)
        }
        None => {
            if !core_runtime::event_loop::running_as_command() {
                return false;
            }
            let text = uncaught_text(scope, error);
            // An exception an `exit` listener throws here is ignored.
            let _ = (|| -> Result<(), ExnThrown> {
                if !is_exiting(scope, process)? {
                    process.set_property(scope, c"_exiting", true)?;
                    process.set_property(scope, c"exitCode", 1)?;
                    let code = scope.root_value(js::value::from_i32(1));
                    emit(scope, process, "exit", &[code]);
                }
                Ok(())
            })();
            js::exception::clear(scope);
            eprintln!("{text}");
            if abort {
                platform::process::abort();
            }
            flush_and_exit(exit_code_if_set(scope, process).unwrap_or(1))
        }
    }
}

/// Print uncaught exception `error` as Node.js does under `--abort-on-uncaught-exception`, as
/// `Uncaught` followed by the error's string and the frames of its stack, and abort the process.
fn abort_uncaught(scope: &Scope<'_>, error: HandleValue<'_>) -> ! {
    let text = js::JSString::from_value(scope, error)
        .and_then(|text| text.to_utf8(scope))
        .unwrap_or_else(|_| crate::errors::describe_value(scope, error));
    let stack = Object::from_value(scope, error)
        .ok()
        .and_then(|error| error.get_property(scope, c"stack").ok())
        .and_then(|stack| String::from_jsval(scope, stack, ()).ok())
        .unwrap_or_default();
    js::exception::clear(scope);
    // A SpiderMonkey frame is `name@location`, which Node.js prints as `name (location)`.
    let frames: Vec<String> = stack
        .lines()
        .filter_map(|line| line.rsplit_once('@'))
        .map(|(name, location)| match name {
            "" => location.to_string(),
            name => format!("{name} ({location})"),
        })
        .collect();
    eprintln!("Uncaught {text}\n\nFROM\n{}", frames.join("\n"));
    platform::process::abort()
}

/// The exit code `value` specifies: an integer, or `process.exitCode` if it is `undefined`.
fn exit_code(
    scope: &Scope<'_>,
    process: Object<'_>,
    value: Option<HandleValue<'_>>,
) -> Result<u8, Thrown> {
    let value = match value.filter(|value| !value.is_undefined()) {
        Some(value) => value,
        None => process.get_property(scope, c"exitCode")?,
    };
    if value.is_undefined() || value.is_null() {
        return Ok(0);
    }
    let code = f64::from_jsval(scope, value, ())
        .ok()
        .filter(|code| code.fract() == 0.0)
        .ok_or_else(|| NodeError::invalid_arg_type(scope, "code", "of type number", value))?;
    Ok(code as i64 as u8)
}

/// Whether `value` is truthy, as JS's `ToBoolean` converts it.
fn truthy(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    bool::from_jsval(scope, value, ()).unwrap_or(false)
}

js::instance_local! {
    /// Set once a printed warning was followed by the hint about the tracing flags.
    static TRACE_HINT_SHOWN: Cell<bool> = const { Cell::new(false) };
}

/// The tick `process.emitWarning` queues, with the warning as its argument: print the warning to
/// stderr as Node.js's default `warning` listener does, unless `--no-warnings` was given, then
/// emit `warning` on `process`.
fn emit_warning_now(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let warning = args.get(0);
    if crate::runtime_flags::get().warnings {
        print_warning(scope, warning)?;
    }
    let process = global_process(scope)?;
    let event = scope.root_value(js::JSString::from_str(scope, "warning")?.as_value());
    Function::call_by_name(scope, process.handle(), c"emit", &[event, warning])?;
    Ok(js::value::undefined())
}

/// Print `warning` to stderr as Node.js's default `warning` listener does.
fn print_warning(scope: &Scope<'_>, warning: HandleValue<'_>) -> Result<(), ExnThrown> {
    let object = Object::from_value(scope, warning).map_err(|e| e.throw(scope))?;
    let code = object.get_property(scope, c"code")?;
    let code = if truthy(scope, code) {
        format!(
            "[{}] ",
            js::JSString::from_value(scope, code)?.to_utf8(scope)?
        )
    } else {
        String::new()
    };
    // A warning whose `toString` isn't a function is converted by `Error.prototype.toString`.
    let mut to_string = object.get_property(scope, c"toString")?;
    if !Object::from_value(scope, to_string).is_ok_and(|function| function.is_callable()) {
        let error = scope.global().get_property(scope, c"Error")?;
        let error = Object::from_value(scope, error).map_err(|e| e.throw(scope))?;
        let prototype = error.get_property(scope, c"prototype")?;
        let prototype = Object::from_value(scope, prototype).map_err(|e| e.throw(scope))?;
        to_string = prototype.get_property(scope, c"toString")?;
    }
    let text = Function::call(scope, warning, to_string, js::function::EmptyArgs)?;
    let text = js::JSString::from_value(scope, text)?.to_utf8(scope)?;
    let detail = object.get_property(scope, c"detail")?;
    let detail = if detail.is_string() {
        format!(
            "\n{}",
            String::from_jsval(scope, detail, ()).map_err(|e| e.throw(scope))?
        )
    } else {
        String::new()
    };
    let process = global_process(scope)?;
    // Node.js follows the first warning it prints with a hint about the tracing flags.
    let hint = if TRACE_HINT_SHOWN.replace(true) {
        String::new()
    } else {
        let name = object.get_property(scope, c"name")?;
        let deprecation = name.is_string()
            && String::from_jsval(scope, name, ()).is_ok_and(|name| name == "DeprecationWarning");
        let flag = if deprecation {
            "--trace-deprecation"
        } else {
            "--trace-warnings"
        };
        let argv0 = process.get_property(scope, c"argv0")?;
        let argv0 = String::from_jsval(scope, argv0, ()).unwrap_or_default();
        let argv0 = argv0
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("node");
        let argv0 = argv0.strip_suffix(".exe").unwrap_or(argv0);
        format!("\n(Use `{argv0} {flag} ...` to show where the warning was created)")
    };
    eprintln!("(node:{}) {code}{text}{detail}{hint}", pid());
    Ok(())
}

/// The key of the global's state object for `process.exitCode` and the uncaught exception
/// capture callback.
static PROCESS_STATE_KEY: u8 = 0;

/// The global's state object for `process.exitCode`, under `exitCode`, and the uncaught exception
/// capture callback, under `capture`.
fn process_state<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::get_or_init_shared_object(scope, &PROCESS_STATE_KEY as *const u8 as usize, |scope| {
        Object::new_plain(scope)
    })
}

/// The callback `process.setUncaughtExceptionCaptureCallback` set, if one is set.
fn capture_callback<'s>(scope: &'s Scope<'_>) -> Option<HandleValue<'s>> {
    let callback = process_state(scope)
        .ok()?
        .get_property(scope, c"capture")
        .ok()?;
    Object::from_value(scope, callback)
        .is_ok_and(|callback| callback.is_callable())
        .then_some(callback)
}

/// The `process.exitCode` getter.
fn get_exit_code(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(process_state(scope)?
        .get_property(scope, c"exitCode")?
        .get())
}

/// The `process.exitCode` setter: `undefined` for `null` and `undefined`, and otherwise an integer,
/// with a numeric string converted to a number, as Node.js validates it.
fn set_exit_code_property(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let code = args.get(0);
    let state = process_state(scope)?;
    if code.is_null() || code.is_undefined() {
        state.set_property(scope, c"exitCode", js::value::undefined())?;
        return Ok(js::value::undefined());
    }
    let mut value = code;
    if code.is_string() {
        let number = f64::from_jsval(scope, code, ()).map_err(|e| e.throw(scope))?;
        let empty = js::JSString::from_value(scope, code)?
            .to_utf8(scope)?
            .is_empty();
        if !empty && !number.is_nan() {
            value = scope.root_value(js::value::from_f64(number));
        }
    }
    const MAX_SAFE_INTEGER: f64 = 9007199254740991.0;
    let number =
        crate::errors::validate_integer(scope, value, "code", -MAX_SAFE_INTEGER, MAX_SAFE_INTEGER)
            .map_err(|error| error.throw(scope))?;
    state.set_property(scope, c"exitCode", number)?;
    Ok(js::value::undefined())
}

#[core_runtime::jsnamespace(node_args, name = "process")]
pub mod functions {
    use super::*;

    /// <https://nodejs.org/api/process.html#processcwd>
    pub fn cwd() -> String {
        super::cwd()
    }

    /// <https://nodejs.org/api/process.html#processgetactiveresourcesinfo>: `FSEventWrap` for each
    /// referenced `fs.watch` or `fsPromises.watch` watcher, `StatWatcher` for each referenced
    /// `fs.watchFile` watcher, `Timeout` for each referenced `setTimeout` and `setInterval` timer,
    /// and `Immediate` for each referenced `setImmediate`. Other resources aren't listed.
    pub fn get_active_resources_info<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        let (fs_events, stat_watchers) = crate::fs::active_watchers();
        let (timeouts, immediates) = crate::timers::active_timers();
        let names: Vec<&str> = std::iter::repeat_n("FSEventWrap", fs_events)
            .chain(std::iter::repeat_n("StatWatcher", stat_watchers))
            .chain(std::iter::repeat_n("Timeout", timeouts))
            .chain(std::iter::repeat_n("Immediate", immediates))
            .collect();
        Ok(*js::Array::with_contents(scope, &names)?)
    }

    /// <https://nodejs.org/api/process.html#processchdirdirectory>
    pub fn chdir(scope: &Scope<'_>, directory: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        let undefined = scope.root_value(js::value::undefined());
        let directory = validate_string(scope, directory.unwrap_or(undefined), "directory")?;
        if let Err(error) = std::env::set_current_dir(&directory) {
            let error = crate::fs::UvError::new(error, "chdir")
                .path(&super::cwd())
                .dest(&directory);
            return Err(js::error::ThrowException::throw(error, scope).into());
        }
        Ok(())
    }

    /// <https://nodejs.org/api/process.html#processexitcode>: sets `process.exitCode` to
    /// `code` if it is given.
    pub fn exit(scope: &Scope<'_>, code: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        let process = global_process(scope)?;
        if let Some(code) = code {
            if code.is_null() || code.is_undefined() {
                let getter = Function::new_callback(scope, c"exitCode", 0, get_exit_code, ())?;
                let setter =
                    Function::new_callback(scope, c"exitCode", 1, set_exit_code_property, ())?;
                process.define_accessor(
                    scope,
                    c"exitCode",
                    Some(*getter),
                    Some(*setter),
                    js::class_spec::JSPROP_ENUMERATE as u32,
                )?;
                process.set_property(scope, c"_exiting", false)?;
            } else {
                exit_code(scope, process, Some(code))?;
                process.set_property(scope, c"exitCode", code)?;
            }
        }
        exit_with(scope, process)
    }

    /// <https://nodejs.org/api/process.html#processabort>
    pub fn abort() {
        let _ = std::io::stderr().flush();
        #[cfg(target_arch = "wasm32")]
        platform::process::exit(134);
        #[cfg(not(target_arch = "wasm32"))]
        std::process::abort();
    }

    /// <https://nodejs.org/api/process.html#processnexttickcallback-args>: queued in the tick
    /// queue, which runs ahead of the microtask queue each time the microtasks run.
    pub fn next_tick(
        scope: &Scope<'_>,
        callback: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        validate_function(scope, callback, "callback")?;
        Ok(js::jobs::queue_tick(scope, callback, &args)?)
    }

    /// <https://nodejs.org/api/process.html#processhrtimetime>
    pub fn hrtime<'s>(
        scope: &'s Scope<'_>,
        time: Option<HandleValue<'_>>,
    ) -> Result<js::Array<'s>, Thrown> {
        let (seconds, nanos) = hrtime_pair(scope, time)?;
        Ok(js::Array::with_contents(scope, &[seconds, nanos])?)
    }

    /// <https://nodejs.org/api/process.html#processuptime>
    pub fn uptime() -> f64 {
        let start = START_NS.with(|start| *start.get().get_or_insert(0));
        platform::clock::monotonic_ns().saturating_sub(start) as f64 / 1e9
    }

    /// <https://nodejs.org/api/process.html#processmemoryusage>: the heap figures are not
    /// tracked, and read 0.
    pub fn memory_usage<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        let usage = Object::new_plain(scope)?;
        for name in [
            c"rss",
            c"heapTotal",
            c"heapUsed",
            c"external",
            c"arrayBuffers",
        ] {
            usage.set_property(scope, name, 0)?;
        }
        Ok(usage)
    }

    /// <https://nodejs.org/api/process.html#processcpuusagepreviousvalue>: not tracked, and reads
    /// 0.
    pub fn cpu_usage<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        let usage = Object::new_plain(scope)?;
        usage.set_property(scope, c"user", 0)?;
        usage.set_property(scope, c"system", 0)?;
        Ok(usage)
    }

    /// <https://nodejs.org/api/process.html#processemitwarningwarning-options>: emits `warning`
    /// on `process` in a tick, after printing the warning to stderr.
    pub fn emit_warning(
        scope: &Scope<'_>,
        warning: Option<HandleValue<'_>>,
        kind: Option<HandleValue<'_>>,
        code: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let undefined = scope.root_value(js::value::undefined());
        let warning = warning.unwrap_or(undefined);
        let mut kind = kind.unwrap_or(undefined);
        let mut code = code.unwrap_or(undefined);
        let mut detail = None;
        if let Ok(options) = Object::from_value(scope, kind) {
            if options.is_callable() {
                kind = undefined;
                code = undefined;
            } else if !js::Array::is_array(scope, options.handle())? {
                code = options.get_property(scope, c"code")?;
                let value = options.get_property(scope, c"detail")?;
                if value.is_string() {
                    detail = Some(value);
                }
                kind = options.get_property(scope, c"type")?;
                if !truthy(scope, kind) {
                    kind = undefined;
                }
            }
        }
        let kind = if kind.is_undefined() {
            "Warning".to_string()
        } else {
            validate_string(scope, kind, "type")?
        };
        let code = if code.is_undefined()
            || Object::from_value(scope, code).is_ok_and(|code| code.is_callable())
        {
            None
        } else {
            Some(validate_string(scope, code, "code")?)
        };
        let warning = if warning.is_string() {
            let message = js::JSString::from_value(scope, warning)?.to_utf8(scope)?;
            let message =
                std::ffi::CString::new(message.replace('\0', "\\0")).expect("NULs were replaced");
            let error = js::error::throw_plain_error(scope, &message);
            let error = js::exception::take_pending(scope).ok_or(error)?;
            let object = Object::from_value(scope, error)?;
            object.set_property(scope, c"name", kind.as_str())?;
            if let Some(code) = &code {
                object.set_property(scope, c"code", code.as_str())?;
            }
            if let Some(detail) = detail {
                object.set_property(scope, c"detail", detail)?;
            }
            error
        } else if js::exception::is_error_object(scope, warning) {
            warning
        } else {
            return Err(NodeError::invalid_arg_type(
                scope,
                "warning",
                "of type string or an instance of Error",
                warning,
            )
            .into());
        };
        let process = global_process(scope)?;
        let object = Object::from_value(scope, warning)?;
        let name = object.get_property(scope, c"name")?;
        if name.is_string() && String::from_jsval(scope, name, ())? == "DeprecationWarning" {
            if truthy(scope, process.get_property(scope, c"noDeprecation")?) {
                return Ok(());
            }
            if truthy(scope, process.get_property(scope, c"throwDeprecation")?) {
                js::exception::set_pending(
                    scope,
                    warning,
                    js::native::ExceptionStackBehavior::Capture,
                );
                return Err(Thrown::Pending);
            }
        }
        Ok(js::jobs::queue_native_tick(
            scope,
            emit_warning_now,
            (),
            &[warning],
        )?)
    }

    /// <https://nodejs.org/api/process.html#processumaskmask>: always the default `0o022`.
    pub fn umask() -> u32 {
        0o022
    }

    /// <https://nodejs.org/api/process.html#processgetuid>: 0.
    pub fn getuid() -> u32 {
        0
    }

    /// <https://nodejs.org/api/process.html#processgeteuid>: 0.
    pub fn geteuid() -> u32 {
        0
    }

    /// <https://nodejs.org/api/process.html#processgetgid>: 0.
    pub fn getgid() -> u32 {
        0
    }

    /// <https://nodejs.org/api/process.html#processgetegid>: 0.
    pub fn getegid() -> u32 {
        0
    }

    /// <https://nodejs.org/api/process.html#processsetuncaughtexceptioncapturecallbackfn>
    pub fn set_uncaught_exception_capture_callback(
        scope: &Scope<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let callback = callback.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        let state = process_state(scope)?;
        if callback.is_null() {
            state.set_property(scope, c"capture", js::value::undefined())?;
            return Ok(());
        }
        if !Object::from_value(scope, callback).is_ok_and(|callback| callback.is_callable()) {
            return Err(NodeError::invalid_arg_type(
                scope,
                "fn",
                "of type function or null",
                callback,
            )
            .into());
        }
        if capture_callback(scope).is_some() {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_UNCAUGHT_EXCEPTION_CAPTURE_ALREADY_SET",
                "`process.setupUncaughtExceptionCapture()` was called while a capture callback \
                 was already active",
            )
            .into());
        }
        Ok(state.set_property(scope, c"capture", callback)?)
    }

    /// <https://nodejs.org/api/process.html#processhasuncaughtexceptioncapturecallback>
    pub fn has_uncaught_exception_capture_callback(scope: &Scope<'_>) -> bool {
        capture_callback(scope).is_some()
    }

    /// <https://nodejs.org/api/process.html#processgetbuiltinmoduleid>: the builtin module `id`
    /// names, as `require` returns it, or `undefined` if it names none.
    pub fn get_builtin_module<'s>(
        scope: &'s Scope<'_>,
        id: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let id = validate_string(scope, id, "id")?;
        match core_runtime::module::builtin_namespace(scope, &id)? {
            Some(namespace) => Ok(namespace.get_property(scope, c"default")?),
            None => Ok(scope.root_value(js::value::undefined())),
        }
    }

    /// `process.binding(name)`, Node.js's deprecated access to its internal bindings: the binding
    /// objects of `internal/test/binding`'s `internalBinding`, for the names Node.js allows.
    pub fn binding<'s>(scope: &'s Scope<'_>, name: String) -> Result<Object<'s>, Thrown> {
        crate::internals::process_binding(scope, &name)
    }

    /// <https://nodejs.org/api/process.html#processkillpid-signal>: there are no other processes
    /// to signal.
    pub fn kill() -> Result<(), NodeError> {
        Err(NodeError::not_implemented("process.kill()"))
    }
}

/// An accessor property of `process` whose value is computed again after each resume.
struct LazyProperty {
    name: &'static CStr,
    compute: fn(&Scope<'_>) -> Result<Value, ExnThrown>,
}

const LAZY_PROPERTIES: &[LazyProperty] = &[
    LazyProperty {
        name: c"env",
        compute: compute_env,
    },
    LazyProperty {
        name: c"argv",
        compute: compute_argv,
    },
    LazyProperty {
        name: c"execPath",
        compute: compute_exec_path,
    },
];

/// A proxy that `process.env` or `process.argv` holds, whose target [`HostView::fill`] fills with
/// the host's values on the proxy's first use after each resume.
struct HostView {
    /// The target's kind: an array if `true`, and a plain object otherwise.
    array: bool,
    /// Replaces the target's contents with the host's values.
    fill: fn(&Scope<'_>, Object<'_>) -> Result<(), ExnThrown>,
    /// Whether the `set` and `defineProperty` traps convert the value to a string.
    coerce: bool,
}

const HOST_VIEWS: &[HostView] = &[
    HostView {
        array: false,
        fill: fill_env,
        coerce: true,
    },
    HostView {
        array: true,
        fill: fill_argv,
        coerce: false,
    },
];

/// The keys of the [`HOST_VIEWS`] proxies in the global's shared objects.
static HOST_VIEW_KEYS: [u8; 2] = [0, 0];

/// The traps of a [`HostView`] proxy, each of which calls the `Reflect` function of its name.
const HOST_VIEW_TRAPS: [&CStr; 7] = [
    c"get",
    c"set",
    c"has",
    c"deleteProperty",
    c"defineProperty",
    c"ownKeys",
    c"getOwnPropertyDescriptor",
];

/// The global's proxy for the [`HostView`] at `index` in [`HOST_VIEWS`], created on first use.
fn host_view<'s>(scope: &'s Scope<'_>, index: usize) -> Result<Object<'s>, ExnThrown> {
    let key = &HOST_VIEW_KEYS[index] as *const u8 as usize;
    js::class::get_or_init_shared_object(scope, key, |scope| {
        let target = if HOST_VIEWS[index].array {
            *js::Array::new(scope, 0)?
        } else {
            Object::new_plain(scope)?
        };
        let state = Object::new_plain(scope)?;
        state.set_property(scope, c"index", index as u32)?;
        state.set_property(scope, c"target", target)?;
        let handler = Object::new_plain(scope)?;
        for (trap, name) in HOST_VIEW_TRAPS.iter().enumerate() {
            let payload = js::Array::with_contents(
                scope,
                &[
                    state.to_jsval_raw_throwing(scope)?,
                    js::value::from_i32(trap as i32),
                ],
            )?;
            let function = Function::new_callback(scope, name, 0, host_view_trap, *payload)?;
            handler.set_property(scope, name, *function)?;
        }
        let proxy = scope.global().get_property(scope, c"Proxy")?;
        let proxy = Function::construct(
            scope,
            Object::from_value(scope, proxy).map_err(|e| e.throw(scope))?,
            &[
                target.to_jsval_throwing(scope)?,
                handler.to_jsval_throwing(scope)?,
            ],
        )?;
        Ok(proxy)
    })
}

js::instance_local! {
    /// Set once DEP0104 was emitted.
    static ENV_VALUE_WARNED: Cell<bool> = const { Cell::new(false) };
}

/// A trap of a [`HostView`] proxy, whose payload is `[state, trap]`: fill the target if this is
/// the proxy's first use since the last resume, then call the `Reflect` function named by the
/// trap's index in [`HOST_VIEW_TRAPS`] with the trap's arguments, after converting the value to a
/// string if the view coerces it. With `--pending-deprecation`, setting a value that isn't a
/// string, number or boolean emits DEP0104 once per instance.
fn host_view_trap(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let payload = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let state =
        Object::from_value(scope, payload.get_element(scope, 0)?).map_err(|e| e.throw(scope))?;
    let trap = payload.get_element(scope, 1)?.to_int32() as usize;
    let view = &HOST_VIEWS[state.get_property(scope, c"index")?.to_int32() as usize];
    let target = Object::from_value(scope, state.get_property(scope, c"target")?)
        .map_err(|e| e.throw(scope))?;
    let generation = RESUME_GENERATION.with(Cell::get);
    let filled = state.get_property(scope, c"generation")?;
    if !(filled.is_number() && filled.to_number() == f64::from(generation)) {
        (view.fill)(scope, target)?;
        state.set_property(scope, c"generation", generation)?;
    }
    let mut call_args: Vec<HandleValue<'_>> =
        (0..args.len()).map(|index| args.get(index)).collect();
    let name = HOST_VIEW_TRAPS[trap];
    if view.coerce && name == c"set" && call_args.len() > 2 {
        let value = call_args[2];
        if !(value.is_string() || value.is_number() || value.is_boolean())
            && crate::runtime_flags::get().pending_deprecation
            && !ENV_VALUE_WARNED.replace(true)
        {
            deprecation_warning(
                scope,
                "Assigning any value other than a string, number, or boolean to a process.env \
                 property is deprecated. Please make sure to convert the value to a string before \
                 setting process.env with it.",
                "DEP0104",
            )?;
        }
        let value = js::JSString::from_value(scope, call_args[2])?;
        call_args[2] = value.to_jsval_throwing(scope)?;
        call_args.truncate(3);
    }
    if view.coerce && name == c"defineProperty" && call_args.len() > 2 {
        if let Ok(descriptor) = Object::from_value(scope, call_args[2]) {
            if descriptor.has_property(scope, c"value")? {
                let value = descriptor.get_property(scope, c"value")?;
                let value = js::JSString::from_value(scope, value)?;
                let copy = Object::new_plain(scope)?;
                for field in [c"writable", c"enumerable", c"configurable"] {
                    if descriptor.has_property(scope, field)? {
                        copy.set_property(scope, field, descriptor.get_property(scope, field)?)?;
                    }
                }
                copy.set_property(scope, c"value", value.to_jsval_throwing(scope)?)?;
                call_args[2] = copy.to_jsval_throwing(scope)?;
            }
        }
    }
    let reflect = scope.global().get_property(scope, c"Reflect")?;
    let reflect = Object::from_value(scope, reflect).map_err(|e| e.throw(scope))?;
    let function = reflect.get_property(scope, name)?;
    let undefined = scope.root_value(js::value::undefined());
    Ok(Function::call(scope, undefined, function, &call_args)?.get())
}

/// Replace the contents of `env`, a plain object, with the environment variables.
fn fill_env(scope: &Scope<'_>, env: Object<'_>) -> Result<(), ExnThrown> {
    let keys = env.own_keys(scope)?;
    for key in keys {
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        env.delete_property_by_id(scope, id)?;
    }
    for (key, value) in platform::process::env_vars() {
        let Ok(key) = std::ffi::CString::new(key) else {
            continue;
        };
        env.set_property_by_name(scope, &key, value.as_str())?;
    }
    Ok(())
}

/// Replace the contents of `argv`, an array, with the program and the entry module's path, if it
/// has one.
fn fill_argv(scope: &Scope<'_>, argv: Object<'_>) -> Result<(), ExnThrown> {
    argv.set_property(scope, c"length", 0)?;
    argv.set_element(scope, 0, exec_path().as_str())?;
    if let Some(entry) = core_runtime::module::entry_path() {
        argv.set_element(scope, 1, entry.as_str())?;
    }
    Ok(())
}

/// `process.env`: a proxy of an object holding each environment variable.
fn compute_env(scope: &Scope<'_>) -> Result<Value, ExnThrown> {
    host_view(scope, 0)?.to_jsval_raw_throwing(scope)
}

/// `process.argv`: a proxy of an array of the program and the entry module's path, if it has one.
fn compute_argv(scope: &Scope<'_>) -> Result<Value, ExnThrown> {
    host_view(scope, 1)?.to_jsval_raw_throwing(scope)
}

/// `process.execPath`: [`exec_path`].
fn compute_exec_path(scope: &Scope<'_>) -> Result<Value, ExnThrown> {
    exec_path().as_str().to_jsval_raw_throwing(scope)
}

/// `process.execPath`: the program's path, or its name if the host gives no path.
fn exec_path() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(path) = std::env::current_exe() {
        return path.to_string_lossy().into_owned();
    }
    platform::process::args()
        .into_iter()
        .next()
        .unwrap_or_else(|| "starlingmonkey".into())
}

/// The getter of a [`LazyProperty`], whose payload is its state object: the property's index in
/// [`LAZY_PROPERTIES`], and the cached value and the resume generation it was computed in.
fn lazy_get(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let generation = RESUME_GENERATION.with(Cell::get);
    let cached = state.get_property(scope, c"generation")?;
    if f64::from_jsval(scope, cached, ()).ok() == Some(generation.into()) {
        return Ok(state.get_property(scope, c"value")?.get());
    }
    let index = f64::from_jsval(scope, state.get_property(scope, c"index")?, ()).unwrap_or(0.0);
    let value = scope.root_value((LAZY_PROPERTIES[index as usize].compute)(scope)?);
    state.set_property(scope, c"value", value)?;
    state.set_property(scope, c"generation", generation)?;
    Ok(value.get())
}

/// The setter of a [`LazyProperty`]: replace the value until the next resume.
fn lazy_set(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    state.set_property(scope, c"value", args.get(0))?;
    state.set_property(scope, c"generation", RESUME_GENERATION.with(Cell::get))?;
    Ok(js::value::undefined())
}

/// An object with the standard stream `fd`'s `write` and `isTTY`, an `EventEmitter`. `isTTY` is
/// an accessor that checks the stream on each read, until it is assigned.
fn standard_stream<'s>(scope: &'s Scope<'_>, fd: i32) -> Result<Object<'s>, ExnThrown> {
    let stream = Function::construct(
        scope,
        crate::events::emitter_constructor(scope)?,
        &[] as &[Value],
    )?;
    stream.set_property(scope, c"fd", fd)?;
    let getter = Function::new_callback(scope, c"isTTY", 0, stream_is_tty, fd)?;
    let setter = Function::new_callback(scope, c"isTTY", 1, stream_set_is_tty, ())?;
    stream.define_accessor(
        scope,
        c"isTTY",
        Some(*getter),
        Some(*setter),
        js::class_spec::JSPROP_ENUMERATE as u32,
    )?;
    stream.set_property(scope, c"writable", true)?;
    let write = Function::new_callback(scope, c"write", 1, stream_write, fd)?;
    stream.set_property(scope, c"write", *write)?;
    Ok(stream)
}

/// The `isTTY` getter of a standard stream, whose payload is its file descriptor: `true` if the
/// stream is a terminal, and `undefined` otherwise.
fn stream_is_tty(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let is_tty = match payload.to_int32() {
        1 => std::io::stdout().is_terminal(),
        _ => std::io::stderr().is_terminal(),
    };
    Ok(if is_tty {
        js::value::from_bool(true)
    } else {
        js::value::undefined()
    })
}

/// The `isTTY` setter of a standard stream: replace the accessor with a data property holding the
/// value.
fn stream_set_is_tty(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    stream.define_property(
        scope,
        c"isTTY",
        args.get(0),
        js::class_spec::JSPROP_ENUMERATE as u32,
    )?;
    Ok(js::value::undefined())
}

/// `stdout.write(chunk[, encoding][, callback])` and `stderr.write`: write the string or bytes
/// `chunk` to the standard stream numbered by the payload, then call `callback`.
fn stream_write(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let chunk = args.get(0);
    let bytes = if chunk.is_string() {
        String::from_jsval(scope, chunk, ())
            .map_err(|e| e.throw(scope))?
            .into_bytes()
    } else if let Some(view) = Object::from_value(scope, chunk)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
    {
        view.with_bytes(<[u8]>::to_vec)
    } else {
        return Err(NodeError::invalid_arg_type(
            scope,
            "chunk",
            "of type string or an instance of Buffer or Uint8Array",
            chunk,
        )
        .throw(scope));
    };
    let result = if payload.to_int32() == 1 {
        let mut stdout = std::io::stdout();
        stdout.write_all(&bytes).and_then(|()| stdout.flush())
    } else {
        std::io::stderr().write_all(&bytes)
    };
    let callback = (1..args.len())
        .map(|index| args.get(index))
        .find(|arg| Object::from_value(scope, *arg).is_ok_and(|arg| arg.is_callable()));
    if let Some(callback) = callback {
        Function::call(scope, js::value::undefined(), callback, &[] as &[Value])?;
    }
    Ok(js::value::from_bool(result.is_ok()))
}

/// `process.config`: the build configuration Node.js reports, with the variables that describe
/// this runtime. Intl is available, and there is no OpenSSL, inspector or shared library.
fn config<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let variables = Object::new_plain(scope)?;
    variables.set_property(scope, c"v8_enable_i18n_support", 1)?;
    variables.set_property(scope, c"node_shared", false)?;
    variables.set_property(scope, c"node_use_openssl", false)?;
    variables.set_property(scope, c"node_shared_openssl", false)?;
    variables.set_property(scope, c"asan", 0)?;
    variables.set_property(scope, c"host_arch", arch())?;
    variables.set_property(scope, c"target_arch", arch())?;
    let target_defaults = Object::new_plain(scope)?;
    target_defaults.set_property(scope, c"default_configuration", "Release")?;
    let config = Object::new_plain(scope)?;
    config.set_property(scope, c"target_defaults", target_defaults)?;
    config.set_property(scope, c"variables", variables)?;
    Ok(config)
}

/// Install the `process` global on `global`: the [`functions`] namespace, made an `EventEmitter`
/// and given its data properties, and stubs for the other members Node.js's `process` has.
pub fn install(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    functions::add_to_global(scope, global);
    let process = global_process(scope)?;

    let emitter = crate::events::emitter_constructor(scope)?;
    let proto = emitter.get_property(scope, c"prototype")?;
    let proto = Object::from_value(scope, proto).map_err(|e| e.throw(scope))?;
    process.set_prototype(scope, proto.handle())?;
    Function::call(scope, process, emitter, &[] as &[Value])?;

    let hrtime = process.get_property(scope, c"hrtime")?;
    let hrtime = Object::from_value(scope, hrtime).map_err(|e| e.throw(scope))?;
    let bigint = Function::new_callback(scope, c"bigint", 0, hrtime_bigint, ())?;
    hrtime.set_property(scope, c"bigint", *bigint)?;

    let version = format!("v{NODE_VERSION}");
    process.set_property(scope, c"version", version.as_str())?;
    let versions = Object::new_plain(scope)?;
    versions.set_property(scope, c"node", NODE_VERSION)?;
    versions.set_property(scope, c"starlingmonkey", env!("CARGO_PKG_VERSION"))?;
    process.set_property(scope, c"versions", versions)?;
    let release = Object::new_plain(scope)?;
    release.set_property(scope, c"name", "node")?;
    // The codename of the LTS line `NODE_VERSION` belongs to.
    release.set_property(scope, c"lts", "Krypton")?;
    process.set_property(scope, c"release", release)?;
    process.set_property(scope, c"platform", platform())?;
    process.set_property(scope, c"arch", arch())?;
    process.set_property(scope, c"pid", pid())?;
    process.set_property(scope, c"ppid", 0)?;
    let flags = crate::runtime_flags::get();
    let title = flags.title.as_deref().unwrap_or("starlingmonkey");
    process.set_property(scope, c"title", title)?;
    process.set_property(scope, c"argv0", "starlingmonkey")?;
    let exec_argv = flags
        .exec_argv
        .iter()
        .map(|flag| flag.as_str().to_jsval_throwing(scope))
        .collect::<Result<Vec<_>, _>>()?;
    let exec_argv = js::Array::with_contents(scope, &exec_argv)?;
    process.set_property(scope, c"execArgv", exec_argv)?;
    if !flags.warnings {
        process.set_property(scope, c"noProcessWarnings", true)?;
    }
    if flags.no_deprecation {
        process.set_property(scope, c"noDeprecation", true)?;
    }
    if flags.throw_deprecation {
        process.set_property(scope, c"throwDeprecation", true)?;
    }
    let getter = Function::new_callback(scope, c"exitCode", 0, get_exit_code, ())?;
    let setter = Function::new_callback(scope, c"exitCode", 1, set_exit_code_property, ())?;
    process.define_accessor(
        scope,
        c"exitCode",
        Some(*getter),
        Some(*setter),
        js::class_spec::JSPROP_ENUMERATE as u32,
    )?;
    process.set_property(scope, c"_exiting", false)?;
    process.set_property(scope, c"config", config(scope)?)?;
    let features = Object::new_plain(scope)?;
    for feature in [c"inspector", c"ipv6", c"tls", c"typescript"] {
        features.set_property(scope, feature, false)?;
    }
    process.set_property(scope, c"features", features)?;
    process.set_property(scope, c"stdout", standard_stream(scope, 1)?)?;
    process.set_property(scope, c"stderr", standard_stream(scope, 2)?)?;
    crate::tty::define_stdin(scope, process)?;
    core_runtime::lifecycle::set_hooks(core_runtime::lifecycle::Hooks {
        before_exit,
        exit,
        uncaught,
        rejection_handled,
    });
    js::jobs::install_tick_queue(
        scope,
        global,
        core_runtime::event_loop::handle_and_clear_exception,
    )?;

    for (index, property) in LAZY_PROPERTIES.iter().enumerate() {
        let state = Object::new_plain(scope)?;
        state.set_property(scope, c"index", index as u32)?;
        let getter = Function::new_callback(scope, property.name, 0, lazy_get, state)?;
        let setter = Function::new_callback(scope, property.name, 1, lazy_set, state)?;
        process.define_accessor(
            scope,
            property.name,
            Some(*getter),
            Some(*setter),
            js::class_spec::JSPROP_ENUMERATE as u32,
        )?;
    }
    START_NS.with(|start| {
        if start.get().is_none() {
            start.set(Some(platform::clock::monotonic_ns()));
        }
    });
    for &(export, kind) in crate::stub::exports_of("node:process").unwrap_or_default() {
        let c_export = std::ffi::CString::new(export).map_err(|_| ExnThrown)?;
        if !process.has_property_by_name(scope, &c_export)? {
            let stub = crate::stub::stub_value(scope, "node:process", export, kind)?;
            process.set_property_by_name(scope, &c_export, stub)?;
        }
    }
    Ok(())
}

/// Register `node:process`, whose `default` export is the global's `process`, and whose named
/// exports are its properties.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let register = || -> Result<(), ExnThrown> {
        let process = global_process(scope)?;
        let exports = crate::stub::exports_of(name).unwrap_or_default();
        let mut values = Vec::with_capacity(exports.len() + 1);
        for &(export, _) in exports {
            let c_export = std::ffi::CString::new(export).map_err(|_| ExnThrown)?;
            values.push((export, process.get_property_by_name(scope, &c_export)?));
        }
        values.push(("default", process.to_jsval_throwing(scope)?));
        core_runtime::module::register_synthetic_module(scope, name, &values)
    };
    register().is_ok()
}
