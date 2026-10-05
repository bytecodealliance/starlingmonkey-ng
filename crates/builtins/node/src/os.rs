// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:os`. See <https://nodejs.org/api/os.html>.
//!
//! WASI exposes no information about the host machine, so on wasm the module describes a single
//! processor with unknown speed and load, no network interfaces, and 4 GiB of memory, the size of
//! a 32-bit address space. Natively, it describes the host where `std` can: the number of
//! processors and the machine type, and the signal and `errno` numbers of macOS or Linux. It uses
//! the same placeholders elsewhere. Paths and user details come from `process.env`: `HOME`,
//! `TMPDIR`, `TMP`, `TEMP`, `USER`, `SHELL` and `HOSTNAME`.

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use crate::errors::Thrown;

/// The memory `totalmem()` and `freemem()` report.
const MEMORY: f64 = 4294967296.0;

/// `process.env[name]`, if it is a non-empty string.
fn env_var(scope: &Scope<'_>, name: &str) -> Result<Option<String>, ExnThrown> {
    let Ok(process) = Object::from_value(scope, *crate::process::process_value(scope)?) else {
        return Ok(None);
    };
    let Ok(env) = Object::from_value(scope, *process.get_property(scope, c"env")?) else {
        return Ok(None);
    };
    let name = std::ffi::CString::new(name).expect("the names hold no NUL");
    let value = env.get_property_by_name(scope, &name)?;
    if !value.is_string() {
        return Ok(None);
    }
    let value = String::from_jsval(scope, value, ()).map_err(|_| ExnThrown)?;
    Ok(Some(value).filter(|value| !value.is_empty()))
}

/// The number of processors: `std::thread::available_parallelism` natively, and 1 on wasm.
fn processors() -> usize {
    if cfg!(target_arch = "wasm32") {
        return 1;
    }
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// macOS's signal numbers.
const DARWIN_SIGNALS: &[(&std::ffi::CStr, i32)] = &[
    (c"SIGHUP", 1),
    (c"SIGINT", 2),
    (c"SIGQUIT", 3),
    (c"SIGILL", 4),
    (c"SIGTRAP", 5),
    (c"SIGABRT", 6),
    (c"SIGIOT", 6),
    (c"SIGBUS", 10),
    (c"SIGFPE", 8),
    (c"SIGKILL", 9),
    (c"SIGUSR1", 30),
    (c"SIGSEGV", 11),
    (c"SIGUSR2", 31),
    (c"SIGPIPE", 13),
    (c"SIGALRM", 14),
    (c"SIGTERM", 15),
    (c"SIGCHLD", 20),
    (c"SIGCONT", 19),
    (c"SIGSTOP", 17),
    (c"SIGTSTP", 18),
    (c"SIGTTIN", 21),
    (c"SIGTTOU", 22),
    (c"SIGURG", 16),
    (c"SIGXCPU", 24),
    (c"SIGXFSZ", 25),
    (c"SIGVTALRM", 26),
    (c"SIGPROF", 27),
    (c"SIGWINCH", 28),
    (c"SIGIO", 23),
    (c"SIGINFO", 29),
    (c"SIGSYS", 12),
];

/// macOS's `errno` values.
const DARWIN_ERRNO: &[(&std::ffi::CStr, i32)] = &[
    (c"E2BIG", 7),
    (c"EACCES", 13),
    (c"EADDRINUSE", 48),
    (c"EADDRNOTAVAIL", 49),
    (c"EAFNOSUPPORT", 47),
    (c"EAGAIN", 35),
    (c"EALREADY", 37),
    (c"EBADF", 9),
    (c"EBADMSG", 94),
    (c"EBUSY", 16),
    (c"ECANCELED", 89),
    (c"ECHILD", 10),
    (c"ECONNABORTED", 53),
    (c"ECONNREFUSED", 61),
    (c"ECONNRESET", 54),
    (c"EDEADLK", 11),
    (c"EDESTADDRREQ", 39),
    (c"EDOM", 33),
    (c"EDQUOT", 69),
    (c"EEXIST", 17),
    (c"EFAULT", 14),
    (c"EFBIG", 27),
    (c"EHOSTUNREACH", 65),
    (c"EIDRM", 90),
    (c"EILSEQ", 92),
    (c"EINPROGRESS", 36),
    (c"EINTR", 4),
    (c"EINVAL", 22),
    (c"EIO", 5),
    (c"EISCONN", 56),
    (c"EISDIR", 21),
    (c"ELOOP", 62),
    (c"EMFILE", 24),
    (c"EMLINK", 31),
    (c"EMSGSIZE", 40),
    (c"EMULTIHOP", 95),
    (c"ENAMETOOLONG", 63),
    (c"ENETDOWN", 50),
    (c"ENETRESET", 52),
    (c"ENETUNREACH", 51),
    (c"ENFILE", 23),
    (c"ENOBUFS", 55),
    (c"ENODATA", 96),
    (c"ENODEV", 19),
    (c"ENOENT", 2),
    (c"ENOEXEC", 8),
    (c"ENOLCK", 77),
    (c"ENOLINK", 97),
    (c"ENOMEM", 12),
    (c"ENOMSG", 91),
    (c"ENOPROTOOPT", 42),
    (c"ENOSPC", 28),
    (c"ENOSR", 98),
    (c"ENOSTR", 99),
    (c"ENOSYS", 78),
    (c"ENOTCONN", 57),
    (c"ENOTDIR", 20),
    (c"ENOTEMPTY", 66),
    (c"ENOTSOCK", 38),
    (c"ENOTSUP", 45),
    (c"ENOTTY", 25),
    (c"ENXIO", 6),
    (c"EOPNOTSUPP", 102),
    (c"EOVERFLOW", 84),
    (c"EPERM", 1),
    (c"EPIPE", 32),
    (c"EPROTO", 100),
    (c"EPROTONOSUPPORT", 43),
    (c"EPROTOTYPE", 41),
    (c"ERANGE", 34),
    (c"EROFS", 30),
    (c"ESPIPE", 29),
    (c"ESRCH", 3),
    (c"ESTALE", 70),
    (c"ETIME", 101),
    (c"ETIMEDOUT", 60),
    (c"ETXTBSY", 26),
    (c"EWOULDBLOCK", 35),
    (c"EXDEV", 18),
];

/// Linux's signal numbers.
const SIGNALS: &[(&std::ffi::CStr, i32)] = &[
    (c"SIGHUP", 1),
    (c"SIGINT", 2),
    (c"SIGQUIT", 3),
    (c"SIGILL", 4),
    (c"SIGTRAP", 5),
    (c"SIGABRT", 6),
    (c"SIGIOT", 6),
    (c"SIGBUS", 7),
    (c"SIGFPE", 8),
    (c"SIGKILL", 9),
    (c"SIGUSR1", 10),
    (c"SIGSEGV", 11),
    (c"SIGUSR2", 12),
    (c"SIGPIPE", 13),
    (c"SIGALRM", 14),
    (c"SIGTERM", 15),
    (c"SIGCHLD", 17),
    (c"SIGCONT", 18),
    (c"SIGSTOP", 19),
    (c"SIGTSTP", 20),
    (c"SIGTTIN", 21),
    (c"SIGTTOU", 22),
    (c"SIGURG", 23),
    (c"SIGXCPU", 24),
    (c"SIGXFSZ", 25),
    (c"SIGVTALRM", 26),
    (c"SIGPROF", 27),
    (c"SIGWINCH", 28),
    (c"SIGIO", 29),
    (c"SIGPOLL", 29),
    (c"SIGPWR", 30),
    (c"SIGSYS", 31),
];

/// Linux's `errno` values.
const ERRNO: &[(&std::ffi::CStr, i32)] = &[
    (c"EPERM", 1),
    (c"ENOENT", 2),
    (c"ESRCH", 3),
    (c"EINTR", 4),
    (c"EIO", 5),
    (c"ENXIO", 6),
    (c"E2BIG", 7),
    (c"ENOEXEC", 8),
    (c"EBADF", 9),
    (c"ECHILD", 10),
    (c"EAGAIN", 11),
    (c"EWOULDBLOCK", 11),
    (c"ENOMEM", 12),
    (c"EACCES", 13),
    (c"EFAULT", 14),
    (c"EBUSY", 16),
    (c"EEXIST", 17),
    (c"EXDEV", 18),
    (c"ENODEV", 19),
    (c"ENOTDIR", 20),
    (c"EISDIR", 21),
    (c"EINVAL", 22),
    (c"ENFILE", 23),
    (c"EMFILE", 24),
    (c"ENOTTY", 25),
    (c"ETXTBSY", 26),
    (c"EFBIG", 27),
    (c"ENOSPC", 28),
    (c"ESPIPE", 29),
    (c"EROFS", 30),
    (c"EMLINK", 31),
    (c"EPIPE", 32),
    (c"EDOM", 33),
    (c"ERANGE", 34),
    (c"EDEADLK", 35),
    (c"ENAMETOOLONG", 36),
    (c"ENOLCK", 37),
    (c"ENOSYS", 38),
    (c"ENOTEMPTY", 39),
    (c"ELOOP", 40),
    (c"ENOTSUP", 95),
    (c"EOPNOTSUPP", 95),
    (c"EADDRINUSE", 98),
    (c"EADDRNOTAVAIL", 99),
    (c"ENETDOWN", 100),
    (c"ENETUNREACH", 101),
    (c"ECONNABORTED", 103),
    (c"ECONNRESET", 104),
    (c"ENOBUFS", 105),
    (c"EISCONN", 106),
    (c"ENOTCONN", 107),
    (c"ETIMEDOUT", 110),
    (c"ECONNREFUSED", 111),
    (c"EHOSTUNREACH", 113),
    (c"EALREADY", 114),
    (c"EINPROGRESS", 115),
    (c"ECANCELED", 125),
];

/// The process priorities.
const PRIORITY: &[(&std::ffi::CStr, i32)] = &[
    (c"PRIORITY_LOW", 19),
    (c"PRIORITY_BELOW_NORMAL", 10),
    (c"PRIORITY_NORMAL", 0),
    (c"PRIORITY_ABOVE_NORMAL", -7),
    (c"PRIORITY_HIGH", -14),
    (c"PRIORITY_HIGHEST", -20),
];

/// The `errno` values of macOS if `process.platform` is `darwin`, and of Linux otherwise.
pub(crate) fn errno_table() -> &'static [(&'static std::ffi::CStr, i32)] {
    if crate::process::platform() == "darwin" {
        DARWIN_ERRNO
    } else {
        ERRNO
    }
}

/// The marker of the realm's `os.constants` object.
struct ConstantsMarker;

/// The `os.constants` object, the same one for every caller in a realm: the signal numbers and
/// `errno` values of macOS if `process.platform` is `darwin`, and of Linux otherwise, the process
/// priorities and the `dlopen` flags, in objects without a prototype. `signals` is frozen.
pub(crate) fn constants<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    if let Some(constants) = js::class::realm_object::<ConstantsMarker>(scope) {
        return Ok(constants);
    }
    let null_object = || -> Result<Object<'s>, ExnThrown> {
        let object = Object::new_plain(scope)?;
        object.set_prototype(scope, js::native::HandleObject::null())?;
        Ok(object)
    };
    let table = |entries: &[(&'static std::ffi::CStr, i32)]| -> Result<Object<'s>, ExnThrown> {
        let object = null_object()?;
        for (name, value) in entries {
            object.set_property(scope, name, *value)?;
        }
        Ok(object)
    };
    let darwin = crate::process::platform() == "darwin";
    let signals = if darwin { DARWIN_SIGNALS } else { SIGNALS };
    let dlopen: &[(&std::ffi::CStr, i32)] = if darwin {
        &[
            (c"RTLD_LAZY", 1),
            (c"RTLD_NOW", 2),
            (c"RTLD_GLOBAL", 8),
            (c"RTLD_LOCAL", 4),
        ]
    } else {
        &[
            (c"RTLD_LAZY", 1),
            (c"RTLD_NOW", 2),
            (c"RTLD_GLOBAL", 256),
            (c"RTLD_LOCAL", 0),
            (c"RTLD_DEEPBIND", 8),
        ]
    };
    let constants = null_object()?;
    constants.set_property(scope, c"UV_UDP_REUSEADDR", 4)?;
    constants.set_property(scope, c"dlopen", table(dlopen)?)?;
    constants.set_property(scope, c"errno", table(errno_table())?)?;
    let signals = table(signals)?;
    signals.freeze(scope)?;
    constants.set_property(scope, c"signals", signals)?;
    constants.set_property(scope, c"priority", table(PRIORITY)?)?;
    js::class::register_realm_object::<ConstantsMarker>(scope, constants);
    Ok(constants)
}

#[core_runtime::jsmodule(node_args, name = "node:os", default_namespace)]
pub mod module {
    use super::*;

    /// <https://nodejs.org/api/os.html#oseol>
    pub const EOL: &str = "\n";

    /// <https://nodejs.org/api/os.html#osdevnull>
    #[allow(non_upper_case_globals)]
    pub const devNull: &str = "/dev/null";

    /// <https://nodejs.org/api/os.html#osarch>: `process.arch`.
    pub fn arch() -> String {
        crate::process::arch().into()
    }

    /// <https://nodejs.org/api/os.html#osplatform>: `process.platform`.
    pub fn platform() -> String {
        crate::process::platform().into()
    }

    /// <https://nodejs.org/api/os.html#ostype>: the operating system name for `process.platform`.
    pub fn r#type() -> String {
        match crate::process::platform() {
            "darwin" => "Darwin",
            "win32" => "Windows_NT",
            _ => "Linux",
        }
        .into()
    }

    /// <https://nodejs.org/api/os.html#osrelease>: unknown, so `0.0.0`.
    pub fn release() -> String {
        "0.0.0".into()
    }

    /// <https://nodejs.org/api/os.html#osversion>: the runtime's name and version.
    pub fn version() -> String {
        format!("StarlingMonkey {}", env!("CARGO_PKG_VERSION"))
    }

    /// <https://nodejs.org/api/os.html#osmachine>: the machine type, as `uname -m` names it.
    pub fn machine() -> String {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return "arm64".into();
        }
        std::env::consts::ARCH.into()
    }

    /// <https://nodejs.org/api/os.html#osendianness>
    pub fn endianness() -> String {
        if cfg!(target_endian = "little") {
            "LE"
        } else {
            "BE"
        }
        .into()
    }

    /// <https://nodejs.org/api/os.html#oshostname>: `HOSTNAME`, or `localhost`.
    pub fn hostname(scope: &Scope<'_>) -> Result<String, ExnThrown> {
        Ok(env_var(scope, "HOSTNAME")?.unwrap_or_else(|| "localhost".into()))
    }

    /// <https://nodejs.org/api/os.html#oshomedir>: `HOME`, or `/`.
    pub fn homedir(scope: &Scope<'_>) -> Result<String, ExnThrown> {
        Ok(env_var(scope, "HOME")?.unwrap_or_else(|| "/".into()))
    }

    /// <https://nodejs.org/api/os.html#ostmpdir>: `TMPDIR`, `TMP` or `TEMP` without a trailing
    /// slash, or `/tmp`.
    pub fn tmpdir(scope: &Scope<'_>) -> Result<String, ExnThrown> {
        let mut dir = None;
        for name in ["TMPDIR", "TMP", "TEMP"] {
            dir = env_var(scope, name)?;
            if dir.is_some() {
                break;
            }
        }
        let dir = dir.unwrap_or_else(|| "/tmp".into());
        Ok(match dir.strip_suffix('/') {
            Some(trimmed) if !trimmed.is_empty() => trimmed.into(),
            _ => dir,
        })
    }

    /// <https://nodejs.org/api/os.html#osuptime>: the time the runtime has run, in seconds.
    pub fn uptime() -> f64 {
        crate::process::functions::uptime().floor()
    }

    /// <https://nodejs.org/api/os.html#ostotalmem>: 4 GiB.
    pub fn totalmem() -> f64 {
        MEMORY
    }

    /// <https://nodejs.org/api/os.html#osfreemem>: 4 GiB.
    pub fn freemem() -> f64 {
        MEMORY
    }

    /// <https://nodejs.org/api/os.html#osloadavg>: `[0, 0, 0]`.
    pub fn loadavg<'s>(scope: &'s Scope<'_>) -> Result<js::Array<'s>, ExnThrown> {
        js::Array::with_contents(scope, &[0, 0, 0])
    }

    /// <https://nodejs.org/api/os.html#osavailableparallelism>: the number of processors.
    pub fn available_parallelism() -> u32 {
        processors() as u32
    }

    /// <https://nodejs.org/api/os.html#oscpus>: one entry per processor, of unknown model and
    /// speed.
    pub fn cpus<'s>(scope: &'s Scope<'_>) -> Result<js::Array<'s>, ExnThrown> {
        let mut cpus = Vec::new();
        for _ in 0..processors() {
            let times = Object::new_plain(scope)?;
            for name in [c"user", c"nice", c"sys", c"idle", c"irq"] {
                times.set_property(scope, name, 0)?;
            }
            let cpu = Object::new_plain(scope)?;
            cpu.set_property(scope, c"model", "unknown")?;
            cpu.set_property(scope, c"speed", 0)?;
            cpu.set_property(scope, c"times", times)?;
            cpus.push(cpu);
        }
        js::Array::with_contents(scope, &cpus)
    }

    /// <https://nodejs.org/api/os.html#osnetworkinterfaces>: none.
    pub fn network_interfaces<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        Object::new_plain(scope)
    }

    /// <https://nodejs.org/api/os.html#osuserinfooptions>: `uid` and `gid` are -1, and the rest
    /// comes from `USER`, `HOME` and `SHELL`.
    #[function(length = 1)]
    pub fn user_info<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        let info = Object::new_plain(scope)?;
        info.set_property(scope, c"uid", -1)?;
        info.set_property(scope, c"gid", -1)?;
        let username = env_var(scope, "USER")?.unwrap_or_else(|| "starlingmonkey".into());
        info.set_property(scope, c"username", username.as_str())?;
        info.set_property(scope, c"homedir", homedir(scope)?.as_str())?;
        match env_var(scope, "SHELL")? {
            Some(shell) => info.set_property(scope, c"shell", shell.as_str())?,
            None => info.set_property(scope, c"shell", js::value::null())?,
        }
        Ok(info)
    }

    /// <https://nodejs.org/api/os.html#osgetprioritypid>: always the normal priority, 0, for a
    /// `pid` that is a 32-bit integer.
    #[function(length = 1)]
    pub fn get_priority(scope: &Scope<'_>, pid: Option<HandleValue<'_>>) -> Result<i32, Thrown> {
        if let Some(pid) = pid.filter(|pid| !pid.is_undefined()) {
            crate::errors::validate_integer(scope, pid, "pid", -2147483648.0, 2147483647.0)?;
        }
        Ok(0)
    }

    /// <https://nodejs.org/api/os.html#ossetprioritypid-priority>: has no effect.
    #[function(length = 2)]
    pub fn set_priority() {}
}

/// Register `node:os`, whose `constants` is [`constants`].
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let Ok(constants) = constants(scope).and_then(|constants| {
        use js::conversion::ToJSVal;
        constants.to_jsval_throwing(scope)
    }) else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(
        scope,
        name,
        &[("constants", constants)],
    )
}
