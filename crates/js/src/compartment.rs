// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Cross-compartment wrapper management and compartment queries.

use std::ptr::NonNull;

use crate::gc::scope::Scope;
use mozjs::gc::HandleObject;
use mozjs::jsapi::{CompartmentFilter, JSClass, JSObject};
use mozjs::rust::wrappers2;

use super::error::ExnThrown;

/// Refresh existing cross-compartment wrappers to an object.
pub fn refresh_cross_compartment_wrappers(
    scope: &Scope<'_>,
    obj: HandleObject,
) -> Result<(), ExnThrown> {
    let ok = unsafe { wrappers2::JS_RefreshCrossCompartmentWrappers(scope.cx_mut(), obj) };
    ExnThrown::check(ok)
}

/// Recompute wrappers matching the given compartment filters.
///
/// # Safety
///
/// `source_filter` and `target_filter` must be valid pointers.
pub unsafe fn recompute_wrappers(
    scope: &Scope<'_>,
    source_filter: *const CompartmentFilter,
    target_filter: *const CompartmentFilter,
) -> Result<(), ExnThrown> {
    let ok = wrappers2::RecomputeWrappers(scope.cx_mut(), source_filter, target_filter);
    ExnThrown::check(ok)
}

/// Check whether an object is in the context's current compartment.
pub fn is_object_in_context_compartment(obj: NonNull<JSObject>, scope: &Scope<'_>) -> bool {
    unsafe { wrappers2::IsObjectInContextCompartment(obj.as_ptr(), scope.cx()) }
}

/// Get the number of system compartments.
pub fn system_compartment_count(scope: &Scope<'_>) -> usize {
    unsafe { wrappers2::SystemCompartmentCount(scope.cx()) }
}

/// Get the number of user compartments.
pub fn user_compartment_count(scope: &Scope<'_>) -> usize {
    unsafe { wrappers2::UserCompartmentCount(scope.cx()) }
}

/// Set the window proxy class for the context.
///
/// # Safety
///
/// `clasp` must be a valid `JSClass` pointer that outlives the context.
pub unsafe fn set_window_proxy_class(scope: &Scope<'_>, clasp: *const JSClass) {
    wrappers2::SetWindowProxyClass(scope.cx(), clasp)
}

/// Set the window proxy for a global object.
pub fn set_window_proxy(scope: &Scope<'_>, global: HandleObject, window_proxy: HandleObject) {
    unsafe { wrappers2::SetWindowProxy(scope.cx(), global, window_proxy) }
}
