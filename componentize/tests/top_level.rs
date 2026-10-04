// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// What the main module's top level may do before the component is wizened.
//
// PREREQUISITE: a runtime build. `just build-runtime` writes the runtime
// component `target/wasm32-wasip3/release/starling.wasm`, which the suites
// link by default. `just test-componentize` builds it and runs every suite.
//
// One world (`fixtures/top_level.wit`). The guest in `fixtures/top_level/`
// imports a module by relative path and a JSON module, and awaits a dynamic
// import and a timer at top level before declaring its export, and the snapshot
// must hold all four results. The other cases componentize inline guests whose
// top level cannot be wizened: a relative import of a missing module, a
// top-level `await` that rejects after a timer, one that never settles, one
// that leaves work other than timers pending, and one that calls an import.
// Each must fail componentization with a message naming the cause. Further
// cases check that timers the top level leaves pending are wizened, that the
// clock a resumed instance's exports read continues past the snapshot, that
// a module importing the main module's file gets the main module, that what
// the top level prints is returned with the component, and that the top level
// reads the location it is given, and the environment variables and read-write directories of
// the init environment.

mod shared;

use {
    std::{
        path::{Path, PathBuf},
        time::Duration,
    },
    wasmtime::{
        component::{Component, Linker},
        Engine,
    },
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

async fn build(js: impl Into<componentize::JsSource<'_>>) -> anyhow::Result<Vec<u8>> {
    let runtime = shared::runtime().expect("reading the runtime build");
    componentize::componentize(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/top_level.wit")),
        Some("top-level"),
        &[],
        false,
        js,
        Some(base_directory()),
        &runtime,
        &[],
        None,
    )
    .await
}

/// The guest's static, JSON and dynamic imports and the value its top-level
/// `await` waited on a timer for are all in the snapshot, and its `const` export
/// is initialized.
#[tokio::test]
async fn imports_and_top_level_await_are_wizened() -> anyhow::Result<()> {
    let component = build(include_str!("fixtures/top_level/main.js")).await?;
    let component = Component::new(&ENGINE, component)?;
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker)?;
    componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
    let mut store = shared::store(&ENGINE);
    let instance = TopLevelPre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
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
        message.contains("Cannot find module './lib/missing.js'"),
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

/// A main module named by the host path of a file in the base directory is
/// compiled under that file's path relative to the base directory, so its stack
/// traces show no host path.
#[tokio::test]
async fn main_module_is_named_by_its_path_in_the_base_directory() {
    let host_path = base_directory().join("main.js");
    let err = build(componentize::JsSource {
        name: host_path.to_str().expect("a UTF-8 fixture path"),
        text: "throw new Error(`stack: ${new Error().stack}`);",
    })
    .await
    .expect_err("a guest whose top level throws must not componentize");
    let message = format!("{err:#}");
    assert!(message.contains("stack: @/main.js:1:"), "{message}");
    let base = base_directory();
    assert!(
        !message.contains(base.to_str().expect("a UTF-8 fixture path")),
        "{message}"
    );
}

/// A top-level `await` on a promise nothing settles fails componentization
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
/// with a message listing each piece of work and the stack that created it. The
/// timers it leaves pending are not listed, since the snapshot holds them.
#[tokio::test]
async fn pending_work_fails_componentization_naming_its_origin() {
    let text = "function listen() {\n\
                  const { port1, port2 } = new MessageChannel();\n\
                  port1.onmessage = () => {};\n\
                  return [port1, port2];\n\
                }\n\
                globalThis.ports = listen();\n\
                setInterval(() => {}, 1000);\n\
                setTimeout(() => {}, 50);\n\
                export const greeting = () => '';";
    let runtime = shared::runtime().expect("reading the runtime build");
    // With Node.js compatibility, a port with a `message` listener keeps the
    // event loop alive.
    let err = componentize::componentize_with_output(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/top_level.wit")),
        Some("top-level"),
        &[],
        false,
        componentize::JsSource {
            name: "pending.js",
            text,
        },
        Some(base_directory()),
        &runtime,
        &[],
        None,
        false,
        true,
        None,
        false,
        None,
        true,
        &componentize::InitEnvironment::default(),
    )
    .await
    .err()
    .expect("a guest that leaves work pending must not componentize");
    let message = format!("{err:#}");
    assert!(
        message.contains("finished evaluating with asynchronous work still pending"),
        "{message}"
    );
    assert!(
        message.contains(
            "a queued `MessagePort` task, created at:\n      \
             listen@pending.js:3:"
        ),
        "the port and its stack are listed: {message}"
    );
    assert!(!message.contains("timer"), "{message}");
}

/// Timers the top level leaves pending don't fail componentization.
#[tokio::test]
async fn pending_timers_are_wizened() -> anyhow::Result<()> {
    build(
        "setInterval(() => {}, 1000);\n\
         setTimeout(() => {}, 50);\n\
         export const greeting = () => '';",
    )
    .await?;
    Ok(())
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

/// `performance.now()` in an export of a resumed instance continues past the
/// reading the top level took before the snapshot, though the resumed
/// instance's host clock starts over at zero.
#[tokio::test]
async fn export_clock_continues_past_the_snapshot() -> anyhow::Result<()> {
    let component = build(
        "await new Promise((resolve) => setTimeout(resolve, 500));\n\
         const atInit = performance.now();\n\
         export const greeting = () => String(performance.now() - atInit);",
    )
    .await?;
    let component = Component::new(&ENGINE, component)?;
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker)?;
    componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
    let mut store = shared::store(&ENGINE);
    let instance = TopLevelPre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    let first: f64 = instance.call_greeting(&mut store).await?.parse()?;
    std::thread::sleep(Duration::from_millis(50));
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
    let component = Component::new(&ENGINE, component)?;
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker)?;
    componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
    let mut store = shared::store(&ENGINE);
    let instance = TopLevelPre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    assert_eq!(instance.call_greeting(&mut store).await?, "1 object");
    Ok(())
}

/// What the top level prints while it initializes is returned with the component.
#[tokio::test]
async fn init_output_is_returned() -> anyhow::Result<()> {
    let runtime = shared::runtime().expect("reading the runtime build");
    let componentized = componentize::componentize_with_output(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/top_level.wit")),
        Some("top-level"),
        &[],
        false,
        "console.log('hello from init');\nconsole.error('warned at init');\n\
         export const greeting = () => '';",
        Some(base_directory()),
        &runtime,
        &[],
        None,
        false,
        false,
        None,
        false,
        None,
        true,
        &componentize::InitEnvironment::default(),
    )
    .await?;
    assert_eq!(
        componentized.init_output,
        "hello from init\nError: warned at init\n"
    );
    Ok(())
}

/// The top level reads the location it is given as `globalThis.location`, and
/// reading it without one throws.
#[tokio::test]
async fn top_level_reads_the_init_location() -> anyhow::Result<()> {
    const MAIN: &str = "\
        let href;\n\
        try { href = location.href; } catch (e) { href = `${e.name}`; }\n\
        export const greeting = () => href;";
    let runtime = shared::runtime().expect("reading the runtime build");
    for (init_location, expected) in [
        (
            Some("http://example.localhost/app/"),
            "http://example.localhost/app/",
        ),
        (None, "TypeError"),
    ] {
        let componentized = componentize::componentize_with_output(
            componentize::Wit::<PathBuf>::String(include_str!("fixtures/top_level.wit")),
            Some("top-level"),
            &[],
            false,
            MAIN,
            Some(base_directory()),
            &runtime,
            &[],
            None,
            false,
            false,
            None,
            false,
            init_location,
            true,
            &componentize::InitEnvironment::default(),
        )
        .await?;
        let component = Component::new(&ENGINE, componentized.component)?;
        let mut linker = Linker::new(&ENGINE);
        componentize::add_wasi(&mut linker)?;
        componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
        let mut store = shared::store(&ENGINE);
        let instance = TopLevelPre::new(linker.instantiate_pre(&component)?)?
            .instantiate_async(&mut store)
            .await?;
        assert_eq!(instance.call_greeting(&mut store).await?, expected);
    }
    Ok(())
}

/// The top level reads the init environment's variables and writes to its read-write
/// directories, and what it wrote stays in the host directory.
#[tokio::test]
async fn top_level_gets_the_init_environment() -> anyhow::Result<()> {
    const MAIN: &str = "\
        import { writeFileSync, readFileSync } from 'node:fs';\n\
        writeFileSync(`${process.env.DATA_DIR}/written.txt`, process.env.GREETING);\n\
        const read = readFileSync(`${process.env.DATA_DIR}/written.txt`, 'utf8');\n\
        export const greeting = () => read;";
    let runtime = shared::runtime().expect("reading the runtime build");
    let host = std::env::temp_dir().join(format!("starling-init-dir-{}", std::process::id()));
    std::fs::create_dir_all(&host)?;
    let init = componentize::InitEnvironment {
        env: vec![
            ("DATA_DIR".into(), "/data".into()),
            ("GREETING".into(), "hello from init".into()),
        ],
        dirs: vec![(host.clone(), "/data".into())],
    };
    let componentized = componentize::componentize_with_output(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/top_level.wit")),
        Some("top-level"),
        &[],
        false,
        MAIN,
        Some(base_directory()),
        &runtime,
        &[],
        None,
        false,
        true,
        None,
        false,
        None,
        true,
        &init,
    )
    .await;
    let written = std::fs::read_to_string(host.join("written.txt"));
    std::fs::remove_dir_all(&host)?;
    let component = Component::new(&ENGINE, componentized?.component)?;
    assert_eq!(written?, "hello from init");
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker)?;
    componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
    let mut store = shared::store(&ENGINE);
    let instance = TopLevelPre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    assert_eq!(instance.call_greeting(&mut store).await?, "hello from init");
    Ok(())
}

/// With `node_compat`, the top level can use the `node:` builtin modules, the `process` global
/// and `require`, and what it computes is in the snapshot. Without it, `node:` modules don't
/// resolve.
#[tokio::test]
async fn node_compat_enables_node_builtins() -> anyhow::Result<()> {
    const MAIN: &str = "\
        import path from 'node:path';\n\
        import { createRequire } from 'node:module';\n\
        const require = createRequire(import.meta.url);\n\
        const value = require('./lib/value.cjs').value;\n\
        const summary = `${path.join('a', 'b')} ${typeof process.nextTick} ${value}`;\n\
        export const greeting = () => summary;";
    let runtime = shared::runtime().expect("reading the runtime build");
    // Named by its path in the base directory, so the main module is that file's module and has
    // an `import.meta.url` to `createRequire` from.
    let name = base_directory().join("main.js").display().to_string();
    let init = componentize::InitEnvironment::default();
    let build = |node_compat| {
        componentize::componentize_with_output(
            componentize::Wit::<PathBuf>::String(include_str!("fixtures/top_level.wit")),
            Some("top-level"),
            &[],
            false,
            componentize::JsSource {
                name: &name,
                text: MAIN,
            },
            Some(base_directory()),
            &runtime,
            &[],
            None,
            false,
            node_compat,
            None,
            false,
            None,
            true,
            &init,
        )
    };
    let error = build(false)
        .await
        .err()
        .expect("without node_compat, node:path must not resolve");
    assert!(
        format!("{error:#}").contains("No such built-in module: node:path"),
        "{error:#}"
    );

    let component = Component::new(&ENGINE, build(true).await?.component)?;
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker)?;
    componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
    let mut store = shared::store(&ENGINE);
    let instance = TopLevelPre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    assert_eq!(
        instance.call_greeting(&mut store).await?,
        "a/b function commonjs"
    );
    Ok(())
}

/// With `node_compat`, a component that doesn't export `wasi:http/handler` fails to build when its
/// top level calls `http.Server.listen()`, since nothing would serve the server's requests.
#[tokio::test]
async fn node_compat_listen_needs_an_http_handler() -> anyhow::Result<()> {
    const MAIN: &str = "\
        import http from 'node:http';\n\
        http.createServer(() => {}).listen(3000);\n\
        export const greeting = () => 'unreachable';";
    let runtime = shared::runtime().expect("reading the runtime build");
    let name = base_directory().join("main.js").display().to_string();
    let error = componentize::componentize_with_output(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/top_level.wit")),
        Some("top-level"),
        &[],
        false,
        componentize::JsSource {
            name: &name,
            text: MAIN,
        },
        Some(base_directory()),
        &runtime,
        &[],
        None,
        false,
        true,
        None,
        false,
        None,
        true,
        &componentize::InitEnvironment::default(),
    )
    .await
    .err()
    .expect("listen() must fail without a wasi:http/handler export");
    assert!(
        format!("{error:#}").contains("ERR_SERVE_MODE_REQUIRED")
            || format!("{error:#}").contains("componentize it with --serve"),
        "{error:#}"
    );
    Ok(())
}

/// With `node_compat`, a CommonJS entry runs through the CommonJS loader, and the component exports
/// the properties of its `module.exports`.
#[tokio::test]
async fn node_compat_runs_commonjs_entries() -> anyhow::Result<()> {
    let runtime = shared::runtime().expect("reading the runtime build");
    let path = base_directory().join("commonjs_main.cjs");
    let name = path.display().to_string();
    let component = componentize::componentize_with_output(
        componentize::Wit::<PathBuf>::String(include_str!("fixtures/top_level.wit")),
        Some("top-level"),
        &[],
        false,
        componentize::JsSource {
            name: &name,
            text: include_str!("fixtures/top_level/commonjs_main.cjs"),
        },
        Some(base_directory()),
        &runtime,
        &[],
        None,
        false,
        true,
        None,
        false,
        None,
        true,
        &componentize::InitEnvironment::default(),
    )
    .await?
    .component;
    let component = Component::new(&ENGINE, component)?;
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker)?;
    componentize::trap_unsatisfied_imports(&ENGINE, &component, &mut linker, &[])?;
    let mut store = shared::store(&ENGINE);
    let instance = TopLevelPre::new(linker.instantiate_pre(&component)?)?
        .instantiate_async(&mut store)
        .await?;
    assert_eq!(instance.call_greeting(&mut store).await?, "a/b commonjs");
    Ok(())
}
