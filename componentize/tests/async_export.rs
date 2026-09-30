// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Async exports: the world of `fixtures/async_export.{wit,js}`, which exports a sync
//! `simple-export.foo` and an async `simple-async-export.foo`.

mod shared;

use shared::Ctx;

use {
    tokio::sync::{Notify, OnceCell},
    wasmtime::{
        component::{Accessor, HasSelf},
        Engine, Store,
    },
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/async_export.wit",
    world: "async-export",
    imports: { default: async },
    exports: { default: async },
});

impl test::async_export::host::Host for Ctx {}

/// Released once by [`sync_exports_run_while_an_async_export_is_parked`].
static RELEASE: Notify = Notify::const_new();

impl test::async_export::host::HostWithStore<Ctx> for HasSelf<Ctx> {
    async fn release(_accessor: &Accessor<Ctx, Self>) {
        RELEASE.notified().await;
    }
}

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// Componentize the async-export world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static AsyncExportPre<Ctx> {
    static PRE: OnceCell<Result<AsyncExportPre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/async_export.wit"),
            "async-export",
            include_str!("fixtures/async_export.js"),
        )
        .await;

        let linker = shared::linker(&ENGINE, &component, &[], |linker| {
            AsyncExport::add_to_linker::<_, HasSelf<_>>(linker, |ctx| ctx)
        });
        AsyncExportPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("AsyncExportPre")
    })
    .await
}

/// A fresh store for one test case. The world needs nothing but WASI.
fn store() -> Store<Ctx> {
    shared::store(&ENGINE)
}

/// An async export that delivers its result but leaves a perpetual `setInterval`
/// running, so the per-call event loop never becomes idle. The export's subtask
/// completes regardless: the driver completes the call and abandons the timer when
/// it drops the loop. With the `timeout`, a driver that instead runs the loop to
/// full idle fails the test rather than hanging the suite. Exports called
/// on the same instance afterwards still complete.
#[tokio::test]
async fn perpetual_trailing_work_does_not_hang() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        store.run_concurrent(async |accessor| {
            instance
                .test_async_export_simple_async_export()
                .call_perpetual(accessor, 42)
                .await
        }),
    )
    .await
    .expect("perpetual-trailing-work async export must complete, not hang")??;
    assert_eq!(
        result, 45,
        "perpetual(42) should return 45 even though a setInterval is left running"
    );

    // The instance stays usable: exports called after it still complete,
    // including the async one, which waits for a timer.
    let (again, after) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        store.run_concurrent(async |accessor| {
            let exports = instance.test_async_export_simple_async_export();
            let again = exports.call_perpetual(accessor, 1).await?;
            let after = exports.call_foo(accessor, 2).await?;
            anyhow::Ok((again, after))
        }),
    )
    .await
    .expect("exports called after abandoned work must complete")??;
    assert_eq!((again, after), (4, 5));
    let sync = instance
        .test_async_export_simple_export()
        .call_foo(&mut store, 7)
        .await?;
    assert_eq!(sync, 10);
    Ok(())
}

/// A deadlocking async export. The guest `await`s a never-resolving promise with
/// no loop work behind it, so the per-call event loop goes idle while the returned
/// promise is still `Pending`. The call traps, and the reason, which names the
/// export, is printed to stderr.
#[tokio::test]
async fn deadlocked_async_export_traps() -> anyhow::Result<()> {
    let (mut store, stderr) = shared::store_capturing_stderr(&ENGINE);
    let instance = pre().await.instantiate_async(&mut store).await?;
    let result = store
        .run_concurrent(async |accessor| {
            instance
                .test_async_export_simple_async_export()
                .call_hang(accessor)
                .await
        })
        .await;
    let trapped = shared::call_failed(&result);
    assert!(
        trapped,
        "a deadlocked async export must trap (host-observable Err), got {result:?}"
    );
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains(
            "the async export `test:async-export/simple-async-export#hang` did not settle"
        ),
        "{stderr}"
    );
    Ok(())
}

/// An async export whose promise rejects with an `Error` after a timer turn
/// returns the message as its `err`, and the rejection is not reported on stderr
/// as unhandled.
#[tokio::test]
async fn rejected_async_export_returns_err() -> anyhow::Result<()> {
    let (mut store, stderr) = shared::store_capturing_stderr(&ENGINE);
    let instance = pre().await.instantiate_async(&mut store).await?;
    let result = store
        .run_concurrent(async |accessor| {
            instance
                .test_async_export_simple_async_export()
                .call_fail(accessor, "boom".to_string())
                .await
        })
        .await??;
    assert_eq!(result, Err("boom".to_string()));
    let stderr = shared::pipe_text(&stderr);
    assert!(!stderr.contains("Uncaught (in promise)"), "{stderr}");
    Ok(())
}

/// `setTimeout` in a sync export throws a `TypeError` naming the export.
#[tokio::test]
async fn timers_in_a_sync_export_throw() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let message = instance
        .test_async_export_simple_export()
        .call_timer_message(&mut store)
        .await?;
    assert!(
        message.starts_with("setTimeout needs an active event loop")
            && message.contains("`test:async-export/simple-export#timer-message`"),
        "{message}"
    );
    Ok(())
}

/// `fetch` in a sync export returns a promise rejected with a `TypeError` naming
/// the export.
#[tokio::test]
async fn fetch_in_a_sync_export_rejects() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let simple = instance.test_async_export_simple_export();
    assert_eq!(simple.call_fetch_message(&mut store).await?, "none");
    let message = simple.call_fetch_message(&mut store).await?;
    assert!(
        message.starts_with("`fetch` needs an active event loop")
            && message.contains("`test:async-export/simple-export#fetch-message`"),
        "{message}"
    );
    Ok(())
}

/// The microtasks a sync export queues run before the call returns.
#[tokio::test]
async fn sync_export_microtasks_run_before_it_returns() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let simple = instance.test_async_export_simple_export();
    for expected in 0..3 {
        assert_eq!(simple.call_drained_microtasks(&mut store).await?, expected);
    }
    Ok(())
}

/// Sync exports called while an async export waits on a host import run at
/// once, without the waiting export's event loop: their microtasks run before
/// they return, and `setTimeout` throws in them.
#[tokio::test]
async fn sync_exports_run_while_an_async_export_is_parked() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre()
        .await
        .instance_pre()
        .instantiate_async(&mut store)
        .await?;
    let bindings = AsyncExport::new(&mut store, &instance)?;
    let interface = instance
        .get_export_index(&mut store, None, "test:async-export/simple-export")
        .expect("the sync interface is exported");
    let mut func = |name: &str| {
        instance
            .get_export_index(&mut store, Some(&interface), name)
            .unwrap_or_else(|| panic!("`{name}` is exported"))
    };
    let (drained, timer) = (func("drained-microtasks"), func("timer-message"));
    let drained = instance.get_typed_func::<(), (u32,)>(&mut store, drained)?;
    let timer = instance.get_typed_func::<(), (String,)>(&mut store, timer)?;

    let (parked, sync) = store
        .run_concurrent(async |accessor| {
            // `join` polls the async export first, so it waits on `release`
            // when the sync calls start.
            futures::future::join(
                bindings
                    .test_async_export_simple_async_export()
                    .call_parked(accessor),
                async {
                    let first = drained.call_concurrent(accessor, ()).await?.0;
                    let second = drained.call_concurrent(accessor, ()).await?.0;
                    let message = timer.call_concurrent(accessor, ()).await?.0;
                    RELEASE.notify_one();
                    anyhow::Ok((first, second, message))
                },
            )
            .await
        })
        .await?;
    let (first, second, message) = sync?;
    assert_eq!((first, second), (0, 1));
    assert!(
        message.starts_with("setTimeout needs an active event loop"),
        "{message}"
    );
    assert_eq!(
        parked?, 2,
        "both microtasks ran while the async export waited"
    );
    Ok(())
}
