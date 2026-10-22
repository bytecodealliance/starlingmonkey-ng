// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end exit-status checks for the `starling` binary.

#![cfg(not(target_arch = "wasm32"))]

use std::ffi::OsStr;
use std::process::{Command, Output};

/// Run `starlingmonkey <args>`, and return the process output.
fn run(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args(args)
        .output()
        .expect("failed to run starling")
}

/// An error before the event loop runs, here an unreadable `-i` initializer
/// script, exits with code 1 rather than aborting.
#[test]
fn initializer_error_exits_with_code_one() {
    let out = run(["-i", "/nonexistent/init.js", "/nonexistent/app.js"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected a clean exit(1), got {:?} (stderr: {})",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Scheduling APIs work during initializer evaluation. The initializer's
/// microtasks drain before the content script runs. Async work left behind by
/// the initializer has nothing to drive it, so the run fails.
#[test]
fn initializer_event_loop_semantics() {
    let dir = tempfile::tempdir().unwrap();

    // Microtasks queued by the initializer are visible to the content script.
    let init = dir.path().join("init.js");
    std::fs::write(
        &init,
        "Promise.resolve().then(() => { globalThis._fromInit = 'ready'; });",
    )
    .unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        "if (globalThis._fromInit !== 'ready') { throw new Error('initializer microtask not drained'); }",
    )
    .unwrap();
    let out = run([OsStr::new("-i"), init.as_os_str(), main.as_os_str()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "initializer microtasks must drain before content (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );

    // A timer left pending when the initializer ends fails the run.
    let init_timer = dir.path().join("init_timer.js");
    std::fs::write(&init_timer, "setTimeout(() => {}, 1000);").unwrap();
    let out = run([OsStr::new("-i"), init_timer.as_os_str(), main.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("left asynchronous work"),
        "expected the leftover-async-work error, got: {stderr}"
    );

    // A timer scheduled and cleared synchronously is fine.
    let init_cleared = dir.path().join("init_cleared.js");
    std::fs::write(
        &init_cleared,
        "globalThis._fromInit = 'ready'; var t = setTimeout(() => {}, 1000); clearTimeout(t);",
    )
    .unwrap();
    let out = run([OsStr::new("-i"), init_cleared.as_os_str(), main.as_os_str()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a synchronously cleared timer must not fail init (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A top-level throw in a module file reports the error and where it was thrown on stderr, and
/// exits with 1.
#[test]
fn top_level_throw_exits_with_code_one_and_reports_its_location() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(&main, "const x = 1;\nthrow new Error('located');").unwrap();
    let out = run([&main]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("located"), "stderr: {stderr}");
    assert!(stderr.contains("main.mjs:2:"), "stderr: {stderr}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("located"),
        "the error must not go to stdout"
    );
}

/// A top level that rejects once a timer has fired, after evaluation handed off to the event
/// loop, reports the error and exits with 1. The module file case has no other work. The `-e`
/// case also checks that the exit does not wait for an interval that keeps the event loop busy.
#[test]
fn late_top_level_rejection_exits_with_code_one() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        "await new Promise((resolve) => setTimeout(resolve, 10));\n\
         throw new Error('late failure');",
    )
    .unwrap();
    let from_file = run([&main]);
    let from_eval = run([
        "-e",
        "setInterval(() => {}, 5);\n\
         await new Promise((resolve) => setTimeout(resolve, 10));\n\
         throw new Error('late failure');",
    ]);
    for (case, out) in [("module file", from_file), ("-e", from_eval)] {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{case}: stderr: {stderr}");
        assert!(stderr.contains("late failure"), "{case}: stderr: {stderr}");
    }
}

/// A top-level `await` on a promise that never settles exits with 1, whether or not the event
/// loop had other work first.
#[test]
fn unsettled_top_level_await_exits_with_code_one() {
    for source in [
        "await new Promise(() => {});",
        "setTimeout(() => {}, 10);\nawait new Promise(() => {});",
    ] {
        let out = run(["-e", source]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{source}: stderr: {stderr}");
        assert!(
            stderr.contains("never settled"),
            "{source}: stderr: {stderr}"
        );
    }
}

/// `--help` exits with 0 and an unknown flag with clap's usage-error status 2.
#[test]
fn argument_errors_use_clap_exit_statuses() {
    assert_eq!(run(["--help"]).status.code(), Some(0));
    assert_eq!(run(["--no-such-flag"]).status.code(), Some(2));
}

/// A syntax error in an imported file module is reported with its own message
/// and location, not replaced by a generic load failure.
#[test]
fn imported_module_syntax_error_keeps_its_location() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(&main, "import './bad.mjs';").unwrap();
    std::fs::write(dir.path().join("bad.mjs"), "export const x = ;").unwrap();
    let out = run([&main]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("expected expression"), "stderr: {stderr}");
    assert!(stderr.contains("bad.mjs:1:"), "stderr: {stderr}");
}
