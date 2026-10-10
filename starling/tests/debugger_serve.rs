// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end tests of `starlingmonkey --serve` with `--debug`, driven by a DAP client over TCP
//! and HTTP requests to the server.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::ffi::OsStr;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use serve_test_support::{full_request_within, PATIENCE};
use support::dap::{Dap, Lines};
use support::{
    free_port, free_ports, get, get_path, lock_ports, stop_in_handler_and_continue, write_script,
    Spawned, HANDLER,
};

/// The prefix of the line a server prints once it listens.
const SERVING: &str = "serving on http://127.0.0.1:";

/// Start `starlingmonkey --serve=<port> --debug=<debug> <args> <script>`, and return the process
/// and the lines of its stderr.
fn spawn_server(port: u16, args: &[&str], debug: &str, script: &Path) -> (Spawned, Lines) {
    let mut spawned = support::spawn(
        [format!("--serve={port}"), format!("--debug={debug}")]
            .iter()
            .map(OsStr::new)
            .chain(args.iter().map(OsStr::new))
            .chain([script.as_os_str()]),
    );
    let stderr = Lines::read(spawned.child().stderr.take().expect("piped stderr"));
    (spawned, stderr)
}

/// Start a server for `script` with `--debug=listen:0,wait` and `args`, attach to it, set
/// breakpoints on `lines`, and configure the session. Returns the client, the server's port and the
/// lines of its stderr once the server listens.
fn attach_to_server(script: &Path, args: &[&str], lines: &[u32]) -> (Dap, u16, Lines) {
    attach_to_server_with(script, args, |dap| {
        dap.set_breakpoints(script, lines);
    })
}

/// [`attach_to_server`], with `before_configuration` in place of setting breakpoints.
fn attach_to_server_with(
    script: &Path,
    args: &[&str],
    before_configuration: impl FnOnce(&mut Dap),
) -> (Dap, u16, Lines) {
    let _ports = lock_ports();
    let port = free_port();
    let (spawned, mut stderr) = spawn_server(port, args, "listen:0,wait", script);
    let debug_port: u16 = stderr
        .after("debugger listening on 127.0.0.1:")
        .parse()
        .expect("a port number");
    let mut dap = Dap::attach_tcp(
        spawned.into_child(),
        SocketAddr::from((Ipv4Addr::LOCALHOST, debug_port)),
    );
    dap.start(json!({}));
    before_configuration(&mut dap);
    dap.request("configurationDone", json!({}));
    stderr.after(SERVING);
    (dap, port, stderr)
}

/// Wait until a server listens on `port`. Panics if none does within 60 s.
fn wait_for_server(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err() {
        assert!(Instant::now() < deadline, "nothing listens on port {port}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn terminate(mut dap: Dap) {
    dap.request("disconnect", json!({ "terminateDebuggee": true }));
    dap.event("terminated");
    assert!(dap.wait_for_exit().success());
}

/// The number of `lines` that report a failed `connect`.
fn connect_failures(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| line.contains("running without debugging"))
        .count()
}

#[test]
fn native_serve_pauses_in_the_fetch_handler() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let (mut dap, port, _stderr) = attach_to_server(&script, &[], &[2]);
    stop_in_handler_and_continue(&mut dap, port);
    terminate(dap);
}

#[test]
fn native_serve_isolated_globals_are_debuggees() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let (mut dap, port, _stderr) = attach_to_server(&script, &["--serve-isolated"], &[2]);
    stop_in_handler_and_continue(&mut dap, port);
    terminate(dap);
}

#[test]
fn native_serve_isolated_breakpoints_stop_in_every_request() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let (mut dap, port, _stderr) = attach_to_server(&script, &["--serve-isolated"], &[2]);
    stop_in_handler_and_continue(&mut dap, port);
    stop_in_handler_and_continue(&mut dap, port);
    dap.set_breakpoints(&script, &[3]);
    let response = get(port);
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    assert_eq!(dap.top_frame_line(), 3);
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(response.join().unwrap(), "42");
    terminate(dap);
}

#[test]
fn native_serve_debugs_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let ports = lock_ports();
    let port = free_port();
    let serve = format!("--serve={port}");
    let mut dap = Dap::spawn_with_args(&[&serve, script.to_str().expect("UTF-8 script path")]);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[2]);
    dap.request("configurationDone", json!({}));
    wait_for_server(port);
    drop(ports);
    stop_in_handler_and_continue(&mut dap, port);
    terminate(dap);
}

#[test]
fn native_serve_evaluate_while_idle_can_use_timers() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let (mut dap, _port, _stderr) = attach_to_server(&script, &[], &[2]);
    let evaluated = dap.evaluate("setTimeout(() => console.log('from eval'), 0); 1");
    assert_eq!(evaluated["result"], "1");
    dap.wait_for_output("from eval\n");
    terminate(dap);
}

#[test]
fn native_serve_runs_a_timer_evaluated_before_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let (mut dap, _port, _stderr) = attach_to_server_with(&script, &[], |dap| {
        let evaluated = dap.evaluate("setTimeout(() => console.log('timer'), 0); 1");
        assert_eq!(evaluated["result"], "1");
    });
    dap.wait_for_output("timer\n");
    terminate(dap);
}

#[test]
fn native_serve_isolated_runs_an_evaluated_timer_while_a_request_is_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.js",
        &[
            "addEventListener('fetch', (e) => {",
            "  e.respondWith(new Promise((resolve) => {",
            "    setTimeout(() => resolve(new Response('ok')), 1500);",
            "  }));",
            "});",
        ],
    );
    let (mut dap, port, _stderr) = attach_to_server(&script, &["--serve-isolated"], &[]);
    let evaluated = dap.evaluate("setTimeout(() => console.log('from eval'), 500); 1");
    assert_eq!(evaluated["result"], "1");
    let response = get(port);
    dap.wait_for_output("from eval\n");
    assert_eq!(response.join().unwrap(), "ok");
    terminate(dap);
}

#[test]
fn native_serve_isolated_evaluate_while_idle_runs_in_the_default_global() {
    let dir = tempfile::tempdir().unwrap();
    let mut lines = vec!["globalThis.requestGlobal = true;"];
    lines.extend_from_slice(HANDLER);
    let script = write_script(dir.path(), "main.js", &lines);
    let (mut dap, port, _stderr) = attach_to_server(&script, &["--serve-isolated"], &[]);
    for _ in 0..3 {
        assert_eq!(get(port).join().unwrap(), "42");
    }
    let evaluated = dap.evaluate("typeof requestGlobal");
    assert_eq!(evaluated["result"], "\"undefined\"");
    terminate(dap);
}

#[test]
fn native_serve_pause_holds_a_request_in_flight_until_the_debuggee_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.js",
        &[
            "addEventListener('fetch', (e) => {",
            "  const path = new URL(e.request.url).pathname;",
            "  console.log(path);",
            "  if (path === '/pause') {",
            "    debugger;",
            "  }",
            "  const delay = path === '/slow' ? 1500 : 0;",
            "  e.respondWith(new Promise((resolve) => {",
            "    setTimeout(() => resolve(new Response(path)), delay);",
            "  }));",
            "});",
        ],
    );
    let (mut dap, port, _stderr) = attach_to_server(&script, &[], &[]);
    let slow = get_path(port, "/slow");
    dap.wait_for_output("/slow\n");
    let paused = get_path(port, "/pause");
    assert_eq!(dap.event("stopped")["reason"], "debugger statement");
    assert_eq!(dap.top_frame_line(), 5);
    // The slow request's timer comes due during the pause, and its response waits for the resume.
    std::thread::sleep(Duration::from_millis(2000));
    assert!(!slow.is_finished(), "a request was answered while paused");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(slow.join().unwrap(), "/slow");
    assert_eq!(paused.join().unwrap(), "/pause");
    assert_eq!(get(port).join().unwrap(), "/");
    terminate(dap);
}

#[test]
fn native_serve_connect_reconnects_per_request() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let ports = lock_ports();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let debug_port = listener.local_addr().unwrap().port();
    let port = free_port();
    let (spawned, mut stderr) = spawn_server(
        port,
        &[],
        &format!("connect:127.0.0.1:{debug_port}"),
        &script,
    );

    // The first connection comes from the before-content hook at startup.
    let mut first = Dap::accept_tcp(spawned.into_child(), &listener);
    first.start(json!({}));
    first.request("configurationDone", json!({}));
    stderr.after(SERVING);
    drop(ports);
    first.request("disconnect", json!({}));
    first.wait_for_close();

    // The next request connects again, and waits until that client is configured.
    let response = get(port);
    let mut second = Dap::accept_tcp(first.take_child(), &listener);
    second.start(json!({}));
    second.set_breakpoints(&script, &[2]);
    second.request("configurationDone", json!({}));
    assert_eq!(second.event("stopped")["reason"], "breakpoint");
    assert_eq!(second.top_frame_line(), 2);
    second.request("continue", json!({ "threadId": 1 }));
    assert_eq!(response.join().unwrap(), "42");

    // A DAP request sent while the debuggee runs is dispatched.
    second.request("disconnect", json!({}));
    second.wait_for_close();

    // With no client listening, requests are served, and only the first failed connection since
    // the last successful one is reported.
    let _ports = lock_ports();
    drop(listener);
    assert_eq!(get(port).join().unwrap(), "42");
    assert_eq!(get(port).join().unwrap(), "42");
    drop(second);
    assert_eq!(connect_failures(&stderr.until_end()), 1);
}

#[test]
fn native_serve_connect_without_listener_serves() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let _ports = lock_ports();
    let [debug_port, port] = free_ports();
    let (spawned, mut stderr) = spawn_server(
        port,
        &[],
        &format!("connect:127.0.0.1:{debug_port}"),
        &script,
    );
    stderr.after(SERVING);
    for _ in 0..2 {
        let response =
            full_request_within(port, "GET", "/", "", PATIENCE).expect("the server responded");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("\r\n\r\n42"), "{response}");
    }
    drop(spawned);
    // The failed connection at startup is reported, and the ones for the two requests are not.
    assert_eq!(connect_failures(&stderr.until_end()), 1);
}

#[test]
fn native_serve_connect_reports_the_first_failure_after_a_connection() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let _ports = lock_ports();
    let [debug_port, port] = free_ports();
    let (spawned, mut stderr) = spawn_server(
        port,
        &[],
        &format!("connect:127.0.0.1:{debug_port}"),
        &script,
    );
    stderr.after(SERVING);
    assert_eq!(get(port).join().unwrap(), "42");

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, debug_port)).unwrap();
    let response = get(port);
    let mut dap = Dap::accept_tcp(spawned.into_child(), &listener);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    assert_eq!(response.join().unwrap(), "42");
    dap.request("disconnect", json!({}));
    dap.wait_for_close();
    drop(listener);

    assert_eq!(get(port).join().unwrap(), "42");
    assert_eq!(get(port).join().unwrap(), "42");
    drop(dap);
    // The failures at startup and for the first request after the connection ended are reported.
    assert_eq!(connect_failures(&stderr.until_end()), 2);
}

#[test]
fn native_serve_connect_to_a_client_that_closes_at_once_logs_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.js",
        &[
            "addEventListener('fetch', (e) => {",
            "  console.log('handled');",
            "  e.respondWith(new Response('42'));",
            "});",
        ],
    );
    let ports = lock_ports();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let debug_port = listener.local_addr().unwrap().port();
    let port = free_port();
    // Accepts and closes the connections made at startup and for two requests.
    let closer = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut accepted = 0;
        while accepted < 3 {
            match listener.accept() {
                Ok(_) => accepted += 1,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "accepted {accepted} connections");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("accepting a debug connection failed: {error}"),
            }
        }
    });
    let (mut spawned, mut stderr) = spawn_server(
        port,
        &[],
        &format!("connect:127.0.0.1:{debug_port}"),
        &script,
    );
    let stdout = Lines::read(spawned.child().stdout.take().expect("piped stdout"));
    stderr.after(SERVING);
    drop(ports);
    assert_eq!(get(port).join().unwrap(), "42");
    assert_eq!(get(port).join().unwrap(), "42");
    closer.join().unwrap();
    drop(spawned);
    assert_eq!(stdout.until_end(), ["handled", "handled"]);
    // Only the first connection that closed before configuring the session is reported.
    assert_eq!(connect_failures(&stderr.until_end()), 1);
}

#[test]
fn native_serve_bind_failure_serves_without_debugging() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.js", HANDLER);
    let _ports = lock_ports();
    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let debug_port = occupied.local_addr().unwrap().port();
    let port = free_port();
    let (spawned, mut stderr) =
        spawn_server(port, &[], &format!("listen:{debug_port},wait"), &script);
    stderr.after(SERVING);
    assert_eq!(get(port).join().unwrap(), "42");
    drop(spawned);
    let lines = stderr.until_end();
    let failure = format!("debugger: Binding the debug listener to 127.0.0.1:{debug_port} failed");
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with(&failure) && line.ends_with("Serving without debugging.")),
        "{lines:?}"
    );
}
