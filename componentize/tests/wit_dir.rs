// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A WIT directory with `deps/`, componentized through `Wit::Paths`, whose world imports and
//! exports two versions of one interface, imports the same interface name from a third package,
//! and `use`s a type from a dependency package at world level.

mod shared;

use shared::Ctx;

use {
    std::{path::PathBuf, sync::LazyLock},
    tokio::sync::OnceCell,
    wasmtime::{
        component::{Component, HasSelf},
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
        let wit_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wit_dir");
        let component_bytes = shared::componentize_with(
            componentize::Wit::Paths(&[wit_dir]),
            Some("versions"),
            include_str!("fixtures/wit_dir.js"),
            &Default::default(),
        )
        .await
        .expect("componentizing the wit-dir world")
        .component;
        let component = Component::new(&ENGINE, &component_bytes).expect("compiling component");
        let linker = shared::linker(
            &ENGINE,
            &component,
            &["local:hello/", "local:other/"],
            |linker| Versions::add_to_linker::<_, HasSelf<_>>(linker, |ctx| ctx),
        );
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
async fn same_interface_name_from_two_packages_is_two_modules() -> anyhow::Result<()> {
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
        exports.call_hello(&mut store, "else").await?,
        "world unknown else"
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
/// interface of its own and at world level, in an imported function's result
/// and an exported function's parameter: the component builds, and
/// instantiates against the host.
#[tokio::test]
async fn world_using_a_wasi_resource() -> anyhow::Result<()> {
    const WIT: &str = "
package test:alias;
interface files {
  use wasi:filesystem/types@0.3.0.{descriptor};
  size: func(d: borrow<descriptor>) -> u64;
}
world w {
  use wasi:filesystem/types@0.3.0.{descriptor};
  import files;
  import open-root: func() -> descriptor;
  export count: func() -> u32;
  export take: func(d: descriptor) -> u32;
}
package wasi:filesystem@0.3.0 {
  interface types {
    resource descriptor;
  }
}
";
    let bytes = shared::try_componentize(
        WIT,
        "w",
        "export function count() { return 1; }\nexport function take(d) { return 1; }",
    )
    .await?;
    let component = Component::new(&ENGINE, &bytes)?;
    let linker = shared::linker(&ENGINE, &component, &[], |_| Ok(()));
    let mut store = store();
    linker.instantiate_async(&mut store, &component).await?;
    Ok(())
}

/// A world from `-w` alone also imports the interfaces the runtime imports,
/// except those of the WASI APIs `--disable` names. `imports` leaves out an
/// interface that declares only records.
#[test]
fn wit_dir_world_imports_what_the_runtime_imports() {
    let wit = shared::workspace_root().join("componentize/tests/fixtures/wit_dir");
    let wit = wit.to_string_lossy();
    let modules = shared::cli_output(&["-w", &wit, "imports"]);
    let modules: Vec<&str> = modules.lines().collect();
    for module in [
        "local:hello/hello@1",
        "wasi:cli/environment@0.3",
        "wasi:random/random@0.3",
        "wasi:http/client@0.3",
    ] {
        assert!(modules.contains(&module), "`{module}` in {modules:?}");
    }
    assert!(!modules.contains(&"local:b/foo"), "{modules:?}");

    let modules = shared::cli_output(&["-w", &wit, "--disable", "random", "imports"]);
    let modules: Vec<&str> = modules.lines().collect();
    assert!(
        !modules.contains(&"wasi:random/random@0.3"),
        "no `wasi:random/random` in {modules:?}"
    );
    assert!(
        modules.contains(&"wasi:random/insecure-seed@0.3"),
        "`wasi:random/insecure-seed` in {modules:?}"
    );
}
