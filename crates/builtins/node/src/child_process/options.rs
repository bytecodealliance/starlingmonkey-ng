// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The validation and normalization of `node:child_process`'s arguments, ported from Node.js's
//! `normalizeSpawnArguments`, `getValidStdio` and their validators.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use crate::errors::{validate_boolean, validate_string, ErrorKind, NodeError, Thrown};

/// The path of the shell that `shell: true` runs a command with.
const SHELL: &str = "/bin/sh";

/// `value` as `ERR_OUT_OF_RANGE` shows a received value: a number as
/// [`crate::errors::out_of_range_received`] shows it, and other values as `util.inspect` does.
pub(super) fn received(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    if value.is_number() {
        return Ok(crate::errors::out_of_range_received(value.to_number()));
    }
    let options = crate::inspect::InspectOptions::current(scope)?;
    crate::inspect::inspect(scope, value, options)
}

/// Node.js's `validateArgumentNullCheck`: `ERR_INVALID_ARG_VALUE` naming `name` if `value` is a
/// string with a null byte.
pub(super) fn null_check(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<(), Thrown> {
    if value.is_string() && String::from_jsval(scope, value, ())?.contains('\0') {
        return Err(NodeError::invalid_arg_value(
            scope,
            name,
            value,
            "must be a string without null bytes",
        )
        .into());
    }
    Ok(())
}

/// Node.js's `validateTimeout`: the timeout `value`, which is `undefined`, `null` or an integer
/// of at least 0.
pub(super) fn timeout(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Option<f64>, Thrown> {
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    let valid = value.is_number() && value.to_number().fract() == 0.0 && value.to_number() >= 0.0;
    if !valid {
        return Err(NodeError::out_of_range(
            "timeout",
            "an unsigned integer",
            &received(scope, value)?,
        )
        .into());
    }
    Ok(Some(value.to_number()))
}

/// Node.js's `validateMaxBuffer`: the buffer limit `value`, which is `undefined`, `null` or a
/// number of at least 0.
pub(super) fn max_buffer(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Option<f64>, Thrown> {
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    if !(value.is_number() && value.to_number() >= 0.0) {
        return Err(NodeError::out_of_range(
            "options.maxBuffer",
            "a positive number",
            &received(scope, value)?,
        )
        .into());
    }
    Ok(Some(value.to_number()))
}

/// Node.js's `convertToValidSignal`: the number of the signal `value`, a signal number or a
/// signal name in any case, or `ERR_UNKNOWN_SIGNAL`.
pub(super) fn signal(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<i32, Thrown> {
    if value.is_number() {
        let number = value.to_number();
        if number.fract() == 0.0 && crate::os::signal_name(number as i32).is_some() {
            return Ok(number as i32);
        }
    } else if value.is_string() {
        let name = String::from_jsval(scope, value, ())?.to_uppercase();
        if let Some(number) = crate::os::signal_number(&name) {
            return Ok(number);
        }
    }
    let shown = crate::errors::js_string(scope, value)?;
    Err(NodeError::new(
        ErrorKind::TypeError,
        "ERR_UNKNOWN_SIGNAL",
        format!("Unknown signal: {shown}"),
    )
    .into())
}

/// Node.js's `sanitizeKillSignal`: the signal `value` names, if it is a string or a number.
pub(super) fn kill_signal(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<i32>, Thrown> {
    if value.is_string() || value.is_number() {
        return signal(scope, value).map(Some);
    }
    if !value.is_null_or_undefined() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "options.killSignal",
            "one of type string or number",
            value,
        )
        .into());
    }
    Ok(None)
}

/// The working directory the `cwd` property `value` of the normalized options names: a string
/// that isn't empty, or `None` for the parent's.
pub(super) fn cwd(scope: &Scope<'_>, value: HandleValue<'_>) -> Option<String> {
    crate::support::rust_string(scope, value).filter(|cwd| value.is_string() && !cwd.is_empty())
}

/// Whether `value` is a number that is a 32-bit integer.
pub(super) fn is_int32(value: HandleValue<'_>) -> bool {
    value.is_number() && {
        let number = value.to_number();
        number.fract() == 0.0 && (-2147483648.0..=2147483647.0).contains(&number)
    }
}

js::instance_local! {
    /// Whether the `DEP0190` warning about arguments with `shell` was emitted.
    static SHELL_ARGS_WARNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Node.js's `normalizeSpawnArguments`: validate `file`, `args` and `options`, and return a copy
/// of `options` with a null prototype and Node.js's `args`, `cwd`, `detached`, `envPairs`,
/// `file`, `windowsHide` and `windowsVerbatimArguments`. With `shell`, `file` is the shell and
/// `args` runs the command in it. `args` starts with `argv0` or `file`. `envPairs` holds a
/// `KEY=value` string for each property of `options.env`, own or inherited, or of
/// `process.env`, whose value isn't `undefined`.
pub(super) fn normalize_spawn_arguments<'s>(
    scope: &'s Scope<'_>,
    file: HandleValue<'_>,
    args: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let mut file_name = validate_string(scope, file, "file")?;
    null_check(scope, file, "file")?;
    if file_name.is_empty() {
        return Err(NodeError::invalid_arg_value(scope, "file", file, "cannot be empty").into());
    }
    let mut options = options;
    let mut arg_values: Vec<HandleValue<'s>> = Vec::new();
    if let Some(elements) = crate::support::array_elements(scope, args)? {
        arg_values = elements;
    } else if args.is_null_or_undefined() {
    } else if !args.is_object() || js::function::is_callable(args) {
        return Err(NodeError::invalid_arg_type(scope, "args", "of type object", args).into());
    } else {
        options = args;
    }
    for (index, arg) in arg_values.iter().enumerate() {
        null_check(scope, *arg, &format!("args[{index}]"))?;
    }
    let normalized = Object::new_with_null_prototype(scope)?;
    if !options.is_undefined() {
        let given = crate::errors::validate_plain_object(scope, options, "options", false)?;
        normalized.assign(scope, given)?;
    }
    let mut cwd = normalized.get_property(scope, c"cwd")?;
    if !cwd.is_null_or_undefined() {
        let path = crate::fs::path_arg(scope, cwd, "options.cwd")?;
        cwd = path.as_str().to_jsval_throwing(scope)?;
    }
    let detached = normalized.get_property(scope, c"detached")?;
    if !detached.is_null_or_undefined() {
        validate_boolean(scope, detached, "options.detached")?;
    }
    for (name, label) in [(c"uid", "options.uid"), (c"gid", "options.gid")] {
        let id = normalized.get_property(scope, name)?;
        if !id.is_null_or_undefined() && !is_int32(id) {
            return Err(NodeError::invalid_arg_type(scope, label, "int32", id).into());
        }
    }
    let shell = normalized.get_property(scope, c"shell")?;
    if !shell.is_null_or_undefined() && !shell.is_boolean() && !shell.is_string() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "options.shell",
            "one of type boolean or string",
            shell,
        )
        .into());
    }
    let argv0 = normalized.get_property(scope, c"argv0")?;
    if !argv0.is_null_or_undefined() {
        validate_string(scope, argv0, "options.argv0")?;
        null_check(scope, argv0, "options.argv0")?;
    }
    for (name, label) in [
        (c"windowsHide", "options.windowsHide"),
        (
            c"windowsVerbatimArguments",
            "options.windowsVerbatimArguments",
        ),
    ] {
        let value = normalized.get_property(scope, name)?;
        if !value.is_null_or_undefined() {
            validate_boolean(scope, value, label)?;
        }
    }
    let mut argv: Vec<HandleValue<'s>> = arg_values;
    if js::conversion::to_boolean(shell) {
        null_check(scope, shell, "options.shell")?;
        if !argv.is_empty() && !SHELL_ARGS_WARNED.get() {
            SHELL_ARGS_WARNED.set(true);
            crate::process::deprecation_warning(
                scope,
                "Passing args to a child process with shell option true can lead to security \
                 vulnerabilities, as the arguments are not escaped, only concatenated.",
                "DEP0190",
            )?;
        }
        // `Array.prototype.join` joins `null` and `undefined` as empty strings.
        let mut command = file_name.clone();
        for arg in &argv {
            command.push(' ');
            if !arg.is_null_or_undefined() {
                command.push_str(&crate::errors::js_string(scope, *arg)?);
            }
        }
        file_name = if shell.is_string() {
            String::from_jsval(scope, shell, ())?
        } else {
            SHELL.to_string()
        };
        argv = vec![
            "-c".to_jsval_throwing(scope)?,
            command.as_str().to_jsval_throwing(scope)?,
        ];
    }
    let first = if argv0.is_string() {
        argv0
    } else {
        file_name.as_str().to_jsval_throwing(scope)?
    };
    argv.insert(0, first);
    let env = normalized.get_property(scope, c"env")?;
    let env = match Object::from_value(scope, *env) {
        Ok(env) => env,
        Err(_) => {
            let copy = Object::new_plain(scope)?;
            let process = crate::process::process_value(scope)?;
            if let Ok(process) = Object::from_value(scope, *process) {
                if let Ok(env) = Object::from_value(scope, *process.get_property(scope, c"env")?) {
                    copy.assign(scope, env)?;
                }
            }
            copy
        }
    };
    let mut pairs = Vec::new();
    for key_value in env.for_in_keys(scope)? {
        let id = scope.root_id(js::id::value_to_id(scope, key_value)?);
        let value = env.get_property_by_id(scope, id)?;
        if value.is_undefined() {
            continue;
        }
        let key = crate::errors::js_string(scope, key_value)?;
        let label = format!("options.env['{key}']");
        null_check(scope, key_value, &label)?;
        null_check(scope, value, &label)?;
        let value = String::from_jsval(scope, value, ())?;
        pairs.push(format!("{key}={value}").to_jsval_throwing(scope)?);
    }
    let args = js::Array::with_contents(scope, &argv)?;
    let env_pairs = js::Array::with_contents(scope, &pairs)?;
    normalized.set_property(scope, c"args", args)?;
    normalized.set_property(scope, c"cwd", cwd)?;
    normalized.set_property(scope, c"detached", js::conversion::to_boolean(detached))?;
    normalized.set_property(scope, c"envPairs", env_pairs)?;
    normalized.set_property(scope, c"file", file_name.as_str())?;
    let windows_hide = normalized.get_property(scope, c"windowsHide")?;
    normalized.set_property(
        scope,
        c"windowsHide",
        js::conversion::to_boolean(windows_hide),
    )?;
    let verbatim = normalized.get_property(scope, c"windowsVerbatimArguments")?;
    normalized.set_property(
        scope,
        c"windowsVerbatimArguments",
        js::conversion::to_boolean(verbatim),
    )?;
    Ok(normalized)
}

/// A descriptor of a child process, as `getValidStdio` translates the `stdio` option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StdioEntry {
    Ignore,
    /// A pipe, which `overlapped` is too.
    Pipe,
    /// The IPC channel.
    Ipc,
    /// The parent's descriptor of the same number.
    Inherit,
    /// The descriptor `fd` of the parent, as the `fs` module numbers them.
    Fd(i32),
    /// The pipe of a socket of another child process, by the host's descriptor.
    Wrap(i32),
}

/// The `stdio` option `stdio` as `getValidStdio` translates it: a string is one of the
/// shorthands, and an array has an entry per descriptor, at least three, where a missing entry is
/// `pipe` for the first three descriptors and `ignore` for the others. With `sync`, `ipc` throws
/// `ERR_IPC_SYNC_FORK`, and entries that are strings or `ArrayBufferView`s are skipped. Without
/// `sync`, they throw `ERR_INVALID_SYNC_FORK_INPUT`.
pub(super) fn valid_stdio(
    scope: &Scope<'_>,
    stdio: HandleValue<'_>,
    sync: bool,
) -> Result<Vec<StdioEntry>, Thrown> {
    if stdio.is_string() {
        return shorthand(scope, stdio);
    }
    let Some(entries) = crate::support::array_elements(scope, stdio)? else {
        return Err(NodeError::invalid_arg_value(scope, "stdio", stdio, "is invalid").into());
    };
    let count = entries.len().max(3);
    let mut valid = Vec::new();
    if valid.try_reserve_exact(count).is_err() {
        js::exception::report_out_of_memory(scope);
        return Err(ExnThrown.into());
    }
    let mut ipc = false;
    for index in 0..count {
        let entry = entries
            .get(index)
            .copied()
            .unwrap_or(HandleValue::undefined());
        let name = if entry.is_null_or_undefined() {
            Some(if index < 3 { "pipe" } else { "ignore" }.to_string())
        } else if entry.is_string() {
            Some(String::from_jsval(scope, entry, ())?)
        } else {
            None
        };
        let fd = if entry.is_number() {
            Some(entry.to_number())
        } else {
            match Object::from_value(scope, *entry) {
                Ok(object) => {
                    let fd = object.get_property(scope, c"fd")?;
                    fd.is_number().then(|| fd.to_number())
                }
                Err(_) => None,
            }
        };
        let translated = match (name.as_deref(), fd) {
            (Some("ignore"), _) => StdioEntry::Ignore,
            (Some("pipe" | "overlapped"), _) => StdioEntry::Pipe,
            (None, Some(fd)) if entry.is_number() && fd < 0.0 => StdioEntry::Pipe,
            (Some("ipc"), _) => {
                if sync || ipc {
                    let (code, message) = if sync {
                        (
                            "ERR_IPC_SYNC_FORK",
                            "IPC cannot be used with synchronous forks",
                        )
                    } else {
                        (
                            "ERR_IPC_ONE_PIPE",
                            "Child process can have only one IPC pipe",
                        )
                    };
                    return Err(NodeError::new(ErrorKind::Error, code, message).into());
                }
                ipc = true;
                StdioEntry::Ipc
            }
            (Some("inherit"), _) => StdioEntry::Inherit,
            (None, Some(fd)) => StdioEntry::Fd(fd as i32),
            (None, None) if pipe_of(scope, entry)?.is_some() => {
                StdioEntry::Wrap(pipe_of(scope, entry)?.unwrap_or(-1))
            }
            _ if is_sync_input(scope, entry)? => {
                if sync {
                    // Node.js's `getValidStdio` skips them, which moves the later entries down.
                    continue;
                }
                let shown = received(scope, entry)?;
                return Err(NodeError::new(
                    ErrorKind::Error,
                    "ERR_INVALID_SYNC_FORK_INPUT",
                    format!(
                        "Asynchronous forks do not support Buffer, TypedArray, DataView or \
                         string input: {shown}"
                    ),
                )
                .into());
            }
            _ => {
                return Err(
                    NodeError::invalid_arg_value(scope, "stdio", entry, "is invalid").into(),
                )
            }
        };
        valid.push(translated);
    }
    Ok(valid)
}

/// The host's descriptor of the pipe of `value`, if it is a socket over a pipe to a child
/// process.
fn pipe_of(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Option<i32>, ExnThrown> {
    match Object::from_value(scope, *value) {
        Ok(socket) => crate::net::socket::pipe_fd(scope, socket),
        Err(_) => Ok(None),
    }
}

/// Whether `value` is an input of `spawnSync`'s `stdio`: a string or an `ArrayBufferView`.
fn is_sync_input(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    Ok(value.is_string()
        || Object::from_value(scope, *value)
            .ok()
            .and_then(js::ArrayBufferView::from_object)
            .is_some())
}

/// The entries of the `stdio` shorthand `stdio`, as Node.js's `stdioStringToArray` makes them,
/// or `ERR_INVALID_ARG_VALUE`: three `ignore` or `pipe` entries, or descriptors 0, 1 and 2 for
/// `inherit`.
fn shorthand(scope: &Scope<'_>, stdio: HandleValue<'_>) -> Result<Vec<StdioEntry>, Thrown> {
    let name = String::from_jsval(scope, stdio, ())?;
    Ok(match name.as_str() {
        "ignore" => vec![StdioEntry::Ignore; 3],
        "overlapped" | "pipe" => vec![StdioEntry::Pipe; 3],
        "inherit" => vec![StdioEntry::Fd(0), StdioEntry::Fd(1), StdioEntry::Fd(2)],
        _ => return Err(NodeError::invalid_arg_value(scope, "stdio", stdio, "is invalid").into()),
    })
}

/// The host's descriptor for the descriptor `fd` that `stdio` names: 0, 1 and 2 are the
/// process's, and others are files the `fs` module opened.
pub(super) fn host_fd(fd: i32) -> Option<i32> {
    match fd {
        0..=2 => Some(fd),
        _ => crate::fs::host_fd(fd),
    }
}

/// The host's descriptors for the stdio `entries` of a child process, and whether an `Fd` entry
/// has no host descriptor. Such an entry becomes `Stdio::Ignore`. Throws if allocating the list
/// fails.
pub(super) fn host_stdio(
    scope: &Scope<'_>,
    entries: &[StdioEntry],
) -> Result<(Vec<platform::child::Stdio>, bool), ExnThrown> {
    use platform::child::Stdio;
    let mut bad_fd = false;
    let descriptors = crate::support::try_collect(entries.iter().enumerate().map(
        |(index, entry)| match *entry {
            StdioEntry::Ignore => Stdio::Ignore,
            StdioEntry::Pipe | StdioEntry::Ipc => Stdio::Pipe,
            StdioEntry::Inherit => Stdio::Fd(index as i32),
            StdioEntry::Wrap(fd) => Stdio::Fd(fd),
            StdioEntry::Fd(fd) => match host_fd(fd) {
                Some(fd) => Stdio::Fd(fd),
                None => {
                    bad_fd = true;
                    Stdio::Ignore
                }
            },
        },
    ));
    let Some(descriptors) = descriptors else {
        js::exception::report_out_of_memory(scope);
        return Err(ExnThrown);
    };
    Ok((descriptors, bad_fd))
}

/// `ERR_INVALID_ARG_TYPE` for the property `name` of the options of `ChildProcess.spawn` unless
/// `value` is an array, as Node.js's `validateArray` checks it.
pub(super) fn validate_array<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<Vec<HandleValue<'s>>, Thrown> {
    crate::support::array_elements(scope, value)?.ok_or_else(|| {
        NodeError::invalid_arg_type(scope, name, "an instance of Array", value).into()
    })
}
