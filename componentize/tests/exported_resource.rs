// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A guest-owned resource: the world of `fixtures/exported_resource.{wit,js}`, which exports
//! `counters.counter`. The host calls the methods and statics of counters the guest constructs,
//! and the `live-count` and `dispose-calls` statics observe the `[Symbol.dispose]()` a host-side
//! drop runs.

mod shared;

use shared::Ctx;

use {
    std::sync::LazyLock,
    tokio::sync::OnceCell,
    wasmtime::{Engine, Store},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/exported_resource.wit",
    world: "exported-resource",
    imports: { default: async },
    exports: { default: async },
});

static ENGINE: LazyLock<Engine> = LazyLock::new(shared::engine);

/// Componentize the exported-resource world once and hold the instantiation-ready
/// `Pre`.
async fn pre() -> &'static ExportedResourcePre<Ctx> {
    static PRE: OnceCell<Result<ExportedResourcePre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/exported_resource.wit"),
            "exported-resource",
            include_str!("fixtures/exported_resource.js"),
        )
        .await;

        let linker = shared::linker(&ENGINE, &component, &[], |_| Ok(()));
        ExportedResourcePre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("ExportedResourcePre")
    })
    .await
}

/// A fresh store for one test case. The world needs nothing but WASI.
fn store() -> Store<Ctx> {
    shared::store(&ENGINE)
}

/// The exported-resource lifecycle through `make-counter`: construct, call
/// methods and statics, and drop. Each host-side drop runs the guest's
/// `[Symbol.dispose]` exactly once, which no later call into the guest repeats.
#[tokio::test]
async fn exported_resource_lifecycle() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counters = instance.test_exported_resource_counters();
    let counter_class = counters.counter();

    let counter = counters.call_make_counter(&mut store, 10).await?;
    assert_eq!(15, counter_class.call_bump(&mut store, counter, 5).await?);
    assert_eq!(22, counter_class.call_bump(&mut store, counter, 7).await?);
    assert_eq!(22, counter_class.call_value(&mut store, counter).await?);
    assert_eq!(1, counter_class.call_total_created(&mut store).await?);
    assert_eq!(1, counter_class.call_live_count(&mut store).await?);
    assert_eq!(0, counter_class.call_dispose_calls(&mut store).await?);

    counter.resource_drop_async(&mut store).await?;
    assert_eq!(0, counter_class.call_live_count(&mut store).await?);
    assert_eq!(1, counter_class.call_dispose_calls(&mut store).await?);
    assert_eq!(1, counter_class.call_total_created(&mut store).await?);

    // A counter constructed after the drop is usable, and the dropped counter's
    // hook does not run again.
    let counter2 = counters.call_make_counter(&mut store, 100).await?;
    assert_eq!(103, counter_class.call_bump(&mut store, counter2, 3).await?);
    assert_eq!(2, counter_class.call_total_created(&mut store).await?);
    assert_eq!(1, counter_class.call_live_count(&mut store).await?);
    assert_eq!(1, counter_class.call_dispose_calls(&mut store).await?);

    counter2.resource_drop_async(&mut store).await?;
    assert_eq!(0, counter_class.call_live_count(&mut store).await?);
    assert_eq!(2, counter_class.call_dispose_calls(&mut store).await?);
    Ok(())
}

/// The host constructs a counter through the generated `[constructor]counter`
/// rather than the freestanding `make-counter`.
#[tokio::test]
async fn exported_resource_constructor() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counters = instance.test_exported_resource_counters();

    let counter = counters.counter().call_constructor(&mut store, 1).await?;
    assert_eq!(
        4,
        counters.counter().call_bump(&mut store, counter, 3).await?
    );
    assert_eq!(4, counters.counter().call_value(&mut store, counter).await?);
    counter.resource_drop_async(&mut store).await?;

    Ok(())
}

/// An object that cannot take new properties, such as a frozen or sealed one, can
/// back an exported resource.
#[tokio::test]
async fn non_extensible_object_backs_a_resource() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counters = instance.test_exported_resource_counters();
    let counter = counters.call_make_sealed_counter(&mut store, 2).await?;
    assert_eq!(
        5,
        counters.counter().call_bump(&mut store, counter, 3).await?
    );
    counter.resource_drop_async(&mut store).await?;
    Ok(())
}

/// An async export borrows a guest-owned resource across a timer turn, and the
/// borrow ending leaves the resource to its owner.
#[tokio::test]
async fn async_export_reads_a_borrow_after_a_timer() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counters = instance.test_exported_resource_counters();
    let counter = counters.call_make_counter(&mut store, 10).await?;

    let value = store
        .run_concurrent(async |accessor| {
            instance
                .test_exported_resource_counters()
                .call_read_later(accessor, counter)
                .await
        })
        .await??;
    assert_eq!(value, 10);
    assert_eq!(1, counters.counter().call_live_count(&mut store).await?);
    assert_eq!(0, counters.counter().call_dispose_calls(&mut store).await?);
    assert_eq!(
        11,
        counters.counter().call_bump(&mut store, counter, 1).await?
    );

    counter.resource_drop_async(&mut store).await?;
    assert_eq!(0, counters.counter().call_live_count(&mut store).await?);
    Ok(())
}
