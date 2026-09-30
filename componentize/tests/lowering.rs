// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Import arguments and stream elements whose state JavaScript changes after the
// call that passes them.
//
// PREREQUISITE: a runtime build (see `shared::runtime`).
//
// This componentizes ONE world (`lowering`, see `fixtures/lowering.{wit,js}`).
// Its guest passes resource wrappers, streams and typed arrays to host imports,
// then disposes of, reuses or changes them, and describes what the imports
// received. The component is built once and shared via a `OnceCell` `Pre`, and
// each test gets a fresh `Store`.

mod shared;

use shared::collect_all;
use tokio::sync::OnceCell;
use wasmtime::component::{Accessor, FutureReader, HasSelf, Linker, Resource, StreamReader};
use wasmtime::{Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "tests/fixtures/lowering.wit",
    world: "lowering",
    imports: { default: async },
    exports: { default: async },
    with: {
        "test:lowering/host.thing": ThingValue,
        "test:lowering/host.bare": BareValue,
    },
});

/// The host's representation of a `thing`: its value.
pub struct ThingValue(u32);

/// The host's representation of a `bare`: its value.
pub struct BareValue(u32);

struct Ctx {
    wasi: shared::Wasi,
    /// The future `accept` received.
    accepted: Option<FutureReader<u32>>,
    /// How many `thing`s the guest dropped.
    thing_drops: u32,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

impl test::lowering::host::Host for Ctx {
    async fn take_two(&mut self, _s: StreamReader<u32>, t: Resource<ThingValue>) -> u32 {
        self.wasi.table().delete(t).expect("a live thing").0
    }

    async fn make_bare(&mut self) -> Resource<BareValue> {
        self.wasi
            .table()
            .push(BareValue(77))
            .expect("pushing a bare")
    }

    async fn bare_value(&mut self, b: Resource<BareValue>) -> u32 {
        self.wasi.table().get(&b).expect("a live bare").0
    }

    async fn accept(&mut self, f: FutureReader<u32>) {
        self.accepted = Some(f);
    }
}

impl test::lowering::host::HostThing for Ctx {
    async fn new(&mut self, v: u32) -> Resource<ThingValue> {
        self.wasi
            .table()
            .push(ThingValue(v))
            .expect("pushing a thing")
    }

    async fn drop(&mut self, rep: Resource<ThingValue>) -> wasmtime::Result<()> {
        self.wasi.table().delete(rep)?;
        self.thing_drops += 1;
        Ok(())
    }
}

impl test::lowering::host::HostBare for Ctx {
    async fn drop(&mut self, rep: Resource<BareValue>) -> wasmtime::Result<()> {
        self.wasi.table().delete(rep)?;
        Ok(())
    }
}

impl test::lowering::host::HostWithStore<Ctx> for HasSelf<Ctx> {
    async fn consume(accessor: &Accessor<Ctx, Self>, t: Resource<ThingValue>) -> u32 {
        tokio::task::yield_now().await;
        accessor.with(|mut access| access.get().wasi.table().delete(t).expect("a live thing").0)
    }

    async fn peek(accessor: &Accessor<Ctx, Self>, t: Resource<ThingValue>) -> u32 {
        tokio::task::yield_now().await;
        accessor.with(|mut access| access.get().wasi.table().get(&t).expect("a live thing").0)
    }

    async fn first_byte(_accessor: &Accessor<Ctx, Self>, data: Vec<u8>) -> u32 {
        data[0].into()
    }

    async fn drain(accessor: &Accessor<Ctx, Self>, s: StreamReader<u32>) -> u32 {
        let (values, _) = collect_all(accessor, s).await.expect("reading the stream");
        values.into_iter().sum()
    }
}

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// Componentize the lowering world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static LoweringPre<Ctx> {
    static PRE: OnceCell<Result<LoweringPre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/lowering.wit"),
            "lowering",
            include_str!("fixtures/lowering.js"),
        )
        .await;
        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker).expect("wasi linker");
        Lowering::add_to_linker::<_, HasSelf<_>>(&mut linker, |ctx| ctx).expect("add_to_linker");
        componentize::trap_unsatisfied_imports(
            &ENGINE,
            &component,
            &mut linker,
            &["test:lowering/"],
        )
        .expect("trap-stub unknown imports");
        LoweringPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("LoweringPre")
    })
    .await
}

/// A fresh store whose stderr goes to the returned pipe.
fn store() -> (Store<Ctx>, MemoryOutputPipe) {
    let (wasi, stderr) = shared::wasi_capturing_stderr();
    let ctx = Ctx {
        wasi,
        accepted: None,
        thing_drops: 0,
    };
    (Store::new(&ENGINE, ctx), stderr)
}

/// Run the guest's case `which` and return its description.
async fn run(which: &str) -> anyhow::Result<String> {
    let (mut store, _) = store();
    run_in(&mut store, which).await
}

/// [`run`] in `store`.
async fn run_in(store: &mut Store<Ctx>, which: &str) -> anyhow::Result<String> {
    let instance = pre().await.instantiate_async(&mut *store).await?;
    let which = which.to_string();
    let described = store
        .run_concurrent(async |store| instance.test_lowering_runner().call_run(store, which).await)
        .await??;
    Ok(described)
}

/// A wrapper passed as an `own` moves to the host when the call starts, so a
/// dispose right after it does nothing.
#[tokio::test]
async fn dispose_after_an_own_argument_does_nothing() -> anyhow::Result<()> {
    assert_eq!(run("disposeAfterConsume").await?, "ok 5");
    Ok(())
}

/// A wrapper passed as an `own` to two calls started together rejects the
/// second with a `TypeError`.
#[tokio::test]
async fn own_argument_to_two_pending_calls_rejects_the_second() -> anyhow::Result<()> {
    assert_eq!(
        run("consumeTwice").await?,
        "TypeError: argument 1 of `consume`: the resource was disposed of, or moved to the host"
    );
    Ok(())
}

/// A wrapper disposed of while an import call borrows it is dropped once the
/// call returns.
#[tokio::test]
async fn dispose_waits_for_a_pending_borrow() -> anyhow::Result<()> {
    let (mut store, _) = store();
    assert_eq!(run_in(&mut store, "disposeWhilePeeked").await?, "ok 6");
    assert_eq!(store.data().thing_drops, 1);
    Ok(())
}

/// A wrapper an import call borrows cannot be passed as an `own` until that
/// call returns.
#[tokio::test]
async fn own_argument_while_borrowed_rejects() -> anyhow::Result<()> {
    assert_eq!(
        run("consumeWhilePeeked").await?,
        "TypeError: argument 1 of `consume`: the resource is lent to an import call that has \
         not returned, ok 3"
    );
    Ok(())
}

/// A `ReadableStream` passed to two calls started together rejects the second
/// with a `TypeError`.
#[tokio::test]
async fn stream_argument_to_two_pending_calls_rejects_the_second() -> anyhow::Result<()> {
    assert_eq!(
        run("drainTwice").await?,
        "TypeError: argument 1 of `drain`: the stream is locked"
    );
    Ok(())
}

/// A typed array is read when the call starts, so changing it afterwards does
/// not change what the host receives.
#[tokio::test]
async fn typed_array_argument_is_read_when_the_call_starts() -> anyhow::Result<()> {
    assert_eq!(run("changeBufferAfterCall").await?, "ok 1");
    Ok(())
}

/// A stream argument's source runs after the call's arguments were lowered, so
/// it cannot dispose of a later argument before the host receives it.
#[tokio::test]
async fn stream_source_runs_after_the_arguments_are_lowered() -> anyhow::Result<()> {
    assert_eq!(run("pullDisposesArgument").await?, "ok 9");
    Ok(())
}

/// An imported resource with no constructor, method or static function has a
/// class, and its wrappers have `[Symbol.dispose]()`.
#[tokio::test]
async fn bare_imported_resource_has_a_class() -> anyhow::Result<()> {
    assert_eq!(run("bareResource").await?, "function true 77");
    Ok(())
}

/// A stream element holding a wrapper an earlier element holds ends the stream
/// early, after the elements before it, and logs why. Chunks the source enqueues
/// together are written together, and a later chunk finds the wrapper moved.
#[tokio::test]
async fn repeated_resource_in_a_stream_ends_it_early() -> anyhow::Result<()> {
    for (together, reason) in [
        (true, "the resource is used elsewhere in the same call"),
        (false, "the resource was disposed of, or moved to the host"),
    ] {
        let (mut store, stderr) = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        let (received, ended) = store
            .run_concurrent(async |store| {
                let runner = instance.test_lowering_runner();
                let things = runner.call_things(store, together).await?;
                collect_all(store, things).await
            })
            .await??;
        assert!(ended, "the stream must end");
        assert_eq!(received.len(), 1);
        let stderr = shared::pipe_text(&stderr);
        assert!(
            stderr.contains(&format!("ended the stream early: a chunk: {reason}")),
            "{stderr}"
        );
    }
    Ok(())
}

/// A sync export cannot pass a future to an import, since no event loop would
/// write it. The import call throws a `TypeError` naming the export.
#[tokio::test]
async fn future_argument_in_a_sync_export_throws() -> anyhow::Result<()> {
    let (mut store, _) = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let described = instance
        .test_lowering_runner()
        .call_give(&mut store)
        .await?;
    assert!(
        described.starts_with(
            "TypeError: argument 1 of `accept`: passing a future needs an active event loop"
        ) && described.contains("`test:lowering/runner#give`"),
        "{described}"
    );
    assert!(store.data().accepted.is_none());
    Ok(())
}

/// An exported resource's method is looked up on the instance, so a subclass's
/// override runs.
#[tokio::test]
async fn exported_method_dispatches_on_the_instance() -> anyhow::Result<()> {
    let (mut store, _) = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let widget = instance.test_lowering_runner().widget();
    let sub = widget.call_make(&mut store, true).await?;
    assert_eq!(widget.call_get(&mut store, sub).await?, 2);
    let base = widget.call_make(&mut store, false).await?;
    assert_eq!(widget.call_get(&mut store, base).await?, 1);
    Ok(())
}
