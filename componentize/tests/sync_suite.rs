// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Every WIT value type, exported and imported resources, and sync imports: the world of
//! `fixtures/sync_suite.{wit,js}`, whose echo exports forward to the host import of the same name.
//! The cases of a property test share one instance.

mod shared;

use {
    proptest::{
        prelude::{Just, Strategy},
        test_runner::{self, TestRng, TestRunner},
    },
    shared::Ctx,
    std::{cell::RefCell, env, sync::LazyLock},
    tokio::{runtime::Runtime, sync::OnceCell},
    wasmtime::{
        component::{HasSelf, Resource},
        Engine, Store,
    },
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
        "test:sync-suite/host-thing-interface.host-thing": ThingString,
    },
});

use test::sync_suite::echoes::{EnumType, RecordType, ResourceType, VariantType};

/// The rep of the borrow the host's `accept-borrow` received last.
static BORROWED_REP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub struct ThingString(String);

static ENGINE: LazyLock<Engine> = LazyLock::new(shared::engine);

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

        let linker = shared::linker(&ENGINE, &component, &["test:sync-suite/"], |linker| {
            SyncSuite::add_to_linker::<_, HasSelf<_>>(linker, |ctx| ctx)
        });
        SyncSuitePre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("SyncSuitePre")
    })
    .await
}

fn store() -> Store<Ctx> {
    shared::store(&ENGINE)
}

// ===========================================================================
// Host import implementations
// ===========================================================================

impl test::sync_suite::types::HostResourceType for Ctx {
    async fn drop(&mut self, v: Resource<ResourceType>) -> wasmtime::Result<()> {
        _ = v;
        Ok(())
    }
}

impl test::sync_suite::types::Host for Ctx {}

impl test::sync_suite::echoes::Host for Ctx {
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
        BORROWED_REP.store(v.rep(), std::sync::atomic::Ordering::SeqCst);
    }
    async fn echo_record(&mut self, v: RecordType) -> RecordType {
        v
    }
    async fn echo_enum(&mut self, v: EnumType) -> EnumType {
        v
    }
    async fn echo_variant(&mut self, v: VariantType) -> VariantType {
        v
    }
}

impl test::sync_suite::host_thing_interface::Host for Ctx {}

impl test::sync_suite::host_thing_interface::HostHostThing for Ctx {
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
    if let Ok(seed) = env::var("STARLING_COMPONENTIZE_TEST_SEED") {
        let bytes = hex::decode(&seed).expect("STARLING_COMPONENTIZE_TEST_SEED must be hex");
        <[u8; 32]>::try_from(bytes.as_slice()).expect("seed must be 32 bytes")
    } else {
        // Derive a non-deterministic seed without pulling in `rand`: hash the
        // current time and thread id. Reproduce a failure by re-running with
        // STARLING_COMPONENTIZE_TEST_SEED set to the printed value.
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
        "using seed {} (set STARLING_COMPONENTIZE_TEST_SEED env var to override)",
        hex::encode(seed)
    );
    seed
});

/// Run `test` on the values `strategy` generates, each call with the same store and
/// instance of the shared component.
fn proptest<S: Strategy>(
    strategy: &S,
    test: impl AsyncFn(&mut Store<Ctx>, &SyncSuite, S::Value) -> anyhow::Result<()>,
) -> anyhow::Result<()>
where
    S::Value: Send + Sync + 'static,
{
    let runtime = Runtime::new()?;
    // `Config::default()` reads `PROPTEST_CASES` itself (default 256), matching
    // it.
    let config = test_runner::Config::default();
    let algorithm = config.rng_algorithm;
    let mut runner = TestRunner::new_with_rng(config, TestRng::from_seed(algorithm, &*SEED));
    let mut store = store();
    let instance = runtime.block_on(async { pre().await.instantiate_async(&mut store).await })?;
    let store = RefCell::new(store);

    Ok(runner.run(strategy, move |v| {
        runtime
            .block_on(test(&mut store.borrow_mut(), &instance, v))
            .unwrap();
        Ok(())
    })?)
}

const MAX_SIZE: usize = 100;

// ===========================================================================
// Test cases
// ===========================================================================

#[tokio::test]
async fn echo_nothing() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    instance
        .test_sync_suite_echoes()
        .call_echo_nothing(&mut store)
        .await?;
    Ok(())
}

// ---- scalar round-trips ----

#[tokio::test]
async fn echo_bools() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    for value in [true, false] {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_bool(&mut store, value)
                .await?
        );
    }
    Ok(())
}

#[test]
fn echo_u8s() -> anyhow::Result<()> {
    proptest(&proptest::num::u8::ANY, async |store, instance, value| {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_u8(store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_s8s() -> anyhow::Result<()> {
    proptest(&proptest::num::i8::ANY, async |store, instance, value| {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_s8(store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_u16s() -> anyhow::Result<()> {
    proptest(&proptest::num::u16::ANY, async |store, instance, value| {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_u16(store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_s16s() -> anyhow::Result<()> {
    proptest(&proptest::num::i16::ANY, async |store, instance, value| {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_s16(store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_u32s() -> anyhow::Result<()> {
    proptest(&proptest::num::u32::ANY, async |store, instance, value| {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_u32(store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_s32s() -> anyhow::Result<()> {
    proptest(&proptest::num::i32::ANY, async |store, instance, value| {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_s32(store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_u64s() -> anyhow::Result<()> {
    proptest(&proptest::num::u64::ANY, async |store, instance, value| {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_u64(store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_s64s() -> anyhow::Result<()> {
    proptest(&proptest::num::i64::ANY, async |store, instance, value| {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_s64(store, value)
                .await?
        );
        Ok(())
    })
}

#[test]
fn echo_chars() -> anyhow::Result<()> {
    proptest(&proptest::char::any(), async |store, instance, value| {
        assert_eq!(
            value,
            instance
                .test_sync_suite_echoes()
                .call_echo_char(store, value)
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
    proptest(
        &proptest::num::f32::ANY.prop_map(MyF32),
        async |store, instance, value| {
            assert_eq!(
                value,
                MyF32(
                    instance
                        .test_sync_suite_echoes()
                        .call_echo_f32(store, value.0)
                        .await?
                )
            );
            Ok(())
        },
    )
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
    proptest(
        &proptest::num::f64::ANY.prop_map(MyF64),
        async |store, instance, value| {
            assert_eq!(
                value,
                MyF64(
                    instance
                        .test_sync_suite_echoes()
                        .call_echo_f64(store, value.0)
                        .await?
                )
            );
            Ok(())
        },
    )
}

#[test]
fn echo_strings() -> anyhow::Result<()> {
    proptest(
        &proptest::string::string_regex(".*")?,
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_string(store, &value)
                    .await?
            );
            Ok(())
        },
    )
}

// ---- list round-trips ----

#[test]
fn echo_lists_bool() -> anyhow::Result<()> {
    proptest(
        &proptest::collection::vec(proptest::bool::ANY, 0..MAX_SIZE),
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_bool(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_u8(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_s8(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_u16(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_s16(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_u32(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_s32(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_u64(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_s64(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_char(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_f32(store, &value.iter().map(|v| v.0).collect::<Vec<_>>())
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_f64(store, &value.iter().map(|v| v.0).collect::<Vec<_>>())
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_string(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_list_u8(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_list_list_list_u8(store, &value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_option_u8(store, value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_option_option_u8(store, value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_result_u8_u8(store, value)
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
        async |store, instance, value| {
            assert_eq!(
                value,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_result_result_u8_u8_u8(store, value)
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
        async |store, instance, ((v1, v2, v3, v4, v5, v6, v7, v8), (v9, v10, v11, v12, v13, v14, v15, v16))| {
            let (r1, r2, r3, r4, r5, r6, r7, r8, r9, r10, r11, r12, r13, r14, r15, r16) = instance
                .test_sync_suite_echoes()
                .call_echo_many(
                    store, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10.0, v11.0, v12, &v13, &v14,
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
        async |store, instance, v| {
            assert_eq!(
                v,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_record(store, &v)
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
        async |store, instance, v| {
            assert_eq!(
                v,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_enum(store, v)
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
        async |store, instance, v| {
            assert_eq!(
                v,
                instance
                    .test_sync_suite_echoes()
                    .call_echo_variant(store, &v)
                    .await?
            );
            Ok(())
        },
    )
}

// ---- resources / borrows ----

/// An owned resource handle the guest echoes keeps its rep.
#[tokio::test]
async fn owned_resource_round_trips() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    assert_eq!(
        42,
        instance
            .test_sync_suite_echoes()
            .call_echo_resource(&mut store, Resource::new_own(42))
            .await?
            .rep()
    );
    Ok(())
}

/// A `borrow<resource>` argument passes through the guest to the host import
/// with its rep.
#[tokio::test]
async fn borrowed_resource_reaches_the_import() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    instance
        .test_sync_suite_echoes()
        .call_accept_borrow(&mut store, Resource::new_borrow(42))
        .await?;
    assert_eq!(BORROWED_REP.load(std::sync::atomic::Ordering::SeqCst), 42);
    Ok(())
}

// ---- imported resource (constructor / instance method / static method / borrow) ----

/// The guest constructs a host resource, and calls its instance method and its
/// static method with a borrow of it.
#[tokio::test]
async fn guest_drives_a_host_resource() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let out = instance
        .test_sync_suite_host_thing_driver()
        .call_drive(&mut store, "hello")
        .await?;
    assert_eq!("hello|hello", out);
    Ok(())
}
