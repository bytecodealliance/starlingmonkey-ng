// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end tests of `starlingmonkey --debug=listen` and `--debug=connect`, driven by a DAP
//! client over TCP.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use support::dap::{forward_lines, line_after, Dap, Lines};
use support::{free_port, lock_ports, spawn_listening, write_script, Spawned};

/// Start `starlingmonkey --debug=<debug> <script>` with stdout and stderr piped.
fn spawn(debug: &str, script: &Path) -> Spawned {
    support::spawn([format!("--debug={debug}").as_ref(), script.as_os_str()])
}

/// Line 2 logs and line 3 returns inside `tick`, which runs every 300 ms.
const TICKS: &[&str] = &[
    "function tick(n) {",
    "  console.log(`tick ${n}`);",
    "  return n;",
    "}",
    "let n = 0;",
    "setInterval(() => tick(++n), 300);",
];

#[test]
fn listen_wait_attaches_before_the_script_runs() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('x');"]);
    let (spawned, addr) = spawn_listening(|| spawn("listen:0,wait", &script));
    let mut dap = Dap::attach_tcp(spawned.into_child(), addr);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[1]);
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    dap.request("continue", json!({ "threadId": 1 }));
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "x\n");
    assert!(finished.status.success());
}

#[test]
fn listen_without_wait_runs_until_a_client_attaches() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &[
            "console.log('started');",
            "setInterval(() => {",
            "  console.log('tick');",
            "}, 100);",
        ],
    );
    let (mut spawned, addr) = spawn_listening(|| spawn("listen:0", &script));
    let stdout = spawned.child().stdout.take().expect("piped stdout");
    line_after(stdout, "started");
    let mut dap = Dap::attach_tcp(spawned.into_child(), addr);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[3]);
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    assert_eq!(dap.top_frame_line(), 3);
    dap.request("continue", json!({ "threadId": 1 }));
    let seen = dap.events_until("stopped");
    assert!(
        seen.iter()
            .any(|m| m["event"] == "output" && m["body"]["output"] == "tick\n"),
        "no `tick` output before the next stop: {seen:?}"
    );
    dap.request("disconnect", json!({ "terminateDebuggee": true }));
    dap.event("terminated");
    assert!(dap.wait_for_exit().success());
}

#[test]
fn listen_reattach_uses_only_the_new_clients_breakpoints() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", TICKS);
    let (spawned, addr) = spawn_listening(|| spawn("listen:0,wait", &script));

    let mut first = Dap::attach_tcp(spawned.into_child(), addr);
    first.start(json!({}));
    first.set_breakpoints(&script, &[2]);
    first.request("configurationDone", json!({}));
    assert_eq!(first.event("stopped")["reason"], "breakpoint");
    assert_eq!(first.top_frame_line(), 2);
    first.request("continue", json!({ "threadId": 1 }));
    first.request("disconnect", json!({}));
    let child = first.into_child();

    let mut second = Dap::attach_tcp(child, addr);
    second.start(json!({}));
    second.set_breakpoints(&script, &[3]);
    second.request("configurationDone", json!({}));
    assert_eq!(second.event("stopped")["reason"], "breakpoint");
    assert_eq!(second.top_frame_line(), 3);
    second.request("continue", json!({ "threadId": 1 }));
    second.request("disconnect", json!({ "terminateDebuggee": true }));
    second.event("terminated");
    assert!(second.wait_for_exit().success());
}

#[test]
fn listen_disconnect_request_closes_the_connection_and_lets_the_next_client_attach() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", TICKS);
    let (spawned, addr) = spawn_listening(|| spawn("listen:0,wait", &script));

    let mut first = Dap::attach_tcp(spawned.into_child(), addr);
    first.start(json!({}));
    first.request("configurationDone", json!({}));
    first.request("disconnect", json!({}));
    first.wait_for_close();
    let child = first.take_child();

    let mut second = Dap::attach_tcp(child, addr);
    second.start(json!({}));
    second.set_breakpoints(&script, &[3]);
    second.request("configurationDone", json!({}));
    assert_eq!(second.event("stopped")["reason"], "breakpoint");
    assert_eq!(second.top_frame_line(), 3);
    second.request("continue", json!({ "threadId": 1 }));
    second.request("disconnect", json!({ "terminateDebuggee": true }));
    second.event("terminated");
    assert!(second.wait_for_exit().success());
    drop(first);
}

#[test]
fn listen_detach_while_paused_lets_the_next_client_attach() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", TICKS);
    let (spawned, addr) = spawn_listening(|| spawn("listen:0,wait", &script));

    let mut first = Dap::attach_tcp(spawned.into_child(), addr);
    first.start(json!({}));
    first.set_breakpoints(&script, &[2]);
    first.request("configurationDone", json!({}));
    assert_eq!(first.event("stopped")["reason"], "breakpoint");
    first.request("disconnect", json!({}));
    // The first client keeps its connection open.
    let child = first.take_child();

    let mut second = Dap::attach_tcp(child, addr);
    second.start(json!({}));
    second.set_breakpoints(&script, &[3]);
    second.request("configurationDone", json!({}));
    assert_eq!(second.event("stopped")["reason"], "breakpoint");
    assert_eq!(second.top_frame_line(), 3);
    second.request("continue", json!({ "threadId": 1 }));
    second.request("disconnect", json!({ "terminateDebuggee": true }));
    second.event("terminated");
    assert!(second.wait_for_exit().success());
    drop(first);
}

#[test]
fn connect_attaches_to_a_listening_client() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["const x = 1;", "console.log(`x ${x}`);"],
    );
    let listener = {
        let _ports = lock_ports();
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
    };
    let port = listener.local_addr().unwrap().port();
    let mut spawned = spawn(&format!("connect:127.0.0.1:{port}"), &script);
    forward_lines(spawned.child().stderr.take().expect("piped stderr"));
    let mut dap = Dap::accept_tcp(spawned.into_child(), &listener);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[2]);
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    assert_eq!(dap.top_frame_line(), 2);
    dap.request("continue", json!({ "threadId": 1 }));
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "x 1\n");
    assert_eq!(finished.exit_code, Some(0));
    assert!(finished.status.success());
}

/// Start `starlingmonkey --debug=connect:127.0.0.1:<port> <script>` for a client listening on a
/// port of its own, and return the process and the listener.
fn spawn_connecting(script: &Path) -> (Spawned, TcpListener) {
    let listener = {
        let _ports = lock_ports();
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
    };
    let port = listener.local_addr().unwrap().port();
    (
        spawn(&format!("connect:127.0.0.1:{port}"), script),
        listener,
    )
}

#[test]
fn connect_client_closing_before_configuration_leaves_output_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('ran');"]);
    let (mut spawned, listener) = spawn_connecting(&script);
    drop(Dap::accept(&listener));
    let (status, stdout, stderr) = spawned.wait_with_output(Duration::from_secs(60));
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(stdout, "ran\n", "{stderr}");
    assert!(
        stderr.contains("disconnected before configuring the session, running without debugging"),
        "{stderr}"
    );
}

#[test]
fn listen_wait_client_closing_before_configuration_leaves_output_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('ran');"]);
    let (mut spawned, addr) = spawn_listening(|| spawn("listen:0,wait", &script));
    let stdout = Lines::read(spawned.child().stdout.take().expect("piped stdout"));
    drop(TcpStream::connect(addr).unwrap());
    assert_eq!(stdout.until_end(), ["ran"]);
    assert!(spawned.child().wait().unwrap().success());
}

#[test]
fn connect_client_closing_while_paused_leaves_later_output_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["const x = 1;", "console.log(`x ${x}`);"],
    );
    let (mut spawned, listener) = spawn_connecting(&script);
    let mut dap = Dap::accept(&listener);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[2]);
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    dap.close_stdin();
    let (status, stdout, stderr) = spawned.wait_with_output(Duration::from_secs(60));
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(stdout, "x 1\n", "{stderr}");
}

#[test]
fn connect_without_listener_runs_undebugged() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('ran');"]);
    let _ports = lock_ports();
    let port = free_port();
    let mut spawned = spawn(&format!("connect:127.0.0.1:{port}"), &script);
    let (status, stdout, stderr) = spawned.wait_with_output(Duration::from_secs(10));
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(stdout, "ran\n", "{stderr}");
    assert!(stderr.contains("running without debugging"), "{stderr}");
}

#[test]
fn listen_rejects_a_port_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('ran');"]);
    let taken = {
        let _ports = lock_ports();
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
    };
    let port = taken.local_addr().unwrap().port();
    let output = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .arg(format!("--debug=listen:{port}"))
        .arg(&script)
        .output()
        .expect("failed to run starlingmonkey");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains(&format!(
            "Binding the debug listener to 127.0.0.1:{port} failed"
        )),
        "{stderr}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}
