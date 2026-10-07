// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end exit-status checks for the `starling` binary.

#![cfg(not(target_arch = "wasm32"))]

use std::process::Command;

/// An error before the event loop runs, here an unreadable `-i` initializer
/// script, exits with code 1 rather than aborting.
#[test]
fn initializer_error_exits_with_code_one() {
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args(["-i", "/nonexistent/init.js", "/nonexistent/app.js"])
        .output()
        .expect("failed to run starling");
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected a clean exit(1), got {:?} (stderr: {})",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Scheduling APIs work during initializer evaluation, and the initializer's
/// microtasks drain before the content script runs — but async work left
/// behind has nothing to drive it and must be a hard error.
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
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args(["-i", &init.to_string_lossy(), &main.to_string_lossy()])
        .output()
        .expect("failed to run starling");
    assert_eq!(
        out.status.code(),
        Some(0),
        "initializer microtasks must drain before content (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );

    // setTimeout no longer throws "No active event loop" — but a timer left
    // pending when the initializer ends is an error.
    let init_timer = dir.path().join("init_timer.js");
    std::fs::write(&init_timer, "setTimeout(() => {}, 1000);").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args(["-i", &init_timer.to_string_lossy(), &main.to_string_lossy()])
        .output()
        .expect("failed to run starling");
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
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args([
            "-i",
            &init_cleared.to_string_lossy(),
            &main.to_string_lossy(),
        ])
        .output()
        .expect("failed to run starling");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a synchronously cleared timer must not fail init (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A content script that throws reports the error on stderr and exits with 1.
#[test]
fn throwing_script_exits_with_code_one() {
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args(["-e", "throw new Error('boom')"])
        .output()
        .expect("failed to run starling");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("boom"), "stderr: {stderr}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("boom"),
        "the error must not go to stdout"
    );
}

/// A top-level throw in a module file reports where it was thrown.
#[test]
fn top_level_throw_reports_its_location() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(&main, "const x = 1;\nthrow new Error('located');").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg(&main)
        .output()
        .expect("failed to run starling");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("located"), "stderr: {stderr}");
    assert!(stderr.contains("main.mjs:2:"), "stderr: {stderr}");
}

/// A top level that rejects once a timer has fired, after evaluation handed off to the event
/// loop, reports the error and exits with 1.
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
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg(&main)
        .output()
        .expect("failed to run starling");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("late failure"), "stderr: {stderr}");
}

/// A top level that rejects while an interval keeps the event loop busy reports the error and
/// exits with 1, without waiting for the interval.
#[test]
fn late_rejection_with_work_left_exits_with_code_one() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        "setInterval(() => {}, 5);\n\
         await new Promise((resolve) => setTimeout(resolve, 10));\n\
         throw new Error('late failure');",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg(&main)
        .output()
        .expect("failed to run starling");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("late failure"), "stderr: {stderr}");
}

/// A top-level `await` on a promise nothing settles exits with 1, whether or not the event loop
/// had other work first.
#[test]
fn unsettled_top_level_await_exits_with_code_one() {
    for source in [
        "await new Promise(() => {});",
        "setTimeout(() => {}, 10);\nawait new Promise(() => {});",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
            .args(["-e", source])
            .output()
            .expect("failed to run starling");
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
    let help = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg("--help")
        .output()
        .expect("failed to run starling");
    assert_eq!(help.status.code(), Some(0));
    let bad = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg("--no-such-flag")
        .output()
        .expect("failed to run starling");
    assert_eq!(bad.status.code(), Some(2));
}

/// A syntax error in an imported file module is reported with its own message
/// and location, not replaced by a generic load failure.
#[test]
fn imported_module_syntax_error_keeps_its_location() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(&main, "import './bad.mjs';").unwrap();
    std::fs::write(dir.path().join("bad.mjs"), "export const x = ;").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg(&main)
        .output()
        .expect("failed to run starling");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("expected expression"), "stderr: {stderr}");
    assert!(stderr.contains("bad.mjs:1:"), "stderr: {stderr}");
}

/// When an `uncaughtException` listener handles a throw from the entry script's top level, the
/// ticks and promise reactions it queued run, in Node.js's order, and the process exits with 0.
#[test]
fn jobs_an_uncaught_exception_listener_queues_run() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.js");
    std::fs::write(
        &main,
        "process.on('uncaughtException', (e) => {\n\
         console.log('caught', e.message);\n\
         process.nextTick(() => console.log('tick'));\n\
         Promise.resolve().then(() => console.log('reaction'));\n\
         });\n\
         throw new Error('top');\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg("--node-compat")
        .arg(&main)
        .output()
        .expect("failed to run starling");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "caught top\ntick\nreaction\n"
    );
}

/// When an `uncaughtException` listener handles a rejection of the entry module's top-level
/// `await`, the promise reactions it queued run before its ticks, as in Node.js.
#[test]
fn jobs_a_listener_queues_for_a_top_level_rejection_run() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        "process.on('uncaughtException', (e) => {\n\
         console.log('caught', e.message);\n\
         process.nextTick(() => console.log('tick'));\n\
         Promise.resolve().then(() => console.log('reaction'));\n\
         });\n\
         await Promise.reject(new Error('top'));\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg("--node-compat")
        .arg(&main)
        .output()
        .expect("failed to run starling");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "caught top\nreaction\ntick\n"
    );
}

/// `-p` runs its code as `-e` does, and logs the completion value when the process exits, after
/// the timers the code started.
#[test]
fn print_logs_the_completion_value_at_exit() {
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args([
            "--node-compat",
            "-p",
            "setTimeout(() => console.log('timer')); require('path').join('a', 'b')",
        ])
        .output()
        .expect("failed to run starling");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "timer\na/b\n");
}

/// `-p` refuses code that only compiles as an ES module with `ERR_EVAL_ESM_CANNOT_PRINT`.
#[test]
fn print_refuses_es_module_input() {
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args(["--node-compat", "-p", "await 3"])
        .output()
        .expect("failed to run starling");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ERR_EVAL_ESM_CANNOT_PRINT"),
        "{out:?}"
    );
}

/// `--trace-warnings` prints a warning with its stack frames and without the hint about the
/// tracing flags.
#[test]
fn trace_warnings_prints_the_stack() {
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args([
            "--node-compat",
            "--allow-node-runtime-flags",
            "--trace-warnings",
            "-e",
            "process.emitWarning('w')",
        ])
        .output()
        .expect("failed to run starling");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(") Warning: w\n    at "), "{stderr}");
    assert!(!stderr.contains("--trace-warnings ..."), "{stderr}");
}

/// Run `source` as the CommonJS entry script `main.js` with `--node-compat`.
fn run_node_script(source: &str) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.js");
    std::fs::write(&main, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg("--node-compat")
        .arg(&main)
        .output()
        .expect("failed to run starling")
}

/// `uncaughtExceptionMonitor` listeners get an uncaught exception or rejection and its origin
/// before the `uncaughtException` listeners, and also when nothing else listens.
#[test]
fn uncaught_exception_monitor_runs_first() {
    let out = run_node_script(
        "process.on('uncaughtExceptionMonitor', (e, o) => console.log('monitor', e.message, o));\n\
         process.on('uncaughtException', (e, o) => console.log('handler', e.message, o));\n\
         setTimeout(() => Promise.reject(new Error('rejected')), 1);\n\
         throw new Error('thrown');\n",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "monitor thrown uncaughtException\nhandler thrown uncaughtException\n\
         monitor rejected unhandledRejection\nhandler rejected unhandledRejection\n"
    );
    let out = run_node_script(
        "process.on('uncaughtExceptionMonitor', (e, o) => console.log('monitor', e.message, o));\n\
         throw new Error('thrown');\n",
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "monitor thrown uncaughtException\n"
    );
}

/// An exception an `unhandledRejection` listener throws is an uncaught exception of origin
/// `uncaughtException`, which exits with code 1 if nothing handles it.
#[test]
fn unhandled_rejection_listener_exceptions_are_uncaught() {
    let out = run_node_script(
        "process.on('unhandledRejection', () => { throw new Error('boom'); });\n\
         process.on('uncaughtException', (e, o) => console.log('caught', e.message, o));\n\
         Promise.reject(1);\n",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "caught boom uncaughtException\n"
    );
    let out = run_node_script(
        "process.on('unhandledRejection', () => { throw new Error('boom'); });\n\
         Promise.reject(1);\n",
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Error: boom"));
}

/// With `process.throwDeprecation`, `emitWarning` returns and throws a deprecation warning in a
/// tick, as an uncaught exception.
#[test]
fn thrown_deprecations_are_uncaught_exceptions() {
    let out = run_node_script(
        "process.throwDeprecation = true;\n\
         process.on('uncaughtException', (e) => console.log('uncaught', e.name, e.message));\n\
         try {\n\
         process.emitWarning('x', 'DeprecationWarning');\n\
         console.log('returned');\n\
         } catch {\n\
         console.log('caught');\n\
         }\n",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "returned\nuncaught DeprecationWarning x\n"
    );
}
