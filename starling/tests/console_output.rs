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
/// `error` print it on stderr, prefixed with the level. `log` returns
/// `undefined`.
#[test]
fn console_levels_write_to_their_streams() {
    let out = run_module(
        "console.log('a', 1); console.info('b'); console.debug('c');\n\
         console.warn('d'); console.error('e');\n\
         console.log(String(console.log('f')));",
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "a 1\nb\nc\nf\nundefined\n"
    );
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

/// `console.assert` doesn't print anything when its condition holds, and otherwise prints
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

/// `--serve 0` listens on a port the system picks and prints its address on
/// stderr, where a client can read it and connect.
#[test]
fn serve_on_port_zero_prints_the_bound_address() {
    use std::io::{BufRead, Read, Write};

    let mut child = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args([
            "--serve",
            "0",
            "--legacy-script",
            "-e",
            "addEventListener('fetch', (e) => e.respondWith(new Response('served')))",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run starling");
    let stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let address = stderr
        .lines()
        .map_while(Result::ok)
        .find_map(|line| Some(line.strip_prefix("serving on http://")?.to_string()))
        .expect("the server printed its address before closing stderr");

    let mut stream = std::net::TcpStream::connect(&address).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    let read = stream.read_to_string(&mut response);
    let _ = child.kill();
    let _ = child.wait();
    read.unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200") && response.ends_with("served"),
        "{response}"
    );
}
