// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Debugger, profiling, and testing utilities.
//!
//! This module provides access to SpiderMonkey's debugging and profiling
//! infrastructure, including the `Debugger` object, profiling stack control,
//! and testing functions.

use std::ptr::NonNull;

use crate::gc::scope::Scope;
use mozjs::gc::HandleObject;
use mozjs::jsapi::{JSObject, ProfilingStack};
use mozjs::rust::wrappers2;

use super::error::ExnThrown;

/// Define the `Debugger` constructor on a global object.
pub fn define_debugger_object(scope: &Scope<'_>, obj: HandleObject) -> Result<(), ExnThrown> {
    let ok = unsafe { wrappers2::JS_DefineDebuggerObject(scope.cx_mut(), obj) };
    ExnThrown::check(ok)
}

/// Get the testing functions object.
///
/// Returns `None` only on OOM.
pub fn get_testing_functions(scope: &Scope<'_>) -> Option<NonNull<JSObject>> {
    NonNull::new(unsafe { wrappers2::GetTestingFunctions(scope.cx_mut()) })
}

/// Set the profiling stack for the context.
///
/// # Safety
///
/// `profiling_stack` must be a valid pointer that outlives the context or
/// until it is replaced.
pub unsafe fn set_context_profiling_stack(scope: &Scope<'_>, profiling_stack: *mut ProfilingStack) {
    wrappers2::SetContextProfilingStack(scope.cx(), profiling_stack)
}

/// Enable or disable the context profiling stack.
pub fn enable_context_profiling_stack(scope: &Scope<'_>, enabled: bool) {
    unsafe { wrappers2::EnableContextProfilingStack(scope.cx(), enabled) }
}

/// Define the profiling functions on a global object.
pub fn define_profiling_functions(scope: &Scope<'_>, obj: HandleObject) -> Result<(), ExnThrown> {
    let ok = unsafe { wrappers2::JS_DefineProfilingFunctions(scope.cx(), obj) };
    ExnThrown::check(ok)
}

/// Realm options for a global that holds a `Debugger`. The realm gets a new compartment in the
/// system zone, and no `Debugger` can add it as a debuggee, including one created in it.
pub fn debugger_realm_options() -> crate::engine::RealmOptions {
    let mut options = crate::engine::RealmOptions::default();
    options.creationOptions_.compSpec_ =
        mozjs::jsapi::JS::CompartmentSpecifier::NewCompartmentInSystemZone;
    options.creationOptions_.invisibleToDebugger_ = true;
    options
}
