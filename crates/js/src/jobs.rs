// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Job queue (microtask) management.
//!
//! SpiderMonkey requires a job queue to execute promise continuations and other
//! microtasks. This module provides wrappers for configuring and draining the
//! queue.
//!
//! # Quick Start
//!
//! ```ignore
//! // Use SpiderMonkey's built-in job queue (simplest option):
//! jobs::use_internal_job_queues(cx)?;
//!
//! // After evaluating JS that creates promises, drain the queue:
//! jobs::run_jobs(cx);
//! ```

use std::cell::{Cell, RefCell};

use crate::async_context::Snapshot;
use crate::gc::handle::RootedHeap;
use crate::gc::scope::{RootScope, Scope};
use mozjs::jsapi::{JSContext as RawJSContext, PromiseRejectionHandlingState};
use mozjs::rust::wrappers2;
use mozjs_sys::jsval::ObjectValue;

use super::error::ExnThrown;

crate::instance_local! {
    /// Promises rejected while no handler was attached, with the async context current when they
    /// were rejected, in rejection order, until [`take_unhandled_rejections`] takes them. A promise
    /// that gets a handler first is removed.
    static UNHANDLED_REJECTIONS: RefCell<Vec<(RootedHeap<crate::promise::Promise>, Snapshot)>> =
        const { RefCell::new(Vec::new()) };

    /// Jobs [`hold_jobs_queued_during`] took out of the job queue, in queue order, until
    /// [`run_jobs`] enqueues them again.
    static HELD_JOBS: RefCell<Vec<RootedHeap<crate::object::Object>>> =
        const { RefCell::new(Vec::new()) };

    /// Set while [`hold_jobs_queued_during`] runs its closure.
    static HOLDING: Cell<bool> = const { Cell::new(false) };

    /// The queue [`set_tick_queue`] installed.
    static TICK_QUEUE: Cell<Option<TickQueue>> = const { Cell::new(None) };
}

/// A queue of callbacks that [`run_jobs`] drains ahead of the microtask queue, such as Node.js's
/// `process.nextTick` queue.
#[derive(Clone, Copy)]
pub struct TickQueue {
    /// Runs every queued callback, including callbacks queued while it runs, and reports the
    /// exceptions they throw.
    pub run: fn(&Scope<'_>),
    /// Whether `run` would run a callback.
    pub is_pending: fn(&Scope<'_>) -> bool,
}

/// Install `queue` as the tick queue, replacing a queue installed before.
pub fn set_tick_queue(queue: TickQueue) {
    TICK_QUEUE.with(|cell| cell.set(Some(queue)));
}

/// Whether the tick queue would run a callback.
fn has_pending_ticks(scope: &Scope<'_>) -> bool {
    TICK_QUEUE
        .with(Cell::get)
        .is_some_and(|queue| (queue.is_pending)(scope))
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
/// fulfillment reaction on a resolved promise. It is therefore called with one
/// argument, that promise's `undefined` result. An exception it throws rejects
/// a promise the engine allocates for the reaction, which counts as an
/// unhandled rejection. Observing that requires the embedding to install a
/// promise rejection tracker.
///
/// With [`crate::async_context::install_job_queue`]'s queue, the promise is a
/// new one resolved after `job` is added, so that the reaction captures the
/// current async context. The engine doesn't capture it for reactions to
/// settled promises.
pub fn queue_microtask(scope: &Scope<'_>, job: &crate::Function<'_>) -> Result<(), ExnThrown> {
    if crate::async_context::is_installed(scope) {
        let promise = crate::Promise::new_pending(scope)?;
        promise.add_reactions(scope, Some(**job), None)?;
        return promise.resolve(scope, scope.root_value(mozjs::jsval::UndefinedValue()));
    }
    let resolved = crate::Promise::shared_resolved_undefined(scope)?;
    resolved.add_reactions(scope, Some(**job), None)
}

/// Drain the tick queue and the job queue, executing all pending callbacks and microtasks.
///
/// The tick queue, if one is installed, runs first. Then all queued promise
/// reactions and other microtasks run until the job queue is empty. Ticks
/// queued by microtasks run after that, followed by the microtasks they
/// queue, until both queues are empty.
///
/// Jobs held by [`hold_jobs_queued_during`] are enqueued behind the jobs
/// already queued, unless its closure is running. Jobs held while the queue
/// drains are enqueued and run in the same call.
pub fn run_jobs(scope: &Scope<'_>) {
    let ticks = TICK_QUEUE.with(Cell::get);
    loop {
        if let Some(ticks) = ticks {
            (ticks.run)(scope);
        }
        run_microtasks(scope);
        if crate::async_context::INTERRUPTED.get() || !has_pending_ticks(scope) {
            return;
        }
    }
}

/// Run the job queue's microtasks and the held jobs until none are left, without running the tick
/// queue.
pub fn run_microtasks(scope: &Scope<'_>) {
    loop {
        let stuck = !enqueue_held_jobs(scope);
        unsafe { wrappers2::RunJobs(scope.cx_mut()) }
        if stuck || !has_held_jobs() {
            return;
        }
    }
}

/// Returns whether [`run_jobs`] would run anything: a tick is queued, a
/// promise reaction or other microtask is pending in the job queue, or a held
/// job would be enqueued.
pub fn has_pending_jobs(scope: &Scope<'_>) -> bool {
    has_pending_ticks(scope)
        || has_held_jobs()
        || unsafe { wrappers2::HasJobsPending(scope.cx_mut()) }
}

/// Whether [`enqueue_held_jobs`] would enqueue a job.
fn has_held_jobs() -> bool {
    !HOLDING.with(Cell::get) && HELD_JOBS.with(|held| !held.borrow().is_empty())
}

/// Run `f`, then take every regular microtask out of the job queue and hold
/// it until the next [`run_jobs`] outside `f`.
///
/// A debugger hook runs with a job queue of its own, which the engine drains
/// when the hook returns, while the debuggee may not run. Jobs held from that
/// queue run in the debuggee's queue instead. While `f` runs, [`run_jobs`]
/// leaves held jobs held.
pub fn hold_jobs_queued_during<R>(scope: &Scope<'_>, f: impl FnOnce() -> R) -> R {
    let holding = HOLDING.with(|holding| holding.replace(true));
    let result = f();
    HOLDING.with(|cell| cell.set(holding));
    // SAFETY: the context is valid while `scope` is.
    while unsafe { wrappers2::HasRegularMicroTasks(scope.cx()) } {
        // SAFETY: as above. The job is rooted below before anything can GC.
        let job = unsafe { wrappers2::DequeueNextRegularMicroTask(scope.cx()) };
        crate::async_context::regular_microtasks_dequeued(1);
        // The internal job queue holds only job objects.
        debug_assert!(job.is_object(), "a regular microtask is an object");
        if !job.is_object() {
            continue;
        }
        // SAFETY: `job` is a live object the queue held until now.
        if let Some(job) = unsafe { crate::Object::from_raw(scope, job.to_object()) } {
            HELD_JOBS.with(|held| held.borrow_mut().push(RootedHeap::new(job)));
        }
    }
    result
}

/// Enqueue the held jobs, in the order they were taken, unless
/// [`hold_jobs_queued_during`] is running its closure. A job the queue cannot
/// take stays held, as do the jobs after it. Returns `false` if jobs were
/// held and none of them could be enqueued.
fn enqueue_held_jobs(scope: &Scope<'_>) -> bool {
    if HOLDING.with(Cell::get) {
        return true;
    }
    HELD_JOBS.with(|held| {
        let mut held = held.borrow_mut();
        let mut enqueued = 0;
        for job in held.iter() {
            // SAFETY: the pointer is passed on before anything can GC.
            let job = ObjectValue(unsafe { job.as_ptr() });
            // SAFETY: the context is valid while `scope` is.
            if !unsafe { wrappers2::EnqueueMicroTask(scope.cx(), &job) } {
                break;
            }
            enqueued += 1;
        }
        let none_held = held.is_empty();
        held.drain(..enqueued);
        none_held || enqueued > 0
    })
}

/// Forget every held job. The held jobs are rooted, so this must run before
/// the context is destroyed.
pub fn clear_held_jobs() {
    HELD_JOBS.with(|held| held.borrow_mut().clear());
}

/// Stop draining the job queue.
///
/// After calling this, [`run_jobs`] does nothing until
/// [`crate::async_context::clear`] runs.
pub fn stop_draining(scope: &Scope<'_>) {
    crate::async_context::INTERRUPTED.set(true);
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
    // promise is rejected or gets a handler, and the check covers any other call.
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
            PromiseRejectionHandlingState::Unhandled => {
                list.push((RootedHeap::new(promise), Snapshot::capture()))
            }
            PromiseRejectionHandlingState::Handled => {
                list.retain(|(tracked, _)| *tracked != promise)
            }
        }
    });
}

/// The promises that were rejected with no handler attached since the last call, and still have
/// none, in rejection order, each with the async context current when it was rejected.
pub fn take_unhandled_rejections<'s>(scope: &'s Scope<'_>) -> Vec<(crate::Promise<'s>, Snapshot)> {
    let taken = UNHANDLED_REJECTIONS.with(|list| std::mem::take(&mut *list.borrow_mut()));
    taken
        .into_iter()
        .map(|(tracked, context)| (tracked.get(scope), context))
        .filter(|(promise, _)| promise.is_rejected() && !promise.is_handled())
        .collect()
}

/// Forget every tracked rejection. The tracked promises are rooted, so this must run before the
/// context is destroyed.
pub fn clear_unhandled_rejections() {
    UNHANDLED_REJECTIONS.with(|list| list.borrow_mut().clear());
}
