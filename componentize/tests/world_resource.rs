// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A resource declared directly in the world (ComponentizeJS's
// `resource-top-level` case).
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
//
// The world (`fixtures/world_resource.{wit,js}`) declares `resource thing`
// with `get` and `set` methods, and exports `f: func(thing: borrow<thing>)`.
// The host owns the thing, the guest bumps it through the methods, and the
// host reads the new value back out of its table.
//
// The world cannot be componentized today: `componentize::finalize::finalize`
// composes the snapshot with `wac-graph`, whose `TypeEncoder::borrow` asserts
// that a scope is open, and the world-level method imports (`[method]thing.get`,
// `[method]thing.set`) take a `borrow<thing>` outside any interface scope. The
// componentizer checks for that shape first and refuses the world with a
// message naming the import, and the test asserts that message. A world-level
// resource without methods, and a world-level `use` of an interface's resource,
// both encode.

mod shared;

use shared::Ctx;

use {
    std::{path::PathBuf, sync::LazyLock},
    wasmtime::{
        component::{Component, HasSelf, Linker, Resource},
        Engine, Store,
    },
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/world_resource.wit",
    world: "world-resource",
    imports: { default: async },
    exports: { default: async },
    with: {
        "thing": ThingValue,
    },
});

/// The host `thing`, held in the resource table.
pub struct ThingValue(u32);

impl WorldResourceImports for Ctx {}

impl HostThing for Ctx {
    async fn new(&mut self, v: u32) -> Resource<ThingValue> {
        self.wasi.table().push(ThingValue(v)).unwrap()
    }
    async fn get(&mut self, self_: Resource<ThingValue>) -> u32 {
        self.wasi.table().get(&self_).unwrap().0
    }
    async fn set(&mut self, self_: Resource<ThingValue>, v: u32) {
        self.wasi.table().get_mut(&self_).unwrap().0 = v;
    }
    async fn drop(&mut self, rep: Resource<ThingValue>) -> wasmtime::Result<()> {
        self.wasi.table().delete(rep)?;
        Ok(())
    }
}

static ENGINE: LazyLock<Engine> = LazyLock::new(shared::engine);

/// Componentize the world. Not shared through a `OnceCell`, since the suite
/// has a single case.
async fn build() -> anyhow::Result<Vec<u8>> {
    let runtime = shared::runtime()?;
    componentize::componentize(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/world_resource.wit")),
        Some("world-resource"),
        &[],
        false,
        include_str!("fixtures/world_resource.js"),
        None::<PathBuf>,
        &runtime,
        &[],
        None,
    )
    .await
}

/// Componentize the world and instantiate it.
async fn instantiate() -> anyhow::Result<(Store<Ctx>, WorldResource)> {
    let component_bytes = build().await?;
    let component = Component::new(&ENGINE, &component_bytes)?;
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker)?;
    WorldResource::add_to_linker::<_, HasSelf<_>>(&mut linker, |ctx| ctx)?;
    componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
    let mut store = shared::store(&ENGINE);
    let instance = WorldResourcePre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    Ok((store, instance))
}

/// The guest would bump the host's thing through the world-level resource's
/// methods, but a world-level resource with methods cannot be finalized:
/// wac-graph 0.11 cannot encode the method imports' `borrow<thing>` outside an
/// interface, so the componentizer refuses the world with a message naming the
/// import and the workaround. `instantiate` stays for the day the composition
/// can encode it.
#[tokio::test]
async fn world_level_resource_methods_are_refused() {
    let err = build()
        .await
        .expect_err("a world-level resource with methods is refused");
    let message = format!("{err:#}");
    assert!(
        message.contains("[method]thing.get") && message.contains("interface"),
        "{message}"
    );
}

#[allow(dead_code)]
async fn world_level_resource_methods() -> anyhow::Result<()> {
    let (mut store, instance) = instantiate().await?;
    let thing = store.data_mut().wasi.table().push(ThingValue(5))?;
    instance
        .call_f(&mut store, Resource::new_borrow(thing.rep()))
        .await?;
    assert_eq!(store.data_mut().wasi.table().get(&thing)?.0, 6);
    Ok(())
}
