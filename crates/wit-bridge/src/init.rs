// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Starting the runtime and evaluating the application's main module.
//!
//! [`initialize_runtime`] creates the process's single SpiderMonkey runtime and
//! evaluates the application's main ES module. The realm of the default global
//! stays entered until [`shutdown_runtime`] is called. [`with_scope`] and
//! [`with_main_scope`] create scopes in it.
//!
//! Builtins must be registered with `register_global_initializer` before
//! [`initialize_runtime`] is called.

use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;
use std::rc::Rc;

use core_runtime::config::RuntimeConfig;
use core_runtime::event_loop::with_event_loop;
use core_runtime::invocation::InvocationState;
use core_runtime::runtime::Runtime;
use core_runtime::ScriptEvaluation;
use js::error::ExnThrown;
use js::gc::scope::{EnteredRealm, RootScope, Scope};
use js::native::RawJSContext;
use js::Object;

js::instance_local! {
    /// The bootstrap state, set once by [`initialize_runtime`] and taken by
    /// [`shutdown_runtime`].
    static BOOTSTRAP: RefCell<Option<Bootstrap>> = const { RefCell::new(None) };

    /// Set by the first [`initialize_runtime`] call, before anything can fail.
    /// A failed call leaves `BOOTSTRAP` empty, and a second call would abort in
    /// `JSEngine::init`, which can run only once per process.
    static INITIALIZE_ATTEMPTED: Cell<bool> = const { Cell::new(false) };

    /// Run at the start of every export call, set by [`set_before_export`].
    static BEFORE_EXPORT: Cell<Option<fn()>> = const { Cell::new(None) };
}

/// Run `hook` at the start of every export call, before any JS runs.
pub fn set_before_export(hook: fn()) {
    BEFORE_EXPORT.with(|cell| cell.set(Some(hook)));
}

/// Run the hook set by [`set_before_export`], if any.
#[cfg(target_arch = "wasm32")]
pub(crate) fn before_export() {
    if let Some(hook) = BEFORE_EXPORT.with(Cell::get) {
        hook();
    }
}

/// One-time bootstrap state.
///
/// `scope` and `runtime` are dropped only by [`Bootstrap::shut_down`], in that
/// order. Dropping the runtime while `scope` has its realm entered would call
/// `DestroyContext` with the realm entered, which SpiderMonkey aborts on.
struct Bootstrap {
    /// The scope that entered the default global's realm. Its lifetime is
    /// extended to `'static`. It borrows `runtime`, which is dropped after it.
    scope: ManuallyDrop<RootScope<'static, EnteredRealm>>,
    runtime: ManuallyDrop<Rc<Runtime>>,
    /// The context whose default global's realm `scope` entered.
    raw_cx: *mut RawJSContext,
}

impl Bootstrap {
    /// Leave the default global's realm and drop the runtime.
    ///
    /// # Panics
    ///
    /// Panics if a clone of the runtime returned by [`runtime`] is alive.
    fn shut_down(mut self) {
        // SAFETY: `self` is consumed, so neither field is used after it is
        // dropped here.
        unsafe { ManuallyDrop::drop(&mut self.scope) };
        // SAFETY: as above.
        let runtime = unsafe { ManuallyDrop::take(&mut self.runtime) };
        // The runtime is dropped in its `Rc` allocation, because its GC tracers
        // are registered with that address.
        assert_eq!(
            Rc::strong_count(&runtime),
            1,
            "a clone of the component-model runtime is still alive"
        );
        drop(runtime);
    }
}

/// Initialize the component-model runtime and evaluate the application's main
/// module.
///
/// `config`'s [`base_path`](RuntimeConfig::base_path) is the module loader's
/// base path. `main` is the source of the main module, compiled under the name
/// `main_name`. `main_path` is the path of the file `main` was read from, if
/// any. A module that imports that file gets the main module (see
/// [`core_runtime::module::register_entry_module`]). `synthesize` creates the
/// import modules, and is called before the main module is compiled.
///
/// If the main module has a top-level `await` that running microtasks doesn't
/// settle, the module is still being evaluated when this returns. The caller
/// must then run the event loop of [`Initialized::invocation`] until
/// [`Initialized::evaluation`] finishes, before resolving exports.
///
/// Returns `Err` with an error message if the engine fails to start,
/// `synthesize` throws, or a module fails to compile, load, link or evaluate.
/// Must be called at most once per process. A second call returns an error,
/// even if the first call failed.
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

    // `scope` enters the default global's realm, which stays entered until
    // `shutdown_runtime` drops `scope`.
    let scope = runtime.default_global();
    // SAFETY: the raw context is only stored, and every later read of it goes
    // through `raw_cx`, which is reachable only once `BOOTSTRAP` is set at
    // the end of this function. The runtime that owns the context is kept alive
    // there together with `scope`, so its realm stays entered.
    let raw_cx = unsafe { scope.raw_cx_no_gc() };

    // The tracer goes in before any value can outlive a collection.
    crate::tracing::install_tracer();

    // Every failure below drops `runtime`, so the values held by this crate are
    // dropped first.
    let fail = |scope: &Scope<'_>, context: &str| -> String {
        let message = capture_message(scope, context);
        crate::tracing::clear_traced_roots();
        message
    };

    synthesize(&scope).map_err(|_| fail(&scope, "import-module synthesis failed"))?;

    // The main module is evaluated on the event loop of a new invocation, which
    // the caller can run later. The invocation is registered for tracing during
    // the evaluation, and unregistered before it is moved to the caller, because
    // the registry holds its address.
    let invocation = InvocationState::new();
    // SAFETY: `invocation` lives at this address until it is unregistered on
    // every path out below, so the tracer never dereferences a freed or
    // moved-from slot.
    unsafe { runtime.register_invocation(&invocation) };

    let eval_result = with_event_loop(invocation.event_loop(), |_| {
        evaluate_main_module(&scope, main, main_name, main_path)
    });
    runtime.unregister_invocation(&invocation);
    let evaluation = eval_result.inspect_err(|_| crate::tracing::clear_traced_roots())?;

    // SAFETY: `scope` borrows the runtime in the `Rc` allocation, which
    // `Bootstrap` keeps alive until after it drops `scope`.
    let scope = unsafe {
        std::mem::transmute::<RootScope<'_, EnteredRealm>, RootScope<'static, EnteredRealm>>(scope)
    };
    let bootstrap = Bootstrap {
        scope: ManuallyDrop::new(scope),
        runtime: ManuallyDrop::new(runtime),
        raw_cx,
    };
    BOOTSTRAP.with(|cell| {
        let previous = cell.borrow_mut().replace(bootstrap);
        assert!(previous.is_none(), "BOOTSTRAP was empty (checked above)");
    });

    Ok(Initialized {
        invocation,
        evaluation,
    })
}

/// The result of [`initialize_runtime`].
pub struct Initialized {
    /// The invocation on whose event loop the main module was evaluated.
    pub invocation: InvocationState,
    /// The main module's evaluation, which is unfinished while a top-level
    /// `await` is still pending on `invocation`'s event loop.
    pub evaluation: ScriptEvaluation,
}

/// Evaluate `source`, compiled under the name `name`, as the entry module, and
/// register it as the module of the file at `path`, if any.
fn evaluate_main_module(
    scope: &Scope<'_>,
    source: &str,
    name: &str,
    path: Option<&str>,
) -> Result<ScriptEvaluation, String> {
    if name.contains('\0') {
        return Err(format!(
            "the main module's name `{name}` contains a NUL byte"
        ));
    }
    // SAFETY: `Runtime::init` initialized the module loader.
    let evaluated = unsafe {
        core_runtime::module::evaluate_entry_module(
            scope,
            source,
            name,
            path.map(std::path::Path::new),
        )
    }
    .map_err(|_| capture_message(scope, "main module failed to evaluate"))?;

    if js::exception::is_pending(scope) {
        return Err(capture_message(
            scope,
            "main module threw during evaluation",
        ));
    }

    // `ModuleEvaluate` reports a top-level throw by rejecting the promise it
    // returns and clearing the pending exception, so the check above doesn't see
    // it. Running microtasks settles the promise unless the module awaits
    // something that requires the event loop.
    core_runtime::event_loop::run_microtasks(scope);
    let pending = core_runtime::module::settled_module_evaluation(
        scope,
        evaluated,
        "main module threw during evaluation",
    )?;

    Ok(ScriptEvaluation::new(pending))
}

/// Run `f` with a new scope in the realm of the default global. Values that
/// must outlive the scope can be kept on a
/// [`CallStack`](crate::stack::CallStack).
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully.
pub fn with_scope<R>(f: impl FnOnce(&Scope<'_>) -> R) -> R {
    // SAFETY: `raw_cx` is the context whose default-global realm was entered by
    // `initialize_runtime`. `raw_cx()` panics once `shutdown_runtime` has left
    // the realm, and until then the runtime that owns the context is kept alive
    // in `BOOTSTRAP`.
    let scope = unsafe { RootScope::from_current_realm(raw_cx()) };
    f(&scope)
}

/// Like [`with_scope`], but also pass the main module's namespace object to
/// `f`.
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully.
pub fn with_main_scope<R>(f: impl FnOnce(&Scope<'_>, &Object<'_>) -> R) -> R {
    with_scope(|scope| f(scope, &main_namespace(scope)))
}

/// The main module's namespace object.
///
/// # Panics
///
/// Panics if the main module has not been evaluated.
fn main_namespace<'s>(scope: &'s Scope<'_>) -> Object<'s> {
    core_runtime::module::entry_namespace(scope).expect("main module not evaluated")
}

/// Whether [`initialize_runtime`] has completed successfully and
/// [`shutdown_runtime`] has not been called.
pub fn is_initialized() -> bool {
    BOOTSTRAP.with(|cell| cell.borrow().is_some())
}

/// Drop the JS values held by this crate, leave the default global's realm, and
/// drop the runtime created by [`initialize_runtime`].
///
/// A native program calls this before it exits. While the runtime is alive, the
/// engine isn't shut down at exit, and on Linux SpiderMonkey then crashes in the
/// destructor of its helper-thread lock. No scope created by [`with_scope`] may
/// be alive. Afterwards the functions of this module that require an
/// initialized runtime panic, and [`initialize_runtime`] returns an error.
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully, or if a
/// clone of the runtime returned by [`runtime`] is alive.
pub fn shutdown_runtime() {
    let bootstrap = BOOTSTRAP
        .with(|cell| cell.borrow_mut().take())
        .expect("component-model runtime not initialized");
    crate::tracing::clear_traced_roots();
    bootstrap.shut_down();
}

/// Return the raw context whose default global's realm stays entered. Code
/// that can't hold a [`Scope`] across an `await` uses it to create a new scope
/// after each `await`.
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully.
pub fn raw_cx() -> *mut RawJSContext {
    BOOTSTRAP.with(|cell| {
        cell.borrow()
            .as_ref()
            .expect("component-model runtime not initialized")
            .raw_cx
    })
}

/// Return a reference to the runtime.
///
/// # Panics
///
/// Panics if [`initialize_runtime`] has not completed successfully.
pub fn runtime() -> Rc<Runtime> {
    BOOTSTRAP.with(|cell| {
        Rc::clone(
            &cell
                .borrow()
                .as_ref()
                .expect("component-model runtime not initialized")
                .runtime,
        )
    })
}

/// Return the pending exception, if any, as a `context: error` string, with the
/// error's source location and stack.
fn capture_message(scope: &Scope<'_>, context: &str) -> String {
    ExnThrown::capture(scope).with_context(context)
}
