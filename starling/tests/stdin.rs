// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `process.stdin` reading standard input piped into the `starling` binary.

#![cfg(not(target_arch = "wasm32"))]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run `crates/builtins/node/tests/js/stdin.mjs` with `input` on standard input, which is closed
/// afterwards if `close`, and return its stdout. Panics if the process doesn't exit within ten
/// seconds.
fn run_with_stdin(input: &str, close: bool) -> String {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/builtins/node/tests/js/stdin.mjs"
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args(["--node-compat", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to run starling");
    let mut stdin = child.stdin.take().expect("a piped stdin");
    stdin.write_all(input.as_bytes()).unwrap();
    let held = if close {
        drop(stdin);
        None
    } else {
        Some(stdin)
    };
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            panic!("starling didn't exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(held);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Closing a `readline` interface over `process.stdin` lets the process exit while standard
/// input stays open. Lines that arrived in the same chunk as the closing line are still emitted,
/// as in Node.js.
#[test]
fn closing_readline_on_stdin_lets_the_process_exit() {
    assert_eq!(
        run_with_stdin("a\nb\nstop\nignored\n", false),
        "{\"lines\":[\"a\",\"b\",\"stop\",\"ignored\"]}\n"
    );
}

/// The end of standard input closes a `readline` interface over `process.stdin`.
#[test]
fn the_end_of_stdin_closes_readline() {
    assert_eq!(run_with_stdin("x\ny", true), "{\"lines\":[\"x\",\"y\"]}\n");
}
