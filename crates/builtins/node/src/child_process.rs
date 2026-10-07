// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:child_process`: child processes over `platform::child`, natively on Unix. See
//! <https://nodejs.org/api/child_process.html>.
//!
//! Where processes can't be spawned, as on wasm, spawning fails with `ENOSYS`: `spawn`, `exec`,
//! `execFile` and `fork` emit it as an `error` event, as they emit `ENOENT`, `spawnSync` returns
//! it as the result's `error`, and `execSync` and `execFileSync` throw it.

mod child;
mod exec;
mod ipc;
mod options;
mod sync;

pub use child::set_runtime_options_variable;
use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::Thrown;

pub(crate) use child::ChildState;
pub(crate) use ipc::connect_to_parent;

/// `child_process.spawn(file, args, options)`, as Node.js's `spawn`: normalize the arguments,
/// spawn a `ChildProcess`, and kill it once the `timeout` option's milliseconds pass or the
/// `signal` option aborts.
fn spawn<'s>(
    scope: &'s Scope<'_>,
    file: HandleValue<'_>,
    args: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let options = options::normalize_spawn_arguments(scope, file, args, options)?;
    let timeout = options::timeout(scope, options.get_property(scope, c"timeout")?)?;
    let signal = options.get_property(scope, c"signal")?;
    crate::errors::validate_abort_signal(scope, signal, "options.signal")?;
    let kill_signal = options::kill_signal(scope, options.get_property(scope, c"killSignal")?)?;
    let class = child::child_process_class::class_constructor(scope)?;
    let child = Function::construct(scope, class, &[] as &[HandleValue<'_>])?;
    child::spawn(scope, child, options.to_jsval_throwing(scope)?)?;
    if let Some(timeout) = timeout.filter(|timeout| *timeout > 0.0) {
        child::kill_after(scope, child, timeout, kill_signal)?;
    }
    if let Ok(signal) = Object::from_value(scope, *signal) {
        child::kill_on_abort(scope, child, signal, kill_signal)?;
    }
    Ok(child)
}

/// `process.kill(pid, signal)`, as Node.js's: send `signal`, a number or a name of
/// `os.constants.signals`, `SIGTERM` by default, to the process `pid` with `kill(2)`. Returns
/// `true`, and throws the error of `kill(2)`, such as `ESRCH`, as an `Error` with the message
/// `kill CODE`. Where there are no other processes, as on wasm, it throws `ENOSYS`.
pub(crate) fn kill_process(
    scope: &Scope<'_>,
    pid: HandleValue<'_>,
    signal: Option<HandleValue<'_>>,
) -> Result<bool, Thrown> {
    let int32 = |value: HandleValue<'_>| -> Result<Option<i32>, ExnThrown> {
        if !(value.is_number() || value.is_string()) {
            return Ok(None);
        }
        let number = <f64 as js::conversion::FromJSVal>::from_jsval(scope, value, ())
            .map_err(|_| ExnThrown)?;
        Ok(
            (number.fract() == 0.0 && (-2147483648.0..=2147483647.0).contains(&number))
                .then_some(number as i32),
        )
    };
    let Some(pid) = int32(pid)? else {
        return Err(crate::errors::NodeError::invalid_arg_type(
            scope,
            "pid",
            "of type number",
            pid,
        )
        .into());
    };
    let signal = signal.unwrap_or(HandleValue::undefined());
    let number = if signal.is_number()
        && signal.to_number().fract() == 0.0
        && (-2147483648.0..=2147483647.0).contains(&signal.to_number())
    {
        signal.to_number() as i32
    } else {
        let name = if js::conversion::to_boolean(signal) {
            crate::errors::js_string(scope, signal)?
        } else {
            "SIGTERM".to_string()
        };
        crate::os::signal_number(&name).ok_or_else(|| {
            crate::errors::NodeError::new(
                crate::errors::ErrorKind::TypeError,
                "ERR_UNKNOWN_SIGNAL",
                format!("Unknown signal: {name}"),
            )
        })?
    };
    if let Err(error) = platform::child::kill(pid, number) {
        let error = platform::fs::FsError::from_io(&error);
        let error = child::errno_exception(scope, error, "kill")?;
        return Err(child::throw_object(scope, error));
    }
    Ok(true)
}

#[core_runtime::jsmodule(node_args, name = "node:child_process", default_namespace)]
pub mod module {
    use super::*;

    pub use super::child::child_process_class as ChildProcess;

    /// <https://nodejs.org/api/child_process.html#child_processspawncommand-args-options>: of
    /// the options, `cwd`, `env`, `argv0`, `stdio`, `detached`, `uid`, `gid`, `serialization`,
    /// `shell`, `signal`, `timeout` and `killSignal` are supported. `windowsHide` and
    /// `windowsVerbatimArguments` are validated and have no effect. Spawning
    /// `process.execPath`, this runtime, adds the options that enable Node.js compatibility.
    #[function(length = 3)]
    pub fn spawn<'s>(
        scope: &'s Scope<'_>,
        file: HandleValue<'_>,
        args: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let undefined = HandleValue::undefined();
        super::spawn(
            scope,
            file,
            args.unwrap_or(undefined),
            options.unwrap_or(undefined),
        )
    }

    /// <https://nodejs.org/api/child_process.html#child_processspawnsynccommand-args-options>:
    /// throws `ERR_BLOCKING_NOT_ALLOWED` where blocking host I/O isn't allowed.
    #[function(length = 3)]
    pub fn spawn_sync<'s>(
        scope: &'s Scope<'_>,
        file: HandleValue<'_>,
        args: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let undefined = HandleValue::undefined();
        super::sync::spawn_sync(
            scope,
            file,
            args.unwrap_or(undefined),
            options.unwrap_or(undefined),
        )
    }

    /// <https://nodejs.org/api/child_process.html#child_processexecsynccommand-options>
    #[function(length = 2)]
    pub fn exec_sync<'s>(
        scope: &'s Scope<'_>,
        command: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        super::sync::exec_sync(scope, command, options.unwrap_or(HandleValue::undefined()))
    }

    /// <https://nodejs.org/api/child_process.html#child_processexecfilesyncfile-args-options>
    #[function(length = 3)]
    pub fn exec_file_sync<'s>(
        scope: &'s Scope<'_>,
        file: HandleValue<'_>,
        args: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let undefined = HandleValue::undefined();
        super::sync::exec_file_sync(
            scope,
            file,
            args.unwrap_or(undefined),
            options.unwrap_or(undefined),
        )
    }

    /// <https://nodejs.org/api/child_process.html#child_processforkmodulepath-args-options>: runs
    /// `options.execPath`, `process.execPath` by default, with the module, connected over an IPC
    /// channel with the `json` serialization.
    #[function(length = 1)]
    pub fn fork<'s>(
        scope: &'s Scope<'_>,
        module_path: HandleValue<'_>,
        args: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let undefined = HandleValue::undefined();
        super::ipc::fork(
            scope,
            module_path,
            args.unwrap_or(undefined),
            options.unwrap_or(undefined),
        )
    }

    /// Node.js's internal `_forkChild(fd, serializationMode)`: connect `process` to its parent
    /// over the IPC channel on the descriptor `fd`.
    #[function(name = "_forkChild", length = 2)]
    pub fn fork_child(scope: &Scope<'_>, fd: HandleValue<'_>) -> Result<(), Thrown> {
        super::ipc::fork_child(scope, fd)
    }
}

/// Register `node:child_process`: the functions of [`module`], and `exec` and `execFile` with
/// Node.js's `util.promisify.custom` versions.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let Ok(exports) = exec::exports(scope) else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &exports)
}

/// Register the classes of `node:child_process` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: js::Object<'_>) -> Result<(), ExnThrown> {
    ChildState::add_to_global(scope, global);
    exec::ExecState::add_to_global(scope, global);
    ipc::register_classes(scope, global)?;
    child::child_process_class::register(scope)?;
    Ok(())
}
