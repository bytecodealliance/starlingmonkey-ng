// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Job queue (microtask) management.
//!
//! SpiderMonkey requires a job queue to execute promise continuations and other
//! microtasks. This module provides wrappers for configuring and draining the
//! queue.

use std::cell::RefCell;

use crate::gc::handle::RootedHeap;
use crate::gc::scope::{RootScope, Scope};
use mozjs::jsapi::{JSContext as RawJSContext, PromiseRejectionHandlingState};
use mozjs::rust::wrappers2;

use super::error::ExnThrown;

crate::instance_local! {
    /// Promises rejected while no handler was attached, in rejection order, until
    /// [`take_unhandled_rejections`] takes them. A promise that gets a handler first is removed.
    static UNHANDLED_REJECTIONS: RefCell<Vec<RootedHeap<crate::promise::Promise>>> =
        const { RefCell::new(Vec::new()) };
}

/// Enable SpiderMonkey's built-in internal job queue.
///
/// This is the simplest way to get Promise resolution working. Must be called
/// before any promises are created. Call [`run_jobs`] after evaluation to
/// drain the queue.
pub fn use_internal_job_queues(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let ok = unsafe { wrappers2::UseInternalJobQueues(scope.cx()) };
    ExnThrown::check(ok)
}

/// Enqueue `job` on the job queue: it runs as its own microtask, in the same
/// FIFO the engine uses for promise reactions.
///
/// The queue holds only engine-internal job records, so `job` is enqueued as a
/// fulfillment reaction on an already-resolved promise. It is therefore called
/// with one argument, that promise's `undefined` result. An exception it throws
/// rejects a promise the engine allocates for the reaction, which counts as an
/// unhandled rejection. Observing that requires the embedding to install a
/// promise rejection tracker.
pub fn queue_microtask(scope: &Scope<'_>, job: &crate::Function<'_>) -> Result<(), ExnThrown> {
    let resolved = crate::Promise::shared_resolved_undefined(scope)?;
    resolved.add_reactions(scope, Some(**job), None)
}

/// Drain the job queue, executing all pending microtasks.
///
/// This runs all queued promise reactions and other microtasks until the
/// queue is empty.
pub fn run_jobs(scope: &Scope<'_>) {
    unsafe { wrappers2::RunJobs(scope.cx_mut()) }
}

/// Returns whether any promise reactions or other microtasks are currently
/// pending in the job queue.
pub fn has_pending_jobs(scope: &Scope<'_>) -> bool {
    unsafe { wrappers2::HasJobsPending(scope.cx_mut()) }
}

/// Stop draining the job queue.
///
/// After calling this, [`run_jobs`] becomes a no-op until the queue is
/// re-enabled.
pub fn stop_draining(scope: &Scope<'_>) {
    unsafe { wrappers2::StopDrainingJobQueue(scope.cx()) }
}

/// Clear the weak references kept alive for the current turn.
///
/// This implements the host hook for WeakRef liveness semantics. Should be
/// called between "turns" (e.g. between event loop iterations).
pub fn clear_kept_objects(scope: &Scope<'_>) {
    unsafe { wrappers2::ClearKeptObjects(scope.cx()) }
}

/// Track promises that are rejected while no handler is attached, for
/// [`take_unhandled_rejections`]. Call once per context.
pub fn track_unhandled_rejections(scope: &Scope<'_>) {
    // SAFETY: `track_rejection` has the callback signature, and takes no data.
    unsafe {
        wrappers2::SetPromiseRejectionTrackerCallback(
            scope.cx(),
            Some(track_rejection),
            std::ptr::null_mut(),
        )
    }
}

/// The engine's rejection tracker: records a promise rejected with no handler, and forgets it
/// again once a handler is attached.
unsafe extern "C" fn track_rejection(
    cx: *mut RawJSContext,
    _muted_errors: bool,
    promise: mozjs::jsapi::HandleObject,
    state: PromiseRejectionHandlingState,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: the engine calls the tracker with a valid context. A realm is entered whenever a
    // promise is rejected or gets a handler. The check covers any other call.
    if unsafe { mozjs::jsapi::GetCurrentRealmOrNull(cx) }.is_null() {
        return;
    }
    // SAFETY: a realm is entered, as checked above.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    // SAFETY: the engine passes a rooted handle to a live promise.
    let Some(promise) = (unsafe { crate::Object::from_raw(&scope, promise.get()) })
        .and_then(|object| object.cast::<crate::Promise>().ok())
    else {
        return;
    };
    UNHANDLED_REJECTIONS.with(|list| {
        let mut list = list.borrow_mut();
        match state {
            PromiseRejectionHandlingState::Unhandled => list.push(RootedHeap::new(promise)),
            PromiseRejectionHandlingState::Handled => list.retain(|tracked| *tracked != promise),
        }
    });
}

/// The promises that were rejected with no handler attached since the last call, and still have
/// none, in rejection order.
pub fn take_unhandled_rejections<'s>(scope: &'s Scope<'_>) -> Vec<crate::Promise<'s>> {
    let taken = UNHANDLED_REJECTIONS.with(|list| std::mem::take(&mut *list.borrow_mut()));
    taken
        .iter()
        .map(|tracked| tracked.get(scope))
        .filter(|promise| promise.is_rejected() && !promise.is_handled())
        .collect()
}

/// Forget every tracked rejection. The tracked promises are rooted, so this must run before the
/// context is destroyed.
pub fn clear_unhandled_rejections() {
    UNHANDLED_REJECTIONS.with(|list| list.borrow_mut().clear());
}
