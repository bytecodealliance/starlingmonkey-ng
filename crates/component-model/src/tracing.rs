// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The crate's single extra-GC-roots tracer.
//!
//! Several parts of the interpreter hold JS values that the object graph does
//! not reach: the per-call value stacks' borrows, the exported-resource slab and
//! the finalization helpers, the main module record, the stream and future
//! element buffers, and the shared resource drop callback. SpiderMonkey walks
//! every registered extra-roots callback on every major collection, so all of
//! them are traced from one callback registered here rather than one per module.
//!
//! The values on the call stacks are stored without write barriers, so they are
//! traced by a root tracer that also runs in minor collections instead.
//!
//! [`install_tracer`] must run before any of those values can outlive a
//! collection. [`crate::init::initialize_runtime`] calls it first thing, and
//! native tests call it themselves.

use std::sync::Once;

use js::gc::scope::Scope;
use js::native::JSTracer;

static TRACER_INSTALLED: Once = Once::new();

/// Install the crate's extra-GC-roots tracer, and register the call-stack value
/// tracer with the current runtime's root tracers.
///
/// The extra-roots registration happens once, keyed to the first caller's `cx`.
/// The interpreter runs on a single context for the process lifetime.
pub fn install_tracer(scope: &Scope<'_>) {
    js::gc::add_root_tracer(crate::stack::trace_stack_values);
    TRACER_INSTALLED.call_once(|| {
        // SAFETY: `trace_all` is a plain `extern "C"` function with no captured
        // state, valid for the entire process lifetime. The `data` pointer is
        // unused, and the registration is never removed.
        unsafe {
            js::gc::add_extra_gc_roots_tracer(
                scope.cx_mut(),
                Some(trace_all),
                std::ptr::null_mut(),
            );
        }
    });
}

/// Trace every value the crate roots outside the JS object graph.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC.
unsafe extern "C" fn trace_all(trc: *mut JSTracer, _data: *mut std::os::raw::c_void) {
    // Each callee reads its thread-local through `as_ptr`, bypassing `RefCell`
    // borrow tracking: GC runs with JS execution paused, so a borrow held by
    // interrupted code is not actively used while these cells are read.
    unsafe {
        crate::stack::trace_live_stacks(trc);
        crate::resources::trace_resources(trc);
        crate::init::trace_main_module(trc);
        crate::exports::trace_export_table(trc);
        crate::component_error::trace_error_classes(trc);
        #[cfg(target_arch = "wasm32")]
        {
            crate::streams::trace_live_elems(trc);
            crate::interpreter::trace_drop_callback(trc);
        }
    }
}

/// Drop every JS value the crate roots, leaving the tracer registered and
/// walking empty state.
///
/// A runtime must not be torn down while these hold values from it: the
/// teardown's final root trace would walk pointers into the dying heap. Called
/// on [`crate::init::initialize_runtime`]'s failure paths, which drop the
/// runtime they just created.
pub(crate) fn clear_traced_roots() {
    crate::resources::clear_traced_roots();
    crate::init::clear_traced_roots();
    crate::exports::clear_traced_roots();
    crate::component_error::clear_traced_roots();
    #[cfg(target_arch = "wasm32")]
    {
        crate::interpreter::clear_traced_roots();
        crate::streams::clear_traced_roots();
    }
}
