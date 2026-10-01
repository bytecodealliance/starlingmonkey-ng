// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end tests of the `starling-dap-relay` component under `wasmtime run`, bridging a native
//! `starlingmonkey --debug=connect` debuggee to a DAP client over stdio or TCP.
//!
//! Skipped, loudly, unless `STARLING_DAP_RELAY` names the relay component and `wasmtime` is on
//! PATH. `just test-dap-relay` builds the relay and runs the suite.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::io::BufReader;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support::dap::{Dap, Lines};
use support::{dap_relay, spawn, spawn_fully_piped, write_script, Spawned};

/// Line 2 logs `x 1`.
const LOG_X: &[&str] = &["const x = 1;", "console.log(`x ${x}`);"];

/// The usage line the relay prints for `--help` and after an invalid argument.
const USAGE: &str = "usage: starling-dap-relay [--instance <addr>:<port>] [--client <port>]";

/// Prefix of the line the relay prints once it listens for instances.
const INSTANCES: &str = "relay listening for instances on 127.0.0.1:";

/// Prefix of the line the relay prints once it listens for the DAP client.
const CLIENT: &str = "relay listening for the client on 127.0.0.1:";

/// Start `relay` under `wasmtime run -Sinherit-network=y` with `args`, and return it with the
/// lines of its stderr. Its stdin, stdout and stderr are piped.
fn spawn_relay(relay: &Path, args: &[&str]) -> (Spawned, Lines) {
    let mut command = Command::new("wasmtime");
    command
        .arg("run")
        .arg("-Sinherit-network=y")
        .arg(relay)
        .args(args);
    let mut spawned = spawn_fully_piped(command);
    let stderr = Lines::read(spawned.child().stderr.take().expect("piped stderr"));
    (spawned, stderr)
}

/// Start `relay` with `--instance 127.0.0.1:0`, and return a DAP client on its stdio, the port it
/// listens on for instances, and the lines of its stderr.
fn relay_on_stdio(relay: &Path) -> (Dap, u16, Lines) {
    let (mut spawned, mut stderr) = spawn_relay(relay, &["--instance", "127.0.0.1:0"]);
    let port = port_after(&mut stderr, INSTANCES);
    let child = spawned.child();
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    (
        Dap::from_streams(spawned.into_child(), stdout, stdin),
        port,
        stderr,
    )
}

/// The port in the next line of `stderr` that starts with `prefix`.
fn port_after(stderr: &mut Lines, prefix: &str) -> u16 {
    stderr.after(prefix).parse().expect("a port number")
}

/// Start `starlingmonkey --debug=connect:127.0.0.1:<port> <script>`.
fn spawn_debuggee(port: u16, script: &Path) -> Spawned {
    spawn([
        format!("--debug=connect:127.0.0.1:{port}").as_ref(),
        script.as_os_str(),
    ])
}

/// Wait for `spawned` to exit, assert that it succeeded, and return its stdout and stderr.
fn succeeded(spawned: &mut Spawned) -> (String, String) {
    let (status, stdout, stderr) = spawned.wait_with_output(Duration::from_secs(60));
    assert!(status.success(), "{status}: {stderr}");
    (stdout, stderr)
}

/// The `terminated` event the relay sends when the instance disconnects.
fn relay_terminated() -> Value {
    json!({ "seq": 1, "type": "event", "event": "terminated" })
}

/// Set a breakpoint on line 2 of `script`, which runs [`LOG_X`], stop there, and continue.
fn stop_on_line_2_and_continue(dap: &mut Dap, script: &Path) {
    dap.start(json!({}));
    dap.set_breakpoints(script, &[2]);
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    assert_eq!(dap.top_frame_line(), 2);
    dap.request("continue", json!({ "threadId": 1 }));
}

/// Collect the text of `output` events until the debuggee's `terminated` event. Then assert that
/// the relay's `terminated` event follows and that the relay exits with status 0.
fn finish(dap: &mut Dap) -> String {
    let output = dap
        .events_until("terminated")
        .iter()
        .filter(|message| message["event"] == "output")
        .map(|message| message["body"]["output"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(dap.events_until("terminated"), [relay_terminated()]);
    assert!(dap.wait_for_exit().success());
    output
}

#[test]
fn relays_a_breakpoint_session_over_stdio() {
    let Some(relay) = dap_relay() else { return };
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", LOG_X);
    let (mut dap, port, _stderr) = relay_on_stdio(&relay);
    let mut debuggee = spawn_debuggee(port, &script);
    stop_on_line_2_and_continue(&mut dap, &script);
    assert_eq!(finish(&mut dap), "x 1\n");
    succeeded(&mut debuggee);
}

#[test]
fn relays_a_breakpoint_session_to_a_client_port() {
    let Some(relay) = dap_relay() else { return };
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", LOG_X);
    let (mut spawned, mut stderr) =
        spawn_relay(&relay, &["--instance", "127.0.0.1:0", "--client", "0"]);
    let instance_port = port_after(&mut stderr, INSTANCES);
    let client_port = port_after(&mut stderr, CLIENT);
    // The relay reads nothing from stdin in this mode, so closing it changes nothing.
    drop(spawned.child().stdin.take());
    let mut dap = Dap::attach_tcp(
        spawned.into_child(),
        SocketAddr::from((Ipv4Addr::LOCALHOST, client_port)),
    );
    let mut debuggee = spawn_debuggee(instance_port, &script);
    stop_on_line_2_and_continue(&mut dap, &script);
    assert_eq!(finish(&mut dap), "x 1\n");
    succeeded(&mut debuggee);
}

#[test]
fn closes_a_second_instance_which_runs_undebugged() {
    let Some(relay) = dap_relay() else { return };
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", LOG_X);
    let second_script = write_script(dir.path(), "second.mjs", &["console.log('second');"]);
    let (mut dap, port, mut relay_stderr) = relay_on_stdio(&relay);
    let mut debuggee = spawn_debuggee(port, &script);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[2]);
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");

    let (stdout, stderr) = succeeded(&mut spawn_debuggee(port, &second_script));
    relay_stderr.after("relay: closed the connection of another instance from 127.0.0.1:");
    assert_eq!(stdout, "second\n", "{stderr}");
    assert!(
        stderr.contains(&format!(
            "the client at 127.0.0.1:{port} disconnected before configuring the session, \
             running without debugging"
        )),
        "{stderr}"
    );

    assert_eq!(dap.top_frame_line(), 2);
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(finish(&mut dap), "x 1\n");
    succeeded(&mut debuggee);
}

#[test]
fn instance_disconnect_sends_terminated_and_exits() {
    let Some(relay) = dap_relay() else { return };
    let (mut dap, port, _stderr) = relay_on_stdio(&relay);
    let mut instance = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();

    let event =
        json!({ "seq": 1, "type": "event", "event": "output", "body": { "output": "hi\n" } });
    debugger::transport::write_message(&mut instance, &event.to_string()).unwrap();
    assert_eq!(dap.event("output")["output"], "hi\n");

    let seq = dap.send_request("threads", json!({}));
    let request = debugger::transport::read_message(&mut BufReader::new(&instance))
        .unwrap()
        .expect("a request");
    let request: Value = serde_json::from_str(&request).unwrap();
    assert_eq!(request["seq"], seq);
    assert_eq!(request["command"], "threads");

    drop(instance);
    assert_eq!(dap.events_until("terminated"), [relay_terminated()]);
    assert!(dap.wait_for_exit().success());
}

#[test]
fn client_output_failure_exits_with_an_error() {
    let Some(relay) = dap_relay() else { return };
    let (mut dap, port, mut stderr) = relay_on_stdio(&relay);
    let mut instance = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    dap.close_stdout();
    let event =
        json!({ "seq": 1, "type": "event", "event": "output", "body": { "output": "hi\n" } });
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        // Errors are expected once the relay exited and closed the connection.
        let _ = debugger::transport::write_message(&mut instance, &event.to_string());
        if let Some(status) = dap.try_exit_status() {
            break status;
        }
        assert!(Instant::now() < deadline, "the relay did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    stderr.after("relay: writing to stdout failed: ");
}

#[test]
fn client_eof_closes_the_instance_connection_and_exits() {
    let Some(relay) = dap_relay() else { return };
    let (mut dap, port, _stderr) = relay_on_stdio(&relay);
    let instance = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    dap.close_stdin();
    assert_eq!(
        debugger::transport::read_message(&mut BufReader::new(&instance)).unwrap(),
        None
    );
    assert!(dap.wait_for_exit().success());
}

#[test]
fn client_port_eof_closes_the_instance_connection_and_exits() {
    let Some(relay) = dap_relay() else { return };
    let (spawned, mut stderr) =
        spawn_relay(&relay, &["--instance", "127.0.0.1:0", "--client", "0"]);
    let instance_port = port_after(&mut stderr, INSTANCES);
    let client_port = port_after(&mut stderr, CLIENT);
    let mut dap = Dap::attach_tcp(
        spawned.into_child(),
        SocketAddr::from((Ipv4Addr::LOCALHOST, client_port)),
    );
    let instance = TcpStream::connect((Ipv4Addr::LOCALHOST, instance_port)).unwrap();
    dap.close_stdin();
    assert_eq!(
        debugger::transport::read_message(&mut BufReader::new(&instance)).unwrap(),
        None
    );
    assert!(dap.wait_for_exit().success());
}

#[test]
fn listens_for_instances_on_127_0_0_1_9230_by_default() {
    let Some(relay) = dap_relay() else { return };
    let (_spawned, mut stderr) = spawn_relay(&relay, &[]);
    // Another process may hold the port, which makes the relay report the failed bind instead.
    let line = stderr.after("relay");
    assert!(line.contains("127.0.0.1:9230"), "{line}");
}

#[test]
fn rejects_invalid_arguments() {
    let Some(relay) = dap_relay() else { return };
    let cases: &[(&[&str], &str)] = &[
        (&["--bogus"], "unknown argument `--bogus`"),
        (&["--client"], "`--client` needs a value"),
        (&["--client", "70000"], "`--client 70000` is not a port"),
        (
            &["--client", "0", "--client", "0"],
            "`--client` is given twice",
        ),
        (
            &["--instance", "localhost:9230"],
            "`--instance localhost:9230` is not <ip>:<port> with an IP address, such as \
             127.0.0.1:9230 or [::1]:9230",
        ),
    ];
    for (args, error) in cases {
        let (mut spawned, stderr) = spawn_relay(&relay, args);
        assert_eq!(
            stderr.until_end(),
            [format!("relay: {error}"), USAGE.to_string()],
            "{args:?}"
        );
        assert!(!spawned.wait_within(Duration::from_secs(60)).success());
    }
}

#[test]
fn help_prints_the_usage() {
    let Some(relay) = dap_relay() else { return };
    for flag in ["--help", "-h"] {
        let (mut spawned, stderr) = spawn_relay(&relay, &[flag]);
        let stdout = Lines::read(spawned.child().stdout.take().expect("piped stdout"));
        assert_eq!(stdout.until_end(), [USAGE]);
        assert_eq!(stderr.until_end(), Vec::<String>::new());
        assert!(spawned.wait_within(Duration::from_secs(60)).success());
    }
}

#[test]
fn listens_for_instances_on_ipv6() {
    let Some(relay) = dap_relay() else { return };
    let (_spawned, mut stderr) = spawn_relay(&relay, &["--instance", "[::1]:0"]);
    let port = port_after(&mut stderr, "relay listening for instances on [::1]:");
    TcpStream::connect((Ipv6Addr::LOCALHOST, port)).unwrap();
    stderr.after("relay: an instance connected from [::1]:");
}
