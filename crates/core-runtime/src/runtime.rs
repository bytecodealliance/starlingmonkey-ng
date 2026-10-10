// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    ffi::c_void,
    ptr::NonNull,
    rc::Rc,
    sync::{Mutex, OnceLock},
};

use crate::{config::RuntimeConfig, event_loop, invocation::InvocationRegistry, module};
use js::{
    engine::{JSEngine, JSEngineHandle, MozJSRuntime, RealmOptions},
    gc::{
        handle::Heap,
        scope::{OnNewGlobalHookOption, Scope},
    },
    heap::Trace,
    native::{JSRuntime, JSTracer, JS_GetRuntime},
    prelude::RootScope,
    Object,
};

// ---------------------------------------------------------------------------
// Global initializer registry
// ---------------------------------------------------------------------------

/// Callback type for installing additional globals on a newly created global object.
///
/// Registered initializers are called during `Runtime::new_global()`.
type GlobalInitFn = for<'a> fn(&'a Scope<'a>, Object<'a>);

js::instance_local! {
    /// The runtime whose SpiderMonkey callbacks are installed on this thread.
    ///
    /// A raw pointer, so this key has no destructor and takes no part in
    /// thread-local teardown. The state it stands in for lives on the `Runtime`
    /// itself, so `Runtime::drop` reaches that state through `&mut self` rather
    /// than through a key that may already be gone.
    ///
    /// Only for the SpiderMonkey callbacks that take no data pointer of their
    /// own. Everything else already has a `Runtime` in hand.
    static CURRENT: Cell<*const Runtime> = const { Cell::new(std::ptr::null()) };
}

/// The runtime this thread's callbacks belong to, or `None` outside
/// `Runtime::init`..`Runtime::drop`.
pub(crate) fn current<'a>() -> Option<&'a Runtime> {
    let rt = CURRENT.get();
    // SAFETY: `init` stores the address of an `Rc` allocation, which is stable
    // for the runtime's life, and `drop` clears it before that allocation goes.
    (!rt.is_null()).then(|| unsafe { &*rt })
}

/// The options of the realm of a new global of the runtime on this thread, as
/// [`Runtime::realm_options`] returns them.
///
/// # Panics
///
/// Panics outside `Runtime::init`..`Runtime::drop`.
pub fn realm_options() -> RealmOptions {
    current()
        .expect("a runtime is running on this thread")
        .realm_options()
}

js::instance_local! {
    static GLOBAL_INITIALIZERS:RefCell<Vec<GlobalInitFn>> = const { RefCell::new(Vec::new()) };
}

/// Register a function to be called whenever a new global object is created.
///
/// This is used by builtins crates (e.g., `web-globals`) to install their
/// functions and constants on every global without creating a dependency
/// from `core-runtime` to the builtins crate.
///
/// Must be called before `Runtime::init()` to take effect on the default
/// global. Registration is idempotent and applies to all future runtimes and global objects.
pub fn register_global_initializer(init: GlobalInitFn) {
    GLOBAL_INITIALIZERS.with(|inits| {
        let mut inits = inits.borrow_mut();
        if !inits.contains(&init) {
            inits.push(init);
        }
    });
}

/// Callback type for re-establishing state that cannot survive a snapshot.
type ResumeFixupFn = fn();

js::instance_local! {
    static RESUME_FIXUPS: RefCell<Vec<ResumeFixupFn>> = const { RefCell::new(Vec::new()) };
}

/// Register a function to run when an instance resumes from a [Wizer](https://github.com/bytecodealliance/wizer)
/// snapshot, to re-establish state the snapshot could not carry.
///
/// Some builtin state is bound to the process that took the snapshot rather than to the one running
/// — a monotonic-clock baseline, a random seed, a cached timezone. The builtin holding it is the
/// only thing that knows, so it registers the repair here instead of the snapshot mechanism
/// enumerating builtins it should not have to know about (which is also what keeps this crate from
/// depending on the builtins crates).
///
/// Registration is idempotent. [`run_resume_fixups`] runs them.
pub fn register_resume_fixup(fixup: ResumeFixupFn) {
    RESUME_FIXUPS.with(|fixups| {
        let mut fixups = fixups.borrow_mut();
        if !fixups.contains(&fixup) {
            fixups.push(fixup);
        }
    });
}

/// Run every fixup registered with [`register_resume_fixup`]. Called once by a resumed instance,
/// before it does any work that would observe the state being repaired.
pub fn run_resume_fixups() {
    let fixups = RESUME_FIXUPS.with(|fixups| fixups.borrow().clone());
    for fixup in fixups {
        fixup();
    }
}

/// Clear all registered global initializers (used between tests that need
/// disjoint initializer sets).
pub fn clear_global_initializers() {
    GLOBAL_INITIALIZERS.with(|inits| inits.borrow_mut().clear());
}

// ---------------------------------------------------------------------------
// Before-content and shutdown hooks
// ---------------------------------------------------------------------------

/// Callback type for work that runs after the runtime is initialized and before the content
/// script is evaluated.
type BeforeContentFn = fn(&Runtime, &RuntimeConfig) -> Result<(), String>;

js::instance_local! {
    static BEFORE_CONTENT_HOOKS: RefCell<Vec<BeforeContentFn>> = const { RefCell::new(Vec::new()) };
}

/// Register a function to run after [`Runtime::init`] and before the content script is read and
/// evaluated. An `Err` aborts the run with that message, and the content script does not run.
///
/// Registration is idempotent.
pub fn register_before_content_hook(hook: BeforeContentFn) {
    BEFORE_CONTENT_HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        if !hooks.contains(&hook) {
            hooks.push(hook);
        }
    });
}

/// Run every hook registered with [`register_before_content_hook`], in registration order,
/// stopping at the first `Err`.
pub fn run_before_content_hooks(runtime: &Runtime, config: &RuntimeConfig) -> Result<(), String> {
    let hooks = BEFORE_CONTENT_HOOKS.with(|hooks| hooks.borrow().clone());
    hooks.into_iter().try_for_each(|hook| hook(runtime, config))
}

/// Callback type for work that runs when a runtime is dropped, while its context is alive.
type ShutdownFn = fn(&Runtime);

js::instance_local! {
    static SHUTDOWN_HOOKS: RefCell<Vec<ShutdownFn>> = const { RefCell::new(Vec::new()) };
}

/// Register a function to run first in `Runtime`'s `Drop`, while the context and the default
/// global are alive. Use it to release GC things held outside the runtime.
///
/// Registration is idempotent.
pub fn register_shutdown_hook(hook: ShutdownFn) {
    SHUTDOWN_HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        if !hooks.contains(&hook) {
            hooks.push(hook);
        }
    });
}

/// Clear all registered before-content and shutdown hooks (used between tests that need
/// disjoint hook sets).
pub fn clear_runtime_hooks() {
    BEFORE_CONTENT_HOOKS.with(|hooks| hooks.borrow_mut().clear());
    SHUTDOWN_HOOKS.with(|hooks| hooks.borrow_mut().clear());
}

// ---------------------------------------------------------------------------
// Engine singleton
// ---------------------------------------------------------------------------

/// Wrapper to allow `JSEngine` inside a `Mutex` in a `static`.
///
/// `JSEngine` is `!Send + !Sync` (via `PhantomData<*mut ()>`), but its
/// actual state is just an `Arc<AtomicU32>` handle refcount. Thread safety
/// for JS *execution* is enforced at the `Runtime` level, not the engine
/// level. `JS_Init` / `JS_ShutDown` are process-global operations.
struct EngineState(Option<JSEngine>);

// SAFETY: See above — the `!Send` bound on `JSEngine` is conservative.
// We only call `JS_Init` once and `JS_ShutDown` once (at exit), and
// `JSEngineHandle` (the thing handed out to runtimes) is already
// `Send + Sync`.
unsafe impl Send for EngineState {}

/// Process-global engine singleton.
///
/// Uses `OnceLock` for thread-safe one-time initialization and `Mutex`
/// for interior mutability so the `atexit` handler can take and drop the
/// engine to call `JS_ShutDown()` cleanly.
static ENGINE: OnceLock<Mutex<EngineState>> = OnceLock::new();

unsafe extern "C" {
    fn atexit(func: unsafe extern "C" fn()) -> std::os::raw::c_int;
}

/// `atexit` callback: takes the `JSEngine` out of the global and drops it,
/// which calls `JS_ShutDown()` and prevents SpiderMonkey's C++ static
/// destructors from crashing on process exit.
unsafe extern "C" fn shutdown_engine() {
    if let Some(mutex) = ENGINE.get() {
        // If the lock is poisoned we're already in a bad state; just skip.
        if let Ok(mut guard) = mutex.lock() {
            // A `Runtime` can still be alive here, e.g because `process::exit` was called from an
            // error path. Its `MozJSRuntime` holds an engine handle, and `JSEngine::drop` asserts
            // the handle count is zero, so dropping anyway would trap.
            // To avoid that, we leave the
            // engine in the static instead and skip `JS_ShutDown`:
            // We're about to exit anyway, so process cleanup will handle it.
            if guard.0.as_ref().is_some_and(|engine| engine.can_shutdown()) {
                drop(guard.0.take());
            }
        }
    }
}

/// Get a `JSEngineHandle` for creating new `MozJSRuntime`s.
///
/// Initializes the engine on first call (once per process) and registers
/// an `atexit` handler so `JS_ShutDown()` runs at process exit.
/// `JSEngineHandle` is `Send + Sync`, so it can be used from any thread.
fn engine_handle() -> JSEngineHandle {
    let mutex = ENGINE.get_or_init(|| {
        let engine = JSEngine::init().expect("failed to init JS engine");
        // SAFETY: `shutdown_engine` is a valid function pointer.
        unsafe {
            atexit(shutdown_engine);
        }
        Mutex::new(EngineState(Some(engine)))
    });
    mutex
        .lock()
        .unwrap()
        .0
        .as_ref()
        .expect("JS engine has been shut down")
        .handle()
}

/// The StarlingMonkey runtime wrapper around SpiderMonkey.
///
/// Each `Runtime` owns a SpiderMonkey `JSContext` (via `MozJSRuntime`), a
/// default global object, and the associated module loader state. Multiple
/// `Runtime` instances can be created consecutively on the same thread
/// (e.g. in tests); each cleans up its state on drop.
///
/// Uses `UnsafeCell` for interior mutability because SpiderMonkey contexts
/// are inherently single-threaded, and the scope-based API requires `&mut`
/// access to the context even though we store the runtime behind `Rc`.
///
/// A `Runtime` instance always roots itself and traces its members.
#[js::allow_unrooted_interior]
pub struct Runtime {
    /// Default global, declared before `mozjs_rt` so it drops first.
    /// `Heap::drop()` fires a GC write barrier, which requires the
    /// SpiderMonkey context to still be alive.
    default_global: Heap<js::object::Object>,
    /// Cached module objects, the resolver and the base path. Declared before
    /// `mozjs_rt` for the same reason as `default_global`.
    pub(crate) modules: module::ModuleState,
    /// `FinalizationRegistry` cleanups still queued. Same ordering requirement.
    pub(crate) finalization: crate::finalization::FinalizationState,
    mozjs_rt: UnsafeCell<MozJSRuntime>,
    /// Registry of live [`InvocationState`](crate::invocation::InvocationState)
    /// instances. The GC trace callback iterates this to trace all event
    /// loops across concurrent invocations.
    invocations: RefCell<InvocationRegistry>,
    /// Whether globals define `SharedArrayBuffer` and `Atomics`, from
    /// [`RuntimeConfig::shared_memory_enabled`].
    shared_memory: bool,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").finish()
    }
}

impl Runtime {
    /// Get a mutable reference to the inner MozJS runtime.
    ///
    /// # Safety
    ///
    /// SpiderMonkey is single-threaded; this is sound as long as no two
    /// `&mut MozJSRuntime` references exist simultaneously (enforced by
    /// the single-threaded usage pattern).
    #[allow(clippy::mut_from_ref)]
    fn mozjs_rt_mut(&self) -> &mut MozJSRuntime {
        unsafe { &mut *self.mozjs_rt.get() }
    }

    /// Initialize a new runtime and return a reference-counted handle to it.
    ///
    /// The runtime owns the SpiderMonkey context and all global objects created
    /// in that context. The caller is responsible for keeping the `Rc<Runtime>`
    /// alive for as long as the runtime is needed.
    ///
    /// Returns `Err` with a description if the configured initializer script can't
    /// be read, throws, or leaves asynchronous work behind.
    pub fn init(config: &RuntimeConfig) -> Result<Rc<Self>, String> {
        crate::config::set_web_security_restrictions(config.web_security_restrictions());
        event_loop::set_blocking_host_io_while_serving(config.allow_blocking_host_io);
        // SAFETY: no parent runtime is given, which is the only condition.
        let mut mozjs_rt =
            unsafe { MozJSRuntime::create_with_internal_job_queues(engine_handle(), None) };
        js::gc::init(mozjs_rt.cx());
        js::watchdog::install(mozjs_rt.cx());
        js::realm::install(mozjs_rt.cx());
        js::context::set_async_stack_capture(mozjs_rt.cx(), !config.async_stacks);
        #[cfg(target_arch = "wasm32")]
        if let Some(limit) = config.recursion_limit {
            js::context::set_wasi_recursion_depth_limit(mozjs_rt.cx(), limit);
        }

        let rt = Rc::new(Self {
            mozjs_rt: UnsafeCell::new(mozjs_rt),
            default_global: Heap::default(),
            modules: module::ModuleState::default(),
            finalization: crate::finalization::FinalizationState::default(),
            invocations: RefCell::new(InvocationRegistry::new()),
            shared_memory: config.shared_memory_enabled(),
        });

        // Set before anything can call back into us. Cleared by the `Drop` impl.
        CURRENT.set(Rc::as_ptr(&rt));

        // Register runtime GC tracer, passing a raw pointer to the Rc's
        // inner allocation so the callback can trace `default_global`
        // without a thread-local lookup.
        //
        // SAFETY: Rc heap-allocates the `Runtime`, so its address is stable.
        // We remove this tracer in `Drop`, guaranteeing the pointer remains
        // valid for the tracer's entire lifetime.
        let self_ptr = Rc::as_ptr(&rt) as *mut c_void;
        unsafe {
            js::gc::add_extra_gc_roots_tracer(
                rt.mozjs_rt_mut().cx(),
                Some(trace_runtime_cb),
                self_ptr,
            );
        }

        // Register GC tracer for the module registry so cached module
        // objects are properly traced.
        // SAFETY: as for the runtime tracer above, `Drop` removes it.
        unsafe { module::init_module_gc_tracer(rt.mozjs_rt_mut().cx(), self_ptr as *const Self) };

        // Ensure `FinalizationRegistry` callbacks get run.
        crate::finalization::install(rt.mozjs_rt_mut().cx());

        rt.modules.node_compat.set(config.node_compat);
        if rt.shared_memory {
            js::context::set_futex_can_wait(rt.mozjs_rt_mut().cx());
        }
        module::init_module_loader(&rt, config.base_path());

        // Create the default global and register builtins.
        drop(rt.new_global());
        js::jobs::track_unhandled_rejections(&rt.default_global())
            .map_err(|_| "Failed to set up promise rejection tracking".to_string())?;
        if config.pre_initialize {
            let scope = rt.default_global();
            // SAFETY: the context lives as long as `rt`, whose `Drop` stops the recording.
            unsafe { js::stack::record_origins(Some(scope.raw_cx_no_gc())) };
        }
        rt.run_initializer_script(config)?;

        Ok(rt)
    }

    /// Create a new global object (and realm), install all registered global initializers on it,
    /// fire the `onNewGlobalObject` hook of every `Debugger` for it, and return a scope entered
    /// into it.
    ///
    /// The first global created becomes the runtime's default global (see
    /// [`default_global`](Self::default_global)), later ones are additional realms and leave the
    /// default untouched.
    pub fn new_global(&self) -> RootScope<'_, js::gc::scope::EnteredRealm> {
        let cx = self.mozjs_rt_mut().cx();
        let options = self.realm_options();
        let scope = RootScope::new_global(
            cx,
            &js::class::STARLING_GLOBAL_CLASS,
            OnNewGlobalHookOption::DontFireOnNewGlobalHook,
            options,
        );

        if !self.default_global.is_initialized() {
            self.default_global.set(scope.global());
        }

        event_loop::timer::install_timer_globals(&scope, scope.global());

        // Call any registered global initializers (e.g., web-globals, WPT builtins).
        // Snapshot the list first: an initializer (or JS it runs) may itself
        // register further initializers, and calling out under the borrow
        // would panic. Initializers registered mid-call apply to subsequent
        // globals only.
        let inits = GLOBAL_INITIALIZERS.with(|inits| inits.borrow().clone());
        for init in inits {
            init(&scope, scope.global());
        }
        js::class::fire_on_new_global_object(&scope, scope.global().handle());

        scope
    }

    /// The options of the realm of a new global: its private data is traced, and it defines
    /// `SharedArrayBuffer` and `Atomics` if [`RuntimeConfig::shared_memory_enabled`] is set.
    pub fn realm_options(&self) -> RealmOptions {
        // Trace any private data a global initializer stores on the
        // global via `set_global_private_and_proto`.
        let mut options = RealmOptions::default();
        js::class::install_global_private_trace(&mut options);
        if self.shared_memory {
            options.creationOptions_.sharedMemoryAndAtomics_ = true;
            options.creationOptions_.defineSharedArrayBufferConstructor_ = true;
        }
        options
    }

    /// Enter the default global realm and return a rooting scope for it.
    pub fn default_global(&self) -> RootScope<'_, js::gc::scope::EnteredRealm> {
        // SAFETY: Raw JSObject pointer is used to create a rooting scope.
        let global = unsafe {
            NonNull::new(self.default_global.as_ptr()).expect("default global should be set")
        };
        RootScope::new_with_realm(self.mozjs_rt_mut().cx(), global)
    }

    fn run_initializer_script(&self, config: &RuntimeConfig) -> Result<(), String> {
        // Run initializer script if provided (always as legacy script).
        let Some(ref init_path) = config.initializer_script_path else {
            return Ok(());
        };
        let scope = self.default_global();
        let init_source = std::fs::read_to_string(init_path)
            .map_err(|e| format!("Error reading initializer script '{init_path}': {e}"))?;
        let filename = init_path.as_str();

        // The initializer runs with its own event loop active, so scheduling APIs such as
        // `setTimeout` work during initialization. Its microtasks drain before the content script
        // runs.
        let invocation = crate::invocation::InvocationState::new();
        // SAFETY: `invocation` stays at this address until the guard drops at
        // the end of this function.
        let _guard = unsafe { crate::invocation::InvocationGuard::new(self, &invocation) };
        let failed = event_loop::with_event_loop(invocation.event_loop(), |_| {
            let failed =
                js::compile::evaluate_with_filename(&scope, &init_source, filename, 1).is_err();
            if !failed {
                event_loop::run_microtasks(&scope);
            }
            failed
        });
        if failed {
            return Err(format!(
                "Error evaluating initializer script '{init_path}': {}",
                js::error::ExnThrown::capture(&scope)
            ));
        }

        // Nothing drives this loop after initialization: leftover async work
        // (a live timer, an in-flight promise-backed operation) would be
        // silently dropped, so make it a hard error instead.
        if invocation.event_loop().is_alive() {
            return Err(format!(
                "Error: initializer script '{init_path}' left asynchronous work \
                 (timers or pending operations) behind. Initializer scripts must \
                 complete synchronously"
            ));
        }
        Ok(())
    }

    /// Returns the `JSRuntime` object.
    pub fn rt(&self) -> *mut JSRuntime {
        // SAFETY: cx_no_gc only needs shared access.
        let rt = unsafe { &*self.mozjs_rt.get() };
        unsafe { JS_GetRuntime(rt.cx_no_gc()) }
    }

    /// Create a `Scope` for the current realm.
    ///
    /// # Safety
    ///
    /// A realm must already be entered on this runtime's context.
    pub unsafe fn scope(&self) -> RootScope<'_, js::gc::scope::EnteredRealm> {
        RootScope::from_current_realm(self.mozjs_rt_mut().cx().raw_cx())
    }

    /// Returns the underlying mozjs `Runtime`.
    pub fn mozjs_rt(&self) -> &MozJSRuntime {
        // SAFETY: shared access is fine for reading.
        unsafe { &*self.mozjs_rt.get() }
    }

    /// Returns a reference to the invocation registry.
    ///
    /// The registry is behind a `RefCell` because the `Runtime` is
    /// stored behind `Rc` while the registry needs `&mut` access for
    /// registration/unregistration.
    pub fn invocations(&self) -> &RefCell<InvocationRegistry> {
        &self.invocations
    }

    /// Register an invocation for GC tracing.
    ///
    /// # Safety
    ///
    /// The caller must ensure `state` remains valid and at a stable address
    /// until [`unregister_invocation`](Self::unregister_invocation) is called.
    pub unsafe fn register_invocation(&self, state: &crate::invocation::InvocationState) {
        self.invocations.borrow_mut().register(state as *const _);
    }

    /// Unregister a previously registered invocation.
    pub fn unregister_invocation(&self, state: &crate::invocation::InvocationState) {
        self.invocations.borrow_mut().unregister(state as *const _);
    }

    /// Re-initialize the module loader.
    ///
    /// Clears any existing module state (registry, cached modules, resolver)
    /// and sets up a fresh module load hook rooted at `base_path`.
    /// Useful in tests to point imports at a temp directory.
    pub fn reset_module_loader(&self, base_path: std::path::PathBuf) {
        module::clear_module_state(&self.modules);
        module::init_module_loader(self, base_path);
    }

    /// Drop the cached module objects, so the next `import` of a path compiles
    /// and evaluates it afresh. The resolver and base path stay in place.
    ///
    /// A caller serving each request from a new global has to clear this between
    /// requests: the registry is keyed by path and holds module objects belonging
    /// to the global they were evaluated in, so otherwise the next request links
    /// against the previous request's already-evaluated modules and their
    /// top-level side effects never run again.
    pub fn clear_module_registry(&self) {
        module::clear_module_registry(&self.modules);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let hooks = SHUTDOWN_HOOKS.with(|hooks| hooks.borrow().clone());
        for hook in hooks {
            hook(self);
        }
        // SAFETY: stopping the recording has no precondition.
        unsafe { js::stack::record_origins(None) };
        // The tracked promises, held jobs and async contexts are rooted, so they go while the
        // context is alive.
        js::jobs::clear_unhandled_rejections();
        js::jobs::clear_held_jobs();
        js::async_context::clear();
        // Futures left by loops that were never canceled, and those spawned with no loop active,
        // have to go while the context is alive.
        js::promise::cancel_all_pending_futures();
        // Clear module state while tracers are still registered.
        // Heap::drop fires GC write barriers which can trigger GC under
        // GC zeal — the module tracer must still be registered.
        module::clear_module_state(&self.modules);
        // Same reason: the queued `doCleanup` functions belong to this runtime,
        // and their tracer is still registered.
        crate::finalization::clear(&self.finalization);

        // Remove GC tracers (module registry is empty, so the tracer
        // is a no-op even if called during barrier processing).
        // The class registry is owned by the global object and cleaned
        // up by its finalize hook — no explicit clearing needed.
        let self_ptr = self as *const Self as *mut c_void;
        // SAFETY: `init` registered both tracers with this callback and this address, which is
        // the `Rc` allocation `self` lives in.
        unsafe {
            js::gc::remove_extra_gc_roots_tracer(
                self.mozjs_rt().cx_no_gc(),
                Some(trace_runtime_cb),
                self_ptr,
            );
            module::remove_module_gc_tracer(self.mozjs_rt().cx_no_gc(), self_ptr as *const Self);
        }
        // Only if this is still the current runtime: a second one created on
        // this thread has already replaced the pointer with its own.
        if std::ptr::eq(CURRENT.get(), self) {
            CURRENT.set(std::ptr::null());
        }
        js::gc::shutdown(self.mozjs_rt().cx_no_gc());
    }
}

/// GC trace callback for this `Runtime`'s `default_global` Heap and the
/// event loops of all registered invocations.
///
/// `data` is a raw pointer to the `Runtime` (passed via
/// `add_extra_gc_roots_tracer`).
///
/// # Safety
///
/// - `trc` must be a valid `JSTracer` pointer provided by SpiderMonkey's GC.
/// - `data` must point to a live `Runtime` instance.
#[js::allow_unrooted]
unsafe extern "C" fn trace_runtime_cb(trc: *mut JSTracer, data: *mut c_void) {
    // SAFETY: the caller guarantees `data` points to a live `Runtime`.
    let rt = unsafe { &*(data as *const Runtime) };
    rt.default_global.trace(trc);
    // Trace all event loops across registered invocations.
    //
    // Use `as_ptr()` to bypass `RefCell` borrow tracking. GC tracing runs
    // with JS execution paused (stop-the-world), so the `&mut` references
    // on the caller's stack aren't being actively used — no aliasing hazard.
    // A normal `borrow()` would panic when the invocation registry is
    // already borrowed mutably.
    // SAFETY: as described above, nothing uses a borrow of the registry while the GC traces.
    let invocations = unsafe { &*rt.invocations.as_ptr() };
    invocations.trace(trc);
}
