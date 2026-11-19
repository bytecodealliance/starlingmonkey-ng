// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end test of DAP requests that reach the p3 component while JavaScript runs, with the
//! `starling-debug-interrupter` component as `wasmtime run`'s debugger.
//!
//! Skipped, loudly, unless `STARLING_WASM_COMPONENT` and `STARLING_DEBUG_INTERRUPTER` name the
//! two components and `wasmtime` is on PATH. `just test-debug-interrupter` builds them and runs
//! the test.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;
use support::dap::{Dap, Lines};
use support::{debug_interrupter, lock_ports, spawn_piped, wasm_component, write_script, BUSY};

/// The path the test's script has in the component, which sees the script directory as `/`.
const SCRIPT: &str = "/main.mjs";

/// How long `wasmtime` may take until the interrupter listens. It compiles the component first,
/// which takes about a minute for a debug build.
const COMPILE_PATIENCE: Duration = Duration::from_secs(600);

/// Requests an interrupt from the interrupter at `addr`: a connection is the request.
fn interrupt(addr: SocketAddr) {
    // The interrupter closes the connection, so a failed connection is the only error to see.
    drop(TcpStream::connect(addr).expect("connecting to the interrupter"));
}

/// Requests an interrupt every 50 ms until dropped, as a client does that sees no response. One
/// request is not always enough: the host may not have delivered the client's message to the
/// instance yet when the interrupt callback runs.
struct Interrupting {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Interrupting {
    fn start(addr: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                while !stop.load(Ordering::SeqCst) {
                    interrupt(addr);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        Interrupting {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Interrupting {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start `component` on [`BUSY`] under `wasmtime run` with `interrupter` as its debugger, attach
/// a client to its `listen` session, and configure the session. Returns the client and the
/// interrupter's address.
fn in_busy_loop(component: &Path, interrupter: &Path, dir: &Path) -> (Dap, SocketAddr) {
    write_script(dir, "main.mjs", BUSY);
    let mut command = Command::new("wasmtime");
    command
        .arg("run")
        .arg(format!("-Ddebugger={}", interrupter.display()))
        .arg("-Darg=127.0.0.1:0")
        .arg("-Shttp=y,inherit-network=y,allow-ip-name-lookup=y,p3=y")
        .arg("-Wcomponent-model-async=y")
        .arg("--dir")
        .arg(format!("{}::/", dir.display()))
        .arg(component)
        .arg("--debug=listen:0,wait")
        .arg(SCRIPT)
        .stdout(Stdio::null());
    let _ports = lock_ports();
    let mut spawned = spawn_piped(command);
    let mut stderr = Lines::read(spawned.child().stderr.take().expect("piped stderr"));
    let port = |line: String| line.parse::<u16>().expect("a port number");
    let interrupter = port(stderr.after_within(
        "Debugger: debug interrupter listening on 127.0.0.1:",
        COMPILE_PATIENCE,
    ));
    let session = port(stderr.after("debugger listening on 127.0.0.1:"));
    let mut dap = Dap::attach_tcp(
        spawned.into_child(),
        SocketAddr::from((Ipv4Addr::LOCALHOST, session)),
    );
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    (dap, SocketAddr::from((Ipv4Addr::LOCALHOST, interrupter)))
}

#[test]
fn requests_are_handled_while_a_busy_loop_runs() {
    let (Some(component), Some(interrupter)) = (wasm_component(), debug_interrupter()) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (mut dap, interrupter) = in_busy_loop(&component, &interrupter, dir.path());

    // The script's output is written once an interrupt flushes the connection.
    {
        let _interrupting = Interrupting::start(interrupter);
        dap.wait_for_output("looping\n");
    }

    // A `pause` request is handled once an interrupt has the session take it in.
    let pause = dap.send_request("pause", json!({ "threadId": 1 }));
    {
        let _interrupting = Interrupting::start(interrupter);
        assert_eq!(dap.response(pause)["success"], true);
    }
    assert_eq!(dap.event("stopped")["reason"], "pause");
    let (name, line) = dap.top_frame();
    assert_eq!(name, "(module)");
    assert!((4..=6).contains(&line), "paused on line {line}");
    dap.request("continue", json!({ "threadId": 1 }));

    // A breakpoint set while the loop runs is hit.
    let breakpoints = dap.send_request(
        "setBreakpoints",
        json!({ "source": { "path": SCRIPT }, "breakpoints": [{ "line": 5 }] }),
    );
    let breakpoints = {
        let _interrupting = Interrupting::start(interrupter);
        dap.response(breakpoints)
    };
    assert_eq!(breakpoints["body"]["breakpoints"][0]["verified"], true);
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    assert_eq!(dap.top_frame().1, 5);
    dap.set_breakpoints(Path::new(SCRIPT), &[]);
    dap.request("continue", json!({ "threadId": 1 }));

    // An `evaluate` runs between two iterations, and ends the loop here.
    let evaluate = dap.send_request(
        "evaluate",
        json!({ "expression": "globalThis.run = false", "context": "repl" }),
    );
    {
        let _interrupting = Interrupting::start(interrupter);
        assert_eq!(dap.response(evaluate)["body"]["result"], "false");
    }
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "done\n");
    assert!(finished.status.success(), "{}", finished.status);
}
