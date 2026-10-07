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
use std::collections::VecDeque;

use crate::async_context::Snapshot;
use crate::conversion::ToJSVal;
use crate::function::{Callback, CallbackArgs};
use crate::gc::handle::{Heap, RootedHeap};
use crate::gc::scope::{RootScope, Scope};
use crate::macros::{jsclass, jsmethods, Traceable};
use crate::native::Value;
use mozjs::jsapi::{JSContext as RawJSContext, PromiseRejectionHandlingState};
use mozjs::rust::wrappers2;
use mozjs::rust::HandleValue;
use mozjs_sys::jsval::ObjectValue;

use super::error::ExnThrown;

crate::instance_local! {
    /// Promises rejected while no handler was attached, with the async context current when they
    /// were rejected and their rejection ID, in rejection order, until
    /// [`take_unhandled_rejections`] takes them. A promise that gets a handler first is removed.
    static UNHANDLED_REJECTIONS: RefCell<Vec<TrackedRejection>> =
        const { RefCell::new(Vec::new()) };

    /// The number of promises rejected while no handler was attached.
    static REJECTION_COUNT: Cell<u64> = const { Cell::new(0) };

    /// A `WeakMap` from each promise [`take_unhandled_rejections`] took to its rejection ID, until
    /// the promise gets a handler. Created by [`track_unhandled_rejections`], and traced by
    /// [`trace_taken_rejections`].
    static TAKEN_REJECTIONS: RefCell<Option<Heap<crate::collections::weak_map::WeakMap>>> =
        const { RefCell::new(None) };

    /// Promises that got a handler after [`take_unhandled_rejections`] took them, with their
    /// rejection IDs, in the order they got it, until [`take_handled_rejections`] takes them.
    static HANDLED_REJECTIONS: RefCell<Vec<(RootedHeap<crate::promise::Promise>, u64)>> =
        const { RefCell::new(Vec::new()) };

    /// Jobs [`hold_jobs_queued_during`] took out of the job queue, in queue order, until
    /// [`run_jobs`] enqueues them again.
    static HELD_JOBS: RefCell<Vec<RootedHeap<crate::object::Object>>> =
        const { RefCell::new(Vec::new()) };

    /// Set while [`hold_jobs_queued_during`] runs its closure.
    static HOLDING: Cell<bool> = const { Cell::new(false) };

    /// The handler [`install_tick_queue`] installed.
    static TICK_EXCEPTION_HANDLER: Cell<Option<fn(&Scope<'_>)>> = const { Cell::new(None) };

    /// Set while [`run_ticks`] runs.
    static RUNNING_TICKS: Cell<bool> = const { Cell::new(false) };

    /// The number of ticks in the tick queues of all globals.
    static QUEUED_TICKS: Cell<usize> = const { Cell::new(0) };
}

/// The tick queue: calls that [`run_jobs`] makes ahead of the microtask queue, in the order they
/// were queued, such as Node.js's `process.nextTick` callbacks. Each global has one.
#[jsclass(hidden)]
struct TickQueue {
    ticks: VecDeque<Tick>,
}

#[jsmethods]
impl TickQueue<'_> {}

/// A queued call: of `callback` with `args`, or of `native` with `callback` as its payload and
/// `args`, with `context` as the current async context.
#[crate::must_root]
#[derive(Traceable)]
struct Tick {
    callback: Heap<Value>,
    #[no_trace]
    native: Option<Callback>,
    args: Vec<Heap<Value>>,
    #[no_trace]
    context: Snapshot,
}

/// The key of the tick queue in the global's shared objects.
static TICK_QUEUE_KEY: u8 = 0;

/// Create the tick queue of `global`, the current global, and install `handle_exception` to
/// handle the exception a tick throws, replacing a handler installed before. Without a handler,
/// the exception is reported and cleared. Ticks can only be queued once the queue exists.
pub fn install_tick_queue(
    scope: &Scope<'_>,
    global: crate::Object<'_>,
    handle_exception: fn(&Scope<'_>),
) -> Result<(), ExnThrown> {
    TickQueue::add_to_global(scope, global);
    let key = std::ptr::from_ref(&TICK_QUEUE_KEY).addr();
    crate::class::get_or_init_shared_object(scope, key, |scope| {
        let queue =
            crate::class::create_instance_with::<TickQueueImpl>(scope, |_| TickQueueImpl {
                ticks: VecDeque::new(),
            })?;
        Ok(*queue)
    })?;
    TICK_EXCEPTION_HANDLER.with(|cell| cell.set(Some(handle_exception)));
    Ok(())
}

/// The current global's tick queue, if [`install_tick_queue`] created one.
fn tick_queue<'s>(scope: &'s Scope<'_>) -> Option<TickQueue<'s>> {
    let queue = crate::class::get_shared_object(scope, std::ptr::from_ref(&TICK_QUEUE_KEY).addr())?;
    Some(
        queue
            .cast::<TickQueue>()
            .expect("the tick queue is a `TickQueue`"),
    )
}

/// Append a tick to the current global's tick queue.
fn push_tick(
    scope: &Scope<'_>,
    callback: HandleValue<'_>,
    native: Option<Callback>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    let queue = tick_queue(scope)
        .ok_or_else(|| crate::error::throw_type_error(scope, c"This global has no tick queue"))?;
    queue.data_mut().ticks.push_back(Tick {
        callback: Heap::from(callback.get()),
        native,
        args: args.iter().map(|arg| Heap::from(arg.get())).collect(),
        context: Snapshot::capture(),
    });
    QUEUED_TICKS.set(QUEUED_TICKS.get() + 1);
    Ok(())
}

/// Queue a call of `callback` with `args` and `undefined` as `this` in the tick queue, as
/// Node.js's `process.nextTick` does. The call runs with the current async context.
pub fn queue_tick(
    scope: &Scope<'_>,
    callback: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    push_tick(scope, callback, None, args)
}

/// Queue a call of `native` with `payload` and `args` in the tick queue, as [`queue_tick`] queues
/// a function. `native` gets the arguments as [`CallbackArgs::from_values`] passes them, and its
/// result is ignored.
pub fn queue_native_tick<'s>(
    scope: &'s Scope<'_>,
    native: Callback,
    payload: impl ToJSVal<'s>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    let payload = payload.to_jsval_throwing(scope)?;
    push_tick(scope, payload, Some(native), args)
}

/// Whether [`run_ticks`] would run a tick.
fn has_pending_ticks(scope: &Scope<'_>) -> bool {
    !RUNNING_TICKS.with(Cell::get)
        && QUEUED_TICKS.get() > 0
        && tick_queue(scope).is_some_and(|queue| !queue.data().ticks.is_empty())
}

/// Run the current global's ticks in order, including ticks queued while they run. An exception
/// a tick throws goes to the handler [`install_tick_queue`] installed, and the ticks after
/// it still run. Nested calls do nothing.
fn run_ticks(scope: &Scope<'_>) {
    if !has_pending_ticks(scope) {
        return;
    }
    RUNNING_TICKS.with(|running| running.set(true));
    while run_next_tick(&scope.inner_scope()) {}
    RUNNING_TICKS.with(|running| running.set(false));
}

/// Take the first tick out of the tick queue and run it as a callback with its async context
/// current, which is also current while the exception it throws is handled. Returns whether
/// there was a tick.
fn run_next_tick(scope: &Scope<'_>) -> bool {
    let Some(queue) = tick_queue(scope) else {
        return false;
    };
    // The tick's values are rooted while the traced queue still holds them, and the tick is
    // dropped before anything can GC again.
    let (callback, native, args, context) = {
        let data = queue.data();
        let Some(tick) = data.ticks.front() else {
            return false;
        };
        let args: Vec<_> = tick.args.iter().map(|arg| arg.get(scope)).collect();
        (
            tick.callback.get(scope),
            tick.native,
            args,
            tick.context.clone(),
        )
    };
    drop(queue.data_mut().ticks.pop_front());
    QUEUED_TICKS.set(QUEUED_TICKS.get().saturating_sub(1));
    let previous = context.enter_callback(scope);
    let result = match native {
        Some(native) => native(scope, CallbackArgs::from_values(&args), callback).map(|_| ()),
        None => crate::Function::call(scope, HandleValue::undefined(), callback, &args).map(|_| ()),
    };
    if result.is_err() {
        handle_tick_exception(scope);
    }
    previous.leave_callback(scope);
    true
}

/// Pass the pending exception to the tick exception handler, or report and clear it.
fn handle_tick_exception(scope: &Scope<'_>) {
    match TICK_EXCEPTION_HANDLER.with(Cell::get) {
        Some(handler) => handler(scope),
        None => crate::exception::report_and_clear(scope, "tick"),
    }
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
/// With [`crate::async_context::install_job_queue`]'s queue and a current async context, the
/// promise is a new one resolved after `job` is added, so that the reaction captures the context.
/// The engine doesn't capture it for reactions to settled promises.
pub fn queue_microtask(scope: &Scope<'_>, job: &crate::Function<'_>) -> Result<(), ExnThrown> {
    if crate::async_context::has_current() && crate::async_context::is_installed(scope) {
        let promise = crate::Promise::new_pending(scope)?;
        promise.add_reactions(scope, Some(**job), None)?;
        return promise.resolve(scope, scope.root_value(mozjs::jsval::UndefinedValue()));
    }
    let resolved = crate::Promise::shared_resolved_undefined(scope)?;
    resolved.add_reactions(scope, Some(**job), None)
}

/// Drain the tick queue and the job queue, executing all pending callbacks and microtasks.
///
/// The tick queue runs first. Then all queued promise
/// reactions and other microtasks run until the job queue is empty. Ticks
/// queued by microtasks run after that, followed by the microtasks they
/// queue, until both queues are empty.
///
/// Jobs held by [`hold_jobs_queued_during`] are enqueued behind the jobs
/// already queued, unless its closure is running. Jobs held while the queue
/// drains are enqueued and run in the same call.
pub fn run_jobs(scope: &Scope<'_>) {
    loop {
        run_ticks(scope);
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

/// A promise rejected while no handler was attached, the async context current when it was
/// rejected, and its rejection ID.
type TrackedRejection = (RootedHeap<crate::promise::Promise>, Snapshot, u64);

/// Track promises that are rejected while no handler is attached, for
/// [`take_unhandled_rejections`], and those that get a handler after it took them, for
/// [`take_handled_rejections`]. Call once per context, with `scope` in the realm that holds the
/// record of the promises [`take_unhandled_rejections`] took. Promises of other compartments are
/// never reported to [`take_handled_rejections`].
pub fn track_unhandled_rejections(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let taken = crate::WeakMap::new(scope)?;
    crate::gc::add_root_tracer(trace_taken_rejections);
    TAKEN_REJECTIONS.with(|cell| *cell.borrow_mut() = Some(Heap::from(taken)));
    // SAFETY: `track_rejection` has the callback signature, and takes no data.
    unsafe {
        wrappers2::SetPromiseRejectionTrackerCallback(
            scope.cx(),
            Some(track_rejection),
            std::ptr::null_mut(),
        )
    }
    Ok(())
}

/// The record of the promises [`take_unhandled_rejections`] took, if it is in the compartment of
/// `scope`.
fn taken_rejections<'s>(scope: &'s Scope<'_>) -> Option<crate::WeakMap<'s>> {
    let taken = TAKEN_REJECTIONS.with(|cell| cell.borrow().as_ref().map(|map| map.get(scope)))?;
    let raw = taken.as_non_null()?;
    crate::compartment::is_object_in_context_compartment(raw, scope).then_some(taken)
}

/// The engine's rejection tracker: records a promise rejected with no handler, with the next
/// rejection ID. Once a handler is attached, it forgets the promise again, or records it for
/// [`take_handled_rejections`] if [`take_unhandled_rejections`] took it.
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
    if state == PromiseRejectionHandlingState::Unhandled {
        let id = REJECTION_COUNT.with(|count| count.get() + 1);
        REJECTION_COUNT.with(|count| count.set(id));
        let tracked = (RootedHeap::new(promise), Snapshot::capture(), id);
        UNHANDLED_REJECTIONS.with(|list| list.borrow_mut().push(tracked));
        return;
    }
    let pending = UNHANDLED_REJECTIONS.with(|list| {
        let mut list = list.borrow_mut();
        let tracked = list.len();
        list.retain(|(tracked, _, _)| *tracked != promise);
        list.len() < tracked
    });
    if pending {
        return;
    }
    // The engine also reports a handler for promises it never reported as rejected with no
    // handler, so only the promises recorded as taken count.
    let Some(taken) = taken_rejections(&scope) else {
        return;
    };
    let in_compartment = promise
        .as_non_null()
        .is_some_and(|raw| crate::compartment::is_object_in_context_compartment(raw, &scope));
    if !in_compartment {
        return;
    }
    let key = scope.root_value(ObjectValue(promise.as_raw()));
    let Ok(id) = taken.lookup(&scope, key) else {
        crate::exception::clear(&scope);
        return;
    };
    if !id.is_number() {
        return;
    }
    let id = id.to_number() as u64;
    if taken.insert(&scope, key, HandleValue::undefined()).is_err() {
        crate::exception::clear(&scope);
    }
    HANDLED_REJECTIONS.with(|list| list.borrow_mut().push((RootedHeap::new(promise), id)));
}

/// A promise [`take_unhandled_rejections`] returns.
pub struct UnhandledRejection<'s> {
    pub promise: crate::Promise<'s>,
    /// The async context current when the promise was rejected.
    pub context: Snapshot,
    /// The number of promises rejected with no handler attached up to and including this one.
    pub id: u64,
}

/// The promises that were rejected with no handler attached since the last call, and still have
/// none, in rejection order. A promise of the compartment of `scope` that gets a handler later is
/// returned by [`take_handled_rejections`].
pub fn take_unhandled_rejections<'s>(scope: &'s Scope<'_>) -> Vec<UnhandledRejection<'s>> {
    let tracked = UNHANDLED_REJECTIONS.with(|list| std::mem::take(&mut *list.borrow_mut()));
    let taken = taken_rejections(scope);
    let mut rejections = Vec::new();
    for (promise, context, id) in tracked {
        let promise = promise.get(scope);
        if !promise.is_rejected() || promise.is_handled() {
            continue;
        }
        let in_compartment = promise
            .as_non_null()
            .is_some_and(|raw| crate::compartment::is_object_in_context_compartment(raw, scope));
        if let Some(taken) = taken.filter(|_| in_compartment) {
            let key = scope.root_value(ObjectValue(promise.as_raw()));
            let value = scope.root_value(mozjs::jsval::DoubleValue(id as f64));
            if taken.insert(scope, key, value).is_err() {
                crate::exception::clear(scope);
            }
        }
        rejections.push(UnhandledRejection {
            promise,
            context,
            id,
        });
    }
    rejections
}

/// The promises that got a handler after [`take_unhandled_rejections`] took them, since the last
/// call, in the order they got it, each with its rejection ID.
pub fn take_handled_rejections<'s>(scope: &'s Scope<'_>) -> Vec<(crate::Promise<'s>, u64)> {
    let taken = HANDLED_REJECTIONS.with(|list| std::mem::take(&mut *list.borrow_mut()));
    taken
        .into_iter()
        .map(|(promise, id)| (promise.get(scope), id))
        .collect()
}

/// Trace the `WeakMap` of [`TAKEN_REJECTIONS`].
///
/// # Safety
///
/// `trc` must be the tracer the root tracers are called with.
unsafe fn trace_taken_rejections(trc: *mut crate::native::JSTracer) {
    // GC runs with JS execution paused, so a borrow held by interrupted code isn't in use.
    TAKEN_REJECTIONS.with(|cell| {
        if let Some(taken) = unsafe { &*cell.as_ptr() } {
            unsafe { crate::heap::Trace::trace(taken, trc) };
        }
    });
}

/// Forget every tracked rejection. The tracked promises are rooted, so this must run before the
/// context is destroyed.
pub fn clear_unhandled_rejections() {
    UNHANDLED_REJECTIONS.with(|list| list.borrow_mut().clear());
    HANDLED_REJECTIONS.with(|list| list.borrow_mut().clear());
    TAKEN_REJECTIONS.with(|cell| *cell.borrow_mut() = None);
}
