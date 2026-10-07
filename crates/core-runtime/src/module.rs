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
//! via a thread-local registry.
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
//! unsafe {
//!     register_module::<my_math::js_module>(&scope);
//!     // JS can now: import { PI, add } from "myMath";
//! }
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
use js::prelude::{FromJSVal, HandleValue, RootScope};
use js::{allow_unrooted, Object};
use oxc_resolver::{ResolveOptions, Resolver};
use std::cell::{Cell, RefCell};
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

/// What a native module exports as `default`, besides its named exports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefaultExport {
    /// No `default` export.
    None,
    /// An object with every named export as a property, the shape of a Node.js builtin module's
    /// CommonJS `module.exports`. A `self_export` is a further named export holding that object,
    /// which is also one of its properties, such as `node:path`'s `posix`.
    Namespace { self_export: Option<&'static str> },
    /// The named export `name`, which must be an object. Every named export, including this one,
    /// is also set as a property on it, so it has the properties the object `Namespace` gives.
    /// Node's `events` module, whose `module.exports` is `EventEmitter`, has this shape. A
    /// `self_export` is as for `Namespace`, such as `node:assert/strict`'s `strict`.
    Export {
        name: &'static str,
        self_export: Option<&'static str>,
    },
}

impl DefaultExport {
    /// The named export holding the `default` export, if there is one besides `default`.
    pub fn self_export(self) -> Option<&'static str> {
        match self {
            DefaultExport::None => None,
            DefaultExport::Namespace { self_export }
            | DefaultExport::Export { self_export, .. } => self_export,
        }
    }
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

    /// The module's `default` export.
    const DEFAULT_EXPORT: DefaultExport = DefaultExport::None;

    /// Named exports initialized from the properties of the same name on the `default` export,
    /// after it was created. Only meaningful with a `DEFAULT_EXPORT` other than `None`.
    const MIRRORED_EXPORTS: &'static [&'static str] = &[];

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
// Module registry (thread-local)
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
    /// Whether Node.js compatibility is enabled, which adds the `node` resolution condition and
    /// CommonJS modules.
    pub(crate) node_compat: Cell<bool>,
    /// The resolver for `require` calls, with Node.js compatibility.
    require_resolver: RefCell<Option<Resolver>>,
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

fn base_path<R>(f: impl FnOnce(&RefCell<Option<PathBuf>>) -> R) -> R {
    with_state(|state| f(&state.base_path))
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
    let Some(promise) = js::Object::from_value(scope, eval_result.get())
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
// Builtin modules
// ============================================================================

/// A builtin module, created the first time a module imports it.
#[derive(Clone, Copy)]
pub struct BuiltinModule {
    /// The specifier the module is registered under, such as `node:fs`.
    pub name: &'static str,
    /// Further specifiers that import the module, such as `fs`.
    pub aliases: &'static [&'static str],
    /// Register the module in the module registry under `name`, which is passed in, the way
    /// [`register_module`] does. Returns `false`, with an exception pending, if that failed.
    pub create: fn(&Scope<'_>, &'static str) -> bool,
}

js::instance_local! {
    static BUILTIN_MODULES: RefCell<Vec<BuiltinModule>> = const { RefCell::new(Vec::new()) };
}

/// Make `module` importable from every runtime created after this call. A module registered under
/// the same name before is replaced.
///
/// A builtin module's name and aliases take precedence over files: importing one never consults
/// the resolver.
pub fn register_builtin_module(module: BuiltinModule) {
    BUILTIN_MODULES.with(|modules| {
        let mut modules = modules.borrow_mut();
        modules.retain(|registered| registered.name != module.name);
        modules.push(module);
    });
}

/// Forget every module [`register_builtin_module`] registered.
pub fn clear_builtin_modules() {
    BUILTIN_MODULES.with(|modules| modules.borrow_mut().clear());
}

/// The builtin module `specifier` names, by its name or one of its aliases.
pub fn builtin_module(specifier: &str) -> Option<BuiltinModule> {
    BUILTIN_MODULES.with(|modules| {
        modules
            .borrow()
            .iter()
            .find(|module| module.name == specifier || module.aliases.contains(&specifier))
            .copied()
    })
}

/// The module object of `builtin`, created and registered on first use.
fn builtin_module_object<'s>(
    scope: &'s Scope<'_>,
    builtin: BuiltinModule,
) -> Result<Object<'s>, ModuleLoadError> {
    let lookup = || {
        registry(|reg| {
            reg.borrow()
                .get(builtin.name)
                .map(|entry| entry.module_obj.get(scope))
        })
    };
    if let Some(module) = lookup() {
        return Ok(module);
    }
    // SAFETY: the module loader is initialized, since its load hook or `require` called this.
    if !(builtin.create)(scope, builtin.name) {
        return Err(ModuleLoadError::CompileThrew);
    }
    lookup().ok_or_else(|| {
        ModuleLoadError::Failed(format!(
            "Builtin module '{}' did not register itself",
            builtin.name
        ))
    })
}

// ============================================================================
// CommonJS
// ============================================================================

/// Runs a CommonJS module for an `import` of it. Given the module's canonical path and its
/// format, `CommonJs` or `Ambiguous`, returns the module's `module.exports`, or `None` for an
/// `Ambiguous` file whose source is an ES module.
pub type CommonJsLoader =
    for<'s> fn(&'s Scope<'_>, &Path, SourceFormat) -> Result<Option<HandleValue<'s>>, ExnThrown>;

/// Runs the `-e` entry script's source as Node.js runs `-e` code. Returns `false`, having run
/// nothing, if the source has ES module syntax, which is then evaluated as an ES module.
pub type CommonJsEval = for<'s> fn(&'s Scope<'_>, &str) -> Result<bool, ExnThrown>;

/// The filename of the `-e` entry script.
pub const EVAL_FILENAME: &str = "<eval>";

js::instance_local! {
    static COMMONJS_LOADER: Cell<Option<CommonJsLoader>> = const { Cell::new(None) };
    static COMMONJS_EVAL: Cell<Option<CommonJsEval>> = const { Cell::new(None) };
}

/// Install the loader that runs CommonJS modules ES modules import, in runtimes with Node.js
/// compatibility. Without one, every module is an ES module.
pub fn set_commonjs_loader(loader: Option<CommonJsLoader>) {
    COMMONJS_LOADER.with(|cell| cell.set(loader));
}

/// Install the function that runs the `-e` entry script, in runtimes with Node.js compatibility.
/// Without one, the `-e` entry script is an ES module.
pub fn set_commonjs_eval(eval: Option<CommonJsEval>) {
    COMMONJS_EVAL.with(|cell| cell.set(eval));
}

/// The module an ES module gets for importing the CommonJS module at `path`, cached under
/// `cache_key`: a module whose `default` and `"module.exports"` exports are the CommonJS module's
/// `module.exports`, and whose other named exports are the own enumerable string-keyed
/// properties of `module.exports`, if it is an object.
///
/// Returns `None` if `path` isn't a CommonJS module: the runtime has no Node.js compatibility or
/// no CommonJS loader, `format` isn't `CommonJs` or `Ambiguous`, or the loader found an
/// `Ambiguous` file to be an ES module.
///
/// The CommonJS module runs here, while the importing module graph loads, rather than when the
/// graph evaluates.
fn commonjs_facade<'s>(
    scope: &'s Scope<'_>,
    path: &Path,
    cache_key: &str,
    format: SourceFormat,
) -> Result<Option<Object<'s>>, ModuleLoadError> {
    if !matches!(format, SourceFormat::CommonJs | SourceFormat::Ambiguous)
        || !with_state(|state| state.node_compat.get())
    {
        return Ok(None);
    }
    let Some(loader) = COMMONJS_LOADER.with(Cell::get) else {
        return Ok(None);
    };
    let Some(exports) = loader(scope, path, format).map_err(|_| ModuleLoadError::CompileThrew)?
    else {
        return Ok(None);
    };
    let mut names = Vec::new();
    if exports.is_object() {
        let keys = (|| -> Result<Vec<String>, ExnThrown> {
            let object = Object::from_value(scope, exports.get()).map_err(|e| e.throw(scope))?;
            object
                .own_enumerable_keys(scope)?
                .into_iter()
                .map(|key| String::from_jsval(scope, key, ()).map_err(|e| e.throw(scope)))
                .collect()
        })()
        .map_err(|_| ModuleLoadError::CompileThrew)?;
        names.extend(
            keys.into_iter()
                .filter(|name| name != "default" && name != "module.exports"),
        );
    }
    let mut values: Vec<(&str, HandleValue)> =
        vec![("default", exports), ("module.exports", exports)];
    for name in &names {
        let c_name = CString::new(name.as_str()).map_err(|_| "Invalid export name".to_string())?;
        let object =
            Object::from_value(scope, exports).map_err(|_| ModuleLoadError::CompileThrew)?;
        let value = object
            .get_property_by_name(scope, &c_name)
            .map_err(|_| ModuleLoadError::CompileThrew)?;
        values.push((name.as_str(), value));
    }
    register_synthetic_module(scope, cache_key, &values)
        .map_err(|_| ModuleLoadError::CompileThrew)?;
    let module = registry(|reg| {
        reg.borrow()
            .get(cache_key)
            .map(|entry| entry.module_obj.get(scope))
    });
    Ok(module)
}

/// Resolve `specifier` as a `require` call from a module in `base_dir` does, with the `require`
/// conditions. Returns the resolved file's canonical path and its format.
pub fn resolve_require(
    specifier: &str,
    base_dir: &Path,
) -> Result<(PathBuf, SourceFormat), ResolveError> {
    let (path, format) = resolve_path_with(specifier, Some(base_dir.to_path_buf()), true)?;
    Ok((canonical_path(path), format))
}

/// `path` with symbolic links and `.` and `..` segments resolved, the key the module registry
/// and `require.cache` use. `path` itself, normalized lexically, if it can't be canonicalized.
pub fn canonical_path(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or_else(|_| lexically_normalize(path))
}

/// The namespace of the builtin module `specifier` names, created if needed, or `None` if it
/// names none.
pub fn builtin_namespace<'s>(
    scope: &'s Scope<'_>,
    specifier: &str,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let Some(builtin) = builtin_module(specifier) else {
        return Ok(None);
    };
    let module = builtin_module_object(scope, builtin).map_err(|error| error.throw(scope))?;
    let namespace = js::module::get_namespace(scope, module.handle())?;
    Ok(Object::from_handle(namespace))
}

/// Load, link and evaluate the ES module file at `path` synchronously, as `require` of an ES
/// module does, and return its namespace.
///
/// Fails if the module graph uses top-level `await` that doesn't settle synchronously, since
/// `require` can't wait for it. Loading a module that isn't in the registry yet reads its file,
/// so it throws an `Error` with the `code` `ERR_BLOCKING_NOT_ALLOWED` where blocking isn't
/// allowed.
pub fn require_module<'s>(scope: &'s Scope<'_>, path: &Path) -> Result<Object<'s>, ExnThrown> {
    let base_dir = path.parent().map(Path::to_path_buf);
    let specifier = path.to_string_lossy();
    if let Some(reason) = crate::event_loop::blocking_host_io_refusal() {
        if !registry(|reg| reg.borrow().contains_key(specifier.as_ref())) {
            let message = format!("Loading '{specifier}' would block {reason}");
            let message = CString::new(message.replace('\0', "")).map_err(|_| ExnThrown)?;
            js::error::throw_plain_error(scope, &message);
            if let Some(error) = js::exception::take_pending(scope) {
                if let Ok(object) = Object::from_value(scope, error) {
                    object.set_property(scope, c"code", "ERR_BLOCKING_NOT_ALLOWED")?;
                }
                js::exception::set_pending(
                    scope,
                    error,
                    js::native::ExceptionStackBehavior::Capture,
                );
            }
            return Err(ExnThrown);
        }
    }
    let module = resolve_file_module(scope, &specifier, base_dir, ModuleKind::JavaScript)
        .map_err(|error| error.throw(scope))?;
    js::module::load_requested_modules(scope, module)?;
    js::module::link(scope, module)?;
    let evaluation = js::module::evaluate(scope, module)?;
    if let Ok(promise) = Object::from_value(scope, evaluation.get())
        .map_err(|_| ())
        .and_then(|object| object.cast::<js::Promise>().map_err(|_| ()))
    {
        if promise.is_pending() {
            return Err(js::error::TypeError(format!(
                "require() cannot be used on an ES module that uses top-level await: {}",
                path.display()
            ))
            .throw(scope));
        }
        if promise.is_rejected() {
            let reason = promise.result(scope).unwrap_or(evaluation);
            js::exception::set_pending(scope, reason, js::native::ExceptionStackBehavior::Capture);
            return Err(ExnThrown);
        }
    }
    let namespace = js::module::get_namespace(scope, module.handle())?;
    Object::from_handle(namespace).ok_or(ExnThrown)
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

    if !referrer.get().is_null() {
        // SAFETY: `referrer` is the load hook's rooted `referrer` argument.
        let private =
            js::module::get_script_private(&scope, unsafe { GCHandle::from_raw(referrer) });
        if let Some(refusal) = DynamicImportRefusal::from_private(private) {
            throw_dynamic_import_refused(&scope, refusal);
            return false;
        }
    }

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

    // 1. Check the builtin modules, which only a runtime with Node.js compatibility has, then the
    // module registry for an exact match. The `json:`-prefixed keys hold JSON modules, which a
    // JavaScript import never gets.
    let builtin = match kind {
        ModuleKind::JavaScript if with_state(|state| state.node_compat.get()) => {
            builtin_module(&specifier)
        }
        ModuleKind::JavaScript | ModuleKind::Json => None,
    };
    let cached = match kind {
        ModuleKind::JavaScript if builtin.is_none() && !specifier.starts_with("json:") => {
            registry(|reg| {
                reg.borrow()
                    .get(&specifier)
                    .map(|entry| entry.module_obj.get(&scope))
            })
        }
        ModuleKind::JavaScript | ModuleKind::Json => None,
    };

    // 2. Resolve via filesystem using oxc_resolver, relative to the referrer.
    let referrer_path = referrer_path(&scope, referrer);
    let loaded = match (builtin, cached) {
        (Some(builtin), _) => builtin_module_object(&scope, builtin),
        (None, Some(obj)) => Ok(obj),
        (None, None) => {
            let base_dir = match referrer_path
                .as_deref()
                .map(Path::new)
                .and_then(Path::parent)
            {
                Some(parent) if !parent.as_os_str().is_empty() => Some(parent.to_path_buf()),
                _ => base_path(|bp| bp.borrow().clone()),
            };
            resolve_file_module(&scope, &specifier, base_dir, kind)
        }
    };
    let module = match loaded {
        Ok(obj) => obj,
        Err(error) => {
            error.throw_for_import(&scope, referrer_path.as_deref());
            return false;
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

/// Why a dynamic `import()` in a Node.js `vm` script fails, and the `TypeError` it fails with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DynamicImportRefusal {
    /// The script was compiled without `importModuleDynamically`:
    /// `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`.
    CallbackMissing,
    /// The script was compiled with an `importModuleDynamically` function, which Node.js only
    /// calls with `--experimental-vm-modules`: `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING_FLAG`.
    CallbackMissingFlag,
}

impl DynamicImportRefusal {
    /// The script private value that marks a script whose `import()` fails for this reason.
    fn private(self) -> i32 {
        match self {
            DynamicImportRefusal::CallbackMissing => 0,
            DynamicImportRefusal::CallbackMissingFlag => 1,
        }
    }

    /// The reason the script private value `private` marks, if it marks one.
    fn from_private(private: HandleValue<'_>) -> Option<Self> {
        if !private.is_int32() {
            return None;
        }
        match private.to_int32() {
            0 => Some(DynamicImportRefusal::CallbackMissing),
            1 => Some(DynamicImportRefusal::CallbackMissingFlag),
            _ => None,
        }
    }
}

/// Make a dynamic `import()` in `script`, or in a function compiled with it, fail with the error
/// of `refusal`, as an `import()` in a Node.js `vm` script does. Sets the script's private value
/// to an integer.
pub fn refuse_dynamic_import(
    scope: &Scope<'_>,
    script: GCHandle<'_, *mut js::native::JSScript>,
    refusal: DynamicImportRefusal,
) {
    let refused = scope.root_value(js::value::from_i32(refusal.private()));
    js::module::set_script_private(script, refused);
}

/// Throw the error of an `import()` that [`refuse_dynamic_import`] refused for `refusal`.
fn throw_dynamic_import_refused(scope: &Scope<'_>, refusal: DynamicImportRefusal) {
    let (code, message) = match refusal {
        DynamicImportRefusal::CallbackMissing => (
            "ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING",
            "A dynamic import callback was not specified.",
        ),
        DynamicImportRefusal::CallbackMissingFlag => (
            "ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING_FLAG",
            "A dynamic import callback was invoked without --experimental-vm-modules",
        ),
    };
    let _ = throw_coded_error(
        scope,
        js::error::JSExnType::JSEXN_TYPEERR,
        Some(code),
        message,
    );
}

/// The path of the referencing module, if its module private holds one. The load hook resolves
/// a relative specifier against its parent directory, or against the loader's base path for a
/// referrer without a path (eval-script entries and native modules).
fn referrer_path(
    scope: &Scope<'_>,
    referrer: js::native::RawHandle<*mut js::native::JSScript>,
) -> Option<String> {
    if referrer.get().is_null() {
        return None;
    }
    // SAFETY: `referrer` is the load hook's rooted `referrer` argument.
    let private = js::module::get_script_private(scope, unsafe { GCHandle::from_raw(referrer) });
    if !private.is_string() {
        return None;
    }
    jsstring_to_string(scope, private.to_string())
}

/// The format of a module file, from its extension and the `type` of the nearest
/// `package.json`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceFormat {
    /// A `.cjs` file, or a `.js` file in a package of `type` `commonjs`.
    CommonJs,
    /// A `.mjs` file, or a `.js` file in a package of `type` `module`.
    Module,
    /// A `.json` file.
    Json,
    /// A `.js` file with no `type` in reach. It is an ES module if its source uses ES module
    /// syntax, and CommonJS otherwise.
    Ambiguous,
    /// Any other file, such as a native addon.
    Other,
}

impl SourceFormat {
    fn from_resolution(module_type: Option<oxc_resolver::ModuleType>, path: &Path) -> Self {
        use oxc_resolver::ModuleType;
        match module_type {
            Some(ModuleType::CommonJs) => SourceFormat::CommonJs,
            Some(ModuleType::Module) => SourceFormat::Module,
            Some(ModuleType::Json) => SourceFormat::Json,
            Some(ModuleType::Wasm | ModuleType::Addon) => SourceFormat::Other,
            None if path.extension().is_some_and(|ext| ext == "js") => SourceFormat::Ambiguous,
            None => SourceFormat::Other,
        }
    }
}

/// Resolve `specifier` to a file with `oxc_resolver`, relative to `base_dir`, using the
/// resolver for `import`s or, with `for_require`, the one for `require` calls.
fn resolve_path_with(
    specifier: &str,
    base_dir: Option<PathBuf>,
    for_require: bool,
) -> Result<(PathBuf, SourceFormat), ResolveError> {
    if specifier.starts_with("node:") {
        return Err(ResolveError {
            kind: ResolveErrorKind::UnknownBuiltin,
            message: format!("No such built-in module: {specifier}"),
        });
    }
    let base_dir = base_dir.ok_or_else(|| ResolveError {
        kind: ResolveErrorKind::NotFound,
        message: format!("Cannot find module '{specifier}': no base path is configured"),
    })?;
    // `oxc_resolver` converts `file:` URLs to paths only when it isn't compiled for wasm32.
    let file_path;
    let target = if specifier.starts_with("file:") {
        file_path = url::Url::parse(specifier)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .ok_or_else(|| ResolveError {
                kind: ResolveErrorKind::Other,
                message: format!("Cannot resolve module '{specifier}': invalid file URL"),
            })?;
        file_path.to_str().unwrap_or(specifier)
    } else {
        specifier
    };
    with_state(|state| {
        let import = state.resolver.borrow();
        let require = state.require_resolver.borrow();
        let resolver = match (for_require, require.as_ref()) {
            (true, Some(require)) => require,
            _ => import
                .as_ref()
                .expect("resolver not initialized — call init_module_loader first"),
        };
        resolver
            .resolve(&base_dir, target)
            .map(|res| {
                let format = SourceFormat::from_resolution(res.module_type(), res.path());
                (res.path().to_path_buf(), format)
            })
            .map_err(|error| ResolveError::from_resolver(specifier, error))
    })
}

/// Why resolving a module specifier failed.
#[derive(Debug)]
pub struct ResolveError {
    pub kind: ResolveErrorKind,
    /// The message of the error, without the referrer an `import`'s error names.
    pub message: String,
}

/// The kinds of [`ResolveError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveErrorKind {
    /// No file or package matches the specifier.
    NotFound,
    /// The `exports` of the package the specifier names defines no entry for its subpath.
    PackagePathNotExported,
    /// A `node:` specifier names no builtin module.
    UnknownBuiltin,
    /// Any other failure, such as an invalid `package.json`.
    Other,
}

impl ResolveErrorKind {
    /// The `code` of the error an `import` throws for this failure, or `None` for
    /// [`ResolveErrorKind::Other`].
    pub fn import_code(self) -> Option<&'static str> {
        match self {
            ResolveErrorKind::NotFound => Some("ERR_MODULE_NOT_FOUND"),
            ResolveErrorKind::PackagePathNotExported => Some("ERR_PACKAGE_PATH_NOT_EXPORTED"),
            ResolveErrorKind::UnknownBuiltin => Some("ERR_UNKNOWN_BUILTIN_MODULE"),
            ResolveErrorKind::Other => None,
        }
    }

    /// The `code` of the error a `require` throws for this failure.
    pub fn require_code(self) -> &'static str {
        match self {
            ResolveErrorKind::NotFound | ResolveErrorKind::Other => "MODULE_NOT_FOUND",
            ResolveErrorKind::PackagePathNotExported => "ERR_PACKAGE_PATH_NOT_EXPORTED",
            ResolveErrorKind::UnknownBuiltin => "ERR_UNKNOWN_BUILTIN_MODULE",
        }
    }
}

impl ResolveError {
    /// The failure `error` of `oxc_resolver` to resolve `specifier`.
    fn from_resolver(specifier: &str, error: oxc_resolver::ResolveError) -> Self {
        use oxc_resolver::ResolveError as Failure;
        match error {
            Failure::NotFound(_) | Failure::MatchedAliasNotFound(..) => ResolveError {
                kind: ResolveErrorKind::NotFound,
                message: format!("Cannot find module '{specifier}'"),
            },
            Failure::PackagePathNotExported {
                subpath,
                package_json_path,
                ..
            } => ResolveError {
                kind: ResolveErrorKind::PackagePathNotExported,
                message: format!(
                    "Package subpath '{subpath}' is not defined by \"exports\" in {}",
                    package_json_path.display()
                ),
            },
            error => ResolveError {
                kind: ResolveErrorKind::Other,
                message: format!("Cannot resolve module '{specifier}': {error}"),
            },
        }
    }

    /// Throw the error of an `import` that failed this way, from the module at `referrer` if it
    /// has a path. The message of a `NotFound` or `PackagePathNotExported` failure names the
    /// referrer.
    fn throw_for_import(self, scope: &Scope<'_>, referrer: Option<&str>) -> ExnThrown {
        let mut message = self.message;
        if let (
            ResolveErrorKind::NotFound | ResolveErrorKind::PackagePathNotExported,
            Some(referrer),
        ) = (self.kind, referrer)
        {
            message = format!("{message} imported from {referrer}");
        }
        throw_coded_error(
            scope,
            js::error::JSExnType::JSEXN_ERR,
            self.kind.import_code(),
            &message,
        )
    }
}

/// Throw a new `kind` error with `message` and, if `code` is set, a `code` property.
fn throw_coded_error(
    scope: &Scope<'_>,
    kind: js::error::JSExnType,
    code: Option<&str>,
    message: &str,
) -> ExnThrown {
    let Ok(error) = js::error::new_error(scope, kind, message) else {
        return ExnThrown;
    };
    if let (Some(code), Ok(object)) = (code, Object::from_value(scope, error)) {
        if object.set_property(scope, c"code", code).is_err() {
            return ExnThrown;
        }
    }
    js::exception::throw_value(scope, error);
    ExnThrown
}

/// The absolute path a file-backed module was compiled from, which [`resolve_file_module`] and
/// [`register_entry_module`] store in its module private. `None` for a module without one.
fn module_path(scope: &Scope<'_>, module: Object<'_>) -> Option<String> {
    let script = js::module::get_module_script(module)?;
    let private = js::module::get_script_private(scope, scope.root_script(script));
    if !private.is_string() {
        return None;
    }
    jsstring_to_string(scope, private.to_string())
}

/// The `file:` URL of the absolute `path`.
fn file_url(path: &str) -> String {
    url::Url::from_file_path(path)
        .map(String::from)
        .unwrap_or_else(|()| format!("file://{path}"))
}

/// The module metadata hook, which populates `import.meta`.
///
/// A file-backed module gets `url` (its `file:` URL), `filename`, `dirname`, `resolve` and
/// `main`. Other modules get `resolve`, resolving against the loader's base path, and `main`.
/// `main` is `true` for the entry module.
unsafe extern "C" fn module_metadata_hook(
    cx: *mut js::native::RawJSContext,
    module_record: js::native::RawHandle<*mut JSObject>,
    meta_object: js::native::RawHandle<*mut JSObject>,
) -> bool {
    // SAFETY: SpiderMonkey calls the hook with a valid context, with the module's realm entered.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    // SAFETY: SpiderMonkey passes the hook rooted handles.
    let (module, meta) = unsafe {
        (
            Object::from_handle(GCHandle::from_raw(module_record)),
            Object::from_handle(GCHandle::from_raw(meta_object)),
        )
    };
    let (Some(module), Some(meta)) = (module, meta) else {
        return false;
    };
    populate_import_meta(&scope, module, meta).is_ok()
}

/// The body of [`module_metadata_hook`].
fn populate_import_meta(
    scope: &Scope<'_>,
    module: Object<'_>,
    meta: Object<'_>,
) -> Result<(), ExnThrown> {
    let path = module_path(scope, module);
    if let Some(path) = &path {
        let file = Path::new(path);
        let dirname = file.parent().map(|dir| dir.to_string_lossy().into_owned());
        meta.set_property(scope, c"url", file_url(path).as_str())?;
        meta.set_property(scope, c"filename", path.as_str())?;
        if let Some(dirname) = dirname {
            meta.set_property(scope, c"dirname", dirname.as_str())?;
        }
    }

    let resolve =
        js::Function::new_callback(scope, c"resolve", 1, import_meta_resolve, path.as_deref())?;
    meta.set_property(scope, c"resolve", resolve)?;

    let is_main = with_state(|state| state.entry.borrow().clone())
        .and_then(|key| {
            registry(|reg| {
                reg.borrow()
                    .get(&key)
                    .map(|entry| entry.module_obj.get(scope))
            })
        })
        .is_some_and(|entry: Object<'_>| entry.as_raw() == module.as_raw());
    meta.set_property(scope, c"main", is_main)?;
    Ok(())
}

/// `import.meta.resolve(specifier)`: the URL `specifier` resolves to from the module whose path
/// is `payload` (`undefined` for a module without one).
///
/// A builtin module resolves to its name, a file to its `file:` URL. Resolution is synchronous
/// and does not load the module.
fn import_meta_resolve(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let specifier = String::from_jsval(scope, args.get(0), ()).map_err(|e| e.throw(scope))?;
    if let Some(builtin) = builtin_module(&specifier) {
        return builtin.name.to_jsval_raw_throwing(scope);
    }
    let base_dir = if payload.is_string() {
        jsstring_to_string(scope, payload.to_string())
            .and_then(|path| Path::new(&path).parent().map(Path::to_path_buf))
    } else {
        base_path(|bp| bp.borrow().clone())
    };
    let referrer = payload
        .is_string()
        .then(|| jsstring_to_string(scope, payload.to_string()))
        .flatten();
    match resolve_path_with(&specifier, base_dir.clone(), false) {
        Ok((path, _)) => file_url(&path.to_string_lossy()).to_jsval_raw_throwing(scope),
        // A relative or absolute specifier resolves to its path whether a file exists there.
        Err(error) if error.kind == ResolveErrorKind::NotFound && is_path_specifier(&specifier) => {
            let path = lexically_normalize(base_dir.unwrap_or_default().join(&specifier));
            file_url(&path.to_string_lossy()).to_jsval_raw_throwing(scope)
        }
        Err(error) => Err(error.throw_for_import(scope, referrer.as_deref())),
    }
}

/// Whether `specifier` is a relative or absolute path: it starts with `/`, `./` or `../`.
fn is_path_specifier(specifier: &str) -> bool {
    specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../")
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
    let (resolved_path, format) = resolve_path_with(specifier, base_dir, false)?;

    // Key the cache by the canonicalized path, so the same file reached via
    // different specifiers, lexically different paths, or symlinks is one
    // module instance (and its side effects run once). `canonicalize` can fail on
    // wasm32-wasip2; fall back to a lexical normalization so `.`/`..`-equivalent
    // paths still dedup there (only symlink aliasing, which needs the filesystem,
    // remains best-effort when canonicalize is unavailable).
    let canonical_path = canonical_path(resolved_path);
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
    let cached = registry(|reg| {
        reg.borrow()
            .get(&cache_key)
            .map(|entry| entry.module_obj.get(scope))
    });
    if let Some(obj) = cached {
        return Ok(obj);
    }

    if kind == ModuleKind::JavaScript {
        if let Some(facade) = commonjs_facade(scope, &canonical_path, &cache_key, format)? {
            return Ok(facade);
        }
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

    registry(|reg| {
        reg.borrow_mut().insert(
            cache_key,
            ModuleEntry {
                module_obj: Heap::from(module),
            },
        );
    });

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
    let canonical = canonical_path(path);
    let key = canonical.to_string_lossy().to_string();
    let path_str = js::JSString::from_str(scope, &key).map_err(|_| ExnThrown)?;
    // SAFETY: `module` is a rooted module object, and `path_str` is rooted.
    unsafe { SetModulePrivate(module.as_raw(), &path_str.as_value()) };
    registry(|reg| {
        reg.borrow_mut().insert(
            key.clone(),
            ModuleEntry {
                module_obj: Heap::from(module),
            },
        );
    });
    with_state(|state| *state.entry.borrow_mut() = Some(key));
    Ok(())
}

/// Record `module`, an entry module with no file, as the entry
/// [`entry_namespace`] reads, under its synthetic `name`. Files are registered
/// under canonical absolute paths, so only an import whose specifier is `name`
/// verbatim gets `module`.
fn record_entry_module(module: Object<'_>, name: &str) {
    registry(|reg| {
        reg.borrow_mut().insert(
            name.to_string(),
            ModuleEntry {
                module_obj: Heap::from(module),
            },
        );
    });
    with_state(|state| *state.entry.borrow_mut() = Some(name.to_string()));
}

/// The absolute path of the file the entry module was read from, or `None` for an entry module
/// with no file, or none at all.
pub fn entry_path() -> Option<String> {
    let key = with_state(|state| state.entry.borrow().clone())?;
    let key = match key.strip_suffix(COMMONJS_ENTRY_SUFFIX) {
        Some(path) => path.to_string(),
        None => key,
    };
    Path::new(&key).is_absolute().then_some(key)
}

/// The suffix of the registry key of the module that imports a CommonJS entry, after the entry's
/// canonical path.
const COMMONJS_ENTRY_SUFFIX: &str = "#commonjs-entry";

/// The namespace of the entry module [`register_entry_module`] or
/// `record_entry_module` recorded last, or `None` if there is none. The
/// namespace is only complete once the module has finished evaluating.
pub fn entry_namespace<'s>(scope: &'s Scope<'_>) -> Option<Object<'s>> {
    let key = with_state(|state| state.entry.borrow().clone())?;
    let module = registry(|reg| {
        reg.borrow()
            .get(&key)
            .map(|entry| entry.module_obj.get(scope))
    })?;
    js::module::get_namespace(scope, module.handle())
        .ok()
        .and_then(Object::from_handle)
}

/// Why [`resolve_file_module`] could not produce a module.
enum ModuleLoadError {
    /// Resolving the specifier failed.
    Resolve(ResolveError),
    /// Reading or registering the module failed. The message describes why.
    Failed(String),
    /// Compiling the module threw, and its exception is pending.
    CompileThrew,
}

impl ModuleLoadError {
    /// Throw the error the load hook throws for an `import` from the module at `referrer` that
    /// failed this way, unless its exception is pending: the [`ResolveError`] of a resolution
    /// failure, or an `Error` with the message of another failure.
    fn throw_for_import(self, scope: &Scope<'_>, referrer: Option<&str>) -> ExnThrown {
        match self {
            ModuleLoadError::Resolve(error) => error.throw_for_import(scope, referrer),
            ModuleLoadError::Failed(message) => {
                let message =
                    CString::new(message).unwrap_or_else(|_| c"Module resolution failed".into());
                js::error::report_error_ascii(scope, &message)
            }
            ModuleLoadError::CompileThrew => ExnThrown,
        }
    }

    /// Throw the error of a module that failed to load this way outside the load hook, unless
    /// its exception is pending: the [`ResolveError`] of a resolution failure, or a `TypeError`
    /// with the message of another failure.
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        match self {
            ModuleLoadError::Resolve(error) => error.throw_for_import(scope, None),
            ModuleLoadError::Failed(message) => js::error::TypeError(message).throw(scope),
            ModuleLoadError::CompileThrew => ExnThrown,
        }
    }
}

impl From<String> for ModuleLoadError {
    fn from(message: String) -> Self {
        ModuleLoadError::Failed(message)
    }
}

impl From<ResolveError> for ModuleLoadError {
    fn from(error: ResolveError) -> Self {
        ModuleLoadError::Resolve(error)
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

/// The runtime's key in the [WinterTC runtime keys](https://runtime-keys.proposal.wintertc.org/)
/// sense: the first `package.json` `exports`/`imports` condition module resolution matches.
pub const RUNTIME_KEY: &str = "starlingmonkey";

/// The `package.json` `exports`/`imports` conditions an ES module import resolves with, in
/// addition to `default`, which always matches. With Node.js compatibility, `node` is added.
fn import_conditions(node_compat: bool) -> Vec<String> {
    let mut conditions = vec![RUNTIME_KEY];
    if node_compat {
        conditions.push("node");
    }
    conditions.extend(["import", "module-sync"]);
    conditions.into_iter().map(String::from).collect()
}

/// Install the module load and metadata hooks and configure the filesystem resolver.
///
/// The hooks are installed on `rt`, and `base` is the directory import
/// specifiers resolve from (typically the directory holding the entry script).
/// The resolver's conditions depend on whether `rt` has Node.js compatibility enabled.
///
/// This must be called once before any modules are registered or imported.
pub fn init_module_loader(rt: &crate::Runtime, base: PathBuf) {
    let state = &rt.modules;
    *state.base_path.borrow_mut() = Some(base);

    // SAFETY: `rt` owns the `JSRuntime` the hooks are installed on, so it is valid
    // and outlives them.
    unsafe {
        js::module::set_module_load_hook(rt.rt(), Some(module_load_hook));
        js::module::set_module_metadata_hook(rt.rt(), Some(module_metadata_hook));
    }

    let resolver = Resolver::new(ResolveOptions {
        extensions: [".js", ".mjs", ".cjs", ".json"]
            .into_iter()
            .map(String::from)
            .collect(),
        condition_names: import_conditions(state.node_compat.get()),
        module_type: true,
        ..ResolveOptions::default()
    });
    if state.node_compat.get() {
        // Shares the import resolver's cache of the filesystem.
        let require = resolver.clone_with_options(ResolveOptions {
            extensions: [".js", ".cjs", ".json", ".node"]
                .into_iter()
                .map(String::from)
                .collect(),
            condition_names: require_conditions(),
            module_type: true,
            ..ResolveOptions::default()
        });
        *state.require_resolver.borrow_mut() = Some(require);
    }
    *state.resolver.borrow_mut() = Some(resolver);
}

/// The `package.json` `exports`/`imports` conditions a `require` call resolves with, in addition
/// to `default`, which always matches.
fn require_conditions() -> Vec<String> {
    [RUNTIME_KEY, "node", "require", "module-sync"]
        .into_iter()
        .map(String::from)
        .collect()
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
    *state.require_resolver.borrow_mut() = None;
    *state.entry.borrow_mut() = None;
}

/// Register a native module, making it available for `import` from JS.
///
/// This:
/// 1. Generates JS source with `export var ...;` for each declaration
/// 2. Compiles it as a module via `CompileModule`
/// 3. Links and evaluates the module
/// 4. Populates the module environment with native functions and values
/// 5. Stores the module in the thread-local registry for the load hook
///
/// # Panics
///
/// Panics if no runtime is current.
pub fn register_module<T: NativeModule>(scope: &Scope<'_>) -> bool {
    register_module_with::<T>(scope, &[])
}

/// [`register_module`], with `extra` exports besides the module's declared ones: each
/// `(name, value)` pair becomes a named export with that value, which is also a property of the
/// module's `default` export, if it has one.
///
/// # Panics
///
/// Panics if no runtime is current.
pub fn register_module_with<T: NativeModule>(
    scope: &Scope<'_>,
    extra: &[(&str, HandleValue)],
) -> bool {
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
    if T::DEFAULT_EXPORT != DefaultExport::None {
        source.push_str("var __default; export { __default as default };\n");
    }
    if let Some(name) = T::DEFAULT_EXPORT.self_export() {
        source.push_str(&format!("export var {name};\n"));
    }
    for name in T::MIRRORED_EXPORTS {
        source.push_str(&format!("export var {name};\n"));
    }
    for (name, _) in extra {
        source.push_str(&format!("export var {name};\n"));
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
    registry(|reg| {
        reg.borrow_mut().insert(
            T::NAME.to_string(),
            ModuleEntry {
                module_obj: Heap::from(module),
            },
        );
    });

    // 4.-6. Link, evaluate, and populate the environment. On any failure,
    // remove the registry entry again so a later import of the module fails
    // to resolve instead of finding a half-initialized module whose value
    // exports are undefined.
    let populated = link_evaluate_and_populate::<T>(scope, module, &declarations, extra);
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
    extra: &[(&str, HandleValue)],
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
    if !unsafe { T::evaluate(scope, env.handle()) } {
        return false;
    }
    let mut names: Vec<&str> = declarations
        .iter()
        .map(|decl| match decl {
            ModuleExport::Function { js_name, .. } | ModuleExport::Value { js_name } => *js_name,
        })
        .collect();
    for (name, value) in extra {
        let Ok(c_name) = CString::new(*name) else {
            return false;
        };
        if env.set_property_by_name(scope, &c_name, *value).is_err() {
            return false;
        }
        names.push(name);
    }
    set_default_export(scope, env, &names, T::DEFAULT_EXPORT, T::MIRRORED_EXPORTS).is_ok()
}

/// Set the `default` export of a native module whose environment is `env` and whose named
/// exports are `names`, as `default_export` describes, then initialize the `mirrored` exports
/// from it. The named exports are copied onto the default export, except onto an existing object
/// that has an own property of the name, whose value replaces the named export instead.
fn set_default_export(
    scope: &Scope<'_>,
    env: Object<'_>,
    names: &[&str],
    default_export: DefaultExport,
    mirrored: &[&str],
) -> Result<(), ExnThrown> {
    let target = match default_export {
        DefaultExport::None => return Ok(()),
        DefaultExport::Namespace { .. } => Object::new_plain(scope)?,
        DefaultExport::Export { name, .. } => {
            let c_name = CString::new(name).map_err(|_| ExnThrown)?;
            let value = env.get_property_by_name(scope, &c_name)?;
            Object::from_value(scope, value.get()).map_err(|_| {
                js::error::TypeError(format!(
                    "the default export '{name}' of a native module must be an object"
                ))
                .throw(scope)
            })?
        }
    };
    let keep_existing = matches!(default_export, DefaultExport::Export { .. });
    for name in names {
        let c_name = CString::new(*name).map_err(|_| ExnThrown)?;
        if keep_existing && target.has_own_property_by_name(scope, &c_name)? {
            let value = target.get_property_by_name(scope, &c_name)?;
            env.set_property_by_name(scope, &c_name, value)?;
        } else {
            let value = env.get_property_by_name(scope, &c_name)?;
            target.set_property_by_name(scope, &c_name, value)?;
        }
    }
    if let Some(name) = default_export.self_export() {
        let c_name = CString::new(name).map_err(|_| ExnThrown)?;
        target.set_property_by_name(scope, &c_name, target)?;
        env.set_property_by_name(scope, &c_name, target)?;
    }
    for name in mirrored {
        let c_name = CString::new(*name).map_err(|_| ExnThrown)?;
        let value = target.get_property_by_name(scope, &c_name)?;
        env.set_property_by_name(scope, &c_name, value)?;
    }
    env.set_property(scope, c"__default", target)
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
/// component-model interpreter builds one such module per imported WIT interface,
/// keyed by the interface's WIT specifier (e.g. `"test:iface/x@0.2.0"`), with
/// the import functions and resource classes as exports.
///
/// Each `(name, value)` pair becomes an export named `name`, which can be any
/// string without lone surrogates, including `default`, initialized to `value`.
/// The module is registered under the owned `name` string, so the load hook
/// resolves an exact-match `import ... from "<name>"`.
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
    // 1. Generate JS module source: one `var` binding per export, exported under the export's
    // name, which can be any string. The bindings start out `undefined`, and step 6 below assigns
    // each its value.
    let mut source = String::new();
    for (index, (export_name, _)) in exports.iter().enumerate() {
        let quoted = quote_export_name(export_name);
        source.push_str(&format!(
            "var __e{index}; export {{ __e{index} as {quoted} }};\n"
        ));
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
    registry(|reg| {
        reg.borrow_mut().insert(
            name.to_string(),
            ModuleEntry {
                module_obj: Heap::from(module),
            },
        );
    });

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

/// `name` as a string literal for an `export { x as "name" }` clause.
fn quote_export_name(name: &str) -> String {
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    for c in name.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\u{2028}' => quoted.push_str("\\u2028"),
            '\u{2029}' => quoted.push_str("\\u2029"),
            c if (c as u32) < 0x20 => quoted.push_str(&format!("\\u{:04x}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Steps 4.-6. of [`register_synthetic_module`]: link and evaluate the compiled
/// module, then assign each export binding its provided value.
fn link_evaluate_and_populate_values(
    scope: &Scope<'_>,
    module: Object<'_>,
    exports: &[(&str, HandleValue)],
) -> Result<(), ExnThrown> {
    let env = link_evaluate_and_get_env(scope, module).ok_or(ExnThrown)?;

    for (index, (_, value)) in exports.iter().enumerate() {
        let binding = CString::new(format!("__e{index}")).map_err(|_| ExnThrown)?;
        env.set_property_by_name(scope, &binding, *value)?;
    }
    Ok(())
}

/// Whether the entry script at `path`, whose source is `source`, is a CommonJS module: the
/// runtime has Node.js compatibility and a CommonJS loader, and the file is a `.cjs` file, a
/// `.js` file in a package of `type` `commonjs`, or a `.js` file without a package `type` whose
/// source, without a byte order mark and `#!` line, compiles as a function body without a
/// `SyntaxError`.
fn is_commonjs_entry(scope: &Scope<'_>, path: &Path, source: &str) -> bool {
    if !with_state(|state| state.node_compat.get()) || COMMONJS_LOADER.with(Cell::get).is_none() {
        return false;
    }
    let absolute = std::env::current_dir().unwrap_or_default().join(path);
    let Some(name) = absolute.file_name() else {
        return false;
    };
    let specifier = format!("./{}", name.to_string_lossy());
    let format = resolve_path_with(&specifier, absolute.parent().map(Path::to_path_buf), false)
        .map(|(_, format)| format);
    match format {
        Ok(SourceFormat::CommonJs) => true,
        Ok(SourceFormat::Ambiguous) => {
            let source = source.strip_prefix('\u{feff}').unwrap_or(source);
            let source = match source.strip_prefix("#!") {
                Some(rest) => std::borrow::Cow::Owned(format!("//{rest}")),
                None => std::borrow::Cow::Borrowed(source),
            };
            let compiled =
                js::compile::compile_function(scope, c"", &[], &source, "commonjs-check", 0)
                    .is_ok();
            let syntax_error = !compiled && js::exception::is_syntax_error_pending(scope);
            js::exception::clear(scope);
            !syntax_error
        }
        _ => false,
    }
}

/// Compile the entry script `source`, named `filename`, as the module record to load, link and
/// evaluate, and register it as the module of the file at `path`, if given. Returns the record,
/// and whether the entry is a CommonJS module (see [`is_commonjs_entry`]). A CommonJS entry runs
/// through the CommonJS loader, from a module record that re-exports its exports, which is the
/// record returned.
pub fn compile_entry<'s>(
    scope: &'s Scope<'_>,
    source: &str,
    filename: &str,
    path: Option<&Path>,
) -> Result<(Object<'s>, bool), ExnThrown> {
    let c_filename = CString::new(filename).map_err(|_| ExnThrown)?;
    let options = js::compile::options(scope, c_filename, 1);
    let is_commonjs = path.is_some_and(|path| is_commonjs_entry(scope, path, source));
    if let (true, Some(path)) = (is_commonjs, path) {
        let absolute = std::env::current_dir().unwrap_or_default().join(path);
        let canonical = canonical_path(absolute);
        let quoted = quote_export_name(&canonical.to_string_lossy());
        let shim_source = format!("export * from {quoted}; export {{ default }} from {quoted};");
        let mut src = transform_str_to_source_text(&shim_source);
        // SAFETY: `options` and `src` are live locals.
        let shim = unsafe { js::module::compile_module(scope, options.ptr, &mut src) }
            .map_err(|_| ExnThrown)?;
        record_entry_module(
            shim,
            &format!("{}{COMMONJS_ENTRY_SUFFIX}", canonical.display()),
        );
        return Ok((shim, true));
    }
    let mut src = transform_str_to_source_text(source);
    // SAFETY: `options` and `src` are live locals.
    let module = unsafe { js::module::compile_module(scope, options.ptr, &mut src) }
        .map_err(|_| ExnThrown)?;
    match path {
        Some(path) => register_entry_module(scope, module, path)?,
        None => record_entry_module(module, filename),
    }
    Ok((module, false))
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
    if filename == EVAL_FILENAME && with_state(|state| state.node_compat.get()) {
        if let Some(eval) = COMMONJS_EVAL.with(Cell::get) {
            if eval(scope, source)? {
                return Ok(HandleValue::undefined());
            }
        }
    }
    let path = Path::new(filename);
    let is_path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.exists(),
        _ => path.is_file(),
    };
    let (module, is_commonjs) = compile_entry(scope, source, filename, is_path.then_some(path))?;

    js::module::load_requested_modules(scope, module).map_err(|_| ExnThrown)?;
    js::module::link(scope, module).map_err(|_| ExnThrown)?;
    let evaluation = js::module::evaluate(scope, module).map_err(|_| ExnThrown)?;
    if with_state(|state| state.node_compat.get()) && !is_commonjs {
        // The microtasks an ES module entry queued run before the ticks it queued, as in Node.js.
        js::jobs::run_microtasks(scope);
    }
    Ok(evaluation)
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

    /// Helper: read back a globalThis property as String.
    fn read_global_string(scope: &js::gc::scope::Scope<'_>, expr: &str) -> String {
        let rval = evaluate_with_filename(scope, expr, "test_read.js", 1)
            .expect("evaluate_with_filename failed");
        assert!(rval.is_string());
        String::from_jsval(scope, rval, ()).expect("string conversion failed")
    }

    #[test]
    fn evaluate_module_inline() {
        let rt = test_runtime();
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(&scope, "globalThis._moduleTest = 42;", "test_inline.mjs");
            assert!(result.is_ok());
            assert_eq!(read_global_f64(&scope, "globalThis._moduleTest"), 42.0);
        }
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
    fn resolve_file_import() {
        let dir = test_tempdir();
        let module_path = dir.path().join("helper.js");
        std::fs::write(&module_path, "export const VALUE = 99;\n").unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"
                    import { VALUE } from "./helper.js";
                    globalThis._imported = VALUE;
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "module evaluation failed");
            assert_eq!(read_global_f64(&scope, "globalThis._imported"), 99.0);
        }
    }

    #[test]
    fn resolve_file_url_import() {
        let dir = test_tempdir();
        let module_path = dir.path().join("helper.js");
        std::fs::write(&module_path, "export const VALUE = 7;\n").unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        let url = file_url(module_path.to_str().unwrap());
        unsafe {
            let result = evaluate_module(
                &scope,
                &format!("import {{ VALUE }} from {url:?};\nglobalThis._imported = VALUE;"),
                "entry.mjs",
            );
            assert!(result.is_ok(), "module evaluation failed");
            assert_eq!(read_global_f64(&scope, "globalThis._imported"), 7.0);
        }
    }

    #[test]
    fn resolve_nested_imports() {
        let dir = test_tempdir();

        // a.js imports from b.js, b.js imports from c.js
        std::fs::write(dir.path().join("c.js"), "export const BASE = 10;\n").unwrap();
        std::fs::write(
            dir.path().join("b.js"),
            r#"
                import { BASE } from "./c.js";
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
                    import { DOUBLED } from "./b.js";
                    globalThis._nested = DOUBLED;
                "#,
                &dir.path().join("entry.mjs").to_string_lossy(),
            );
            assert!(result.is_ok(), "nested module evaluation failed");
            assert_eq!(read_global_f64(&scope, "globalThis._nested"), 20.0);
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
    fn resolve_with_extension_inference() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("utils.js"), "export const RATIO = 2.5;\n").unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            // Import without .js extension — oxc_resolver should add it
            let result = evaluate_module(
                &scope,
                r#"
                    import { RATIO } from "./utils";
                    globalThis._ratio = RATIO;
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "extension inference failed");
            assert_eq!(read_global_f64(&scope, "globalThis._ratio"), 2.5);
        }
    }

    #[test]
    fn resolve_index_file() {
        let dir = test_tempdir();
        let sub = dir.path().join("mylib");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("index.js"), "export const HELLO = 'world';\n").unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"
                    import { HELLO } from "./mylib";
                    globalThis._hello = HELLO;
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "index.js resolution failed");
            assert_eq!(read_global_string(&scope, "globalThis._hello"), "world");
        }
    }

    #[test]
    fn duplicate_import_uses_cache() {
        let dir = test_tempdir();

        // A module with a side-effect counter
        std::fs::write(
            dir.path().join("counter.js"),
            r#"
                if (!globalThis._counter) globalThis._counter = 0;
                globalThis._counter++;
                export const dummy = 1;
            "#,
        )
        .unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            // Two separate entry modules both import counter.js
            let r1 = evaluate_module(
                &scope,
                r#"import { dummy } from "./counter.js";"#,
                "entry1.mjs",
            );
            assert!(r1.is_ok());
            // Second import of same module should be cached (not re-evaluated)
            let r2 = evaluate_module(
                &scope,
                r#"import { dummy } from "./counter.js";"#,
                "entry2.mjs",
            );
            assert!(r2.is_ok());
            assert_eq!(read_global_f64(&scope, "globalThis._counter"), 1.0);
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
    fn json_import_evaluates_to_the_parsed_value() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("data.json"), r#"{ "n": 7, "s": "seven" }"#).unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"
                    import data from "./data.json" with { type: "json" };
                    globalThis._jsonN = data.n;
                    globalThis._jsonS = data.s;
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "JSON module import failed");
            assert_eq!(read_global_f64(&scope, "globalThis._jsonN"), 7.0);
            assert_eq!(read_global_string(&scope, "globalThis._jsonS"), "seven");
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
        std::fs::write(dir.path().join("data.json"), r#"{ "n": 3 }"#).unwrap();

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
        }
    }

    #[test]
    fn dynamic_import_of_a_missing_module_rejects() {
        let dir = test_tempdir();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"
                    import("./missing.js").catch((e) => { globalThis._error = e.message; });
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "module evaluation failed");
            crate::event_loop::run_microtasks(&scope);
            assert!(read_global_string(&scope, "globalThis._error")
                .contains("Cannot find module './missing.js'"));
        }
    }

    #[test]
    fn dynamic_import_of_a_module_with_a_missing_dependency_rejects() {
        let dir = test_tempdir();
        std::fs::write(dir.path().join("present.js"), "import './missing.js';\n").unwrap();

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            let result = evaluate_module(
                &scope,
                r#"
                    import("./present.js").catch((e) => { globalThis._error = e.message; });
                "#,
                "entry.mjs",
            );
            assert!(result.is_ok(), "module evaluation failed");
            crate::event_loop::run_microtasks(&scope);
            assert!(read_global_string(&scope, "globalThis._error")
                .contains("Cannot find module './missing.js'"));
        }
    }

    /// A runtime like [`test_runtime`]'s, with Node.js compatibility enabled.
    fn node_compat_runtime() -> std::rc::Rc<Runtime> {
        let config = RuntimeConfig::from_args(
            ["starling", "--node-compat", "-e", "42"]
                .iter()
                .map(|s| s.to_string()),
        )
        .unwrap();
        Runtime::init(&config).expect("runtime init")
    }

    /// Package `exports` conditions match in the package's object order: the runtime key wins
    /// where it comes first, and `node` only matches with Node.js compatibility enabled.
    #[test]
    fn exports_conditions_depend_on_node_compat() {
        let dir = test_tempdir();
        for (name, exports) in [
            (
                "conds",
                r#"{"node": "./node.js", "import": "./import.js", "default": "./default.js"}"#,
            ),
            (
                "keyed",
                r#"{"starlingmonkey": "./sm.js", "node": "./node.js", "default": "./default.js"}"#,
            ),
        ] {
            let pkg = dir.path().join("node_modules").join(name);
            std::fs::create_dir_all(&pkg).unwrap();
            std::fs::write(
                pkg.join("package.json"),
                format!(r#"{{"exports": {exports}}}"#),
            )
            .unwrap();
            for file in ["node", "import", "default", "sm"] {
                std::fs::write(
                    pkg.join(format!("{file}.js")),
                    format!("export default '{file}';\n"),
                )
                .unwrap();
            }
        }

        let runtimes = [
            (test_runtime as fn() -> std::rc::Rc<Runtime>, "import sm"),
            (node_compat_runtime, "node sm"),
        ];
        for (runtime, expected) in runtimes {
            let rt = runtime();
            rt.reset_module_loader(dir.path().to_path_buf());
            let scope = rt.default_global();
            unsafe {
                evaluate_module(
                    &scope,
                    "import a from 'conds'; import b from 'keyed'; globalThis._picked = `${a} ${b}`;",
                    "entry.mjs",
                )
                .expect("module evaluation failed");
            }
            assert_eq!(read_global_string(&scope, "globalThis._picked"), expected);
        }
    }

    /// A file-backed module's `import.meta` has its `file:` URL, path and directory, `main` is
    /// `true` only for the entry module, and `resolve` resolves relative to the module without
    /// loading anything.
    #[test]
    fn import_meta_describes_the_module() {
        let dir = test_tempdir();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("target.js"), "throw new Error('loaded');\n").unwrap();
        std::fs::write(
            sub.join("meta.js"),
            "globalThis._meta = { ...import.meta, resolved: import.meta.resolve('./target.js') };\n",
        )
        .unwrap();
        let entry = dir.path().join("entry.mjs");

        let rt = test_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            evaluate_module(
                &scope,
                "import './sub/meta.js';\n\
                 globalThis._entryMain = import.meta.main;\n\
                 globalThis._missing = import.meta.resolve('./missing.js');\n\
                 try { import.meta.resolve('missing-package'); } catch (e) { globalThis._error = e.code; }",
                entry.to_str().unwrap(),
            )
            .expect("module evaluation failed");
        }

        let canonical_sub = std::fs::canonicalize(&sub).unwrap();
        let meta_path = canonical_sub.join("meta.js");
        let url_of = |path: &Path| url::Url::from_file_path(path).unwrap().to_string();
        assert_eq!(read_global_string(&scope, "_meta.url"), url_of(&meta_path));
        assert_eq!(
            read_global_string(&scope, "_meta.filename"),
            meta_path.to_str().unwrap()
        );
        assert_eq!(
            read_global_string(&scope, "_meta.dirname"),
            canonical_sub.to_str().unwrap()
        );
        assert_eq!(
            read_global_string(&scope, "_meta.resolved"),
            url_of(&canonical_sub.join("target.js"))
        );
        assert_eq!(
            read_global_string(&scope, "`${_meta.main} ${_entryMain} ${_error}`"),
            "false true ERR_MODULE_NOT_FOUND"
        );
        assert!(read_global_string(&scope, "_missing").ends_with("/missing.js"));
    }

    /// A builtin module is created on its first import, by name or alias, and every later import
    /// gets the same module. It takes precedence over a file the specifier would resolve to.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn builtin_modules_are_created_on_first_import() {
        thread_local! {
            static CREATED: Cell<u32> = const { Cell::new(0) };
        }
        fn create(scope: &Scope<'_>, name: &'static str) -> bool {
            CREATED.with(|created| created.set(created.get() + 1));
            let value = scope.root_value(js::value::from_i32(7));
            register_synthetic_module(scope, name, &[("value", value)]).is_ok()
        }

        let dir = test_tempdir();
        let shadow = dir.path().join("node_modules").join("builtin-alias");
        std::fs::create_dir_all(&shadow).unwrap();
        std::fs::write(shadow.join("index.js"), "export const value = 'file';\n").unwrap();

        register_builtin_module(BuiltinModule {
            name: "test:builtin",
            aliases: &["builtin-alias"],
            create,
        });
        let rt = node_compat_runtime();
        rt.reset_module_loader(dir.path().to_path_buf());
        let scope = rt.default_global();
        unsafe {
            evaluate_module(
                &scope,
                "import { value as a } from 'test:builtin';\n\
                 import { value as b } from 'builtin-alias';\n\
                 const c = (await import('builtin-alias')).value;\n\
                 globalThis._values = `${a} ${b} ${c}`;",
                "entry.mjs",
            )
            .expect("module evaluation failed");
            crate::event_loop::run_microtasks(&scope);
        }
        clear_builtin_modules();
        assert_eq!(read_global_string(&scope, "globalThis._values"), "7 7 7");
        assert_eq!(CREATED.with(Cell::get), 1);
    }
}
