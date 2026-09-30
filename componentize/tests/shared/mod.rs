// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Helpers shared by the componentization suites.

#![allow(dead_code)]

use futures::FutureExt as _;
use std::any::Any;
use std::collections::VecDeque;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::OnceCell;
use wasmtime::component::{
    Accessor, Destination, FutureConsumer, FutureReader, ResourceTable, Source, StreamConsumer,
    StreamProducer, StreamReader, StreamResult, VecBuffer,
};
use wasmtime::{AsContextMut, Store, StoreContextMut};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// The repository root, which the runtime builds and the fixtures are found
/// relative to.
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("canonicalizing the workspace root")
}

/// The runtime component `STARLING_RUNTIME` names, defaulting to what `just
/// build-runtime-debug` writes.
pub fn runtime_path() -> PathBuf {
    std::env::var("STARLING_RUNTIME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root().join("target/wasm32-wasip3/debug/starling.wasm"))
}

/// Run `starling-componentize <args>` in `dir`, or the test's working directory
/// without one, with the runtime at [`runtime_path`] rather than the one it
/// embeds.
pub fn run_cli(args: &[&str], dir: Option<&std::path::Path>) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_starling-componentize"));
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    command
        .args(args)
        .env("STARLING_RUNTIME", runtime_path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("running `starling-componentize`")
}

/// What `starling-componentize <args>` prints to stdout (see [`run_cli`]),
/// panicking with its stderr if it fails.
pub fn cli_output(args: &[&str]) -> String {
    let output = run_cli(args, None);
    assert!(
        output.status.success(),
        "`starling-componentize {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

/// The runtime build at [`runtime_path`]. The error names the path, since a
/// missing runtime build is the usual way a suite fails on a fresh checkout.
pub fn runtime() -> anyhow::Result<componentize::Runtime> {
    let path = runtime_path();
    let bytes = std::fs::read(&path).map_err(|e| {
        anyhow::anyhow!(
            "unable to read `{}` ({e}); run `just build-runtime-debug` first",
            path.display()
        )
    })?;
    componentize::Runtime::new(bytes)
}

/// The WASI context and resource table a suite's store data holds.
///
/// A suite that needs host state of its own (a captured log, an HTTP context)
/// holds this next to it, rather than repeating the two fields and the
/// `WasiView` impl over them.
pub struct Wasi {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl Wasi {
    /// A context inheriting the test process's stdout and stderr.
    pub fn inherit() -> Wasi {
        Wasi::from_builder(WasiCtxBuilder::new().inherit_stdout().inherit_stderr())
    }

    /// A context from a suite's own builder, for one that captures output or
    /// preopens a directory.
    pub fn from_builder(builder: &mut WasiCtxBuilder) -> Wasi {
        Wasi {
            ctx: builder.build(),
            table: ResourceTable::default(),
        }
    }

    /// The view a `WasiView` impl returns.
    pub fn view(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }

    /// The resource table, for a `WasiHttpView` that pairs it with its own
    /// context.
    pub fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

/// The store data for a suite whose world needs nothing but WASI.
pub struct Ctx {
    pub wasi: Wasi,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

/// A fresh store for a suite whose world needs nothing but WASI.
pub fn store(engine: &wasmtime::Engine) -> Store<Ctx> {
    Store::new(
        engine,
        Ctx {
            wasi: Wasi::inherit(),
        },
    )
}

/// A WASI context whose stderr goes to the returned pipe, for the tests that
/// assert on the message the runtime prints when it traps a call.
pub fn wasi_capturing_stderr() -> (Wasi, MemoryOutputPipe) {
    let stderr = MemoryOutputPipe::new(64 * 1024);
    let wasi = Wasi::from_builder(
        WasiCtxBuilder::new()
            .inherit_stdout()
            .stderr(stderr.clone()),
    );
    (wasi, stderr)
}

/// A fresh store for a suite whose world needs nothing but WASI, with the
/// guest's stderr going to the returned pipe.
pub fn store_capturing_stderr(engine: &wasmtime::Engine) -> (Store<Ctx>, MemoryOutputPipe) {
    let (wasi, stderr) = wasi_capturing_stderr();
    (Store::new(engine, Ctx { wasi }), stderr)
}

/// Componentize `js` against `world` of `wit` with the runtime build [`runtime`]
/// reads and `options`.
pub async fn componentize_with<'a>(
    wit: componentize::Wit<'_>,
    world: impl Into<componentize::WorldSelection<'a>>,
    js: impl Into<componentize::JsSource<'_>>,
    options: &componentize::ComponentizeOptions<'_>,
) -> anyhow::Result<componentize::Componentized> {
    componentize::componentize(wit, world, js, &runtime()?, options).await
}

/// Componentize `js` against the world `world` of the WIT source `wit`, with the
/// runtime build [`runtime`] reads and the default options.
pub async fn try_componentize(wit: &str, world: &str, js: &str) -> anyhow::Result<Vec<u8>> {
    componentize_with(
        componentize::Wit::String(wit),
        Some(world),
        js,
        &Default::default(),
    )
    .await
    .map(|componentized| componentized.component)
}

/// Like [`try_componentize`], but compiles the result for `engine` and panics if
/// either step fails.
pub async fn compile_fixture(
    engine: &wasmtime::Engine,
    wit: &str,
    world: &str,
    js: &str,
) -> wasmtime::component::Component {
    let component = try_componentize(wit, world, js)
        .await
        .unwrap_or_else(|e| panic!("componentizing the `{world}` world: {e:#}"));
    wasmtime::component::Component::new(engine, &component).expect("compiling the component")
}

/// Whether the call or the `run_concurrent` around it returned `Err`.
pub fn call_failed<T>(outcome: &wasmtime::Result<wasmtime::Result<T>>) -> bool {
    match outcome {
        Err(_) => true,
        Ok(inner) => inner.is_err(),
    }
}

/// The text a [`MemoryOutputPipe`] holds.
pub fn pipe_text(pipe: &MemoryOutputPipe) -> String {
    String::from_utf8_lossy(&pipe.contents()).into_owned()
}

/// Componentize a suite's world once and hand every case in the binary the same value.
///
/// [`OnceCell::get_or_init`] abandons the cell when its initializer panics, so the next case
/// would start the initializer again, and componentizing a world takes tens of seconds. This
/// records the failure and replays it, so a broken world takes one attempt and every case after
/// the first fails immediately.
pub async fn build_once<T>(
    cell: &'static OnceCell<Result<T, String>>,
    build: impl Future<Output = T>,
) -> &'static T {
    cell.get_or_init(|| async {
        AssertUnwindSafe(build)
            .catch_unwind()
            .await
            .map_err(panic_message)
    })
    .await
    .as_ref()
    .unwrap_or_else(|message| panic!("{message}"))
}

/// The message from a panic payload, for replaying a recorded failure. The panic itself was
/// already reported by the default hook when it happened.
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "componentizing the world panicked".to_string())
}

/// The engine configuration under which every suite runs its componentized app.
///
/// Compiling the snapshot is the second cranelift pass over the component in a suite, after the
/// componentizer's own, and it repeats on every run. With wasmtime's compilation cache, a run that
/// did not rebuild the runtime reuses the compilations of both the snapshot and the
/// componentizer.
pub fn engine() -> wasmtime::Engine {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    // As in the componentizer's own engine, the compiled code's unwind information isn't
    // registered with the system unwinder, which serializes the suite's threads on macOS.
    config.native_unwind_info(false);
    config.cache(wasmtime::Cache::from_file(None).ok());
    wasmtime::Engine::new(&config).unwrap()
}

/// A `StreamConsumer<u8>` that collects bytes until it has `limit` of them, then
/// drops its end of the stream, and records when it is dropped: when the stream
/// ends, or when it hangs up itself.
pub struct BytesConsumer {
    pub sink: Arc<Mutex<Vec<u8>>>,
    pub ended: Arc<AtomicBool>,
    pub limit: usize,
}

impl<D> StreamConsumer<D> for BytesConsumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, u8>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let pending = source.remaining(store.as_context_mut());
        let mut buf: Vec<u8> = Vec::with_capacity(pending);
        source.read(store.as_context_mut(), &mut buf)?;
        let mut sink = self.sink.lock().unwrap();
        sink.extend_from_slice(&buf);
        if sink.len() >= self.limit {
            Poll::Ready(Ok(StreamResult::Dropped))
        } else {
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }
}

impl Drop for BytesConsumer {
    fn drop(&mut self) {
        self.ended.store(true, Ordering::SeqCst);
    }
}

/// A `StreamProducer<u8>` that delivers one queued chunk per read, and records
/// when it is dropped, which happens once the reader drops its end or every
/// chunk is delivered. With `endless`, it delivers a 1 KiB chunk per read once
/// the queue is empty, forever.
pub struct ChunkProducer {
    pub chunks: VecDeque<Vec<u8>>,
    pub endless: bool,
    pub dropped: Arc<AtomicBool>,
}

impl<D> StreamProducer<D> for ChunkProducer {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, u8, VecBuffer<u8>>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let this = self.get_mut();
        let chunk = match this.chunks.pop_front() {
            Some(chunk) => chunk,
            None if this.endless => vec![7; 1024],
            None => return Poll::Ready(Ok(StreamResult::Dropped)),
        };
        destination.set_buffer(chunk.into());
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

impl Drop for ChunkProducer {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// Yield to the guest until `done` holds. Panics if it still does not after
/// 100 000 turns.
pub async fn until(done: impl Fn() -> bool) {
    for _ in 0..100_000 {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the awaited condition did not hold within 100 000 turns");
}

/// Read a stream the guest produced until it ends. Returns the values and
/// whether it ended.
pub async fn collect_all<T: 'static, D: wasmtime::component::HasData, E>(
    store: &Accessor<T, D>,
    stream: StreamReader<E>,
) -> anyhow::Result<(Vec<E>, bool)>
where
    E: wasmtime::component::Lift + Send + Sync + 'static,
{
    let sink = Arc::new(Mutex::new(Vec::new()));
    let ended = Arc::new(AtomicBool::new(false));
    store.with(|store| {
        stream.pipe(
            store,
            EndingConsumer {
                sink: sink.clone(),
                ended: ended.clone(),
            },
        )
    })?;
    until(|| ended.load(Ordering::SeqCst)).await;
    let values = std::mem::take(&mut *sink.lock().unwrap());
    Ok((values, ended.load(Ordering::SeqCst)))
}

/// A `StreamConsumer<T>` that collects every value it receives, and records when
/// it is dropped, which happens once the stream ends.
struct EndingConsumer<T> {
    sink: Arc<Mutex<Vec<T>>>,
    ended: Arc<AtomicBool>,
}

impl<D, T> StreamConsumer<D> for EndingConsumer<T>
where
    T: wasmtime::component::Lift + Send + Sync + 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let pending = source.remaining(store.as_context_mut());
        let mut buf: Vec<T> = Vec::with_capacity(pending);
        source.read(store.as_context_mut(), &mut buf)?;
        self.sink.lock().unwrap().extend(buf);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

impl<T> Drop for EndingConsumer<T> {
    fn drop(&mut self) {
        self.ended.store(true, Ordering::SeqCst);
    }
}

/// A `FutureConsumer<T>` that stores the value it receives in a shared slot.
pub struct OptionConsumer<T> {
    pub slot: Arc<Mutex<Option<T>>>,
}

impl<D, T> FutureConsumer<D> for OptionConsumer<T>
where
    T: wasmtime::component::Lift + Send + Sync + 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<()>> {
        let mut buf: Vec<T> = Vec::with_capacity(1);
        source.read(store.as_context_mut(), &mut buf)?;
        if let Some(value) = buf.into_iter().next() {
            *self.slot.lock().unwrap() = Some(value);
        }
        Poll::Ready(Ok(()))
    }
}

/// Read a `future<E>` the guest wrote.
pub async fn read_future<T: 'static, D: wasmtime::component::HasData, E>(
    store: &Accessor<T, D>,
    future: FutureReader<E>,
) -> anyhow::Result<E>
where
    E: wasmtime::component::Lift + Send + Sync + 'static,
{
    let slot = Arc::new(Mutex::new(None));
    store.with(|store| future.pipe(store, OptionConsumer { slot: slot.clone() }))?;
    until(|| slot.lock().unwrap().is_some()).await;
    let value = slot.lock().unwrap().take();
    value.ok_or_else(|| anyhow::anyhow!("the future delivered no value"))
}

/// Read a `stream<u8>` the guest returned until it ends, or until `limit` bytes
/// arrived, at which point the host drops its end. Returns the bytes and whether
/// the host's end was dropped.
pub async fn read_bytes<T: 'static>(
    store: &Accessor<T>,
    stream: StreamReader<u8>,
    limit: usize,
) -> anyhow::Result<(Vec<u8>, bool)> {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let ended = Arc::new(AtomicBool::new(false));
    store.with(|store| {
        stream.pipe(
            store,
            BytesConsumer {
                sink: sink.clone(),
                ended: ended.clone(),
                limit,
            },
        )
    })?;
    until(|| ended.load(Ordering::SeqCst)).await;
    let bytes = std::mem::take(&mut *sink.lock().unwrap());
    Ok((bytes, ended.load(Ordering::SeqCst)))
}

/// A host-produced `stream<u8>` delivering `chunks` one per read, and the flag its
/// producer sets when it is dropped.
pub fn chunked_input<T: 'static>(
    store: &Accessor<T>,
    chunks: &[&[u8]],
    endless: bool,
) -> anyhow::Result<(StreamReader<u8>, Arc<AtomicBool>)> {
    let dropped = Arc::new(AtomicBool::new(false));
    let producer = ChunkProducer {
        chunks: chunks.iter().map(|chunk| chunk.to_vec()).collect(),
        endless,
        dropped: dropped.clone(),
    };
    let stream = store.with(|store| StreamReader::new(store, producer))?;
    Ok((stream, dropped))
}

/// The `wasi:http` hooks of the suites that `fetch`: every outgoing request is
/// answered with a `200` whose body is the request's body for a `POST`,
/// `echo <method> <path>` for a path starting with `/echo`, `partial ` followed
/// by a `connection-terminated` error for a path starting with `/broken`, and
/// `body` otherwise. `default-features = false` on
/// `wasmtime-wasi-http` drops `default-send-request`, so no request reaches a
/// real network.
pub struct UpstreamHooks {
    pub body: &'static str,
}

impl wasmtime_wasi_http::WasiHttpHooks for UpstreamHooks {
    fn send_request(
        &mut self,
        request: http::Request<
            http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, wasmtime_wasi_http::Error>,
        >,
        _options: Option<wasmtime_wasi_http::RequestOptions>,
        _fut: Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        http::Response<
                            http_body_util::combinators::UnsyncBoxBody<
                                bytes::Bytes,
                                wasmtime_wasi_http::Error,
                            >,
                        >,
                        Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
                    ),
                    wasmtime_wasi_http::Error,
                >,
            > + Send,
    > {
        use http_body_util::BodyExt as _;
        let path = request.uri().path().to_string();
        let text = if path.starts_with("/echo") {
            format!("echo {} {path}", request.method())
        } else {
            self.body.to_string()
        };
        let broken = path.starts_with("/broken");
        Box::new(async move {
            let body = if broken {
                use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;
                let frames = futures::stream::iter([
                    Ok(http_body::Frame::data(bytes::Bytes::from_static(
                        b"partial ",
                    ))),
                    Err(wasmtime_wasi_http::Error::from(
                        ErrorCode::ConnectionTerminated,
                    )),
                ]);
                http_body_util::StreamBody::new(frames).boxed_unsync()
            } else if request.method() == http::Method::POST {
                request.into_body()
            } else {
                http_body_util::Full::new(bytes::Bytes::from(text))
                    .map_err(|never: std::convert::Infallible| match never {})
                    .boxed_unsync()
            };
            let response = http::Response::builder()
                .status(200)
                .body(body)
                .expect("building the upstream response");
            let io: Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send> =
                Box::new(async { Ok(()) });
            Ok((response, io))
        })
    }
}

/// The store data for a suite whose world needs WASI and `wasi:http`.
pub struct HttpCtx {
    pub wasi: Wasi,
    pub http: wasmtime_wasi_http::WasiHttpCtx,
    pub hooks: UpstreamHooks,
}

impl WasiView for HttpCtx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

impl wasmtime_wasi_http::WasiHttpView for HttpCtx {
    fn http(&mut self) -> wasmtime_wasi_http::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::WasiHttpCtxView {
            ctx: &mut self.http,
            table: self.wasi.table(),
            hooks: &mut self.hooks,
        }
    }
}

/// A fresh store for a suite whose world needs WASI and `wasi:http`, whose
/// outgoing requests are answered with `body` (see [`UpstreamHooks`]).
pub fn http_store(engine: &wasmtime::Engine, body: &'static str) -> Store<HttpCtx> {
    Store::new(
        engine,
        HttpCtx {
            wasi: Wasi::inherit(),
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: UpstreamHooks { body },
        },
    )
}

/// Like [`http_store`], with the guest's stderr captured in the returned pipe.
pub fn http_store_capturing_stderr(
    engine: &wasmtime::Engine,
    body: &'static str,
) -> (Store<HttpCtx>, MemoryOutputPipe) {
    let (wasi, stderr) = wasi_capturing_stderr();
    let ctx = HttpCtx {
        wasi,
        http: wasmtime_wasi_http::WasiHttpCtx::new(),
        hooks: UpstreamHooks { body },
    };
    (Store::new(engine, ctx), stderr)
}

/// A linker for `component`: the WASI host added by [`componentize::add_wasi`],
/// the definitions from `add`, and trapping stubs for the component's other imports.
/// `provided` names the interface prefixes `add` provides, as
/// [`componentize::trap_unsatisfied_imports`] takes them.
pub fn linker<T: WasiView + 'static>(
    engine: &wasmtime::Engine,
    component: &wasmtime::component::Component,
    provided: &[&str],
    add: impl FnOnce(&mut wasmtime::component::Linker<T>) -> wasmtime::Result<()>,
) -> wasmtime::component::Linker<T> {
    let mut linker = wasmtime::component::Linker::new(engine);
    componentize::add_wasi(&mut linker).expect("wasi linker");
    add(&mut linker).expect("adding the suite's host");
    componentize::trap_unsatisfied_imports(engine, component, &mut linker, provided)
        .expect("trap-stub unknown imports");
    linker
}

/// Like [`linker`], for a component that exports `wasi:http/handler`, and with
/// the p3 `wasi:http` host, which defines the handler's request and response
/// resources.
pub fn http_linker(
    engine: &wasmtime::Engine,
    component: &wasmtime::component::Component,
) -> wasmtime::component::Linker<HttpCtx> {
    http_linker_with(engine, component, &[], |_| Ok(()))
}

/// Like [`http_linker`], with the definitions from `add`, which provide the
/// interfaces whose prefixes `provided` names.
pub fn http_linker_with(
    engine: &wasmtime::Engine,
    component: &wasmtime::component::Component,
    provided: &[&str],
    add: impl FnOnce(&mut wasmtime::component::Linker<HttpCtx>) -> wasmtime::Result<()>,
) -> wasmtime::component::Linker<HttpCtx> {
    let provided: Vec<&str> = ["wasi:http/"]
        .into_iter()
        .chain(provided.iter().copied())
        .collect();
    linker(engine, component, &provided, |linker| {
        wasmtime_wasi_http::p3::add_to_linker(linker)?;
        add(linker)
    })
}

/// Drive `wasi:http/handler.handle()` on `service`, an instance in `store`, with
/// a `method` request to `path` whose body is `body`. Returns the response status
/// and the collected body as a string, or the `error-code` the handler returned.
pub async fn handle_on(
    store: &mut Store<HttpCtx>,
    service: &wasmtime_wasi_http::p3::bindings::Service,
    method: &str,
    path: &str,
    body: &'static str,
) -> anyhow::Result<Result<(u16, String), wasmtime_wasi_http::p3::bindings::http::types::ErrorCode>>
{
    use bytes::Bytes;
    use futures::future::{select, Either};
    use http_body_util::{BodyExt as _, Collected, Full};
    use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;

    // `Full<Bytes>`'s error type is `Infallible`, which `ErrorCode:
    // From<Infallible>` lifts into the wit stream's element error.
    let req = http::Request::builder()
        .method(method)
        .uri(format!("http://example.com{path}"))
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .expect("building the request");
    let (req, io) = wasmtime_wasi_http::p3::Request::from_http(
        &mut UpstreamHooks {
            body: store.data().hooks.body,
        },
        req,
    );

    let response: Result<http::Response<Collected<Bytes>>, ErrorCode> = store
        .run_concurrent(async |store| {
            // The handler call and the request-body-processing future run
            // concurrently: `io` resolves once the request body is consumed,
            // while `handle` runs the guest until it responds. A handler that
            // responds without consuming the body leaves `io` pending, so the
            // response is returned without waiting for it.
            let handled = std::pin::pin!(async {
                let res = match service.handle(store, req).await? {
                    Ok(res) => res,
                    Err(code) => return anyhow::Ok(Err(code)),
                };
                let res = store.with(|store| res.into_http(store, async { Ok(()) }))?;
                let (parts, body) = res.into_parts();
                let body = body.collect().await?;
                anyhow::Ok(Ok(http::Response::from_parts(parts, body)))
            });
            let io = std::pin::pin!(async {
                io.await.map_err(|e| anyhow::anyhow!("request body: {e:?}"))
            });
            match select(handled, io).await {
                Either::Left((res, _io)) => res,
                Either::Right((io, handled)) => {
                    io?;
                    handled.await
                }
            }
        })
        .await??;

    Ok(response.map(|response| {
        let status = response.status().as_u16();
        let body = String::from_utf8_lossy(&response.into_body().to_bytes()).into_owned();
        (status, body)
    }))
}

/// The import and export names of a component's world.
pub fn surface(component: &[u8]) -> (Vec<String>, Vec<String>) {
    let wit_parser::decoding::DecodedWasm::Component(resolve, world) =
        wit_parser::decoding::decode(component).expect("decoding the component")
    else {
        panic!("not a component");
    };
    let names = |items: &indexmap::IndexMap<wit_parser::WorldKey, wit_parser::WorldItem>| {
        items
            .keys()
            .map(|key| resolve.name_world_key(key))
            .collect::<Vec<_>>()
    };
    (
        names(&resolve.worlds[world].imports),
        names(&resolve.worlds[world].exports),
    )
}
