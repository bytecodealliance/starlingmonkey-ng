// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end checks of what the `starling` binary writes to stdout and stderr.

#![cfg(not(target_arch = "wasm32"))]

use std::process::{Command, Output};

/// Run `source` as the module `main.mjs`, and return the process output.
fn run_module(source: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(&main, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg(&main)
        .output()
        .expect("failed to run starling")
}

/// `log`, `info` and `debug` print the bare message on stdout. `warn` and
/// `error` print it on stderr, prefixed with the level.
#[test]
fn console_levels_write_to_their_streams() {
    let out = run_module(
        "console.log('a', 1); console.info('b'); console.debug('c');\n\
         console.warn('d'); console.error('e');",
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a 1\nb\nc\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "Warn: d\nError: e\n");
}

/// Messages are written whole, including NUL characters in them.
#[test]
fn console_messages_keep_nul_characters() {
    let out = run_module("console.log('a\\0b', 'c'); console.error('d\\0e');");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"a\0b c\n");
    assert_eq!(out.stderr, b"Error: d\0e\n");
}

/// `console.assert` prints nothing when its condition holds, and otherwise prints
/// "Assertion failed" and its data on stderr.
#[test]
fn console_assert_reports_failed_assertions() {
    let out = run_module(
        "console.assert(true, 'not printed');\n\
         console.assert(1, 'not printed either');\n\
         console.assert(false, 'failed', 1);\n\
         console.assert(0);\n\
         console.assert('', 2);",
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: Assertion failed: failed 1\nError: Assertion failed\nError: Assertion failed 2\n"
    );
}

/// A promise rejected with no handler attached by the end of the microtask
/// drain is reported on stderr with its reason and location, and one that gets
/// a handler in the same turn is not. Neither changes the exit status.
#[test]
fn unhandled_rejections_are_reported() {
    let out = run_module(
        "Promise.reject(new Error('never handled'));\n\
         Promise.reject(new Error('handled')).catch(() => {});\n\
         setTimeout(async () => { throw new Error('from a timer'); }, 0);",
    );
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Uncaught (in promise) never handled\n") && stderr.contains("main.mjs:1:"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Uncaught (in promise) from a timer\n") && stderr.contains("main.mjs:3:"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Uncaught (in promise) handled"),
        "{stderr}"
    );
}

/// A rejection that settles from host I/O, with nothing else left to run, is
/// reported too.
#[test]
fn unhandled_host_io_rejection_is_reported() {
    let out = run_module("fetch('http://127.0.0.1:1/');");
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Uncaught (in promise) "), "{stderr}");
}

/// A top-level `await` that rejects fails the script once, as an evaluation
/// failure, and is not also reported as an unhandled rejection.
#[test]
fn top_level_rejection_is_reported_once() {
    let out = run_module("await Promise.reject(new Error('top level'));");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("top level"), "{stderr}");
    assert!(!stderr.contains("Uncaught (in promise)"), "{stderr}");
}

/// With `--node-compat`, the console formats its arguments with `util.format`, so `%` placeholders
/// are replaced and objects show as `util.inspect` shows them, and `warn` and `error` print without
/// a level prefix. `console.assert` calls `warn` with Node's message.
#[test]
fn node_compat_console_formats_as_node_does() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        "console.log('%s=%d', 'n', 42, { a: 'x', b: [1, { c: new Map([[1, 2]]) }] });\n\
         console.error('err %o', { x: 1 });\n\
         console.dir({ deep: { deeper: 1 } }, { depth: 0 });\n\
         console.assert(false, 'msg %d', 5);\n\
         console.log('100%');",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg("--node-compat")
        .arg(&main)
        .output()
        .expect("failed to run starling");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "n=42 { a: 'x', b: [ 1, { c: [Map] } ] }\n{ deep: [Object] }\n100%\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "err { x: 1 }\nAssertion failed: msg 5\n"
    );
}

/// `fs.writeSync` to descriptor 1 writes its data before it returns, so output without a line
/// break appears before what the console prints next.
#[test]
fn node_compat_fs_write_sync_to_stdout_is_unbuffered() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        "import fs from 'node:fs';\n\
         fs.writeSync(1, 'A'); console.log('B'); fs.writeSync(1, 'C\\n');",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg("--node-compat")
        .arg(&main)
        .output()
        .expect("failed to run starling");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "AB\nC\n");
}
