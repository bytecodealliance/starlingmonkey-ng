// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The sync subset of the componentize test suite, run against the
// starling-based componentize pipeline.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// This exercises every WIT value type, exported and
// imported resources, and sync imports through the REAL componentize pipeline,
// including proptest round-trips. The async, stream and future cases are
// excluded here. See
// `sync_suite.wit`.
//
// One world (`sync-suite`) is componentized once via a `OnceCell` and shared
// across every case, since componentizing takes seconds. Each case gets a fresh
// `Store`.

mod shared;

use {
    proptest::{
        prelude::{Just, Strategy},
        test_runner::{self, TestRng, TestRunner},
    },
    std::{env, sync::LazyLock},
    tokio::{runtime::Runtime, sync::OnceCell},
    wasmtime::{
        component::{HasSelf, Linker, Resource},
        Engine, Store,
    },
    wasmtime_wasi::{WasiCtxView, WasiView},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/sync_suite.wit",
    world: "sync-suite",
    imports: { default: async },
    exports: { default: async },
    additional_derives: [PartialEq, Eq],
    with: {
        // wasmtime's bindgen keys a resource as `<interface-id>.<resource>`
        // (the interface id has its own `pkg/iface` separator).
        "componentize-js:tests/host-thing-interface.host-thing": ThingString,
    },
});

use componentize_js::tests::echoes::{EnumType, FlagsType, RecordType, ResourceType, VariantType};

pub struct ThingString(String);

struct Ctx {
    wasi: shared::Wasi,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

static ENGINE: LazyLock<Engine> = LazyLock::new(shared::engine);

fn add_to_linker(linker: &mut Linker<Ctx>) -> anyhow::Result<()> {
    componentize::add_wasi(linker)?;
    SyncSuite::add_to_linker::<_, HasSelf<_>>(linker, |ctx| ctx)?;
    Ok(())
}

/// Componentize the sync-suite world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static SyncSuitePre<Ctx> {
    static PRE: OnceCell<Result<SyncSuitePre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/sync_suite.wit"),
            "sync-suite",
            include_str!("fixtures/sync_suite.js"),
        )
        .await;

        let mut linker = Linker::new(&ENGINE);
        add_to_linker(&mut linker).expect("add_to_linker");
        // The wizened component still declares the runtime's transitive
        // imports, since Wizer does not strip them, so trap-stub the ones our world does
        // not provide. They are never called by the suite.
        componentize::trap_unsatisfied_imports(
            &ENGINE,
            &component,
            &mut linker,
            &["componentize-js:tests/"],
        )
        .expect("trap-stub unknown imports");

        SyncSuitePre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("SyncSuitePre")
    })
    .await
}

fn store() -> Store<Ctx> {
    let wasi = shared::Wasi::inherit();
    Store::new(&ENGINE, Ctx { wasi })
}

// ===========================================================================
// Host import implementations
// ===========================================================================

impl componentize_js::tests::simple_import_and_export::Host for Ctx {
    async fn foo(&mut self, v: u32) -> u32 {
        v + 2
    }
}

impl componentize_js::tests::types::HostResourceType for Ctx {
    async fn drop(&mut self, v: Resource<ResourceType>) -> wasmtime::Result<()> {
        _ = v;
        Ok(())
    }
}

impl componentize_js::tests::types::Host for Ctx {}

impl componentize_js::tests::echoes::Host for Ctx {
    async fn echo_nothing(&mut self) {}
    async fn echo_bool(&mut self, v: bool) -> bool {
        v
    }
    async fn echo_u8(&mut self, v: u8) -> u8 {
        v
    }
    async fn echo_s8(&mut self, v: i8) -> i8 {
        v
    }
    async fn echo_u16(&mut self, v: u16) -> u16 {
        v
    }
    async fn echo_s16(&mut self, v: i16) -> i16 {
        v
    }
    async fn echo_u32(&mut self, v: u32) -> u32 {
        v
    }
    async fn echo_s32(&mut self, v: i32) -> i32 {
        v
    }
    async fn echo_char(&mut self, v: char) -> char {
        v
    }
    async fn echo_u64(&mut self, v: u64) -> u64 {
        v
    }
    async fn echo_s64(&mut self, v: i64) -> i64 {
        v
    }
    async fn echo_f32(&mut self, v: f32) -> f32 {
        v
    }
    async fn echo_f64(&mut self, v: f64) -> f64 {
        v
    }
    async fn echo_string(&mut self, v: String) -> String {
        v
    }
    async fn echo_list_bool(&mut self, v: Vec<bool>) -> Vec<bool> {
        v
    }
    async fn echo_list_u8(&mut self, v: Vec<u8>) -> Vec<u8> {
        v
    }
    async fn echo_list_s8(&mut self, v: Vec<i8>) -> Vec<i8> {
        v
    }
    async fn echo_list_u16(&mut self, v: Vec<u16>) -> Vec<u16> {
        v
    }
    async fn echo_list_s16(&mut self, v: Vec<i16>) -> Vec<i16> {
        v
    }
    async fn echo_list_u32(&mut self, v: Vec<u32>) -> Vec<u32> {
        v
    }
    async fn echo_list_s32(&mut self, v: Vec<i32>) -> Vec<i32> {
        v
    }
    async fn echo_list_char(&mut self, v: Vec<char>) -> Vec<char> {
        v
    }
    async fn echo_list_u64(&mut self, v: Vec<u64>) -> Vec<u64> {
        v
    }
    async fn echo_list_s64(&mut self, v: Vec<i64>) -> Vec<i64> {
        v
    }
    async fn echo_list_f32(&mut self, v: Vec<f32>) -> Vec<f32> {
        v
    }
    async fn echo_list_f64(&mut self, v: Vec<f64>) -> Vec<f64> {
        v
    }
    async fn echo_list_string(&mut self, v: Vec<String>) -> Vec<String> {
        v
    }
    async fn echo_list_list_u8(&mut self, v: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        v
    }
    async fn echo_list_list_list_u8(&mut self, v: Vec<Vec<Vec<u8>>>) -> Vec<Vec<Vec<u8>>> {
        v
    }
    async fn echo_option_u8(&mut self, v: Option<u8>) -> Option<u8> {
        v
    }
    async fn echo_option_option_u8(&mut self, v: Option<Option<u8>>) -> Option<Option<u8>> {
        v
    }
    async fn echo_result_u8_u8(&mut self, v: Result<u8, u8>) -> Result<u8, u8> {
        v
    }
    async fn echo_result_result_u8_u8_u8(
        &mut self,
        v: Result<Result<u8, u8>, u8>,
    ) -> Result<Result<u8, u8>, u8> {
        v
    }
    #[allow(clippy::type_complexity)]
    async fn echo_many(
        &mut self,
        v1: bool,
        v2: u8,
        v3: u16,
        v4: u32,
        v5: u64,
        v6: i8,
        v7: i16,
        v8: i32,
        v9: i64,
        v10: f32,
        v11: f64,
        v12: char,
        v13: String,
        v14: Vec<bool>,
        v15: Vec<u8>,
        v16: Vec<u16>,
    ) -> (
        bool,
        u8,
        u16,
        u32,
        u64,
        i8,
        i16,
        i32,
        i64,
        f32,
        f64,
        char,
        String,
        Vec<bool>,
        Vec<u8>,
        Vec<u16>,
    ) {
        (
            v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15, v16,
        )
    }
    async fn echo_resource(&mut self, v: Resource<ResourceType>) -> Resource<ResourceType> {
        v
    }
    async fn accept_borrow(&mut self, v: Resource<ResourceType>) {
        _ = v;
    }
    async fn echo_record(&mut self, v: RecordType) -> RecordType {
        v
    }
    async fn echo_enum(&mut self, v: EnumType) -> EnumType {
        v
    }
    async fn echo_flags(&mut self, v: FlagsType) -> FlagsType {
        v
    }
    async fn echo_variant(&mut self, v: VariantType) -> VariantType {
        v
    }
}

impl componentize_js::tests::host_thing_interface::Host for Ctx {}

impl componentize_js::tests::host_thing_interface::HostHostThing for Ctx {
    async fn new(&mut self, s: String) -> Resource<ThingString> {
        self.wasi.table().push(ThingString(s)).unwrap()
    }
    async fn get(&mut self, self_: Resource<ThingString>) -> String {
        self.wasi.table().get(&self_).unwrap().0.clone()
    }
    async fn get_static(&mut self, v: Resource<ThingString>) -> String {
        self.wasi.table().get(&v).unwrap().0.clone()
    }
    async fn drop(&mut self, rep: Resource<ThingString>) -> wasmtime::Result<()> {
        self.wasi.table().delete(rep)?;
        Ok(())
    }
}

// ===========================================================================
// proptest harness
// ===========================================================================

fn get_seed() -> [u8; 32] {
    if let Ok(seed) = env::var("COMPONENTIZE_JS_TEST_SEED") {
        let bytes = hex::decode(&seed).expect("COMPONENTIZE_JS_TEST_SEED must be hex");
        <[u8; 32]>::try_from(bytes.as_slice()).expect("seed must be 32 bytes")
    } else {
        // Derive a non-deterministic seed without pulling in `rand`: hash the
        // current time and thread id. Reproduce a failure by re-running with
        // COMPONENTIZE_JS_TEST_SEED set to the printed value.
        use std::hash::{Hash, Hasher};
        let mut seed = [0u8; 32];
        for (i, chunk) in seed.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::time::SystemTime::now().hash(&mut h);
            std::thread::current().id().hash(&mut h);
            i.hash(&mut h);
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
        seed
    }
}

static SEED: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let seed = get_seed();
    eprintln!(
        "using seed {} (set COMPONENTIZE_JS_TEST_SEED env var to override)",
        hex::encode(seed)
    );
    seed
});

fn proptest<S: Strategy>(
    strategy: &S,
    test: impl AsyncFn(S::Value) -> anyhow::Result<()>,
) -> anyhow::Result<()>
where
    S::Value: Send + Sync + 'static,
{
    let runtime = Runtime::new()?;
    // `Config::default()` reads `PROPTEST_CASES` itself (default 256), matching
    // it. Each case re-instantiates the shared component and crosses the wasm
    // boundary. Instantiation is cheap (the one-time componentize
    // dominates), so the full default count runs in seconds.
    let config = test_runner::Config::default();
    let algorithm = config.rng_algorithm;
    let mut runner = TestRunner::new_with_rng(config, TestRng::from_seed(algorithm, &*SEED));

    Ok(runner.run(strategy, move |v| {
        runtime.block_on(test(v)).unwrap();
        Ok(())
    })?)
}

const MAX_SIZE: usize = 100;

// ===========================================================================
// Test cases
// ===========================================================================

#[tokio::test]
async fn simple_export() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    assert_eq!(
        42 + 3,
        instance
            .componentize_js_tests_simple_export()
            .call_foo(&mut store, 42)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn simple_import_and_export() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    assert_eq!(
        42 + 3 + 2,
        instance
            .componentize_js_tests_simple_import_and_export()
            .call_foo(&mut store, 42)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn echo_nothing() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    instance
        .componentize_js_tests_echoes()
        .call_echo_nothing(&mut store)
        .await?;
    Ok(())
}

// ---- scalar round-trips ----

#[test]
fn echo_bools() -> anyhow::Result<()> {
    proptest(&proptest::bool::ANY, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_bool(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_u8s() -> anyhow::Result<()> {
    proptest(&proptest::num::u8::ANY, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_u8(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_s8s() -> anyhow::Result<()> {
    proptest(&proptest::num::i8::ANY, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_s8(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_u16s() -> anyhow::Result<()> {
    proptest(&proptest::num::u16::ANY, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_u16(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_s16s() -> anyhow::Result<()> {
    proptest(&proptest::num::i16::ANY, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_s16(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_u32s() -> anyhow::Result<()> {
    proptest(&proptest::num::u32::ANY, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_u32(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_s32s() -> anyhow::Result<()> {
    proptest(&proptest::num::i32::ANY, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_s32(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_u64s() -> anyhow::Result<()> {
    proptest(&proptest::num::u64::ANY, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_u64(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_s64s() -> anyhow::Result<()> {
    proptest(&proptest::num::i64::ANY, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_s64(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_chars() -> anyhow::Result<()> {
    proptest(&proptest::char::any(), async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_char(&mut store, value)
                .await?
        );
        Ok(())
    })
}

#[derive(Debug, Copy, Clone)]
struct MyF32(f32);

impl PartialEq<MyF32> for MyF32 {
    fn eq(&self, other: &Self) -> bool {
        (self.0.is_nan() && other.0.is_nan()) || (self.0 == other.0)
    }
}

#[test]
fn echo_f32s() -> anyhow::Result<()> {
    proptest(&proptest::num::f32::ANY.prop_map(MyF32), async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            MyF32(
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_f32(&mut store, value.0)
                    .await?
            )
        );
        Ok(())
    })
}

#[derive(Debug, Copy, Clone)]
struct MyF64(f64);

impl PartialEq<MyF64> for MyF64 {
    fn eq(&self, other: &Self) -> bool {
        (self.0.is_nan() && other.0.is_nan()) || (self.0 == other.0)
    }
}

#[test]
fn echo_f64s() -> anyhow::Result<()> {
    proptest(&proptest::num::f64::ANY.prop_map(MyF64), async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            MyF64(
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_f64(&mut store, value.0)
                    .await?
            )
        );
        Ok(())
    })
}

#[test]
fn echo_strings() -> anyhow::Result<()> {
    proptest(&proptest::string::string_regex(".*")?, async |value| {
        let mut store = store();
        let instance = pre().await.instantiate_async(&mut store).await?;
        assert_eq!(
            value,
            instance
                .componentize_js_tests_echoes()
                .call_echo_string(&mut store, &value)
                .await?
        );
        Ok(())
    })
}

// ---- list round-trips ----

#[test]
fn echo_lists_bool() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::bool::ANY, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_bool(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_u8() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::u8::ANY, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_u8(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_s8() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::i8::ANY, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_s8(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_u16() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::u16::ANY, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_u16(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_s16() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::i16::ANY, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_s16(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_u32() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::u32::ANY, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_u32(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_s32() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::i32::ANY, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_s32(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_u64() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::u64::ANY, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_u64(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_s64() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::i64::ANY, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_s64(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_char() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::char::any(), 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_char(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_f32() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::f32::ANY.prop_map(MyF32), 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_f32(&mut store, &value.iter().map(|v| v.0).collect::<Vec<_>>())
                    .await?
                    .into_iter()
                    .map(MyF32)
                    .collect::<Vec<_>>()
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_f64() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::num::f64::ANY.prop_map(MyF64), 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_f64(&mut store, &value.iter().map(|v| v.0).collect::<Vec<_>>())
                    .await?
                    .into_iter()
                    .map(MyF64)
                    .collect::<Vec<_>>()
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_string() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::string::string_regex(".*")?, 0..MAX_SIZE),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_string(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_list_u8() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(
            proptest::collection::vec(proptest::num::u8::ANY, 0..MAX_SIZE / 2),
            0..MAX_SIZE,
        ),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_list_u8(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_lists_list_list_u8() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(
            proptest::collection::vec(
                proptest::collection::vec(proptest::num::u8::ANY, 0..MAX_SIZE / 4),
                0..MAX_SIZE / 2,
            ),
            0..MAX_SIZE,
        ),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_list_list_list_u8(&mut store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

// ---- option / result round-trips ----

#[test]
fn echo_options_u8() -> anyhow::Result<()> {
    proptest(
        &proptest::option::of(proptest::num::u8::ANY),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_option_u8(&mut store, value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_options_option_u8() -> anyhow::Result<()> {
    proptest(
        &proptest::option::of(proptest::option::of(proptest::num::u8::ANY)),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_option_option_u8(&mut store, value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_results_u8_u8() -> anyhow::Result<()> {
    proptest(
        &proptest::result::maybe_ok(proptest::num::u8::ANY, proptest::num::u8::ANY),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_result_u8_u8(&mut store, value)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_results_result_u8_u8_u8() -> anyhow::Result<()> {
    proptest(
        &proptest::result::maybe_ok(
            proptest::result::maybe_ok(proptest::num::u8::ANY, proptest::num::u8::ANY),
            proptest::num::u8::ANY,
        ),
        async |value| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                value,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_result_result_u8_u8_u8(&mut store, value)
                    .await?
            );
            Ok(())
        },
    )
}

// ---- echo_many (tuple of every scalar + a few lists) ----

#[test]
fn echo_many() -> anyhow::Result<()> {
    proptest(
        &(
            (
                proptest::bool::ANY,
                proptest::num::u8::ANY,
                proptest::num::u16::ANY,
                proptest::num::u32::ANY,
                proptest::num::u64::ANY,
                proptest::num::i8::ANY,
                proptest::num::i16::ANY,
                proptest::num::i32::ANY,
            ),
            (
                proptest::num::i64::ANY,
                proptest::num::f32::ANY.prop_map(MyF32),
                proptest::num::f64::ANY.prop_map(MyF64),
                proptest::char::any(),
                proptest::string::string_regex(".*")?,
                proptest::collection::vec(proptest::bool::ANY, 0..MAX_SIZE),
                proptest::collection::vec(proptest::num::u8::ANY, 0..MAX_SIZE),
                proptest::collection::vec(proptest::num::u16::ANY, 0..MAX_SIZE),
            ),
        ),
        async |((v1, v2, v3, v4, v5, v6, v7, v8), (v9, v10, v11, v12, v13, v14, v15, v16))| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            let (r1, r2, r3, r4, r5, r6, r7, r8, r9, r10, r11, r12, r13, r14, r15, r16) = instance
                .componentize_js_tests_echoes()
                .call_echo_many(
                    &mut store, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10.0, v11.0, v12, &v13, &v14,
                    &v15, &v16,
                )
                .await?;
            assert_eq!(
                (
                    (v1, v2, v3, v4, v5, v6, v7, v8),
                    (v9, v10, v11, v12, v13, v14, v15, v16)
                ),
                (
                    (r1, r2, r3, r4, r5, r6, r7, r8),
                    (r9, MyF32(r10), MyF64(r11), r12, r13, r14, r15, r16)
                ),
            );
            Ok(())
        },
    )
}

// ---- record / enum / flags / variant round-trips ----

#[test]
fn echo_records() -> anyhow::Result<()> {
    proptest(
        &(
            proptest::num::u32::ANY,
            proptest::string::string_regex(".*")?,
            proptest::bool::ANY.prop_flat_map(|v| {
                if v {
                    proptest::num::u32::ANY.prop_map(Ok).boxed()
                } else {
                    proptest::num::u64::ANY.prop_map(Err).boxed()
                }
            }),
        )
            .prop_map(|(a, b, c)| RecordType { a, b, c }),
        async |v| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                v,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_record(&mut store, &v)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_enums() -> anyhow::Result<()> {
    proptest(
        &(0..3).prop_map(|v| match v {
            0 => EnumType::A,
            1 => EnumType::B,
            2 => EnumType::C,
            _ => unreachable!(),
        }),
        async |v| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                v,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_enum(&mut store, v)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_flags() -> anyhow::Result<()> {
    proptest(
        &(
            proptest::bool::ANY,
            proptest::bool::ANY,
            proptest::bool::ANY,
        )
            .prop_map(|(a, b, c)| {
                let mut flags = FlagsType::default();
                if a {
                    flags |= FlagsType::A;
                }
                if b {
                    flags |= FlagsType::B;
                }
                if c {
                    flags |= FlagsType::C;
                }
                flags
            }),
        async |v| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                v,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_flags(&mut store, v)
                    .await?
            );
            Ok(())
        },
    )
}

#[test]
fn echo_variants() -> anyhow::Result<()> {
    proptest(
        &(0..5).prop_flat_map(|v| match v {
            0 => proptest::num::u32::ANY.prop_map(VariantType::A).boxed(),
            1 => proptest::string::string_regex(".*")
                .unwrap()
                .prop_map(VariantType::B)
                .boxed(),
            2 => proptest::num::u32::ANY
                .prop_map(|v| VariantType::C(Ok(v)))
                .boxed(),
            3 => proptest::num::u64::ANY
                .prop_map(|v| VariantType::C(Err(v)))
                .boxed(),
            4 => Just(VariantType::D).boxed(),
            _ => unreachable!(),
        }),
        async |v| {
            let mut store = store();
            let instance = pre().await.instantiate_async(&mut store).await?;
            assert_eq!(
                v,
                instance
                    .componentize_js_tests_echoes()
                    .call_echo_variant(&mut store, &v)
                    .await?
            );
            Ok(())
        },
    )
}

// ---- resources / borrows ----

#[tokio::test]
async fn echo_resource() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    assert_eq!(
        42,
        instance
            .componentize_js_tests_echoes()
            .call_echo_resource(&mut store, Resource::new_own(42))
            .await?
            .rep()
    );
    Ok(())
}

// Verifies a `borrow<resource>` argument lowers from the host and lifts into the
// guest across the canonical ABI without trapping (a malformed handle would trap
// in `.await?`). It does NOT assert on the borrow's rep value: the export returns
// nothing and the host impl ignores `v`, so a corrupted-but-still-valid rep would
// not be caught, since checking that would need the export to return a value derived
// from the borrow (a WIT/fixture change).
#[tokio::test]
async fn accept_borrow() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    instance
        .componentize_js_tests_echoes()
        .call_accept_borrow(&mut store, Resource::new_borrow(42))
        .await?;
    Ok(())
}

// ---- imported resource (constructor / instance method / static method / borrow) ----

#[tokio::test]
async fn host_thing_driver() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let out = instance
        .componentize_js_tests_host_thing_driver()
        .call_drive(&mut store, "hello")
        .await?;
    assert_eq!("hello|hello", out);
    Ok(())
}
