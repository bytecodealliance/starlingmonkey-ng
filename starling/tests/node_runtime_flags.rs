// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end checks of the Node.js runtime flags `--allow-node-runtime-flags` accepts.

#![cfg(not(target_arch = "wasm32"))]

use std::process::{Command, Output};

/// Run `source` as the CommonJS module `main.js` with `--node-compat` and `args` before its path,
/// and return the process output.
fn run(args: &[&str], source: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.js");
    std::fs::write(&main, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg("--node-compat")
        .args(args)
        .arg(&main)
        .output()
        .expect("failed to run starling")
}

/// The flags appear in `process.execArgv` as given, and `--expose-gc` installs `gc`.
#[test]
fn flags_appear_in_exec_argv() {
    let source = "console.log(JSON.stringify(process.execArgv), typeof gc, process.title);";
    let out = run(
        &[
            "--allow-node-runtime-flags",
            "--expose_gc",
            "--title=flagged",
        ],
        source,
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "[\"--expose_gc\",\"--title=flagged\"] function flagged\n"
    );

    let out = run(&[], source);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "[] undefined starlingmonkey\n"
    );
}

/// Without `--allow-node-runtime-flags`, a Node.js flag is an unknown option.
#[test]
fn flags_need_the_option() {
    let out = run(&["--expose-gc"], "");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

/// A rejected flag ends the process with an error naming it, before the script runs.
#[test]
fn rejected_flags_are_errors() {
    let out = run(
        &["--allow-node-runtime-flags", "--allow-natives-syntax"],
        "console.log('ran');",
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Unsupported Node.js runtime flag `--allow-natives-syntax`"),
        "{stderr}"
    );

    let out = run(&["--allow-node-runtime-flags", "--no-such-flag"], "");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Unknown Node.js runtime flag"));
}

/// `--no-warnings` keeps warnings from being printed, and still emits them on `process`.
#[test]
fn no_warnings_suppresses_printing() {
    let source = "process.on('warning', (w) => console.log('warning', w.message));\n\
                  process.emitWarning('careful');";
    let out = run(&["--allow-node-runtime-flags", "--no-warnings"], source);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "warning careful\n");
    assert!(out.stderr.is_empty(), "{out:?}");

    let out = run(&[], source);
    assert!(String::from_utf8_lossy(&out.stderr).contains("Warning: careful"));
}

/// `--expose-internals` makes `internal/` modules loadable.
#[test]
fn expose_internals_resolves_internal_modules() {
    let source = "const { internalBinding } = require('internal/test/binding');\n\
                  const { UV_ENOENT } = internalBinding('uv');\n\
                  const { stringToFlags } = require('internal/fs/utils');\n\
                  console.log(UV_ENOENT < 0, stringToFlags('r') === require('fs').constants.O_RDONLY);";
    let out = run(
        &[
            "--allow-node-runtime-flags",
            "--expose-internals",
            "--no-warnings",
        ],
        source,
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "true true\n");

    let out = run(&[], source);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
}

/// `--abort-on-uncaught-exception` aborts the process on an uncaught exception, without calling
/// the `uncaughtException` listeners.
#[cfg(unix)]
#[test]
fn abort_on_uncaught_exception_aborts() {
    use std::os::unix::process::ExitStatusExt;

    let out = run(
        &[
            "--allow-node-runtime-flags",
            "--abort-on-uncaught-exception",
        ],
        "process.on('uncaughtException', () => console.log('listener'));\n\
         throw new Error('boom');",
    );
    assert_eq!(out.status.signal(), Some(6), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("Uncaught Error: boom\n\nFROM\n"));
}
