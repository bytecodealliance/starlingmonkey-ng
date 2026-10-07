// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `spawnSync`, `execSync` and `execFileSync`, which block until the child process exits. Ported
//! from Node.js's `lib/child_process.js` and its `spawn_sync` binding.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Function, Object};
use platform::child::SyncOptions;
use platform::fs::FsError;

use super::child::{errno_exception, rest_of, runtime_args, throw_object};
use super::options;
use crate::errors::{NodeError, Thrown};

/// The strings of the array `value`, converted as `String(element)` converts them.
fn strings(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Vec<String>, ExnThrown> {
    crate::support::array_elements(scope, value)?
        .unwrap_or_default()
        .iter()
        .map(|value| crate::errors::js_string(scope, *value))
        .collect()
}

/// `child_process.spawnSync(file, args, options)`, as Node.js's `spawnSync`: spawn `file` as
/// `spawn` does, write `options.input` to its standard input, and return its exit code or
/// signal, its output, and the error of a spawn that failed, timed out or produced more than
/// `options.maxBuffer` bytes, in an object.
pub(super) fn spawn_sync<'s>(
    scope: &'s Scope<'_>,
    file: HandleValue<'_>,
    args: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let options = options::normalize_spawn_arguments(scope, file, args, options)?;
    let max_buffer = if options.has_own_property(scope, c"maxBuffer")? {
        options.get_property(scope, c"maxBuffer")?
    } else {
        scope.root_value(js::value::from_f64(super::exec::MAX_BUFFER))
    };
    let timeout = options::timeout(scope, options.get_property(scope, c"timeout")?)?;
    options::max_buffer(scope, max_buffer)?;
    let kill_signal = options::kill_signal(scope, options.get_property(scope, c"killSignal")?)?;
    let mut stdio = options.get_property(scope, c"stdio")?;
    if !js::conversion::to_boolean(stdio) {
        stdio = "pipe".to_jsval_throwing(scope)?;
    }
    let entries = options::valid_stdio(scope, stdio, true)?;
    let encoding = options.get_property(scope, c"encoding")?;
    let input = options.get_property(scope, c"input")?;
    let input = if js::conversion::to_boolean(input) {
        let bytes = crate::support::chunk_bytes(scope, input, Some(encoding))?;
        let Some((bytes, _)) = bytes else {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options.stdio[0]",
                "of type string or an instance of Buffer, TypedArray, or DataView",
                input,
            )
            .into());
        };
        Some(bytes)
    } else {
        None
    };
    if platform::child::SUPPORTED {
        if let Some(error) = crate::errors::blocking_not_allowed("child_process.spawnSync") {
            return Err(error.into());
        }
    }
    let file = String::from_jsval(scope, options.get_property(scope, c"file")?, ())?;
    let args_value = options.get_property(scope, c"args")?;
    let args = strings(scope, args_value)?;
    let cwd = options::cwd(scope, options.get_property(scope, c"cwd")?);
    let env = strings(scope, options.get_property(scope, c"envPairs")?)?
        .into_iter()
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (pair, String::new()),
        })
        .collect();
    let id = |name| -> Result<Option<u32>, ExnThrown> {
        let value = options.get_property(scope, name)?;
        Ok(options::is_int32(value).then(|| value.to_number() as i32 as u32))
    };
    let (descriptors, bad_fd) = options::host_stdio(scope, &entries)?;
    let max_buffer = if max_buffer.is_number() {
        let max = max_buffer.to_number();
        (max > 0.0 && max.is_finite()).then_some(max as usize)
    } else {
        None
    };
    let spawn_options = platform::child::Options {
        file: file.clone(),
        args: runtime_args(&file, args),
        cwd,
        env: Some(env),
        stdio: descriptors,
        detached: js::conversion::to_boolean(options.get_property(scope, c"detached")?),
        uid: id(c"uid")?,
        gid: id(c"gid")?,
    };
    let sync = SyncOptions {
        input: vec![input],
        timeout: timeout
            .filter(|timeout| *timeout > 0.0)
            .map(|timeout| std::time::Duration::from_millis(timeout as u64)),
        max_buffer,
        kill_signal: kill_signal
            .or_else(|| crate::os::signal_number("SIGTERM"))
            .unwrap_or(15),
    };
    let outcome = if bad_fd {
        platform::child::SyncOutput {
            error: Some(std::io::Error::from_raw_os_error(ebadf())),
            ..Default::default()
        }
    } else {
        platform::child::run_sync(&spawn_options, sync)
    };
    let result = Object::new_plain(scope)?;
    if let Some(error) = &outcome.error {
        let error = FsError::from_io(error);
        let error = errno_exception(scope, error, &format!("spawnSync {file}"))?;
        error.set_property(scope, c"path", file.as_str())?;
        let args_array = Object::from_value(scope, *args_value).ok();
        error.set_property(scope, c"spawnargs", rest_of(scope, args_array)?)?;
        result.set_property(scope, c"error", error)?;
    }
    let spawned = outcome.status.is_some();
    let (status, signal) = match outcome.status {
        Some(status) => match status.signal.and_then(crate::os::signal_name) {
            Some(name) => (HandleValue::null(), name.to_jsval_throwing(scope)?),
            None => (
                scope.root_value(js::value::from_f64(f64::from(status.code.unwrap_or(0)))),
                HandleValue::null(),
            ),
        },
        None => (HandleValue::null(), HandleValue::null()),
    };
    result.set_property(scope, c"status", status)?;
    result.set_property(scope, c"signal", signal)?;
    let encoding = crate::support::rust_string(scope, encoding).filter(|name| name != "buffer");
    let output = if spawned {
        let mut values = Vec::new();
        if values.try_reserve_exact(outcome.output.len()).is_err() {
            js::exception::report_out_of_memory(scope);
            return Err(ExnThrown.into());
        }
        for bytes in &outcome.output {
            values.push(match bytes {
                Some(bytes) => decode(scope, bytes, encoding.as_deref())?,
                None => HandleValue::null(),
            });
        }
        js::Array::with_contents(scope, &values)?.to_jsval_throwing(scope)?
    } else {
        HandleValue::null()
    };
    result.set_property(scope, c"output", output)?;
    result.set_property(scope, c"pid", f64::from(outcome.pid))?;
    let stream = |index: u32| -> Result<HandleValue<'s>, ExnThrown> {
        match Object::from_value(scope, *output) {
            Ok(output) => output.get_element(scope, index),
            Err(_) => Ok(HandleValue::undefined()),
        }
    };
    result.set_property(scope, c"stdout", stream(1)?)?;
    result.set_property(scope, c"stderr", stream(2)?)?;
    Ok(result)
}

/// `EBADF`, the error of a descriptor in `stdio` that isn't open.
fn ebadf() -> i32 {
    -FsError::from_code("EBADF").errno
}

/// The output `bytes` as a `Buffer`, or as a string in `encoding`.
fn decode<'s>(
    scope: &'s Scope<'_>,
    bytes: &[u8],
    encoding: Option<&str>,
) -> Result<HandleValue<'s>, Thrown> {
    let Some(name) = encoding else {
        return Ok(crate::buffer::new_buffer(scope, bytes)?.to_jsval_throwing(scope)?);
    };
    let Some(encoding) = crate::buffer::Encoding::parse(name) else {
        let name = name.to_jsval_throwing(scope)?;
        return Err(crate::buffer::unknown_encoding(scope, name));
    };
    Ok(crate::buffer::decode_bytes(scope, bytes, encoding)?.to_jsval_throwing(scope)?)
}

/// Node.js's `checkExecSyncError`: the error of the `spawnSync` result `result`, with the
/// result's properties, or "Command failed" with `command` for an exit with a status other than
/// 0, or `None`.
fn check_exec_sync_error<'s>(
    scope: &'s Scope<'_>,
    result: Object<'s>,
    command: &str,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let error = result.get_property(scope, c"error")?;
    if let Ok(error) = Object::from_value(scope, *error) {
        error.assign(scope, result)?;
        return Ok(Some(error));
    }
    let status = result.get_property(scope, c"status")?;
    if status.is_number() && status.to_number() == 0.0 {
        return Ok(None);
    }
    let mut message = format!("Command failed: {command}");
    let stderr = result.get_property(scope, c"stderr")?;
    let length = match Object::from_value(scope, *stderr) {
        Ok(stderr) => stderr.get_property(scope, c"length")?,
        Err(_) if stderr.is_string() => {
            let length = js::JSString::from_value(scope, stderr)?.len();
            scope.root_value(js::value::from_f64(length as f64))
        }
        Err(_) => HandleValue::undefined(),
    };
    if js::conversion::to_boolean(stderr) && length.is_number() && length.to_number() > 0.0 {
        let text = match Object::from_value(scope, *stderr) {
            Ok(stderr) => Function::call_by_name(
                scope,
                stderr.handle(),
                c"toString",
                js::function::EmptyArgs,
            )?,
            Err(_) => stderr,
        };
        message.push('\n');
        message.push_str(&crate::errors::js_string(scope, text)?);
    }
    let error = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, &message)?;
    let error = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
    error.assign(scope, result)?;
    Ok(Some(error))
}

/// Write the `stderr` of the `spawnSync` result `result` to `process.stderr`, if it has one.
fn forward_stderr(scope: &Scope<'_>, result: Object<'_>) -> Result<(), ExnThrown> {
    let stderr = result.get_property(scope, c"stderr")?;
    if !js::conversion::to_boolean(stderr) {
        return Ok(());
    }
    let process = crate::process::process_value(scope)?;
    let Ok(process) = Object::from_value(scope, *process) else {
        return Ok(());
    };
    let Ok(stream) = Object::from_value(scope, *process.get_property(scope, c"stderr")?) else {
        return Ok(());
    };
    Function::call_by_name(scope, stream.handle(), c"write", &[stderr])?;
    Ok(())
}

/// `child_process.execFileSync(file, args, options)`, as Node.js's `execFileSync`: `spawnSync`,
/// with the error output written to `process.stderr` unless `options.stdio` is given, and the
/// error thrown. Returns the output.
pub(super) fn exec_file_sync<'s>(
    scope: &'s Scope<'_>,
    file: HandleValue<'_>,
    args: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<HandleValue<'s>, Thrown> {
    let (args, options, _) =
        super::exec::normalize_exec_file_args(scope, args, options, HandleValue::undefined())?;
    let inherit_stderr = match options {
        Some(options) => !js::conversion::to_boolean(options.get_property(scope, c"stdio")?),
        None => true,
    };
    let options_value = match options {
        Some(options) => options.to_jsval_throwing(scope)?,
        None => HandleValue::undefined(),
    };
    let result = spawn_sync(scope, file, args, options_value)?;
    if inherit_stderr {
        forward_stderr(scope, result)?;
    }
    let argv0 = match options {
        Some(options) => options.get_property(scope, c"argv0")?,
        None => HandleValue::undefined(),
    };
    let mut command = crate::errors::js_string(
        scope,
        if js::conversion::to_boolean(argv0) {
            argv0
        } else {
            file
        },
    )?;
    for arg in crate::support::array_elements(scope, args)?.unwrap_or_default() {
        command.push(' ');
        command.push_str(&crate::errors::js_string(scope, arg)?);
    }
    if let Some(error) = check_exec_sync_error(scope, result, &command)? {
        return Err(throw_object(scope, error));
    }
    Ok(result.get_property(scope, c"stdout")?)
}

/// `child_process.execSync(command, options)`, as Node.js's `execSync`: `execFileSync` of
/// `command` in a shell, `options.shell` or `/bin/sh`.
pub(super) fn exec_sync<'s>(
    scope: &'s Scope<'_>,
    command: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<HandleValue<'s>, Thrown> {
    let text = crate::errors::validate_string(scope, command, "command")?;
    options::null_check(scope, command, "command")?;
    let copy = Object::new_with_null_prototype(scope)?;
    if !js::function::is_callable(options) {
        if let Ok(options) = Object::from_value(scope, *options) {
            copy.assign(scope, options)?;
        }
    }
    if !copy.get_property(scope, c"shell")?.is_string() {
        copy.set_property(scope, c"shell", true)?;
    }
    let inherit_stderr = !js::conversion::to_boolean(copy.get_property(scope, c"stdio")?);
    let result = spawn_sync(
        scope,
        command,
        copy.to_jsval_throwing(scope)?,
        HandleValue::undefined(),
    )?;
    if inherit_stderr {
        forward_stderr(scope, result)?;
    }
    if let Some(error) = check_exec_sync_error(scope, result, &text)? {
        return Err(throw_object(scope, error));
    }
    Ok(result.get_property(scope, c"stdout")?)
}
