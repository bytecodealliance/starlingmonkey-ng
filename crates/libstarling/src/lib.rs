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
    runtime::register_global_initializer(web_url::add_to_global);
    runtime::register_global_initializer(web_files::add_to_global);
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
/// from `--init-location`, the WPT test globals from `--wpt-mode`, and Node.js compatibility from
/// `--node-compat`, with the Node.js runtime flags `--allow-node-runtime-flags` accepted. Shared
/// between the native and wasm32 entry points, and between the CLI and HTTP ones: a WPT run under
/// `--serve` needs the same globals a command-mode run does.
pub fn apply_pre_init_config(config: &config::RuntimeConfig) -> Result<(), String> {
    if let Some(location) = config.init_location.as_deref() {
        let url = url::Url::parse(location)
            .map_err(|e| format!("Invalid --init-location URL {location:?}: {e}"))?;
        web_globals::worker_location::set_init_location(url);
    }
    if config.wpt_mode {
        runtime::register_global_initializer(wpt_support::add_to_global);
    }
    if config.node_compat {
        cpp_builtins::set_console_level_prefixes(false);
    }
    Ok(())
}

/// Canonicalize the entry script's path. The entry script's filename becomes its URL in the
/// debugger, which maps only absolute paths to client paths. A path that cannot be canonicalized
/// is kept as given. Does nothing for a `-e` script.
fn canonicalize_script_path(config: &mut config::RuntimeConfig) {
    if config.eval_script.is_some() {
        return;
    }
    if let Some(path) = std::fs::canonicalize(&config.script_path)
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
    {
        config.script_path = path;
    }
}

/// Run a JavaScript script or module on native targets.
///
/// Registers all builtin globals and then delegates to [`serve_native::serve`] with `--serve`, and
/// to [`core_runtime::run()`] with a tokio-based event loop driver otherwise.
#[cfg(not(target_arch = "wasm32"))]
pub fn run(mut config: config::RuntimeConfig) -> Result<(), String> {
    register_builtins();
    apply_pre_init_config(&config)?;
    let debugging = config.debug.is_some();
    if debugging {
        canonicalize_script_path(&mut config);
        match config.serve {
            Some(_) => debugger::register_for_serve(),
            None => debugger::register(),
        }
    }
    let result = match config.serve {
        Some(port) => serve_native::serve(config, port),
        None => {
            core_runtime::event_loop::set_running_as_command();
            core_runtime::run(config, drive_event_loop_native)
        }
    };
    if debugging {
        debugger::finish(&result);
    }
    result
}

/// Run a JavaScript script or module on wasm32 targets.
///
/// Must be `.await`ed from inside an async-lifted `wasi:cli/run.run` task
/// — the event loop awaits WASIp3 async-lowered imports (e.g.
/// `monotonic_clock::wait_for`), and those are only valid inside an
/// async-lifted task. The cdylib in the `starling` package provides such a
/// task via `wasip3::cli::command::export!`.
#[cfg(target_arch = "wasm32")]
pub async fn run(mut config: config::RuntimeConfig) -> Result<(), String> {
    register_builtins();
    apply_pre_init_config(&config)?;
    let debugging = config.debug.is_some();
    if debugging {
        canonicalize_script_path(&mut config);
        debugger::register();
    }
    let result = run_wasm(config).await;
    if debugging {
        debugger::finish(&result);
    }
    result
}

/// The body of the wasm32 [`run`], after the builtins are registered.
#[cfg(target_arch = "wasm32")]
async fn run_wasm(config: config::RuntimeConfig) -> Result<(), String> {
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

    core_runtime::event_loop::set_running_as_command();
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

    {
        // SAFETY: `raw_cx` is valid for the duration of the await. `scope`
        // keeps the `Runtime` alive and the realm entered.
        let content = std::pin::pin!(unsafe {
            evaluation.run_to_completion(raw_cx, el, platform::clock::sleep)
        });
        // SAFETY: as above.
        let debugger = std::pin::pin!(unsafe {
            debugger::serve_while_running(
                raw_cx,
                debugger::RunningLoop::Content(el),
                platform::clock::sleep,
            )
        });
        futures_util::future::select(content, debugger).await;
    }

    let settled = evaluation.settled(&scope, "Script evaluation failed");
    drop(evaluation);
    if settled.is_ok() {
        core_runtime::event_loop::with_event_loop(el, |_| core_runtime::lifecycle::exit(&scope));
    }
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
        // SAFETY: the scope (and its Runtime) outlives this future. Since the caller owns both
        // and `block_on` runs synchronously on the same thread, this is guaranteed.
        let raw_cx = unsafe { scope.raw_cx_no_gc() };
        // SAFETY: as above, and `scope` keeps the default realm entered while either future runs.
        let content =
            std::pin::pin!(unsafe { evaluation.run_to_completion(raw_cx, el, tokio::time::sleep) });
        // SAFETY: as above.
        let debugger = std::pin::pin!(unsafe {
            debugger::serve_while_running(
                raw_cx,
                debugger::RunningLoop::Content(el),
                tokio::time::sleep,
            )
        });
        futures_util::future::select(content, debugger).await;
    });

    drop(scope);
    Ok(())
}
