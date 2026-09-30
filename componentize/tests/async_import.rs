// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Async imports end-to-end.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// One world (`fixtures/async_import.wit` + `fixtures/async_import.js`) whose
// `async` export `runner.foo` calls the `async` host import `host.bar` and adds 3
// to its result. The host's `bar(x)` yields to the executor before returning
// `x + 2`, so `foo(42)` is `await bar(42)` => 44, `+ 3` => 47. The recorded yield
// is asserted too, since only a runtime that drove the import's future through
// the loop can produce 47 after a host suspension.
//
// Componentizing a world takes seconds, so the component is built once and
// shared via a `OnceCell` `Pre`, and each test gets a fresh `Store`.

mod shared;

use {
    std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    tokio::sync::OnceCell,
    wasmtime::{
        component::{HasSelf, Linker},
        Engine, Store,
    },
    wasmtime_wasi::{WasiCtxView, WasiView},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/async_import.wit",
    world: "async-import",
    imports: { default: async },
    exports: { default: async },
});

/// How often the host's `bar` started and finished. `Arc<...>` so the counts
/// survive the per-call `Store` and are readable after the call returns.
#[derive(Clone, Default)]
struct BarLog {
    /// Incremented when `bar` is entered (before the yield).
    started: Arc<AtomicU32>,
    /// Incremented when `bar` resumes after its yield and is about to answer.
    finished: Arc<AtomicU32>,
}

/// The host store data: WASI context + resource table + the `bar` call log.
struct Ctx {
    wasi: shared::Wasi,
    bar: BarLog,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

/// The interface's non-store `Host` trait has no methods, since its only function
/// is a with-store async import on `HostWithStore`, but the linker still requires
/// the data view (`&mut Ctx`) to be `Host`.
impl test::async_import::host::Host for Ctx {}

/// The async host import. Records that it started, yields control back to the
/// executor so the runtime must re-poll the import's loop-driven future, records
/// that it resumed, then returns `x + 2`.
///
/// `HostWithStore` is implemented on the `HasData` type `HasSelf<Ctx>`, whose
/// `Data<'a>` is `&'a mut Ctx`. The world has no `trappable` flag, so `bar`
/// returns a bare `u32`.
impl test::async_import::host::HostWithStore<Ctx> for HasSelf<Ctx> {
    async fn bar(accessor: &wasmtime::component::Accessor<Ctx, Self>, x: u32) -> u32 {
        let log = accessor.with(|mut access| access.get().bar.clone());
        log.started.fetch_add(1, Ordering::SeqCst);

        // Hands control back to the executor so this call cannot complete inline.
        // The per-call event loop must re-poll the import's future to get past
        // this point.
        tokio::task::yield_now().await;

        log.finished.fetch_add(1, Ordering::SeqCst);
        x + 2
    }

    /// The `result`-typed async host import: `+ 2` on `ok`, `+ 3` on `err`,
    /// after the same yield as `bar`.
    async fn baz(
        _accessor: &wasmtime::component::Accessor<Ctx, Self>,
        v: Result<u32, u32>,
    ) -> Result<u32, u32> {
        tokio::task::yield_now().await;
        v.map(|x| x + 2).map_err(|x| x + 3)
    }
}

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// Componentize the async-import world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static AsyncImportPre<Ctx> {
    static PRE: OnceCell<Result<AsyncImportPre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/async_import.wit"),
            "async-import",
            include_str!("fixtures/async_import.js"),
        )
        .await;

        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker).expect("wasi linker");
        // The per-call event loop sleeps on the WASIp3 monotonic clock between
        // turns, so the host must satisfy the p3 WASI surface. Only the clock is
        // exercised here.
        // The world's own async import `host.bar`. `HasSelf<Ctx>` maps the linker
        // type parameter to `&mut Ctx`, so the host getter is the identity.
        test::async_import::host::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |c| c)
            .expect("async-import host linker");
        // The wizened component still declares the runtime's other transitive
        // imports, since Wizer does not strip uncalled imports, so trap-stub the rest.
        componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])
            .expect("trap-stub unknown imports");

        AsyncImportPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("AsyncImportPre")
    })
    .await
}

fn store(bar: BarLog) -> Store<Ctx> {
    let wasi = shared::Wasi::inherit();
    Store::new(&ENGINE, Ctx { wasi, bar })
}

/// The async export calls the async host import: `foo(42)` is `await bar(42)`,
/// which yields and returns 44, `+ 3` => 47. Asserts both the value and that the
/// import suspended.
#[tokio::test]
async fn async_import_and_export() -> anyhow::Result<()> {
    let bar = BarLog::default();
    let mut store = store(bar.clone());
    let instance = pre().await.instantiate_async(&mut store).await?;
    let result = store
        .run_concurrent(async |accessor| {
            instance
                .test_async_import_runner()
                .call_foo(accessor, 42)
                .await
        })
        .await??;

    assert_eq!(
        result, 47,
        "async foo(42) must await the async host import bar(42)=44 then add 3"
    );

    // `bar` ran exactly once and both started and finished around its
    // `yield_now().await`, which an inline-completing import could not do.
    assert_eq!(
        bar.started.load(Ordering::SeqCst),
        1,
        "the async host import bar must have been entered exactly once"
    );
    assert_eq!(
        bar.finished.load(Ordering::SeqCst),
        1,
        "the async host import bar must have resumed after yielding and answered once"
    );
    Ok(())
}

/// The `result`-typed async export forwards each arm of its argument through
/// the `result`-typed async host import: `ok(42)` is `await baz(ok(42))` =>
/// `ok(44)`, `+ 3` => `ok(47)`, and `err(42)` is `err(45)` from the host, which
/// rejects the guest's promise with a `ComponentError`, `+ 3` => `err(48)`.
#[tokio::test]
async fn async_import_and_export_result() -> anyhow::Result<()> {
    let mut store = store(BarLog::default());
    let instance = pre().await.instantiate_async(&mut store).await?;
    let ok = store
        .run_concurrent(async |accessor| {
            instance
                .test_async_import_runner()
                .call_qux(accessor, Ok(42))
                .await
        })
        .await??;
    assert_eq!(ok, Ok(47), "ok(42) must come back as ok(42 + 2 + 3)");

    let err = store
        .run_concurrent(async |accessor| {
            instance
                .test_async_import_runner()
                .call_qux(accessor, Err(42))
                .await
        })
        .await??;
    assert_eq!(err, Err(48), "err(42) must come back as err(42 + 3 + 3)");
    Ok(())
}

/// An async import called from a sync export returns a promise rejected with a
/// `TypeError` naming the export, rather than throwing.
#[tokio::test]
async fn async_import_in_a_sync_export_rejects() -> anyhow::Result<()> {
    let mut store = store(BarLog::default());
    let instance = pre().await.instantiate_async(&mut store).await?;
    let runner = instance.test_async_import_runner();
    assert_eq!(runner.call_bar_message(&mut store).await?, "none");
    let message = runner.call_bar_message(&mut store).await?;
    assert!(
        message.starts_with("the async import `bar` needs an active event loop")
            && message.contains("`test:async-import/runner#bar-message`"),
        "{message}"
    );
    Ok(())
}
