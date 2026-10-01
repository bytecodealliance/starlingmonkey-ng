// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

pub mod dap;

use std::ffi::OsStr;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::json;
use serve_test_support::{full_request_within, wasm_serve, PATIENCE};

use dap::Dap;

/// Line 3 runs three times. The script logs `total 3`.
pub const LOOP: &[&str] = &[
    "let total = 0;",
    "for (let i = 0; i < 3; i++) {",
    "  total += i;",
    "}",
    "console.log(`total ${total}`);",
];

/// Line 3 awaits a 50 ms timer inside `work`. The script logs `3`.
pub const AWAIT: &[&str] = &[
    "async function work() {",
    "  const a = 1;",
    "  await new Promise((r) => setTimeout(r, 50));",
    "  const b = 2;",
    "  return a + b;",
    "}",
    "work().then((v) => console.log(v));",
];

/// Line 3 returns inside `add(1, 41)`.
pub const INSPECT: &[&str] = &[
    "function add(a, b) {",
    "  let sum = a + b;",
    "  return sum;",
    "}",
    "const point = { x: 1, y: [2, 3] };",
    "console.log(add(point.x, 41));",
];

/// A `fetch` handler that computes its response on line 2.
pub const HANDLER: &[&str] = &[
    "addEventListener('fetch', (e) => {",
    "  const n = 41 + 1;",
    "  e.respondWith(new Response(String(n)));",
    "});",
];

/// Send `GET /` to the server on `port` from a thread, and return the thread, which yields the
/// response body with any chunked framing decoded.
pub fn get(port: u16) -> JoinHandle<String> {
    get_path(port, "/")
}

/// [`get`] for `path`.
pub fn get_path(port: u16, path: &'static str) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let response = full_request_within(port, "GET", path, "", PATIENCE)
            .unwrap_or_else(|| panic!("no response from port {port} within {PATIENCE:?}"));
        wasm_serve::message_body(&response)
    })
}

/// Stop in the [`HANDLER`] on line 2 while serving a request, and let the request finish with
/// `42`.
pub fn stop_in_handler_and_continue(dap: &mut Dap, port: u16) {
    let response = get(port);
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    assert_eq!(dap.top_frame_line(), 2);
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(response.join().unwrap(), "42");
}

/// Write `lines` to `dir/name`, one per line, so line `n` of the file is `lines[n - 1]`.
pub fn write_script(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

/// Held while a test binds a port on 127.0.0.1 or has a process bind one, and while a process runs
/// that expects nothing to listen on a port the test picked. Every test in the binary that binds
/// ports takes it, so none of them binds a port in the meantime.
static PORTS: Mutex<()> = Mutex::new(());

pub fn lock_ports() -> MutexGuard<'static, ()> {
    PORTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `N` distinct ports on 127.0.0.1 that nothing listens on. Callers hold [`lock_ports`] until
/// they no longer need the ports to stay free.
pub fn free_ports<const N: usize>() -> [u16; N] {
    let listeners: [TcpListener; N] =
        std::array::from_fn(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap());
    listeners.map(|listener| listener.local_addr().unwrap().port())
}

/// A port on 127.0.0.1 that nothing listens on. Callers hold [`lock_ports`] until they no longer
/// need the port to stay free.
pub fn free_port() -> u16 {
    let [port] = free_ports();
    port
}

/// A process that is killed when this is dropped, unless [`Spawned::into_child`] took it.
pub struct Spawned(Option<Child>);

impl Spawned {
    pub fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("the process was handed off")
    }

    pub fn into_child(mut self) -> Child {
        self.0.take().expect("the process was handed off")
    }

    /// Wait for the process to exit, and return its status, stdout and stderr. Both streams must
    /// be piped. Panics if the process does not exit within `timeout`.
    pub fn wait_with_output(&mut self, timeout: Duration) -> (ExitStatus, String, String) {
        let child = self.child();
        let stdout = read_to_end(child.stdout.take().expect("piped stdout"));
        let stderr = read_to_end(child.stderr.take().expect("piped stderr"));
        let status = self.wait_within(timeout);
        (status, stdout.join().unwrap(), stderr.join().unwrap())
    }

    /// Wait for the process to exit, and return its status. Panics if it does not exit within
    /// `timeout`.
    pub fn wait_within(&mut self, timeout: Duration) -> ExitStatus {
        let child = self.child();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the process did not exit within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Read `stream` to its end on a thread.
fn read_to_end(mut stream: impl Read + Send + 'static) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        text
    })
}

/// Start a process with `spawn`, which starts one with a `listen` configuration and stderr piped,
/// and return it with the address it listens on. [`lock_ports`] is held while it starts.
pub fn spawn_listening(spawn: impl FnOnce() -> Spawned) -> (Spawned, SocketAddr) {
    let _ports = lock_ports();
    let mut spawned = spawn();
    let stderr = spawned.child().stderr.take().expect("piped stderr");
    let port = dap::listening_port(stderr);
    (spawned, SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
}

impl Drop for Spawned {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The p3 component under test, or `None` to skip the wasm debugger tests. See
/// [`serve_test_support::wasm_serve::component`].
pub fn wasm_component() -> Option<PathBuf> {
    wasm_serve::component_for(
        "STARLING_WASM_COMPONENT",
        "the wasm debugger tests",
        "just test-debugger-wasm",
    )
}

/// The relay component under test, or `None` to skip the relay tests. See
/// [`serve_test_support::wasm_serve::component`].
pub fn dap_relay() -> Option<PathBuf> {
    wasm_serve::component_for(
        "STARLING_DAP_RELAY",
        "the relay tests",
        "just test-dap-relay",
    )
}

/// A `wasmtime run` command for `component` with `dir` preopened as `/`, and with the network,
/// name lookup and `wasi:http` enabled. Arguments added to it go to the component. The command
/// runs the component compiled ahead of time when compiling it succeeds.
pub fn wasmtime_run(component: &Path, dir: &Path) -> Command {
    wasmtime_run_with(component, dir, &[])
}

/// [`wasmtime_run`] with the additional `wasmtime` options `options`.
pub fn wasmtime_run_with(component: &Path, dir: &Path, options: &[&str]) -> Command {
    let precompiled = wasm_serve::precompiled(component);
    let mut command = Command::new("wasmtime");
    command
        .arg("run")
        .args(wasm_serve::CODEGEN_FLAGS)
        .args(precompiled.is_some().then_some("--allow-precompiled"))
        .arg("-Shttp=y,inherit-network=y,allow-ip-name-lookup=y,p3=y")
        .arg("-Wcomponent-model-async=y")
        .arg("--dir")
        .arg(format!("{}::/", dir.display()))
        .args(options)
        .arg(precompiled.as_deref().unwrap_or(component));
    command
}

/// Start `starlingmonkey <args>` with stdout and stderr piped.
pub fn spawn(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Spawned {
    let mut command = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"));
    command.args(args);
    spawn_piped(command)
}

/// Start `command` with stdin set to `Stdio::null()`, and stdout and stderr piped.
pub fn spawn_piped(mut command: Command) -> Spawned {
    command.stdin(Stdio::null());
    spawn_with_stdin(command)
}

/// Start `command` with stdin, stdout and stderr piped.
pub fn spawn_fully_piped(mut command: Command) -> Spawned {
    command.stdin(Stdio::piped());
    spawn_with_stdin(command)
}

/// Start `command`, which has its stdin configured, with stdout and stderr piped.
fn spawn_with_stdin(mut command: Command) -> Spawned {
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start the process");
    Spawned(Some(child))
}
