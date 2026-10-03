// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Components built for debugging.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
//
// A serve application is componentized with and without `debug`, and served
// with `STARLING_DEBUG` set:
//
//   - Without `debug`, the component imports no `wasi:sockets` interface,
//     serves the request, and opens no debug listener.
//   - With `debug`, it imports `wasi:sockets/types@0.3.0`, and the request
//     waits for a DAP client on the `listen:<port>,wait` listener. The client
//     sets a breakpoint in the `fetch` listener, sees it hit, and continues, and
//     the request is answered.
//
// The CLI componentizes a `fetch` listener given by a relative path with
// `--debug`, and a breakpoint set on the file's host path binds and is hit.
//
// A world with an async export is componentized with `debug`, and a call to the
// export, made with `STARLING_DEBUG=connect:<host>:<port>`, connects to a DAP
// client the test listens with, then stops at a breakpoint in the export. A
// breakpoint in the world's sync export is skipped with an `output` event.
// Without `debug`, the world's own `wasi:sockets/types` import stays, and the
// call ignores `STARLING_DEBUG`.
//
// `debug` is refused with `sockets` disabled and with the runtime dylib.
//
// The DAP client is minimal: it frames JSON messages over a TCP stream, on a
// thread of its own, while the test drives the guest.

mod shared;

use {
    serde_json::{json, Value},
    std::{
        collections::VecDeque,
        io::{self, BufRead as _, BufReader, Read as _, Write as _},
        net::{Ipv4Addr, TcpListener, TcpStream},
        path::PathBuf,
        thread,
        time::{Duration, Instant},
    },
    wasmtime::{
        component::{Component, Linker},
        Engine, Store,
    },
    wasmtime_wasi::{p2::pipe::MemoryOutputPipe, WasiCtxBuilder},
    wasmtime_wasi_http::p3::bindings::Service,
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/debuggee.wit",
    world: "debuggee",
    exports: { default: async },
});

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// How long the DAP client waits for a message, and the test for a call into
/// the guest.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The serve application's name, which is its script URL.
const HANDLER_PATH: &str = "/debug/handler.js";

/// A `fetch` listener answering `42`. Line 2 runs for every request.
const HANDLER: &str = "addEventListener('fetch', (event) => {
  event.respondWith(new Response(String(6 * 7)));
});
";

/// The async export's module name, which is its script URL.
const EXPORT_PATH: &str = "/debug/export.js";

/// The implementation of `debuggee`'s exports. Line 3 of `compute` runs after a
/// timer turn, and line 6 is the body of `twice`.
const EXPORT: &str = "export async function compute(v) {
  await new Promise((resolve) => setTimeout(resolve, 1));
  return v * 2;
}
export function twice(v) {
  return v * 2;
}
";

/// Componentize `js`, named `name`, against the world `world` of the WIT source
/// `wit`.
async fn build(wit: &str, world: &str, name: &str, js: &str, debug: bool) -> Vec<u8> {
    let runtime = shared::runtime().expect("reading the runtime build");
    componentize::componentize_with_output(
        componentize::Wit::<PathBuf>::String(wit),
        Some(world),
        &[],
        false,
        componentize::JsSource { name, text: js },
        None::<PathBuf>,
        &runtime,
        &[],
        None,
        debug,
        false,
        None,
        false,
        None,
        true,
    )
    .await
    .unwrap_or_else(|e| panic!("componentizing the `{world}` world: {e:#}"))
    .component
}

/// [`HANDLER`], componentized against the serve world.
async fn build_handler(debug: bool) -> Vec<u8> {
    build(
        include_str!("fixtures/serve.wit"),
        "serve",
        HANDLER_PATH,
        HANDLER,
        debug,
    )
    .await
}

/// A WASI context with the host network and `STARLING_DEBUG` set to `debug`,
/// whose stderr goes to the returned pipe.
fn debug_wasi(debug: &str) -> (shared::Wasi, MemoryOutputPipe) {
    let stderr = MemoryOutputPipe::new(64 * 1024);
    let wasi = shared::Wasi::from_builder(
        WasiCtxBuilder::new()
            .inherit_stdout()
            .stderr(stderr.clone())
            .env("STARLING_DEBUG", debug)
            .inherit_network()
            .allow_tcp(true),
    );
    (wasi, stderr)
}

/// A store for a serve component with `wasi`.
fn http_store(wasi: shared::Wasi) -> Store<shared::HttpCtx> {
    Store::new(
        &ENGINE,
        shared::HttpCtx {
            wasi,
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: shared::UpstreamHooks { body: "" },
        },
    )
}

/// A port nothing listens on when this returns.
fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .expect("binding a free port")
        .port()
}

/// A DAP client on one TCP connection.
struct Dap {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    /// Messages read while waiting for a different one.
    backlog: VecDeque<Value>,
    next_seq: i64,
}

impl Dap {
    fn new(stream: TcpStream) -> Dap {
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.set_nodelay(true).unwrap();
        Dap {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            backlog: VecDeque::new(),
            next_seq: 1,
        }
    }

    /// Connect to the debug listener on `port`, retrying until it accepts the
    /// connection.
    fn connect(port: u16) -> Dap {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
                Ok(stream) => return Dap::new(stream),
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "nothing accepted a connection on port {port}: {error}"
                    );
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// Accept a debuggee's connection on `listener`.
    fn accept(listener: &TcpListener) -> Dap {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    return Dap::new(stream);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "the debuggee did not connect");
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("accepting the debuggee's connection failed: {error}"),
            }
        }
    }

    /// Send a request and return its `seq`.
    fn send(&mut self, command: &str, arguments: Value) -> i64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let body = json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        })
        .to_string();
        write!(self.writer, "Content-Length: {}\r\n\r\n{body}", body.len())
            .expect("writing a request failed");
        seq
    }

    /// Read the next message from the connection.
    fn read(&mut self) -> Value {
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = self
                .reader
                .read_line(&mut line)
                .expect("reading a message header failed");
            assert!(read > 0, "the debuggee closed the connection");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                length = Some(value.trim().parse::<usize>().expect("a numeric length"));
            }
        }
        let mut body = vec![0; length.expect("a message without a `Content-Length`")];
        self.reader
            .read_exact(&mut body)
            .expect("reading a message body failed");
        serde_json::from_slice(&body).expect("the adapter sent invalid JSON")
    }

    /// The first message matching `matches`, read from the backlog first. Other
    /// messages stay queued.
    fn take(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.backlog.iter().position(&matches) {
            return self.backlog.remove(index).unwrap();
        }
        loop {
            let message = self.read();
            if matches(&message) {
                return message;
            }
            self.backlog.push_back(message);
        }
    }

    /// Send a request, assert that it succeeded, and return the response body.
    fn request(&mut self, command: &str, arguments: Value) -> Value {
        let seq = self.send(command, arguments);
        let response =
            self.take(|message| message["type"] == "response" && message["request_seq"] == seq);
        assert_eq!(response["success"], true, "`{command}` failed: {response}");
        response["body"].clone()
    }

    /// The body of the first `name` event.
    fn event(&mut self, name: &str) -> Value {
        self.take(|message| message["type"] == "event" && message["event"] == name)["body"].clone()
    }

    /// Configure the session with a breakpoint on `line` of `path`, wait for it
    /// to be hit, check the top frame, and continue.
    fn stop_at_and_continue(&mut self, path: &str, line: u32) {
        self.configure(path, line);
        assert_eq!(self.event("stopped")["reason"], "breakpoint");
        let trace = self.request("stackTrace", json!({ "threadId": 1 }));
        let frame = &trace["stackFrames"][0];
        assert_eq!(frame["line"], line, "{frame}");
        assert_eq!(frame["source"]["path"], path, "{frame}");
        self.request("continue", json!({ "threadId": 1 }));
    }

    /// Configure the session with a breakpoint on `line` of `path`.
    fn configure(&mut self, path: &str, line: u32) {
        self.request(
            "initialize",
            json!({
                "adapterID": "starling",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
            }),
        );
        self.request("attach", json!({}));
        self.event("initialized");
        self.request(
            "setBreakpoints",
            json!({ "source": { "path": path }, "breakpoints": [{ "line": line }] }),
        );
        self.request("configurationDone", json!({}));
    }
}

#[tokio::test]
async fn components_without_debug_ignore_starling_debug() -> anyhow::Result<()> {
    let component = build_handler(false).await;
    let (imports, _) = shared::surface(&component);
    assert!(
        !imports.iter().any(|i| i.starts_with("wasi:sockets/")),
        "{imports:?}"
    );

    let component = Component::new(&ENGINE, &component)?;
    let debug_port = free_port();
    let (wasi, stderr) = debug_wasi(&format!("listen:{debug_port},wait"));
    let mut store = http_store(wasi);
    let service = Service::instantiate_async(
        &mut store,
        &component,
        &shared::http_linker(&ENGINE, &component),
    )
    .await?;
    let response = tokio::time::timeout(
        TIMEOUT,
        shared::handle_on(&mut store, &service, "GET", "/", ""),
    )
    .await
    .unwrap_or_else(|_| panic!("no response\n{}", shared::pipe_text(&stderr)))?
    .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))?;
    assert_eq!(
        response,
        (200, "42".to_string()),
        "{}",
        shared::pipe_text(&stderr)
    );
    assert!(
        TcpStream::connect((Ipv4Addr::LOCALHOST, debug_port)).is_err(),
        "something listens on the debug port"
    );
    assert!(
        !shared::pipe_text(&stderr).contains("debugger"),
        "{}",
        shared::pipe_text(&stderr)
    );
    Ok(())
}

/// Whether the suite links the runtime dylib, which a debuggable component
/// cannot use. Reports the skip.
fn skip_debug() -> bool {
    let skip = shared::dynamic_link_mode();
    if skip {
        eprintln!("skipped: a debuggable component needs the static runtime");
    }
    skip
}

/// Serve `component` with `STARLING_DEBUG=listen:<port>,wait`, and send `GET /`
/// while a DAP client stops at a breakpoint on `line` of `path` and continues.
/// Returns the response, after checking that the guest reported the listener.
async fn serve_stopping_at(
    component: &[u8],
    path: String,
    line: u32,
) -> anyhow::Result<(u16, String)> {
    let component = Component::new(&ENGINE, component)?;
    let debug_port = free_port();
    let (wasi, stderr) = debug_wasi(&format!("listen:{debug_port},wait"));
    let mut store = http_store(wasi);
    let service = Service::instantiate_async(
        &mut store,
        &component,
        &shared::http_linker(&ENGINE, &component),
    )
    .await?;
    let client = thread::spawn(move || {
        let mut dap = Dap::connect(debug_port);
        dap.stop_at_and_continue(&path, line);
        dap
    });
    let response = tokio::time::timeout(
        TIMEOUT,
        shared::handle_on(&mut store, &service, "GET", "/", ""),
    )
    .await;
    // The client is joined first, so its failed assertion is reported instead
    // of the timeout of the call it left waiting.
    let _dap = client.join().expect("the DAP client failed");
    let response = response
        .unwrap_or_else(|_| panic!("no response\n{}", shared::pipe_text(&stderr)))?
        .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))?;
    assert!(
        shared::pipe_text(&stderr)
            .contains(&format!("debugger listening on 127.0.0.1:{debug_port}")),
        "{}",
        shared::pipe_text(&stderr)
    );
    Ok(response)
}

#[tokio::test]
async fn components_with_debug_listen_and_pause() -> anyhow::Result<()> {
    if skip_debug() {
        return Ok(());
    }
    let component = build_handler(true).await;
    let (imports, _) = shared::surface(&component);
    assert!(
        imports.iter().any(|i| i == "wasi:sockets/types@0.3.0"),
        "{imports:?}"
    );
    let response = serve_stopping_at(&component, HANDLER_PATH.to_string(), 2).await?;
    assert_eq!(response, (200, "42".to_string()));
    Ok(())
}

/// The input path of the CLI test's entry module, relative to the crate
/// directory, which the tests run in.
const ENTRY: &str = "tests/fixtures/debug_entry.js";

/// `starling-componentize --debug` names an entry module given by a relative
/// path by its path in the guest, so a breakpoint set on the file binds.
#[tokio::test]
async fn cli_debug_binds_breakpoints_in_a_relatively_named_entry_module() -> anyhow::Result<()> {
    if skip_debug() {
        return Ok(());
    }
    let output =
        std::env::temp_dir().join(format!("starling-debug-entry-{}.wasm", std::process::id()));
    let args: Vec<std::ffi::OsString> = vec![
        "starling-componentize".into(),
        "--serve".into(),
        "-q".into(),
        "componentize".into(),
        ENTRY.into(),
        "--debug".into(),
        "--runtime".into(),
        shared::static_runtime_path().into_os_string(),
        "-o".into(),
        output.clone().into_os_string(),
    ];
    // `run` starts a tokio runtime of its own, which cannot be started on a
    // thread that runs one.
    thread::spawn(move || componentize::command::run(args, None))
        .join()
        .expect("the componentizer panicked")?;
    let component = std::fs::read(&output)?;
    std::fs::remove_file(&output)?;

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(ENTRY)
        .to_str()
        .expect("a UTF-8 path")
        .to_string();
    let response = serve_stopping_at(&component, path, 7).await?;
    assert_eq!(response, (200, "42".to_string()));
    Ok(())
}

/// An instance of [`EXPORT`] componentized against `debuggee` with `debug`,
/// with `STARLING_DEBUG` set to `starling_debug`, the pipe its stderr goes to,
/// and the component's import names.
async fn debuggee(
    debug: bool,
    starling_debug: &str,
) -> anyhow::Result<(Store<shared::Ctx>, Debuggee, MemoryOutputPipe, Vec<String>)> {
    let component = build(
        include_str!("fixtures/debuggee.wit"),
        "debuggee",
        EXPORT_PATH,
        EXPORT,
        debug,
    )
    .await;
    let (imports, _) = shared::surface(&component);
    let component = Component::new(&ENGINE, &component)?;
    let (wasi, stderr) = debug_wasi(starling_debug);
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker)?;
    componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
    let mut store = Store::new(&ENGINE, shared::Ctx { wasi });
    let instance = DebuggeePre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    Ok((store, instance, stderr, imports))
}

#[tokio::test]
async fn async_exports_connect_and_pause() -> anyhow::Result<()> {
    if skip_debug() {
        return Ok(());
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let (mut store, instance, stderr, _) =
        debuggee(true, &format!("connect:127.0.0.1:{port}")).await?;
    let client = thread::spawn(move || {
        let mut dap = Dap::accept(&listener);
        dap.stop_at_and_continue(EXPORT_PATH, 3);
        dap
    });
    let result = tokio::time::timeout(
        TIMEOUT,
        store.run_concurrent(async |accessor| instance.call_compute(accessor, 21).await),
    )
    .await;
    // The client is joined first, so its failed assertion is reported instead
    // of the timeout of the call it left waiting.
    let _dap = client.join().expect("the DAP client failed");
    let result =
        result.unwrap_or_else(|_| panic!("no result\n{}", shared::pipe_text(&stderr)))??;
    assert_eq!(result, 42, "{}", shared::pipe_text(&stderr));
    Ok(())
}

/// A breakpoint hit in a synchronous export is skipped with an `output` event,
/// and the call returns. A call to the async export starts the session, and the
/// queued event is written during a later call to it.
#[tokio::test]
async fn sync_exports_skip_breakpoints() -> anyhow::Result<()> {
    if skip_debug() {
        return Ok(());
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let (mut store, instance, stderr, _) =
        debuggee(true, &format!("connect:127.0.0.1:{port}")).await?;
    let client = thread::spawn(move || {
        let mut dap = Dap::accept(&listener);
        dap.configure(EXPORT_PATH, 6);
        let output = dap.take(|message| {
            message["event"] == "output"
                && message["body"]["output"]
                    .as_str()
                    .is_some_and(|output| output.contains("cannot pause"))
        });
        assert!(
            !dap.backlog
                .iter()
                .any(|message| message["event"] == "stopped"),
            "{output}"
        );
        dap
    });
    let results = tokio::time::timeout(TIMEOUT, async {
        let first = store
            .run_concurrent(async |accessor| instance.call_compute(accessor, 21).await)
            .await??;
        let doubled = instance.call_twice(&mut store, 21).await?;
        let last = store
            .run_concurrent(async |accessor| instance.call_compute(accessor, 1).await)
            .await??;
        anyhow::Ok((first, doubled, last))
    })
    .await;
    // The client is joined first, so its failed assertion is reported instead
    // of the timeout of the call it left waiting.
    let _dap = client.join().expect("the DAP client failed");
    let results =
        results.unwrap_or_else(|_| panic!("no result\n{}", shared::pipe_text(&stderr)))?;
    assert_eq!(results, (42, 42, 2), "{}", shared::pipe_text(&stderr));
    Ok(())
}

/// Without `debug`, a `wasi:sockets` interface the world imports itself stays
/// imported, the others are stubbed, and `STARLING_DEBUG` is ignored.
#[tokio::test]
async fn world_sockets_imports_stay_without_debug() -> anyhow::Result<()> {
    let debug_port = free_port();
    let (mut store, instance, stderr, imports) =
        debuggee(false, &format!("listen:{debug_port},wait")).await?;
    let sockets: Vec<&String> = imports
        .iter()
        .filter(|i| i.starts_with("wasi:sockets/"))
        .collect();
    assert_eq!(sockets, ["wasi:sockets/types@0.3.0"], "{imports:?}");
    let result = tokio::time::timeout(
        TIMEOUT,
        store.run_concurrent(async |accessor| instance.call_compute(accessor, 21).await),
    )
    .await
    .unwrap_or_else(|_| panic!("no result\n{}", shared::pipe_text(&stderr)))??;
    assert_eq!(result, 42, "{}", shared::pipe_text(&stderr));
    assert!(
        TcpStream::connect((Ipv4Addr::LOCALHOST, debug_port)).is_err(),
        "something listens on the debug port"
    );
    assert!(
        !shared::pipe_text(&stderr).contains("debugger"),
        "{}",
        shared::pipe_text(&stderr)
    );
    Ok(())
}

/// A debuggable component needs the static runtime, so `debug` with the
/// runtime dylib is refused.
#[tokio::test]
async fn debug_with_the_runtime_dylib_is_refused() {
    let runtime = componentize::Runtime::Dynamic(componentize::Libraries {
        runtime: Vec::new(),
        libc: Vec::new(),
        libcxx: Vec::new(),
        libcxxabi: Vec::new(),
        wasi_emulated_getpid: Vec::new(),
    });
    let error = componentize::componentize_with_output(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/serve.wit")),
        Some("serve"),
        &[],
        false,
        HANDLER,
        None::<PathBuf>,
        &runtime,
        &[],
        None,
        true,
        false,
        None,
        false,
        None,
        true,
    )
    .await
    .err()
    .expect("`debug` with the runtime dylib must be refused");
    assert!(
        format!("{error:#}").contains("a debuggable component needs the static runtime"),
        "{error:#}"
    );
}

/// A debuggable component keeps the `wasi:sockets` imports, so disabling them
/// with `debug` is refused.
#[tokio::test]
async fn debug_with_sockets_disabled_is_refused() {
    if skip_debug() {
        return;
    }
    let runtime = shared::runtime().expect("reading the runtime build");
    let error = componentize::componentize_with_output(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/serve.wit")),
        Some("serve"),
        &[],
        false,
        HANDLER,
        None::<PathBuf>,
        &runtime,
        &[componentize::finalize::WASIAPISet::Sockets],
        None,
        true,
        false,
        None,
        false,
        None,
        true,
    )
    .await
    .err()
    .expect("`debug` with `sockets` disabled must be refused");
    assert!(
        format!("{error:#}").contains("the `sockets` feature cannot be disabled"),
        "{error:#}"
    );
}
