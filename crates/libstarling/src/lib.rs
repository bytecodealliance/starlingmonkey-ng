// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

// Re-export everything from core-runtime.
pub use core_runtime::*;

/// Native HTTP serve mode (the `--serve` flag).
#[cfg(not(target_arch = "wasm32"))]
pub mod serve_native;

/// The per-request dispatch core shared by the serve modes.
mod serve_common;

/// Wasm HTTP serve mode: the body of the `wasi:http/handler` export.
#[cfg(target_arch = "wasm32")]
pub mod serve_wasm;

/// Register all built-in global initializers.
///
/// This must be called before `Runtime::init()` to ensure built-in web
/// globals (like `btoa`, `atob`) are installed on every global object.
pub fn register_builtins() {
    runtime::register_global_initializer(web_globals::add_to_global);
    runtime::register_global_initializer(web_streams::add_to_global);
    runtime::register_global_initializer(encoding::add_to_global);
    runtime::register_global_initializer(web_url::add_to_global);
    runtime::register_global_initializer(web_fetch::add_to_global);
    runtime::register_global_initializer(fetch_event::add_to_global);
    // SAFETY: a global initializer runs with the new global's realm entered, and `global` is
    // that global.
    runtime::register_global_initializer(|scope, global| unsafe {
        cpp_builtins::install(scope.cx_mut().raw_cx(), global.handle());
    });

    // `performance`'s time origin is a monotonic-clock reading, which belongs to whichever process
    // took the Wizer snapshot: a resumed instance reads `performance.now()` as zero until its own
    // clock catches up. Registered alongside the builtin itself, so the snapshot machinery does not
    // have to know which builtins keep state that cannot cross one.
    runtime::register_resume_fixup(web_globals::performance::reset_time_origin);
}

/// Apply CLI options that take effect before the runtime is initialized: the worker location URL
/// from `--init-location`, and the WPT test globals from `--wpt-mode`. Shared between the native
/// and wasm32 entry points, and between the CLI and HTTP ones — a WPT run under `--serve` needs
/// the same globals a command-mode run does.
fn apply_pre_init_config(config: &config::RuntimeConfig) -> Result<(), String> {
    if let Some(location) = config.init_location.as_deref() {
        let url = url::Url::parse(location)
            .map_err(|e| format!("Invalid --init-location URL {location:?}: {e}"))?;
        web_globals::worker_location::set_init_location(url);
    }
    if config.wpt_mode {
        runtime::register_global_initializer(wpt_support::add_to_global);
    }
    Ok(())
}

/// Run a JavaScript script or module on native targets.
///
/// Registers all builtin globals and then delegates to [`core_runtime::run()`]
/// with a tokio-based event loop driver.
#[cfg(not(target_arch = "wasm32"))]
pub fn run(config: config::RuntimeConfig) -> Result<(), String> {
    apply_pre_init_config(&config)?;
    register_builtins();
    if let Some(port) = config.serve {
        return serve_native::serve(config, port);
    }
    core_runtime::run(config, drive_event_loop_native)
}

/// Run a JavaScript script or module on wasm32 targets.
///
/// Must be `.await`ed from inside an async-lifted `wasi:cli/run.run` task
/// — the event loop awaits WASIp3 async-lowered imports (e.g.
/// `monotonic_clock::wait_for`), and those are only valid inside an
/// async-lifted task. The cdylib in the `starling` package provides such a
/// task via `wasip3::cli::command::export!`.
#[cfg(target_arch = "wasm32")]
pub async fn run(config: config::RuntimeConfig) -> Result<(), String> {
    apply_pre_init_config(&config)?;
    register_builtins();

    // `--serve` has no meaning for this entry point. Serving on wasm is the `wasi:http/handler`
    // export (see `serve_wasm`), which the host invokes directly — the component exports both, and
    // running it as a command is the host having picked the other one. Say so rather than running
    // the script once and exiting, which looks like the server immediately gave up.
    if config.serve.is_some() {
        return Err(
            "--serve is not supported when running as a command: serve the component with a \
             wasi:http host (e.g. `wasmtime serve`), which invokes its wasi:http/handler export"
                .to_string(),
        );
    }

    let Some((runtime, invocation, evaluation)) = core_runtime::setup(config)? else {
        return Ok(());
    };

    // Register `invocation` for GC tracing now that it has reached its final,
    // stable location. `setup` hands it back unregistered precisely because
    // moving it here would have invalidated any earlier registration. The guard
    // unregisters on every exit, including this future being dropped.
    //
    // SAFETY: `invocation` lives in this frame, declared before the guard, so
    // it outlives it at a stable address.
    let _invocation_guard = unsafe { invocation::InvocationGuard::new(&runtime, &invocation) };

    // Enter the default global's realm for the lifetime of the event loop.
    // Tasks running inside `run_to_completion` (timers, promise reactions,
    // etc.) need an active realm to call into JS.
    let scope = runtime.default_global();
    // SAFETY: `scope` (and the `Runtime` it borrows from) lives until the
    // explicit `drop(scope)` below.
    let raw_cx = unsafe { scope.raw_cx_no_gc() };
    let el = invocation.event_loop();

    // SAFETY: `raw_cx` is valid for the duration of the await — `scope`
    // keeps the `Runtime` alive and the realm entered.
    unsafe {
        evaluation
            .run_to_completion(raw_cx, el, platform::clock::sleep)
            .await;
    }

    let settled = evaluation.settled(&scope, "Script evaluation failed");
    drop(evaluation);
    drop(scope);
    settled
}

/// Drive the event loop on native targets using a tokio current-thread
/// runtime with async timer support.
#[cfg(not(target_arch = "wasm32"))]
fn drive_event_loop_native(
    runtime: std::rc::Rc<runtime::Runtime>,
    invocation: invocation::InvocationState,
    evaluation: &core_runtime::ScriptEvaluation,
) -> Result<(), String> {
    // Enable both the timer and IO drivers: timers back `setTimeout`, and IO backs
    // `fetch`'s async HTTP transport (which uses tokio TCP sockets).
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to create tokio runtime: {e}"))?;

    // Register `invocation` for GC tracing now that it has reached its final,
    // stable location. `setup` hands it back unregistered precisely because
    // moving it here would have invalidated any earlier registration. The guard
    // unregisters on every exit.
    //
    // SAFETY: `invocation` lives in this frame, declared before the guard, so
    // it outlives it at a stable address.
    let _invocation_guard = unsafe { invocation::InvocationGuard::new(&runtime, &invocation) };

    // Enter the default global's realm for the lifetime of the event loop.
    // Tasks running inside `run_to_completion` (timers, promise reactions,
    // etc.) need an active realm to call into JS.
    let scope = runtime.default_global();
    let el = invocation.event_loop();

    tokio_rt.block_on(async {
        // SAFETY: the scope (and its Runtime) must outlive this future.
        // Since the caller owns both and `block_on` runs synchronously on
        // the same thread, this is guaranteed.
        let raw_cx = unsafe { scope.raw_cx_no_gc() };
        unsafe {
            evaluation
                .run_to_completion(raw_cx, el, tokio::time::sleep)
                .await
        }
    });

    drop(scope);
    Ok(())
}
