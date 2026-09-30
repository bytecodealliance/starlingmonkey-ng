// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The finalized component's imports and exports: the world of `fixtures/surface.{wit,js}`,
//! componentized once with nothing disabled. The cases that disable features run
//! `finalize::finalize` over that component again.

mod shared;

use {
    componentize::finalize::{KeptExport, WasiApiSet},
    tokio::sync::OnceCell,
    wasmtime::{
        component::{Component, HasSelf},
        Engine, Store,
    },
    wasmtime_wasi::{WasiCtxView, WasiView},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/surface.wit",
    world: "surface",
    imports: { default: async },
    exports: { default: async },
});

struct Ctx {
    wasi: shared::Wasi,
    logged: Vec<String>,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

impl SurfaceImports for Ctx {
    async fn log(&mut self, msg: String) -> () {
        self.logged.push(msg);
    }
}

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// The world componentized with no feature given to disable, built once.
async fn component() -> &'static [u8] {
    static COMPONENT: OnceCell<Result<Vec<u8>, String>> = OnceCell::const_new();
    shared::build_once(&COMPONENT, async {
        shared::try_componentize(WIT, "surface", include_str!("fixtures/surface.js"))
            .await
            .expect("componentizing the surface world")
    })
    .await
}

/// [`component`] finalized again with the features `disabled`.
async fn disable(disabled: &[WasiApiSet]) -> anyhow::Result<Vec<u8>> {
    let exports = ["run", "random", "now"].map(|name| KeptExport::unrenamed(name.to_string()));
    componentize::finalize::finalize(component().await, &exports, disabled, None)
}

const WIT: &str = include_str!("fixtures/surface.wit");

async fn instantiate(component: &[u8]) -> anyhow::Result<(Store<Ctx>, Surface)> {
    let component = Component::new(&ENGINE, component)?;
    let linker = shared::linker(&ENGINE, &component, &[], |linker| {
        Surface::add_to_linker::<_, HasSelf<_>>(linker, |ctx| ctx)
    });
    let mut store = Store::new(
        &ENGINE,
        Ctx {
            wasi: shared::Wasi::inherit(),
            logged: Vec::new(),
        },
    );
    let instance = SurfacePre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    Ok((store, instance))
}

/// With nothing disabled the component exports the world's three functions and
/// nothing else, and still imports the runtime's WASI surface. `run` calls the
/// world's `log` import with a string.
#[tokio::test]
async fn exports_are_exactly_the_worlds() -> anyhow::Result<()> {
    let component = component().await;
    let (imports, exports) = shared::surface(component);
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

    let (mut store, instance) = instantiate(component).await?;
    assert_eq!(instance.call_run(&mut store, 41).await?, 42);
    assert_eq!(store.data().logged, ["run 41"]);
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
    let component = disable(&[
        WasiApiSet::Stdio,
        WasiApiSet::Random,
        WasiApiSet::Clocks,
        WasiApiSet::Http,
        WasiApiSet::Filesystem,
    ])
    .await?;
    let (imports, exports) = shared::surface(&component);
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
    let err = disable(&[WasiApiSet::Clocks])
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
    let err = shared::componentize_with(
        componentize::Wit::String(
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
        "export function count() { return 1; }",
        &componentize::ComponentizeOptions {
            disabled: &[WasiApiSet::Filesystem],
            ..Default::default()
        },
    )
    .await
    .err()
    .expect("the filesystem cannot go while the world uses its types");
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
    let err = shared::try_componentize(WIT, "surface", "export function run(n) { return n + 1; }")
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

/// An export gated on a feature is part of the world only with the feature
/// enabled, by name or with every feature.
#[test]
fn features_select_gated_exports() -> anyhow::Result<()> {
    for (features, all, expected) in [
        (vec![], false, &["base"][..]),
        (vec!["extra".to_string()], false, &["base", "extra"]),
        (vec![], true, &["base", "extra"]),
    ] {
        let (resolve, world) = componentize::load_world(
            componentize::Wit::<std::path::PathBuf>::String(GATED_WIT),
            Some("gated"),
            &features,
            all,
        )?;
        assert_eq!(
            componentize::finalize::world_export_names(&resolve, world),
            expected,
            "features {features:?}, all {all}"
        );
    }
    Ok(())
}

/// With a gated export's feature enabled, the guest must provide the export.
#[tokio::test]
async fn enabled_gated_export_must_be_provided() {
    let err = shared::componentize_with(
        componentize::Wit::String(GATED_WIT),
        Some("gated"),
        "export const base = () => 1;",
        &componentize::ComponentizeOptions {
            features: &["extra".to_string()],
            ..Default::default()
        },
    )
    .await
    .err()
    .expect("with `extra` enabled, the guest must provide it");
    assert!(
        format!("{err:#}").contains("does not provide a function `extra`"),
        "{err:#}"
    );
}

/// Each feature that no other one uses can be disabled by itself, which leaves
/// out its interfaces and keeps the others'.
#[tokio::test]
async fn single_features_are_disabled_alone() -> anyhow::Result<()> {
    for (feature, prefixes) in [
        (
            WasiApiSet::Stdio,
            &["wasi:cli/std", "wasi:cli/terminal-"][..],
        ),
        (WasiApiSet::Random, &["wasi:random/random"]),
        (WasiApiSet::Http, &["wasi:http/"]),
        (WasiApiSet::Filesystem, &["wasi:filesystem/"]),
    ] {
        let (imports, _) = shared::surface(&disable(&[feature]).await?);
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

/// A world whose function uses `error-context` is refused at componentize
/// time, naming the function.
#[tokio::test]
async fn error_context_is_refused() {
    let err = shared::try_componentize(
        "package test:ec;\nworld w { export f: func(e: error-context); }\n",
        "w",
        "export function f(e) {}",
    )
    .await
    .expect_err("a world using `error-context` is refused");
    let message = format!("{err:#}");
    assert!(
        message.contains("`f` uses a `error-context`, which componentize does not support yet"),
        "{message}"
    );
}

/// A world-level resource's method imports take a `borrow` outside any
/// interface, which the component composition cannot encode, so the world is
/// refused with a message naming the import.
#[tokio::test]
async fn world_level_resource_methods_are_refused() {
    let err = shared::try_componentize(
        include_str!("fixtures/world_resource.wit"),
        "world-resource",
        include_str!("fixtures/world_resource.js"),
    )
    .await
    .expect_err("a world-level resource with methods is refused");
    let message = format!("{err:#}");
    assert!(
        message.contains("[method]thing.get") && message.contains("interface"),
        "{message}"
    );
}
