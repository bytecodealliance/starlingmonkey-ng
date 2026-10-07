// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The Node.js runtime flags the runtime accepted with `--allow-node-runtime-flags`, such as
//! `--expose-gc` and `--no-warnings`.
//!
//! [`set`] parses the flags. Each flag is written as `--name` or `--name=value`, and dashes and
//! underscores in the name are interchangeable, as they are in Node.js. A flag either takes effect,
//! is accepted without effect, or is rejected: see `docs/node-compat.md`.

use std::cell::RefCell;
use std::rc::Rc;

/// The settings the Node.js runtime flags make.
#[derive(Clone, Debug)]
pub struct RuntimeFlags {
    /// `--node-http-listen`, the runtime's own flag: `http.Server.listen()` in a script run as a
    /// command accepts connections itself.
    pub http_listen: bool,
    /// The flags as given, for `process.execArgv`.
    pub exec_argv: Vec<String>,
    /// `--expose-internals`: `require` and `import` resolve Node.js's `internal/` modules.
    pub expose_internals: bool,
    /// `--expose-gc`: the global `gc` function.
    pub expose_gc: bool,
    /// `false` with `--no-warnings`: process warnings aren't printed.
    pub warnings: bool,
    /// `--pending-deprecation`: pending deprecations emit their warnings.
    pub pending_deprecation: bool,
    /// `--no-deprecation`: the initial `process.noDeprecation`.
    pub no_deprecation: bool,
    /// `--throw-deprecation`: the initial `process.throwDeprecation`.
    pub throw_deprecation: bool,
    /// `--trace-warnings`: the initial `process.traceProcessWarnings`.
    pub trace_warnings: bool,
    /// `--trace-deprecation`: the initial `process.traceDeprecation`.
    pub trace_deprecation: bool,
    /// `--abort-on-uncaught-exception`: an uncaught exception aborts the process.
    pub abort_on_uncaught_exception: bool,
    /// `--title`: the initial `process.title`.
    pub title: Option<String>,
    /// `false` with `--no-network-family-autoselection`: the initial
    /// `net.getDefaultAutoSelectFamily()`.
    pub network_family_autoselection: bool,
    /// `--network-family-autoselection-attempt-timeout`: the initial
    /// `net.getDefaultAutoSelectFamilyAttemptTimeout()`, in milliseconds.
    pub network_family_autoselection_attempt_timeout: f64,
    /// `--experimental-stream-iter`: the `stream/iter` module exists.
    pub experimental_stream_iter: bool,
    /// `--test-udp-no-try-send`: `dgram` sockets queue every send instead of sending at once.
    pub test_udp_no_try_send: bool,
    /// `--insecure-http-parser`: HTTP messages are parsed leniently unless their server or
    /// request sets `insecureHTTPParser` or `httpValidation`.
    pub insecure_http_parser: bool,
    /// `false` with `--no-experimental-websocket`: the globals `WebSocket` and `CloseEvent`
    /// exist.
    pub experimental_websocket: bool,
    /// `--dns-result-order`: the initial `dns.getDefaultResultOrder()`, one of `verbatim`,
    /// `ipv4first` and `ipv6first`.
    pub dns_result_order: Option<String>,
    /// `--unhandled-rejections`: how a promise rejected with no handler attached is handled.
    pub unhandled_rejections: UnhandledRejections,
}

/// The modes of `--unhandled-rejections`. See
/// <https://nodejs.org/api/cli.html#--unhandled-rejectionsmode>.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnhandledRejections {
    /// `throw`: emit `unhandledRejection`, and handle the rejection as an uncaught exception if
    /// nothing listens.
    #[default]
    Throw,
    /// `strict`: handle the rejection as an uncaught exception, then emit `unhandledRejection`,
    /// and warn if nothing listens.
    Strict,
    /// `warn`: emit `unhandledRejection`, and warn.
    Warn,
    /// `none`: emit `unhandledRejection`.
    None,
    /// `warn-with-error-code`: emit `unhandledRejection`, and warn and set `process.exitCode` to 1
    /// if nothing listens.
    WarnWithErrorCode,
}

impl Default for RuntimeFlags {
    fn default() -> Self {
        Self {
            http_listen: false,
            exec_argv: Vec::new(),
            expose_internals: false,
            expose_gc: false,
            warnings: true,
            pending_deprecation: false,
            no_deprecation: false,
            throw_deprecation: false,
            trace_warnings: false,
            trace_deprecation: false,
            abort_on_uncaught_exception: false,
            title: None,
            network_family_autoselection: true,
            network_family_autoselection_attempt_timeout: 250.0,
            experimental_stream_iter: false,
            test_udp_no_try_send: false,
            insecure_http_parser: false,
            experimental_websocket: true,
            dns_result_order: None,
            unhandled_rejections: UnhandledRejections::Throw,
        }
    }
}

js::instance_local! {
    static FLAGS: RefCell<Option<Rc<RuntimeFlags>>> = const { RefCell::new(None) };
}

/// The flags that are accepted without effect.
const IGNORED: &[&str] = &[
    "zero-fill-buffers",
    "max-old-space-size",
    "max-semi-space-size",
    "no-concurrent-array-buffer-sweeping",
    "experimental-vm-modules",
];

/// The flags that are rejected, and the reason for each.
const REJECTED: &[(&str, &str)] = &[
    (
        "allow-natives-syntax",
        "V8's `%` intrinsics have no SpiderMonkey equivalent",
    ),
    (
        "expose-externalize-string",
        "V8's external strings have no SpiderMonkey equivalent",
    ),
    ("permission", "the permission model is not implemented"),
    ("allow-fs-read", "the permission model is not implemented"),
    ("allow-fs-write", "the permission model is not implemented"),
    (
        "allow-child-process",
        "the permission model is not implemented",
    ),
    ("allow-worker", "the permission model is not implemented"),
    ("allow-addons", "the permission model is not implemented"),
    ("allow-wasi", "the permission model is not implemented"),
];

/// Parse `flags`, the Node.js runtime flags as given on the command line, and make their settings
/// and `http_listen`, from `--node-http-listen`, the ones [`get`] returns. Returns an error naming
/// the first flag that is unknown, rejected, or malformed.
pub fn set(flags: &[String], http_listen: bool) -> Result<(), String> {
    let settings = parse(flags, http_listen)?;
    FLAGS.with(|cell| *cell.borrow_mut() = Some(Rc::new(settings)));
    Ok(())
}

/// The settings `flags`, the Node.js runtime flags as given on the command line, make, with
/// `http_listen` from `--node-http-listen`. Returns an error naming the first flag that is unknown,
/// rejected, or malformed.
pub fn parse(flags: &[String], http_listen: bool) -> Result<RuntimeFlags, String> {
    let mut settings = RuntimeFlags {
        http_listen,
        exec_argv: flags.to_vec(),
        ..RuntimeFlags::default()
    };
    for flag in flags {
        apply(&mut settings, flag)?;
    }
    Ok(settings)
}

/// The settings [`set`] last made, or the defaults if it wasn't called.
pub(crate) fn get() -> Rc<RuntimeFlags> {
    FLAGS.with(|cell| cell.borrow_mut().get_or_insert_default().clone())
}

/// Apply `flag` to `settings`.
fn apply(settings: &mut RuntimeFlags, flag: &str) -> Result<(), String> {
    let Some(option) = flag.strip_prefix("--") else {
        return Err(format!("Unsupported Node.js runtime flag `{flag}`"));
    };
    let (name, value) = match option.split_once('=') {
        Some((name, value)) => (name.replace('_', "-"), Some(value)),
        None => (option.replace('_', "-"), None),
    };
    if let Some((_, reason)) = REJECTED.iter().find(|(rejected, _)| *rejected == name) {
        return Err(format!(
            "Unsupported Node.js runtime flag `{flag}`: {reason}"
        ));
    }
    if IGNORED.contains(&name.as_str()) {
        return Ok(());
    }
    let switch = |setting: &mut bool, enabled: bool| match value {
        None => {
            *setting = enabled;
            Ok(())
        }
        Some(_) => Err(format!("Node.js runtime flag `--{name}` takes no value")),
    };
    let required = || value.ok_or_else(|| format!("Node.js runtime flag `--{name}` needs a value"));
    match name.as_str() {
        "expose-internals" => switch(&mut settings.expose_internals, true),
        "expose-gc" => switch(&mut settings.expose_gc, true),
        "warnings" => switch(&mut settings.warnings, true),
        "no-warnings" => switch(&mut settings.warnings, false),
        "pending-deprecation" => switch(&mut settings.pending_deprecation, true),
        "deprecation" => switch(&mut settings.no_deprecation, false),
        "no-deprecation" => switch(&mut settings.no_deprecation, true),
        "throw-deprecation" => switch(&mut settings.throw_deprecation, true),
        "trace-warnings" => switch(&mut settings.trace_warnings, true),
        "trace-deprecation" => switch(&mut settings.trace_deprecation, true),
        "abort-on-uncaught-exception" => switch(&mut settings.abort_on_uncaught_exception, true),
        "experimental-stream-iter" => switch(&mut settings.experimental_stream_iter, true),
        "test-udp-no-try-send" => switch(&mut settings.test_udp_no_try_send, true),
        "insecure-http-parser" => switch(&mut settings.insecure_http_parser, true),
        "experimental-websocket" => switch(&mut settings.experimental_websocket, true),
        "no-experimental-websocket" => switch(&mut settings.experimental_websocket, false),
        "network-family-autoselection" => switch(&mut settings.network_family_autoselection, true),
        "no-network-family-autoselection" => {
            switch(&mut settings.network_family_autoselection, false)
        }
        "network-family-autoselection-attempt-timeout" => {
            let value = required()?;
            let timeout = value.parse::<u32>().map_err(|_| {
                format!("Invalid value `{value}` for Node.js runtime flag `--{name}`")
            })?;
            settings.network_family_autoselection_attempt_timeout = f64::from(timeout);
            Ok(())
        }
        "title" => {
            settings.title = Some(required()?.to_string());
            Ok(())
        }
        "dns-result-order" => {
            let value = required()?;
            if !["verbatim", "ipv4first", "ipv6first"].contains(&value) {
                return Err(format!(
                    "The argument '--dns-result-order' must be one of: 'verbatim', 'ipv4first', \
                     'ipv6first'. Received '{value}'"
                ));
            }
            settings.dns_result_order = Some(value.to_string());
            Ok(())
        }
        "unhandled-rejections" => {
            settings.unhandled_rejections = match required()? {
                "throw" => UnhandledRejections::Throw,
                "strict" => UnhandledRejections::Strict,
                "warn" => UnhandledRejections::Warn,
                "none" => UnhandledRejections::None,
                "warn-with-error-code" => UnhandledRejections::WarnWithErrorCode,
                _ => return Err("invalid value for --unhandled-rejections".into()),
            };
            Ok(())
        }
        _ => Err(format!("Unknown Node.js runtime flag `{flag}`")),
    }
}
