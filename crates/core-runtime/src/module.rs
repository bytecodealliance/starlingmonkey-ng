// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Native ES module support for SpiderMonkey.
//!
//! This module provides an ergonomic way to define ES modules backed by native
//! Rust functions and values, inspired by rquickjs's `ModuleDef` pattern but
//! built on SpiderMonkey's module compilation pipeline.
//!
//! # Strategy
//!
//! SpiderMonkey has no "synthetic module" API — every module must start as JS
//! source text that is compiled with `CompileModule`. The approach here is:
//!
//! 1. Generate JS source with `export var name;` declarations
//! 2. `CompileModule` → `ModuleLink` → `ModuleEvaluate`
//! 3. Retrieve the module environment via `GetModuleEnvironment`
//! 4. Populate it with native values/functions using `JS_SetProperty` /
//!    `JS_DefineFunction`
//!
//! A module load hook maps specifier strings to compiled module objects
//! via the runtime's module registry.
//!
//! # Example
//!
//! ```rust,ignore
//! #[::core_runtime::jsmodule]
//! mod my_math {
//!     pub const PI: f64 = 3.14159;
//!     pub fn add(a: f64, b: f64) -> f64 { a + b }
//! }
//!
//! // Register and use:
//! let rt = Runtime::init(&config).expect("runtime init");
//! let scope = rt.default_global();
//! register_module::<my_math::js_module>(&scope);
//! // JS can now: import { PI, add } from "myMath";
//! ```
//!
//! The specifier is the `mod` name camelCased, unless overridden with
//! `#[jsmodule(name = "...")]`.

use js::conversion::{jsstr_to_string, ToJSVal};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::Trace;
use js::module::ModuleType;
use js::module_raw::{transform_str_to_source_text, SetModulePrivate};
use js::native::{GCHandle, HandleObject, JSNative, JSObject, JSString, JSTracer, Value};
use js::prelude::{HandleValue, RootScope};
use js::{allow_unrooted, Object};
use oxc_resolver::{ResolveOptions, Resolver};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

// ============================================================================
// Module export descriptors
// ============================================================================

/// Describes a single export from a native module.
pub enum ModuleExport {
    /// A native function export.
    Function {
        /// Name as it appears in JS (the export name).
        js_name: &'static str,
        /// The JSNative callback implementing the function.
        native: JSNative,
        /// Number of expected arguments.
        nargs: u32,
    },
    /// A value export (constant). The value is set by the `evaluate` callback.
    Value {
        /// Name as it appears in JS (the export name).
        js_name: &'static str,
    },
}

// ============================================================================
// NativeModule trait
// ============================================================================

/// Trait for types that define a native ES module.
///
/// Implement this trait (usually via `#[jsmodule]`) to expose Rust
/// functions and constants as ES module exports.
pub trait NativeModule: 'static {
    /// The module specifier string used in JS `import` statements.
    /// e.g. `"myMath"` for `import { add } from "myMath";`
    const NAME: &'static str;

    /// Return the list of exports this module provides.
    fn declarations() -> Vec<ModuleExport>;

    /// Populate the module environment with native values.
    ///
    /// Called after `ModuleEvaluate` — the module environment object is
    /// passed in so you can set property values for `Value` exports.
    /// Function exports are set up automatically before this is called.
    ///
    /// # Safety
    ///
    /// `scope` must be valid. `env` is the module environment object.
    unsafe fn evaluate(scope: &Scope<'_>, env: HandleObject) -> bool;
}

// ============================================================================
// Module registry
// ============================================================================

/// A cached compiled module object, stored in a `Heap` so SpiderMonkey's
/// moving GC can update the pointer during compaction.
/// Traced by `trace_module_registry`, so allowed to contain unrooted interior.
#[js::must_root]
struct ModuleEntry {
    module_obj: Heap<js::object::Object>,
}

/// A runtime's module state, held as a field of [`Runtime`](crate::Runtime).
///
/// `Runtime` declares this before its `mozjs_rt`, so the cached module objects
/// drop while the SpiderMonkey context is still alive and their write barriers
/// can fire.
#[js::must_root]
#[derive(Default)]
pub struct ModuleState {
    registry: RefCell<HashMap<String, ModuleEntry>>,
    /// The resolver instance, created once per runtime via `init_module_loader`.
    resolver: RefCell<Option<Resolver>>,
    /// Fallback base directory for the entry module (before any module objects exist).
    base_path: RefCell<Option<PathBuf>>,
    /// The registry key of the entry module recorded last (see [`entry_namespace`]).
    entry: RefCell<Option<String>>,
}

/// Run `f` against the current runtime's module state.
///
/// The SpiderMonkey callbacks below take no data pointer of their own, so they
/// reach the runtime through [`crate::runtime::current`].
fn with_state<R>(f: impl FnOnce(&ModuleState) -> R) -> R {
    let rt = crate::runtime::current().expect("module state used outside a runtime");
    f(&rt.modules)
}

fn registry<R>(f: impl FnOnce(&RefCell<HashMap<String, ModuleEntry>>) -> R) -> R {
    with_state(|state| f(&state.registry))
}

fn resolver<R>(f: impl FnOnce(&RefCell<Option<Resolver>>) -> R) -> R {
    with_state(|state| f(&state.resolver))
}

fn base_path<R>(f: impl FnOnce(&RefCell<Option<PathBuf>>) -> R) -> R {
    with_state(|state| f(&state.base_path))
}

/// The module the registry holds under `key`.
fn cached_module<'s>(scope: &'s Scope<'_>, key: &str) -> Option<Object<'s>> {
    registry(|reg| {
        reg.borrow()
            .get(key)
            .map(|entry| entry.module_obj.get(scope))
    })
}

/// The module the registry holds under a versioned WIT interface name compatible
/// with `specifier`'s (see [`wit_name_mapping::same_interface`]), at the highest
/// version if it holds several. `None` if `specifier` has no version.
fn compatible_cached_module<'s>(scope: &'s Scope<'_>, specifier: &str) -> Option<Object<'s>> {
    if !specifier.contains('@') {
        return None;
    }
    registry(|reg| {
        let reg = reg.borrow();
        reg.iter()
            .filter(|(key, _)| wit_name_mapping::same_interface(key, specifier))
            .max_by_key(|(key, _)| wit_name_mapping::interface_version(key))
            .map(|(_, entry)| entry.module_obj.get(scope))
    })
}

/// Hold `module` in the registry under `key`.
fn cache_module(key: String, module: Object<'_>) {
    registry(|reg| {
        reg.borrow_mut().insert(
            key,
            ModuleEntry {
                module_obj: Heap::from(module),
            },
        );
    });
}

/// Trace all cached module objects.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` pointer provided by SpiderMonkey's GC.
#[allow_unrooted]
unsafe fn trace_module_registry(state: &ModuleState, trc: *mut JSTracer) {
    for entry in state.registry.borrow().values() {
        entry.module_obj.trace(trc);
    }
}

/// C-compatible trampoline for [`trace_module_registry`].
///
/// # Safety
///
/// `data` must be the `Runtime` address [`init_module_gc_tracer`] was given.
unsafe extern "C" fn trace_module_registry_cb(trc: *mut JSTracer, data: *mut c_void) {
    // SAFETY: the caller passes the `Runtime` address the tracer was registered with, which
    // `init_module_gc_tracer`'s caller keeps valid while the tracer is registered.
    let rt = unsafe { &*(data as *const crate::Runtime) };
    // SAFETY: SpiderMonkey calls a roots tracer with a valid tracer.
    unsafe { trace_module_registry(&rt.modules, trc) };
}

/// Register the module registry as a GC root tracer.
///
/// Called automatically by `Runtime::init` — only needed when using a
/// raw mozjs `Runtime` directly.
/// `rt` is handed back to the tracer, which reads the registry out of it.
///
/// # Safety
///
/// `rt` must stay valid until [`remove_module_gc_tracer`] removes the tracer.
pub unsafe fn init_module_gc_tracer(cx: &mut js::native::JSContext, rt: *const crate::Runtime) {
    // SAFETY: the callback is a valid function, and the caller keeps `rt` valid for as long as
    // the tracer is registered.
    unsafe {
        js::gc::add_extra_gc_roots_tracer(cx, Some(trace_module_registry_cb), rt as *mut c_void)
    };
}

/// Remove the module registry GC root tracer.
///
/// # Safety
///
/// `rt` must be the address passed to [`init_module_gc_tracer`].
pub unsafe fn remove_module_gc_tracer(cx: &js::native::JSContext, rt: *const crate::Runtime) {
    // SAFETY: the callback is the registered one, and the caller passes the registered `rt`.
    unsafe {
        js::gc::remove_extra_gc_roots_tracer(cx, Some(trace_module_registry_cb), rt as *mut c_void)
    };
}

/// Classify a module's evaluation result once microtasks have drained.
///
/// `JS::ModuleEvaluate` reports a top-level throw by rejecting the promise it
/// returns and clearing the pending exception, so a caller that only checks for
/// a pending exception reports success for a module that threw.
///
/// Returns the promise while a top-level `await` leaves it pending, `None` once
/// it has fulfilled, and `Err` with `context: reason` once it has rejected. A
/// classic script's completion value is not a promise and gives `None`.
// TODO: this is kinda messy and hard to understand and we should clean it up.
pub fn settled_module_evaluation<'s>(
    scope: &'s Scope<'_>,
    eval_result: HandleValue<'s>,
    context: &str,
) -> Result<Option<js::Promise<'s>>, String> {
    let Some(promise) = js::Object::from_value(scope, eval_result)
        .ok()
        .and_then(|object| object.cast::<js::Promise>().ok())
    else {
        return Ok(None);
    };

    if promise.is_pending() {
        return Ok(Some(promise));
    }
    if !promise.is_rejected() {
        return Ok(None);
    }
    Err(rejection_message(scope, &promise, context))
}

/// `context: reason` for the rejected module evaluation `promise`.
pub(crate) fn rejection_message(
    scope: &Scope<'_>,
    promise: &js::Promise<'_>,
    context: &str,
) -> String {
    // Re-raise the reason so the shared capture path formats it the same way a
    // thrown exception is formatted, then clear it again.
    let Some(reason) = promise.result(scope) else {
        return context.to_string();
    };
    js::exception::set_pending(scope, reason, js::native::ExceptionStackBehavior::Capture);
    ExnThrown::capture(scope).with_context(context)
}

// ============================================================================
// Module load hook
// ============================================================================

/// The module load hook called by SpiderMonkey once per `import` while loading
/// a module graph.
///
/// Resolution strategy:
/// 1. For a JavaScript request, check the module registry for an exact match:
///    native modules by name, already-compiled file modules by canonical path.
/// 2. Resolve via `oxc_resolver` against the referencing module's directory
///    (file-backed modules carry their canonical path in their module
///    private), or against the loader's base path for pathless referrers
///    (eval scripts, native modules).
///
/// A request with `with { type: "json" }` compiles the file as a JSON module.
/// Every other module type is refused.
///
/// For a dynamic `import()`, the hook also loads the module's own imports
/// before handing it over, and the engine links and evaluates it in a promise
/// job.
///
/// Every call ends either by handing the module to
/// `finish_loading_imported_module` or by returning `false`, which leaves the
/// engine to fail the load with the exception this reported.
unsafe extern "C" fn module_load_hook(
    cx: *mut js::native::RawJSContext,
    referrer: js::native::RawHandle<*mut js::native::JSScript>,
    module_request: js::native::RawHandle<*mut JSObject>,
    _host_defined: js::native::RawHandle<Value>,
    payload: js::native::RawHandle<Value>,
    _line_number: u32,
    _column_number: js::native::ColumnNumberOneOrigin,
) -> bool {
    // SAFETY: cx is a valid RawJSContext from the load hook, which SpiderMonkey
    // calls with the referrer's realm entered.
    let scope = unsafe { RootScope::from_current_realm(cx) };

    // Extract the specifier string from the ModuleRequest object
    // SAFETY: SpiderMonkey passes the hook a rooted `module_request` handle.
    let request = unsafe { GCHandle::from_raw(module_request) };
    let Some(specifier_str) = js::module::get_module_request_specifier(&scope, request) else {
        return false;
    };
    let specifier = jsstr_to_string(&scope, specifier_str);
    let kind = match js::module::get_module_request_type(&scope, request) {
        ModuleType::JavaScript => ModuleKind::JavaScript,
        ModuleType::JSON => ModuleKind::Json,
        _ => {
            let message = format!(
                "Cannot import '{specifier}': only JavaScript and JSON modules are supported"
            );
            let c_msg = CString::new(message).unwrap_or_else(|_| c"Unsupported module type".into());
            js::error::report_error_ascii(&scope, &c_msg);
            return false;
        }
    };

    // 1. Check the module registry for an exact match, then for a compatible
    // version of a versioned WIT interface name. The `json:`-prefixed
    // keys hold JSON modules, which a JavaScript import never gets.
    let cached = match kind {
        ModuleKind::JavaScript if !specifier.starts_with("json:") => {
            cached_module(&scope, &specifier)
                .or_else(|| compatible_cached_module(&scope, &specifier))
        }
        ModuleKind::JavaScript | ModuleKind::Json => None,
    };

    // 2. Resolve via filesystem using oxc_resolver, relative to the referrer.
    let module = match cached {
        Some(obj) => obj,
        None => {
            let base_dir = referrer_base_dir(&scope, referrer);
            match resolve_file_module(&scope, &specifier, base_dir, kind) {
                Ok(obj) => obj,
                Err(ModuleLoadError::Failed(msg)) => {
                    let c_msg =
                        CString::new(msg).unwrap_or_else(|_| c"Module resolution failed".into());
                    js::error::report_error_ascii(&scope, &c_msg);
                    return false;
                }
                // The compile error, with its location, is already pending.
                Err(ModuleLoadError::CompileThrew) => return false,
            }
        }
    };

    // SAFETY: SpiderMonkey passes the hook a rooted `payload` handle.
    let payload_handle = unsafe { GCHandle::from_raw(payload) };
    if js::module::is_dynamic_import_payload(&scope, payload_handle)
        && js::module::load_requested_modules(&scope, module).is_err()
    {
        return false;
    }

    // SAFETY: this is the load hook, passing its own arguments, and this is its only call.
    unsafe {
        js::module::finish_loading_imported_module(
            &scope,
            GCHandle::from_raw(referrer),
            GCHandle::from_raw(module_request),
            GCHandle::from_raw(payload),
            module.handle(),
        )
    }
    .is_ok()
}

/// The directory to resolve a relative specifier against: the parent of the
/// referencing module's path if the referrer carries one in its module
/// private, the loader's base path otherwise (eval-script entries and native
/// modules have no path).
fn referrer_base_dir(
    scope: &Scope<'_>,
    referrer: js::native::RawHandle<*mut js::native::JSScript>,
) -> Option<PathBuf> {
    if !referrer.get().is_null() {
        // SAFETY: `referrer` is the load hook's rooted `referrer` argument.
        let private =
            js::module::get_script_private(scope, unsafe { GCHandle::from_raw(referrer) });
        if private.is_string() {
            if let Some(path) = jsstring_to_string(scope, private.to_string()) {
                if let Some(parent) = Path::new(&path).parent() {
                    if !parent.as_os_str().is_empty() {
                        return Some(parent.to_path_buf());
                    }
                }
            }
        }
    }
    base_path(|bp| bp.borrow().clone())
}

/// The kinds of module the loader compiles from a file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModuleKind {
    JavaScript,
    Json,
}

/// Resolve a specifier to a file on disk, compile it as a `kind` module, and
/// cache it.
///
/// Only compiles the module. Loading its own imports, linking and evaluation
/// are handled by SpiderMonkey's module pipeline.
fn resolve_file_module<'r>(
    scope: &'r Scope,
    specifier: &str,
    base_dir: Option<PathBuf>,
    kind: ModuleKind,
) -> Result<Object<'r>, ModuleLoadError> {
    let base_dir = base_dir
        .ok_or_else(|| format!("Module '{}' not found (no base path configured)", specifier))?;
    // `oxc_resolver` converts `file:` URLs to paths only when it isn't compiled for wasm32.
    let file_path;
    let target = if specifier.starts_with("file:") {
        file_path = url::Url::parse(specifier)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .ok_or_else(|| format!("Cannot resolve module '{specifier}': invalid file URL"))?;
        file_path.to_str().unwrap_or(specifier)
    } else {
        specifier
    };

    // Resolve using oxc_resolver
    let resolved_path = resolver(|r| {
        let borrow = r.borrow();
        let resolver = borrow
            .as_ref()
            .expect("resolver not initialized — call init_module_loader first");
        resolver
            .resolve(&base_dir, target)
            .map(|res| res.path().to_path_buf())
            .map_err(|e| format!("Cannot resolve module '{}': {}", specifier, e))
    })?;

    // Key the cache by the canonicalized path, so the same file reached via
    // different specifiers, lexically different paths, or symlinks is one
    // module instance (and its side effects run once). `canonicalize` can fail on
    // wasm32-wasip2; fall back to a lexical normalization so `.`/`..`-equivalent
    // paths still dedup there (only symlink aliasing, which needs the filesystem,
    // remains best-effort when canonicalize is unavailable).
    let canonical_path = std::fs::canonicalize(&resolved_path)
        .unwrap_or_else(|_| lexically_normalize(resolved_path));
    let canonical_key = canonical_path.to_string_lossy().to_string();
    // A `.json` file imported as JavaScript is refused rather than compiled as a
    // script, which would fail on its syntax or silently evaluate it.
    if kind == ModuleKind::JavaScript
        && canonical_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        return Err(ModuleLoadError::Failed(format!(
            "Cannot import '{specifier}' as JavaScript: it resolves to the JSON file '{canonical_key}'. \
             Import it with `with {{ type: \"json\" }}`."
        )));
    }
    // A file imported both as JavaScript and as JSON is two modules, so the JSON
    // one is cached under a `json:`-prefixed key, which no canonical path has,
    // and which the registry lookup by specifier skips.
    let cache_key = match kind {
        ModuleKind::JavaScript => canonical_key.clone(),
        ModuleKind::Json => format!("json:{canonical_key}"),
    };

    // Check if already compiled under the canonical path
    let cached = cached_module(scope, &cache_key);
    if let Some(obj) = cached {
        return Ok(obj);
    }

    // Read source from disk
    let source = std::fs::read_to_string(&canonical_path)
        .map_err(|e| format!("Failed to read '{}': {}", canonical_path.display(), e))?;

    // Compile (but do NOT link or evaluate — SpiderMonkey handles that)
    let c_filename =
        CString::new(canonical_key.as_bytes()).map_err(|_| "Invalid filename".to_string())?;
    let options = js::compile::options(scope, c_filename, 1);
    let mut src = transform_str_to_source_text(&source);
    let module = match kind {
        ModuleKind::JavaScript => {
            // SAFETY: `options` and `src` are valid for the duration of this call.
            let module = unsafe { js::module::compile_module(scope, options.ptr, &mut src) }
                .map_err(|_| ModuleLoadError::CompileThrew)?;

            // Store the canonical path in the module private: the load hook reads it
            // to resolve this module's own relative imports against its directory.
            let path_str = js::JSString::from_str(scope, &canonical_key)
                .map_err(|_| "Failed to allocate module path string".to_string())?;
            // SAFETY: `module` is the rooted module object `compile_module` returned, and
            // `path_str` is rooted.
            unsafe { SetModulePrivate(module.as_raw(), &path_str.as_value()) };
            module
        }
        // A JSON module has no imports, so it needs no path.
        // SAFETY: `options` and `src` are valid for the duration of this call.
        ModuleKind::Json => {
            unsafe { js::module::compile_json_module(scope, options.ptr, &mut src) }
                .map_err(|_| ModuleLoadError::CompileThrew)?
        }
    };

    cache_module(cache_key, module);

    Ok(module)
}

/// Register `module`, an entry module compiled from the source of the file at
/// `path`, as that file's module, the way [`resolve_file_module`] registers a
/// file it compiles: a module importing the file gets `module`, and `module`'s
/// relative imports resolve against the file's directory. A relative `path` is
/// resolved against the current directory.
pub fn register_entry_module(
    scope: &Scope<'_>,
    module: Object<'_>,
    path: &Path,
) -> Result<(), ExnThrown> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| lexically_normalize(path));
    let key = canonical.to_string_lossy().to_string();
    let path_str = js::JSString::from_str(scope, &key).map_err(|_| ExnThrown)?;
    // SAFETY: `module` is a rooted module object, and `path_str` is rooted.
    unsafe { SetModulePrivate(module.as_raw(), &path_str.as_value()) };
    cache_module(key.clone(), module);
    with_state(|state| *state.entry.borrow_mut() = Some(key));
    Ok(())
}

/// Record `module`, an entry module with no file, as the entry
/// [`entry_namespace`] reads, under its synthetic `name`. Files are registered
/// under canonical absolute paths, so only an import whose specifier is `name`
/// verbatim gets `module`.
fn record_entry_module(module: Object<'_>, name: &str) {
    cache_module(name.to_string(), module);
    with_state(|state| *state.entry.borrow_mut() = Some(name.to_string()));
}

/// The namespace of the entry module [`register_entry_module`] or
/// `record_entry_module` recorded last, or `None` if there is none. The
/// namespace is only complete once the module has finished evaluating.
pub fn entry_namespace<'s>(scope: &'s Scope<'_>) -> Option<Object<'s>> {
    let key = with_state(|state| state.entry.borrow().clone())?;
    let module = cached_module(scope, &key)?;
    js::module::get_namespace(scope, module.handle())
        .ok()
        .and_then(Object::from_handle)
}

/// Why [`resolve_file_module`] could not produce a module.
enum ModuleLoadError {
    /// Resolving or reading the module failed. The message describes why.
    Failed(String),
    /// Compiling the module threw, and its exception is pending.
    CompileThrew,
}

impl From<String> for ModuleLoadError {
    fn from(message: String) -> Self {
        ModuleLoadError::Failed(message)
    }
}

/// Resolve `.` and `..` components lexically, without touching the filesystem, so
/// two lexically-equivalent paths produce the same module-cache key. Used as the
/// fallback key when [`std::fs::canonicalize`] is unavailable (it can fail under
/// wasm32-wasip2).
fn lexically_normalize(path: PathBuf) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            // Pop a preceding normal segment; keep a leading `..` (nothing to pop)
            // and never pop past a root/prefix.
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.push("..");
                } else {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Convert a JSString to a Rust String.
fn jsstring_to_string(scope: &Scope<'_>, s: *mut JSString) -> Option<String> {
    NonNull::new(s).map(|nn| jsstr_to_string(scope, nn))
}

// ============================================================================
// Public API
// ============================================================================

/// Install the module load hook and configure the filesystem resolver.
///
/// The load hook is installed on `rt`, and `base` is the directory import
/// specifiers resolve from (typically the directory holding the entry script).
///
/// This must be called once before any modules are registered or imported.
pub fn init_module_loader(rt: &crate::Runtime, base: PathBuf) {
    let state = &rt.modules;
    *state.base_path.borrow_mut() = Some(base);

    // SAFETY: `rt` owns the `JSRuntime` the hook is installed on, so it is valid
    // and outlives the hook.
    unsafe { js::module::set_module_load_hook(rt.rt(), Some(module_load_hook)) };

    *state.resolver.borrow_mut() = Some(Resolver::new(ResolveOptions {
        extensions: vec![".js".into(), ".mjs".into(), ".json".into()],
        ..ResolveOptions::default()
    }));
}

/// Drop the cached module objects. See [`crate::Runtime::clear_module_registry`], the public entry
/// point, for when a caller needs this.
///
/// The resolver and base path are left in place. They are per-runtime setup from
/// [`init_module_loader`], not per-evaluation state.
///
/// Must be called while the `JSContext` is still alive, because `Heap::drop()` fires GC write
/// barriers.
pub(crate) fn clear_module_registry(state: &ModuleState) {
    state.registry.borrow_mut().clear();
}

/// Clear all module state (registry, resolver, base path).
///
/// Must be called while the `JSContext` is still alive, because
/// `Heap::drop()` fires GC write barriers. Called automatically
/// by `Runtime::drop`.
pub(crate) fn clear_module_state(state: &ModuleState) {
    clear_module_registry(state);
    *state.base_path.borrow_mut() = None;
    *state.resolver.borrow_mut() = None;
    *state.entry.borrow_mut() = None;
}

/// Register a native module, making it available for `import` from JS.
///
/// This:
/// 1. Generates JS source with `export var ...;` for each declaration
/// 2. Compiles it as a module via `CompileModule`
/// 3. Links and evaluates the module
/// 4. Populates the module environment with native functions and values
/// 5. Stores the module in the runtime's module registry for the load hook
///
/// # Panics
///
/// Panics if no runtime is current.
pub fn register_module<T: NativeModule>(scope: &Scope<'_>) -> bool {
    let declarations = T::declarations();

    // 1. Generate JS module source: `export var name1; export var name2; ...`
    let mut source = String::new();
    for decl in &declarations {
        let name = match decl {
            ModuleExport::Function { js_name, .. } => js_name,
            ModuleExport::Value { js_name } => js_name,
        };
        source.push_str(&format!("export var {};\n", name));
    }

    // 2. Compile module
    let filename = CString::new(T::NAME).unwrap();
    let options = js::compile::options(scope, filename, 1);

    let mut src = transform_str_to_source_text(&source);
    // SAFETY: `options` and `src` are live locals.
    let module = match unsafe { js::module::compile_module(scope, options.ptr, &mut src) } {
        Ok(m) => m,
        Err(_) => return false,
    };

    // Native modules carry no path in their module private: the generated
    // `export var` source has no imports to resolve.

    // 3. Store in registry before loading (load hook must find it)
    cache_module(T::NAME.to_string(), module);

    // 4.-6. Link, evaluate, and populate the environment. On any failure,
    // remove the registry entry again so a later import of the module fails
    // to resolve instead of finding a half-initialized module whose value
    // exports are undefined.
    let populated = link_evaluate_and_populate::<T>(scope, module, &declarations);
    if !populated {
        registry(|reg| {
            reg.borrow_mut().remove(T::NAME);
        });
    }
    populated
}

/// Steps 4.-6. of [`register_module`]: link and evaluate the compiled module,
/// then populate its environment with the native exports.
fn link_evaluate_and_populate<T: NativeModule>(
    scope: &Scope<'_>,
    module: Object<'_>,
    declarations: &[ModuleExport],
) -> bool {
    let env = match link_evaluate_and_get_env(scope, module) {
        Some(env) => env,
        None => return false,
    };

    // Set up function exports by creating functions and setting them as properties
    for decl in declarations {
        if let ModuleExport::Function {
            js_name,
            native,
            nargs,
        } = decl
        {
            let c_name = CString::new(*js_name).unwrap();
            let func = match js::Function::new(scope, *native, *nargs, 0, &c_name) {
                Ok(f) => f,
                Err(_) => return false,
            };
            if env.set_property_by_name(scope, &c_name, func).is_err() {
                return false;
            }
        }
    }

    // Let the module implementation set value exports
    // SAFETY: `env` is the module's environment object.
    unsafe { T::evaluate(scope, env.handle()) }
}

/// Steps 4.-5. shared by every synthetic-module path: load the compiled
/// module's dependency graph, link it, evaluate it (running the
/// `export var ...` initializations), and return its environment object, ready
/// for the caller to populate.
///
/// `module` must be a freshly compiled, registered module object on `scope`'s
/// realm.
fn link_evaluate_and_get_env<'s>(scope: &'s Scope<'_>, module: Object<'_>) -> Option<Object<'s>> {
    // 4. Load the dependency graph, then link
    if js::module::load_requested_modules(scope, module).is_err() {
        return None;
    }
    if js::module::link(scope, module).is_err() {
        return None;
    }

    // 5. Evaluate (runs the `export var ...` initializations)
    if js::module::evaluate(scope, module).is_err() {
        return None;
    }

    // 6. Get the module environment
    js::module::get_environment(scope, module.handle())
        .and_then(|env| Object::from_handle(scope.root_object(env)))
}

/// Register a module under an arbitrary specifier, exporting the given named
/// values, and make it available for `import` from JS.
///
/// This generalizes [`register_module`] for callers that synthesize a module's
/// exports at runtime rather than from a static [`NativeModule`] type: the
/// `wit-bridge` interpreter builds one such module per imported WIT interface,
/// keyed by the interface's WIT specifier (e.g. `"test:iface/x@0.2.0"`), with
/// the import functions and resource classes as exports.
///
/// Each `(name, value)` pair becomes an `export var <name>` binding initialized
/// to `value`. The module is registered under the owned `name` string, so the
/// load hook resolves an exact-match `import ... from "<name>"`.
///
/// On any failure the half-registered entry is removed again, matching
/// [`register_module`]: a later import then fails to resolve rather than finding
/// a module whose value exports are undefined.
///
/// # Panics
///
/// Panics if no runtime is current.
pub fn register_synthetic_module(
    scope: &Scope<'_>,
    name: &str,
    exports: &[(&str, HandleValue)],
) -> Result<(), ExnThrown> {
    // 1. Generate JS module source: `export var name1; export var name2; ...`.
    // The bindings start out `undefined`; step 6 below assigns each its value.
    let mut source = String::new();
    for (export_name, _) in exports {
        source.push_str("export var ");
        source.push_str(export_name);
        source.push_str(";\n");
    }

    // 2. Compile the module.
    let filename = CString::new(name).map_err(|_| ExnThrown)?;
    let options = js::compile::options(scope, filename, 1);
    let mut src = transform_str_to_source_text(&source);
    // Synthetic modules carry no path in their module private: the generated
    // `export var` source has no imports of its own to resolve.
    // SAFETY: `options` and `src` are live locals.
    let module = unsafe { js::module::compile_module(scope, options.ptr, &mut src) }?;

    // 3. Store in the registry under the owned specifier before linking, so the
    //    load hook finds it.
    cache_module(name.to_string(), module);

    // 4.-6. Link, evaluate, and populate the environment with the export values.
    // On any failure, remove the registry entry again.
    let result = link_evaluate_and_populate_values(scope, module, exports);
    if result.is_err() {
        registry(|reg| {
            reg.borrow_mut().remove(name);
        });
    }
    result
}

/// Steps 4.-6. of [`register_synthetic_module`]: link and evaluate the compiled
/// module, then assign each export binding its provided value.
fn link_evaluate_and_populate_values(
    scope: &Scope<'_>,
    module: Object<'_>,
    exports: &[(&str, HandleValue)],
) -> Result<(), ExnThrown> {
    let env = link_evaluate_and_get_env(scope, module).ok_or(ExnThrown)?;

    for (export_name, value) in exports {
        let c_name = CString::new(*export_name).map_err(|_| ExnThrown)?;
        env.set_property_by_name(scope, &c_name, *value)?;
    }
    Ok(())
}

/// Evaluate a JS script as a module, with access to registered native modules.
///
/// This compiles the given source as a module, loads its dependency graph (the
/// load hook will find registered native modules and resolve file imports),
/// links it, and evaluates it.
///
/// The `filename` is used both as the script origin for error messages and
/// (if it's a real filesystem path) as the base for resolving relative imports.
///
/// # Safety
///
/// [`init_module_loader`] must have been called first.
// TODO: mark this as safe: it's no less safe than `evaluate_with_filename`, really
pub unsafe fn evaluate_module<'s>(
    scope: &'s Scope<'_>,
    source: &str,
    filename: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    // If the filename is a path in an existing directory, or a file in the
    // current directory, register the entry as that file's module. Other entries
    // (eval scripts, synthetic filenames) are recorded as the entry only, and
    // resolve their relative imports against the loader's base path.
    let path = Path::new(filename);
    let is_path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.exists(),
        _ => path.is_file(),
    };
    // SAFETY: the caller guarantees the module loader was initialized.
    unsafe { evaluate_entry_module(scope, source, filename, is_path.then_some(path)) }
}

/// Evaluate `source` as the entry module, compiled under `name`, which stack
/// traces and error messages show for it.
///
/// With a `path`, the module is registered as the module of that file (see
/// [`register_entry_module`]). Without one, it is recorded as the entry under
/// `name` only, and its relative imports resolve against the loader's base path.
/// Either way, [`entry_namespace`] reads its namespace afterwards.
///
/// # Panics
///
/// Panics if `name` contains a NUL character.
///
/// # Safety
///
/// [`init_module_loader`] must have been called first.
pub unsafe fn evaluate_entry_module<'s>(
    scope: &'s Scope<'_>,
    source: &str,
    name: &str,
    path: Option<&Path>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let c_name = CString::new(name).expect("module names contain no NUL character");
    let options = js::compile::options(scope, c_name, 1);

    let mut src = transform_str_to_source_text(source);
    // SAFETY: `options` and `src` are live locals.
    let module = unsafe { js::module::compile_module(scope, options.ptr, &mut src) }
        .map_err(|_| ExnThrown)?;

    match path {
        Some(path) => register_entry_module(scope, module, path)?,
        None => record_entry_module(module, name),
    }

    js::module::load_requested_modules(scope, module).map_err(|_| ExnThrown)?;
    js::module::link(scope, module).map_err(|_| ExnThrown)?;
    js::module::evaluate(scope, module).map_err(|_| ExnThrown)
}

/// Helper to set a value export on a module environment object.
///
/// Used by generated `evaluate` implementations from `#[jsmodule]`.
///
/// # Safety
///
/// `env` must be a valid module environment object.
pub unsafe fn set_module_export<'s, V: ToJSVal<'s> + ?Sized>(
    scope: &'s Scope<'_>,
    env: HandleObject,
    name: &str,
    value: &V,
) -> bool {
    let c_name = CString::new(name).unwrap();
    let env_obj = js::Object::from_handle(env).expect("module environment object is null");
    env_obj.set_property_by_name(scope, &c_name, value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use crate::runtime::Runtime;
    use crate::test_util::test_tempdir;
    use js::compile::evaluate_with_filename;
    use js::prelude::FromJSVal;

    #[test]
    fn lexically_normalize_dedups_dot_segments() {
        // The fallback key when canonicalize is unavailable must collapse
        // `.`/`..` so lexically-different specifiers for the same file dedup.
        assert_eq!(
            lexically_normalize(PathBuf::from("/base/./counter.js")),
            PathBuf::from("/base/counter.js")
        );
        assert_eq!(
            lexically_normalize(PathBuf::from("/base/sub/../counter.js")),
            PathBuf::from("/base/counter.js")
        );
        assert_eq!(
            lexically_normalize(PathBuf::from("/base/a/b/../../counter.js")),
            PathBuf::from("/base/counter.js")
        );
        // A leading `..` on a relative path is preserved (nothing to pop).
        assert_eq!(
            lexically_normalize(PathBuf::from("../x/counter.js")),
            PathBuf::from("../x/counter.js")
        );
    }

    /// Create a `Runtime` for testing with module support.
    ///
    /// Uses a minimal eval config so no filesystem script path is needed.
    /// The returned `Rc<Runtime>` cleans up all state on drop.
    fn test_runtime() -> std::rc::Rc<Runtime> {
        let config =
            RuntimeConfig::from_args(["starling", "-e", "42"].iter().map(|s| s.to_string()))
                .unwrap();
        Runtime::init(&config).expect("runtime init")
    }

    /// Helper: read back a globalThis property as f64.
    /// Handles both SpiderMonkey int32 and double representations.
    fn read_global_f64(scope: &js::gc::scope::Scope<'_>, expr: &str) -> f64 {
        let rval = evaluate_with_filename(scope, expr, "test_read.js", 1)
            .expect("evaluate_with_filename failed");
        if rval.is_double() {
            rval.to_double()
        } else if rval.is_int32() {
            rval.to_int32() as f64
        } else {
            panic!("expected number, got neither double nor int32");
        }
    }

    /// The `file:` URL of the absolute `path`.
    fn file_url(path: &str) -> String {
        url::Url::from_file_path(path).unwrap().to_string()
    }

    /// Helper: read back a globalThis property as String.
    fn read_global_string(scope: &js::gc::scope::Scope<'_>, expr: &str) -> String {
        let rval = evaluate_with_filename(scope, expr, "test_read.js", 1)
            .expect("evaluate_with_filename failed");
        assert!(rval.is_string());
        String::from_jsval(scope, rval, ()).expect("string conversion failed")
    }

    #[test]
    fn evaluate_module_syntax_error_fails() {
        let rt = test_runtime();
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(&scope, "this is not valid JS {{{", "bad.mjs");
            assert!(result.is_err());
        }
    }

    #[test]
    fn file_urls_extensionless_paths_and_directories_resolve() {
        let dir = test_tempdir();
        let helper_path = dir.path().join("helper.js");
        std::fs::write(&helper_path, "export const VALUE = 7;\n").unwrap();
        std::fs::write(dir.path().join("utils.js"), "export const RATIO = 2.5;\n").unwrap();
        let lib = dir.path().join("mylib");
        std::fs::create_dir(&lib).unwrap();
        std::fs::write(lib.join("index.js"), "export const HELLO = 'world';\n").unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        let url = file_url(helper_path.to_str().unwrap());
        unsafe {
            let result = evaluate_module(
                &scope,
                &format!(
                    r#"
                    import {{ VALUE }} from {url:?};
                    import {{ RATIO }} from "./utils";
                    import {{ HELLO }} from "./mylib";
                    globalThis._imported = VALUE;
                    globalThis._ratio = RATIO;
                    globalThis._hello = HELLO;
                "#
                ),
                "entry.mjs",
            );
            assert!(result.is_ok(), "module evaluation failed");
            assert_eq!(read_global_f64(&scope, "globalThis._imported"), 7.0);
            assert_eq!(read_global_f64(&scope, "globalThis._ratio"), 2.5);
            assert_eq!(read_global_string(&scope, "globalThis._hello"), "world");
        }
    }

    #[test]
    fn nested_imports_resolve_against_the_importing_module() {
        let dir = test_tempdir();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        // A decoy at the entry's base dir: resolution anchored to the entry
        // (instead of the importing module) would pick this one up.
        std::fs::write(dir.path().join("c.js"), "export const WHO = 'decoy';\n").unwrap();
        std::fs::write(sub.join("c.js"), "export const WHO = 'sub';\n").unwrap();
        std::fs::write(
            sub.join("b.js"),
            r#"import { WHO } from "./c.js"; export const FROM_B = WHO;"#,
        )
        .unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"
                    import { FROM_B } from "./sub/b.js";
                    globalThis._who = FROM_B;
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "nested module evaluation failed");
            assert_eq!(
                read_global_string(&scope, "globalThis._who"),
                "sub",
                "sub/b.js's './c.js' must resolve next to b.js, not next to the entry"
            );
        }
    }

    #[test]
    fn same_specifier_resolves_per_directory() {
        let dir = test_tempdir();
        let dir1 = dir.path().join("one");
        let dir2 = dir.path().join("two");
        std::fs::create_dir(&dir1).unwrap();
        std::fs::create_dir(&dir2).unwrap();
        std::fs::write(dir1.join("helper.js"), "export const N = 1;\n").unwrap();
        std::fs::write(dir2.join("helper.js"), "export const N = 2;\n").unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let r1 = evaluate_module(
                &scope,
                r#"import { N } from "./helper.js"; globalThis._n1 = N;"#,
                &dir1.join("entry1.mjs").to_string_lossy(),
            );
            assert!(r1.is_ok());
            let r2 = evaluate_module(
                &scope,
                r#"import { N } from "./helper.js"; globalThis._n2 = N;"#,
                &dir2.join("entry2.mjs").to_string_lossy(),
            );
            assert!(r2.is_ok());
            assert_eq!(
                (
                    read_global_f64(&scope, "globalThis._n1"),
                    read_global_f64(&scope, "globalThis._n2"),
                ),
                (1.0, 2.0),
                "the same relative specifier must resolve per importing directory"
            );
        }
    }

    #[test]
    fn lexically_different_paths_share_one_instance() {
        let dir = test_tempdir();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(
            dir.path().join("counter.js"),
            r#"
                if (!globalThis._lexCounter) globalThis._lexCounter = 0;
                globalThis._lexCounter++;
                export const dummy = 1;
            "#,
        )
        .unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let r1 = evaluate_module(
                &scope,
                r#"import { dummy } from "./counter.js";"#,
                &dir.path().join("entry1.mjs").to_string_lossy(),
            );
            assert!(r1.is_ok());
            // The same file via a lexically different path must hit the cache.
            let r2 = evaluate_module(
                &scope,
                r#"import { dummy } from "./sub/../counter.js";"#,
                &dir.path().join("entry2.mjs").to_string_lossy(),
            );
            assert!(r2.is_ok());
            assert_eq!(
                read_global_f64(&scope, "globalThis._lexCounter"),
                1.0,
                "one file must be one module instance regardless of the path spelling"
            );
        }
    }

    #[test]
    fn resolve_missing_module_fails() {
        let dir = test_tempdir();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"import { x } from "./nonexistent.js";"#,
                "entry.mjs",
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn versioned_specifier_resolves_to_a_compatible_version() {
        let rt = test_runtime();
        let scope = rt.default_global();
        for (name, version) in [
            ("test:pkg/iface@0.2.3", 3),
            ("test:pkg/iface@0.2.9", 9),
            ("test:pkg/major@1.10.0", 10),
            ("test:pkg/major@1.2.0", 2),
        ] {
            let value = scope.root_value(js::value::from_u32(version));
            register_synthetic_module(&scope, name, &[("version", value)])
                .expect("registering the module");
        }
        unsafe {
            evaluate_module(
                &scope,
                r#"import { version } from "test:pkg/iface@0.2.12";
                   globalThis._patchVersion = version;"#,
                "entry.mjs",
            )
            .expect("a patch version the registry lacks resolves to a compatible one");
            assert_eq!(read_global_f64(&scope, "globalThis._patchVersion"), 9.0);
            evaluate_module(
                &scope,
                r#"import { version } from "test:pkg/iface@0.2";
                   globalThis._minorVersion = version;"#,
                "no-patch.mjs",
            )
            .expect("a version without a patch number resolves to a compatible one");
            assert_eq!(read_global_f64(&scope, "globalThis._minorVersion"), 9.0);
            evaluate_module(
                &scope,
                r#"import { version } from "test:pkg/major@1";
                   globalThis._majorVersion = version;"#,
                "major.mjs",
            )
            .expect("a canonical major version resolves to the highest compatible one");
            assert_eq!(read_global_f64(&scope, "globalThis._majorVersion"), 10.0);
            let result = evaluate_module(
                &scope,
                r#"import { version } from "test:pkg/iface@0.3.0";"#,
                "minor.mjs",
            );
            assert!(
                result.is_err(),
                "a different minor version must not resolve"
            );
        }
    }

    #[test]
    fn failed_native_module_is_not_importable() {
        // A native module whose `evaluate` fails must not stay registered:
        // importing it afterwards has to fail instead of yielding a module
        // with undefined value exports.
        struct BrokenNative;
        impl NativeModule for BrokenNative {
            const NAME: &'static str = "broken_native";
            fn declarations() -> Vec<ModuleExport> {
                vec![ModuleExport::Value { js_name: "VAL" }]
            }
            unsafe fn evaluate(_scope: &js::gc::scope::Scope<'_>, _env: HandleObject) -> bool {
                false
            }
        }

        let rt = test_runtime();
        let scope = rt.default_global();
        unsafe {
            assert!(!register_module::<BrokenNative>(&scope));
            let result = evaluate_module(
                &scope,
                r#"import { VAL } from "broken_native";"#,
                "entry.mjs",
            );
            assert!(
                result.is_err(),
                "import of a failed module must not resolve"
            );
        }
    }

    #[test]
    fn file_and_native_modules_coexist() {
        // Define a minimal native module inline (without the #[jsmodule] macro,
        // which generates code referencing the crate externally).
        struct TestNative;
        impl NativeModule for TestNative {
            const NAME: &'static str = "test_native";
            fn declarations() -> Vec<ModuleExport> {
                vec![ModuleExport::Value {
                    js_name: "NATIVE_VAL",
                }]
            }
            unsafe fn evaluate(scope: &js::gc::scope::Scope<'_>, env: HandleObject) -> bool {
                set_module_export(scope, env, "NATIVE_VAL", &777.0f64)
            }
        }

        let dir = test_tempdir();
        std::fs::write(
            dir.path().join("file_mod.js"),
            "export const FILE_VAL = 888;\n",
        )
        .unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            assert!(register_module::<TestNative>(&scope));

            let result = evaluate_module(
                &scope,
                r#"
                    import { NATIVE_VAL } from "test_native";
                    import { FILE_VAL } from "./file_mod.js";
                    globalThis._native = NATIVE_VAL;
                    globalThis._file = FILE_VAL;
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "mixed native+file module evaluation failed");
            assert_eq!(read_global_f64(&scope, "globalThis._native"), 777.0);
            assert_eq!(read_global_f64(&scope, "globalThis._file"), 888.0);
        }
    }

    #[test]
    fn invalid_json_module_fails_to_load() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("bad.json"), r#"{ "n": "#).unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"import data from "./bad.json" with { type: "json" };"#,
                "entry.mjs",
            );
            assert!(result.is_err(), "an invalid JSON module must not load");
        }
    }

    #[test]
    fn json_file_imported_as_javascript_is_refused() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("data.json"), r#"{ "n": 7 }"#).unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(&scope, r#"import data from "./data.json";"#, "entry.mjs");
            assert!(
                result.is_err(),
                "a JSON file imported as JavaScript must be refused"
            );
            let message = ExnThrown::capture(&scope).message.unwrap_or_default();
            assert!(message.contains(r#"with { type: "json" }"#), "{message}");
        }
    }

    #[test]
    fn unsupported_module_type_is_refused() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("style.css"), "body {}\n").unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"import style from "./style.css" with { type: "css" };"#,
                "entry.mjs",
            );
            assert!(result.is_err(), "a CSS module import must be refused");
            let message = ExnThrown::capture(&scope).message.unwrap_or_default();
            assert!(
                message.contains("only JavaScript and JSON modules are supported"),
                "{message}"
            );
        }
    }

    #[test]
    fn dynamic_import_loads_the_module_graph_after_the_call_returns() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("c.js"), "export const BASE = 10;\n").unwrap();
        std::fs::write(
            dir.path().join("b.js"),
            r#"
                import { BASE } from "./c.js";
                globalThis._order.push("b evaluated");
                export const DOUBLED = BASE * 2;
            "#,
        )
        .unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"
                    globalThis._order = [];
                    import("./b.js").then((m) => { globalThis._dynamic = m.DOUBLED; });
                    globalThis._order.push("import() returned");
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "module evaluation failed");
            crate::event_loop::run_microtasks(&scope);
            assert_eq!(read_global_f64(&scope, "globalThis._dynamic"), 20.0);
            assert_eq!(
                read_global_string(&scope, "globalThis._order.join()"),
                "import() returned,b evaluated"
            );
        }
    }

    #[test]
    fn top_level_await_on_dynamic_import() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("helper.js"), "export const VALUE = 5;\n").unwrap();
        std::fs::write(dir.path().join("data.json"), r#"{ "n": 3, "s": "seven" }"#).unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"
                    import data from "./data.json" with { type: "json" };
                    const { VALUE } = await import("./helper.js");
                    const json = await import("./data.json", { with: { type: "json" } });
                    globalThis._sum = VALUE + json.default.n;
                    globalThis._sameJson = json.default === data;
                    globalThis._jsonS = data.s;
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "module evaluation failed");
            crate::event_loop::run_microtasks(&scope);
            assert_eq!(read_global_f64(&scope, "globalThis._sum"), 8.0);
            assert_eq!(
                read_global_string(&scope, "String(globalThis._sameJson)"),
                "true"
            );
            assert_eq!(read_global_string(&scope, "globalThis._jsonS"), "seven");
        }
    }

    #[test]
    fn dynamic_imports_of_missing_modules_and_dependencies_reject() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("present.js"), "import './absent.js';\n").unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"
                    import("./missing.js").catch((e) => { globalThis._missing = e.message; });
                    import("./present.js").catch((e) => { globalThis._dependency = e.message; });
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "module evaluation failed");
            crate::event_loop::run_microtasks(&scope);
            assert!(read_global_string(&scope, "globalThis._missing")
                .contains("Cannot resolve module './missing.js'"));
            assert!(read_global_string(&scope, "globalThis._dependency")
                .contains("Cannot resolve module './absent.js'"));
        }
    }
}
