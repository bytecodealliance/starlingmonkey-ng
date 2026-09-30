// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Resource handles nested in aggregates, shared across interfaces, declared at world level, and
//! disposed during a call: the world of `fixtures/resources.{wit,js}`. Every interface but
//! `counters` is imported and exported, so the host holds the imported resources in its table, and
//! the guest wraps each one in a class of its own that the host calls through the exported
//! interface.

mod shared;

use {
    std::sync::LazyLock,
    tokio::sync::OnceCell,
    wasmtime::{
        component::{HasSelf, Resource, ResourceType},
        Engine, Store,
    },
    wasmtime_wasi::{WasiCtxView, WasiView},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/resources.wit",
    world: "resources",
    imports: { default: async },
    exports: { default: async },
    with: {
        "test:resources/aggregates.thing": AggregatesThing,
        "test:resources/borrow-in-record.thing": BorrowInRecordThing,
        "test:resources/alias1.thing": AliasThing,
        "test:resources/import-and-export.thing": ImportAndExportThing,
        "test:resources/counters.counter": HostCounterValue,
    },
});

use test::resources::{aggregates, alias1, alias2, borrow_in_record, counters, import_and_export};

/// The host `aggregates.thing`: the constructor argument plus two.
pub struct AggregatesThing(u32);

/// The host `borrow-in-record.thing`: the constructor string with a suffix.
pub struct BorrowInRecordThing(String);

/// The host `alias1.thing`, which `alias2` shares through `use`.
pub struct AliasThing(String);

/// The host `import-and-export.thing`: the constructor argument plus one.
pub struct ImportAndExportThing(u32);

/// The host `counters.counter`.
pub struct HostCounterValue(u32);

struct Ctx {
    wasi: shared::Wasi,
    /// How many times the host dropped a `counter`, so a call that disposes a
    /// borrowed counter can be checked not to have dropped the host's resource.
    counter_drops: u32,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

static ENGINE: LazyLock<Engine> = LazyLock::new(shared::engine);

/// Componentize the resources world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static ResourcesPre<Ctx> {
    static PRE: OnceCell<Result<ResourcesPre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/resources.wit"),
            "resources",
            include_str!("fixtures/resources.js"),
        )
        .await;

        let linker = shared::linker(&ENGINE, &component, &["test:resources/"], |linker| {
            Resources::add_to_linker::<_, HasSelf<_>>(linker, |ctx| ctx)?;
            // The world-level `use`s import `thing` and `counter` at the root as
            // aliases of the interfaces' resources. Bindgen registers only resources
            // the world declares itself, and `trap_unsatisfied_imports` would stub
            // the two with a resource type of its own, which the linker then rejects
            // as a mismatch, so they are defined here.
            for (name, ty) in [
                ("thing", ResourceType::host::<AliasThing>()),
                ("counter", ResourceType::host::<HostCounterValue>()),
            ] {
                linker.root().resource(name, ty, |_, _| Ok(()))?;
            }
            Ok(())
        });
        ResourcesPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("ResourcesPre")
    })
    .await
}

fn store() -> Store<Ctx> {
    Store::new(
        &ENGINE,
        Ctx {
            wasi: shared::Wasi::inherit(),
            counter_drops: 0,
        },
    )
}

// ===========================================================================
// Host import implementations
// ===========================================================================

impl aggregates::Host for Ctx {
    #[allow(clippy::too_many_arguments)]
    async fn foo(
        &mut self,
        r1: aggregates::R1,
        r2: aggregates::R2,
        r3: aggregates::R3,
        t1: aggregates::T1,
        t2: aggregates::T2,
        v1: aggregates::V1,
        v2: aggregates::V2,
        l1: aggregates::L1,
        l2: aggregates::L2,
        o1: Option<Resource<AggregatesThing>>,
        o2: Option<Resource<AggregatesThing>>,
        result1: Result<Resource<AggregatesThing>, ()>,
        result2: Result<Resource<AggregatesThing>, ()>,
    ) -> u32 {
        let table = self.wasi.table();
        // An `own` handle transfers to the host, which removes it from the table.
        // A borrow is only read.
        let mut own = |thing: Resource<AggregatesThing>| table.delete(thing).unwrap().0;
        let mut sum = own(r1.thing);
        let aggregates::V1::Thing(v1) = v1;
        sum += own(r3.thing2) + own(t1.0) + own(t1.1.thing) + own(v1);
        for thing in l1 {
            sum += own(thing);
        }
        if let Some(thing) = o1 {
            sum += own(thing);
        }
        if let Ok(thing) = result1 {
            sum += own(thing);
        }
        let borrow = |thing: &Resource<AggregatesThing>| table.get(thing).unwrap().0;
        sum += borrow(&r2.thing) + borrow(&r3.thing1) + borrow(&t2.0);
        let aggregates::V2::Thing(v2) = v2;
        sum += borrow(&v2);
        for thing in &l2 {
            sum += borrow(thing);
        }
        if let Some(thing) = &o2 {
            sum += borrow(thing);
        }
        if let Ok(thing) = &result2 {
            sum += borrow(thing);
        }
        sum + 3
    }
}

impl aggregates::HostThing for Ctx {
    async fn new(&mut self, v: u32) -> Resource<AggregatesThing> {
        self.wasi.table().push(AggregatesThing(v + 2)).unwrap()
    }
    async fn drop(&mut self, rep: Resource<AggregatesThing>) -> wasmtime::Result<()> {
        self.wasi.table().delete(rep)?;
        Ok(())
    }
}

impl borrow_in_record::Host for Ctx {
    async fn test(&mut self, a: Vec<borrow_in_record::Foo>) -> Vec<Resource<BorrowInRecordThing>> {
        let table = self.wasi.table();
        a.into_iter()
            .map(|record| {
                let value = table.get(&record.thing).unwrap().0.clone();
                table
                    .push(BorrowInRecordThing(value + " test HostThing"))
                    .unwrap()
            })
            .collect()
    }
}

impl borrow_in_record::HostThing for Ctx {
    async fn new(&mut self, s: String) -> Resource<BorrowInRecordThing> {
        self.wasi
            .table()
            .push(BorrowInRecordThing(s + " HostThing"))
            .unwrap()
    }
    async fn get(&mut self, self_: Resource<BorrowInRecordThing>) -> String {
        self.wasi.table().get(&self_).unwrap().0.clone() + " HostThing.get"
    }
    async fn drop(&mut self, rep: Resource<BorrowInRecordThing>) -> wasmtime::Result<()> {
        self.wasi.table().delete(rep)?;
        Ok(())
    }
}

impl alias1::Host for Ctx {
    async fn a(&mut self, f: alias1::Foo) -> Vec<Resource<AliasThing>> {
        vec![f.thing]
    }
}

impl alias1::HostThing for Ctx {
    async fn new(&mut self, s: String) -> Resource<AliasThing> {
        self.wasi
            .table()
            .push(AliasThing(s + " HostThing"))
            .unwrap()
    }
    async fn get(&mut self, self_: Resource<AliasThing>) -> String {
        self.wasi.table().get(&self_).unwrap().0.clone() + " HostThing.get"
    }
    async fn drop(&mut self, rep: Resource<AliasThing>) -> wasmtime::Result<()> {
        self.wasi.table().delete(rep)?;
        Ok(())
    }
}

impl alias2::Host for Ctx {
    async fn b(&mut self, f: alias2::Foo, g: alias2::Bar) -> Vec<Resource<AliasThing>> {
        vec![f.thing, g.thing]
    }
}

impl import_and_export::Host for Ctx {}

impl import_and_export::HostThing for Ctx {
    async fn new(&mut self, v: u32) -> Resource<ImportAndExportThing> {
        self.wasi.table().push(ImportAndExportThing(v + 1)).unwrap()
    }
    async fn foo(&mut self, self_: Resource<ImportAndExportThing>) -> u32 {
        self.wasi.table().get(&self_).unwrap().0 + 2
    }
    async fn bar(&mut self, self_: Resource<ImportAndExportThing>, v: u32) {
        self.wasi.table().get_mut(&self_).unwrap().0 = v + 3;
    }
    async fn baz(
        &mut self,
        a: Resource<ImportAndExportThing>,
        b: Resource<ImportAndExportThing>,
    ) -> Resource<ImportAndExportThing> {
        let table = self.wasi.table();
        let a = table.delete(a).unwrap().0;
        let b = table.delete(b).unwrap().0;
        // `foo` on each, plus four, through the constructor's plus one.
        table
            .push(ImportAndExportThing((a + 2) + (b + 2) + 4 + 1))
            .unwrap()
    }
    async fn drop(&mut self, rep: Resource<ImportAndExportThing>) -> wasmtime::Result<()> {
        self.wasi.table().delete(rep)?;
        Ok(())
    }
}

impl counters::Host for Ctx {
    async fn reject(
        &mut self,
        c: Resource<HostCounterValue>,
    ) -> Result<u32, Resource<HostCounterValue>> {
        Err(c)
    }
}

impl counters::HostCounter for Ctx {
    async fn new(&mut self, v: u32) -> Resource<HostCounterValue> {
        self.wasi.table().push(HostCounterValue(v)).unwrap()
    }
    async fn get(&mut self, self_: Resource<HostCounterValue>) -> u32 {
        self.wasi.table().get(&self_).unwrap().0
    }
    async fn set(&mut self, self_: Resource<HostCounterValue>, v: u32) {
        self.wasi.table().get_mut(&self_).unwrap().0 = v;
    }
    async fn drop(&mut self, rep: Resource<HostCounterValue>) -> wasmtime::Result<()> {
        self.counter_drops += 1;
        self.wasi.table().delete(rep)?;
        Ok(())
    }
}

// ===========================================================================
// Test cases
// ===========================================================================

/// Own and borrow handles of the exported `thing` in every aggregate position.
/// The guest unwraps each one to the host thing inside, passes the same shape
/// to the host import, and adds a constant on the way back, so the one return
/// value checks every position in both directions.
#[tokio::test]
async fn resources_nested_in_aggregates_round_trip() -> anyhow::Result<()> {
    use exports::test::resources::aggregates::{R1, R2, R3, V1, V2};

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let aggregates = instance.test_resources_aggregates();

    let mut things = Vec::new();
    let mut expected = 0;
    for i in 1..18 {
        things.push(aggregates.thing().call_constructor(&mut store, i).await?);
        // The guest constructor adds one, the host constructor two.
        expected += i + 1 + 2;
    }
    // The host's constant, then the guest's.
    expected += 3 + 4;

    let sum = aggregates
        .call_foo(
            &mut store,
            R1 { thing: things[0] },
            R2 { thing: things[1] },
            R3 {
                thing1: things[2],
                thing2: things[3],
            },
            (things[4], R1 { thing: things[5] }),
            (things[6],),
            V1::Thing(things[7]),
            V2::Thing(things[8]),
            &vec![things[9], things[10]],
            &vec![things[11], things[12]],
            Some(things[13]),
            Some(things[14]),
            Ok(things[15]),
            Ok(things[16]),
        )
        .await?;
    assert_eq!(sum, expected);

    // The borrowed things stay owned by the host and are dropped here. The
    // owned ones transferred into the guest.
    for index in [1, 2, 6, 8, 11, 12, 14, 16] {
        things[index].resource_drop_async(&mut store).await?;
    }
    Ok(())
}

/// A `list<record>` of borrows in, a `list<own>` out: each borrowed thing is
/// read through the record and a fresh owned thing returned in its place.
#[tokio::test]
async fn borrows_in_records_return_owned_things() -> anyhow::Result<()> {
    use exports::test::resources::borrow_in_record::Foo;

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let iface = instance.test_resources_borrow_in_record();

    let thing1 = iface
        .thing()
        .call_constructor(&mut store, "Bonjour")
        .await?;
    let thing2 = iface
        .thing()
        .call_constructor(&mut store, "mon cher")
        .await?;

    let returned = iface
        .call_test(&mut store, &[Foo { thing: thing1 }, Foo { thing: thing2 }])
        .await?;
    let mut values = Vec::new();
    for thing in &returned {
        values.push(iface.thing().call_get(&mut store, *thing).await?);
    }
    assert_eq!(
        values,
        [
            "Bonjour Thing HostThing test HostThing HostThing.get Thing.get",
            "mon cher Thing HostThing test HostThing HostThing.get Thing.get",
        ]
    );

    for thing in returned.into_iter().chain([thing1, thing2]) {
        thing.resource_drop_async(&mut store).await?;
    }
    Ok(())
}

/// One resource type shared by two interfaces through `use` and `use ... as`,
/// and by the world through `use alias1.{thing}`: `alias2.b` takes a record of
/// its own and the aliased `alias1.foo`, and the world-level `test` passes host
/// things through the guest unchanged.
#[tokio::test]
async fn aliased_resources_are_one_type() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;

    // World-level `test`: host-owned things pass through the guest and come
    // back as the same table entries.
    let host_thing = store
        .data_mut()
        .wasi
        .table()
        .push(AliasThing("Ni Hao HostThing".to_string()))?;
    let returned = instance.call_test(&mut store, &[host_thing]).await?;
    let values: Vec<String> = returned
        .into_iter()
        .map(|thing| store.data_mut().wasi.table().delete(thing).unwrap().0)
        .collect();
    assert_eq!(values, ["Ni Hao HostThing"]);

    let alias1 = instance.test_resources_alias1();
    let alias2 = instance.test_resources_alias2();

    let thing2 = alias1.thing().call_constructor(&mut store, "Ciao").await?;
    let returned = alias1
        .call_a(
            &mut store,
            exports::test::resources::alias1::Foo { thing: thing2 },
        )
        .await?;
    let mut values = Vec::new();
    for thing in &returned {
        values.push(alias1.thing().call_get(&mut store, *thing).await?);
    }
    assert_eq!(values, ["Ciao Thing HostThing HostThing.get Thing.get"]);
    for thing in returned {
        thing.resource_drop_async(&mut store).await?;
    }

    let thing3 = alias1.thing().call_constructor(&mut store, "Ciao").await?;
    let thing4 = alias1.thing().call_constructor(&mut store, "Aloha").await?;
    let returned = alias2
        .call_b(
            &mut store,
            exports::test::resources::alias2::Foo { thing: thing3 },
            exports::test::resources::alias2::Bar { thing: thing4 },
        )
        .await?;
    let mut values = Vec::new();
    for thing in &returned {
        values.push(alias1.thing().call_get(&mut store, *thing).await?);
    }
    assert_eq!(
        values,
        [
            "Ciao Thing HostThing HostThing.get Thing.get",
            "Aloha Thing HostThing HostThing.get Thing.get",
        ]
    );
    for thing in returned {
        thing.resource_drop_async(&mut store).await?;
    }
    Ok(())
}

/// The same interface imported and exported: the guest class wraps the host
/// class, and its static `baz` passes two owned guest things down to the host
/// static and wraps the host's result.
#[tokio::test]
async fn guest_class_wraps_the_host_class_of_the_same_interface() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let thing = instance.test_resources_import_and_export().thing();

    let thing1 = thing.call_constructor(&mut store, 42).await?;
    assert_eq!(
        thing.call_foo(&mut store, thing1).await?,
        42 + 1 + 1 + 2 + 2
    );

    thing.call_bar(&mut store, thing1, 33).await?;
    assert_eq!(
        thing.call_foo(&mut store, thing1).await?,
        33 + 3 + 3 + 2 + 2
    );

    let thing2 = thing.call_constructor(&mut store, 81).await?;
    let thing3 = thing.call_baz(&mut store, thing1, thing2).await?;
    assert_eq!(
        thing.call_foo(&mut store, thing3).await?,
        33 + 3 + 3 + 81 + 1 + 1 + 2 + 2 + 4 + 1 + 2 + 4 + 1 + 1 + 2 + 2
    );
    thing3.resource_drop_async(&mut store).await?;
    Ok(())
}

/// An export that disposes its borrowed host resource before returning. The
/// early dispose drops the borrow handle, and the release at call end finds no
/// handle left to drop, so the call completes, the host's own resource is
/// untouched, and the next call on it still works.
#[tokio::test]
async fn dispose_borrow_during_call() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;

    let counter = store.data_mut().wasi.table().push(HostCounterValue(7))?;
    let value = instance
        .call_dispose_early(&mut store, Resource::new_borrow(counter.rep()))
        .await?;
    assert_eq!(value, 7);
    assert_eq!(
        store.data().counter_drops,
        0,
        "disposing a borrow must not drop the host's resource"
    );
    assert_eq!(store.data_mut().wasi.table().get(&counter)?.0, 7);

    instance
        .call_bump(&mut store, Resource::new_borrow(counter.rep()))
        .await?;
    assert_eq!(store.data_mut().wasi.table().get(&counter)?.0, 8);

    let value = instance
        .call_dispose_early(&mut store, Resource::new_borrow(counter.rep()))
        .await?;
    assert_eq!(value, 8);
    assert_eq!(store.data().counter_drops, 0);
    Ok(())
}

/// Calling a resource class's `[Symbol.dispose]` on an object that is not a
/// wrapper throws a `TypeError`, for a plain object and for an object with a
/// wrapper's prototype alike, and doesn't drop a handle.
#[tokio::test]
async fn forged_dispose_throws() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counter = store.data_mut().wasi.table().push(HostCounterValue(1))?;
    let messages = instance
        .call_forged_dispose_messages(&mut store, Resource::new_borrow(counter.rep()))
        .await?;
    assert_eq!(
        messages,
        [
            "dispose was called on an object that is not a resource wrapper",
            "dispose was called on an object that is not a resource wrapper",
        ]
    );
    assert_eq!(store.data().counter_drops, 0);
    Ok(())
}

/// An owned imported resource disposed by a `using` block is dropped by the
/// host during the call, and a second dispose does nothing.
#[tokio::test]
async fn using_disposes_an_owned_wrapper_once() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counter = store.data_mut().wasi.table().push(HostCounterValue(4))?;
    assert_eq!(instance.call_using_owned(&mut store, counter).await?, 4);
    assert_eq!(store.data().counter_drops, 1);
    Ok(())
}

/// An `err` whose type is a resource arrives as a `ComponentError` holding the
/// resource's wrapper.
#[tokio::test]
async fn resource_err_is_a_component_error() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counter = store.data_mut().wasi.table().push(HostCounterValue(7))?;
    assert_eq!(
        instance
            .call_describe_resource_error(&mut store, counter)
            .await?,
        "ComponentError true 7"
    );
    Ok(())
}

/// A plain object passed where an imported resource is expected throws a
/// `TypeError` the guest can catch, rather than trapping.
#[tokio::test]
async fn plain_object_for_imported_resource_throws() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let described = instance.call_plain_object_for_resource(&mut store).await?;
    assert!(
        described.starts_with(
            "true argument 1 of `reject`: expected an instance of the imported resource's class"
        ),
        "{described}"
    );
    Ok(())
}

/// Resource wrappers passed to imports in ways that do not lower throw a
/// `TypeError` the guest can catch, before anything is lowered.
#[tokio::test]
async fn resource_misuse_throws() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let counter = store.data_mut().wasi.table().push(HostCounterValue(0))?;
    let messages = instance
        .call_resource_misuse_messages(&mut store, Resource::new_borrow(counter.rep()))
        .await?;
    let used_elsewhere =
        "the resource is used elsewhere in the same call, and an `own` use must be its only use";
    assert_eq!(
        messages,
        [
            format!("argument 2 of `Thing.baz`: {used_elsewhere}"),
            format!("argument 2 of `b` `.thing`: {used_elsewhere}"),
            "argument 1 of `reject`: expected an owned resource, got a borrowed one".to_string(),
            "argument 1 of `b` `.thing`: the resource was disposed of, or moved to the host"
                .to_string(),
            "2".to_string(),
            "no error".to_string(),
        ]
    );
    Ok(())
}

/// A payload-less `err` takes a thrown `Error`, and the error is logged on
/// stderr.
#[tokio::test]
async fn payload_less_err_logs_the_error() -> anyhow::Result<()> {
    let (wasi, stderr) = shared::wasi_capturing_stderr();
    let mut store = Store::new(
        &ENGINE,
        Ctx {
            wasi,
            counter_drops: 0,
        },
    );
    let instance = pre().await.instantiate_async(&mut store).await?;
    assert_eq!(instance.call_fail_quietly(&mut store).await?, Err(()));
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("the reason for failing quietly"),
        "{stderr}"
    );
    Ok(())
}
