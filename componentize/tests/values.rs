// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Value shapes beyond the sync suite's echoes, through the real componentize
// pipeline.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// One world (`fixtures/values.{wit,js}`), ported from the ComponentizeJS cases
// `flags`, `keywords`, `variants` (the `casts` functions), `many-arguments`,
// `strings` and `repeated-calls`: flags of every width, escaped keyword
// identifiers, payload-punning variants, a `list<variant>`,
// `result<string, list<u8>>` in both directions, sixteen `u64` arguments and a
// twenty-string record, a large string, and two thousand calls on one
// instance. Every interface but `errors`, `strings` and `checks` is imported
// and exported, so each value crosses the boundary in both directions. The
// `checks` cases cover a plain `Array` for a numeric list, the messages for a
// guest value that does not match its WIT type, and an error class exported
// from the world-level module.
//
// The world is componentized ONCE via a `OnceCell` and shared across cases, and
// each case gets a fresh `Store`.

mod shared;

use shared::Ctx;

use {
    std::sync::LazyLock,
    tokio::sync::OnceCell,
    wasmtime::{
        component::{HasSelf, Linker},
        Engine, Store,
    },
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/values.wit",
    world: "values",
    imports: { default: async },
    exports: { default: async },
    additional_derives: [PartialEq],
});

use test::values::{errors, flags, keyword_imports, keywords, many, variants};

static ENGINE: LazyLock<Engine> = LazyLock::new(shared::engine);

/// Componentize the values world once and hold the instantiation-ready `Pre`.
async fn pre() -> &'static ValuesPre<Ctx> {
    static PRE: OnceCell<Result<ValuesPre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let component = shared::compile_fixture(
            &ENGINE,
            include_str!("fixtures/values.wit"),
            "values",
            include_str!("fixtures/values.js"),
        )
        .await;

        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker).expect("wasi linker");
        Values::add_to_linker::<_, HasSelf<_>>(&mut linker, |ctx| ctx).expect("add_to_linker");
        // The wizened component still declares the runtime's transitive
        // imports, since Wizer does not strip them, so trap-stub the ones this
        // world does not provide. They are never called.
        componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &["test:values/"])
            .expect("trap-stub unknown imports");

        ValuesPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("ValuesPre")
    })
    .await
}

fn store() -> Store<Ctx> {
    shared::store(&ENGINE)
}

// ===========================================================================
// Host import implementations
// ===========================================================================

impl flags::Host for Ctx {
    async fn roundtrip_flag1(&mut self, x: flags::Flag1) -> flags::Flag1 {
        x | flags::Flag1::B0
    }
    async fn roundtrip_flag2(&mut self, x: flags::Flag2) -> flags::Flag2 {
        x | flags::Flag2::B1
    }
    async fn roundtrip_flag4(&mut self, x: flags::Flag4) -> flags::Flag4 {
        x | flags::Flag4::B3
    }
    async fn roundtrip_flag8(&mut self, x: flags::Flag8) -> flags::Flag8 {
        x | flags::Flag8::B7
    }
    async fn roundtrip_flag16(&mut self, x: flags::Flag16) -> flags::Flag16 {
        x | flags::Flag16::B15
    }
    async fn roundtrip_flag32(&mut self, x: flags::Flag32) -> flags::Flag32 {
        x | flags::Flag32::B0
    }
}

impl errors::Host for Ctx {}

impl keyword_imports::Host for Ctx {
    async fn type_(&mut self, v: u32) -> keyword_imports::Flags {
        v as i32 + 1
    }
}

impl keywords::Host for Ctx {
    async fn func(&mut self, _v: u32) -> keywords::Flags {
        unreachable!("the guest calls `keyword-imports.%type` instead")
    }
    async fn echo_record(&mut self, record: keywords::Record) -> keywords::Record {
        keywords::Record {
            type_: record.flags,
            flags: record.type_,
        }
    }
}

impl variants::Host for Ctx {
    async fn casts(
        &mut self,
        a: variants::Casts1,
        b: variants::Casts2,
        c: variants::Casts3,
        d: variants::Casts4,
        e: variants::Casts5,
        f: variants::Casts6,
    ) -> (
        variants::Casts1,
        variants::Casts2,
        variants::Casts3,
        variants::Casts4,
        variants::Casts5,
        variants::Casts6,
    ) {
        (a, b, c, d, e, f)
    }
    async fn echo_list_variant(&mut self, mut v: Vec<variants::V1>) -> Vec<variants::V1> {
        v.reverse();
        v
    }
    async fn echo_result(&mut self, v: Result<String, Vec<u8>>) -> Result<String, Vec<u8>> {
        match v {
            Ok(s) => Ok(s + "!"),
            Err(mut bytes) => {
                bytes.reverse();
                Err(bytes)
            }
        }
    }
    async fn fail_with(&mut self, bytes: Vec<u8>) -> Result<String, Vec<u8>> {
        Err(bytes)
    }
}

impl many::Host for Ctx {
    #[allow(clippy::too_many_arguments)]
    async fn many_args(
        &mut self,
        a1: u64,
        a2: u64,
        a3: u64,
        a4: u64,
        a5: u64,
        a6: u64,
        a7: u64,
        a8: u64,
        a9: u64,
        a10: u64,
        a11: u64,
        a12: u64,
        a13: u64,
        a14: u64,
        a15: u64,
        a16: u64,
    ) -> Vec<u64> {
        vec![
            a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14, a15, a16,
        ]
    }
    async fn big_argument(&mut self, x: many::BigStruct) -> many::BigStruct {
        many::BigStruct {
            a1: x.a1 + "a1",
            a2: x.a2 + "a2",
            a3: x.a3 + "a3",
            a4: x.a4 + "a4",
            a5: x.a5 + "a5",
            a6: x.a6 + "a6",
            a7: x.a7 + "a7",
            a8: x.a8 + "a8",
            a9: x.a9 + "a9",
            a10: x.a10 + "a10",
            a11: x.a11 + "a11",
            a12: x.a12 + "a12",
            a13: x.a13 + "a13",
            a14: x.a14 + "a14",
            a15: x.a15 + "a15",
            a16: x.a16 + "a16",
            a17: x.a17 + "a17",
            a18: x.a18 + "a18",
            a19: x.a19 + "a19",
            a20: x.a20 + "a20",
        }
    }
}

// ===========================================================================
// Test cases
// ===========================================================================

/// Flags of widths 1 to 32 round-trip with the bits the host and the guest set
/// on the way, so the value the guest lowers is checked bit for bit, including
/// bit 31, which the guest sets past the signed-int32 range.
#[tokio::test]
async fn flags_of_every_width() -> anyhow::Result<()> {
    use exports::test::values::flags::{Flag1, Flag16, Flag2, Flag32, Flag4, Flag8};

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let flags = instance.test_values_flags();

    assert_eq!(
        flags
            .call_roundtrip_flag1(&mut store, Flag1::empty())
            .await?,
        Flag1::B0
    );
    assert_eq!(
        flags
            .call_roundtrip_flag2(&mut store, Flag2::empty())
            .await?,
        Flag2::B0 | Flag2::B1
    );
    assert_eq!(
        flags.call_roundtrip_flag4(&mut store, Flag4::B1).await?,
        Flag4::B0 | Flag4::B1 | Flag4::B3
    );
    assert_eq!(
        flags.call_roundtrip_flag8(&mut store, Flag8::B4).await?,
        Flag8::B0 | Flag8::B4 | Flag8::B7
    );
    assert_eq!(
        flags
            .call_roundtrip_flag16(&mut store, Flag16::B8 | Flag16::B9)
            .await?,
        Flag16::B0 | Flag16::B8 | Flag16::B9 | Flag16::B15
    );
    assert_eq!(
        flags
            .call_roundtrip_flag32(&mut store, Flag32::B16 | Flag32::B30)
            .await?,
        Flag32::B0 | Flag32::B16 | Flag32::B30 | Flag32::B31
    );
    assert_eq!(
        flags
            .call_roundtrip_flag32(&mut store, Flag32::all())
            .await?,
        Flag32::all()
    );
    Ok(())
}

/// Escaped keyword identifiers: functions, a type alias, a record, its fields
/// and a parameter all named with `%`-escaped WIT keywords, which the guest
/// sees under their plain names.
#[tokio::test]
async fn escaped_keywords() -> anyhow::Result<()> {
    use exports::test::values::keywords::Record;

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let keywords = instance.test_values_keywords();

    assert_eq!(keywords.call_func(&mut store, 5).await?, 7);
    assert_eq!(
        keywords
            .call_echo_record(&mut store, Record { type_: 1, flags: 2 })
            .await?,
        Record { type_: 2, flags: 1 }
    );
    Ok(())
}

/// Variants whose cases share flat slots of different types (`s32`/`f32`,
/// `f64`/`u64`, `f32`/`s64`, a pair of tuples) round-trip with each case's
/// payload intact.
#[tokio::test]
async fn payload_punning_variants() -> anyhow::Result<()> {
    use exports::test::values::variants::{Casts1, Casts2, Casts3, Casts4, Casts5, Casts6};

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let variants = instance.test_values_variants();

    let cases = [
        (
            Casts1::A(-1),
            Casts2::A(1.5),
            Casts3::A(-2.25),
            Casts4::A(u32::MAX),
            Casts5::A(0.125),
            Casts6::A((3.5, 7)),
        ),
        (
            Casts1::B(-0.5),
            Casts2::B(2.5),
            Casts3::B(u64::MAX),
            Casts4::B(i64::MIN),
            Casts5::B(-(1 << 40)),
            Casts6::B((u32::MAX, 1)),
        ),
    ];
    for (a, b, c, d, e, f) in cases {
        let got = variants.call_casts(&mut store, a, b, c, d, e, f).await?;
        assert_eq!(got, (a, b, c, d, e, f));
    }
    Ok(())
}

/// A `list<variant>` with a payload-less case, a numeric case and a string
/// case, reversed by the host on the way through.
#[tokio::test]
async fn list_of_variants() -> anyhow::Result<()> {
    use exports::test::values::variants::V1;

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let variants = instance.test_values_variants();

    let list = vec![V1::A, V1::B(7), V1::C("seven".to_string()), V1::B(0)];
    let mut expected = list.clone();
    expected.reverse();
    assert_eq!(
        variants.call_echo_list_variant(&mut store, &list).await?,
        expected
    );
    Ok(())
}

/// `result<string, list<u8>>` in both directions: an `err` the host returns
/// reaches the guest as a `ComponentError` whose payload is a `Uint8Array`,
/// which the guest rethrows as the export's `err`, and an `err` the guest
/// raises itself arrives as `Err`.
#[tokio::test]
async fn result_with_bytes_error() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let variants = instance.test_values_variants();

    assert_eq!(
        variants.call_echo_result(&mut store, Ok("hi")).await?,
        Ok("hi!".to_string())
    );
    assert_eq!(
        variants
            .call_echo_result(&mut store, Err(&[1, 2, 3]))
            .await?,
        Err(vec![3, 2, 1])
    );
    assert_eq!(
        variants.call_fail_with(&mut store, &[9, 8, 7]).await?,
        Err(vec![9, 8, 7])
    );
    Ok(())
}

/// An `enum` `err` lowers from an `Error` whose message names a case and from an
/// instance of the enum's error class, for a sync export and for a rejected
/// async one.
#[tokio::test]
async fn enum_err_lowers_from_errors() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;

    assert_eq!(
        instance.call_lookup(&mut store, "present").await?,
        Ok("present".to_string())
    );
    assert_eq!(
        instance.call_lookup(&mut store, "missing").await?,
        Err(errors::KvError::NotFound)
    );
    assert_eq!(
        instance.call_lookup(&mut store, "secret").await?,
        Err(errors::KvError::Denied)
    );
    let results = store
        .run_concurrent(async |store| {
            let mut results = Vec::new();
            for key in ["present", "missing", "secret"] {
                results.push(instance.call_lookup_async(store, key.to_string()).await?);
            }
            anyhow::Ok(results)
        })
        .await??;
    assert_eq!(
        results,
        [
            Ok("present".to_string()),
            Err(errors::KvError::NotFound),
            Err(errors::KvError::Denied),
        ]
    );
    Ok(())
}

/// Sixteen `u64` arguments, the last ones above the 32-bit and 53-bit ranges,
/// lift as bigints and come back as a `list<u64>` the guest lowers from a
/// `BigUint64Array`.
#[tokio::test]
async fn sixteen_u64_arguments() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let many = instance.test_values_many();

    let args: [u64; 16] = [
        1,
        2,
        3,
        4,
        5,
        6,
        7,
        8,
        9,
        10,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
        1 << 53,
        (1 << 53) + 1,
        u64::MAX - 1,
        u64::MAX,
    ];
    let [a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14, a15, a16] = args;
    let got = many
        .call_many_args(
            &mut store, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14, a15, a16,
        )
        .await?;
    assert_eq!(got, args);
    Ok(())
}

/// A record of twenty strings, past the sixteen-flat-value limit, so the
/// arguments and the results both go through memory.
#[tokio::test]
async fn twenty_string_record() -> anyhow::Result<()> {
    use exports::test::values::many::BigStruct;

    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let many = instance.test_values_many();

    let field = |i: u32| format!("value{i}");
    let input = BigStruct {
        a1: field(1),
        a2: field(2),
        a3: field(3),
        a4: field(4),
        a5: field(5),
        a6: field(6),
        a7: field(7),
        a8: field(8),
        a9: field(9),
        a10: field(10),
        a11: field(11),
        a12: field(12),
        a13: field(13),
        a14: field(14),
        a15: field(15),
        a16: field(16),
        a17: field(17),
        a18: field(18),
        a19: field(19),
        a20: field(20),
    };
    let got = many.call_big_argument(&mut store, &input).await?;
    let expected = |i: u32| format!("value{i}a{i}");
    assert_eq!(
        [
            got.a1, got.a2, got.a3, got.a4, got.a5, got.a6, got.a7, got.a8, got.a9, got.a10,
            got.a11, got.a12, got.a13, got.a14, got.a15, got.a16, got.a17, got.a18, got.a19,
            got.a20,
        ],
        std::array::from_fn::<_, 20, _>(|i| expected(i as u32 + 1))
    );
    Ok(())
}

/// A string of about 120 KB, mixing one-, two- and three-byte UTF-8, comes back
/// intact.
#[tokio::test]
async fn large_string() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;

    let unit = "Beware the Jubjub bird, and shun\n\tThe frumious Bandersnatch! ß€";
    let big: String = std::iter::repeat_n(unit, 120 * 1024 / unit.len() + 1).collect();
    assert!(big.len() > 120 * 1024);
    let got = instance
        .test_values_strings()
        .call_echo(&mut store, &big)
        .await?;
    assert_eq!(got, big);
    Ok(())
}

/// Two thousand calls on one instance, each returning a fresh string.
#[tokio::test]
async fn two_thousand_calls() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let strings = instance.test_values_strings();

    for i in 0..2000 {
        let got = strings.call_hello(&mut store).await?;
        assert_eq!(got, "hello", "call {i}");
    }
    Ok(())
}

/// A plain `Array` of numbers lowers to a `list<u8>` element by element.
#[tokio::test]
async fn plain_array_for_a_numeric_list() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let checks = instance.test_values_checks();

    assert_eq!(
        checks.call_bytes_from_array(&mut store, 5).await?,
        vec![0, 1, 2, 3, 4]
    );
    assert_eq!(
        checks.call_bytes_from_array(&mut store, 0).await?,
        Vec::<u8>::new()
    );
    // A list far longer than the others the suites pass.
    let large = checks.call_bytes_from_array(&mut store, 100_000).await?;
    assert_eq!(large.len(), 100_000);
    assert!(large.iter().enumerate().all(|(i, b)| *b == i as u8));
    Ok(())
}

/// The module of an interface exports a frozen object for each flags and enum
/// type, mapping case names to values and values to case names. An error class
/// of the same name takes the name in its module.
#[tokio::test]
async fn enum_and_flags_objects() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let json = instance
        .test_values_checks()
        .call_enum_objects(&mut store)
        .await?;
    assert_eq!(json, r#"[2,-2147483648,"b31",true,1,"notFound",true,true]"#);
    Ok(())
}

/// An export result that does not match its WIT type traps, and the message on
/// stderr names the export, the field and the value.
#[tokio::test]
async fn mismatched_export_result_names_the_field() -> anyhow::Result<()> {
    let (mut store, stderr) = shared::store_capturing_stderr(&ENGINE);
    let instance = pre().await.instantiate_async(&mut store).await?;

    let result = instance
        .test_values_checks()
        .call_bad_point(&mut store)
        .await;
    assert!(result.is_err(), "a u8 field holding 310 must trap");
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("the result of export `test:values/checks#bad-point` `.x`")
            && stderr.contains("expected a u8, got 310"),
        "{stderr}"
    );
    Ok(())
}

/// A lone surrogate in a string the guest returns is lowered as U+FFFD.
#[tokio::test]
async fn lone_surrogates_in_a_string_become_replacement_characters() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let lowered = instance
        .test_values_checks()
        .call_lone_surrogates(&mut store)
        .await?;
    assert_eq!(lowered, "a\u{FFFD}b\u{FFFD}c");
    Ok(())
}

/// A lone surrogate returned as a `char` traps, and the message on stderr names
/// the export.
#[tokio::test]
async fn lone_surrogate_char_traps() -> anyhow::Result<()> {
    let (mut store, stderr) = shared::store_capturing_stderr(&ENGINE);
    let instance = pre().await.instantiate_async(&mut store).await?;
    let result = instance
        .test_values_checks()
        .call_lone_surrogate_char(&mut store)
        .await;
    assert!(result.is_err(), "a lone surrogate is not a `char`");
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("test:values/checks#lone-surrogate-char"),
        "{stderr}"
    );
    Ok(())
}

/// An import argument that does not match its WIT type throws a `TypeError`
/// naming the argument, the field and the value.
#[tokio::test]
async fn mismatched_import_argument_throws() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;

    let message = instance
        .test_values_checks()
        .call_bad_import_argument(&mut store)
        .await?;
    assert!(
        message.contains("argument 1 of `bigArgument` `.a1`")
            && message.contains("expected a string, got 5"),
        "{message}"
    );
    Ok(())
}

/// A sync export that returns a promise traps, and the message suggests declaring
/// the function `async`.
#[tokio::test]
async fn sync_export_returning_a_promise_traps_with_a_hint() -> anyhow::Result<()> {
    let (mut store, stderr) = shared::store_capturing_stderr(&ENGINE);
    let instance = pre().await.instantiate_async(&mut store).await?;
    let result = instance
        .test_values_checks()
        .call_promised(&mut store)
        .await;
    assert!(result.is_err(), "returning a promise must trap");
    let stderr = shared::pipe_text(&stderr);
    assert!(
        stderr.contains("got a Promise") && stderr.contains("Declare it `async`"),
        "{stderr}"
    );
    Ok(())
}

/// An import argument is read once, so a getter that returns something else on a
/// later read changes neither what is checked nor what the host receives.
#[tokio::test]
async fn import_argument_is_read_once() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let a1 = instance
        .test_values_checks()
        .call_shifting_import_argument(&mut store)
        .await?;
    // The host appends each field's name to its value.
    assert_eq!(a1, "firsta1");
    Ok(())
}

/// The error class of a type the world `use`s is exported from `external`,
/// though the world imports no function of its own, and an instance of it
/// thrown from an export becomes the export's `err`.
#[tokio::test]
async fn world_level_error_class() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;

    assert_eq!(
        instance.call_fallible(&mut store, false).await?,
        Err(errors::Failure::Bad("bad".to_string()))
    );
    assert_eq!(
        instance.call_fallible(&mut store, true).await?,
        Err(errors::Failure::Worse)
    );
    Ok(())
}
