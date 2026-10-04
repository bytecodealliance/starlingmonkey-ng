// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Context options, callbacks, and memory management.
//!
//! This module provides access to `JSContext`-level configuration, including
//! interrupt callbacks, private data, stack quotas, and runtime queries.

use std::ptr::NonNull;

use crate::gc::scope::Scope;
use mozjs::jsapi::{ContextOptions, JSInterruptCallback, JSRuntime, NativeStackSize};
use mozjs::rust::wrappers2;

use super::error::ExnThrown;

/// Get a reference to the context's [`ContextOptions`].
///
/// The returned reference borrows the context, so it is valid as long as the
/// borrow is held.
pub fn options<'a>(scope: &'a Scope<'_>) -> &'a ContextOptions {
    // SAFETY: ContextOptionsRef returns a non-null pointer to options that
    // live as long as the JSContext.
    unsafe { &*wrappers2::ContextOptionsRef(scope.cx()) }
}

/// Get a mutable reference to the context's [`ContextOptions`].
///
/// The returned reference borrows the context, so it is valid as long as the
/// borrow is held.
///
/// # Safety
///
/// The reference points at context-owned data reachable through any
/// `&Scope`, so Rust's uniqueness rule cannot be enforced by the signature.
/// The caller must not let it overlap with any other reference to the
/// options — neither one from [`options`] nor from a second `options_mut`
/// call.
#[allow(clippy::mut_from_ref)]
pub unsafe fn options_mut<'a>(scope: &'a Scope<'_>) -> &'a mut ContextOptions {
    // SAFETY: ContextOptionsRef returns a non-null pointer to options that
    // live as long as the JSContext.
    unsafe { &mut *wrappers2::ContextOptionsRef(scope.cx()) }
}

/// Enable recording the JavaScript stack each promise and async call was created on. SpiderMonkey
/// shows the recorded stacks as async parent frames in `Error.stack` and in debuggers.
///
/// With `debuggee_only`, stacks are recorded only in realms a `Debugger` observes. Otherwise they
/// are recorded in every realm.
pub fn set_async_stack_capture(cx: &mut mozjs::context::JSContext, debuggee_only: bool) {
    // SAFETY: ContextOptionsRef returns a non-null pointer to options that
    // live as long as the JSContext, and `cx` is borrowed exclusively.
    let options = unsafe { &mut *wrappers2::ContextOptionsRef(cx) };
    options.set_asyncStack_(true);
    options.set_asyncStackCaptureDebuggeeOnly_(debuggee_only);
}

/// Get the private data pointer associated with the context.
pub fn get_private(scope: &Scope<'_>) -> *mut std::os::raw::c_void {
    unsafe { wrappers2::JS_GetContextPrivate(scope.cx()) }
}

/// Set the private data pointer on the context.
///
/// # Safety
///
/// The caller must ensure `data` remains valid for the lifetime of the context
/// or until replaced.
pub unsafe fn set_private(scope: &Scope<'_>, data: *mut std::os::raw::c_void) {
    wrappers2::JS_SetContextPrivate(scope.cx(), data)
}

/// Get the parent `JSRuntime` for this context.
pub fn get_runtime(scope: &Scope<'_>) -> NonNull<JSRuntime> {
    let ptr = unsafe { wrappers2::JS_GetRuntime(scope.cx()) };
    // SAFETY: a valid JSContext always has a non-null runtime.
    NonNull::new(ptr).expect("JSContext runtime should never be null")
}

/// Set native stack size quotas for the context.
///
/// # Safety
///
/// Must be called before any script execution on this context.
pub unsafe fn set_native_stack_quota(
    scope: &Scope<'_>,
    system_code: NativeStackSize,
    trusted_script: NativeStackSize,
    untrusted_script: NativeStackSize,
) {
    wrappers2::JS_SetNativeStackQuota(scope.cx(), system_code, trusted_script, untrusted_script)
}

/// Bytes of shadow stack that [`RecursionGuard::enter`] keeps free above SpiderMonkey's limit on
/// wasm. SpiderMonkey's WASI limit is 1 KiB above the bottom of the stack, and the stack is the
/// first 1 MiB of linear memory.
#[cfg(target_family = "wasm")]
const WASM_STACK_MARGIN: usize = 128 * 1024;

/// One level of native recursion over a structure from content code, from
/// [`enter`](Self::enter) until the guard is dropped. Native code that recurses over such a
/// structure enters a level at each step and throws when it can't.
#[must_use = "the guard counts a level of recursion until it is dropped"]
pub struct RecursionGuard<'s> {
    #[cfg(target_family = "wasm")]
    rooting: NonNull<mozjs::jsapi::JS::RootingContext>,
    _scope: std::marker::PhantomData<&'s ()>,
}

impl<'s> RecursionGuard<'s> {
    /// Enter a level of recursion, or return `None` at the limit at which SpiderMonkey reports
    /// "too much recursion" for JavaScript:
    ///
    /// - Natively, the limit is the native stack limit for untrusted script.
    /// - On wasm, it is SpiderMonkey's recursion depth limit, which counts the levels of these
    ///   guards together with the engine's own nested recursion checks, or the shadow stack
    ///   position [`WASM_STACK_MARGIN`] above SpiderMonkey's stack limit, whichever comes first.
    ///   [`set_wasi_recursion_depth_limit`] changes the depth limit.
    ///
    /// Guards are dropped in the reverse order of entering them.
    #[inline]
    pub fn enter(scope: &'s Scope<'_>) -> Option<Self> {
        let marker = 0u8;
        let sp = std::ptr::addr_of!(marker) as usize;
        // SAFETY: reading the context pointer performs no GC.
        let rooting = unsafe { scope.raw_cx_no_gc() } as *mut mozjs::jsapi::JS::RootingContext;
        // SAFETY: a `JSContext` starts with its `JS::RootingContext` base, as in
        // `JS::RootingContext::get`, and `nativeStackLimit` is only written by
        // `JS_SetNativeStackQuota`.
        let limit = unsafe {
            (*rooting).nativeStackLimit
                [mozjs::jsapi::JS::StackKind::StackForUntrustedScript as usize]
        };
        #[cfg(target_family = "wasm")]
        let limit = limit + WASM_STACK_MARGIN;
        // The stack grows downward on every target the runtime supports.
        if sp <= limit {
            return None;
        }
        #[cfg(target_family = "wasm")]
        // SAFETY: as above. The engine reads and writes the depth only on this thread, in
        // properly nested scopes, as the guard does.
        unsafe {
            if (*rooting).wasiRecursionDepth >= (*rooting).wasiRecursionDepthLimit {
                return None;
            }
            (*rooting).wasiRecursionDepth += 1;
        }
        Some(RecursionGuard {
            #[cfg(target_family = "wasm")]
            // SAFETY: `raw_cx_no_gc` returns the live, non-null context.
            rooting: unsafe { NonNull::new_unchecked(rooting) },
            _scope: std::marker::PhantomData,
        })
    }
}

impl Drop for RecursionGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        #[cfg(target_family = "wasm")]
        // SAFETY: the context outlives the scope the guard borrows, and `enter` incremented the
        // depth.
        unsafe {
            (*self.rooting.as_ptr()).wasiRecursionDepth -= 1;
        }
    }
}

/// Set SpiderMonkey's recursion depth limit on wasm, the number of nested recursion checks at
/// which JavaScript and [`RecursionGuard::enter`] report "too much recursion". The default is 350.
/// A host that runs the runtime with a larger native stack can raise it.
#[cfg(target_family = "wasm")]
pub fn set_wasi_recursion_depth_limit(cx: &mut mozjs::context::JSContext, limit: u32) {
    // SAFETY: `cx` is a live context, and the setter only writes one of its fields.
    unsafe { wrappers2::SetWasiRecursionDepthLimit(cx, limit) }
}

/// Add an interrupt callback.
///
/// The callback is invoked when the engine is interrupted (e.g., by
/// `JS_RequestInterruptCallback`). Returns `false` to cancel execution.
///
/// # Safety
///
/// `callback` must be a valid function pointer that remains valid for the
/// lifetime of the context.
pub unsafe fn add_interrupt_callback(scope: &Scope<'_>, callback: JSInterruptCallback) -> bool {
    wrappers2::JS_AddInterruptCallback(scope.cx(), callback)
}

/// Check for a pending interrupt and invoke any registered callbacks.
///
/// If a callback cancels execution, this returns `Err(ExnThrown)` with no
/// pending exception — uncatchable termination has no exception value.
pub fn check_for_interrupt(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let ok = unsafe { wrappers2::JS_CheckForInterrupt(scope.cx_mut()) };
    ExnThrown::check(ok)
}

/// Request an interrupt callback (thread-safe).
pub fn request_interrupt_callback(scope: &Scope<'_>) {
    unsafe { wrappers2::JS_RequestInterruptCallback(scope.cx()) }
}

/// Allocate memory tracked by the GC.
///
/// # Safety
///
/// The returned pointer must be freed with [`free`].
pub unsafe fn malloc(scope: &Scope<'_>, nbytes: usize) -> *mut std::os::raw::c_void {
    wrappers2::JS_malloc(scope.cx(), nbytes)
}

/// Free memory previously allocated with [`malloc`].
///
/// # Safety
///
/// `p` must have been allocated with [`malloc`] on the same context.
pub unsafe fn free(scope: &Scope<'_>, p: *mut std::os::raw::c_void) {
    wrappers2::JS_free(scope.cx(), p)
}

/// Allow `Atomics.wait` to block on the thread of `cx`.
pub fn set_futex_can_wait(cx: &mut mozjs::context::JSContext) {
    // SAFETY: `cx` is a live context.
    unsafe { mozjs::jsapi::JS_SetFutexCanWait(cx.raw_cx()) }
}
