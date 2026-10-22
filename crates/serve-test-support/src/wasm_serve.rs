// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Standing the wasm serve path up for an end-to-end test: a `wasmtime serve` child running the
//! component, the content script and configuration it serves, and the log the guest's `stderr`
//! lands in.
//!
//! The guest writes to that log through `console.error`/`eprintln!`, which is the only channel
//! left for what happens after a response is gone — `waitUntil` work, request-signal aborts, the
//! server's own diagnostics — so a test asserts on it with [`WasmServer::wait_for_marker`].

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The WASI capabilities the component needs: `cli` for `stderr`, the environment its configuration
/// arrives in, and the network for both directions of `wasi:http`.
const WASI_FLAGS: &str = "-Scli=y,inherit-env=y,inherit-network=y,http=y";

/// The prefix `core_runtime::event_loop::trace_idle` writes before its tag. Duplicated from
/// `core_runtime::event_loop::IDLE_TRACE` to keep this crate dependency-free; `libstarling`'s
/// test support checks the copies against each other at compile time.
pub const IDLE_TRACE: &str = "starling: event loop idle ";

/// Codegen options for every `wasmtime` invocation here.
///
/// Native unwind information is off because deregistering it dominates every server this suite
/// starts: macOS's `__deregister_frame` walks the registered frames linearly, which is quadratic
/// over a component this size. The cost is that native profilers can no longer unwind through wasm
/// frames. JS exceptions and wasm traps have their own stacks, so a failure's serve log is
/// unchanged.
const CODEGEN_FLAGS: [&str; 2] = ["-C", "native-unwind-info=n"];

/// How long a request may take before the harness calls the server hung. Generous: every case here
/// responds in milliseconds unless it is testing a bound of its own.
const PATIENCE: Duration = Duration::from_secs(20);

/// How long a server gets to respond to its first request. More than [`PATIENCE`], since it has an
/// instance to stand up. It does not cover compiling the component. See [`precompiled`].
const READY_PATIENCE: Duration = Duration::from_secs(60);

/// A `wasmtime serve` child running the component, killed and reaped on drop.
pub struct WasmServer {
    port: u16,
    child: Child,
    dir: PathBuf,
}

/// How the harness tells that a server is up.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    /// `GET /ready` responds with `200`, the convention the handler scripts here follow. Waits out
    /// startup — a script with a top-level `await` is not ready until the first request drives it.
    Route,
    /// The port responds with anything at all, for the cases whose point is that the server cannot
    /// serve: a broken configuration, a script with no `fetch` listener.
    AnyResponse,
    /// The port accepts a connection; nothing is asked of the guest. For the cases about what the
    /// first request meets. A probe would have driven startup and taken that away.
    Listening,
}

/// A server to start: what to serve, how, and how to tell it is up. Every field has a default that
/// suits the common case (a legacy script, no extra configuration, a `/ready` route), so a test
/// names only what it cares about.
pub struct Serve {
    files: Vec<(String, String)>,
    flags: Vec<String>,
    script: Vec<String>,
    wasmtime_flags: Vec<String>,
    env: Vec<(String, String)>,
    ready: Ready,
    wizer_passes: usize,
    no_zeal: bool,
}

impl Default for Serve {
    fn default() -> Self {
        Self::new()
    }
}

impl Serve {
    /// A server on a port the system picks. The port is read back from `wasmtime serve`'s output
    /// and reported by [`WasmServer::port`].
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            flags: Vec::new(),
            script: Vec::new(),
            wasmtime_flags: Vec::new(),
            env: Vec::new(),
            ready: Ready::Route,
            wizer_passes: 0,
            no_zeal: false,
        }
    }

    /// Keep the host on one instance for the whole test. `--idle-instance-timeout` defaults to a
    /// second, so an instance that idles longer than that between two requests is dropped and the
    /// second request starts a fresh script — indistinguishable, from the test's side, from state
    /// that failed to carry over.
    pub fn reusing_one_instance(self) -> Self {
        self.wasmtime_flags(["--idle-instance-timeout", "30s"])
    }

    /// Serve `source` as the content script, in the classic-script shape most handlers here take.
    pub fn script(mut self, source: &str) -> Self {
        self.files.push(("handler.js".into(), source.into()));
        self.script = vec!["--legacy-script".into(), "handler.js".into()];
        self
    }

    /// Serve `source` as an ES module named `name`, for the cases that need a top-level `await` or
    /// an import.
    pub fn module(mut self, name: &str, source: &str) -> Self {
        self.files.push((name.into(), source.into()));
        self.script = vec![name.into()];
        self
    }

    /// Write another file into the work directory, for a content script that imports or reads it.
    pub fn file(mut self, name: &str, source: &str) -> Self {
        self.files.push((name.into(), source.into()));
        self
    }

    /// Extra `STARLINGMONKEY_CONFIG` flags. The content script is always last, whatever order the
    /// builder is called in.
    pub fn flags<'a>(mut self, flags: impl IntoIterator<Item = &'a str>) -> Self {
        self.flags.extend(flags.into_iter().map(str::to_string));
        self
    }

    /// Extra `wasmtime serve` flags, for the cases that pin down the host's instance handling.
    pub fn wasmtime_flags<'a>(mut self, flags: impl IntoIterator<Item = &'a str>) -> Self {
        self.wasmtime_flags
            .extend(flags.into_iter().map(str::to_string));
        self
    }

    /// Run the guest's SpiderMonkey under a GC zeal mode (`mode,frequency`, as
    /// `scripts/test-gc-zeal.sh` documents them). Reaches the engine through the environment,
    /// which `-Sinherit-env=y` passes on; only a `debugmozjs` build acts on it.
    pub fn gc_zeal(mut self, mode: &str) -> Self {
        self.env.push(("JS_GC_ZEAL".into(), mode.into()));
        self
    }

    /// Run the guest's SpiderMonkey under its ordinary GC, whatever `scripts/test-gc-zeal.sh` set
    /// for the host's own tests. `-Sinherit-env=y` passes that setting on, and a case whose cost or
    /// validity depends on the collector picks its own mode instead.
    pub fn without_gc_zeal(mut self) -> Self {
        self.no_zeal = true;
        self
    }

    pub fn ready(mut self, ready: Ready) -> Self {
        self.ready = ready;
        self
    }

    /// Snapshot the configured component with `wasmtime wizer` and serve the snapshot: the shape a
    /// deployed server has, with the engine stood up and the script evaluated at build time. Each
    /// further call snapshots the previous snapshot again, which runs its init export once more.
    pub fn wizen(mut self) -> Self {
        self.wizer_passes += 1;
        self
    }

    /// Start the server, or return `None` when the wasm component or `wasmtime` is missing.
    pub fn start(self) -> Option<WasmServer> {
        let component = component()?;
        let dir = work_dir();
        for (name, source) in &self.files {
            std::fs::write(dir.join(name), source).unwrap();
        }
        let config = self
            .flags
            .iter()
            .chain(&self.script)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");

        // Compiled here rather than by the server below, so every server after the first loads a
        // component that is already compiled.
        let mut component = component;
        for pass in 1..=self.wizer_passes {
            let snapshot = dir.join(format!("snapshot-{pass}.wasm"));
            snapshot_with_wizer(&dir, &component, &snapshot, &config, self.no_zeal);
            component = snapshot;
        }
        let (component, ahead_of_time) = match precompiled(&component) {
            Some(cwasm) => (cwasm, true),
            None => (component, false),
        };

        let log = std::fs::File::create(dir.join("serve.log")).unwrap();
        let mut command = Command::new("wasmtime");
        command
            .current_dir(&dir)
            .args(["serve"])
            .args(CODEGEN_FLAGS)
            .args(ahead_of_time.then_some("--allow-precompiled"))
            .args(["--dir=.::/cwd", "--dir=.", WASI_FLAGS])
            .args(&self.wasmtime_flags)
            .args(["--addr", "127.0.0.1:0"])
            .arg(&component)
            .env("STARLINGMONKEY_CONFIG", &config)
            // Read by `WasmServer::await_new_idle`.
            .env("STARLING_TRACE_IDLE", "1")
            // `wasmtime serve` logs which instance took each request at info level, which
            // `instances_since` reads. Scoped to that target, since `info` elsewhere is far
            // larger.
            .env("WASMTIME_LOG", "wasmtime_cli=info")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        if self.no_zeal {
            command.env_remove("JS_GC_ZEAL");
        }
        // After the removal, so a mode the case asked for outranks it.
        let child = command.envs(self.env.iter().cloned()).spawn().unwrap();

        let mut server = WasmServer {
            port: 0,
            child,
            dir,
        };
        server.port = server.await_bound_port();
        server.await_ready(self.ready);
        Some(server)
    }
}

impl WasmServer {
    /// Poll the serve log for the address `wasmtime serve` reports once it has bound its
    /// listener, and return that address's port. Panics with the log if the child exits first.
    fn await_bound_port(&mut self) -> u16 {
        const BOUND: &str = "Serving HTTP on http://";
        let deadline = Instant::now() + READY_PATIENCE;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "wasmtime serve exited with {status} before listening\n{}",
                    self.log()
                );
            }
            let log = self.log();
            let port = log
                .split_once(BOUND)
                .and_then(|(_, rest)| rest.split_once('/'))
                .and_then(|(address, _)| address.rsplit_once(':'))
                .and_then(|(_, port)| port.parse().ok());
            if let Some(port) = port {
                return port;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("wasmtime serve never reported its address\n{}", self.log());
    }

    /// Poll until the server responds, or panic with the serve log. A child that died on a
    /// rejected flag or a runtime that refused its configuration is otherwise a silent hang.
    fn await_ready(&mut self, ready: Ready) {
        let deadline = Instant::now() + READY_PATIENCE;
        let mut last = None;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "wasmtime serve exited with {status} before answering\n{}",
                    self.log()
                );
            }
            // The server is probed through a separate connection. Until it binds, connecting fails
            // rather than being slow, and the request helpers don't hide such failures.
            let listening = std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok();
            if listening && ready == Ready::Listening {
                return;
            }
            let answer = listening.then(|| {
                crate::full_request_within(
                    self.port,
                    "GET",
                    "/ready",
                    "",
                    // Long enough to outlast a first instantiation. A probe that gives up leaves
                    // its request in flight, so the next one finds no instance free and has a
                    // second raised for it, and the host then hands the two requests in turn.
                    READY_PATIENCE,
                )
            });
            match (ready, answer.flatten()) {
                (Ready::AnyResponse, Some(response)) if response.starts_with("HTTP/") => return,
                (Ready::Route, Some(response)) if response.starts_with("HTTP/1.1 200") => return,
                (_, answer) => {
                    last = answer;
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        // The last response, not just the log: `Ready::Route` requires a 200 from `/ready`, and a
        // handler that responds it with something else otherwise looks like a server that never
        // started.
        panic!(
            "wasmtime serve did not become ready on port {}; last answer to /ready: {}\n{}",
            self.port,
            last.unwrap_or_else(|| "<none>".to_string()),
            self.log()
        );
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Everything the server has written to `stdout`/`stderr` so far, the guest's `console.error`
    /// output included.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("serve.log")).unwrap_or_default()
    }

    /// Wait for `needle` to appear in the serve log, up to `patience`. Whether it arrived — a test
    /// asserting a marker is *absent* waits out its window and expects `false`.
    pub fn wait_for_marker(&self, needle: &str, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        loop {
            if self.log().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Wait until something that has not reported before reports an idle event loop, recording it
    /// in `seen`.
    ///
    /// A serving host hands a further request to an instance blocked on I/O, and raises a fresh one
    /// beside an instance running guest code. A case that needs several requests in one instance
    /// therefore sends each once the one before it has parked. Requests already in flight go on
    /// running their own timers and fetches and park again under the tag they reported before, so
    /// only a new tag reports the request just sent. See `core_runtime::event_loop::trace_idle`.
    pub fn await_new_idle(&self, seen: &mut std::collections::HashSet<String>, patience: Duration) {
        let deadline = Instant::now() + patience;
        loop {
            let log = self.log();
            if let Some(tag) = idle_tags(&log).find(|tag| !seen.contains(tag)) {
                seen.insert(tag);
                // The report is written just before the guest hands control back, and the host
                // counts the instance free a turn or two later. Nothing marks that, so wait a
                // fixed span for it.
                std::thread::sleep(SETTLE);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "nothing new went idle on port {}\n{log}",
                self.port,
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The instances to which the host reports having handed a request since `from` bytes into the
    /// log, and the length from which to read the next batch.
    ///
    /// A serving host hands a request to an instance it counts as free, and raises a fresh one
    /// otherwise. A case whose requests have to meet cannot make it choose, so it reads back what
    /// happened.
    pub fn instances_since(&self, from: usize) -> (Vec<u64>, usize) {
        let log = self.log();
        let ids = log[from.min(log.len())..]
            .lines()
            .filter_map(|line| {
                line.split_once("Instance ")?
                    .1
                    .split_once(" handling request")
            })
            .filter_map(|(id, _)| id.trim().parse().ok())
            .collect();
        (ids, log.len())
    }

    /// How much of the log has been written, for [`instances_since`](Self::instances_since).
    pub fn log_len(&self) -> usize {
        self.log().len()
    }

    /// The tags already in the log, for a test that starts counting new ones partway through.
    pub fn idle_so_far(&self) -> std::collections::HashSet<String> {
        idle_tags(&self.log()).collect()
    }

    /// The response to `GET path`, body only.
    pub fn get(&self, path: &str) -> String {
        self.request("GET", path, "")
    }

    /// The response to `method path` with `body`: the message body, de-chunked. `wasmtime serve`
    /// frames almost everything the guest sends as chunked — that framing is the host's business,
    /// and [`full_request`](Self::full_request) is where a test that cares about it looks.
    pub fn request(&self, method: &str, path: &str, body: &str) -> String {
        message_body(&self.full_request(method, path, body))
    }

    /// The whole response to `method path` with `body`, status line and headers included.
    pub fn full_request(&self, method: &str, path: &str, body: &str) -> String {
        crate::full_request_within(self.port, method, path, body, PATIENCE)
            .unwrap_or_else(|| panic!("no response to {method} {path}\n{}", self.log()))
    }
}

/// How long to wait after a loop reports handing control back, for the host to count the instance
/// free.
const SETTLE: Duration = Duration::from_millis(10);

/// Every idle tag in `log`, in the order they were written.
fn idle_tags(log: &str) -> impl Iterator<Item = String> + '_ {
    log.lines().filter_map(|line| {
        line.split_once(IDLE_TRACE)
            .map(|(_, tag)| tag.trim().to_string())
    })
}

impl Drop for WasmServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// A response's message body, with the host's chunked framing decoded. The free-standing form of
/// [`WasmServer::request`], for the tests that address a server's port from a thread of their own.
pub fn message_body(response: &str) -> String {
    match crate::split_response(response) {
        (Some(head), body) if head.to_lowercase().contains("transfer-encoding: chunked") => {
            crate::dechunk(body)
        }
        (_, body) => body.to_string(),
    }
}

/// The component under test, or `None` to skip the suite, with one note per test run.
///
/// `STARLING_WASM_COMPONENT` set and present: run against it, so the suite only ever tests a
/// component something just built. Set but missing: panic, so a stale path does not turn into a
/// skip. Unset, or `wasmtime` not on PATH: skip; `just test-serve-wasm` builds the component and
/// runs the suite.
pub fn component() -> Option<PathBuf> {
    static NOTED: std::sync::Once = std::sync::Once::new();
    let configured = std::env::var_os("STARLING_WASM_COMPONENT").map(PathBuf::from);
    if let Some(component) = &configured {
        assert!(
            component.is_file(),
            "STARLING_WASM_COMPONENT is set, but there is no component at {}",
            component.display()
        );
        if wasmtime_version().is_some() {
            // Absolutized: each server runs in a work directory of its own and hands the path to
            // `wasmtime` from there.
            return Some(component.canonicalize().unwrap());
        }
    }
    NOTED.call_once(|| {
        let missing = if configured.is_some() {
            "wasmtime is not on PATH"
        } else {
            "STARLING_WASM_COMPONENT is not set"
        };
        // Written to `stderr` directly: `eprintln!` inside a test is captured and only shown for
        // a failure, and every test here passes when it skips.
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPING the wasm serve end-to-end tests: {missing}\n\
             Run `just test-serve-wasm` to build the component and run them."
        );
    });
    None
}

/// The output of `wasmtime --version`, or `None` when `wasmtime` is not on PATH. Run once per
/// process.
fn wasmtime_version() -> Option<&'static str> {
    static VERSION: OnceLock<Option<String>> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            let output = Command::new("wasmtime").arg("--version").output().ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .as_deref()
}

/// The component compiled ahead of time, or `None` to serve the `.wasm` and let each server
/// compile it itself.
///
/// Compiling a component this size takes longer than everything else a server here does, and every
/// test runs in a process of its own, so left alone the suite compiles it once per test.
/// `wasmtime compile` moves that to once per build, and servers load the result with
/// `--allow-precompiled`.
///
/// A precompiled artifact loads only into a runtime configured the same way, so the stamp covers
/// the `wasmtime` version and [`CODEGEN_FLAGS`] alongside the component's own timestamp.
fn precompiled(component: &Path) -> Option<PathBuf> {
    let out = component.with_extension("cwasm");
    let stamp_path = component.with_extension("cwasm.stamp");
    let version = wasmtime_version()?;
    let modified = std::fs::metadata(component).ok()?.modified().ok()?;
    let stamp = format!("{version}\n{modified:?}\n{}", CODEGEN_FLAGS.join(" "));
    let current = |stamp: &str| {
        out.is_file() && std::fs::read_to_string(&stamp_path).is_ok_and(|found| found == stamp)
    };
    if current(&stamp) {
        return Some(out);
    }

    // One process or thread compiles. The rest wait for it rather than each compiling the same
    // artifact. Whoever creates the lock is the one that compiles.
    let lock = component.with_extension("cwasm.lock");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    {
        Ok(_) => {
            // Checked again, since a compile may have finished and released the lock between the
            // check above and taking it.
            if current(&stamp) {
                let _ = std::fs::remove_file(&lock);
                return Some(out);
            }
            eprintln!("Precompiling the wasm component (once for this build) ...");
            // Written next to `out` and renamed over it, so a server never loads a partly
            // written artifact.
            let partial = component.with_extension("cwasm.partial");
            let result = Command::new("wasmtime")
                .arg("compile")
                .args(CODEGEN_FLAGS)
                .arg(component)
                .arg("-o")
                .arg(&partial)
                .output();
            let compiled = result.is_ok_and(|result| result.status.success())
                && std::fs::rename(&partial, &out).is_ok();
            if compiled {
                let _ = std::fs::write(&stamp_path, &stamp);
            }
            let _ = std::fs::remove_file(&lock);
            compiled.then_some(out)
        }
        // Someone else is compiling it. A stale lock from a killed run would leave this waiting
        // out the deadline and then serving the `.wasm`, which is slow rather than wrong.
        Err(_) => {
            let deadline = Instant::now() + Duration::from_secs(300);
            while Instant::now() < deadline {
                if current(&stamp) {
                    return Some(out);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            None
        }
    }
}

/// A clean directory for one server to run in.
///
/// The directory is named after the current thread, which the test harness names after the test,
/// and after the process, so concurrent runs of one suite do not share it. A test's second and
/// later servers get a numbered suffix. [`WasmServer`]'s drop removes the directory unless the
/// thread is panicking, so a failure's serve log and content script can be looked at.
fn work_dir() -> PathBuf {
    static STARTED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let thread = std::thread::current();
    let test = thread.name().unwrap_or("server").replace("::", "-");
    let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
    let earlier = started.iter().filter(|name| **name == test).count();
    started.push(test.clone());
    drop(started);
    let process = std::process::id();
    let name = match earlier {
        0 => format!("{test}-{process}"),
        _ => format!("{test}-{process}-{}", earlier + 1),
    };
    let dir = std::env::temp_dir().join("starling-serve-wasm").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Snapshot `component`, configured with `config` and run in `dir`, into `out`. With `no_zeal`,
/// SpiderMonkey runs under its ordinary GC (see [`Serve::without_gc_zeal`]).
///
/// `--keep-init-func` because dropping the init export — wizer's default — leaves the component
/// referring to a core export that is no longer there, and the snapshot then fails to load at all.
fn snapshot_with_wizer(dir: &Path, component: &Path, out: &Path, config: &str, no_zeal: bool) {
    let mut command = Command::new("wasmtime");
    command
        .current_dir(dir)
        .args(["wizer"])
        .args(CODEGEN_FLAGS)
        .args(["--keep-init-func=true", "-o"])
        .arg(out)
        .args(["--dir=.::/cwd", "--dir=.", WASI_FLAGS, "--env"])
        .arg(format!("STARLINGMONKEY_CONFIG={config}"))
        .arg(component);
    if no_zeal {
        command.env_remove("JS_GC_ZEAL");
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "wizer failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
