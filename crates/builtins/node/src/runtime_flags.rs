// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The Node.js runtime flags the runtime accepted with `--allow-node-runtime-flags`, such as
//! `--expose-gc` and `--no-warnings`.
//!
//! [`set`] parses the flags. Each flag is written as `--name` or `--name=value`, and dashes and
//! underscores in the name are interchangeable, as they are in Node.js. A flag either takes effect,
//! is accepted without effect, or is rejected: see `docs/node-compat.md`.

use std::cell::RefCell;

/// The settings the Node.js runtime flags make.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeFlags {
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
            abort_on_uncaught_exception: false,
            title: None,
            network_family_autoselection: true,
            network_family_autoselection_attempt_timeout: 250.0,
            experimental_stream_iter: false,
        }
    }
}

js::instance_local! {
    static FLAGS: RefCell<Option<RuntimeFlags>> = const { RefCell::new(None) };
}

/// The flags that are accepted without effect.
const IGNORED: &[&str] = &[
    "zero-fill-buffers",
    "insecure-http-parser",
    "max-old-space-size",
    "max-semi-space-size",
    "no-concurrent-array-buffer-sweeping",
    "experimental-vm-modules",
];

/// The flags that are rejected, and the reason for each. `UNSUPPORTED_FLAGS` in
/// `tests/node-harness/run-node-tests.mjs` lists the same flags.
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
    let mut settings = RuntimeFlags {
        http_listen,
        exec_argv: flags.to_vec(),
        ..RuntimeFlags::default()
    };
    for flag in flags {
        apply(&mut settings, flag)?;
    }
    FLAGS.with(|cell| *cell.borrow_mut() = Some(settings));
    Ok(())
}

/// The settings of the flags [`set`] last parsed, or the defaults if it wasn't called.
pub(crate) fn get() -> RuntimeFlags {
    FLAGS.with(|cell| cell.borrow().clone().unwrap_or_default())
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
        "abort-on-uncaught-exception" => switch(&mut settings.abort_on_uncaught_exception, true),
        "experimental-stream-iter" => switch(&mut settings.experimental_stream_iter, true),
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
        _ => Err(format!("Unknown Node.js runtime flag `{flag}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(flags: &[&str]) -> Result<RuntimeFlags, String> {
        let mut settings = RuntimeFlags::default();
        for flag in flags {
            apply(&mut settings, flag)?;
        }
        Ok(settings)
    }

    #[test]
    fn dashes_and_underscores_are_interchangeable() {
        let settings = parse(&["--expose_gc", "--expose-internals", "--no_warnings"]).unwrap();
        assert!(settings.expose_gc);
        assert!(settings.expose_internals);
        assert!(!settings.warnings);
    }

    #[test]
    fn valued_flags_take_their_value_after_an_equals_sign() {
        let settings = parse(&[
            "--title=foo",
            "--network-family-autoselection-attempt-timeout=123",
            "--no-network-family-autoselection",
            "--max-old-space-size=16",
        ])
        .unwrap();
        assert_eq!(settings.title.as_deref(), Some("foo"));
        assert_eq!(settings.network_family_autoselection_attempt_timeout, 123.0);
        assert!(!settings.network_family_autoselection);
        assert!(parse(&["--title"]).is_err());
        assert!(parse(&["--expose-gc=1"]).is_err());
        assert!(parse(&["--network-family-autoselection-attempt-timeout=x"]).is_err());
    }

    #[test]
    fn unsupported_flags_are_rejected_with_a_reason() {
        let error = parse(&["--allow-natives-syntax"]).unwrap_err();
        assert!(error.contains("`--allow-natives-syntax`"), "{error}");
        assert!(error.contains("intrinsics"), "{error}");
        assert!(parse(&["--allow-fs-read=*"]).is_err());
        assert!(parse(&["--permission"]).is_err());
        let error = parse(&["--no-such-flag"]).unwrap_err();
        assert_eq!(error, "Unknown Node.js runtime flag `--no-such-flag`");
        assert!(parse(&["-r"]).is_err());
    }
}
