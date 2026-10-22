// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Running `FinalizationRegistry` cleanup callbacks.
//!
//! SpiderMonkey collects a registry's dead targets but runs none of its
//! callbacks itself. It hands the host a `doCleanup` function per registry with
//! work pending, and calling that function runs the callbacks. [`install`]
//! registers the hook that receives those functions, and [`run_pending`] calls
//! them.
//!
//! The hook runs inside a collection, so it only pushes `do_cleanup` onto the
//! [`FinalizationState`] queue and returns. The queued functions are traced until they
//! run, since nothing else keeps them alive.

use js::function::EmptyArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::{JSContext, JSFunction, JSObject, JSTracer, Value};
use js::prelude::HandleValue;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::raw::c_void;

/// The queue of `doCleanup` functions, in the order the hook received them.
///
/// A newtype so the traced `Heap`s live behind one place the GC analysis is told
/// about, rather than in a bare `Vec` at every use site.
#[js::must_root]
#[derive(Default)]
struct PendingCleanups(VecDeque<Heap<Value>>);

/// A runtime's queued `FinalizationRegistry` cleanups, held as a field of
/// [`Runtime`](crate::Runtime) so they drop with it rather than with a
/// thread-local of their own.
#[js::must_root]
#[derive(Default)]
pub struct FinalizationState {
    /// `doCleanup` functions still to be called, drained by [`run_pending`].
    pending: RefCell<PendingCleanups>,
}

/// Run `f` against the current runtime's queue.
///
/// The `FinalizationRegistry` hook SpiderMonkey calls takes no data pointer, so
/// it reaches the runtime through [`crate::runtime::current`].
fn with_pending<R>(f: impl FnOnce(&RefCell<PendingCleanups>) -> R) -> R {
    let rt = crate::runtime::current().expect("finalization queue used outside a runtime");
    f(&rt.finalization.pending)
}

/// Install the `FinalizationRegistry` cleanup hook and the tracer for the queued
/// `doCleanup` functions. Call once per runtime.
pub fn install(cx: &mut JSContext) {
    // SAFETY: both callbacks are plain `extern "C"` functions with no captured
    // state, valid for the process lifetime. Neither reads `data`.
    unsafe {
        js::gc::add_extra_gc_roots_tracer(cx, Some(trace_pending), std::ptr::null_mut());
        js::gc::set_host_cleanup_finalization_registry_callback(
            cx,
            Some(queue_cleanup),
            std::ptr::null_mut(),
        );
    }
}

/// Call every `doCleanup` function queued since the last drain, which runs the
/// `FinalizationRegistry` callbacks for the targets collected so far.
///
/// A callback that throws leaves the remaining ones to run: its exception is
/// reported and cleared.
pub fn run_pending(scope: &Scope<'_>) {
    // Take the functions one at a time, so the undrained functions stay in the
    // traced queue through any GC a callback triggers.
    while let Some(fval) =
        with_pending(|cell| cell.borrow_mut().0.pop_front().map(|h| h.get(scope)))
    {
        if js::Function::call(scope, HandleValue::undefined(), fval, EmptyArgs).is_err() {
            js::exception::report_and_clear(scope, "FinalizationRegistry cleanup");
        }
    }
}

/// Whether any `doCleanup` function is waiting to run.
pub fn has_pending() -> bool {
    with_pending(|cell| !cell.borrow().0.is_empty())
}

/// Drop every queued `doCleanup` function without calling it.
///
/// Tests share one engine across runtimes, so one runtime's queue must not carry
/// into the next.
pub(crate) fn clear(state: &FinalizationState) {
    state.pending.borrow_mut().0.clear();
}

/// The host hook: record `do_cleanup` for [`run_pending`] to call.
///
/// `incumbent_global` is the realm the callbacks should run in. The runtime this
/// serves has a single realm entered for the process, so it is not read.
///
/// # Safety
///
/// Called by SpiderMonkey from inside a collection, with a live `JSFunction`.
#[js::allow_unrooted]
unsafe extern "C" fn queue_cleanup(
    do_cleanup: *mut JSFunction,
    _incumbent_global: *mut JSObject,
    _data: *mut c_void,
) {
    // A `JSFunction` is a `JSObject`. Building the value and storing it writes
    // only the pointer and its GC barrier, which the hook's no-GC contract
    // allows.
    // SAFETY: the engine passes a live, non-null `JSFunction`, and the value is
    // stored into a traced `Heap` before anything can collect.
    let value = js::value::from_raw_function(do_cleanup);
    with_pending(|cell| cell.borrow_mut().0.push_back(Heap::from(value)));
}

/// Extra-GC-roots tracer for the queued `doCleanup` functions.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC.
#[js::allow_unrooted]
unsafe extern "C" fn trace_pending(trc: *mut JSTracer, _data: *mut c_void) {
    // This registration is never removed, so the tracer still runs while the
    // runtime's context is being destroyed, after `Runtime::drop` has cleared the
    // queue and released the runtime. There is nothing left to trace then.
    let Some(rt) = crate::runtime::current() else {
        return;
    };
    // `as_ptr` bypasses `RefCell` borrow tracking: GC runs with JS execution
    // paused, so a borrow held by interrupted code is not actively used while the
    // queue is read here.
    for heap in &(*rt.finalization.pending.as_ptr()).0 {
        heap.trace(trc);
    }
}
