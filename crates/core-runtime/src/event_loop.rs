// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Event loop for the StarlingMonkey runtime.
//!
//! The event loop manages asynchronous tasks (timers, I/O completions, promise
//! resolutions, etc.) and drives them to completion. It is designed around three
//! layered abstractions:
//!
//! 1. **[`Task`] trait** — extensible interface that builtins implement. The
//!    event loop knows nothing about specific task types; it just stores trait
//!    objects and runs them when ready.
//!
//! 2. **[`EventLoop`] struct** — platform-independent task registry. Manages
//!    queuing, cancellation, readiness signaling, timer advancement, GC
//!    tracing, and notification of idle drivers via [`event_listener::Event`].
//!
//! 3. **[`run_to_completion`]** — a single `async fn` driver that calls
//!    [`EventLoop::step`] in a loop and, when idle, races the next timer
//!    deadline against a readiness notification. Platform differences are
//!    confined to the sleep function and the executor:
//!    - `wasi`: `wasi:clocks/monotonic-clock.wait-for` for sleep,
//!      `wasip3::wit_bindgen::spawn` as executor.
//!    - `native`: the caller provides a proper async sleep (e.g.
//!      `tokio::time::sleep`) and executor (e.g.
//!      `tokio::runtime::Runtime::block_on`). This keeps the core runtime
//!      free of any specific async runtime dependency.
//!
//! # Task lifecycle
//!
//! ```text
//! queue(task) → Queued → signal_ready(id) → Ready → step() → run(scope)
//!                 │                                              │
//!                 └── cancel(id) ───────────────────── dropped ◄─┘
//! ```
//!
//! Tasks start in the **Queued** state. External events (timer expiry, I/O
//! completion, a future resolving) move them to **Ready** via
//! [`EventLoop::signal_ready`]. Each [`EventLoop::step`] runs the batch of tasks that are ready
//! at its start. Alternatively, embedders can use [`EventLoop::pop_ready`] to drive the loop by
//! hand.
//!
//! Running consumes the task, repeating behaviors like
//! `setInterval` re-queue themselves inside `run()`.
//!
//! # GC integration
//!
//! Tasks that hold references to GC-managed JS objects must trace them in
//! [`Task::trace`]. The [`EventLoop`] is registered as a SpiderMonkey
//! extra-roots tracer so that all live tasks are traced during both minor
//! and major GC.

pub mod interest;
pub mod timer;

use platform::clock::Instant;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::time::Duration;

use event_listener::{Event, EventListener};
use js::error::ExnThrown;
use js::native::JSTracer;

use js::gc::scope::Scope;
use js::jobs;

pub use interest::{InterestHandle, InterestTracker};

// ---------------------------------------------------------------------------
// Current event loop pointer
// ---------------------------------------------------------------------------

js::instance_local! {
    /// Pointer to the active [`EventLoop`], set by [`with_event_loop`] for the
    /// duration of its closure. `JSNative` callbacks (`setTimeout` etc.) have
    /// no other way to reach Rust state, so they read this pointer via
    /// [`with_active_event_loop`].
    ///
    /// The pointee is only ever accessed through shared references, so re-entrant access from JS
    /// callbacks aliases nothing.
    static CURRENT_EVENT_LOOP: Cell<Option<*const EventLoop>> = const { Cell::new(None) };
}

/// Run a closure with `event_loop` installed as the thread's active loop, which is how JS
/// callbacks reach it ([`with_active_event_loop`]). The previous value is restored afterwards, so
/// calls nest.
///
/// Safe because the shared borrow outlives `f` and the installed pointer is only dereferenced
/// within `f`: the loop can be neither dropped nor moved while installed.
pub fn with_event_loop<R>(event_loop: &EventLoop, f: impl FnOnce(&EventLoop) -> R) -> R {
    /// Restores the previous active loop and `future` owner.
    struct Restore {
        prev_loop: Option<*const EventLoop>,
        prev_owner: u64,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT_EVENT_LOOP.with(|el| el.set(self.prev_loop));
            js::promise::set_current_future_owner(self.prev_owner);
        }
    }

    // Attribute futures spawned during `f` (e.g. a `fetch`) to this loop, so a concurrent loop
    // does not drive or settle them. Restored on exit.
    let prev_owner = js::promise::set_current_future_owner(event_loop.loop_id);
    let prev_loop = CURRENT_EVENT_LOOP.with(|el| el.replace(Some(event_loop as *const EventLoop)));
    let _restore = Restore {
        prev_loop,
        prev_owner,
    };
    f(event_loop)
}

js::instance_local! {
    /// Why no event loop is active, set by [`without_event_loop`] for the
    /// duration of its closure.
    static NO_LOOP_REASON: RefCell<Option<Rc<str>>> = const { RefCell::new(None) };
}

/// Whether code running now may block the current task: false inside [`without_event_loop`],
/// which runs sync-lifted exports.
pub fn blocking_allowed() -> bool {
    NO_LOOP_REASON.with(|cell| cell.borrow().is_none())
}

js::instance_local! {
    /// Set while the debugger holds the debuggee paused.
    static SUSPENDED: Cell<bool> = const { Cell::new(false) };
    /// Notified when [`set_suspended`] clears the flag.
    static SUSPENSION_ENDED: Event = const { Event::new() };
}

/// Suspend or resume every event loop. While suspended, [`EventLoop::step`] runs no task and
/// [`run_until`] waits for the suspension to end before it steps or polls futures.
pub fn set_suspended(suspended: bool) {
    SUSPENDED.with(|cell| cell.set(suspended));
    if !suspended {
        SUSPENSION_ENDED.with(|event| event.notify(usize::MAX));
    }
}

/// Whether [`set_suspended`] last suspended the event loops.
pub fn is_suspended() -> bool {
    SUSPENDED.with(Cell::get)
}

/// Run `f` with no active event loop, and `reason` as the explanation
/// [`throw_no_event_loop`] gives for work that needs one. The previous state is
/// restored afterwards.
///
/// This is mainly useful for running code in sync exports, which mustn't block, and hence
/// can't perform async operations.
pub fn without_event_loop<R>(reason: impl Into<Rc<str>>, f: impl FnOnce() -> R) -> R {
    /// Restores the previous active loop and reason.
    struct Restore {
        prev_loop: Option<*const EventLoop>,
        prev_reason: Option<Rc<str>>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT_EVENT_LOOP.with(|el| el.set(self.prev_loop));
            NO_LOOP_REASON.with(|cell| *cell.borrow_mut() = self.prev_reason.take());
        }
    }

    let prev_loop = CURRENT_EVENT_LOOP.with(|el| el.replace(None));
    let prev_reason = NO_LOOP_REASON.with(|cell| cell.borrow_mut().replace(reason.into()));
    let _restore = Restore {
        prev_loop,
        prev_reason,
    };
    f()
}

/// Throw a `TypeError` whose message is that `what` needs an active event loop,
/// followed by the reason [`without_event_loop`] set, if any.
pub fn throw_no_event_loop(scope: &Scope<'_>, what: &str) -> ExnThrown {
    let message = NO_LOOP_REASON.with(|cell| match &*cell.borrow() {
        Some(reason) => format!("{what} needs an active event loop, and {reason}"),
        None => format!("{what} needs an active event loop"),
    });
    js::error::TypeError(message).throw(scope)
}

/// Reject `promise` with the `TypeError` [`throw_no_event_loop`] throws for
/// `what`, and return `true`, if no event loop is active to drive the work that
/// would settle it. Otherwise return `false`, and leave `promise` alone.
pub fn reject_without_event_loop(
    scope: &Scope<'_>,
    promise: &js::Promise<'_>,
    what: &str,
) -> Result<bool, ExnThrown> {
    if with_active_event_loop(|_| ()).is_some() {
        return Ok(false);
    }
    throw_no_event_loop(scope, what);
    promise.reject_with_pending(scope)?;
    Ok(true)
}

/// Run a closure with a reference to the active event loop.
///
/// Returns `None` if no event loop is active (i.e. we're not inside a
/// driver's run loop). The raw pointer never escapes this function.
pub fn with_active_event_loop<R>(f: impl FnOnce(&EventLoop) -> R) -> Option<R> {
    CURRENT_EVENT_LOOP.with(|el| {
        el.get().map(|ptr| {
            // SAFETY: the pointer was installed by `with_event_loop`, whose
            // shared borrow of the loop spans the closure currently on the
            // stack. The loop is alive, and all its state is behind cells,
            // so shared access from here aliases nothing.
            f(unsafe { &*ptr })
        })
    })
}

// ---------------------------------------------------------------------------
// TaskId
// ---------------------------------------------------------------------------

/// Opaque identifier for a queued task.
///
/// Task IDs are unique within a single [`EventLoop`] instance and are never
/// reused.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TaskId(u64);

impl TaskId {
    /// Create a `TaskId` from a raw `u64` value.
    ///
    /// Test-only, since in production code it'd be a footgun.
    #[cfg(test)]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw `u64` value.
    ///
    /// Test-only, since in production code it'd be a footgun.
    #[cfg(test)]
    pub fn as_raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TaskId({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// StepOutcome
// ---------------------------------------------------------------------------

/// Result of a single [`EventLoop::step`] iteration.
///
/// Platform drivers use this to decide what to do next: exit, wait for
/// external events, or immediately step again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    /// The event loop has completed — no pending tasks and no external
    /// interest. The driver should exit.
    Done,

    /// There is still pending work (queued tasks or external interest),
    /// but nothing was ready to run in this step. The driver should wait
    /// for an external event (timer expiry, I/O completion, etc.) before
    /// stepping again.
    Idle,

    /// At least one task ran successfully. The driver should step again
    /// immediately — there may be more ready work.
    Progressed,
}

/// A `setTimeout`/`setInterval` timer, keyed by the id JS sees.
struct JsTimer {
    /// The task the timer controls. An interval re-queues itself under the same one.
    task: TaskId,
    /// Where the timer was created, kept for each re-queue of an interval.
    origin: Option<Rc<str>>,
}

// ---------------------------------------------------------------------------
// Pending work
// ---------------------------------------------------------------------------

/// One thing that keeps an event loop alive, as [`EventLoop::pending_work`] reports it.
#[derive(Debug)]
pub struct PendingWork {
    pub kind: PendingKind,
    /// The JS call stack that created the work, if origins were being recorded then. See
    /// [`js::stack::record_origins`].
    pub origin: Option<Rc<str>>,
}

/// The kinds of [`PendingWork`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// A task waiting for its deadline, labelled with its [`Task::kind`].
    Timer(&'static str),
    /// A task waiting to be signalled ready or to run, labelled with its [`Task::kind`].
    Task(&'static str),
    /// External interest held through an [`InterestHandle`], such as a stream being read.
    Interest,
    /// An async-promise future, such as a `fetch` in flight.
    Future,
    /// A [`PendingSend`], such as a stream being written.
    Send,
}

impl std::fmt::Display for PendingKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingKind::Timer(kind) => write!(f, "a pending `{kind}` timer"),
            PendingKind::Task(kind) => write!(f, "a queued `{kind}` task"),
            PendingKind::Interest => f.write_str("an operation that keeps the event loop alive"),
            PendingKind::Future => f.write_str("a host operation in flight"),
            PendingKind::Send => f.write_str("a stream or future being written"),
        }
    }
}

// ---------------------------------------------------------------------------
// Task trait
// ---------------------------------------------------------------------------

/// A unit of asynchronous work managed by the event loop.
///
/// Implementations are provided by individual builtins (timers, fetch,
/// promise resolution, etc.). The event loop stores tasks as trait objects
/// and is completely agnostic to their concrete types.
pub trait Task {
    /// A human-readable label for this task type (e.g. `"timer"`,
    /// `"promise"`, `"fetch"`). Used for debugging and diagnostics only.
    fn kind(&self) -> &'static str;

    /// Execute the task's work, reporting a thrown JS exception through `Err` for the caller to
    /// report. The task is consumed, so a repeating one re-queues itself here, under `id`.
    fn run(self: Box<Self>, scope: &Scope<'_>, id: TaskId) -> Result<(), ExnThrown>;

    /// Trace any GC-managed pointers held by this task.
    ///
    /// Called by SpiderMonkey during garbage collection. Implementations
    /// must call `.trace(trc)` on every `Heap<*mut JSObject>` (or other
    /// `Trace`-implementing) field.
    ///
    /// The default implementation is a no-op, which is correct for tasks
    /// that hold no JS references.
    fn trace(&self, _trc: *mut JSTracer) {}
}

// ---------------------------------------------------------------------------
// Task entry (internal)
// ---------------------------------------------------------------------------

/// Internal wrapper pairing a [`TaskId`] with its [`Task`] and readiness
/// state.
struct TaskEntry {
    id: TaskId,
    task: Box<dyn Task>,
    ready: bool,
    /// For timer tasks: the `Instant` at which this task becomes ready.
    /// `None` for non-timer tasks.
    ///
    /// The reading comes from `platform::clock`, not `std::time`, so a deadline
    /// taken before a Wizer snapshot is already past when the instance resumes.
    deadline: Option<Instant>,
    /// Where the task was queued, if origins were being recorded then. See
    /// [`js::stack::record_origins`].
    origin: Option<Rc<str>>,
}

impl TaskEntry {
    /// The entry's position in [`TaskQueue::ready`]: non-timer tasks first in allocation
    /// order, then timers by deadline, ties broken by allocation order.
    fn ready_key(&self) -> (Option<Instant>, u64) {
        (self.deadline, self.id.0)
    }
}

/// The live tasks, indexed so that lookups by id, the next timer, and the ready tasks in
/// dispatch order are found without scanning.
#[derive(Default)]
struct TaskQueue {
    /// Every live task by id.
    entries: HashMap<TaskId, TaskEntry>,
    /// The ready tasks, by [`TaskEntry::ready_key`].
    ready: BTreeSet<(Option<Instant>, u64)>,
    /// The timers that are not ready yet, by deadline then id.
    timers: BTreeSet<(Instant, u64)>,
}

impl TaskQueue {
    fn insert(&mut self, entry: TaskEntry) {
        if entry.ready {
            self.ready.insert(entry.ready_key());
        } else if let Some(deadline) = entry.deadline {
            self.timers.insert((deadline, entry.id.0));
        }
        self.entries.insert(entry.id, entry);
    }

    fn remove(&mut self, id: TaskId) -> Option<TaskEntry> {
        let entry = self.entries.remove(&id)?;
        if entry.ready {
            self.ready.remove(&entry.ready_key());
        } else if let Some(deadline) = entry.deadline {
            self.timers.remove(&(deadline, id.0));
        }
        Some(entry)
    }

    /// Mark `id` ready. Returns whether a task not yet ready was found.
    fn mark_ready(&mut self, id: TaskId) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        if entry.ready {
            return false;
        }
        entry.ready = true;
        if let Some(deadline) = entry.deadline {
            self.timers.remove(&(deadline, id.0));
        }
        self.ready.insert(entry.ready_key());
        true
    }

    /// Mark every timer whose deadline is at or before `now` ready, returning how many.
    fn expire_timers(&mut self, now: Instant) -> usize {
        let mut count = 0;
        while let Some(&(deadline, id)) = self.timers.first() {
            if deadline > now {
                break;
            }
            self.mark_ready(TaskId(id));
            count += 1;
        }
        count
    }
}

// ---------------------------------------------------------------------------
// EventLoop
// ---------------------------------------------------------------------------

/// Namespace token for the per-global setTimeout/setInterval id counter. Its
/// address is a process-unique, stable `usize` key for
/// [`js::class::next_global_counter`], mirroring how shared functions key on
/// their native pointer.
static TIMER_ID_COUNTER_KEY: u8 = 0;

/// The event loop task registry.
///
/// Owns all queued tasks and tracks which are ready to run. Platform
/// drivers (`native`, `wasi`) call into this struct to advance the
/// loop.
///
/// The `EventLoop` is stored on the [`Runtime`](crate::runtime::Runtime)
/// and its [`trace`](EventLoop::trace) method is called during GC to keep
/// JS references inside tasks alive.
/// All mutable state lives in cells, so every method takes `&self`: JS
/// callbacks re-entering through [`with_active_event_loop`] while the driver
/// is mid-[`step`](Self::step) only ever alias shared references.
///
/// Note that this only works because all state on `EventLoop` is of a nature where borrows are
/// naturally very short-lived: holding a borrow across JS calls would run the risk of double-borrows, which aren't guarded against.
// TODO: investigate whether we should hide this behind some kind of no-JS guard to prevent accidental long-lived borrows.
pub struct EventLoop {
    /// A process-unique id so async-promise futures (`js::promise`) can be attributed to the loop
    /// that spawned them: concurrent event loops drive and settle only their own.
    loop_id: u64,
    /// Monotonically increasing counter for generating unique [`TaskId`]s.
    next_id: Cell<u64>,
    /// All live tasks.
    tasks: RefCell<TaskQueue>,
    /// HTML's per-loop "map of setTimeout and setInterval IDs": the timer ids
    /// JS sees, mapped to the internal task they control.
    js_timers: RefCell<HashMap<u64, JsTimer>>,
    /// Scratch buffer for [`step`](Self::step)'s per-batch dispatch list,
    /// reused across steps so the hot path stays allocation-free.
    batch_buf: RefCell<Vec<TaskId>>,
    /// Set by [`request_stop`](Self::request_stop) to end this loop after its current step,
    /// overriding all other signals of event loop activity, such as pending async tasks and
    /// timers, or active interest.
    stop_requested: Cell<bool>,
    /// External keep-alive interest. When positive, the event loop stays
    /// alive even with an empty task queue.
    interest: InterestTracker,
    /// Notification event: signaled whenever a task becomes ready or a
    /// timer is queued. The async driver awaits this to avoid busy-polling.
    /// Shared with this loop's [`InterestHandle`]s, whose release
    /// must wake this loop even when it happens during another loop's turn.
    notify: Rc<Event>,
    /// Sends [`register_pending_send`](Self::register_pending_send) registered,
    /// for the loop's driver to take, each with the JS call stack that
    /// registered it, if origins recording was active during registration.
    pending_sends: RefCell<Vec<(PendingSend, Option<Rc<str>>)>>,
}

/// A send, such as a response body's, that the loop's work can feed. The loop's
/// driver awaits `done` while the loop has work, and calls `abandon` once no more
/// work is left, after which `done` completes as well.
pub struct PendingSend {
    pub done: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>,
    pub abandon: Box<dyn FnOnce()>,
}

impl EventLoop {
    /// Create a new, empty event loop with a fresh process-unique id.
    pub fn new() -> Self {
        static NEXT_LOOP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let notify = Rc::new(Event::new());
        Self {
            loop_id: NEXT_LOOP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            next_id: Cell::new(0),
            tasks: RefCell::new(TaskQueue::default()),
            js_timers: RefCell::new(HashMap::new()),
            batch_buf: RefCell::new(Vec::new()),
            stop_requested: Cell::new(false),
            interest: InterestTracker::new(Rc::clone(&notify)),
            notify,
            pending_sends: RefCell::new(Vec::new()),
        }
    }

    /// Queue a task for later execution.
    ///
    /// The task starts in the **Queued** (not ready) state. Call
    /// [`signal_ready`](Self::signal_ready) to mark it runnable.
    ///
    /// Returns the [`TaskId`] assigned to this task.
    pub fn queue(&self, task: Box<dyn Task>) -> TaskId {
        let id = self.next_task_id();
        // Read before `tasks` is borrowed: recording the origin can GC, which traces the loop.
        let origin = js::stack::current_origin();
        self.tasks.borrow_mut().insert(TaskEntry {
            id,
            task,
            ready: false,
            deadline: None,
            origin,
        });
        id
    }

    /// Queue a task that is immediately ready to run.
    ///
    /// This is a convenience for tasks that don't need to wait for an
    /// external event (e.g. resolved promises, `queueMicrotask` work).
    pub fn queue_ready(&self, task: Box<dyn Task>) -> TaskId {
        let id = self.next_task_id();
        // Read before `tasks` is borrowed: recording the origin can GC, which traces the loop.
        let origin = js::stack::current_origin();
        self.tasks.borrow_mut().insert(TaskEntry {
            id,
            task,
            ready: true,
            deadline: None,
            origin,
        });
        self.notify.notify(1);
        id
    }

    /// Queue a task with a timer deadline.
    ///
    /// The task will become ready when [`advance_timers`](Self::advance_timers)
    /// detects that the deadline has passed. For `setTimeout(fn, 0)` or
    /// similar, use a deadline of `Instant::now()`.
    pub fn queue_timer(&self, task: Box<dyn Task>, deadline: Instant) -> TaskId {
        let id = self.next_task_id();
        self.insert_timer(id, task, deadline, js::stack::current_origin());
        id
    }

    /// Insert a timer task under `id`, and wake the driver so it can re-evaluate the earliest
    /// timer deadline.
    fn insert_timer(
        &self,
        id: TaskId,
        task: Box<dyn Task>,
        deadline: Instant,
        origin: Option<Rc<str>>,
    ) {
        self.tasks.borrow_mut().insert(TaskEntry {
            id,
            task,
            ready: false,
            deadline: Some(deadline),
            origin,
        });
        self.notify.notify(1);
    }

    /// Cancel a queued task, removing it from the event loop.
    ///
    /// Returns `true` if the task was found and removed. A task that is not
    /// in the queue (already run, or currently being executed by the driver)
    /// is left alone.
    pub fn cancel_if_queued(&self, id: TaskId) -> bool {
        self.tasks.borrow_mut().remove(id).is_some()
    }

    /// Queue a `setTimeout`/`setInterval` task under a fresh JS timer id.
    pub fn queue_js_timer(
        &self,
        scope: &Scope<'_>,
        deadline: Instant,
        make_task: impl FnOnce(u64) -> Box<dyn Task>,
    ) -> u64 {
        let timer_id = self.allocate_js_timer_id(scope);
        let task = self.next_task_id();
        let origin = js::stack::current_origin();
        self.insert_timer(task, make_task(timer_id), deadline, origin.clone());
        self.js_timers
            .borrow_mut()
            .insert(timer_id, JsTimer { task, origin });
        timer_id
    }

    /// Re-queue an interval timer with the same JS timer id and [`TaskId`].
    ///
    /// If the timer was cleared while its callback ran (via `clearInterval`
    /// from within the callback), the id no longer maps to this task and the
    /// task is silently dropped instead of re-queued.
    pub fn requeue_js_timer(
        &self,
        timer_id: u64,
        id: TaskId,
        task: Box<dyn Task>,
        deadline: Instant,
    ) {
        let origin = match self.js_timers.borrow().get(&timer_id) {
            Some(timer) if timer.task == id => timer.origin.clone(),
            _ => return,
        };
        self.insert_timer(id, task, deadline, origin);
    }

    /// Cancel a JS timer by its `setTimeout`/`setInterval` id.
    ///
    /// Invalid IDs are silently ignored per spec.
    pub fn clear_js_timer(&self, timer_id: u64) {
        let removed = self.js_timers.borrow_mut().remove(&timer_id);
        if let Some(timer) = removed {
            // If the task is still queued, remove it. If it is currently
            // running (an interval clearing itself from its own callback),
            // the now-missing map entry suppresses the re-queue instead.
            self.cancel_if_queued(timer.task);
        }
    }

    /// Release a one-shot JS timer's id after it fired.
    ///
    /// Mirrors HTML's "remove global's map of `setTimeout` and `setInterval`
    /// `IDs[id]`" task substep for non-repeating timers.
    pub fn js_timer_fired(&self, timer_id: u64) {
        self.js_timers.borrow_mut().remove(&timer_id);
    }

    /// Allocate the next JS timer id: greater than zero and not currently in
    /// this loop's timer map.
    fn allocate_js_timer_id(&self, scope: &Scope<'_>) -> u64 {
        let key = &TIMER_ID_COUNTER_KEY as *const u8 as usize;
        let js_timers = self.js_timers.borrow();
        loop {
            let id = js::class::next_global_counter(scope, key);
            if id != 0 && !js_timers.contains_key(&id) {
                return id;
            }
        }
    }

    /// Mark a queued task as ready to run.
    ///
    /// Has no effect if the task ID is not found (the task may have
    /// already been cancelled or run).
    pub fn signal_ready(&self, id: TaskId) {
        if self.tasks.borrow_mut().mark_ready(id) {
            self.notify.notify(1);
        }
    }

    /// Take the next ready task out of the queue.
    ///
    /// Returns `None` if no tasks are currently ready. The returned task
    /// is removed from the event loop — the caller must `run()` it.
    /// "Next" follows [`step`](Self::step)'s dispatch order: ready non-timer
    /// tasks in allocation order, then expired timers by deadline.
    pub fn pop_ready(&self) -> Option<(TaskId, Box<dyn Task>)> {
        let mut tasks = self.tasks.borrow_mut();
        let &(_, id) = tasks.ready.first()?;
        let entry = tasks.remove(TaskId(id)).expect("ready tasks are live");
        Some((entry.id, entry.task))
    }

    /// Check all timer-based tasks and mark those whose deadline has
    /// passed as ready.
    ///
    /// Returns the number of timers that became ready.
    // TODO: consider merging with `time_to_next_timer` and returning the tasks directly.
    pub fn advance_timers(&self) -> usize {
        self.tasks.borrow_mut().expire_timers(Instant::now())
    }

    /// Returns the duration until the next timer fires, or `None` if
    /// there are no pending timers.
    ///
    /// A return value of `Duration::ZERO` (or very small) means a timer
    /// is already expired and [`advance_timers`](Self::advance_timers)
    /// should be called.
    pub fn time_to_next_timer(&self) -> Option<Duration> {
        let &(deadline, _) = self.tasks.borrow().timers.first()?;
        Some(deadline.saturating_duration_since(Instant::now()))
    }

    /// Returns `true` if there are any tasks (ready or not) in the queue.
    pub fn has_pending(&self) -> bool {
        !self.tasks.borrow().entries.is_empty()
    }

    /// Returns `true` if at least one task is in the ready state.
    pub fn has_ready(&self) -> bool {
        !self.tasks.borrow().ready.is_empty()
    }

    /// Returns the number of tasks currently queued (ready or not).
    pub fn len(&self) -> usize {
        self.tasks.borrow().entries.len()
    }

    /// Returns `true` if the event loop has no tasks.
    pub fn is_empty(&self) -> bool {
        self.tasks.borrow().entries.is_empty()
    }

    // -----------------------------------------------------------------------
    // Interest tracking
    // -----------------------------------------------------------------------

    /// Register external interest — the event loop stays alive until the
    /// returned handle is dropped. The handle targets *this* loop's counter
    /// directly (and wakes this loop's driver on release), so releasing it
    /// during another request's turn, e.g. by a promise settling in a
    /// foreign loop's microtask drain, still lands on the right loop.
    pub fn acquire_interest_handle(&self) -> InterestHandle {
        self.interest.acquire_handle()
    }

    /// Register `send` for this loop's driver to finish (see [`PendingSend`]).
    pub fn register_pending_send(&self, send: PendingSend) {
        let origin = js::stack::current_origin();
        self.pending_sends.borrow_mut().push((send, origin));
    }

    /// Take the sends [`register_pending_send`](Self::register_pending_send)
    /// registered.
    pub fn take_pending_sends(&self) -> Vec<PendingSend> {
        std::mem::take(&mut *self.pending_sends.borrow_mut())
            .into_iter()
            .map(|(send, _)| send)
            .collect()
    }

    /// Whether a send is registered that no driver has taken yet.
    pub fn has_pending_sends(&self) -> bool {
        !self.pending_sends.borrow().is_empty()
    }

    /// Returns `true` if at least one external interest is held.
    pub fn has_interest(&self) -> bool {
        self.interest.has_interest()
    }

    /// Returns `true` if the event loop should stay alive: there are pending
    /// tasks, external interest is held, or an async-promise future (e.g. a
    /// `fetch`) is in flight.
    pub fn is_alive(&self) -> bool {
        self.has_pending() || self.has_interest() || js::promise::has_pending_futures(self.loop_id)
    }

    /// Whether any futures backed by external async tasks are active in this event loop.
    /// This includes things like filesystem or network I/O.
    pub fn has_active_external_async_tasks(&self) -> bool {
        js::promise::has_pending_futures(self.loop_id)
    }

    /// `Ok` if this loop holds no work, and otherwise `Err` describing each piece of work it holds
    /// and where it was created, for a caller about to take a Wizer snapshot, which cannot capture
    /// pending work.
    pub fn ensure_idle_for_snapshot(&self) -> Result<(), String> {
        use std::fmt::Write as _;

        let work = self.pending_work();
        if work.is_empty() {
            return Ok(());
        }
        let mut message = String::from(
            "the application's top level finished evaluating with asynchronous work still \
             pending, which a snapshot cannot hold. Await or cancel each of these before the top \
             level finishes:",
        );
        for item in work {
            match item.origin {
                Some(origin) => {
                    let _ = write!(message, "\n  - {}, created at:", item.kind);
                    for frame in origin.lines() {
                        let _ = write!(message, "\n      {frame}");
                    }
                }
                None => {
                    let _ = write!(
                        message,
                        "\n  - {}, created with no JavaScript on the stack",
                        item.kind
                    );
                }
            }
        }
        Err(message)
    }

    /// Everything that keeps this loop alive: its tasks, its external interest, the async-promise
    /// futures it owns, and its pending sends. Each entry's origin is set if origins were being
    /// recorded when the work was created (see [`js::stack::record_origins`]).
    ///
    /// Tasks come first, in the order they were queued, then interest, then futures, then sends.
    pub fn pending_work(&self) -> Vec<PendingWork> {
        let tasks = self.tasks.borrow();
        let mut entries: Vec<&TaskEntry> = tasks.entries.values().collect();
        entries.sort_unstable_by_key(|entry| entry.id.0);
        let mut work: Vec<PendingWork> = entries
            .into_iter()
            .map(|entry| PendingWork {
                kind: match entry.deadline {
                    Some(_) => PendingKind::Timer(entry.task.kind()),
                    None => PendingKind::Task(entry.task.kind()),
                },
                origin: entry.origin.clone(),
            })
            .collect();
        let interest = self.interest.origins();
        let unrecorded = self.interest.count() as usize - interest.len();
        work.extend(interest.into_iter().map(|origin| PendingWork {
            kind: PendingKind::Interest,
            origin: Some(origin),
        }));
        work.extend((0..unrecorded).map(|_| PendingWork {
            kind: PendingKind::Interest,
            origin: None,
        }));
        work.extend(
            js::promise::pending_future_origins(self.loop_id)
                .into_iter()
                .map(|origin| PendingWork {
                    kind: PendingKind::Future,
                    origin,
                }),
        );
        work.extend(
            self.pending_sends
                .borrow()
                .iter()
                .map(|(_, origin)| PendingWork {
                    kind: PendingKind::Send,
                    origin: origin.clone(),
                }),
        );
        work
    }

    /// An [`EventListener`] resolving on the next readiness notification: a task became ready, a
    /// timer was queued. It must be awaited, and completes on its first poll if the notification
    /// arrived before it was created.
    pub fn notified(&self) -> EventListener<()> {
        self.notify.listen()
    }

    /// Drop this loop's still-pending async-promise futures, cancelling the host I/O each has in
    /// flight and unregistering its rooted promise box.
    ///
    /// Every driver that abandons a loop with futures still pending must call this *while the
    /// JSContext is alive*: left in place, the boxes outlive the engine and are freed during
    /// thread-local teardown, where `finishRoots` traces memory that is already gone.
    pub fn cancel_pending_futures(&self) {
        js::promise::cancel_pending_futures_for(self.loop_id);
    }

    /// Request that this loop's [`run_to_completion`] return after the
    /// current step completes, even if tasks, timers, or futures remain.
    pub fn request_stop(&self) {
        self.stop_requested.set(true);
    }
}

/// Request that the active loop's [`run_to_completion`] return after the
/// current step completes, even if tasks, timers, or futures remain.
///
/// WPT mode calls this from a test's `done()`, since a finished test may leave a `setInterval`
/// running that would keep the loop alive and hang the process. It targets the loop running the
/// caller's JS and is dropped when there is none: nothing to stop, and a thread-global flag would
/// instead stop whichever unrelated loop stepped next.
pub fn request_stop() {
    with_active_event_loop(|el| el.request_stop());
}

/// Run the event loop until it has nothing left: no tasks, no interest, no futures in flight.
///
/// [`run_until`] with a stop condition that never fires.
///
/// # Safety
///
/// `raw_cx` must be a valid JSContext pointer that remains valid for the
/// lifetime of this future (i.e. the `Runtime` must not be dropped).
pub async unsafe fn run_to_completion<S, F>(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    sleep: S,
) where
    S: Fn(Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    unsafe { run_until(raw_cx, event_loop, sleep, |_| false).await }
}

/// The prefix [`trace_idle`] writes before its tag.
pub const IDLE_TRACE: &str = "starling: event loop idle ";

/// Report that the loop identified by `tag` is about to become idle.
///
/// Mostly useful for testing, where the test harness can use the output as a signal that it can
/// start sending another request which must be handled by the same instance—which only happens
/// if that instance is idle.
///
/// Must be called immediately before the `await` that hands control back to the host, to minimize
/// the delay between printing the message and the instance becoming eligible for reuse.
///
/// Off unless `STARLING_TRACE_IDLE` is set, read once because this sits in the loop's hot path.
pub fn trace_idle(tag: impl std::fmt::Display) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::var_os("STARLING_TRACE_IDLE").is_some()) {
        eprintln!("{IDLE_TRACE}{tag}");
    }
}

/// Drive `event_loop` until `evaluation`'s content script has finished evaluating, including
/// awaited (and only those) async functions and promises.
///
/// # Safety
///
/// `raw_cx` must be a valid JSContext pointer that remains valid for the
/// lifetime of this future (i.e. the `Runtime` must not be dropped).
pub async unsafe fn run_until_evaluated<S, F>(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    sleep: S,
    evaluation: &crate::ScriptEvaluation,
) where
    S: Fn(Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    unsafe {
        run_until(raw_cx, event_loop, sleep, |scope| {
            evaluation.is_finished(scope)
        })
        .await
    }
}

/// Settle `completed` async-promise futures with `event_loop` active, so a reaction (a timer, or
/// releasing the loop's interest, e.g. a FetchEvent's `respondWith` resolving from a `fetch`) runs
/// against this loop rather than no loop or another request's.
///
/// # Safety
///
/// As for [`run_until`].
unsafe fn settle_completed_futures(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    completed: Vec<js::promise::CompletedFuture>,
) {
    with_event_loop(event_loop, |_| {
        // SAFETY: guaranteed by this function's caller.
        let scope = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
        if completed.is_empty() {
            // A poll that completed nothing can still have run JS, such as the
            // lowering of a stream nested in a stream element, which queues
            // microtasks.
            if jobs::has_pending_jobs(&scope) {
                run_microtasks(&scope);
            }
            return;
        }
        js::promise::settle_completed_futures(&scope, completed);
        // Settling drains the reactions, and this reports the rejections they left
        // unhandled, as after a task.
        run_microtasks(&scope);
    });
}

/// Drive `event_loop` until every send registered on it (see [`PendingSend`]) is done. Once the
/// loop has no work left while a send is unfinished, every unfinished send is abandoned.
///
/// # Safety
///
/// As for [`run_until`].
pub async unsafe fn finish_pending_sends<S, F>(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    sleep: S,
) where
    S: Fn(Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    let sends = event_loop.take_pending_sends();
    if sends.is_empty() {
        return;
    }
    let (dones, abandons): (Vec<_>, Vec<_>) = sends
        .into_iter()
        .map(|send| (send.done, send.abandon))
        .unzip();
    let mut all_done = std::pin::pin!(async {
        for done in dones {
            done.await;
        }
    });
    let finished = futures_lite::future::or(
        async {
            all_done.as_mut().await;
            true
        },
        async {
            // SAFETY: guaranteed by this function's caller.
            unsafe { run_until(raw_cx, event_loop, &sleep, |_| false).await };
            false
        },
    )
    .await;
    if !finished {
        // An abandon can run JS, whose microtasks are drained before the loop
        // steps again.
        with_event_loop(event_loop, |_| {
            for abandon in abandons {
                abandon();
            }
            // SAFETY: guaranteed by this function's caller.
            let scope = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
            run_microtasks(&scope);
        });
        all_done.await;
    }
}

/// Runs the event loop in an async loop, calling `should_stop` each turn, and returns when it
/// returns true.
///
/// # Safety
///
/// `raw_cx` must be a valid JSContext pointer that remains valid for the
/// lifetime of this future (i.e. the `Runtime` must not be dropped).
pub async unsafe fn run_until<S, F>(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    sleep: S,
    should_stop: impl Fn(&Scope<'_>) -> bool,
) where
    S: Fn(Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    // Completions that arrived as a suspension began, settled once it ends.
    let mut held: Vec<js::promise::CompletedFuture> = Vec::new();
    loop {
        if is_suspended() {
            let ended = SUSPENSION_ENDED.with(Event::listen);
            if is_suspended() {
                ended.await;
            }
            continue;
        }
        if !held.is_empty() {
            // SAFETY: guaranteed by this function's caller.
            unsafe { settle_completed_futures(raw_cx, event_loop, std::mem::take(&mut held)) };
        }

        // Create a fresh rooting scope each iteration — the same
        // technique JSNative trampolines use. It is dropped before any
        // await point so GC roots don't span a suspension.
        let scope = js::gc::scope::RootScope::from_current_realm(raw_cx);
        let outcome = with_event_loop(event_loop, |el| el.step(&scope));
        let caller_stop = should_stop(&scope);
        drop(scope);

        // A step may request the loop end, e.g. in WPT mode once a test's completion callback
        // fired even though there are still async tasks, such as timers, pending.
        if event_loop.stop_requested.get() {
            event_loop.cancel_pending_futures();
            return;
        }

        // The caller's stop condition holds — its work is done; whatever the
        // loop still carries stays in place for a later driver.
        if caller_stop {
            return;
        }

        // `fetch` and other async-IO builtins return a JS promise backed by a Rust
        // future (see `js::promise`). Those futures aren't event-loop tasks, so the
        // loop must stay alive — and keep polling them — while any of *its own* is in
        // flight, even when `step` reports `Done`.
        let owner = event_loop.loop_id;
        let have_futures = js::promise::has_pending_futures(owner);

        match outcome {
            StepOutcome::Done if !have_futures => return,
            StepOutcome::Progressed => {
                // A perpetually-ready task chain — a self-rescheduling timer
                // whose handler outlasts the 4ms nested-timer clamp — never
                // reaches the await arm below, where async-promise futures
                // are normally polled: a fetch would never even issue its
                // request. Give the executor one turn (so the platform
                // reactor can deliver I/O readiness) and this loop's futures
                // one poll before re-stepping; completions settle with the
                // loop active, same as in the await arm.
                futures_lite::future::yield_now().await;
                if is_suspended() {
                    continue;
                }
                let completed = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(with_event_loop(event_loop, |_| {
                        js::promise::poll_pending_futures(owner, cx)
                    }))
                })
                .await;
                // SAFETY: guaranteed by this function's caller.
                unsafe { settle_completed_futures(raw_cx, event_loop, completed) };
                continue;
            }
            StepOutcome::Done | StepOutcome::Idle => {
                // Wait for the next timer, an external notification (a task was
                // signaled ready, a new timer was queued), or progress on an
                // async-promise future (its I/O became ready). Whichever happens
                // first wakes us; we then loop and re-`step` to drain any microtasks
                // a settled promise queued.
                let timer_wait = async {
                    if let Some(wait) = event_loop.time_to_next_timer() {
                        sleep(wait).await;
                    } else {
                        // No timers — pend forever; another branch will wake us.
                        std::future::pending::<()>().await;
                    }
                };
                let notified = event_loop.notified();
                // Poll this loop's async-promise futures with the real task waker so their I/O
                // readiness wakes this await; complete once one settles, stashing the completions to
                // settle below. The loop is active during the polls, since one can run JS, such as
                // the lowering of a stream nested in a stream element, which starts work of its own.
                // A poll that left jobs queued completes this branch as well, so they are drained
                // below, since the jobs can start work, such as a write, that the futures wait on.
                let mut completed = Vec::new();
                let mut suspension_ended: Option<EventListener> = None;
                let drive_futures = std::future::poll_fn(|cx| {
                    // While suspended, poll nothing and wait for the suspension to end.
                    if is_suspended() {
                        let ended = suspension_ended
                            .get_or_insert_with(|| SUSPENSION_ENDED.with(Event::listen));
                        if std::future::Future::poll(std::pin::Pin::new(ended), cx).is_ready() {
                            suspension_ended = None;
                            cx.waker().wake_by_ref();
                        }
                        return std::task::Poll::Pending;
                    }
                    suspension_ended = None;
                    let (done, jobs) = with_event_loop(event_loop, |_| {
                        let done = js::promise::poll_pending_futures(owner, cx);
                        // SAFETY: guaranteed by this function's caller.
                        let scope = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
                        (done, jobs::has_pending_jobs(&scope))
                    });
                    if done.is_empty() && !jobs {
                        std::task::Poll::Pending
                    } else {
                        completed = done;
                        std::task::Poll::Ready(())
                    }
                });

                // Immediately before the await below, so nothing of this loop's runs in between.
                trace_idle(format_args!("loop:{}", event_loop.loop_id));
                // Race: first one to complete wins.
                futures_lite::future::or(
                    futures_lite::future::or(timer_wait, notified),
                    drive_futures,
                )
                .await;

                if is_suspended() {
                    held.append(&mut completed);
                    continue;
                }
                // SAFETY: guaranteed by this function's caller.
                unsafe { settle_completed_futures(raw_cx, event_loop, completed) };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Exception handling helper
// ---------------------------------------------------------------------------

/// Handle and clear a pending JS exception.
///
/// Reports the exception as `Uncaught` followed by its message, location and
/// stack (see [`crate::diagnostics`]), and clears the pending state, allowing
/// the event loop to continue. This is called by [`EventLoop::step`] when a
/// task or microtask throws.
fn handle_and_clear_exception(scope: &Scope<'_>) {
    if js::exception::is_pending(scope) {
        let captured = js::error::ExnThrown::capture(scope);
        crate::diagnostics::report(&format!("Uncaught {}", captured.to_string().trim_end()));
    }
}

impl EventLoop {
    /// Advance the event loop by one step: advance timers, then run the tasks that are ready,
    /// draining microtasks and checking for exceptions after each. The [`StepOutcome`] tells the
    /// driver what to do next.
    pub fn step(&self, scope: &Scope<'_>) -> StepOutcome {
        debug_assert!(
            !js::jobs::has_pending_jobs(scope),
            "Pending microtask detected"
        );
        debug_assert!(
            !js::exception::is_pending(scope),
            "Pending JS exception detected"
        );
        if is_suspended() {
            return if self.is_alive() {
                StepOutcome::Idle
            } else {
                StepOutcome::Done
            };
        }
        self.advance_timers();

        // One batch per step: a task that becomes ready while the batch runs — a zero-delay
        // interval re-queueing itself, a timer expiring during a long handler — waits for the
        // next one. Re-popping here would let a perpetually-ready task pin the loop inside a
        // single `step`, starving the driver's await branch, where async-promise futures are
        // polled and the platform reactor turns.
        //
        // Ready non-timer tasks go first in allocation order, then expired timers by deadline,
        // ties broken the same way, which is the order of the ready set. The batch buffer is
        // reused to keep the path allocation-free.
        let mut batch = self.batch_buf.take();
        batch.extend(self.tasks.borrow().ready.iter().map(|&(_, id)| TaskId(id)));
        let mut ran_any = false;
        for &id in &batch {
            // Take the task out before running it, ending the borrow: the
            // task's JS can re-enter this loop (setTimeout, clearTimeout)
            // through `with_active_event_loop`. Re-locate by id — an earlier
            // task in the batch may have cancelled this one.
            let Some(entry) = self.tasks.borrow_mut().remove(id) else {
                continue;
            };
            if entry.task.run(scope, id).is_err() {
                handle_and_clear_exception(scope);
            }
            ran_any = true;

            // After each task, drain microtasks — the task may have
            // resolved promises or scheduled reactions.
            run_microtasks(scope);

            if js::exception::is_pending(scope) {
                handle_and_clear_exception(scope);
            }
        }
        batch.clear();
        *self.batch_buf.borrow_mut() = batch;

        if ran_any {
            return StepOutcome::Progressed;
        }

        if self.is_alive() {
            StepOutcome::Idle
        } else {
            StepOutcome::Done
        }
    }

    /// Trace all live tasks for GC.
    ///
    /// Called by SpiderMonkey's GC through the extra-roots-tracer mechanism, running every task's
    /// [`Task::trace`] so their `Heap<*mut JSObject>` fields are marked.
    ///
    /// # Safety
    ///
    /// `trc` must be a valid `JSTracer` pointer provided by SpiderMonkey. GC only runs at JS
    /// allocation points, and no `tasks` borrow is held across a call into JS, so the borrow here
    /// cannot conflict.
    pub unsafe fn trace(&self, trc: *mut JSTracer) {
        for entry in self.tasks.borrow().entries.values() {
            entry.task.trace(trc);
        }
    }

    /// Allocate the next unique [`TaskId`].
    fn next_task_id(&self) -> TaskId {
        let id = TaskId(self.next_id.get());
        self.next_id.set(id.0 + 1);
        id
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run SpiderMonkey's microtask queue and clear weak references.
///
/// This should be called after running any task that may have created
/// promise reactions or other microtasks. It drains the job queue and
/// then clears the weak-reference set for the current "turn".
///
/// Any `FinalizationRegistry` cleanup a collection queued runs first, so a
/// callback's own microtasks are drained in the same turn. A promise rejected
/// with no handler attached by the end of the drain is reported by
/// [`report_unhandled_rejections`].
pub fn run_microtasks(scope: &Scope<'_>) {
    debug_assert!(
        !js::exception::is_pending(scope),
        "Cannot run microtasks with pending exception"
    );
    crate::finalization::run_pending(scope);
    jobs::run_jobs(scope);
    // Weak-ref set is cleared by run_jobs.
    report_unhandled_rejections(scope);
}

/// Report each promise that was rejected with no handler attached, and still has none, with the
/// rejection reason's message and stack (see [`crate::diagnostics`]).
pub fn report_unhandled_rejections(scope: &Scope<'_>) {
    for promise in jobs::take_unhandled_rejections(scope) {
        let Some(reason) = promise.result(scope) else {
            continue;
        };
        js::exception::set_pending(scope, reason, js::native::ExceptionStackBehavior::Capture);
        let captured = js::error::ExnThrown::capture(scope);
        crate::diagnostics::report(&format!(
            "Uncaught (in promise) {}",
            captured.to_string().trim_end()
        ));
    }
}
