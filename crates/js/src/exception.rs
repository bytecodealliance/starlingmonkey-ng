// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Pending exception management.
//!
//! This module provides direct access to the pending exception on a
//! `JSContext`. For the higher-level error type that wraps these operations,
//! see [`super::error::ExnThrown`].

use crate::gc::scope::Scope;
use mozjs::jsapi::ExceptionStackBehavior;
use mozjs::jsval::UndefinedValue;
use mozjs::rust::wrappers2;
use mozjs::rust::HandleValue;

use super::error::ExnThrown;

#[inline]
pub fn check_fn_return(scope: &Scope<'_>, ok: bool, name: &str) -> bool {
    if ok {
        debug_assert!(
            !is_pending(scope),
            "Native function '{name}' returned true but an exception is pending",
        );
    }
    // Note that we can't do the inverse of the above assert: `false` without a pending
    // exception is legitimate, since SpiderMonkey's API can return `false` without a
    // pending exception for uncatchable exceptions.
    ok
}

/// Check whether an exception is pending on the context.
pub fn is_pending(scope: &Scope<'_>) -> bool {
    unsafe { wrappers2::JS_IsExceptionPending(scope.cx()) }
}

/// Check whether the context is throwing an out-of-memory error.
pub fn is_throwing_oom(scope: &Scope<'_>) -> bool {
    unsafe { wrappers2::JS_IsThrowingOutOfMemory(scope.cx()) }
}

/// Get the pending exception value, or `None` if no exception is pending or it
/// can't be retrieved.
pub fn get_pending<'r>(scope: &'r Scope<'_>) -> Option<HandleValue<'r>> {
    let mut vp = scope.root_value_mut(UndefinedValue());
    let ok = unsafe { wrappers2::JS_GetPendingException(scope.cx_mut(), vp.reborrow()) };
    ok.then(|| vp.handle())
}

/// Get and clear the pending exception value, or `None` if no exception is
/// pending or it can't be retrieved. An exception that can't be retrieved stays
/// pending.
pub fn take_pending<'r>(scope: &'r Scope<'_>) -> Option<HandleValue<'r>> {
    let result = get_pending(scope)?;
    clear(scope);
    Some(result)
}

/// Get and clear the pending exception value, or return `undefined` if there is none.
///
/// Can be used in contexts where JS execution failed, but an uncatchable exception
/// (e.g., OOM) may be pending.
pub fn take_pending_or_undefined<'r>(scope: &'r Scope<'_>) -> HandleValue<'r> {
    take_pending(scope).unwrap_or(HandleValue::undefined())
}

/// Set a pending exception on the context.
pub fn set_pending(
    scope: &Scope<'_>,
    v: HandleValue,
    behavior: ExceptionStackBehavior,
) -> ExnThrown {
    unsafe { wrappers2::JS_SetPendingException(scope.cx_mut(), v, behavior) };
    ExnThrown
}

/// Throw `value` without capturing a new exception stack.
pub fn throw_value(scope: &Scope<'_>, value: HandleValue) -> ExnThrown {
    set_pending(scope, value, ExceptionStackBehavior::DoNotCapture)
}

/// Clear any pending exception on the context.
pub fn clear(scope: &Scope<'_>) {
    unsafe { wrappers2::JS_ClearPendingException(scope.cx()) }
}

/// Report the pending exception to stderr with a context label, then clear it.
///
/// For algorithms where a callback's throw is "reported" per spec but must not
/// abort the surrounding algorithm — event listener invocation, abort
/// algorithms, promise-settle bookkeeping. Swallowing such exceptions silently
/// makes real bugs in callback code undetectable, so they are printed in the
/// same shape as the event loop's uncaught-exception report. No-op when
/// nothing is pending.
pub fn report_and_clear(scope: &Scope<'_>, context: &str) {
    if !is_pending(scope) {
        return;
    }
    // `capture` clears the pending exception as part of extracting it.
    let captured = ExnThrown::capture(scope);
    eprintln!(
        "[{context}] Uncaught exception: {}",
        captured.to_string().trim_end()
    );
}

/// Get the `JSErrorReport` from an Error object.
///
/// Returns a reference to the error report if the object is an Error, or
/// `None` otherwise. The returned reference borrows the exception object's
/// internal data and is valid as long as the exception object remains rooted
/// (guaranteed by the [`HandleObject`](mozjs::gc::HandleObject) argument, whose
/// lifetime bounds the result).
pub fn error_from_exception<'a>(
    scope: &Scope<'_>,
    obj: mozjs::gc::HandleObject<'a>,
) -> Option<&'a mozjs::jsapi::JSErrorReport> {
    mozjs::rust::borrowed_error_report(scope.cx(), |cx, report| {
        if !unsafe { wrappers2::JS_ErrorFromException(cx, obj, report) } {
            return None;
        }
        // SAFETY: the call populated the report, and SpiderMonkey keeps it
        // alive for the lifetime of the Error object, which is kept alive by
        // the Handle.
        Some(unsafe { &*report.report_ })
    })
}

/// Whether the pending exception is a `SyntaxError` with an engine error report, such as a compile
/// error. Leaves it pending.
pub fn is_syntax_error_pending(scope: &Scope<'_>) -> bool {
    let Some(error) = get_pending(scope) else {
        return false;
    };
    let Some(object) = std::ptr::NonNull::new(error.to_object_or_null()) else {
        return false;
    };
    let object = scope.root_object(object);
    error_from_exception(scope, object)
        .is_some_and(|report| report.exnType == mozjs::jsapi::JSExnType::JSEXN_SYNTAXERR as i16)
}

/// Report an uncatchable exception (e.g., OOM or stack overflow).
pub fn report_uncatchable(scope: &Scope<'_>) {
    unsafe { wrappers2::ReportUncatchableException(scope.cx()) }
}

/// Report an out-of-memory condition.
pub fn report_out_of_memory(scope: &Scope<'_>) {
    unsafe { wrappers2::JS_ReportOutOfMemory(scope.cx()) }
}

/// Whether `value` is an `Error` object: one created by `Error` or one of its
/// native subclasses, or by a class extending one of those.
pub fn is_error_object(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    if !value.is_object() {
        return false;
    }
    let obj = crate::Object::from_value(scope, value).expect("the value is an object");
    // A failed check counts as not an `Error` object, with its exception cleared.
    match obj.builtin_class(scope) {
        Ok(class) => class == crate::object::ESClass::Error,
        Err(ExnThrown) => {
            clear(scope);
            false
        }
    }
}
