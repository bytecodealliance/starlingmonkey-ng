// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `wasi:cli/run` components: JS modules that export `run`, componentized against the world of
//! `fixtures/cli_run.wit` and run through `wasi:cli/run.run()`.

mod shared;

use {
    bytes::Bytes,
    tokio::sync::OnceCell,
    wasmtime::{
        component::{Component, Linker},
        Engine, Store,
    },
    wasmtime_wasi::{
        p2::pipe::{MemoryInputPipe, MemoryOutputPipe},
        p3::bindings::Command,
        WasiCtxBuilder, WasiCtxView, WasiView,
    },
};

/// The host store data: WASI context (with captured stdout/stderr) + resource
/// table. The CLI world has no imports of its own.
struct Ctx {
    wasi: shared::Wasi,
    stdout: MemoryOutputPipe,
    stderr: MemoryOutputPipe,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.view()
    }
}

static ENGINE: std::sync::LazyLock<Engine> = std::sync::LazyLock::new(shared::engine);

const WIT: &str = include_str!("fixtures/cli_run.wit");

/// Componentize a CLI application against the fixed CLI world and compile it.
async fn componentize_cli(js: &'static str) -> Component {
    shared::compile_fixture(&ENGINE, WIT, "command", js).await
}

/// The componentized CLI app that exports `run`, built once.
async fn run_exporting_app() -> &'static Component {
    static POSITIVE: OnceCell<Result<Component, String>> = OnceCell::const_new();
    shared::build_once(
        &POSITIVE,
        componentize_cli(include_str!("fixtures/cli_run.js")),
    )
    .await
}

/// A CLI app whose async `run` rejects, built once.
async fn rejecting() -> &'static Component {
    static REJECTING: OnceCell<Result<Component, String>> = OnceCell::const_new();
    shared::build_once(
        &REJECTING,
        componentize_cli("export async function run() { throw new Error('boom from run'); }"),
    )
    .await
}

/// A CLI app whose async `run` never settles, built once.
async fn unsettled() -> &'static Component {
    static UNSETTLED: OnceCell<Result<Component, String>> = OnceCell::const_new();
    shared::build_once(
        &UNSETTLED,
        componentize_cli("export async function run() { await new Promise(() => {}); }"),
    )
    .await
}

/// A fresh store with captured stdout/stderr.
fn store() -> Store<Ctx> {
    let stdout = MemoryOutputPipe::new(64 * 1024);
    let stderr = MemoryOutputPipe::new(64 * 1024);
    let wasi = shared::Wasi::from_builder(
        WasiCtxBuilder::new()
            .stdin(MemoryInputPipe::new(Bytes::new()))
            .stdout(stdout.clone())
            .stderr(stderr.clone()),
    );
    Store::new(
        &ENGINE,
        Ctx {
            wasi,
            stdout,
            stderr,
        },
    )
}

/// A linker for the CLI world: the WASI host, and trapping stubs for the
/// runtime's other imports.
fn linker(component: &Component) -> Linker<Ctx> {
    let mut linker = Linker::new(&ENGINE);
    componentize::add_wasi(&mut linker).expect("wasi linker");
    componentize::trap_unsatisfied_imports(&ENGINE, component, &mut linker, &[])
        .expect("trap-stub unknown imports");
    linker
}

/// Drive `wasi:cli/run.run()` on a component, returning the program result and
/// the captured stdout/stderr.
async fn run_cli(component: &Component) -> anyhow::Result<(Result<(), ()>, String, String)> {
    let mut store = store();
    let command = Command::instantiate_async(&mut store, component, &linker(component)).await?;
    let result = store
        .run_concurrent(async move |store| command.wasi_cli_run().call_run(store).await)
        .await??;
    let stdout = String::from_utf8_lossy(&store.data().stdout.contents()).into_owned();
    let stderr = String::from_utf8_lossy(&store.data().stderr.contents()).into_owned();
    Ok((result, stdout, stderr))
}

/// A CLI app that exports `run` has `wasi:cli/run.run()` forwarded to it: the
/// JS `run` awaits a real timer, writes its lines, and the call exits 0. Output
/// its top level wrote under Wizer is not part of `run()`'s output, and
/// `performance.now()` counts from the resume. The component exports
/// `wasi:cli/run` under the world's name for it, a version compatible with the
/// runtime builtin's but different from it.
#[tokio::test]
async fn cli_run_forwards_to_js_run() -> anyhow::Result<()> {
    let exports: Vec<String> = run_exporting_app()
        .await
        .component_type()
        .exports(&ENGINE)
        .map(|(name, _)| name.to_string())
        .collect();
    assert!(
        exports.contains(&"wasi:cli/run@0.3.5".to_string()),
        "{exports:?}"
    );
    let (result, stdout, _stderr) = run_cli(run_exporting_app().await).await?;
    assert_eq!(
        result,
        Ok(()),
        "cli/run.run() on a run-exporting app should exit 0"
    );
    assert!(
        stdout.contains("cli run start") && stdout.contains("cli run done"),
        "both run() log lines should reach stdout; got: {stdout:?}"
    );
    let (start, done) = (
        stdout.find("cli run start").unwrap(),
        stdout.find("cli run done").unwrap(),
    );
    assert!(
        start < done,
        "run() should log start before done (the timer turn ran in between); got: {stdout:?}"
    );
    assert!(
        !stdout.contains("logged under Wizer"),
        "output written under Wizer should stay in the componentizer's capture; got: {stdout:?}"
    );

    // Componentizing takes seconds, so a `performance.now()` still measured against the
    // wizened instance's clock would read in the thousands rather than the single digits.
    let elapsed: f64 = stdout
        .split("cli run elapsed ")
        .nth(1)
        .and_then(|rest| rest.split('\n').next())
        .expect("run() logs the elapsed time")
        .trim()
        .parse()
        .expect("the elapsed time is a number");
    assert!(
        (0.0..1000.0).contains(&elapsed),
        "`performance.now()` in a resumed instance should be the time since it resumed, got \
         {elapsed} ms"
    );
    Ok(())
}

/// Componentizing a CLI app that exports no `run` fails, naming the function to
/// export.
#[tokio::test]
async fn cli_run_without_run_export_fails_componentization() {
    let error =
        shared::try_componentize(WIT, "command", include_str!("fixtures/cli_run_no_run.js"))
            .await
            .expect_err("an app without `run` must not componentize as a CLI tool");
    let message = format!("{error:#}");
    assert!(
        message.contains("the main module exports no `run` function"),
        "{message}"
    );
}

/// A world exporting `wasi:cli/run` at a version not compatible with the
/// runtime builtin's is rejected, rather than silently componentized against
/// the builtin's version.
#[tokio::test]
async fn cli_run_version_mismatch_is_rejected() {
    const OLD_WIT: &str = "\
package test:cli-run-old;

world command {
  export wasi:cli/run@0.2.3;
}

package wasi:cli@0.2.3 {
  interface run {
    run: func() -> result;
  }
}
";

    let error = shared::try_componentize(OLD_WIT, "command", include_str!("fixtures/cli_run.js"))
        .await
        .expect_err("a mismatched cli/run version should be rejected");

    let message = format!("{error:#}");
    assert!(
        message.contains("wasi:cli/run@0.2.3") && message.contains("wasi:cli/run@0.3.0"),
        "the error should name both the world's version and the runtime's; got: {message}"
    );
}

/// An async `run` that rejects reports failure, and stderr holds the rejection
/// reason's message and where it was thrown.
#[tokio::test]
async fn cli_run_rejection_reports_the_reason() -> anyhow::Result<()> {
    let (result, _stdout, stderr) = run_cli(rejecting().await).await?;
    assert_eq!(result, Err(()));
    assert!(
        stderr.contains("run() failed: boom from run"),
        "stderr should hold the rejection message; got: {stderr:?}"
    );
    assert!(
        stderr.contains("main.js:1:"),
        "stderr should hold the rejection's location; got: {stderr:?}"
    );
    Ok(())
}

/// An async `run` whose promise is still pending once the event loop goes idle
/// reports failure.
#[tokio::test]
async fn cli_run_unsettled_promise_fails() -> anyhow::Result<()> {
    let (result, _stdout, stderr) = run_cli(unsettled().await).await?;
    assert_eq!(result, Err(()));
    assert!(
        stderr.contains("run() did not settle"),
        "stderr should report the unsettled promise; got: {stderr:?}"
    );
    Ok(())
}
