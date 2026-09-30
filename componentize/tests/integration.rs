// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// First end-to-end componentization.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// The wasi-sdk shared libraries are read from `$WASI_SDK_PATH` (falling back to
// `/opt/wasi-sdk`). `just test-componentize` exports it.
//
// This smoke test componentizes one tiny world (`test:smoke`, see
// `fixtures/smoke.wit` + `fixtures/smoke.js`), instantiates it under wasmtime
// with a host that records the logged message, calls `run(41)`, and asserts the
// result is `42` and the host saw the exact log line. Each of those, u32
// argument lifting, u32 result lowering, export resolution, import resolution,
// and string lowering, independently gates the assert, so a regression in any
// one of them fails the test rather than passing silently.
//
// Componentizing a world takes seconds, so the component is built once and
// shared via a `OnceCell` `Pre`, and each test gets a fresh `Store`.

mod shared;

use {
    std::sync::{Arc, Mutex},
    tokio::sync::OnceCell,
    wasmtime::{
        component::{HasSelf, Linker},
        Engine, Store,
    },
    wasmtime_wasi::{WasiCtxView, WasiView},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/smoke.wit",
    world: "smoke",
    imports: { default: async },
    exports: { default: async },
});

/// The host store data: WASI context, resource table, and the log sink the
/// world-level `log` import writes into.
struct Ctx {
    wasi: shared::Wasi,
    logged: Arc<Mutex<Vec<String>>>,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

// The world-level `log` import. `bindgen!` emits it as a root-level `SmokeImports`
// trait (world-level imports are not behind an interface accessor).
impl SmokeImports for Ctx {
    async fn log(&mut self, msg: String) -> () {
        self.logged.lock().unwrap().push(msg);
    }
}

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// Componentize the smoke world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static SmokePre<Ctx> {
    static PRE: OnceCell<Result<SmokePre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/smoke.wit"),
            "smoke",
            include_str!("fixtures/smoke.js"),
        )
        .await;

        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker).expect("wasi linker");
        Smoke::add_to_linker::<_, HasSelf<_>>(&mut linker, |ctx| ctx).expect("smoke linker");
        // The wizened component still declares the runtime's transitive
        // imports (Wizer does not strip uncalled imports from the component
        // type): `wasi:http`, wasip3 clocks, etc. `run` only calls `log` and
        // returns `n+1`, so trap-stubbing the rest is correct and never fires.
        // Reuse the componentizer's async-aware stubber, since `wasi:http/client#send`
        // is async, which wasmtime's own `define_unknown_imports_as_traps`
        // mis-stubs as sync.
        componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])
            .expect("trap-stub unknown imports");

        SmokePre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("SmokePre")
    })
    .await
}

fn store(logged: Arc<Mutex<Vec<String>>>) -> Store<Ctx> {
    let wasi = shared::Wasi::inherit();
    Store::new(&ENGINE, Ctx { wasi, logged })
}

#[tokio::test]
async fn smoke_run_and_log() -> anyhow::Result<()> {
    let logged = Arc::new(Mutex::new(Vec::new()));
    let mut store = store(logged.clone());
    let instance = pre().await.instantiate_async(&mut store).await?;

    let result = instance.call_run(&mut store, 41).await?;
    assert_eq!(result, 42, "run(41) should return 42");

    let messages = logged.lock().unwrap();
    assert_eq!(
        &*messages,
        &["run was called with 41".to_string()],
        "host should have seen the logged message with the lowered string"
    );

    Ok(())
}
