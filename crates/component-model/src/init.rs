// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Runtime bootstrap for the component-model interpreter.
//!
//! A componentized JavaScript application runs against a single SpiderMonkey
//! runtime that lives for the whole process. [`initialize_runtime`] creates that
//! runtime, evaluates the application's main ES module, and stashes what the
//! export and import paths need after `init` returns. The default global's realm
//! is entered once and kept entered by leaking the entering scope, so later calls
//! re-root against it with [`with_scope`] or [`with_main_scope`].
//!
//! Builtins must be registered via `register_global_initializer` before this
//! runs.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use core_runtime::config::RuntimeConfig;
use core_runtime::event_loop::with_event_loop;
use core_runtime::invocation::InvocationState;
use core_runtime::runtime::Runtime;
use core_runtime::ScriptEvaluation;
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::{RootScope, Scope};
use js::heap::Trace;
use js::module_raw::transform_str_to_source_text;
use js::native::{JSTracer, RawJSContext};
use js::Object;

js::instance_local! {
    /// The process-lifetime bootstrap state, set once by [`initialize_runtime`].
    static BOOTSTRAP: OnceCell<Bootstrap> = const { OnceCell::new() };

    /// Set by the first [`initialize_runtime`] call, before anything can fail.
    /// `BOOTSTRAP` stays empty when a call fails partway, and the once-per-process
    /// `JSEngine::init` a retry would re-enter aborts, so the "at most once" rule
    /// is enforced on attempts rather than on successes.
    static INITIALIZE_ATTEMPTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// The main module's compiled record, kept alive across calls by
    /// [`trace_main_module`]. As the entry point it is in neither the module
    /// registry nor the leaked scope's slots, so nothing else would trace it.
    static MAIN_MODULE: RefCell<Option<Heap<js::object::Object>>> = const { RefCell::new(None) };

    /// Run at the start of every export call, set by [`set_before_export`].
    static BEFORE_EXPORT: Cell<Option<fn()>> = const { Cell::new(None) };
}

/// Run `hook` at the start of every export call, before any JS runs.
pub fn set_before_export(hook: fn()) {
    BEFORE_EXPORT.with(|cell| cell.set(Some(hook)));
}

/// Run the hook [`set_before_export`] set, if any.
#[cfg(target_arch = "wasm32")]
pub(crate) fn before_export() {
    if let Some(hook) = BEFORE_EXPORT.with(Cell::get) {
        hook();
    }
}

/// One-time process bootstrap state.
struct Bootstrap {
    /// [`ManuallyDrop`](std::mem::ManuallyDrop) because dropping the runtime
    /// would call `DestroyContext` while the leaked scope still has its realm
    /// entered, which SpiderMonkey aborts on. At process exit the OS reclaims it.
    runtime: std::mem::ManuallyDrop<Rc<Runtime>>,
    /// The context whose default global's realm is entered for the process.
    raw_cx: *mut RawJSContext,
}

/// Initialize the component-model runtime and evaluate the application's main
/// module.
///
/// `config`'s [`base_path`](RuntimeConfig::base_path) roots the module loader.
/// `main` is the entry-point ES module source, compiled under the name
/// `main_name`, and exported functions are resolved against its namespace.
/// `main_path` is the path of the file `main` was read from, if any, which a
/// module importing that file then gets `main` for (see
/// [`core_runtime::module::register_entry_module`]). `synthesize` builds the
/// import modules so that an `import { f } from "iface"` resolves. It runs
/// before the main module is compiled.
///
/// A main module with a top-level `await` that microtasks alone do not settle is
/// still evaluating when this returns. The caller drives
/// [`Initialized::invocation`]'s event loop until [`Initialized::evaluation`]
/// finishes, and only then resolves exports against the namespace with
/// [`with_main_scope`].
///
/// Returns `Err` with a human-readable message if the engine fails to start,
/// `synthesize` throws, or a module fails to compile, load, link or evaluate. May
/// be called at most once per process, whether or not the first call succeeded,
/// and a second call returns an error.
pub fn initialize_runtime(
    config: &RuntimeConfig,
    main: &str,
    main_name: &str,
    main_path: Option<&str>,
    synthesize: impl FnOnce(&Scope<'_>) -> Result<(), ExnThrown>,
) -> Result<Initialized, String> {
    if INITIALIZE_ATTEMPTED.with(|attempted| attempted.replace(true)) {
        return Err("component-model runtime already initialized".to_string());
    }

    let runtime = Runtime::init(config)?;

    // Entered here and kept entered for the process by leaking `scope` below.
    let scope = runtime.default_global();
    // SAFETY: the raw context is only stored, and every later read of it goes
    // through `bootstrap_cx`, which is reachable only once `BOOTSTRAP` is set at
    // the end of this function. The runtime that owns the context is kept alive
    // there for the process, and `scope` is leaked below so its realm stays
    // entered.
    let raw_cx = unsafe { scope.raw_cx_no_gc() };

    // The tracer goes in before any value can outlive a collection.
    crate::tracing::install_tracer(&scope);

    // The registry must exist before the main module evaluates, so resource
    // wrapping during init has somewhere to register.
    // Every failure below drops `runtime`, whose teardown traces the crate's
    // roots one last time, so they must be empty by then.
    let fail = |scope: &Scope<'_>, context: &str| -> String {
        let message = capture_message(scope, context);
        crate::tracing::clear_traced_roots();
        message
    };

    crate::resources::init_finalization(&scope)
        .map_err(|_| fail(&scope, "finalization registry failed to initialize"))?;

    synthesize(&scope).map_err(|_| fail(&scope, "import-module synthesis failed"))?;

    // Evaluate the top level under a fresh invocation's event loop, so a
    // top-level `setTimeout` or `fetch` queues onto a loop the caller
    // can later drive. The invocation is traced across the eval so a timer's
    // rooted callback survives a collection, and unregistered before it is
    // returned, since moving it to the caller invalidates the registry's pointer.
    let invocation = InvocationState::new();
    // SAFETY: `invocation` lives at this address until it is unregistered on
    // every path out below, so the tracer never dereferences a freed or
    // moved-from slot.
    unsafe { runtime.register_invocation(&invocation) };

    let eval_result = with_event_loop(invocation.event_loop(), |_| {
        evaluate_main_module(&scope, main, main_name, main_path)
    });

    let evaluation = match eval_result {
        Ok(evaluation) => evaluation,
        Err(message) => {
            runtime.unregister_invocation(&invocation);
            crate::tracing::clear_traced_roots();
            return Err(message);
        }
    };

    runtime.unregister_invocation(&invocation);

    // Keep the realm entered for the process.
    std::mem::forget(scope);

    BOOTSTRAP.with(|cell| {
        cell.set(Bootstrap {
            runtime: std::mem::ManuallyDrop::new(runtime),
            raw_cx,
        })
        .map_err(|_| ())
        .expect("BOOTSTRAP was empty (checked above)");
    });

    Ok(Initialized {
        invocation,
        evaluation,
    })
}

/// What [`initialize_runtime`] hands its caller.
pub struct Initialized {
    /// The invocation whose event loop the main module's top level ran under,
    /// for the caller to drain.
    pub invocation: InvocationState,
    /// The main module's evaluation, which is unfinished while a top-level
    /// `await` is still pending on `invocation`'s event loop.
    pub evaluation: ScriptEvaluation,
}

/// Compile `source` under the name `name`, register it as the module of the
/// file at `path`, if any, then load, link, and evaluate it as the main module,
/// stashing its record so [`with_main_scope`] can derive the namespace from it.
fn evaluate_main_module(
    scope: &Scope<'_>,
    source: &str,
    name: &str,
    path: Option<&str>,
) -> Result<ScriptEvaluation, String> {
    let filename = std::ffi::CString::new(name)
        .map_err(|_| format!("the main module's name `{name}` contains a NUL byte"))?;
    let options = js::compile::options(scope, filename, 1);

    let mut src = transform_str_to_source_text(source);
    // SAFETY: `options` and `src` are valid for the duration of this call.
    let module = unsafe { js::module::compile_module(scope, options.ptr, &mut src) }
        .map_err(|_| capture_message(scope, "main module failed to compile"))?;
    if let Some(path) = path {
        core_runtime::module::register_entry_module(scope, module, std::path::Path::new(path))
            .map_err(|_| capture_message(scope, "registering the main module failed"))?;
    }

    js::module::load_requested_modules(scope, module)
        .map_err(|_| capture_message(scope, "main module failed to load"))?;

    js::module::link(scope, module)
        .map_err(|_| capture_message(scope, "main module failed to link"))?;

    let evaluated = js::module::evaluate(scope, module)
        .map_err(|_| capture_message(scope, "main module failed to evaluate"))?;

    if js::exception::is_pending(scope) {
        return Err(capture_message(
            scope,
            "main module threw during evaluation",
        ));
    }

    // `ModuleEvaluate` reports a top-level throw by rejecting the promise it
    // returns and clearing the pending exception, so the check above does not see
    // it. Drain microtasks first, since they are usually what settles the promise
    // for a top level that only awaits already-resolved values. A module whose
    // top-level `await` is still pending here is finished by the caller's drive of
    // the invocation's event loop.
    core_runtime::event_loop::run_microtasks(scope);
    let pending = core_runtime::module::settled_module_evaluation(
        scope,
        evaluated,
        "main module threw during evaluation",
    )?;

    // The `Heap` is constructed inline as the cell is set, so it is never bound
    // to an unrooted local before `trace_main_module` can see it.
    MAIN_MODULE.with(|cell| *cell.borrow_mut() = Some(Heap::from(module)));
    Ok(ScriptEvaluation::new(pending))
}

/// Run `f` with a freshly rooted scope on the bootstrap's entered realm, skipping
/// the namespace lookup [`with_main_scope`] does.
///
/// The scope is dropped when `f` returns, so values that must outlive it live on
/// a GC-rooted [`CallStack`](crate::stack::CallStack) instead.
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully.
pub fn with_scope<R>(f: impl FnOnce(&Scope<'_>) -> R) -> R {
    // SAFETY: `raw_cx` is the context whose default-global realm was entered and
    // left entered for the process by `initialize_runtime`, and the runtime that
    // owns it is kept alive in `BOOTSTRAP`.
    let scope = unsafe { RootScope::from_current_realm(bootstrap_cx()) };
    f(&scope)
}

/// Run `f` with a freshly rooted scope on the bootstrap's entered realm and the
/// main module's namespace object.
///
/// The namespace is derived from the stored module record per call rather than
/// cached, so a moving GC's updates to the traced record are always reflected.
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully.
pub fn with_main_scope<R>(f: impl FnOnce(&Scope<'_>, &Object<'_>) -> R) -> R {
    // SAFETY: `raw_cx` is the context whose default-global realm was entered and
    // left entered for the process by `initialize_runtime`, and the runtime that
    // owns it is kept alive in `BOOTSTRAP`.
    let scope = unsafe { RootScope::from_current_realm(bootstrap_cx()) };

    let namespace = main_namespace(&scope);
    f(&scope, &namespace)
}

/// The main module's namespace object, derived from the stored module record.
///
/// # Panics
///
/// Panics if the main module has not been evaluated.
fn main_namespace<'s>(scope: &'s Scope<'_>) -> Object<'s> {
    let module = MAIN_MODULE.with(|cell| {
        cell.borrow()
            .as_ref()
            .expect("main module not evaluated")
            .get(scope)
    });
    js::module::get_namespace(scope, module.handle())
        .map(|h| Object::from_handle(h).expect("module namespace is non-null"))
        .expect("module namespace is available after successful evaluation")
}

/// Whether [`initialize_runtime`] has completed successfully. A wizened
/// snapshot was bootstrapped at `init` time, while a script-path run must
/// bootstrap on demand.
pub fn is_initialized() -> bool {
    BOOTSTRAP.with(|cell| cell.get().is_some())
}

/// The bootstrap context whose default-global realm is entered for the process.
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully.
fn bootstrap_cx() -> *mut RawJSContext {
    BOOTSTRAP.with(|cell| {
        cell.get()
            .expect("component-model runtime not initialized")
            .raw_cx
    })
}

/// The raw context whose default-global realm is entered for the process.
///
/// An async export future drives the event loop across `await` points, where it
/// cannot hold a [`Scope`], since GC roots must not span a suspension. It
/// re-roots a fresh scope from this context on each loop step instead.
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully.
pub fn raw_cx() -> *mut RawJSContext {
    bootstrap_cx()
}

/// Clone the process runtime handle, for building an
/// [`OwnedInvocation`](core_runtime::invocation::OwnedInvocation) whose per-call
/// event loop registers itself for GC tracing against this runtime.
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully.
pub fn runtime() -> Rc<Runtime> {
    BOOTSTRAP.with(|cell| {
        Rc::clone(
            &cell
                .get()
                .expect("component-model runtime not initialized")
                .runtime,
        )
    })
}

/// Capture the pending exception, if any, into a `context: error` string whose error
/// includes its source location and stack.
fn capture_message(scope: &Scope<'_>, context: &str) -> String {
    js::error::ExnThrown::capture(scope).with_context(context)
}

/// Drop the stored main-module record. See
/// [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    MAIN_MODULE.with(|cell| *cell.borrow_mut() = None);
}

/// Trace the stored main-module record.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC, and this must
/// run from the crate tracer, with JS execution paused.
pub(crate) unsafe fn trace_main_module(trc: *mut JSTracer) {
    MAIN_MODULE.with(|cell| {
        if let Some(heap) = &*cell.as_ptr() {
            heap.trace(trc);
        }
    });
}
