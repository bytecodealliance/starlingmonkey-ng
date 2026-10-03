// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! ES module compilation, linking, and evaluation.
//!
//! This module wraps SpiderMonkey's ES module API, providing access to
//! compiling modules from source, linking them, evaluating them, and
//! inspecting their requested imports and namespace.

use std::cell::Cell;
use std::ptr::NonNull;

use crate::gc::scope::Scope;
use crate::{Object, Promise};
use mozjs::gc::{Handle, HandleObject, HandleValue};
use mozjs::jsapi::mozilla::Utf8Unit;
use mozjs::jsapi::HandleValue as RawHandleValue;
pub use mozjs::jsapi::ModuleType;
use mozjs::jsapi::{
    ExceptionStackBehavior, JSObject, JSScript, JSString, ModuleErrorBehaviour,
    ReadOnlyCompileOptions, SourceText,
};
use mozjs::jsval::UndefinedValue;
use mozjs::rust::wrappers2;

use super::error::ExnThrown;

/// Compile an ES module from UTF-16 source.
///
/// # Safety
///
/// `options` and `src_buf` must be valid pointers.
pub unsafe fn compile_module_utf16<'s>(
    scope: &'s Scope<'_>,
    options: *const ReadOnlyCompileOptions,
    src_buf: *mut SourceText<u16>,
) -> Result<Handle<'s, *mut JSObject>, ExnThrown> {
    let obj = wrappers2::CompileModule(scope.cx_mut(), options, src_buf);
    NonNull::new(obj)
        .map(|p| scope.root_object(p))
        .ok_or(ExnThrown)
}

/// Compile an ES module from UTF-8 source.
///
/// # Safety
///
/// `options` and `src_buf` must be valid pointers.
pub unsafe fn compile_module<'s>(
    scope: &'s Scope<'_>,
    options: *const ReadOnlyCompileOptions,
    src_buf: *mut SourceText<Utf8Unit>,
) -> Result<Object<'s>, ExnThrown> {
    let obj = wrappers2::CompileModule1(scope.cx_mut(), options, src_buf);
    Object::from_raw(scope, obj).ok_or(ExnThrown)
}

/// Compile a JSON module from UTF-8 source.
///
/// # Safety
///
/// `options` and `src_buf` must be valid pointers.
pub unsafe fn compile_json_module<'s>(
    scope: &'s Scope<'_>,
    options: *const ReadOnlyCompileOptions,
    src_buf: *mut SourceText<Utf8Unit>,
) -> Result<Object<'s>, ExnThrown> {
    let obj = wrappers2::CompileJsonModule1(scope.cx_mut(), options, src_buf);
    Object::from_raw(scope, obj).ok_or(ExnThrown)
}

/// Link a compiled module, resolving its imports.
///
/// This must be called after compilation and before evaluation.
pub fn link(scope: &Scope<'_>, module_record: Object) -> Result<(), ExnThrown> {
    let ok = unsafe { wrappers2::ModuleLink(scope.cx_mut(), module_record.handle()) };
    ExnThrown::check(ok)
}

/// Evaluate a linked module.
///
/// Returns the evaluation result (typically a promise for top-level await). A
/// promise is marked handled, since the caller consumes its outcome, so its
/// rejection is not reported as an unhandled one.
pub fn evaluate<'r>(
    scope: &'r Scope<'_>,
    module_record: Object,
) -> Result<HandleValue<'r>, ExnThrown> {
    let mut rval = scope.root_value_mut(UndefinedValue());
    let ok = unsafe {
        wrappers2::ModuleEvaluate(scope.cx_mut(), module_record.handle(), rval.reborrow())
    };
    ExnThrown::check(ok)?;
    let rval = rval.handle();
    if let Some(promise) = Object::from_value(scope, rval)
        .ok()
        .and_then(|object| object.cast::<Promise>().ok())
    {
        promise.set_any_is_handled(scope)?;
    }
    Ok(rval)
}

/// Throw if module evaluation failed.
pub fn throw_on_evaluation_failure(
    scope: &Scope<'_>,
    evaluation_promise: Promise,
    error_behaviour: ModuleErrorBehaviour,
) -> Result<(), ExnThrown> {
    let ok = unsafe {
        wrappers2::ThrowOnModuleEvaluationFailure(
            scope.cx_mut(),
            evaluation_promise.handle(),
            error_behaviour,
        )
    };
    ExnThrown::check(ok)
}

/// Get the module type for a requested import at `index`.
pub fn get_requested_module_type(
    scope: &Scope<'_>,
    module_record: Object,
    index: u32,
) -> ModuleType {
    unsafe { wrappers2::GetRequestedModuleType(scope.cx(), module_record.handle(), index) }
}

/// Get the private value of a script.
///
/// For a module's script this is the value [`SetModulePrivate`] stored, since
/// both go through the script's source object. A script that was never given
/// one yields `undefined`.
///
/// [`SetModulePrivate`]: crate::module_raw::SetModulePrivate
pub fn get_script_private<'s>(
    scope: &'s Scope<'_>,
    script: Handle<'_, *mut JSScript>,
) -> HandleValue<'s> {
    let mut rval = scope.root_value_mut(UndefinedValue());
    // SAFETY: `script` is rooted by the handle, and the out-param is rooted on
    // `scope`.
    unsafe { mozjs::glue::JS_GetScriptPrivate(script.get(), rval.reborrow().into()) };
    rval.handle()
}

/// Get the `JSScript` associated with a module record.
pub fn get_module_script(module_record: Object) -> Option<NonNull<JSScript>> {
    NonNull::new(unsafe { wrappers2::GetModuleScript(module_record.handle()) })
}

/// Get the specifier string of a module request.
pub fn get_module_request_specifier(
    scope: &Scope<'_>,
    module_request: HandleObject,
) -> Option<NonNull<JSString>> {
    NonNull::new(unsafe { wrappers2::GetModuleRequestSpecifier(scope.cx(), module_request) })
}

/// Get the type of a module request.
pub fn get_module_request_type(scope: &Scope<'_>, module_request: HandleObject) -> ModuleType {
    unsafe { wrappers2::GetModuleRequestType(scope.cx(), module_request) }
}

/// Get the namespace object of a module.
pub fn get_namespace<'s>(
    scope: &'s Scope<'_>,
    module_record: HandleObject,
) -> Result<Handle<'s, *mut JSObject>, ExnThrown> {
    let obj = unsafe { wrappers2::GetModuleNamespace(scope.cx_mut(), module_record) };
    NonNull::new(obj)
        .map(|p| scope.root_object(p))
        .ok_or(ExnThrown)
}

/// Get the module for a given namespace object.
pub fn get_module_for_namespace(
    scope: &Scope<'_>,
    module_namespace: HandleObject,
) -> Option<NonNull<JSObject>> {
    NonNull::new(unsafe { wrappers2::GetModuleForNamespace(scope.cx(), module_namespace) })
}

/// Get the module environment (lexical scope) object.
pub fn get_environment(scope: &Scope<'_>, module_obj: HandleObject) -> Option<NonNull<JSObject>> {
    NonNull::new(unsafe { wrappers2::GetModuleEnvironment(scope.cx(), module_obj) })
}

/// Set the module load hook on the runtime.
///
/// The hook is called by [`load_requested_modules`] once per import in the
/// graph, to turn a module request into a compiled module object. It must
/// conclude each call either by passing the module to
/// [`finish_loading_imported_module`] or by returning `false`, which leaves
/// the engine to fail the load with whatever exception is pending.
///
/// # Safety
///
/// `rt` must be a valid `JSRuntime` pointer. `hook` must be a valid function
/// pointer (or `None` to clear the hook).
pub unsafe fn set_module_load_hook(
    rt: *mut mozjs::jsapi::JSRuntime,
    hook: mozjs::jsapi::ModuleLoadHook,
) {
    mozjs::jsapi::SetModuleLoadHook(rt, hook);
}

/// Install the hook that populates a module's `import.meta` object.
///
/// The engine calls `hook` once per module, on the first evaluation of `import.meta` in it, with
/// the module record and the empty meta object, which the hook defines properties on. Without a
/// hook, evaluating `import.meta` throws.
///
/// # Safety
///
/// `rt` must be a valid `JSRuntime` pointer. `hook` must be a valid function pointer (or `None` to
/// clear the hook).
pub unsafe fn set_module_metadata_hook(
    rt: *mut mozjs::jsapi::JSRuntime,
    hook: mozjs::jsapi::ModuleMetadataHook,
) {
    mozjs::jsapi::SetModuleMetadataHook(rt, hook);
}

/// This module's share of the crate's thread-local state. See [`crate::tls`].
pub(crate) struct ModuleTls {
    /// Outcome of the innermost in-flight [`load_requested_modules`] call:
    /// `None` until one of its callbacks fires, `Some(Ok(()))` once the graph
    /// has loaded, `Some(Err(ExnThrown))` once it has failed.
    load_outcome: Cell<Option<Result<(), ExnThrown>>>,
}

impl ModuleTls {
    pub(crate) const fn new() -> Self {
        Self {
            load_outcome: Cell::new(None),
        }
    }
}

fn load_outcome<R>(f: impl FnOnce(&Cell<Option<Result<(), ExnThrown>>>) -> R) -> R {
    crate::tls::with(|tls| f(&tls.module.load_outcome))
}

unsafe extern "C" fn load_resolved(
    _cx: *mut mozjs::jsapi::JSContext,
    _host_defined: RawHandleValue,
) -> bool {
    load_outcome(|outcome| outcome.set(Some(Ok(()))));
    true
}

unsafe extern "C" fn load_rejected(
    cx: *mut mozjs::jsapi::JSContext,
    _host_defined: RawHandleValue,
    error: RawHandleValue,
) -> bool {
    load_outcome(|outcome| outcome.set(Some(Err(ExnThrown))));
    // The engine hands the failure to us as a value rather than a pending
    // exception, so re-throw it for the caller of `load_requested_modules`.
    unsafe {
        mozjs::jsapi::JS_SetPendingException(cx, error, ExceptionStackBehavior::Capture);
    }
    // This return value is `LoadRequestedModules`'s own, so `false` reports the
    // exception just set.
    false
}

/// Load the dependency graph of a compiled module, calling the runtime's
/// module load hook once per import.
///
/// This is the ES2025 `LoadRequestedModules` operation, and must be called
/// after compilation and before [`link`]. Loading runs to completion before
/// this returns, which requires a load hook that resolves every request
/// synchronously. A hook that defers leaves the graph unloaded, and this
/// returns `Err`.
pub fn load_requested_modules(scope: &Scope<'_>, module_record: Object) -> Result<(), ExnThrown> {
    // `LoadRequestedModules` takes no host-defined data. The callbacks receive
    // this value back and ignore it.
    let host_defined = HandleValue::undefined();
    // A load hook may itself compile and load a module graph, so save and
    // restore any enclosing call's outcome around this one.
    let enclosing = load_outcome(|outcome| outcome.replace(None));
    // The return value is whatever the callback that ran returned, which the
    // callbacks already recorded. Use the recorded outcome instead, which also
    // covers the case where neither callback ran.
    let _ = unsafe {
        wrappers2::LoadRequestedModules(
            scope.cx_mut(),
            module_record.handle(),
            host_defined,
            Some(load_resolved),
            Some(load_rejected),
        )
    };
    load_outcome(|outcome| outcome.replace(enclosing)).unwrap_or_else(|| {
        // Neither callback ran, so loading never concluded: either an
        // engine-level failure such as OOM, which leaves its own exception
        // pending, or a load hook that deferred, which leaves none.
        if crate::exception::is_pending(scope) {
            Err(ExnThrown)
        } else {
            Err(crate::error::throw_internal_error(
                scope,
                c"module loading did not complete: the module load hook deferred",
            ))
        }
    })
}

/// Supply the module a load hook was asked for, concluding that call.
///
/// `referrer`, `module_request` and `payload` must be the arguments the hook
/// received. The hook must call this exactly once per invocation unless it
/// returns `false`.
///
/// A promise `payload` belongs to a dynamic `import()`. The hook must have
/// loaded `result`'s own imports with [`load_requested_modules`] first, and the
/// engine then links and evaluates `result` in a promise job.
///
/// # Safety
///
/// Only valid from within a module load hook, with that hook's own arguments.
pub unsafe fn finish_loading_imported_module(
    scope: &Scope<'_>,
    referrer: Handle<'_, *mut JSScript>,
    module_request: HandleObject,
    payload: HandleValue,
    result: HandleObject,
) -> Result<(), ExnThrown> {
    let ok = unsafe {
        wrappers2::FinishLoadingImportedModule(
            scope.cx_mut(),
            referrer,
            module_request,
            payload,
            result,
            is_dynamic_import_payload(scope, payload),
        )
    };
    ExnThrown::check(ok)
}

/// Whether a load hook's `payload` belongs to a dynamic `import()`, whose
/// payload is its promise, rather than to loading a static module graph.
pub fn is_dynamic_import_payload(scope: &Scope<'_>, payload: HandleValue) -> bool {
    Object::from_value(scope, payload).is_ok_and(|object| object.cast::<Promise>().is_ok())
}
