// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// An async export combining builtins, and two async export calls in flight at
// once.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// This componentizes ONE world (`parity-demo`, see `fixtures/parity_demo.wit` +
// `fixtures/parity_demo.js`) whose `async` export `run` uses builtins under the
// single per-call event loop the async export drives. It then:
//
//   - calls `run(ms)` under a wasmtime async `Store`, asserting the combined
//     string reflecting BOTH a timer turn (`setTimeout`) and a `ReadableStream`
//     drain (web-streams builtin), so a bug in either leg changes the output.
//   - launches two `delayed(tag, ms)` calls concurrently on the same store
//     (`futures::join` inside one `run_concurrent`), with equal delays and
//     distinct tags. Each returns its own tag, and the first to finish counts
//     both calls in flight, which a serialized run would not.
//
// The fetch leg rounds out `run`: the wasm fetch backend builds its request body
// via wasip3 `wit_stream`/`wit_future`, whose canonical stream/future intrinsics
// stay live in the PIC dylib because their vtables store local-wrapper addresses
// (the wit-bindgen vtable wrapper fix). The
// p3 `wasi:http` host wired below intercepts every outgoing request and responds
// with a fixed in-memory body ([`SERVED_BODY`]), so the body round-trips back
// into `run`'s result with no real network egress.
//
// Componentizing a world takes seconds, so the component is built once and
// shared via a `OnceCell` `Pre`, and each test gets a fresh `Store`.

mod shared;

use shared::HttpCtx as Ctx;

use {
    tokio::sync::OnceCell,
    wasmtime::{component::Linker, Engine, Store},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/parity_demo.wit",
    world: "parity-demo",
    imports: { default: async },
    exports: { default: async },
});

/// The fixed body the harness serves for every outgoing `fetch`. The guest
/// reads it back via `response.text()`, so it lands verbatim in `run`'s result.
const SERVED_BODY: &str = "hello-from-host";

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// Componentize the parity-demo world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static ParityDemoPre<Ctx> {
    static PRE: OnceCell<Result<ParityDemoPre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/parity_demo.wit"),
            "parity-demo",
            include_str!("fixtures/parity_demo.js"),
        )
        .await;

        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker).expect("wasi linker");
        // The async export drives a per-call event loop whose `setTimeout` sleeps
        // on the WASIp3 monotonic clock (`wasi:clocks/monotonic-clock`), and whose
        // `fetch` reaches the WASIp3 outgoing HTTP client
        // (`wasi:http/client@0.3.0`). Both are real host imports the
        // loop's driver awaits, so the host MUST satisfy them.
        wasmtime_wasi_http::p3::add_to_linker(&mut linker).expect("wasi:http p3 linker");
        // The wizened component still declares the runtime's other transitive
        // imports, since Wizer does not strip uncalled imports, so trap-stub the rest.
        componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &["wasi:http/"])
            .expect("trap-stub unknown imports");

        ParityDemoPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("ParityDemoPre")
    })
    .await
}

fn store() -> Store<Ctx> {
    shared::http_store(&ENGINE, SERVED_BODY)
}

/// The combined-builtins parity export: `run(ms)` sleeps `ms` on a timer, then
/// drains a literal `ReadableStream`, and returns a string combining both legs.
/// The result is deterministic, and a bug in either leg changes it.
#[tokio::test]
async fn combined_builtins_export_runs() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let result = store
        .run_concurrent(async |accessor| {
            instance.test_parity_demo_demo().call_run(accessor, 1).await
        })
        .await??;
    assert_eq!(
        result, "timer=1 stream=part-apart-bpart-c fetch=hello-from-host",
        "run(1) must combine the timer delay, the drained ReadableStream chunks, \
         and the body fetched through the WASIp3 HTTP client"
    );
    Ok(())
}

/// Two `delayed` calls in flight at once on the same store, with equal delays
/// and distinct tags, each return their own tag, so the per-call event loops do
/// not mix up their work. The first to finish counts both calls in flight, which
/// a serialized run would not.
#[tokio::test]
async fn concurrent_async_exports_interleave() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;

    let (first, second) = store
        .run_concurrent(async |accessor| {
            let demo = instance.test_parity_demo_demo();
            // `join` polls both calls at once on the one accessor, so their
            // guest async export calls, and the per-call event loops each
            // drives, overlap.
            futures::future::join(
                demo.call_delayed(accessor, "first".to_string(), 20),
                demo.call_delayed(accessor, "second".to_string(), 20),
            )
            .await
        })
        .await?;
    assert_eq!(first?, "first:2");
    assert_eq!(second?, "second:1");
    Ok(())
}
