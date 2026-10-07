// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `internal/test/binding`'s `internalBinding(name)`, and the binding objects it and
//! `process.binding(name)` return. A binding object holds the members of Node.js's binding that
//! are implemented here, and nothing else.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};

/// The names of Node.js's internal bindings, which `internalBinding` accepts.
const BINDINGS: &[&str] = &[
    "async_wrap",
    "block_list",
    "buffer",
    "builtins",
    "cares_wrap",
    "config",
    "constants",
    "contextify",
    "credentials",
    "encoding_binding",
    "errors",
    "fs",
    "fs_dir",
    "fs_event_wrap",
    "heap_utils",
    "http_parser",
    "icu",
    "inspector",
    "js_stream",
    "js_udp_wrap",
    "messaging",
    "module_wrap",
    "mksnapshot",
    "options",
    "os",
    "performance",
    "permission",
    "pipe_wrap",
    "process_methods",
    "process_wrap",
    "report",
    "serdes",
    "signal_wrap",
    "spawn_sync",
    "stream_pipe",
    "stream_wrap",
    "string_decoder",
    "symbols",
    "task_queue",
    "tcp_wrap",
    "timers",
    "trace_events",
    "tty_wrap",
    "types",
    "udp_wrap",
    "url",
    "util",
    "uv",
    "v8",
    "wasi",
    "wasm_web_api",
    "watchdog",
    "worker",
    "zlib",
];

/// The bindings `process.binding` accepts.
const PROCESS_BINDINGS: &[&str] = &[
    "async_wrap",
    "buffer",
    "cares_wrap",
    "config",
    "constants",
    "contextify",
    "fs",
    "fs_event_wrap",
    "http_parser",
    "icu",
    "inspector",
    "js_stream",
    "natives",
    "os",
    "pipe_wrap",
    "process_wrap",
    "signal_wrap",
    "spawn_sync",
    "stream_wrap",
    "tcp_wrap",
    "tls_wrap",
    "tty_wrap",
    "udp_wrap",
    "url",
    "util",
    "uv",
    "v8",
    "zlib",
];

/// The libuv error codes that aren't `errno` values, with their numbers.
const UV_ERRORS: &[(&str, i32)] = &[
    ("EAI_ADDRFAMILY", -3000),
    ("EAI_AGAIN", -3001),
    ("EAI_BADFLAGS", -3002),
    ("EAI_BADHINTS", -3013),
    ("EAI_CANCELED", -3003),
    ("EAI_FAIL", -3004),
    ("EAI_FAMILY", -3005),
    ("EAI_MEMORY", -3006),
    ("EAI_NODATA", -3007),
    ("EAI_NONAME", -3008),
    ("EAI_OVERFLOW", -3009),
    ("EAI_PROTOCOL", -3014),
    ("EAI_SERVICE", -3010),
    ("EAI_SOCKTYPE", -3011),
    ("ECHARSET", -4080),
    ("ENONET", -4056),
    ("UNKNOWN", -4094),
    ("EOF", -4095),
    ("EREMOTEIO", -4030),
    ("EUNATCH", -4023),
];

/// The key of the shared object that holds the binding objects created so far, by name.
static BINDINGS_KEY: u8 = 0;

/// Add the members of `internal/test/binding` to `module`: `internalBinding`.
pub(super) fn add_module_members(scope: &Scope<'_>, module: Object<'_>) -> Result<(), ExnThrown> {
    let internal_binding =
        Function::new_callback(scope, c"internalBinding", 1, internal_binding, ())?;
    module.set_property(scope, c"internalBinding", *internal_binding)
}

/// `internalBinding(name)`: the binding object `name`, or an `Error` with the code
/// `ERR_INVALID_MODULE` for a name that isn't one.
fn internal_binding(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let name = String::from_jsval_throwing(scope, args.get(0), ())?;
    if !BINDINGS.contains(&name.as_str()) {
        let error = NodeError::new(
            ErrorKind::Error,
            "ERR_INVALID_MODULE",
            format!("No such binding: {name}"),
        );
        return Err(Thrown::from(error).throw(scope));
    }
    binding_object(scope, &name)?.to_jsval_raw_throwing(scope)
}

/// `process.binding(name)`: the binding object `name`, if `process.binding` accepts it, and an
/// `Error` otherwise. Emits DEP0111 with `--pending-deprecation`.
pub(crate) fn process_binding<'s>(scope: &'s Scope<'_>, name: &str) -> Result<Object<'s>, Thrown> {
    if crate::runtime_flags::get().pending_deprecation {
        crate::process::deprecation_warning(
            scope,
            "process.binding() is deprecated. Please use public APIs instead.",
            "DEP0111",
        )?;
    }
    if !PROCESS_BINDINGS.contains(&name) {
        return Err(NodeError::new(
            ErrorKind::Error,
            "ERR_INVALID_MODULE",
            format!("No such module: {name}"),
        )
        .into());
    }
    Ok(binding_object(scope, name)?)
}

/// The binding object `name`, created on first use.
pub(crate) fn binding_object<'s>(
    scope: &'s Scope<'_>,
    name: &str,
) -> Result<Object<'s>, ExnThrown> {
    let bindings = js::class::get_or_init_shared_object(
        scope,
        &BINDINGS_KEY as *const u8 as usize,
        Object::new_plain,
    )?;
    let key = std::ffi::CString::new(name).map_err(|_| ExnThrown)?;
    if let Ok(binding) = Object::from_value(scope, bindings.get_property_by_name(scope, &key)?) {
        return Ok(binding);
    }
    let binding = Object::new_plain(scope)?;
    match name {
        "buffer" => {
            let fill = Function::new_callback(scope, c"fill", 5, buffer_fill, ())?;
            binding.set_property(scope, c"fill", *fill)?;
        }
        "constants" => add_constants(scope, binding)?,
        "performance" => {
            let counts = crate::perf_hooks::observer_counts_array(scope)?;
            binding.set_property(scope, c"observerCounts", counts)?;
            let constants = crate::perf_hooks::constants(scope)?;
            binding.set_property(scope, c"constants", constants)?;
        }
        "timers" => {
            let now = Function::new_callback(scope, c"getLibuvNow", 0, get_libuv_now, ())?;
            binding.set_property(scope, c"getLibuvNow", *now)?;
        }
        "uv" => add_uv(scope, binding)?,
        _ => {}
    }
    bindings.set_property_by_name(scope, &key, binding)?;
    Ok(binding)
}

/// `buffer.fill(buffer, value, start, end, encoding)`: `buffer.fill(value, start, end, encoding)`,
/// or `ERR_OUT_OF_RANGE` if `start` and `end` aren't indices of `buffer` with `start <= end`.
fn buffer_fill(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let arg = |index| {
        if index < args.len() {
            args.get(index)
        } else {
            HandleValue::undefined()
        }
    };
    let buffer = Object::from_value(scope, arg(0)).map_err(|e| e.throw(scope))?;
    let length = f64::from_jsval_throwing(scope, buffer.get_property(scope, c"length")?, ())?;
    let start = f64::from_jsval_throwing(scope, arg(2), ())?;
    let end = f64::from_jsval_throwing(scope, arg(3), ())?;
    if !(start >= 0.0 && start <= end && end <= length) {
        let error = NodeError::new(
            ErrorKind::RangeError,
            "ERR_OUT_OF_RANGE",
            "Index out of range",
        );
        return Err(Thrown::from(error).throw(scope));
    }
    let encoding = if arg(4).is_string() {
        arg(4)
    } else {
        HandleValue::undefined()
    };
    crate::buffer::buffer::fill(
        scope,
        buffer,
        arg(1),
        Some(arg(2)),
        Some(arg(3)),
        Some(encoding),
    )
    .map_err(|error| error.throw(scope))?;
    Ok(js::value::undefined())
}

/// `timers.getLibuvNow()`: `process.uptime()` in milliseconds, as an integer.
fn get_libuv_now(_: &Scope<'_>, _: CallbackArgs<'_>, _: HandleValue) -> Result<Value, ExnThrown> {
    Ok(js::value::from_f64(
        (crate::process::functions::uptime() * 1000.0).floor(),
    ))
}

/// Add the members of the `constants` binding to `binding`: `fs` and `os`, which are
/// `fs.constants` and `os.constants`.
fn add_constants(scope: &Scope<'_>, binding: Object<'_>) -> Result<(), ExnThrown> {
    binding.set_property(scope, c"fs", crate::fs::constants(scope)?)?;
    binding.set_property(scope, c"os", crate::os::constants(scope)?)?;
    Ok(())
}

/// Add the members of the `uv` binding to `binding`: a `UV_` constant for each libuv error code,
/// and `errname`.
fn add_uv(scope: &Scope<'_>, binding: Object<'_>) -> Result<(), ExnThrown> {
    for (name, code) in uv_errors() {
        let key = std::ffi::CString::new(format!("UV_{name}")).map_err(|_| ExnThrown)?;
        binding.set_property_by_name(scope, &key, code)?;
    }
    let errname = Function::new_callback(scope, c"errname", 1, uv_errname, ())?;
    binding.set_property(scope, c"errname", *errname)
}

/// The libuv error codes and their numbers: the negated `errno` values of
/// [`crate::os::errno_table`], and [`UV_ERRORS`].
fn uv_errors() -> impl Iterator<Item = (&'static str, i32)> {
    crate::os::errno_table()
        .iter()
        .map(|(name, errno)| (name.to_str().unwrap_or_default(), -errno))
        .chain(UV_ERRORS.iter().copied())
}

/// `uv.errname(code)`: the name of libuv error code `code`, converted to an int32, or `Unknown
/// system error <code>`. Node.js aborts the process for a code that isn't negative, and this
/// throws `ERR_OUT_OF_RANGE` instead.
fn uv_errname(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let code = i32::from_jsval_throwing(
        scope,
        args.get(0),
        js::conversion::ConversionBehavior::Default,
    )?;
    if code >= 0 {
        let received = crate::errors::out_of_range_received(f64::from(code));
        return Err(NodeError::out_of_range("err", "a negative integer", &received).throw(scope));
    }
    let name = uv_errors().find(|&(_, value)| value == code).map_or_else(
        || format!("Unknown system error {code}"),
        |(name, _)| name.to_string(),
    );
    name.as_str().to_jsval_raw_throwing(scope)
}
