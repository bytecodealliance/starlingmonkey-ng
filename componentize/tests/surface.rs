// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The finalized component's surface.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
//
// One world (`fixtures/surface.wit` + `fixtures/surface.js`), componentized
// twice: with every WASI feature enabled, and with every feature disabled. The
// first proves the component exports exactly the world's exports, without the
// runtime's `init`, `wasi:cli/run` and `wasi:http/handler`. The second proves
// the disabled features' interfaces are gone from the imports, that the
// component still runs (the runtime's own clock reads go to the zero stub, so
// `Date.now()` reads zero), and that a call reaching a disabled feature traps.
// Builds with a single feature disabled leave out just that feature. A third build asks for a
// feature another remaining feature depends on and must be refused with a
// message naming both. A fourth build componentizes a guest that lacks the
// `random` export and must be refused at componentize time, naming the
// missing JS export. The last builds select an export gated on a WIT feature,
// which the guest provides only with the feature enabled.

mod shared;

use {
    componentize::finalize::WASIAPISet,
    std::path::PathBuf,
    wasmtime::{
        component::{Component, HasSelf, Linker},
        Engine, Store,
    },
    wasmtime_wasi::{WasiCtxView, WasiView},
    wit_parser::{decoding::DecodedWasm, Resolve, WorldId},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/surface.wit",
    world: "surface",
    imports: { default: async },
    exports: { default: async },
});

struct Ctx {
    wasi: shared::Wasi,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

impl SurfaceImports for Ctx {
    async fn log(&mut self, _msg: String) -> () {}
}

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

async fn build(disabled: &[WASIAPISet]) -> anyhow::Result<Vec<u8>> {
    build_js(include_str!("fixtures/surface.js"), disabled).await
}

async fn build_js(js: &str, disabled: &[WASIAPISet]) -> anyhow::Result<Vec<u8>> {
    let runtime = shared::runtime().expect("reading the runtime build");
    componentize::componentize(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/surface.wit")),
        Some("surface"),
        &[],
        false,
        js,
        None::<PathBuf>,
        &runtime,
        disabled,
        None,
    )
    .await
}

/// The decoded world of a component, and its import and export names.
fn surface(component: &[u8]) -> (Resolve, WorldId, Vec<String>, Vec<String>) {
    let DecodedWasm::Component(resolve, world) =
        wit_parser::decoding::decode(component).expect("decoding the component")
    else {
        panic!("not a component");
    };
    let names = |items: &indexmap::IndexMap<wit_parser::WorldKey, wit_parser::WorldItem>| {
        items
            .keys()
            .map(|key| resolve.name_world_key(key))
            .collect::<Vec<_>>()
    };
    let imports = names(&resolve.worlds[world].imports);
    let exports = names(&resolve.worlds[world].exports);
    (resolve, world, imports, exports)
}

async fn instantiate(component: &[u8]) -> anyhow::Result<(Store<Ctx>, Surface)> {
    let component = Component::new(&ENGINE, component)?;
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker)?;
    Surface::add_to_linker::<_, HasSelf<_>>(&mut linker, |ctx| ctx)?;
    componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
    let mut store = Store::new(
        &ENGINE,
        Ctx {
            wasi: shared::Wasi::inherit(),
        },
    );
    let instance = SurfacePre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    Ok((store, instance))
}

/// With nothing disabled the component exports the world's two functions and
/// nothing else, and still imports the runtime's WASI surface.
#[tokio::test]
async fn exports_are_exactly_the_worlds() -> anyhow::Result<()> {
    let component = build(&[]).await?;
    let (_, _, imports, exports) = surface(&component);
    assert_eq!(exports, ["run", "random", "now"]);
    assert!(
        imports.iter().any(|i| i.starts_with("wasi:random/")),
        "random stays imported when enabled: {imports:?}"
    );
    // The dependency-only interface the standard streams use stays as well: a
    // wasm32-wasip2 runtime reaches them through `wasi:io`, a wasm32-wasip3 one
    // through `wasi:cli/types`.
    assert!(
        imports
            .iter()
            .any(|i| i.starts_with("wasi:io/") || i.starts_with("wasi:cli/types")),
        "the standard streams' dependency interface stays: {imports:?}"
    );

    let (mut store, instance) = instantiate(&component).await?;
    assert_eq!(instance.call_run(&mut store, 41).await?, 42);
    let value = instance.call_random(&mut store).await?;
    assert!((0.0..1.0).contains(&value), "Math.random gave {value}");
    let now = instance.call_now(&mut store).await?;
    assert!(now > 1.7e12, "Date.now() gave {now}");
    Ok(())
}

/// With every feature disabled the component imports no WASI interface beyond
/// the environment, exit and insecure seed, runs, and traps where a disabled
/// feature is reached.
#[tokio::test]
async fn disabled_features_are_stubbed() -> anyhow::Result<()> {
    let component = build(&[
        WASIAPISet::Stdio,
        WASIAPISet::Random,
        WASIAPISet::Clocks,
        WASIAPISet::Http,
        WASIAPISet::Filesystem,
    ])
    .await?;
    let (_, _, imports, exports) = surface(&component);
    assert_eq!(exports, ["run", "random", "now"]);
    let (wasi, own): (Vec<&String>, Vec<&String>) =
        imports.iter().partition(|i| i.starts_with("wasi:"));
    assert!(
        wasi.iter().all(|i| i.starts_with("wasi:cli/environment")
            || i.starts_with("wasi:cli/exit")
            || i.starts_with("wasi:random/insecure-seed")),
        "only the environment, exit and insecure seed remain: {imports:?}"
    );
    assert_eq!(
        own,
        ["log"],
        "the world's own import and nothing else remains"
    );

    let (mut store, instance) = instantiate(&component).await?;
    assert_eq!(instance.call_run(&mut store, 1).await?, 2);
    assert_eq!(
        instance.call_now(&mut store).await?,
        0.0,
        "a disabled clock reads as zero"
    );
    let err = instance
        .call_random(&mut store)
        .await
        .expect_err("Math.random must trap with random disabled");
    assert!(
        format!("{err:?}").contains("disabled-features!wasi:random/random@0.3.0#"),
        "the trap is in the disabled function's stub: {err:?}"
    );
    Ok(())
}

/// The filesystem interfaces use the clock's types, so disabling the clocks
/// alone is refused, naming every interface that keeps them and the features to
/// disable with them.
#[tokio::test]
async fn a_feature_another_depends_on_cannot_be_disabled_alone() {
    let err = build(&[WASIAPISet::Clocks])
        .await
        .expect_err("clocks cannot go while the filesystem stays");
    let message = format!("{err:#}");
    assert!(
        message.contains("cannot disable these features")
            && message.contains("of `clocks` is used by `wasi:filesystem/types@")
            && message.contains("(feature `filesystem`)"),
        "{message}"
    );
    assert!(message.contains("Disable `filesystem`"), "{message}");
}

/// A world that `use`s a disabled feature's type at its top level is refused,
/// naming the world as the user.
#[tokio::test]
async fn a_feature_the_world_uses_cannot_be_disabled() {
    let runtime = shared::runtime().expect("reading the runtime build");
    let err = componentize::componentize(
        componentize::Wit::<PathBuf>::String(
            "package test:uses;
             world uses {
               use wasi:filesystem/types@0.3.0.{descriptor};
               import open-root: func() -> descriptor;
               export count: func() -> u32;
             }
             package wasi:filesystem@0.3.0 {
               interface types { resource descriptor; }
             }",
        ),
        Some("uses"),
        &[],
        false,
        "export function count() { return 1; }",
        None::<PathBuf>,
        &runtime,
        &[WASIAPISet::Filesystem],
        None,
    )
    .await
    .expect_err("the filesystem cannot go while the world uses its types");
    let message = format!("{err:#}");
    assert!(
        message.contains("cannot disable these features")
            && message.contains("of `filesystem` is used by the world"),
        "{message}"
    );
}

/// A guest that does not define a function for one of the world's exports is
/// refused at componentize time, with the message naming the missing JS export
/// and the statement that would define it.
#[tokio::test]
async fn missing_export_fails_at_componentize_time() {
    let err = build_js("export function run(n) { return n + 1; }", &[])
        .await
        .expect_err("a guest without `random` must not componentize");
    let message = format!("{err:#}");
    assert!(
        message.contains(
            "does not provide a function `random` for the WIT export `random`. Export it as \
             `random`, for example: export function random(...) { ... }"
        ),
        "{message}"
    );
}

/// A world with an export gated on a WIT feature.
const GATED_WIT: &str = "\
package test:gated;

world gated {
  export base: func() -> u32;
  @unstable(feature = extra)
  export extra: func() -> u32;
}
";

/// Componentize `js` against [`GATED_WIT`] with `features` and `all_features`.
async fn build_gated(js: &str, features: &[String], all_features: bool) -> anyhow::Result<Vec<u8>> {
    let runtime = shared::runtime().expect("reading the runtime build");
    componentize::componentize(
        componentize::Wit::<PathBuf>::String(GATED_WIT),
        Some("gated"),
        features,
        all_features,
        js,
        None::<PathBuf>,
        &runtime,
        &[],
        None,
    )
    .await
}

/// An export gated on a feature is part of the world only with the feature
/// enabled, by name or with every feature.
#[tokio::test]
async fn features_select_gated_exports() -> anyhow::Result<()> {
    let base_only = "export const base = () => 1;";
    let both = "export const base = () => 1;\nexport const extra = () => 2;";

    let (_, _, _, exports) = surface(&build_gated(base_only, &[], false).await?);
    assert_eq!(exports, ["base"]);

    let err = build_gated(base_only, &["extra".to_string()], false)
        .await
        .expect_err("with `extra` enabled, the guest must provide it");
    assert!(
        format!("{err:#}").contains("does not provide a function `extra`"),
        "{err:#}"
    );

    for (features, all) in [(vec!["extra".to_string()], false), (vec![], true)] {
        let (_, _, _, exports) = surface(&build_gated(both, &features, all).await?);
        assert_eq!(
            exports,
            ["base", "extra"],
            "features {features:?}, all {all}"
        );
    }
    Ok(())
}

/// Each feature that no other one uses can be disabled by itself, which leaves
/// out its interfaces and keeps the others'.
#[tokio::test]
async fn single_features_are_disabled_alone() -> anyhow::Result<()> {
    for (feature, prefixes) in [
        (
            WASIAPISet::Stdio,
            &["wasi:cli/std", "wasi:cli/terminal-"][..],
        ),
        (WASIAPISet::Random, &["wasi:random/random"]),
        (WASIAPISet::Http, &["wasi:http/"]),
        (WASIAPISet::Filesystem, &["wasi:filesystem/"]),
    ] {
        let (_, _, imports, _) = surface(&build(&[feature]).await?);
        assert!(
            !imports
                .iter()
                .any(|i| prefixes.iter().any(|prefix| i.starts_with(prefix))),
            "{feature:?} disabled: {imports:?}"
        );
        assert!(
            imports.iter().any(|i| i.starts_with("wasi:clocks/")),
            "{feature:?} disabled keeps the clocks: {imports:?}"
        );
    }
    Ok(())
}
