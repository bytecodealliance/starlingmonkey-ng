// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

pub mod config;
pub mod diagnostics;
pub mod event_loop;
pub mod finalization;
pub mod invocation;
pub mod lifecycle;
pub mod module;
pub mod runtime;

pub mod test_util;

pub use js::macros::{
    jsclass, jsfunction_class, jsglobals, jsmethods, jsmodule, jsnamespace, webidl_dictionary,
    webidl_interface, webidl_methods, webidl_namespace, webidl_union, Traceable,
};
use js::{error::ExnThrown, exception};

use crate::runtime::Runtime;

/// Run a JavaScript script or module based on the provided configuration.
///
/// This is the main entry point for the StarlingMonkey runtime. It:
/// 1. Initializes the SpiderMonkey JS engine (once per process)
/// 2. Creates a global object and realm
/// 3. Installs builtin globals and modules.
/// 4. Optionally runs an initializer script
/// 5. Executes the content script (from `--eval` or a file path)
///    in either ES module mode (default) or legacy script mode
/// 6. Delegates to `drive_event_loop` to run the event loop.
///
/// The `drive_event_loop` callback receives the `Runtime`, the
/// `InvocationState` and the script's [`ScriptEvaluation`], and is responsible
/// for driving the event loop to completion using whatever executor and timer
/// mechanism the embedding provides. The invocation arrives unregistered
/// from GC tracing: the callback must register it once it sits at its final
/// address, most simply by wrapping it in [`invocation::InvocationGuard`] or
/// [`invocation::OwnedInvocation`], whose drop also handles the required
/// unregistration, and then drive [`ScriptEvaluation::run_to_completion`].
/// Driving the loop without registering leaves task-held JS objects
/// invisible to the GC.
///
/// On native targets, the callback typically creates an async runtime, e.g.
/// Tokio, and calls `block_on(evaluation.run_to_completion(..., tokio::time::sleep))`.
/// On WASIp3, it spawns the event loop via `wit_bindgen::spawn`.
pub fn run(
    config: config::RuntimeConfig,
    drive_event_loop: impl FnOnce(
        std::rc::Rc<Runtime>,
        invocation::InvocationState,
        &ScriptEvaluation,
    ) -> Result<(), String>,
) -> Result<(), String> {
    let Some((runtime, invocation, evaluation)) = setup(config)? else {
        return Ok(());
    };
    drive_event_loop(std::rc::Rc::clone(&runtime), invocation, &evaluation)?;
    let settled = evaluation.settled(&runtime.default_global(), "Script evaluation failed");
    drop(evaluation);
    settled?;
    // The `exit` hook runs with an event loop of its own, whose tasks never run.
    let invocation = invocation::InvocationState::new();
    // SAFETY: `invocation` stays at this address until it is unregistered below.
    unsafe { runtime.register_invocation(&invocation) };
    event_loop::with_event_loop(invocation.event_loop(), |_| {
        lifecycle::exit(&runtime.default_global())
    });
    runtime.unregister_invocation(&invocation);
    Ok(())
}

/// The content script's source and the filename to report it under: the `-e` string when there is
/// one, otherwise the file at `script_path`.
///
/// Public because the serve path re-evaluates it: with `--serve-isolated` each request gets a
/// fresh global, which needs the script run in it to register a handler.
pub fn content_script(config: &config::RuntimeConfig) -> Result<(String, String), String> {
    if let Some(ref eval) = config.eval_script {
        return Ok((eval.clone(), "<eval>".to_string()));
    }
    let path = &config.script_path;
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("Error reading script '{path}': {e}"))?;
    Ok((source, path.clone()))
}

/// Evaluate a content script into `scope` — as a module or as a classic script, per `module_mode`
/// — with `event_loop` active, and drain the microtasks it queued.
///
/// Shared by the process-wide startup path and the per-request one, since `--serve-isolated`
/// re-evaluates the same script into a fresh global for every request. What follows the evaluation
/// has to be the same either way: microtasks drained even when no event-loop task is queued (a
/// promise reaction the script's top level queued still has to run), with the loop kept active
/// while they drain, since a microtask may itself call `setTimeout`.
///
/// Returns the module's evaluation promise while it is still pending — a module with a top-level
/// `await` has not finished running, and whatever it registers is not registered yet. A classic
/// script, and a module that completed synchronously, return `None`: there is nothing left to
/// wait for. See [`ScriptEvaluation`].
///
/// # Safety
///
/// In module mode, the module loader must have been initialized by [`Runtime::init`].
pub unsafe fn evaluate_content_script(
    scope: &js::gc::scope::Scope<'_>,
    event_loop: &event_loop::EventLoop,
    source: &str,
    filename: &str,
    module_mode: bool,
) -> Result<ScriptEvaluation, String> {
    let eval_result = event_loop::with_event_loop(event_loop, |_| {
        if module_mode {
            // SAFETY: the caller guarantees the module loader was initialized.
            unsafe { module::evaluate_module(scope, source, filename) }
        } else {
            js::compile::evaluate_with_filename(scope, source, filename, 1)
        }
    });

    let value = match eval_result {
        Ok(value) if !exception::is_pending(scope) => value,
        _ => {
            if let Some(error) = exception::take_pending(scope) {
                if event_loop::with_event_loop(event_loop, |_| {
                    lifecycle::uncaught(scope, error, None)
                }) {
                    return Ok(ScriptEvaluation::new(None));
                }
                exception::set_pending(
                    scope,
                    error,
                    js::native::ExceptionStackBehavior::DoNotCapture,
                );
            }
            let exn = ExnThrown::capture(scope);
            return Err(format!("Script evaluation failed with error {exn}"));
        }
    };

    event_loop::with_event_loop(event_loop, |_| {
        event_loop::run_microtasks(scope);
    });

    if !module_mode {
        return Ok(ScriptEvaluation::new(None));
    }
    let promise = js::Object::from_value(scope, value.get())
        .ok()
        .and_then(|object| object.cast::<js::Promise>().ok());
    let evaluation = ScriptEvaluation::new(
        promise.filter(|promise| promise.is_pending() || promise.is_rejected()),
    );
    event_loop::with_event_loop(event_loop, |_| evaluation.report_rejection(scope));
    evaluation.rejection(scope, "Script evaluation failed")?;
    Ok(evaluation)
}

/// What a content script left running: its evaluation promise, if a top-level `await` means it is
/// still going. [`ScriptEvaluation::is_finished`] is how a driver tells that startup is over —
/// the script's own work, and not the timers it may have started along the way.
pub struct ScriptEvaluation {
    pending: Option<js::gc::handle::RootedHeap<js::promise::Promise>>,
    /// Set once the `uncaught` lifecycle hook handled the rejection of `pending`.
    rejection_handled: std::cell::Cell<bool>,
}

impl ScriptEvaluation {
    /// The evaluation of a module whose evaluation promise is `pending`, as
    /// [`module::settled_module_evaluation`] returns it. `None` is a finished evaluation.
    pub fn new(pending: Option<js::Promise<'_>>) -> Self {
        Self {
            pending: pending.map(js::gc::handle::RootedHeap::new),
            rejection_handled: std::cell::Cell::new(false),
        }
    }

    /// If the script's top-level `await` rejected, pass the reason to the `uncaught` lifecycle
    /// hook, once. A rejection the hook handled counts as a fulfillment from then on.
    fn report_rejection(&self, scope: &js::gc::scope::Scope<'_>) {
        let Some(promise) = &self.pending else {
            return;
        };
        let promise = promise.get(scope);
        if !promise.is_rejected() || self.rejection_handled.get() || !lifecycle::has_hooks() {
            return;
        }
        let Some(reason) = promise.result(scope) else {
            return;
        };
        self.rejection_handled
            .set(lifecycle::uncaught(scope, reason, None));
    }

    /// Whether the script has finished evaluating.
    pub fn is_finished(&self, scope: &js::gc::scope::Scope<'_>) -> bool {
        self.pending
            .as_ref()
            .is_none_or(|promise| !promise.get(scope).is_pending())
    }

    /// `Err` with `context: reason` if the script's top-level `await` rejected after this
    /// evaluation was created, and `Ok` otherwise, including while it is still pending.
    pub fn rejection(&self, scope: &js::gc::scope::Scope<'_>, context: &str) -> Result<(), String> {
        match &self.pending {
            Some(promise) if !self.rejection_handled.get() => {
                let promise = promise.get(scope);
                if promise.is_rejected() {
                    Err(module::rejection_message(scope, &promise, context))
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }

    /// Whether the script's top-level `await` rejected, and the `uncaught` lifecycle hook didn't
    /// handle the rejection.
    pub fn is_rejected(&self, scope: &js::gc::scope::Scope<'_>) -> bool {
        !self.rejection_handled.get()
            && self
                .pending
                .as_ref()
                .is_some_and(|promise| promise.get(scope).is_rejected())
    }

    /// Run `event_loop` until it has nothing left, or until the script's top-level `await`
    /// rejects. A rejection is passed to the `uncaught` lifecycle hook. If the hook doesn't
    /// handle it, the rejection ends the run once the current step finishes, leaving the loop's
    /// remaining tasks unrun, and cancels its pending futures. Each time the loop has nothing
    /// left, the `before_exit` lifecycle hook is called, and the run continues while the work it
    /// queued keeps the loop alive.
    ///
    /// # Safety
    ///
    /// `raw_cx` must be a valid JSContext pointer whose default global's realm is entered, and
    /// which stays valid for the lifetime of this future.
    pub async unsafe fn run_to_completion<S, F>(
        &self,
        raw_cx: *mut js::native::RawJSContext,
        event_loop: &event_loop::EventLoop,
        sleep: S,
    ) where
        S: Fn(std::time::Duration) -> F,
        F: std::future::Future<Output = ()>,
    {
        loop {
            // SAFETY: guaranteed by this function's caller.
            unsafe {
                event_loop::run_until(raw_cx, event_loop, &sleep, |scope| self.is_rejected(scope))
                    .await;
            }
            // SAFETY: guaranteed by this function's caller.
            let scope = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
            event_loop::with_event_loop(event_loop, |_| self.report_rejection(&scope));
            if self.is_rejected(&scope) {
                event_loop.cancel_pending_futures();
                return;
            }
            if self.rejection_handled.get() && event_loop.is_alive() {
                continue;
            }
            // The `before_exit` hook may queue more work, which runs before it is called again.
            let hooked =
                event_loop::with_event_loop(event_loop, |_| lifecycle::before_exit(&scope));
            if !hooked || !event_loop.is_alive() {
                return;
            }
        }
    }

    /// `Err` with `context: reason` if the script's top-level `await` rejected, `Err` with a
    /// message that the top-level `await` never settled if it did not, and `Ok` if it fulfilled. Called once the event loop
    /// has run out of work.
    pub fn settled(&self, scope: &js::gc::scope::Scope<'_>, context: &str) -> Result<(), String> {
        self.rejection(scope, context)?;
        if !self.is_finished(scope) {
            return Err(
                "the script's top-level `await` never settled: the event loop ran out of work \
                 while it was still pending"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// Initialize the runtime and evaluate the script (the synchronous portion of an
/// invocation), draining initial microtasks. Returns the runtime and its
/// invocation, with the invocation unregistered from the GC tracer. It is
/// registered only for the eval phase here, then unregistered before being moved
/// out, since the registry holds a raw pointer that the move would invalidate. The
/// caller re-registers at the final location, drives the loop, then unregisters.
fn init_and_eval(
    config: config::RuntimeConfig,
) -> Result<
    (
        std::rc::Rc<Runtime>,
        invocation::InvocationState,
        ScriptEvaluation,
    ),
    String,
> {
    let runtime = Runtime::init(&config)?;
    runtime::run_before_content_hooks(&runtime, &config)?;

    let (source, filename) = content_script(&config)?;

    let invocation = invocation::InvocationState::new();

    // SAFETY: `invocation` lives at this address until it is unregistered on
    // every path out of this function, so the tracer never dereferences a
    // freed or moved-from slot.
    unsafe { runtime.register_invocation(&invocation) };

    let scope = runtime.default_global();
    // SAFETY: the default global's realm is entered, and `Runtime::init` above initialized the
    // module loader.
    let evaluated = unsafe {
        evaluate_content_script(
            &scope,
            invocation.event_loop(),
            &source,
            &filename,
            config.module_mode(),
        )
    };
    let evaluation = match evaluated {
        Ok(evaluation) => evaluation,
        Err(message) => {
            runtime.unregister_invocation(&invocation);
            return Err(message);
        }
    };
    drop(scope);

    // Unregister before moving `invocation` out: the move invalidates the raw
    // pointer the registry holds. The caller re-registers at the final address.
    runtime.unregister_invocation(&invocation);
    Ok((runtime, invocation, evaluation))
}

/// Perform the synchronous portion of a runtime invocation: initialize the
/// runtime, evaluate the script, drain initial microtasks, and check whether
/// any asynchronous work remains.
///
/// Returns:
/// - `Ok(Some((runtime, invocation, evaluation)))` if the script, or the
///   `before_exit` lifecycle hook, left work in the event loop. The caller must
///   call [`Runtime::register_invocation`] once the invocation reaches its final
///   (stable) location, drive [`ScriptEvaluation::run_to_completion`] to
///   completion, then call [`Runtime::unregister_invocation`] and check
///   [`ScriptEvaluation::settled`] before dropping the runtime. If `settled`
///   returns `Ok`, the caller then calls [`lifecycle::exit`].
/// - `Ok(None)` if the script completed without scheduling any async work. The
///   `before_exit` and `exit` lifecycle hooks were called, and the `exit` hook
///   may have ended the process.
/// - `Err(_)` if the script failed to parse, threw during top-level
///   evaluation, or awaits a promise that nothing can settle.
///
/// Splitting this out lets the wasm32 cdylib driver hold an async event-loop
/// future on its own stack instead of bridging through a sync callback.
pub fn setup(
    config: config::RuntimeConfig,
) -> Result<
    Option<(
        std::rc::Rc<Runtime>,
        invocation::InvocationState,
        ScriptEvaluation,
    )>,
    String,
> {
    let (runtime, invocation, evaluation) = init_and_eval(config)?;
    if invocation.event_loop().is_alive() {
        return Ok(Some((runtime, invocation, evaluation)));
    }
    // SAFETY: `invocation` stays at this address until it is unregistered below, before it is
    // moved.
    unsafe { runtime.register_invocation(&invocation) };
    let scope = runtime.default_global();
    if !evaluation.is_rejected(&scope) {
        // The `before_exit` hook may queue work, which the caller then runs.
        event_loop::with_event_loop(invocation.event_loop(), |_| lifecycle::before_exit(&scope));
    }
    if invocation.event_loop().is_alive() {
        drop(scope);
        runtime.unregister_invocation(&invocation);
        return Ok(Some((runtime, invocation, evaluation)));
    }
    let settled = evaluation.settled(&scope, "Script evaluation failed");
    drop(evaluation);
    if settled.is_ok() {
        event_loop::with_event_loop(invocation.event_loop(), |_| lifecycle::exit(&scope));
    }
    drop(scope);
    runtime.unregister_invocation(&invocation);
    settled.map(|()| None)
}

/// Like [`setup`], but always returns the runtime — for **serve mode**, where the
/// runtime must outlive the script (which registers `fetch` handlers) so it can
/// handle incoming requests. The returned invocation holds the script's initial
/// event loop; drive it to completion (the script's top-level async) before
/// serving, then keep the runtime alive for the accept loop.
pub fn setup_for_serve(
    config: config::RuntimeConfig,
) -> Result<
    (
        std::rc::Rc<Runtime>,
        invocation::InvocationState,
        ScriptEvaluation,
    ),
    String,
> {
    init_and_eval(config)
}

#[cfg(test)]
mod tests {
    use super::config::RuntimeConfig;
    use super::*;
    use crate::test_util::test_tempdir;

    fn config_from(args: &[&str]) -> RuntimeConfig {
        RuntimeConfig::from_args(args.iter().map(|s| s.to_string())).unwrap()
    }

    /// Event-loop driver for tests whose scripts complete synchronously.
    ///
    /// `run` only invokes the driver when the script left async work in the
    /// event loop, so for these tests being called at all is a failure.
    fn noop_driver(
        _runtime: std::rc::Rc<runtime::Runtime>,
        _invocation: invocation::InvocationState,
        _evaluation: &ScriptEvaluation,
    ) -> Result<(), String> {
        Err("script unexpectedly left the event loop alive".to_string())
    }

    #[test]
    fn run_eval_module_mode() {
        let config = config_from(&["starling", "-e", "globalThis._x = 1 + 2;"]);
        assert!(config.module_mode());
        run(config, noop_driver)
            .map_err(|e| println!("{e}"))
            .expect("Run failed");
    }

    #[test]
    fn run_eval_legacy_script() {
        let config = config_from(&["starling", "-e", "var x = 42;", "--legacy-script"]);
        assert!(!config.module_mode());
        run(config, noop_driver)
            .map_err(|e| println!("{e}"))
            .expect("Run failed");
    }

    #[test]
    fn run_file_module_mode() {
        let dir = test_tempdir();
        let script = dir.path().join("test.mjs");
        std::fs::write(&script, "const x = 1 + 2;\n").unwrap();

        let config = config_from(&["starling", &script.to_string_lossy()]);
        run(config, noop_driver)
            .map_err(|e| println!("{e}"))
            .expect("Run failed");
    }

    #[test]
    fn run_file_legacy_script() {
        let dir = test_tempdir();
        let script = dir.path().join("test.js");
        std::fs::write(&script, "var x = 1 + 2;\n").unwrap();

        let config = config_from(&["starling", &script.to_string_lossy(), "--legacy-script"]);
        run(config, noop_driver)
            .map_err(|e| println!("{e}"))
            .expect("Run failed");
    }

    #[test]
    fn run_file_with_imports() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("helper.js"), "export const V = 10;\n").unwrap();
        let entry = dir.path().join("main.mjs");
        std::fs::write(
            &entry,
            r#"import { V } from "./helper.js"; globalThis._v = V;"#,
        )
        .unwrap();

        let config = config_from(&["starling", &entry.to_string_lossy()]);
        run(config, noop_driver)
            .map_err(|e| println!("{e}"))
            .expect("Run failed");
    }

    #[test]
    fn run_with_initializer_script() {
        let dir = test_tempdir();
        let init = dir.path().join("init.js");
        std::fs::write(&init, "globalThis._initialized = true;\n").unwrap();
        let main = dir.path().join("main.mjs");
        std::fs::write(&main, "const ok = globalThis._initialized;\n").unwrap();

        let config = config_from(&[
            "starling",
            &main.to_string_lossy(),
            "-i",
            &init.to_string_lossy(),
        ]);
        run(config, noop_driver)
            .map_err(|e| println!("{e}"))
            .expect("Run failed");
    }
}
