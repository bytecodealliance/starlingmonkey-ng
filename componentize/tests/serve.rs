// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// `wasi:http/handler` (serve) via the componentize pipeline, end to end: apps
// served by the runtime's native handler and apps implementing `handle`.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// This componentizes serve applications against the serve world
// (`fixtures/serve.wit`), which exports only `wasi:http/handler`. The component
// serves that export through one of two implementations, picked at Wizer time
// by `init`: the runtime's native handler, which dispatches each request as a
// `fetch` event to the application's listeners, or the application's own
// implementation of the interface. It then drives the component's
// `wasi:http/handler.handle()` through wasmtime's p3 `wasi:http` `Service`
// bindings and asserts:
//
//   - LISTENER (`fixtures/serve.js`, `addEventListener('fetch', …)`): a request
//     reaches the baked `fetch` listener WITHOUT re-initializing the runtime. The
//     listener awaits a real `setTimeout` (a genuine per-request event-loop
//     suspension) and returns `200` with a body echoing the request method and
//     path. A real `200` with the exact body proves the baked dispatch path: if
//     the runtime's handler had instead fallen to the script-path branch (runtime
//     not initialized), it would read a non-existent script from argv and return
//     `500`. So body == "GET /hello" only happens on the correct baked path.
//   - RAW (`fixtures/serve_raw.js`, `export const wasiHttp`): a request
//     reaches the application's own `handle`, which responds with a `200` naming
//     the method and path it read from the request, or fails with an
//     `internal-error` naming them.
//   - AFTER AWAIT (`fixtures/serve_after_await.js`): a listener registered after
//     a top-level `await` on a timer serves the first request.
//   - Componentizing fails for an application that does both
//     (`fixtures/serve_raw_and_listener.js`) or neither
//     (`fixtures/serve_no_listener.js`).
//   - CLI AND SERVE (`fixtures/cli_and_serve.js`): built with `--cli --serve`
//     through the command line, one component serves a request through its
//     listener and, in another instance, runs its `run` export.
//
// A successful `Service::instantiate_async` is itself the structural proof that
// the component's `wasi:http/handler` export survived linking, Wizer and
// finalization: the bindings require that export to exist.
//
// Componentizing an app takes seconds, so each app is built once and shared via
// a `OnceCell`, and each test gets a fresh `Store`.

mod shared;

use shared::HttpCtx as Ctx;

use {
    std::path::PathBuf,
    tokio::sync::OnceCell,
    wasmtime::{
        component::{Component, Linker},
        Engine, Store,
    },
    wasmtime_wasi_http::p3::bindings::{http::types::ErrorCode, Service},
};

/// The body of the harness's response to every outgoing request.
const UPSTREAM_BODY: &str = "from upstream";

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// Componentize a serve application against the serve world.
async fn try_componentize_serve(js: &'static str) -> anyhow::Result<Vec<u8>> {
    let runtime = shared::runtime().expect("reading the runtime build");
    componentize::componentize(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/serve.wit")),
        Some("serve"),
        &[],
        false,
        js,
        None::<PathBuf>,
        &runtime,
        // The serve world has no imports, so the default linker (WASI plus trapping
        // stubs) is fine for the init instantiation under Wizer.
        &[],
        None,
    )
    .await
}

/// Componentize a serve application against the serve world and compile it.
async fn componentize_serve(js: &'static str) -> Component {
    let component_bytes = try_componentize_serve(js)
        .await
        .expect("componentizing the serve world");
    Component::new(&ENGINE, &component_bytes).expect("compiling component")
}

/// The componentized serve app that registers a `fetch` listener, built once.
async fn listener() -> &'static Component {
    static LISTENER: OnceCell<Result<Component, String>> = OnceCell::const_new();
    shared::build_once(
        &LISTENER,
        componentize_serve(include_str!("fixtures/serve.js")),
    )
    .await
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

/// `--cli` and `--serve` together build one component that serves requests
/// and runs as a command, from a module that registers a `fetch` listener and
/// exports `run`. It is built through the command line, so the flags are
/// exercised as given, against the static runtime only.
#[tokio::test]
async fn cli_and_serve_component_serves_and_runs() -> anyhow::Result<()> {
    if shared::dynamic_link_mode() {
        return Ok(());
    }
    let dir = std::env::temp_dir().join(format!("starling-cli-and-serve-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let output = dir.join("both.wasm");
    let args: Vec<std::ffi::OsString> = vec![
        "starling-componentize".into(),
        "-q".into(),
        "--cli".into(),
        "--serve".into(),
        "componentize".into(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cli_and_serve.js")
            .into(),
        "-o".into(),
        output.clone().into(),
        "--runtime".into(),
        shared::static_runtime_path().into(),
    ];
    // `command::run` starts a Tokio runtime of its own, which cannot run on
    // this test's.
    std::thread::spawn(move || componentize::command::run(args, None))
        .join()
        .expect("componentizing did not panic")?;
    let component = Component::new(&ENGINE, std::fs::read(&output)?)?;
    std::fs::remove_dir_all(&dir)?;

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

/// The world of `--serve` and `--cli` imports the interfaces the runtime
/// imports, so an application can import them, and the component imports no
/// more than the runtime does.
#[tokio::test]
async fn serve_world_imports_what_the_runtime_imports() -> anyhow::Result<()> {
    let runtime = shared::runtime()?;
    let selection = componentize::WorldSelection {
        worlds: vec!["serve"],
        builtins: Vec::new(),
        resolved: vec![componentize::runtime_imports_world(&runtime, &[])?],
    };
    let bytes = componentize::componentize(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/serve.wit")),
        selection,
        &[],
        false,
        include_str!("fixtures/serve_wasi_imports.js"),
        None::<PathBuf>,
        &runtime,
        &[],
        None,
    )
    .await?;
    let component = Component::new(&ENGINE, &bytes)?;
    assert_eq!(
        handle_request(&component, "GET", "/").await??,
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
    assert_eq!(imports(&component), imports(listener().await));
    Ok(())
}

/// With a deterministic insecure seed, the component imports no
/// `wasi:random/insecure-seed`, and `getInsecureSeed` returns the seed's high
/// and low 64 bits. Disabling `random` leaves the interface in place without
/// one.
#[tokio::test]
async fn deterministic_insecure_seed_is_returned() -> anyhow::Result<()> {
    use componentize::finalize::WASIAPISet;
    const JS: &str = "\
        import { getInsecureSeed } from 'wasi:random/insecure-seed@0.3.0';\n\
        addEventListener('fetch', (event) => {\n\
          const [high, low] = getInsecureSeed();\n\
          event.respondWith(new Response(`${high},${low}`));\n\
        });";
    let runtime = shared::runtime()?;
    let build = |disabled: &'static [WASIAPISet], insecure_seed| {
        let runtime = &runtime;
        async move {
            let selection = componentize::WorldSelection {
                worlds: vec!["serve"],
                builtins: Vec::new(),
                resolved: vec![componentize::runtime_imports_world(runtime, disabled)?],
            };
            let componentized = componentize::componentize_with_output(
                componentize::Wit::<PathBuf>::String(include_str!("fixtures/serve.wit")),
                selection,
                &[],
                false,
                JS,
                None::<PathBuf>,
                runtime,
                disabled,
                insecure_seed,
                false,
                false,
                None,
                false,
                None,
                true,
            )
            .await?;
            anyhow::Ok(Component::new(&ENGINE, &componentized.component)?)
        }
    };
    let imports = |component: &Component| -> Vec<String> {
        component
            .component_type()
            .imports(&ENGINE)
            .map(|(name, _)| name.to_string())
            .collect()
    };

    let seed = (1u128 << 64) | 2;
    for disabled in [&[][..], &[WASIAPISet::Random][..]] {
        let component = build(disabled, Some(seed)).await?;
        assert_eq!(
            handle_request(&component, "GET", "/").await??,
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

    let component = build(&[WASIAPISet::Random], None).await?;
    let imports = imports(&component);
    assert!(
        imports
            .iter()
            .any(|name| name.starts_with("wasi:random/insecure-seed")),
        "{imports:?}"
    );
    Ok(())
}

/// The world of `--serve` and `--cli` leaves out the interfaces of disabled
/// features.
#[tokio::test]
async fn runtime_imports_world_leaves_out_disabled_features() -> anyhow::Result<()> {
    let runtime = shared::runtime()?;
    let names = |disabled: &[componentize::finalize::WASIAPISet]| -> anyhow::Result<Vec<String>> {
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
    let without = names(&[componentize::finalize::WASIAPISet::Filesystem])?;
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
/// with the application's state carried from one to the next.
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
    for expected in ["1", "2"] {
        let response = shared::handle_on(&mut store, &service, "GET", "/count", "")
            .await?
            .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))?;
        assert_eq!(response, (200, expected.to_string()));
    }
    let response = shared::handle_on(&mut store, &service, "GET", "/hello", "")
        .await?
        .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))?;
    assert_eq!(response, (200, "GET /hello".to_string()));
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

/// [`handle_request`] for a handler that must return a response.
async fn serve_request(
    component: &Component,
    method: &str,
    path: &str,
) -> anyhow::Result<(u16, String)> {
    handle_request(component, method, path)
        .await?
        .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))
}

/// POSITIVE: a serve app that registers a `fetch` listener serves a real request
/// from the baked listener, without re-initializing the runtime, returning `200`
/// and a body echoing the request method and path. The body only appears on the
/// baked-dispatch path (the script-path fallback, with no argv script, would
/// `500`), so it proves the runtime stood up at Wizer time, the listener was
/// wizened, and the per-request event loop drove the `setTimeout` turn.
#[tokio::test]
async fn serve_dispatches_to_fetch_listener() -> anyhow::Result<()> {
    let (status, body) = serve_request(listener().await, "GET", "/hello").await?;
    assert_eq!(status, 200, "the wizened fetch listener should answer 200");
    assert_eq!(
        body, "GET /hello",
        "the listener should echo the request method and path into the body"
    );
    Ok(())
}

/// `fixtures/serve.js` produces the same responses when the runtime component
/// reads it from the path `STARLINGMONKEY_CONFIG` names, without componentizing
/// it.
#[tokio::test]
async fn script_path_serves_like_the_component() -> anyhow::Result<()> {
    if shared::dynamic_link_mode() {
        eprintln!("skipped: the dynamic link mode has no runtime component to serve");
        return Ok(());
    }
    let component = Component::new(&ENGINE, std::fs::read(shared::static_runtime_path())?)?;
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let wasi = shared::Wasi::from_builder(
        wasmtime_wasi::WasiCtxBuilder::new()
            .inherit_stdout()
            .inherit_stderr()
            .env("STARLINGMONKEY_CONFIG", "serve.js")
            .preopened_dir(&fixtures, ".", wasmtime_wasi::FsPerms::ReadOnly)?,
    );
    let mut store = Store::new(
        &ENGINE,
        Ctx {
            wasi,
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: shared::UpstreamHooks {
                body: UPSTREAM_BODY,
            },
        },
    );
    let service = Service::instantiate_async(
        &mut store,
        &component,
        &shared::http_linker(&ENGINE, &component),
    )
    .await?;
    let mut script = Vec::new();
    let mut componentized = Vec::new();
    for path in ["/hello", "/count", "/count"] {
        script.push(
            shared::handle_on(&mut store, &service, "GET", path, "")
                .await?
                .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))?,
        );
    }
    let mut store = self::store();
    let service = Service::instantiate_async(
        &mut store,
        listener().await,
        &shared::http_linker(&ENGINE, listener().await),
    )
    .await?;
    for path in ["/hello", "/count", "/count"] {
        componentized.push(
            shared::handle_on(&mut store, &service, "GET", path, "")
                .await?
                .map_err(|code| anyhow::anyhow!("handler errored: {code:?}"))?,
        );
    }
    assert_eq!(script, componentized);
    assert_eq!(script[0], (200, "GET /hello".to_string()));
    Ok(())
}

/// The positive handler runs per request against the same baked snapshot: a second
/// request with a different method and path produces its own body, proving the
/// runtime is reused (not re-bootstrapped) and each request gets a fresh per-call
/// event loop.
#[tokio::test]
async fn serve_handles_a_second_request() -> anyhow::Result<()> {
    let (status, body) = serve_request(listener().await, "POST", "/other").await?;
    assert_eq!(status, 200, "the wizened fetch listener should answer 200");
    assert_eq!(
        body, "POST /other",
        "a second request must dispatch to the same listener with its own method and path"
    );
    Ok(())
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
/// hanging, and a `Response` whose body was read fails the call.
#[tokio::test]
async fn raw_handler_response_body_failures() -> anyhow::Result<()> {
    let stalled = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        handle_request(raw_response().await, "GET", "/stall"),
    )
    .await
    .expect("a stalled body must be abandoned, not hang the request");
    assert!(
        !matches!(stalled, Ok(Ok((_, ref body))) if !body.is_empty()),
        "a stalled body delivers nothing: {stalled:?}"
    );

    let used = handle_request(raw_response().await, "GET", "/used").await;
    assert!(used.is_err(), "a read body must fail the call: {used:?}");
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
    let component = componentize_serve(include_str!("fixtures/serve_after_await.js")).await;
    let (status, body) = serve_request(&component, "GET", "/first").await?;
    assert_eq!(status, 200);
    assert_eq!(body, "ready /first");
    Ok(())
}

/// An application that both implements `wasi:http/handler` and registers a
/// `fetch` listener fails to componentize, naming the export.
#[tokio::test]
async fn serve_with_raw_handler_and_listener_is_rejected() {
    let error = try_componentize_serve(include_str!("fixtures/serve_raw_and_listener.js"))
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
    let error = try_componentize_serve(include_str!("fixtures/serve_no_listener.js"))
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

/// The runtime's `wasi:http/types` has only the functions its builtins call, and a
/// world importing the interface adds the functions it declares: here `fields`'
/// `get` and `has`, which a raw handler calls on the request's headers.
#[tokio::test]
async fn world_imports_extend_the_runtime_wasi_http_types() -> anyhow::Result<()> {
    const WIT: &str = "\
package test:serve-fields;

world serve {
  import wasi:http/types@0.3.0;
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
  }
  interface handler {}
}
";
    const JS: &str = "\
export const handler = {
  handle(request) {
    const headers = request.getHeaders();
    const [length] = headers.get('content-length');
    const absent = headers.has('x-absent');
    return new Response(`content-length=${new TextDecoder().decode(length)} absent=${absent}`);
  },
};
";
    let runtime = shared::runtime().expect("reading the runtime build");
    let component_bytes = componentize::componentize(
        componentize::Wit::<PathBuf>::String(WIT),
        Some("serve"),
        &[],
        false,
        JS,
        None::<PathBuf>,
        &runtime,
        &[],
        None,
    )
    .await?;
    let component = Component::new(&ENGINE, &component_bytes)?;
    let (status, body) = serve_request(&component, "GET", "/").await?;
    assert_eq!(status, 200);
    assert_eq!(body, "content-length=0 absent=false");
    Ok(())
}

/// Disabling `http` for a component that exports `wasi:http/handler` is refused,
/// once, saying the component exports the interface that uses it.
#[tokio::test]
async fn disabling_http_for_a_serve_component_is_refused() {
    let runtime = shared::runtime().expect("reading the runtime build");
    let error = componentize::componentize(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/serve.wit")),
        Some("serve"),
        &[],
        false,
        include_str!("fixtures/serve.js"),
        None::<PathBuf>,
        &runtime,
        &[componentize::finalize::WASIAPISet::Http],
        None,
    )
    .await
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
    let component = componentize_serve(include_str!("fixtures/serve_raw_throws.js")).await;
    let (wasi, stderr) = shared::wasi_capturing_stderr();
    let mut store = Store::new(
        &ENGINE,
        Ctx {
            wasi,
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: shared::UpstreamHooks {
                body: UPSTREAM_BODY,
            },
        },
    );
    let service = Service::instantiate_async(
        &mut store,
        &component,
        &shared::http_linker(&ENGINE, &component),
    )
    .await?;
    let result = shared::handle_on(&mut store, &service, "GET", "/", "").await;
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
    let runtime = shared::runtime().expect("reading the runtime build");
    let error = componentize::componentize(
        componentize::Wit::<PathBuf>::String(WIT),
        Some("serve"),
        &[],
        false,
        include_str!("fixtures/serve.js"),
        None::<PathBuf>,
        &runtime,
        &[],
        None,
    )
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
    const WIT: &str = "\
package test:serve-lend;

interface peeker {
  use wasi:http/types@0.3.0.{request};
  peek: async func(r: borrow<request>) -> u32;
}

world serve {
  import peeker;
  export wasi:http/handler@0.3.0;
}

package wasi:http@0.3.0 {
  interface types {
    resource request;
  }
  interface handler {}
}
";
    const JS: &str = "\
import { peek } from 'test:serve-lend/peeker';
export const handler = {
  async handle(request) {
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
  },
};
";
    let runtime = shared::runtime().expect("reading the runtime build");
    let component_bytes = componentize::componentize(
        componentize::Wit::<PathBuf>::String(WIT),
        Some("serve"),
        &[],
        false,
        JS,
        None::<PathBuf>,
        &runtime,
        &[],
        None,
    )
    .await?;
    let component = Component::new(&ENGINE, &component_bytes)?;
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker).expect("wasi linker");
    wasmtime_wasi_http::p3::add_to_linker(&mut linker).expect("wasi:http p3 linker");
    linker
        .instance("test:serve-lend/peeker")?
        .func_wrap_concurrent(
        "peek",
        |_accessor,
         (_request,): (wasmtime::component::Resource<wasmtime_wasi_http::p3::Request>,)| {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok((1u32,))
            })
        },
    )?;
    componentize::trap_unsatisfied_imports(
        &ENGINE,
        &component,
        &mut linker,
        &["wasi:http/", "test:serve-lend/"],
    )?;
    let (wasi, stderr) = shared::wasi_capturing_stderr();
    let mut store = Store::new(
        &ENGINE,
        Ctx {
            wasi,
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: shared::UpstreamHooks {
                body: UPSTREAM_BODY,
            },
        },
    );
    let service = Service::instantiate_async(&mut store, &component, &linker).await?;
    let (status, body) = shared::handle_on(&mut store, &service, "GET", "/", "")
        .await?
        .map_err(|code| anyhow::anyhow!("{code:?}: {}", shared::pipe_text(&stderr)))?;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        "threw TypeError: the resource is lent to an import call that has not returned"
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
    let runtime = shared::runtime().expect("reading the runtime build");
    let component_bytes = componentize::componentize(
        componentize::Wit::<PathBuf>::String(WIT),
        Some("serve"),
        &[],
        false,
        JS,
        None::<PathBuf>,
        &runtime,
        &[],
        None,
    )
    .await?;
    let component = Component::new(&ENGINE, &component_bytes)?;
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker).expect("wasi linker");
    wasmtime_wasi_http::p3::add_to_linker(&mut linker).expect("wasi:http p3 linker");
    linker.instance("test:serve-future/sink")?.func_wrap(
        "accept",
        |_store, (f,): (wasmtime::component::FutureReader<u32>,)| {
            STASH.lock().unwrap().push(f);
            Ok(())
        },
    )?;
    componentize::trap_unsatisfied_imports(
        &ENGINE,
        &component,
        &mut linker,
        &["wasi:http/", "test:serve-future/"],
    )?;
    let (wasi, stderr) = shared::wasi_capturing_stderr();
    let mut store = Store::new(
        &ENGINE,
        Ctx {
            wasi,
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: shared::UpstreamHooks {
                body: UPSTREAM_BODY,
            },
        },
    );
    let service = Service::instantiate_async(&mut store, &component, &linker).await?;
    let (status, _) = shared::handle_on(&mut store, &service, "GET", "/", "")
        .await?
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    assert_eq!(status, 200);
    // Let the request's background drain run to its end.
    store
        .run_concurrent(async |_store| {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        })
        .await?;
    let reader = STASH.lock().unwrap().pop().expect("accept was called");
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<u32>));
    let read = store
        .run_concurrent(async |store| {
            let slot2 = slot.clone();
            store.with(|store| reader.pipe(store, SlotConsumer { slot: slot2 }))?;
            for _ in 0..200 {
                if slot.lock().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            anyhow::Ok(*slot.lock().unwrap())
        })
        .await;
    assert_eq!(read??, Some(7), "{}", shared::pipe_text(&stderr));
    Ok(())
}

/// A `FutureConsumer<u32>` that stores the value it reads.
struct SlotConsumer {
    slot: std::sync::Arc<std::sync::Mutex<Option<u32>>>,
}

impl<D> wasmtime::component::FutureConsumer<D> for SlotConsumer {
    type Item = u32;
    fn poll_consume(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, D>,
        mut source: wasmtime::component::Source<'_, Self::Item>,
        _finish: bool,
    ) -> std::task::Poll<wasmtime::Result<()>> {
        use wasmtime::AsContextMut as _;
        let mut buf: Vec<u32> = Vec::with_capacity(1);
        source.read(store.as_context_mut(), &mut buf)?;
        *self.slot.lock().unwrap() = buf.into_iter().next();
        std::task::Poll::Ready(Ok(()))
    }
}

/// A `fetch` listener's rejection that nothing handles is reported during the
/// request, even when the listener waits on nothing.
#[tokio::test]
async fn listener_unhandled_rejection_is_reported() -> anyhow::Result<()> {
    let component = componentize_serve(
        "addEventListener('fetch', (e) => { \
           Promise.reject(new Error('listener rejection')); \
           e.respondWith(new Response('ok')); \
         });",
    )
    .await;
    let (wasi, stderr) = shared::wasi_capturing_stderr();
    let mut store = Store::new(
        &ENGINE,
        Ctx {
            wasi,
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: shared::UpstreamHooks {
                body: UPSTREAM_BODY,
            },
        },
    );
    let service = Service::instantiate_async(
        &mut store,
        &component,
        &shared::http_linker(&ENGINE, &component),
    )
    .await?;
    let (status, _) = shared::handle_on(&mut store, &service, "GET", "/", "")
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
    let component = componentize_serve(
        "import { Response as WasiResponse } from 'wasi:http/types@0.3.0';
export async function handle(request) {
  let outcome;
  try { WasiResponse.consumeBody({}, Promise.resolve()); outcome = 'returned'; }
  catch (e) { outcome = `threw ${e.constructor.name}: ${e.message}`; }
  return new Response(outcome);
}",
    )
    .await;
    let (status, body) = handle_request(&component, "GET", "/")
        .await?
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
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
    let component = componentize_serve(
        "import { Response as WasiResponse } from 'wasi:http/types@0.3.0';
function attempt(out, label, response) {
  try { WasiResponse.consumeBody(response, Promise.resolve()); out.push(`${label}: sent`); }
  catch (e) { out.push(`${label}: ${e.constructor.name}: ${e.message}`); }
}
export async function handle(request) {
  const out = [];
  const read = new Response('read');
  await read.text();
  attempt(out, 'read', read);
  const locked = new Response('locked');
  locked.body.getReader();
  attempt(out, 'locked', locked);
  const fetched = await fetch('http://upstream.example/data');
  attempt(out, 'fetched', fetched);
  attempt(out, 'again', fetched);
  await new Promise((resolve) => setTimeout(resolve, 5));
  return new Response(out.join('\\n'));
}",
    )
    .await;
    let (status, body) = handle_request(&component, "GET", "/")
        .await?
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    assert_eq!(status, 200);
    let unusable = "TypeError: argument 1 of `Response.consumeBody`: the Response has an \
                    unusable body: it was read, or is locked";
    assert_eq!(
        body,
        format!("read: {unusable}\nlocked: {unusable}\nfetched: sent\nagain: {unusable}")
    );
    Ok(())
}
