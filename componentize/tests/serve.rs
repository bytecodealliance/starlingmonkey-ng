// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `wasi:http/handler` components: applications componentized against the serve world of
//! `fixtures/serve.wit`, served either by the runtime's handler, which dispatches each request as
//! a `fetch` event to the application's listeners, or by the application's own `handle`. The
//! tests drive `wasi:http/handler.handle()` through wasmtime's p3 `wasi:http` bindings.

mod shared;

use shared::HttpCtx as Ctx;

use {
    tokio::sync::OnceCell,
    wasmtime::{component::Component, Engine, Store},
    wasmtime_wasi_http::p3::bindings::{http::types::ErrorCode, Service},
};

/// The body of the harness's response to every outgoing request.
const UPSTREAM_BODY: &str = "from upstream";

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

const SERVE_WIT: &str = include_str!("fixtures/serve.wit");

/// Componentize a serve application against the serve world and compile it.
async fn componentize_serve(js: &'static str) -> Component {
    shared::compile_fixture(&ENGINE, SERVE_WIT, "serve", js).await
}

/// The componentized serve app that registers a `fetch` listener, and the
/// component compiled from it, built once.
async fn listener_build() -> &'static (Vec<u8>, Component) {
    static LISTENER: OnceCell<Result<(Vec<u8>, Component), String>> = OnceCell::const_new();
    shared::build_once(&LISTENER, async {
        let bytes = shared::try_componentize(SERVE_WIT, "serve", include_str!("fixtures/serve.js"))
            .await
            .expect("componentizing the serve world");
        let component = Component::new(&ENGINE, &bytes).expect("compiling component");
        (bytes, component)
    })
    .await
}

/// The componentized serve app that registers a `fetch` listener, built once.
async fn listener() -> &'static Component {
    &listener_build().await.1
}

/// The `wasi:http/handler` export of a component built from the serve world, for
/// finalizing it again.
fn handler_export() -> [componentize::finalize::KeptExport; 1] {
    [componentize::finalize::KeptExport::unrenamed(
        "wasi:http/handler@0.3.0".to_string(),
    )]
}

/// A store whose WASI context sends the guest's stderr to the returned pipe.
fn store_capturing_stderr() -> (Store<Ctx>, wasmtime_wasi::p2::pipe::MemoryOutputPipe) {
    let (wasi, stderr) = shared::wasi_capturing_stderr();
    let store = Store::new(
        &ENGINE,
        Ctx {
            wasi,
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: shared::UpstreamHooks {
                body: UPSTREAM_BODY,
            },
        },
    );
    (store, stderr)
}

/// The componentized serve app that implements `wasi:http/handler` itself, built
/// once.
async fn raw() -> &'static Component {
    static RAW: OnceCell<Result<Component, String>> = OnceCell::const_new();
    shared::build_once(
        &RAW,
        componentize_serve(include_str!("fixtures/serve_raw.js")),
    )
    .await
}

/// A fresh store with a WASI + `wasi:http` context.
fn store() -> Store<Ctx> {
    shared::http_store(&ENGINE, UPSTREAM_BODY)
}

/// Drive `wasi:http/handler.handle()` on a fresh instance of a component with a
/// bodyless `method` request to `path`. Returns the response status and the
/// collected body as a string, or the `error-code` the handler returned.
async fn handle_request(
    component: &Component,
    method: &str,
    path: &str,
) -> anyhow::Result<Result<(u16, String), ErrorCode>> {
    let mut store = store();
    let service = Service::instantiate_async(
        &mut store,
        component,
        &shared::http_linker(&ENGINE, component),
    )
    .await?;
    shared::handle_on(&mut store, &service, method, path, "").await
}

/// `--export-cli` and `--export-http` together build one component that serves
/// requests and runs as a command, from a module that registers a `fetch`
/// listener and exports `run`. It is built through the command line, whose `-q` leaves
/// stdout empty.
#[tokio::test]
async fn cli_and_serve_component_serves_and_runs() -> anyhow::Result<()> {
    let output = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("cli-and-serve.wasm");
    let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cli_and_serve.js")
        .display()
        .to_string();
    let args = [
        "-q",
        "--export-cli",
        "--export-http",
        "componentize",
        &input,
        "-o",
        &output.display().to_string(),
    ];
    let cli = shared::run_cli(&args, None);
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&cli.stdout), "");
    let component = Component::new(&ENGINE, std::fs::read(&output)?)?;

    assert_eq!(
        handle_request(&component, "GET", "/both").await??,
        (200, "hello from one component, serving /both".to_string())
    );

    let stdout = wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(64 * 1024);
    let mut store = Store::new(
        &ENGINE,
        Ctx {
            wasi: shared::Wasi::from_builder(
                wasmtime_wasi::WasiCtxBuilder::new()
                    .stdout(stdout.clone())
                    .inherit_stderr(),
            ),
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: shared::UpstreamHooks {
                body: UPSTREAM_BODY,
            },
        },
    );
    let command = wasmtime_wasi::p3::bindings::Command::instantiate_async(
        &mut store,
        &component,
        &shared::http_linker(&ENGINE, &component),
    )
    .await?;
    let result = store
        .run_concurrent(async move |store| command.wasi_cli_run().call_run(store).await)
        .await??;
    assert_eq!(result, Ok(()));
    assert_eq!(
        shared::pipe_text(&stdout),
        "hello from one component, running\n"
    );
    Ok(())
}

/// `fixtures/serve_wasi_imports.js` componentized against the serve world with
/// the interfaces the runtime imports, and the component compiled from it, built
/// once.
async fn wasi_imports() -> &'static (Vec<u8>, Component) {
    static WASI_IMPORTS: OnceCell<Result<(Vec<u8>, Component), String>> = OnceCell::const_new();
    shared::build_once(&WASI_IMPORTS, async {
        let runtime = shared::runtime().expect("reading the runtime build");
        let selection = componentize::WorldSelection {
            worlds: vec!["serve"],
            builtins: Vec::new(),
            resolved: vec![componentize::runtime_imports_world(&runtime, &[])
                .expect("the runtime's imports world")],
        };
        let bytes = componentize::componentize(
            componentize::Wit::<std::path::PathBuf>::String(SERVE_WIT),
            selection,
            include_str!("fixtures/serve_wasi_imports.js"),
            &runtime,
            &Default::default(),
        )
        .await
        .expect("componentizing the serve world with the runtime's imports")
        .component;
        let component = Component::new(&ENGINE, &bytes).expect("compiling component");
        (bytes, component)
    })
    .await
}

/// The world of `--export-http` and `--export-cli` imports the interfaces the
/// runtime imports, so an application can import them, and the component
/// imports no more than the runtime does.
#[tokio::test]
async fn serve_world_imports_what_the_runtime_imports() -> anyhow::Result<()> {
    let component = &wasi_imports().await.1;
    assert_eq!(
        handle_request(component, "GET", "/").await??,
        (
            200,
            r#"{"arguments":true,"random":16,"clock":"bigint"}"#.to_string()
        )
    );

    let imports = |component: &Component| -> Vec<String> {
        let mut names: Vec<String> = component
            .component_type()
            .imports(&ENGINE)
            .map(|(name, _)| name.to_string())
            .collect();
        names.sort();
        names
    };
    assert_eq!(imports(component), imports(listener().await));
    Ok(())
}

/// With a deterministic insecure seed, the component imports no
/// `wasi:random/insecure-seed`, and `getInsecureSeed` returns the seed's high
/// and low 64 bits. Disabling `random` leaves the interface in place without
/// one. The cases run `finalize::finalize` over an already componentized
/// component again.
#[tokio::test]
async fn deterministic_insecure_seed_is_returned() -> anyhow::Result<()> {
    use componentize::finalize::WasiApiSet;
    let built = &wasi_imports().await.0;
    let finalize = |disabled: &[WasiApiSet], insecure_seed| {
        let bytes =
            componentize::finalize::finalize(built, &handler_export(), disabled, insecure_seed)?;
        anyhow::Ok(Component::new(&ENGINE, &bytes)?)
    };
    let imports = |component: &Component| -> Vec<String> {
        component
            .component_type()
            .imports(&ENGINE)
            .map(|(name, _)| name.to_string())
            .collect()
    };

    let seed = (1u128 << 64) | 2;
    for disabled in [&[][..], &[WasiApiSet::Random][..]] {
        let component = finalize(disabled, Some(seed))?;
        assert_eq!(
            handle_request(&component, "GET", "/seed").await??,
            (200, "1,2".to_string()),
            "{disabled:?}"
        );
        let imports = imports(&component);
        assert!(
            !imports
                .iter()
                .any(|name| name.starts_with("wasi:random/insecure-seed")),
            "{disabled:?}: {imports:?}"
        );
        assert_eq!(
            imports
                .iter()
                .any(|name| name.starts_with("wasi:random/random")),
            disabled.is_empty(),
            "{disabled:?}: {imports:?}"
        );
    }

    let component = finalize(&[WasiApiSet::Random], None)?;
    let imports = imports(&component);
    assert!(
        imports
            .iter()
            .any(|name| name.starts_with("wasi:random/insecure-seed")),
        "{imports:?}"
    );
    Ok(())
}

/// The world of `--export-http` and `--export-cli` leaves out the interfaces of
/// disabled features.
#[tokio::test]
async fn runtime_imports_world_leaves_out_disabled_features() -> anyhow::Result<()> {
    let runtime = shared::runtime()?;
    let names = |disabled: &[componentize::finalize::WasiApiSet]| -> anyhow::Result<Vec<String>> {
        let (resolve, world) = componentize::runtime_imports_world(&runtime, disabled)?;
        Ok(resolve.worlds[world]
            .imports
            .keys()
            .map(|key| resolve.name_world_key(key))
            .collect())
    };
    let all = names(&[])?;
    assert!(
        all.iter().any(|name| name.starts_with("wasi:filesystem/")),
        "{all:?}"
    );
    let without = names(&[componentize::finalize::WasiApiSet::Filesystem])?;
    assert!(
        !without
            .iter()
            .any(|name| name.starts_with("wasi:filesystem/")),
        "{without:?}"
    );
    assert!(
        without.iter().any(|name| name.starts_with("wasi:random/")),
        "{without:?}"
    );
    Ok(())
}

/// One instance serves several requests, as `wasmtime serve` reuses instances,
/// with the application's state kept from one to the next, and each
/// request's method and path reach the listener.
#[tokio::test]
async fn one_instance_serves_several_requests() -> anyhow::Result<()> {
    let component = listener().await;
    let mut store = store();
    let service = Service::instantiate_async(
        &mut store,
        component,
        &shared::http_linker(&ENGINE, component),
    )
    .await?;
    for (method, path, expected) in [
        ("GET", "/count", "1"),
        ("GET", "/count", "2"),
        ("GET", "/hello", "GET /hello"),
        ("POST", "/other", "POST /other"),
    ] {
        let response = shared::handle_on(&mut store, &service, method, path, "")
            .await?
            .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))?;
        assert_eq!(response, (200, expected.to_string()), "{method} {path}");
    }
    Ok(())
}

/// A listener reads the request's body.
#[tokio::test]
async fn listener_reads_the_request_body() -> anyhow::Result<()> {
    let component = listener().await;
    let mut store = store();
    let service = Service::instantiate_async(
        &mut store,
        component,
        &shared::http_linker(&ENGINE, component),
    )
    .await?;
    let response = shared::handle_on(&mut store, &service, "POST", "/echo-body", "a body")
        .await?
        .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))?;
    assert_eq!(response, (200, "a body".to_string()));
    Ok(())
}

/// A listener that does not call `respondWith` gets a network error, which is
/// sent as a `500`, and the instance serves the next request.
#[tokio::test]
async fn listener_without_respond_with_gets_500() -> anyhow::Result<()> {
    let component = listener().await;
    let mut store = store();
    let service = Service::instantiate_async(
        &mut store,
        component,
        &shared::http_linker(&ENGINE, component),
    )
    .await?;
    let response = shared::handle_on(&mut store, &service, "GET", "/no-response", "").await?;
    assert!(matches!(response, Ok((500, _))), "{response:?}");
    let response = shared::handle_on(&mut store, &service, "GET", "/count", "")
        .await?
        .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))?;
    assert_eq!(response, (200, "2".to_string()));
    Ok(())
}

/// Like [`handle_request`], for a handler that must return a response.
async fn serve_request(
    component: &Component,
    method: &str,
    path: &str,
) -> anyhow::Result<(u16, String)> {
    handle_request(component, method, path)
        .await?
        .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))
}

/// An application that implements `wasi:http/handler` itself serves the
/// component's export: its `handle` receives the request and builds the response,
/// or fails with an `error-code`, which reaches the host.
#[tokio::test]
async fn serve_dispatches_to_raw_handler() -> anyhow::Result<()> {
    let (status, body) = serve_request(raw().await, "GET", "/hello").await?;
    assert_eq!(status, 200, "the application's handler should answer 200");
    assert_eq!(
        body, "raw get /hello",
        "the application's handler should echo the method and path"
    );

    let result = handle_request(raw().await, "GET", "/fail").await?;
    assert!(
        matches!(&result, Err(ErrorCode::InternalError(Some(message))) if message == "raw get /fail"),
        "the application's handler should fail with the error naming the request; got: {result:?}"
    );
    Ok(())
}

/// The componentized serve app whose own handler returns `Response` objects,
/// built once.
async fn raw_response() -> &'static Component {
    static RAW_RESPONSE: OnceCell<Result<Component, String>> = OnceCell::const_new();
    shared::build_once(
        &RAW_RESPONSE,
        componentize_serve(include_str!("fixtures/serve_raw_response.js")),
    )
    .await
}

/// An application's own `wasi:http/handler` can return a `Response`: the one
/// `fetch` resolved to, whose body the host receives without the guest reading
/// it, one whose body a stream produces across timers, or a constructed one,
/// whose status it keeps.
#[tokio::test]
async fn raw_handler_returns_response_objects() -> anyhow::Result<()> {
    let (status, body) = serve_request(raw_response().await, "GET", "/proxy").await?;
    assert_eq!(status, 200);
    assert_eq!(
        body, UPSTREAM_BODY,
        "the fetched response is served as it is"
    );

    let (status, body) = serve_request(raw_response().await, "GET", "/slow").await?;
    assert_eq!(status, 200);
    assert_eq!(
        body, "one two three",
        "a body a stream produces across timers is sent whole"
    );

    let (status, body) = serve_request(raw_response().await, "GET", "/built").await?;
    assert_eq!(status, 201, "the constructed response keeps its status");
    assert_eq!(body, "built /built");
    Ok(())
}

/// A `Response` body that stops producing while nothing else is left for the
/// handler's event loop to do is abandoned, so the request ends instead of
/// hanging, and a `Response` whose body was read fails the call, naming the
/// reason on stderr.
#[tokio::test]
async fn stalled_and_read_response_bodies_fail_the_request() -> anyhow::Result<()> {
    let component = raw_response().await;
    let request = async |path: &str| {
        let (mut store, stderr) = store_capturing_stderr();
        let service = Service::instantiate_async(
            &mut store,
            component,
            &shared::http_linker(&ENGINE, component),
        )
        .await?;
        let result = shared::handle_on(&mut store, &service, "GET", path, "").await;
        anyhow::Ok((result, shared::pipe_text(&stderr)))
    };

    let (stalled, _) = tokio::time::timeout(std::time::Duration::from_secs(30), request("/stall"))
        .await
        .expect("a stalled body must be abandoned, not hang the request")?;
    assert!(
        !matches!(stalled, Ok(Ok((_, ref body))) if !body.is_empty()),
        "a stalled body delivers nothing: {stalled:?}"
    );

    let (used, stderr) = request("/used").await?;
    assert!(used.is_err(), "a read body must fail the call: {used:?}");
    assert!(
        stderr.contains("the Response has an unusable body: it was read, or is locked"),
        "{stderr}"
    );
    Ok(())
}

/// `fetch` takes the `wasi:http/types` `request` an application's own handler
/// receives, and sends it with its method and URL.
#[tokio::test]
async fn raw_handler_forwards_its_request_with_fetch() -> anyhow::Result<()> {
    let (status, body) = serve_request(raw_response().await, "PUT", "/echo/forwarded").await?;
    assert_eq!(status, 200);
    assert_eq!(body, "echo PUT /echo/forwarded");
    Ok(())
}

/// A listener registered after a top-level `await` on a timer is in place before
/// the snapshot, so the first request reaches it.
#[tokio::test]
async fn listener_added_after_top_level_await_serves_the_first_request() -> anyhow::Result<()> {
    let (status, body) = serve_request(listener().await, "GET", "/ready").await?;
    assert_eq!(status, 200);
    assert_eq!(body, "ready /ready");
    Ok(())
}

/// An application that both implements `wasi:http/handler` and registers a
/// `fetch` listener fails to componentize, naming the export.
#[tokio::test]
async fn serve_with_raw_handler_and_listener_is_rejected() {
    let error = shared::try_componentize(
        SERVE_WIT,
        "serve",
        include_str!("fixtures/serve_raw_and_listener.js"),
    )
    .await
    .expect_err("an application with both handlers must not componentize");
    let message = format!("{error:#}");
    assert!(
        message.contains(
            "both exports a `wasi:http/handler` implementation as `wasiHttp.handler.handle` or \
             `handler.handle` or `handle` and registers a `fetch` listener"
        ),
        "the error should name both handlers; got: {message}"
    );
}

/// An application that neither implements `wasi:http/handler` nor registers a
/// `fetch` listener fails to componentize, since its component could not serve
/// any request.
#[tokio::test]
async fn serve_without_handler_is_rejected() {
    let error = shared::try_componentize(
        SERVE_WIT,
        "serve",
        include_str!("fixtures/serve_no_listener.js"),
    )
    .await
    .expect_err("an application with no handler must not componentize");
    let message = format!("{error:#}");
    assert!(
        message.contains(
            "neither exports a `wasi:http/handler` implementation as `wasiHttp.handler.handle` \
             or `handler.handle` or `handle` nor registers a `fetch` listener"
        ),
        "the error should name both ways to serve requests; got: {message}"
    );
}

/// A world importing `wasi:http/types`, declaring `fields`' `get` and `has`,
/// which the runtime's declaration lacks, and an interface whose `async`
/// function borrows a `request`.
const IMPORTS_WIT: &str = "\
package test:serve-imports;

interface peeker {
  use wasi:http/types@0.3.0.{request};
  peek: async func(r: borrow<request>) -> u32;
}

world serve {
  import wasi:http/types@0.3.0;
  import peeker;
  export wasi:http/handler@0.3.0;
}

package wasi:http@0.3.0 {
  interface types {
    type field-name = string;
    type field-value = list<u8>;
    resource fields {
      get: func(name: field-name) -> list<field-value>;
      has: func(name: field-name) -> bool;
    }
    resource request;
  }
  interface handler {}
}
";

/// A raw handler for [`IMPORTS_WIT`]. For `/lend`, it `fetch`es the request
/// while `peek` borrows it, and responds with the outcome. Otherwise it
/// responds with what `fields`' `get` and `has` return for the request's headers.
const IMPORTS_JS: &str = "\
import { peek } from 'test:serve-imports/peeker';
export const handler = {
  async handle(request) {
    if (request.getPathWithQuery() === '/lend') {
      const peeked = peek(request);
      let outcome;
      try {
        await fetch(request);
        outcome = 'fetched';
      } catch (e) {
        outcome = `threw ${e.constructor.name}: ${e.message}`;
      }
      await peeked;
      return new Response(outcome);
    }
    const headers = request.getHeaders();
    const [length] = headers.get('content-length');
    const absent = headers.has('x-absent');
    return new Response(`content-length=${new TextDecoder().decode(length)} absent=${absent}`);
  },
};
";

/// [`IMPORTS_JS`] componentized against [`IMPORTS_WIT`], built once.
async fn imports_app() -> &'static Component {
    static IMPORTS: OnceCell<Result<Component, String>> = OnceCell::const_new();
    shared::build_once(
        &IMPORTS,
        shared::compile_fixture(&ENGINE, IMPORTS_WIT, "serve", IMPORTS_JS),
    )
    .await
}

/// Send a `GET` request to `path` to a fresh instance of [`imports_app`], whose
/// `peek` returns on the second time it is polled, so the call does not return
/// before the guest's next turn.
async fn serve_imports_app(path: &str) -> anyhow::Result<(u16, String)> {
    let component = imports_app().await;
    let linker = shared::http_linker_with(&ENGINE, component, &["test:serve-imports/"], |linker| {
        linker
                .instance("test:serve-imports/peeker")?
                .func_wrap_concurrent(
                    "peek",
                    |_accessor,
                     (_request,): (
                        wasmtime::component::Resource<wasmtime_wasi_http::p3::Request>,
                    )| {
                        Box::pin(async move {
                            tokio::task::yield_now().await;
                            Ok((1u32,))
                        })
                    },
                )
    });
    let (mut store, stderr) = store_capturing_stderr();
    let service = Service::instantiate_async(&mut store, component, &linker).await?;
    shared::handle_on(&mut store, &service, "GET", path, "")
        .await?
        .map_err(|code| anyhow::anyhow!("{code:?}: {}", shared::pipe_text(&stderr)))
}

/// The runtime's `wasi:http/types` has only the functions its builtins call, and a
/// world importing the interface adds the functions it declares: here `fields`'
/// `get` and `has`, which a raw handler calls on the request's headers.
#[tokio::test]
async fn world_imports_extend_the_runtime_wasi_http_types() -> anyhow::Result<()> {
    assert_eq!(
        serve_imports_app("/").await?,
        (200, "content-length=0 absent=false".to_string())
    );
    Ok(())
}

/// Disabling `http` for a component that exports `wasi:http/handler` is refused,
/// once, saying the component exports the interface that uses it. The case runs
/// `finalize::finalize` over an already componentized component again.
#[tokio::test]
async fn disabling_http_for_a_serve_component_is_refused() {
    let error = componentize::finalize::finalize(
        &listener_build().await.0,
        &handler_export(),
        &[componentize::finalize::WasiApiSet::Http],
        None,
    )
    .expect_err("a serve component cannot go without `http`");
    let message = format!("{error:#}");
    assert!(
        message.contains("is used by `wasi:http/handler@0.3.0`, which the component exports"),
        "{message}"
    );
    assert_eq!(message.matches("is used by").count(), 1, "{message}");
    assert!(!message.contains("Disable"), "{message}");
}

/// A failing `handle` is named `wasi:http/handler#handle` in the message, not by
/// the export name the componentizer gives it internally.
#[tokio::test]
async fn failing_raw_handler_is_named_as_the_handler() -> anyhow::Result<()> {
    let component = raw().await;
    let (mut store, stderr) = store_capturing_stderr();
    let service = Service::instantiate_async(
        &mut store,
        component,
        &shared::http_linker(&ENGINE, component),
    )
    .await?;
    let result = shared::handle_on(&mut store, &service, "GET", "/throw", "").await;
    assert!(
        !matches!(result, Ok(Ok(_))),
        "a throwing handler cannot answer"
    );
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("export `wasi:http/handler#handle` threw: boom from handle"),
        "{stderr}"
    );
    Ok(())
}

/// A world declaring `wasi:http/handler` differently from the runtime fails to
/// componentize, saying it must match the runtime's declaration.
#[tokio::test]
async fn mismatched_wasi_http_declaration_is_rejected() {
    const WIT: &str = "\
package test:serve-mismatch;

world serve {
  export wasi:http/handler@0.3.0;
}

package wasi:http@0.3.0 {
  interface handler {
    handle: func(n: u32) -> u32;
  }
}
";
    let error = shared::try_componentize(WIT, "serve", include_str!("fixtures/serve.js"))
        .await
        .expect_err("a mismatched `wasi:http` declaration must not componentize");
    let message = format!("{error:#}");
    assert!(
        message.contains("must declare it as the runtime does"),
        "{message}"
    );
}

/// `fetch` given a request wrapper an `async` import call still borrows rejects
/// with a `TypeError`, rather than moving the request while it is lent.
#[tokio::test]
async fn fetch_of_a_lent_request_rejects() -> anyhow::Result<()> {
    assert_eq!(
        serve_imports_app("/lend").await?,
        (
            200,
            "threw TypeError: the resource is lent to an import call that has not returned"
                .to_string()
        )
    );
    Ok(())
}

/// A future a `fetch` listener passes to an import is written even when the host
/// reads it only after the request has finished.
#[tokio::test]
async fn listener_future_is_written_after_the_request() -> anyhow::Result<()> {
    const WIT: &str = "\
package test:serve-future;

interface sink {
  accept: func(f: future<u32>);
}

world serve {
  import sink;
  export wasi:http/handler@0.3.0;
}

package wasi:http@0.3.0 {
  interface handler {}
}
";
    const JS: &str = "\
import { accept } from 'test:serve-future/sink';
addEventListener('fetch', (event) => {
  accept(Promise.resolve(7));
  event.respondWith(new Response('ok'));
});
";
    static STASH: std::sync::Mutex<Vec<wasmtime::component::FutureReader<u32>>> =
        std::sync::Mutex::new(Vec::new());
    let component = shared::compile_fixture(&ENGINE, WIT, "serve", JS).await;
    let linker = shared::http_linker_with(&ENGINE, &component, &["test:serve-future/"], |linker| {
        linker.instance("test:serve-future/sink")?.func_wrap(
            "accept",
            |_store, (f,): (wasmtime::component::FutureReader<u32>,)| {
                STASH.lock().unwrap().push(f);
                Ok(())
            },
        )
    });
    let (mut store, stderr) = store_capturing_stderr();
    let service = Service::instantiate_async(&mut store, &component, &linker).await?;
    let (status, _) = shared::handle_on(&mut store, &service, "GET", "/", "")
        .await?
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    assert_eq!(status, 200);
    // Let the request's background drain run before the future is read. The read
    // succeeds whether or not the drain has finished by then.
    store
        .run_concurrent(async |_store| {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        })
        .await?;
    let reader = STASH.lock().unwrap().pop().expect("accept was called");
    let read = store
        .run_concurrent(async |store| shared::read_future(store, reader).await)
        .await;
    assert_eq!(read??, 7, "{}", shared::pipe_text(&stderr));
    Ok(())
}

/// A `fetch` listener's rejection that nothing handles is reported during the
/// request, even when the listener waits on nothing.
#[tokio::test]
async fn listener_unhandled_rejection_is_reported() -> anyhow::Result<()> {
    let component = listener().await;
    let (mut store, stderr) = store_capturing_stderr();
    let service = Service::instantiate_async(
        &mut store,
        component,
        &shared::http_linker(&ENGINE, component),
    )
    .await?;
    let (status, _) = shared::handle_on(&mut store, &service, "GET", "/reject", "")
        .await?
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    assert_eq!(status, 200);
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("Uncaught (in promise) listener rejection"),
        "{stderr}"
    );
    Ok(())
}

/// An object that is not a `Response` passed where an import takes an owned
/// `response` throws a `TypeError` the guest can catch.
#[tokio::test]
async fn plain_object_for_an_owned_response_throws() -> anyhow::Result<()> {
    let (status, body) = serve_request(raw_response().await, "GET", "/plain-object").await?;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        "threw TypeError: argument 1 of `Response.consumeBody`: expected an instance of the \
         imported resource's class, got an object"
    );
    Ok(())
}

/// A `Response` whose body was read, is locked, or was sent already, passed
/// where an import takes an owned `response`, throws a `TypeError` the guest can
/// catch.
#[tokio::test]
async fn unusable_response_for_an_owned_response_throws() -> anyhow::Result<()> {
    let (status, body) = serve_request(raw_response().await, "GET", "/unusable").await?;
    assert_eq!(status, 200);
    let unusable = "TypeError: argument 1 of `Response.consumeBody`: the Response has an \
                    unusable body: it was read, or is locked";
    assert_eq!(
        body,
        format!("read: {unusable}\nlocked: {unusable}\nfetched: sent\nagain: {unusable}")
    );
    Ok(())
}
