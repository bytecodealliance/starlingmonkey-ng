// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end tests of the p3 component under `wasmtime run --debug` and `wasmtime serve`, driven
//! by a DAP client over stdio or TCP.
//!
//! Skipped, loudly, unless `STARLING_WASM_COMPONENT` names the component to test and `wasmtime` is
//! on PATH. `just test-debugger-wasm` builds the component and runs the suite.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use serve_test_support::wasm_serve::{Ready, Serve, WasmServer};
use support::dap::{start_debuggee, values, Dap, Lines};
use support::{
    free_port, free_ports, get, lock_ports, spawn_listening, spawn_piped,
    stop_in_handler_and_continue, wasm_component, wasmtime_run, wasmtime_run_with, write_script,
    Spawned, AWAIT, HANDLER, INSPECT, LOOP,
};

/// The path the tests' scripts have in the component, which sees the script directory as `/`.
const SCRIPT: &str = "/main.mjs";

/// Write `lines` to `main.mjs` in a new directory, and return the directory.
fn script_dir(lines: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_script(dir.path(), "main.mjs", lines);
    dir
}

/// Start `component` under `wasmtime run --debug=stdio` on `lines`, configure the session with
/// breakpoints on `breakpoints`, and wait for the first stop.
fn stopped_at(component: &Path, lines: &[&str], breakpoints: &[u32]) -> (Dap, tempfile::TempDir) {
    let dir = script_dir(lines);
    let mut dap = Dap::spawn_wasm(component, dir.path(), &[SCRIPT]);
    dap.start(json!({}));
    dap.set_breakpoints(Path::new(SCRIPT), breakpoints);
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    (dap, dir)
}

/// Start `component` under `wasmtime run --debug=<debug> /main.mjs` with stdout and stderr
/// piped.
fn spawn(component: &Path, dir: &Path, debug: &str) -> Spawned {
    let mut command = wasmtime_run(component, dir);
    command.arg(format!("--debug={debug}")).arg(SCRIPT);
    spawn_piped(command)
}

#[test]
fn wasm_stdio_breakpoint_and_continue() {
    let Some(component) = wasm_component() else {
        return;
    };
    let dir = script_dir(LOOP);
    let mut dap = Dap::spawn_wasm(&component, dir.path(), &[SCRIPT]);
    dap.start(json!({}));
    let breakpoints = dap.set_breakpoints(Path::new(SCRIPT), &[3]);
    assert_eq!(
        breakpoints[0]["verified"], false,
        "the module is not loaded yet"
    );
    dap.request("configurationDone", json!({}));
    let bound = dap.event("breakpoint");
    assert_eq!(bound["breakpoint"]["verified"], true);
    assert_eq!(bound["breakpoint"]["line"], 3);
    for _ in 0..3 {
        let stopped = dap.event("stopped");
        assert_eq!(stopped["reason"], "breakpoint");
        assert_eq!(stopped["threadId"], 1);
        assert_eq!(stopped["hitBreakpointIds"], json!([breakpoints[0]["id"]]));
        dap.request("continue", json!({ "threadId": 1 }));
    }
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "total 3\n");
    assert_eq!(finished.exit_code, Some(0));
    assert!(finished.status.success(), "{}", finished.status);
}

#[test]
fn wasm_stdio_steps_over_await() {
    let Some(component) = wasm_component() else {
        return;
    };
    let (mut dap, _dir) = stopped_at(&component, AWAIT, &[2]);
    dap.request("next", json!({ "threadId": 1 }));
    dap.event("stopped");
    assert_eq!(dap.top_frame(), ("work".to_string(), 3));
    dap.request("next", json!({ "threadId": 1 }));
    dap.event("stopped");
    assert_eq!(dap.top_frame(), ("work".to_string(), 4));
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "3\n");
}

#[test]
fn wasm_stdio_evaluate_and_variables() {
    let Some(component) = wasm_component() else {
        return;
    };
    let (mut dap, _dir) = stopped_at(&component, INSPECT, &[3]);
    let sum = dap.request(
        "evaluate",
        json!({ "expression": "sum", "context": "repl" }),
    );
    assert_eq!(sum["result"], "42");
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    let frame_id = trace["stackFrames"][0]["id"].clone();
    let scopes = dap.request("scopes", json!({ "frameId": frame_id }));
    assert_eq!(scopes["scopes"][0]["name"], "Local");
    let reference = scopes["scopes"][0]["variablesReference"].clone();
    let locals = values(&dap.request("variables", json!({ "variablesReference": reference })));
    for expected in [("a", "1"), ("b", "41")] {
        assert!(
            locals.contains(&(expected.0.to_string(), expected.1.to_string())),
            "{locals:?}"
        );
    }
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "42\n");
}

#[test]
fn wasm_evaluate_fetch_while_paused_returns() {
    let Some(component) = wasm_component() else {
        return;
    };
    let (mut dap, _dir) = stopped_at(&component, &["console.log('x');"], &[1]);
    let result = dap.request(
        "evaluate",
        json!({
            "expression": "fetch('http://127.0.0.1:9/').then(() => 0, () => 0), 1",
            "context": "repl",
        }),
    );
    assert_eq!(result["result"], "1");
    dap.request("continue", json!({ "threadId": 1 }));
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "x\n");
    assert_eq!(finished.exit_code, Some(0));
    assert!(finished.status.success(), "{}", finished.status);
}

#[test]
fn wasm_jobs_an_evaluate_queues_while_paused_run_after_resume() {
    let Some(component) = wasm_component() else {
        return;
    };
    let (mut dap, _dir) = stopped_at(&component, &["console.log('after');"], &[1]);
    dap.evaluate("Promise.resolve().then(() => console.log('job'))");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "after\njob\n");
}

#[test]
fn wasm_jobs_an_evaluate_queues_while_paused_in_a_job_run_after_resume() {
    let Some(component) = wasm_component() else {
        return;
    };
    let script = [
        "Promise.resolve().then(() => { debugger; });",
        "setTimeout(() => console.log('timer'), 0);",
    ];
    let (mut dap, _dir) = stopped_at(&component, &script, &[]);
    dap.evaluate("Promise.resolve().then(() => console.log('job'))");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "job\ntimer\n");
}

#[test]
fn wasm_listen_wait_attaches() {
    let Some(component) = wasm_component() else {
        return;
    };
    let dir = script_dir(&["console.log('x');"]);
    let (spawned, addr) = spawn_listening(|| spawn(&component, dir.path(), "listen:0,wait"));
    let mut dap = Dap::attach_tcp(spawned.into_child(), addr);
    dap.start(json!({}));
    dap.set_breakpoints(Path::new(SCRIPT), &[1]);
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    dap.request("continue", json!({ "threadId": 1 }));
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "x\n");
    assert!(finished.status.success(), "{}", finished.status);
}

#[test]
fn wasm_connect_without_listener_runs_undebugged() {
    let Some(component) = wasm_component() else {
        return;
    };
    let dir = script_dir(&["console.log('ran');"]);
    let _ports = lock_ports();
    let port = free_port();
    let mut spawned = spawn(&component, dir.path(), &format!("connect:127.0.0.1:{port}"));
    let (status, stdout, stderr) = spawned.wait_with_output(Duration::from_secs(60));
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(stdout, "ran\n", "{stderr}");
    assert!(stderr.contains("running without debugging"), "{stderr}");
}

#[test]
fn wasm_two_frames_in_one_write_while_paused() {
    let Some(component) = wasm_component() else {
        return;
    };
    let (mut dap, _dir) = stopped_at(
        &component,
        &["const x = 1;", "setTimeout(() => console.log(x), 500);"],
        &[2],
    );
    let seqs = dap.send_requests(&[
        ("continue", json!({ "threadId": 1 })),
        ("threads", json!({})),
    ]);
    let continued = dap.response(seqs[0]);
    assert_eq!(continued["success"], true, "{continued}");
    let threads = dap.response(seqs[1]);
    assert_eq!(threads["success"], true, "{threads}");
    assert_eq!(threads["body"]["threads"][0]["id"], 1);
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "1\n");
    assert!(finished.status.success(), "{}", finished.status);
}

#[test]
fn wasm_pause_after_a_running_mode_read_is_in_flight() {
    let Some(component) = wasm_component() else {
        return;
    };
    let dir = script_dir(&[
        "setTimeout(() => {",
        "  debugger;",
        "  setTimeout(() => console.log('done'), 1000);",
        "}, 1000);",
    ]);
    let mut dap = Dap::spawn_wasm(&component, dir.path(), &[SCRIPT]);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    // The running-mode future dispatches this request and starts the next read, which is still
    // in flight when the timer pauses.
    let threads = dap.request("threads", json!({}));
    assert_eq!(threads["threads"][0]["id"], 1);
    assert!(
        !dap.queued().any(|m| m["event"] == "stopped"),
        "the timer paused before the request was dispatched"
    );
    let stopped = dap.event("stopped");
    assert_eq!(stopped["reason"], "debugger statement");
    assert_eq!(dap.top_frame().1, 2);
    dap.request("continue", json!({ "threadId": 1 }));
    let threads = dap.request("threads", json!({}));
    assert_eq!(threads["threads"][0]["id"], 1);
    dap.request("disconnect", json!({ "terminateDebuggee": true }));
    let seen = dap.events_until("terminated");
    assert!(
        !seen.iter().any(|m| m["event"] == "stopped"),
        "the debuggee paused again: {seen:?}"
    );
    assert!(dap.wait_for_exit().success());
}

#[test]
fn wasm_large_output_while_running_and_paused() {
    let Some(component) = wasm_component() else {
        return;
    };
    let (mut dap, _dir) = stopped_at(
        &component,
        &[
            "const big = 'x'.repeat(262144);",
            "console.log(big);",
            "setTimeout(() => console.log(big), 100);",
        ],
        &[3],
    );
    dap.request("continue", json!({ "threadId": 1 }));
    let finished = dap.run_to_end();
    let line = format!("{}\n", "x".repeat(262144));
    assert_eq!(finished.output.len(), 2 * line.len());
    assert!(finished.output == line.repeat(2), "the output differs");
    assert_eq!(finished.exit_code, Some(0));
    assert!(finished.status.success(), "{}", finished.status);
}

#[test]
fn wasm_terminate_reports_exit_and_closes() {
    let Some(component) = wasm_component() else {
        return;
    };
    let dir = script_dir(&["setTimeout(() => {}, 60000);"]);

    let mut dap = Dap::spawn_wasm(&component, dir.path(), &[SCRIPT]);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    dap.request("disconnect", json!({ "terminateDebuggee": true }));
    assert_eq!(dap.event("exited")["exitCode"], 0);
    dap.event("terminated");
    assert!(dap.wait_for_exit().success());

    let (spawned, addr) = spawn_listening(|| spawn(&component, dir.path(), "listen:0,wait"));
    let mut dap = Dap::attach_tcp(spawned.into_child(), addr);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    dap.request("disconnect", json!({ "terminateDebuggee": true }));
    assert_eq!(dap.event("exited")["exitCode"], 0);
    dap.event("terminated");
    dap.wait_for_close();
    assert!(dap.wait_for_exit().success());
}

/// A reader that keeps a copy of every byte read through it.
struct Recorded<R> {
    inner: R,
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl<R: Read> Read for Recorded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.bytes.lock().unwrap().extend_from_slice(&buf[..read]);
        Ok(read)
    }
}

/// Assert that `bytes` consists of DAP frames only, each a `Content-Length` header and a JSON
/// body, and return how many there are.
fn assert_only_frames(mut bytes: &[u8]) -> usize {
    let mut frames = 0;
    while !bytes.is_empty() {
        let context = || String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).into_owned();
        let header_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("no frame header in {:?}", context()));
        let length: usize = std::str::from_utf8(&bytes[..header_end])
            .ok()
            .and_then(|header| header.strip_prefix("Content-Length: "))
            .and_then(|length| length.parse().ok())
            .unwrap_or_else(|| panic!("not a frame header: {:?}", context()));
        let body_end = header_end + 4 + length;
        assert!(body_end <= bytes.len(), "truncated frame: {:?}", context());
        serde_json::from_slice::<Value>(&bytes[header_end + 4..body_end])
            .unwrap_or_else(|error| panic!("invalid JSON ({error}): {:?}", context()));
        bytes = &bytes[body_end..];
        frames += 1;
    }
    frames
}

#[test]
fn wasm_stdio_has_no_stray_output() {
    let Some(component) = wasm_component() else {
        return;
    };
    let dir = script_dir(&[
        "console.log('logged');",
        "console.warn('warned');",
        "setTimeout(() => { throw new Error('thrown'); }, 10);",
        "Promise.reject(new Error('rejected'));",
    ]);
    let mut child = start_debuggee(
        wasmtime_run(&component, dir.path()),
        &[SCRIPT],
        Stdio::inherit(),
    );
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let reader = Recorded {
        inner: stdout,
        bytes: Arc::clone(&bytes),
    };
    let mut dap = Dap::from_streams(child, reader, stdin);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    let seen = dap.events_until("terminated");
    let output: String = seen
        .iter()
        .filter(|m| m["event"] == "output")
        .filter_map(|m| m["body"]["output"].as_str())
        .collect();
    assert_eq!(
        output,
        "logged\nWarn: warned\nUncaught (in promise) rejected\n@/main.mjs:4:16\n\
         Uncaught thrown\n@/main.mjs:3:26\n",
        "{seen:?}"
    );
    dap.close_stdin();
    assert!(dap.wait_for_exit().success());
    dap.wait_for_close();
    let bytes = bytes.lock().unwrap();
    let frames = assert_only_frames(&bytes);
    // Outgoing `seq` numbers count up from 1, so the last one is the number of messages sent.
    // `seen` leaves out the messages `start` and `request` took.
    assert_eq!(
        Some(frames as u64),
        seen.last().and_then(|m| m["seq"].as_u64())
    );
}

#[test]
fn wasm_breakpoint_binds_in_an_entry_script_given_by_a_relative_path() {
    let Some(component) = wasm_component() else {
        return;
    };
    let dir = script_dir(&["const a = 1;", "console.log(a);"]);
    let mut dap = Dap::spawn_wasm(&component, dir.path(), &["main.mjs"]);
    dap.start(json!({}));
    dap.set_breakpoints(Path::new(SCRIPT), &[2]);
    dap.request("configurationDone", json!({}));
    let bound = dap.event("breakpoint");
    assert_eq!(bound["breakpoint"]["verified"], true);
    dap.event("stopped");
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    let source = &trace["stackFrames"][0]["source"];
    assert_eq!(source["path"], SCRIPT, "{source}");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "1\n");
}

#[test]
fn wasm_uncaught_errors_are_output_events() {
    let Some(component) = wasm_component() else {
        return;
    };
    let dir = script_dir(&[
        "setTimeout(() => { throw new Error('thrown'); }, 10);",
        "Promise.reject(new Error('rejected'));",
    ]);
    let mut child = start_debuggee(
        wasmtime_run(&component, dir.path()),
        &[SCRIPT],
        Stdio::piped(),
    );
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = Lines::read(child.stderr.take().expect("piped stderr"));
    let mut dap = Dap::from_streams(child, stdout, stdin);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    let seen = dap.events_until("terminated");
    let reported: Vec<&str> = seen
        .iter()
        .filter(|m| m["event"] == "output" && m["body"]["category"] == "stderr")
        .filter_map(|m| m["body"]["output"].as_str())
        .collect();
    assert_eq!(
        reported,
        [
            "Uncaught (in promise) rejected\n@/main.mjs:2:16\n",
            "Uncaught thrown\n@/main.mjs:1:26\n",
        ],
        "{seen:?}"
    );
    dap.close_stdin();
    assert!(dap.wait_for_exit().success());
    let stderr = stderr.until_end();
    assert!(
        !stderr.iter().any(|line| line.starts_with("Uncaught")),
        "{stderr:?}"
    );
}

/// `lines` joined into a script, one per line.
fn lines(lines: &[&str]) -> String {
    lines.join("\n") + "\n"
}

/// The path `wasmtime serve` gives the served script in the component: the harness preopens the
/// server's work directory as `/cwd`.
const SERVED: &str = "/cwd/handler.js";

/// A server for `script` under `wasmtime serve` with `--debug=listen:<port>,wait`, the client of
/// its debug session, and the request whose handling started the session.
struct Served {
    server: WasmServer,
    dap: Dap,
    /// Yields the body of the response to the first request.
    first: JoinHandle<String>,
}

/// Serve `script` with `--debug=listen:<port>,wait`, send `GET /`, and attach to the debug listener
/// that request opens. The session is initialized with breakpoints on `breakpoints`, and
/// `configurationDone` is not sent yet. Returns `None` to skip.
fn serve_and_attach(script: &[&str], breakpoints: &[u32]) -> Option<Served> {
    serve_with_debugger(script, breakpoints, |serve, debug| {
        serve.flags([format!("--debug={debug}").as_str()])
    })
}

/// [`serve_and_attach`], with the server configured by `configure`, which receives the server and
/// the `listen:<port>,wait` transport to debug over.
fn serve_with_debugger(
    script: &[&str],
    breakpoints: &[u32],
    configure: impl FnOnce(Serve, &str) -> Serve,
) -> Option<Served> {
    wasm_component()?;
    let ports = lock_ports();
    let [port, debug_port] = free_ports();
    let server = configure(Serve::new(port), &format!("listen:{debug_port},wait"))
        .script(&lines(script))
        .ready(Ready::Listening)
        .start()?;
    // The request blocks until the client has configured the session.
    let first = get(port);
    let mut dap = Dap::connect_tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, debug_port)));
    drop(ports);
    dap.start(json!({}));
    dap.set_breakpoints(Path::new(SERVED), breakpoints);
    Some(Served { server, dap, first })
}

#[test]
fn wasm_serve_pauses_in_the_fetch_handler() {
    let Some(Served {
        server,
        mut dap,
        first,
    }) = serve_and_attach(HANDLER, &[2])
    else {
        return;
    };
    dap.request("configurationDone", json!({}));
    assert_eq!(
        dap.event("stopped")["reason"],
        "breakpoint",
        "{}",
        server.log()
    );
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    let frame = &trace["stackFrames"][0];
    assert_eq!(frame["line"], 2, "{frame}");
    assert_eq!(frame["source"]["path"], SERVED, "{frame}");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(first.join().unwrap(), "42");
}

#[test]
fn wasm_serve_pauses_while_startup_loop_task_is_queued() {
    let mut script = vec!["setInterval(() => {}, 1000);"];
    script.extend_from_slice(HANDLER);
    let Some(Served {
        server,
        mut dap,
        first,
    }) = serve_and_attach(&script, &[3])
    else {
        return;
    };
    // The first request's `ensure_started` spawns the task that keeps the interval running, and
    // the handler pauses in the same poll.
    dap.request("configurationDone", json!({}));
    assert_eq!(
        dap.event("stopped")["reason"],
        "breakpoint",
        "{}",
        server.log()
    );
    assert_eq!(dap.top_frame_line(), 3);
    dap.request("continue", json!({ "threadId": 1 }));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !first.is_finished() {
        assert!(
            Instant::now() < deadline,
            "no response within 10 s of continuing\n{}",
            server.log()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(first.join().unwrap(), "42");
}

#[test]
fn wasm_serve_second_request_hits_the_breakpoint_again() {
    let Some(Served {
        server,
        mut dap,
        first,
    }) = serve_and_attach(HANDLER, &[2])
    else {
        return;
    };
    dap.request("configurationDone", json!({}));
    assert_eq!(
        dap.event("stopped")["reason"],
        "breakpoint",
        "{}",
        server.log()
    );
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(first.join().unwrap(), "42");
    stop_in_handler_and_continue(&mut dap, server.port());
}

#[test]
fn wasm_serve_evaluate_while_idle_can_use_timers() {
    let Some(Served {
        server: _server,
        mut dap,
        first,
    }) = serve_and_attach(HANDLER, &[])
    else {
        return;
    };
    dap.request("configurationDone", json!({}));
    assert_eq!(first.join().unwrap(), "42");
    let evaluated = dap.evaluate("setTimeout(() => console.log('from eval'), 0); 1");
    assert_eq!(evaluated["result"], "1");
    dap.wait_for_output("from eval\n");
}

#[test]
fn wasm_serve_runs_a_timer_evaluated_before_configuration() {
    let Some(Served {
        server: _server,
        mut dap,
        first,
    }) = serve_and_attach(HANDLER, &[])
    else {
        return;
    };
    let evaluated = dap.evaluate("setTimeout(() => console.log('timer'), 0); 1");
    assert_eq!(evaluated["result"], "1");
    dap.request("configurationDone", json!({}));
    assert_eq!(first.join().unwrap(), "42");
    dap.wait_for_output("timer\n");
}

#[test]
fn wasm_serve_resumed_snapshot_starts_from_starling_debug() {
    let Some(Served {
        server,
        mut dap,
        first,
    }) = serve_with_debugger(HANDLER, &[2], |serve, debug| {
        serve
            .flags(["--debug=stdio"])
            .wizen()
            .env("STARLING_DEBUG", debug)
    })
    else {
        return;
    };
    dap.request("configurationDone", json!({}));
    assert_eq!(
        dap.event("stopped")["reason"],
        "breakpoint",
        "{}",
        server.log()
    );
    assert_eq!(dap.top_frame_line(), 2);
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(first.join().unwrap(), "42");
}

#[test]
fn wasm_serve_snapshot_without_debug_ignores_starling_debug() {
    if wasm_component().is_none() {
        return;
    }
    let _ports = lock_ports();
    let [port, debug_port] = free_ports();
    let Some(server) = Serve::new(port)
        .script(&lines(HANDLER))
        .wizen()
        .env("STARLING_DEBUG", &format!("listen:{debug_port},wait"))
        .ready(Ready::Listening)
        .start()
    else {
        return;
    };
    assert_eq!(server.get("/"), "42");
    assert!(
        std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, debug_port)).is_err(),
        "something listens on the debug port\n{}",
        server.log()
    );
}

#[test]
fn wasm_serve_second_instance_serves_without_debugging() {
    if wasm_component().is_none() {
        return;
    }
    let _ports = lock_ports();
    let [first_port, second_port, debug_port] = free_ports();
    let Some(first) = Serve::new(first_port)
        .script(&lines(HANDLER))
        .flags([format!("--debug=listen:{debug_port}").as_str()])
        .ready(Ready::Listening)
        .start()
    else {
        return;
    };
    assert_eq!(first.get("/"), "42");
    assert!(first
        .log()
        .contains(&format!("debugger listening on 127.0.0.1:{debug_port}")));
    let Some(second) = Serve::new(second_port)
        .script(&lines(HANDLER))
        .flags([format!("--debug=listen:{debug_port},wait").as_str()])
        .ready(Ready::Listening)
        .start()
    else {
        return;
    };
    assert_eq!(second.get("/"), "42");
    assert_eq!(second.get("/"), "42");
    let log = second.log();
    let failure = format!("debugger: Binding the debug listener to 127.0.0.1:{debug_port} failed");
    assert_eq!(log.matches(&failure).count(), 1, "{log}");
    assert!(log.contains("Serving without debugging."), "{log}");
}

#[test]
fn wasm_serve_startup_failure_ends_the_session() {
    let Some(Served {
        server,
        mut dap,
        first,
    }) = serve_and_attach(&["throw new Error('broken');"], &[])
    else {
        return;
    };
    dap.request("configurationDone", json!({}));
    let seen = dap.events_until("terminated");
    let errors: String = seen
        .iter()
        .filter(|m| m["event"] == "output" && m["body"]["category"] == "stderr")
        .filter_map(|m| m["body"]["output"].as_str())
        .collect();
    assert!(errors.contains("broken"), "{seen:?}");
    let exited = seen.iter().find(|m| m["event"] == "exited");
    assert_eq!(
        exited.map(|m| &m["body"]["exitCode"]),
        Some(&json!(1)),
        "{seen:?}"
    );
    dap.wait_for_close();
    assert_eq!(first.join().unwrap(), "Internal Server Error");
    assert_eq!(server.get("/"), "Internal Server Error");
}

#[test]
fn wasm_serve_connect_reconnects_per_request() {
    if wasm_component().is_none() {
        return;
    }
    let ports = lock_ports();
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let debug_port = listener.local_addr().unwrap().port();
    let port = free_port();
    let Some(server) = Serve::new(port)
        .script(&lines(HANDLER))
        .flags([format!("--debug=connect:127.0.0.1:{debug_port}").as_str()])
        .ready(Ready::Listening)
        .start()
    else {
        return;
    };
    drop(ports);

    // The first request connects in the before-content hook, and waits until the client is
    // configured.
    let response = get(port);
    let mut first = Dap::accept(&listener);
    first.start(json!({}));
    first.request("configurationDone", json!({}));
    assert_eq!(response.join().unwrap(), "42");
    first.request("disconnect", json!({}));
    first.wait_for_close();

    // The next request connects again.
    let response = get(port);
    let mut second = Dap::accept(&listener);
    second.start(json!({}));
    second.set_breakpoints(Path::new(SERVED), &[2]);
    second.request("configurationDone", json!({}));
    assert_eq!(
        second.event("stopped")["reason"],
        "breakpoint",
        "{}",
        server.log()
    );
    assert_eq!(second.top_frame_line(), 2);
    second.request("continue", json!({ "threadId": 1 }));
    assert_eq!(response.join().unwrap(), "42");
}

#[test]
fn wasm_serve_listen_without_wait_attaches_while_serving() {
    if wasm_component().is_none() {
        return;
    }
    let ports = lock_ports();
    let [port, debug_port] = free_ports();
    let Some(server) = Serve::new(port)
        .script(&lines(HANDLER))
        .flags([format!("--debug=listen:{debug_port}").as_str()])
        .ready(Ready::Listening)
        .start()
    else {
        return;
    };
    // Served without waiting for a client.
    assert_eq!(server.get("/"), "42");
    let mut dap = Dap::connect_tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, debug_port)));
    drop(ports);
    dap.start(json!({}));
    let breakpoints = dap.set_breakpoints(Path::new(SERVED), &[2]);
    assert_eq!(breakpoints[0]["verified"], true, "{breakpoints:?}");
    dap.request("configurationDone", json!({}));
    stop_in_handler_and_continue(&mut dap, port);
}

#[test]
fn wasm_run_resumed_snapshot_debugs_its_run_export() {
    let Some(component) = wasm_component() else {
        return;
    };
    let dir = script_dir(&[
        "export async function run() {",
        "  const n = 41 + 1;",
        "  await new Promise((resolve) => setTimeout(resolve, 1000));",
        "  console.log(n);",
        "}",
    ]);
    let snapshot = dir.path().join("snapshot.wasm");
    serve_test_support::wasm_serve::snapshot_with_wizer(
        dir.path(),
        &component,
        &snapshot,
        "--debug=stdio main.mjs",
        false,
    );
    let command = wasmtime_run_with(&snapshot, dir.path(), &["--env", "STARLING_DEBUG=stdio"]);
    let mut child = start_debuggee(command, &[], Stdio::inherit());
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut dap = Dap::from_streams(child, stdout, stdin);
    dap.start(json!({}));
    dap.set_breakpoints(Path::new("/cwd/main.mjs"), &[2]);
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    assert_eq!(dap.top_frame(), ("run".to_string(), 2));
    dap.request("continue", json!({ "threadId": 1 }));
    // Answered while `run` awaits the timer.
    let threads = dap.request("threads", json!({}));
    assert_eq!(threads["threads"][0]["id"], 1);
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "42\n");
    assert_eq!(finished.exit_code, Some(0));
    assert!(finished.status.success(), "{}", finished.status);
}
