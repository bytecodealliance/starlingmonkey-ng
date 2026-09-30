// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// End-to-end coverage of an EXPORTED (guest-owned) resource through the real
// componentize pipeline.
//
// The other worlds only ever import host-owned resources, so the
// exported-resource slab (`component-model/src/resources.rs`), covering a guest
// constructing and returning a resource the host then uses and finally drops,
// was exercised only by native tests driving closures for the resource's
// `new`/`rep`/`drop`. The wasm path instead wires the real wit-dylib `Resource`
// fn-pointers. This suite covers it end to end.
//
// The world (`fixtures/exported_resource.wit`) exports `counters.counter`, a
// guest-owned resource implemented in `fixtures/exported_resource.js`. The test
// drives the full lifecycle across the boundary:
//
//   1. construct:     `make-counter(10)`, a freestanding `own<counter>` return
//                        and the generated `[constructor]counter`, each minting
//                        a slab entry + a canonical handle via `[resource-new]`.
//   2. host -> guest:  `bump(5)` and `value()` instance methods, whose `this` is
//                        the slab-resident object (resolved by rep), and the
//                        `total-created` static.
//   3. drop:           `ResourceAny::resource_drop_async` drives the runtime's
//                        `resource_dtor`, which removes the slab entry AND runs
//                        the object's `[Symbol.dispose]()` (this
//                        engine's stand-in for an exported `Symbol.dispose`). The
//                        guest's hook decrements a `live-count` static, so the
//                        drop is observed positively, as `live-count` going
//                        1 -> 0 across the boundary, not merely as "did not
//                        trap".
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// The world is componentized ONCE via a `OnceCell` and shared across cases, and each
// case gets a fresh `Store`.

mod shared;

use shared::Ctx;

use {
    std::sync::LazyLock,
    tokio::sync::OnceCell,
    wasmtime::{component::Linker, Engine, Store},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/exported_resource.wit",
    world: "exported-resource",
    imports: { default: async },
    exports: { default: async },
});

static ENGINE: LazyLock<Engine> = LazyLock::new(shared::engine);

/// Componentize the exported-resource world once and hold the instantiation-ready
/// `Pre`. The world imports nothing of its own (only the runtime's transitive
/// WASI), so the linker needs only WASI plus the trap stubs for the imports Wizer
/// leaves declared.
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

        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker).expect("wasi linker");
        // The wizened component still declares the runtime's transitive
        // imports, since Wizer does not strip them, so trap-stub the ones this world does
        // not provide. They are never called.
        componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])
            .expect("trap-stub unknown imports");

        ExportedResourcePre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("ExportedResourcePre")
    })
    .await
}

/// A fresh store for one test case. The world needs nothing but WASI.
fn store() -> Store<Ctx> {
    shared::store(&ENGINE)
}

/// The full exported-resource lifecycle through `make-counter`: construct → use
/// methods → static → drop, with the drop observed positively as `live-count`
/// falling.
#[tokio::test]
async fn exported_resource_lifecycle() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counters = instance.componentize_js_exported_resource_counters();

    // (1) construct: the guest returns `new Counter(10)`, minted into the slab
    // and handed back as a canonical handle.
    let counter = counters.call_make_counter(&mut store, 10).await?;

    // (2) host -> guest methods on the guest-owned resource.
    assert_eq!(
        15,
        counters.counter().call_bump(&mut store, counter, 5).await?
    );
    assert_eq!(
        22,
        counters.counter().call_bump(&mut store, counter, 7).await?
    );
    assert_eq!(
        22,
        counters.counter().call_value(&mut store, counter).await?
    );

    // The statics reflect the one live construction so far.
    assert_eq!(1, counters.counter().call_total_created(&mut store).await?);
    assert_eq!(
        1,
        counters.counter().call_live_count(&mut store).await?,
        "the counter is live before the drop"
    );

    // (3) drop: the host releases the handle, driving the runtime's
    // `resource_dtor` -> `drop_exported`, which removes the slab entry and runs
    // the guest's `[Symbol.dispose]()`, which decrements the
    // guest's `liveCount`, so the drop is observable positively (not merely "did
    // not trap") as `live-count` falling to 0.
    counter.resource_drop_async(&mut store).await?;
    assert_eq!(
        0,
        counters.counter().call_live_count(&mut store).await?,
        "the host-side drop ran the guest drop hook"
    );
    // `total-created` is monotonic (incremented only at construction), so it
    // stays at 1, confirming the drop decremented live-count specifically, not
    // some construction-side counter.
    assert_eq!(
        1,
        counters.counter().call_total_created(&mut store).await?,
        "total-created is not touched by the drop"
    );

    // A counter constructed after the drop is fully usable, and the live count
    // returns to 1, so the runtime is healthy past a host-side drop, and the drop
    // hook ran exactly once (live-count is 1, not 0 or -1).
    let counter2 = counters.call_make_counter(&mut store, 100).await?;
    assert_eq!(
        103,
        counters
            .counter()
            .call_bump(&mut store, counter2, 3)
            .await?
    );
    assert_eq!(
        2,
        counters.counter().call_total_created(&mut store).await?,
        "the second construction bumps total-created"
    );
    assert_eq!(
        1,
        counters.counter().call_live_count(&mut store).await?,
        "one counter live again after the second construction"
    );
    counter2.resource_drop_async(&mut store).await?;
    assert_eq!(
        0,
        counters.counter().call_live_count(&mut store).await?,
        "the second drop hook ran too"
    );

    Ok(())
}

/// A host-side drop runs the guest's `[Symbol.dispose]` exactly once: the
/// count of dispose calls is 1 after the drop and unchanged by further calls
/// into the guest, and a second counter's drop adds exactly one more.
#[tokio::test]
async fn host_drop_runs_dispose_exactly_once() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counters = instance.componentize_js_exported_resource_counters();

    let counter = counters.call_make_counter(&mut store, 1).await?;
    assert_eq!(0, counters.counter().call_dispose_calls(&mut store).await?);

    counter.resource_drop_async(&mut store).await?;
    assert_eq!(1, counters.counter().call_dispose_calls(&mut store).await?);

    // Further calls into the guest, including one that constructs and uses
    // another counter, do not run the dropped counter's hook again.
    let other = counters.call_make_counter(&mut store, 2).await?;
    counters.counter().call_bump(&mut store, other, 1).await?;
    assert_eq!(2, counters.counter().call_total_created(&mut store).await?);
    assert_eq!(1, counters.counter().call_dispose_calls(&mut store).await?);

    other.resource_drop_async(&mut store).await?;
    assert_eq!(2, counters.counter().call_dispose_calls(&mut store).await?);
    assert_eq!(0, counters.counter().call_live_count(&mut store).await?);
    Ok(())
}

/// The generated `[constructor]counter` path: the host invokes the resource's
/// constructor directly (a `Resolved::Constructor` -> `Function::construct` in
/// `exports.rs`), rather than the freestanding `make-counter`. Both mint a slab
/// entry the same way, and this covers the second entry point.
#[tokio::test]
async fn exported_resource_constructor() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counters = instance.componentize_js_exported_resource_counters();

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
    let counters = instance.componentize_js_exported_resource_counters();
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
    let counters = instance.componentize_js_exported_resource_counters();
    let counter = counters.call_make_counter(&mut store, 10).await?;

    let value = store
        .run_concurrent(async |accessor| {
            instance
                .componentize_js_exported_resource_counters()
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
