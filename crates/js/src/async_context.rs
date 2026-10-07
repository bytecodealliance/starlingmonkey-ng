// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The async context: an object that promise reactions and event-loop tasks capture when they are
//! created, and that is current again while they run.
//!
//! Promise reactions capture the context once [`install_job_queue`] has replaced the engine's job
//! queue. Code that queues other work, such as an event-loop task, captures it with
//! [`Snapshot::capture`] and makes it current around running that work with [`Snapshot::enter`].

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ptr::{self, NonNull};
use std::rc::Rc;

use mozjs::glue::{CreateJobQueue, JS_DequeueNextMicroTask, JobQueueTraps};
use mozjs::jsapi::{self, JSAutoRealm, JSContext as RawJSContext, JSObject, JSTracer};
use mozjs::jsval::UndefinedValue;

use crate::error::ExnThrown;
use crate::gc::handle::RootedHeap;
use crate::gc::scope::{RootScope, Scope};
use crate::Object;

crate::instance_local! {
    /// The current async context, if one is set.
    static CURRENT: RefCell<Option<Rc<RootedHeap<crate::object::Object>>>> = const { RefCell::new(None) };

    /// The engine's internal job queue, which [`install_job_queue`] replaced.
    static INTERNAL_QUEUE: Cell<*mut jsapi::JS::JobQueue> = const { Cell::new(ptr::null_mut()) };

    /// The job queue [`install_job_queue`] created.
    static QUEUE: Cell<*mut jsapi::JS::JobQueue> = const { Cell::new(ptr::null_mut()) };

    /// The position of the regular microtask at the head of the job queue: the number of regular
    /// microtasks dequeued before it.
    static HEAD: Cell<u64> = const { Cell::new(0) };

    /// The async contexts current when jobs that capture no data were queued, by the position they
    /// were queued at, in position order. See [`get_host_defined_global`].
    static TAGS: RefCell<VecDeque<(u64, Snapshot)>> = const { RefCell::new(VecDeque::new()) };

    /// Set while [`run_jobs`] drains the job queue.
    static DRAINING: Cell<bool> = const { Cell::new(false) };

    /// Set by [`crate::jobs::stop_draining`]. [`run_jobs`] does nothing while it is set.
    pub(crate) static INTERRUPTED: Cell<bool> = const { Cell::new(false) };

    /// The context of the last job [`run_next_job`] ran with a context it captured, which the
    /// next job that captured the same object reuses.
    static LAST_JOB_CONTEXT: RefCell<Option<Rc<RootedHeap<crate::object::Object>>>> =
        const { RefCell::new(None) };

    /// The hooks [`set_callback_hooks`] installed.
    static CALLBACK_HOOKS: Cell<Option<CallbackHooks>> = const { Cell::new(None) };
}

/// Functions called around each callback [`Snapshot::enter_callback`] runs with the async context
/// it captured: promise jobs, ticks, event-loop tasks and the settling of spawned futures. The
/// hooks must not run JavaScript.
#[derive(Clone, Copy)]
pub struct CallbackHooks {
    /// Called once the callback's context is current, before the callback runs.
    pub before: fn(&Scope<'_>),
    /// Called after the callback ran and an exception it threw was handled, once the context the
    /// callback's context replaced is current again.
    pub after: fn(&Scope<'_>),
}

/// Install `hooks` as the [`CallbackHooks`], replacing the hooks installed before, or remove them
/// if `hooks` is `None`.
pub fn set_callback_hooks(hooks: Option<CallbackHooks>) {
    CALLBACK_HOOKS.set(hooks);
}

/// Run `f` inside a running callback with the callback's `after` hook called before `f` and its
/// `before` hook called again after `f`.
pub fn outside_callback<R>(scope: &Scope<'_>, f: impl FnOnce() -> R) -> R {
    let hooks = CALLBACK_HOOKS.get();
    if let Some(hooks) = hooks {
        call_hook(scope, hooks.after);
    }
    let result = f();
    if let Some(hooks) = hooks {
        call_hook(scope, hooks.before);
    }
    result
}

/// Call `hook` with the pending exception, if there is one, cleared, and make it pending again
/// afterwards.
fn call_hook(scope: &Scope<'_>, hook: fn(&Scope<'_>)) {
    let pending = crate::exception::take_pending(scope);
    hook(scope);
    if let Some(error) = pending {
        crate::exception::throw_value(scope, error);
    }
}

/// A captured async context, or the absence of one.
#[derive(Clone, Default)]
pub struct Snapshot(Option<Rc<RootedHeap<crate::object::Object>>>);

impl Snapshot {
    /// The current async context.
    pub fn capture() -> Self {
        CURRENT.with(|current| Snapshot(current.borrow().clone()))
    }

    /// A snapshot holding `context`, or no context if it is `None`.
    pub fn of(context: Option<Object<'_>>) -> Self {
        Snapshot(context.map(|context| Rc::new(RootedHeap::new(context))))
    }

    /// The context this snapshot holds.
    pub fn get<'s>(&self, scope: &'s Scope<'_>) -> Option<Object<'s>> {
        self.0.as_ref().map(|context| context.get(scope))
    }

    /// Make this snapshot's context current, and return a snapshot of the context it replaced.
    /// Entering the returned snapshot restores that context.
    pub fn enter(self) -> Snapshot {
        CURRENT.with(|current| Snapshot(current.replace(self.0)))
    }

    /// Make this snapshot's context current to run a callback that captured it, and call the
    /// `before` hook. Returns a snapshot of the context it replaced, for
    /// [`Snapshot::leave_callback`].
    pub fn enter_callback(self, scope: &Scope<'_>) -> Snapshot {
        let previous = self.enter();
        if let Some(hooks) = CALLBACK_HOOKS.get() {
            call_hook(scope, hooks.before);
        }
        previous
    }

    /// Make this snapshot's context current again, then call the `after` hook for the callback
    /// [`Snapshot::enter_callback`] ran.
    pub fn leave_callback(self, scope: &Scope<'_>) {
        self.enter();
        if let Some(hooks) = CALLBACK_HOOKS.get() {
            call_hook(scope, hooks.after);
        }
    }
}

/// The current async context, if one is set.
pub fn current<'s>(scope: &'s Scope<'_>) -> Option<Object<'s>> {
    Snapshot::capture().get(scope)
}

/// Whether an async context is current.
pub fn has_current() -> bool {
    CURRENT.with(|current| current.borrow().is_some())
}

/// Replace the engine's job queue with one that runs each promise job with the async context it
/// captured current. A reaction to a pending promise captures the context current when it is
/// added, and every other job the context current when it is queued.
///
/// Call before any promise reaction is created. Calls for a context that has the queue already do
/// nothing.
///
/// Note: jobs that off-thread work queues, such as the reactions to `WebAssembly.compile`'s
/// promise, run without their captured context, as do the reactions those jobs create.
pub fn install_job_queue(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    static TRAPS: JobQueueTraps = JobQueueTraps {
        getHostDefinedData: Some(get_host_defined_data),
        getHostDefinedGlobal: Some(get_host_defined_global),
        runJobs: Some(run_jobs),
        traceNonGCThingMicroTask: Some(trace_non_gc_thing_micro_task),
    };
    // SAFETY: the context is valid while `scope` is.
    let cx = unsafe { scope.raw_cx_no_gc() };
    // SAFETY: as above.
    let internal = unsafe { jsapi::JS::GetJobQueue(cx) };
    if internal == QUEUE.get() {
        return Ok(());
    }
    // The internal queue stays alive with the context, which owns it.
    INTERNAL_QUEUE.set(internal);
    // SAFETY: `TRAPS` lives for the program. The queue is never freed.
    let queue = unsafe { CreateJobQueue(&TRAPS) };
    if queue.is_null() {
        crate::exception::report_out_of_memory(scope);
        return Err(ExnThrown);
    }
    QUEUE.set(queue.cast());
    // SAFETY: as above. `RustJobQueue` derives from `JS::JobQueue`.
    unsafe { jsapi::JS::SetJobQueue(cx, queue.cast()) };
    Ok(())
}

/// Whether the context has the job queue [`install_job_queue`] creates.
pub fn is_installed(scope: &Scope<'_>) -> bool {
    // SAFETY: the context is valid while `scope` is.
    let queue = unsafe { jsapi::JS::GetJobQueue(scope.raw_cx_no_gc()) };
    !queue.is_null() && queue == QUEUE.get()
}

/// Forget the current async context, the recorded contexts and the [`CallbackHooks`], and clear
/// the flag [`crate::jobs::stop_draining`] sets. The contexts are rooted, so this must run before
/// the context is destroyed.
pub fn clear() {
    CALLBACK_HOOKS.set(None);
    CURRENT.with(|current| current.borrow_mut().take());
    LAST_JOB_CONTEXT.with(|last| last.borrow_mut().take());
    TAGS.with(|tags| tags.borrow_mut().clear());
    INTERRUPTED.set(false);
}

/// Record that `count` regular microtasks were dequeued outside [`run_jobs`].
pub(crate) fn regular_microtasks_dequeued(count: u64) {
    HEAD.set(HEAD.get() + count);
}

/// The context recorded for the job at `position`, dropping it and the contexts recorded for
/// earlier positions.
fn take_tag(position: u64) -> Option<Snapshot> {
    TAGS.with(|tags| {
        let mut tags = tags.borrow_mut();
        while let Some(&(tagged, _)) = tags.front() {
            if tagged > position {
                return None;
            }
            let (tagged, snapshot) = tags.pop_front().expect("checked above");
            if tagged == position {
                return Some(snapshot);
            }
        }
        None
    })
}

/// The engine's hook for the data a new promise reaction keeps: the current global, and the current
/// async context, or the current global again if none is set, so that a job without data is one
/// that captured nothing. Without a current global, the reaction keeps neither, since the engine
/// discards data that has no global.
unsafe extern "C" fn get_host_defined_data(
    cx: *mut RawJSContext,
    incumbent_global: jsapi::JS::MutableHandle<*mut JSObject>,
    host_defined_data: jsapi::JS::MutableHandle<*mut JSObject>,
) -> bool {
    // SAFETY: the engine calls the hook with a valid context.
    let global = unsafe { jsapi::CurrentGlobalOrNull(cx) };
    let context = if global.is_null() {
        ptr::null_mut()
    } else {
        // SAFETY: the pointer is passed to the engine, which roots it, before anything can GC.
        CURRENT.with(|current| match current.borrow().as_ref() {
            Some(context) => unsafe { context.as_ptr() },
            None => global,
        })
    };
    // SAFETY: the engine passes handles to valid locations.
    unsafe {
        *incumbent_global.ptr = global;
        *host_defined_data.ptr = context;
    }
    true
}

/// The engine's hook for the global of a job that captures no data. The engine calls it while
/// queueing a reaction to a settled promise or a thenable job, which it queues before returning,
/// so the hook records the current async context for the position at the end of the queue.
///
/// `FinalizationRegistry` also calls it, without queueing a job. The context it records is replaced
/// by the next call for the same position, and dropped when a job that captured data is dequeued
/// from that position.
unsafe extern "C" fn get_host_defined_global(
    cx: *mut RawJSContext,
    _data: jsapi::JS::MutableHandle<*mut JSObject>,
) -> bool {
    // SAFETY: the engine calls the hook with a valid context.
    let position = HEAD.get() + unsafe { jsapi::JS::GetRegularMicroTaskCount(cx) } as u64;
    let snapshot = Snapshot::capture();
    // A job without a recorded context runs without one, so no context needs no record.
    TAGS.with(|tags| {
        let mut tags = tags.borrow_mut();
        match tags.back_mut() {
            Some((tagged, _)) if *tagged == position && snapshot.0.is_none() => {
                tags.pop_back();
            }
            Some((tagged, current)) if *tagged == position => *current = snapshot,
            _ if snapshot.0.is_none() => {}
            _ => tags.push_back((position, snapshot)),
        }
    });
    true
}

/// The engine's hook for tracing microtasks that aren't GC things, which this queue never creates.
unsafe extern "C" fn trace_non_gc_thing_micro_task(
    _trc: *mut JSTracer,
    _value: *mut jsapi::JS::Value,
) {
}

/// Drain the job queue, running each job in its realm with the async context it captured current,
/// or the one [`get_host_defined_global`] recorded for it. An exception a job throws is reported
/// and cleared.
///
/// Nested calls, and calls after [`crate::jobs::stop_draining`], do nothing. Once the queue is
/// empty, the engine's internal queue runs the jobs that off-thread work queued. The contexts
/// recorded until then are dropped, since that queue dequeues jobs this one doesn't count.
unsafe extern "C" fn run_jobs(cx: *mut RawJSContext) {
    if DRAINING.get() || INTERRUPTED.get() {
        return;
    }
    DRAINING.set(true);
    // SAFETY: the engine calls the hook with a valid context.
    while unsafe { jsapi::JS::HasAnyMicroTasks(cx) } {
        if INTERRUPTED.get() {
            break;
        }
        // SAFETY: as above.
        unsafe { run_next_job(cx) };
    }
    DRAINING.set(false);
    if INTERRUPTED.get() {
        return;
    }
    // SAFETY: as above. The internal queue is the context's own, and this queue is current while
    // its hook runs. It is current again before the hook returns, since saved job queues require
    // it.
    unsafe {
        let queue = jsapi::JS::GetJobQueue(cx);
        jsapi::JS::SetJobQueue(cx, INTERNAL_QUEUE.get());
        jsapi::js::RunJobs(cx);
        jsapi::JS::SetJobQueue(cx, queue);
    }
    TAGS.with(|tags| tags.borrow_mut().clear());
}

/// A snapshot of `captured`, the context a job captured. It shares the allocation of the last
/// snapshot this returned if that holds the same object.
fn job_context(scope: &Scope<'_>, captured: *mut JSObject) -> Snapshot {
    LAST_JOB_CONTEXT.with(|last| {
        let mut last = last.borrow_mut();
        // SAFETY: the pointer is only compared.
        if let Some(context) = last
            .as_ref()
            .filter(|context| unsafe { context.as_ptr() } == captured)
        {
            return Snapshot(Some(context.clone()));
        }
        // SAFETY: the engine stored a live object, which the caller roots.
        let snapshot = Snapshot::of(unsafe { Object::from_raw(scope, captured) });
        last.clone_from(&snapshot.0);
        snapshot
    })
}

/// Dequeue the next job and run it.
///
/// # Safety
///
/// `cx` must be a valid context whose job queue holds a job.
unsafe fn run_next_job(cx: *mut RawJSContext) {
    mozjs::rooted!(in(cx) let mut generic = UndefinedValue());
    // SAFETY: the caller guarantees a valid context with a queued job.
    let (job, regular) = unsafe {
        let before = jsapi::JS::GetRegularMicroTaskCount(cx);
        JS_DequeueNextMicroTask(cx, generic.handle_mut().into());
        let regular = jsapi::JS::GetRegularMicroTaskCount(cx) < before;
        (jsapi::JS::ToMaybeWrappedJSMicroTask(&*generic), regular)
    };
    let position = regular.then(|| {
        let position = HEAD.get();
        HEAD.set(position + 1);
        position
    });
    // SAFETY: as above. Per `JS::JobQueueIsEmpty`, the engine may skip creating promise jobs once
    // `JobQueueIsEmpty` was called.
    if !unsafe { jsapi::JS::HasAnyMicroTasks(cx) } {
        unsafe { jsapi::JS::JobQueueIsEmpty(cx) };
    }
    let Some(job) = NonNull::new(job) else {
        return;
    };
    // SAFETY: `job` is a live microtask, rooted through `generic`.
    let Some(global) =
        NonNull::new(unsafe { jsapi::JS::GetExecutionGlobalFromJSMicroTask(job.as_ptr()) })
    else {
        return;
    };
    let _realm = JSAutoRealm::new(cx, global.as_ptr());
    // SAFETY: the realm was entered above.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    let job = scope.root_object(job);
    let mut incumbent = scope.root_object_mut(ptr::null_mut());
    let mut data = scope.root_object_mut(ptr::null_mut());
    // SAFETY: `job` is a rooted microtask, and the handles are rooted locations.
    let has_data = unsafe {
        jsapi::JS::MaybeGetHostDefinedDataFromJSMicroTask(
            job.get(),
            incumbent.reborrow().into(),
            data.reborrow().into(),
        )
    };
    let captured = if has_data {
        data.get()
    } else {
        ptr::null_mut()
    };
    let tagged = position.and_then(take_tag);
    let snapshot = if captured.is_null() {
        tagged.unwrap_or_default()
    } else if captured == incumbent.get() {
        Snapshot::default()
    } else {
        job_context(&scope, captured)
    };
    let previous = snapshot.enter_callback(&scope);
    // SAFETY: `job` is a rooted microtask, and its realm is entered.
    let ok = unsafe { jsapi::JS::RunJSMicroTask(cx, job.into()) };
    if !ok {
        crate::exception::report_and_clear(&scope, "microtask");
    }
    previous.leave_callback(&scope);
}
