// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Async imports: the world of `fixtures/async_import.{wit,js}`, whose async export `runner.foo`
//! calls the async host import `host.bar` and adds 3 to its result. The host's `bar(x)` yields to
//! the executor before returning `x + 2`, so `foo(42)` returns 47.

mod shared;

use {
    shared::Ctx,
    tokio::sync::OnceCell,
    wasmtime::{component::HasSelf, Engine, Store},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/async_import.wit",
    world: "async-import",
    imports: { default: async },
    exports: { default: async },
});

impl test::async_import::host::Host for Ctx {}

/// The async host imports. Each yields to the executor before answering, so the
/// call cannot complete inline and the per-call event loop must poll the
/// import's future again.
impl test::async_import::host::HostWithStore<Ctx> for HasSelf<Ctx> {
    async fn bar(_accessor: &wasmtime::component::Accessor<Ctx, Self>, x: u32) -> u32 {
        tokio::task::yield_now().await;
        x + 2
    }

    /// `+ 2` on `ok`, `+ 3` on `err`.
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

        let linker = shared::linker(&ENGINE, &component, &[], |linker| {
            test::async_import::host::add_to_linker::<Ctx, HasSelf<Ctx>>(linker, |c| c)
        });
        AsyncImportPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("AsyncImportPre")
    })
    .await
}

fn store() -> Store<Ctx> {
    shared::store(&ENGINE)
}

/// The async export calls the async host import: `foo(42)` is `await bar(42)`,
/// which yields and returns 44, `+ 3` => 47.
#[tokio::test]
async fn async_import_and_export() -> anyhow::Result<()> {
    let mut store = store();
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
    Ok(())
}

/// The `result`-typed async export forwards each arm of its argument through
/// the `result`-typed async host import: `ok(42)` is `await baz(ok(42))` =>
/// `ok(44)`, `+ 3` => `ok(47)`, and `err(42)` is `err(45)` from the host, which
/// rejects the guest's promise with a `ComponentError`, `+ 3` => `err(48)`.
#[tokio::test]
async fn async_import_and_export_result() -> anyhow::Result<()> {
    let mut store = store();
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
    let mut store = store();
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
