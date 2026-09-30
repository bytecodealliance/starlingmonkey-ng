// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A WIT directory with `deps/`, componentized through `Wit::Paths`, whose
// world imports and exports two versions of one interface, imports the same
// interface name from a third package, and `use`s a type from a dependency
// package at world level (ComponentizeJS's `versions`,
// `import-duplicated-interface` and `type-imports` cases).
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
// `shared::runtime` documents the environment variables that select the dynamic
// link mode instead.
//
// The world is componentized ONCE via a `OnceCell` and shared across cases, and
// each case gets a fresh `Store`.

mod shared;

use shared::Ctx;

use {
    std::{path::PathBuf, sync::LazyLock},
    tokio::sync::OnceCell,
    wasmtime::{
        component::{Component, HasSelf, Linker},
        Engine, Store,
    },
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/wit_dir",
    world: "versions",
    imports: { default: async },
    exports: { default: async },
    additional_derives: [PartialEq],
});

static ENGINE: LazyLock<Engine> = LazyLock::new(shared::engine);

/// Componentize the world once from its WIT directory and hold the
/// instantiation-ready `Pre`.
async fn pre() -> &'static VersionsPre<Ctx> {
    static PRE: OnceCell<Result<VersionsPre<Ctx>, String>> = OnceCell::const_new();
    shared::build_once(&PRE, async {
        let runtime = shared::runtime().expect("reading the runtime build");
        let wit_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wit_dir");
        let component_bytes = componentize::componentize(
            componentize::Wit::Paths(&[wit_dir]),
            Some("versions"),
            &[],
            false,
            include_str!("fixtures/wit_dir.js"),
            None::<PathBuf>,
            &runtime,
            &[],
            None,
        )
        .await
        .expect("componentizing the wit-dir world");

        let component = Component::new(&ENGINE, &component_bytes).expect("compiling component");

        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker).expect("wasi linker");
        Versions::add_to_linker::<_, HasSelf<_>>(&mut linker, |ctx| ctx).expect("add_to_linker");
        // The wizened component still declares the runtime's transitive
        // imports, since Wizer does not strip them, so trap-stub the ones this
        // world does not provide. They are never called.
        componentize::trap_unsatisfied_imports(
            &ENGINE,
            &component,
            &mut linker,
            &["local:hello/", "local:other/"],
        )
        .expect("trap-stub unknown imports");

        VersionsPre::new(linker.instantiate_pre(&component).expect("instantiate_pre"))
            .expect("VersionsPre")
    })
    .await
}

fn store() -> Store<Ctx> {
    shared::store(&ENGINE)
}

// ===========================================================================
// Host import implementations
// ===========================================================================

impl local::hello1_0_0::hello::Host for Ctx {
    async fn hello(&mut self, name: String) -> String {
        format!("Hello 1.0.0, {name}")
    }
}

impl local::hello2_0_0::hello::Host for Ctx {
    async fn hello(&mut self, name: Option<String>) -> Option<String> {
        name.map(|name| format!("Hello 2.0.0, {name}"))
    }
}

impl local::b::foo::Host for Ctx {}

impl local::other::hello::Host for Ctx {
    async fn hello(&mut self, name: String) -> String {
        format!("Hello other, {name}")
    }
}

// ===========================================================================
// Test cases
// ===========================================================================

/// Each exported version forwards to the import of the same version, and the
/// two signatures differ, so a call reaching the wrong version fails to
/// lift.
#[tokio::test]
async fn two_versions_of_one_interface() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;

    assert_eq!(
        instance
            .local_hello1_0_0_hello()
            .call_hello(&mut store, "foo")
            .await?,
        "Hello 1.0.0, foo"
    );
    assert_eq!(
        instance
            .local_hello2_0_0_hello()
            .call_hello(&mut store, Some("bar"))
            .await?,
        Some("Hello 2.0.0, bar".to_string())
    );
    assert_eq!(
        instance
            .local_hello2_0_0_hello()
            .call_hello(&mut store, None)
            .await?,
        None
    );
    Ok(())
}

/// The same interface name from two packages resolves to two distinct import
/// modules.
#[tokio::test]
async fn same_interface_name_from_two_packages() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    let exports = instance.exports();

    assert_eq!(
        exports.call_hello(&mut store, "hello").await?,
        "world hello (Hello 1.0.0, world)"
    );
    assert_eq!(
        exports.call_hello(&mut store, "other").await?,
        "world other (Hello other, world)"
    );
    assert_eq!(
        exports.call_hello(&mut store, "unknown").await?,
        "world unknown unknown"
    );
    Ok(())
}

/// A world-level `use` of a record from a dependency package.
#[tokio::test]
async fn world_level_use_of_dependency_type() -> anyhow::Result<()> {
    let mut store = store();
    let instance = pre().await.instantiate_async(&mut store).await?;
    assert_eq!(
        instance.call_make_bar(&mut store).await?,
        local::b::foo::Bar { thing: 5 }
    );
    Ok(())
}

/// A world can `use` a resource of an interface the WASI host provides, in an
/// interface of its own and at world level: the component builds, and
/// instantiates against the host.
#[tokio::test]
async fn world_using_a_wasi_resource() -> anyhow::Result<()> {
    const WASI: &str = "
package wasi:filesystem@0.3.0 {
  interface types {
    resource descriptor;
  }
}
";
    let worlds = [
        "package test:alias;
interface files {
  use wasi:filesystem/types@0.3.0.{descriptor};
  size: func(d: borrow<descriptor>) -> u64;
}
world w {
  import files;
  export count: func() -> u32;
}",
        "package test:alias;
world w {
  use wasi:filesystem/types@0.3.0.{descriptor};
  import open-root: func() -> descriptor;
  export count: func() -> u32;
}",
        "package test:alias;
world w {
  use wasi:filesystem/types@0.3.0.{descriptor};
  export count: func() -> u32;
  export take: func(d: descriptor) -> u32;
}",
    ];
    let runtime = shared::runtime().expect("reading the runtime build");
    for world in worlds {
        let wit = format!("{world}\n{WASI}");
        let bytes = componentize::componentize(
            componentize::Wit::<PathBuf>::String(&wit),
            Some("w"),
            &[],
            false,
            "export function count() { return 1; }\nexport function take(d) { return 1; }",
            None::<PathBuf>,
            &runtime,
            &[],
            None,
        )
        .await
        .map_err(|e| e.context(world.to_string()))?;
        let component = Component::new(&ENGINE, &bytes)?;
        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker)?;
        componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
        let mut store = store();
        linker.instantiate_async(&mut store, &component).await?;
    }
    Ok(())
}

/// A world from `-d` alone also imports the interfaces the runtime imports,
/// except those of the WASI APIs `--disable` names.
#[test]
fn wit_dir_world_imports_what_the_runtime_imports() {
    let wit = shared::workspace_root().join("componentize/tests/fixtures/wit_dir");
    let wit = wit.to_string_lossy();
    let modules = shared::cli_output(&["-d", &wit, "imports"]);
    let modules: Vec<&str> = modules.lines().collect();
    for module in [
        "local:hello/hello@1.0.0",
        "wasi:cli/environment@0.3.0",
        "wasi:random/random@0.3.0",
        "wasi:http/client@0.3.0",
    ] {
        assert!(modules.contains(&module), "`{module}` in {modules:?}");
    }

    let modules = shared::cli_output(&["-d", &wit, "--disable", "random", "imports"]);
    let modules: Vec<&str> = modules.lines().collect();
    assert!(
        !modules.contains(&"wasi:random/random@0.3.0"),
        "no `wasi:random/random` in {modules:?}"
    );
    assert!(
        modules.contains(&"wasi:random/insecure-seed@0.3.0"),
        "`wasi:random/insecure-seed` in {modules:?}"
    );
}
