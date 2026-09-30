// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! What the main module's top level may do before the component is wizened: the world of
//! `fixtures/top_level.wit`, with the guest in `fixtures/top_level/` and inline guests.

mod shared;

use {
    std::{
        path::{Path, PathBuf},
        time::Duration,
    },
    tokio::sync::OnceCell,
    wasmtime::{component::Component, Engine, Store},
};

wasmtime::component::bindgen!({
    path: "tests/fixtures/top_level.wit",
    world: "top-level",
    exports: { default: async },
});

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

/// The directory the guest's relative imports resolve against.
fn base_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/top_level")
}

/// Componentize `js` against the world, with [`base_directory`] as the base
/// directory and `init_location` as the location.
///
/// Every case builds the same world, so the componentizer compiles the same instrumented
/// component for each, and wasmtime's cache holds the compilation only once the first build
/// wrote it. The first build runs alone, so the cases that start together don't each compile
/// it with a cold cache.
async fn build_with(
    js: impl Into<componentize::JsSource<'_>>,
    init_location: Option<&str>,
) -> anyhow::Result<componentize::Componentized> {
    let base_directory = base_directory();
    let options = componentize::ComponentizeOptions {
        base_directory: Some(&base_directory),
        init_location,
        ..Default::default()
    };
    static WARM: OnceCell<()> = OnceCell::const_new();
    WARM.get_or_init(|| async {
        // A failure is reported by the cases themselves.
        let _ = shared::componentize_with(
            componentize::Wit::String(WIT),
            Some("top-level"),
            "export const greeting = () => '';",
            &options,
        )
        .await;
    })
    .await;
    shared::componentize_with(
        componentize::Wit::String(WIT),
        Some("top-level"),
        js,
        &options,
    )
    .await
}

const WIT: &str = include_str!("fixtures/top_level.wit");

async fn build(js: impl Into<componentize::JsSource<'_>>) -> anyhow::Result<Vec<u8>> {
    Ok(build_with(js, None).await?.component)
}

/// An instance of `component` in a fresh store.
async fn instantiate(component: &[u8]) -> anyhow::Result<(Store<shared::Ctx>, TopLevel)> {
    let component = Component::new(&ENGINE, component)?;
    let linker = shared::linker(&ENGINE, &component, &[], |_| Ok(()));
    let mut store = shared::store(&ENGINE);
    let instance = TopLevelPre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    Ok((store, instance))
}

/// The guest's static, JSON and dynamic imports and the value for which its
/// top-level `await` waited on a timer are all in the snapshot, and its `const` export
/// is initialized.
#[tokio::test]
async fn imports_and_top_level_await_are_wizened() -> anyhow::Result<()> {
    let component = build(include_str!("fixtures/top_level/main.js")).await?;
    let (mut store, instance) = instantiate(&component).await?;
    let greeting = instance.call_greeting(&mut store).await?;
    assert_eq!(
        greeting,
        "hello from a relative import and a dynamic import after a timer!"
    );
    Ok(())
}

/// A relative import of a module that does not exist fails componentization,
/// naming the specifier.
#[tokio::test]
async fn missing_relative_import_fails_componentization() {
    let err =
        build("import { name } from './lib/missing.js';\nexport const greeting = () => name;")
            .await
            .expect_err("a guest importing a missing module must not componentize");
    let message = format!("{err:#}");
    assert!(
        message.contains("Cannot resolve module './lib/missing.js'"),
        "{message}"
    );
}

/// A top-level `await` that rejects after a timer fails componentization with
/// the rejection's message.
#[tokio::test]
async fn top_level_rejection_after_a_timer_fails_componentization() {
    let err = build(
        "await new Promise((resolve) => setTimeout(resolve, 5));\n\
         throw new Error('configuration failed');",
    )
    .await
    .expect_err("a guest whose top level rejects must not componentize");
    let message = format!("{err:#}");
    assert!(
        message.contains("main module threw during evaluation: configuration failed"),
        "{message}"
    );
}

/// A top-level call of an export that throws synchronously fails
/// componentization with the exception's message.
#[tokio::test]
async fn top_level_call_of_a_throwing_export_fails_componentization() {
    let err = build(
        "export function greeting() { throw new Error('greeting failed'); }\n\
         greeting();",
    )
    .await
    .expect_err("a guest whose top level throws must not componentize");
    let message = format!("{err:#}");
    assert!(
        message.contains("main module threw during evaluation: greeting failed"),
        "{message}"
    );
}

/// A top-level `await` on a promise that never settles fails componentization
/// once the event loop has no work left.
#[tokio::test]
async fn unsettled_top_level_await_fails_componentization() {
    let err = build("await new Promise(() => {});\nexport const greeting = () => '';")
        .await
        .expect_err("a guest whose top level never finishes must not componentize");
    let message = format!("{err:#}");
    assert!(
        message.contains("the application's top-level `await` never settled"),
        "{message}"
    );
}

/// Work the top level leaves pending once it has finished fails componentization
/// with a message listing each piece of work and the stack that created it.
#[tokio::test]
async fn pending_work_fails_componentization_naming_its_origin() {
    let text = "function poll() {\n\
                  setInterval(() => {}, 1000);\n\
                }\n\
                poll();\n\
                setTimeout(() => {}, 50);\n\
                export const greeting = () => '';";
    let err = build(componentize::JsSource {
        name: "pending.js",
        text,
    })
    .await
    .expect_err("a guest that leaves work pending must not componentize");
    let message = format!("{err:#}");
    assert!(
        message.contains("finished evaluating with asynchronous work still pending"),
        "{message}"
    );
    let interval = message
        .find("a pending `interval` timer, created at:\n      poll@pending.js:2:")
        .unwrap_or_else(|| panic!("the interval and its stack are listed: {message}"));
    let timeout = message
        .find("a pending `timeout` timer, created at:\n      @pending.js:5:")
        .unwrap_or_else(|| panic!("the timeout and its stack are listed: {message}"));
    assert!(
        interval < timeout,
        "work is listed in creation order: {message}"
    );
}

/// A top-level call of an import fails componentization, naming the import.
#[tokio::test]
async fn top_level_import_call_fails_componentization() {
    let err = build(
        "import { logEvent } from 'external';\n\
         logEvent('started');\n\
         export const greeting = () => '';",
    )
    .await
    .expect_err("a guest calling an import at top level must not componentize");
    let message = format!("{err:#}");
    assert!(
        message.contains(
            "the import `logEvent` was called while the application initializes, before the \
             component is wizened"
        ),
        "{message}"
    );
}

/// What the top level prints while it initializes is returned with the
/// component, and reading `location` without an init location throws.
/// `performance.now()` in an export of a resumed instance continues past the
/// reading taken by the top level before the snapshot, though the resumed
/// instance's host clock starts over at zero.
#[tokio::test]
async fn init_output_is_returned_location_throws_and_the_clock_continues() -> anyhow::Result<()> {
    let componentized = build_with(
        "let href;\n\
         try { href = location.href; } catch (e) { href = e.name; }\n\
         console.log(`location: ${href}`);\n\
         console.error('warned at init');\n\
         await new Promise((resolve) => setTimeout(resolve, 50));\n\
         const atInit = performance.now();\n\
         export const greeting = () => String(performance.now() - atInit);",
        None,
    )
    .await?;
    assert_eq!(
        componentized.init_output,
        "location: TypeError\nError: warned at init\n"
    );
    let (mut store, instance) = instantiate(&componentized.component).await?;
    let first: f64 = instance.call_greeting(&mut store).await?.parse()?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let second: f64 = instance.call_greeting(&mut store).await?.parse()?;
    assert!(
        first >= 0.0,
        "the clock went back past the snapshot: {first}"
    );
    assert!(second - first >= 40.0, "{first} then {second}");
    Ok(())
}

/// A module importing the main module's file gets the main module itself, so
/// the main module's top level runs once.
#[tokio::test]
async fn importing_the_main_module_gets_the_main_module() -> anyhow::Result<()> {
    let path = base_directory().join("entry.js");
    let text = std::fs::read_to_string(&path)?;
    let name = path.display().to_string();
    let component = build(componentize::JsSource {
        name: &name,
        text: &text,
    })
    .await?;
    let (mut store, instance) = instantiate(&component).await?;
    assert_eq!(instance.call_greeting(&mut store).await?, "1 object");
    Ok(())
}

/// The top level reads the location it is given as `globalThis.location`.
#[tokio::test]
async fn top_level_reads_the_init_location() -> anyhow::Result<()> {
    const MAIN: &str = "const href = location.href;\nexport const greeting = () => href;";
    let componentized = build_with(MAIN, Some("http://example.localhost/app/")).await?;
    let (mut store, instance) = instantiate(&componentized.component).await?;
    assert_eq!(
        instance.call_greeting(&mut store).await?,
        "http://example.localhost/app/"
    );
    Ok(())
}
