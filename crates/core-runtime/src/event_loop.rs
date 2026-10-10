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
use std::rc::{Rc, Weak};
use std::time::Duration;

use event_listener::{Event, EventListener};
use js::async_context::Snapshot;
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
    /// Set once the runtime serves requests, which can run concurrently.
    static SERVING_REQUESTS: Cell<bool> = const { Cell::new(false) };
    /// Whether blocking on host I/O is allowed while serving requests, from
    /// `--allow-blocking-host-io`.
    static BLOCKING_HOST_IO_WHILE_SERVING: Cell<bool> = const { Cell::new(false) };
    /// Set when the runtime runs its script as a command, where nothing serves requests.
    static RUNNING_AS_COMMAND: Cell<bool> = const { Cell::new(false) };
    /// Set when the runtime is a component without a `wasi:http/handler` export.
    static WITHOUT_REQUEST_HANDLER: Cell<bool> = const { Cell::new(false) };
}

/// Record that the runtime runs its script as a command, so it serves no requests.
pub fn set_running_as_command() {
    RUNNING_AS_COMMAND.with(|cell| cell.set(true));
}

/// Whether the runtime runs its script as a command, so it serves no requests.
pub fn running_as_command() -> bool {
    RUNNING_AS_COMMAND.with(Cell::get)
}

/// Record that the runtime is a component without a `wasi:http/handler` export, so it serves no
/// requests.
pub fn set_without_request_handler() {
    WITHOUT_REQUEST_HANDLER.with(|cell| cell.set(true));
}

/// Whether the runtime is a component without a `wasi:http/handler` export, so it serves no
/// requests.
pub fn without_request_handler() -> bool {
    WITHOUT_REQUEST_HANDLER.with(Cell::get)
}

/// Record that the runtime serves requests from now on.
pub fn set_serving_requests() {
    SERVING_REQUESTS.with(|serving| serving.set(true));
}

/// Allow blocking on host I/O while serving requests, as `--allow-blocking-host-io` does.
pub fn set_blocking_host_io_while_serving(allowed: bool) {
    BLOCKING_HOST_IO_WHILE_SERVING.with(|cell| cell.set(allowed));
}

/// Why code running now must not block the current task on host I/O, such as a synchronous file
/// read, or `None` if it may.
///
/// Blocking is refused inside synchronous exports, which must not block at all, and while the
/// runtime serves requests, unless `--allow-blocking-host-io` allows it: a blocked task holds the
/// component instance's lock, which stalls every other request the instance serves.
pub fn blocking_host_io_refusal() -> Option<&'static str> {
    if !blocking_allowed() {
        return Some("inside a synchronous export, which must not block");
    }
    let serving = SERVING_REQUESTS.with(Cell::get);
    if serving && !BLOCKING_HOST_IO_WHILE_SERVING.with(Cell::get) {
        return Some(
            "while serving requests, where it would stall every concurrent request \
             (--allow-blocking-host-io allows it)",
        );
    }
    None
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
    /// Whether the timer keeps the loop alive. See [`EventLoop::set_js_timer_ref`].
    keeps_alive: bool,
    /// The async context current when the timer was created, kept for each re-queue of an
    /// interval.
    context: Snapshot,
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
    /// The reading comes from `platform::clock`, not `std::time`, so a timer
    /// pending when a Wizer snapshot is taken has the time it had left then
    /// when an instance resumes from the snapshot.
    deadline: Option<Instant>,
    /// Where the task was queued, if origins were being recorded then. See
    /// [`js::stack::record_origins`].
    origin: Option<Rc<str>>,
    /// Whether the task keeps the loop alive. Only unreferenced JS timers don't.
    keeps_alive: bool,
    /// The async context current when the task was queued, which is current while it runs.
    context: Snapshot,
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
    /// The number of entries that keep the loop alive.
    keep_alive: usize,
}

impl TaskQueue {
    fn insert(&mut self, entry: TaskEntry) {
        if entry.ready {
            self.ready.insert(entry.ready_key());
        } else if let Some(deadline) = entry.deadline {
            self.timers.insert((deadline, entry.id.0));
        }
        self.keep_alive += entry.keeps_alive as usize;
        self.entries.insert(entry.id, entry);
    }

    fn remove(&mut self, id: TaskId) -> Option<TaskEntry> {
        let entry = self.entries.remove(&id)?;
        self.keep_alive -= entry.keeps_alive as usize;
        if entry.ready {
            self.ready.remove(&entry.ready_key());
        } else if let Some(deadline) = entry.deadline {
            self.timers.remove(&(deadline, id.0));
        }
        Some(entry)
    }

    /// Set whether the entry `id`, if it is queued, keeps the loop alive.
    fn set_keeps_alive(&mut self, id: TaskId, keeps_alive: bool) {
        if let Some(entry) = self.entries.get_mut(&id) {
            self.keep_alive = self.keep_alive - entry.keeps_alive as usize + keeps_alive as usize;
            entry.keeps_alive = keeps_alive;
        }
    }

    /// Make the queued timer task `id` due at `deadline` instead, and not ready. Returns whether
    /// the task was queued.
    fn reschedule(&mut self, id: TaskId, deadline: Instant) -> bool {
        let Some(mut entry) = self.remove(id) else {
            return false;
        };
        entry.ready = false;
        entry.deadline = Some(deadline);
        self.insert(entry);
        true
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

js::instance_local! {
    /// The last JS timer id [`reserve_js_timer_id`] returned.
    static LAST_JS_TIMER_ID: Cell<u64> = const { Cell::new(0) };
}

js::instance_local! {
    /// The state of the loop that holds each queued or running JS timer, by timer id.
    static JS_TIMER_LOOPS: RefCell<HashMap<u64, Weak<LoopTasks>>> =
        RefCell::new(HashMap::new());
}

/// A JS timer id, greater than zero, that no earlier call or timer in this instance got, for a
/// timer that [`timer::start_timer`] queues later under it. Ids are unique across globals and
/// event loops, so that [`JS_TIMER_LOOPS`] finds the loop that holds a timer by its id alone.
pub fn reserve_js_timer_id() -> u64 {
    let id = LAST_JS_TIMER_ID.with(Cell::get) + 1;
    assert!(
        id < js::conversion::Number::MAX,
        "JS timer ids wrapped around"
    );
    LAST_JS_TIMER_ID.with(|last| last.set(id));
    id
}

/// The tasks and JS timers of an [`EventLoop`], shared with [`JS_TIMER_LOOPS`] so that a timer
/// can be cleared, referenced, unreferenced and rescheduled while another loop is active.
struct LoopTasks {
    /// All live tasks.
    tasks: RefCell<TaskQueue>,
    /// HTML's "map of setTimeout and setInterval IDs": the timer ids
    /// JS sees, mapped to the internal task they control.
    js_timers: RefCell<HashMap<u64, JsTimer>>,
    /// The owning loop's driver notification.
    notify: Rc<Event>,
}

impl LoopTasks {
    /// Cancel the JS timer `timer_id`. Returns whether this loop holds it.
    fn clear_js_timer(self: &Rc<Self>, timer_id: u64) -> bool {
        let removed = self.js_timers.borrow_mut().remove(&timer_id);
        let Some(timer) = removed else {
            return false;
        };
        self.unregister_js_timer(timer_id);
        // If the task is still queued, remove it. If it is currently
        // running (an interval clearing itself from its own callback),
        // the now-missing map entry suppresses the re-queue instead.
        self.tasks.borrow_mut().remove(timer.task);
        true
    }

    /// Set whether the JS timer `timer_id` keeps the loop alive. Returns whether this loop holds
    /// it.
    fn set_js_timer_ref(&self, timer_id: u64, referenced: bool) -> bool {
        let mut timers = self.js_timers.borrow_mut();
        let Some(timer) = timers.get_mut(&timer_id) else {
            return false;
        };
        timer.keeps_alive = referenced;
        self.tasks
            .borrow_mut()
            .set_keeps_alive(timer.task, referenced);
        true
    }

    /// Make the queued JS timer `timer_id` due at `deadline` instead. Returns whether this loop
    /// holds it queued.
    fn reschedule_js_timer(&self, timer_id: u64, deadline: Instant) -> bool {
        let Some(task) = self
            .js_timers
            .borrow()
            .get(&timer_id)
            .map(|timer| timer.task)
        else {
            return false;
        };
        self.tasks.borrow_mut().reschedule(task, deadline)
    }

    /// Record in [`JS_TIMER_LOOPS`] that this loop holds the JS timer `timer_id`.
    fn register_js_timer(self: &Rc<Self>, timer_id: u64) {
        JS_TIMER_LOOPS.with(|loops| {
            loops.borrow_mut().insert(timer_id, Rc::downgrade(self));
        });
    }

    /// Remove the [`JS_TIMER_LOOPS`] entry of the JS timer `timer_id` if it is this loop's.
    fn unregister_js_timer(self: &Rc<Self>, timer_id: u64) {
        JS_TIMER_LOOPS.with(|loops| {
            let mut loops = loops.borrow_mut();
            if loops
                .get(&timer_id)
                .is_some_and(|owner| std::ptr::eq(owner.as_ptr(), Rc::as_ptr(self)))
            {
                loops.remove(&timer_id);
            }
        });
    }
}

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
    /// All live tasks and JS timers.
    shared: Rc<LoopTasks>,
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
    /// The [`platform::clock::monotonic_ns`] reading at the loop's first [`step`](Self::step).
    started_ns: Cell<Option<u64>>,
    /// The nanoseconds the loop's driver spent waiting for work, as [`idle_ns`](Self::idle_ns)
    /// reports them.
    idle_ns: Cell<u64>,
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
            shared: Rc::new(LoopTasks {
                tasks: RefCell::new(TaskQueue::default()),
                js_timers: RefCell::new(HashMap::new()),
                notify: Rc::clone(&notify),
            }),
            batch_buf: RefCell::new(Vec::new()),
            stop_requested: Cell::new(false),
            interest: InterestTracker::new(Rc::clone(&notify)),
            notify,
            pending_sends: RefCell::new(Vec::new()),
            started_ns: Cell::new(None),
            idle_ns: Cell::new(0),
        }
    }

    /// The loop's process-unique id.
    pub fn id(&self) -> u64 {
        self.loop_id
    }

    /// The [`platform::clock::monotonic_ns`] reading at the loop's first [`step`](Self::step), or
    /// `None` before it.
    pub fn started_ns(&self) -> Option<u64> {
        self.started_ns.get()
    }

    /// The nanoseconds the loop's driver spent waiting for a timer, a task or I/O, between steps
    /// that found nothing to run.
    pub fn idle_ns(&self) -> u64 {
        self.idle_ns.get()
    }

    /// Queue a task for later execution.
    ///
    /// The task starts in the **Queued** (not ready) state. Call
    /// [`signal_ready`](Self::signal_ready) to mark it runnable.
    ///
    /// Returns the [`TaskId`] assigned to this task.
    pub fn queue(&self, task: Box<dyn Task>) -> TaskId {
        let id = self.next_task_id();
        self.shared.tasks.borrow_mut().insert(TaskEntry {
            id,
            task,
            ready: false,
            deadline: None,
            origin: None,
            keeps_alive: true,
            context: Snapshot::capture(),
        });
        self.record_origin(id);
        id
    }

    /// Queue a task that is immediately ready to run.
    ///
    /// This is a convenience for tasks that don't need to wait for an
    /// external event (e.g. resolved promises, `queueMicrotask` work).
    pub fn queue_ready(&self, task: Box<dyn Task>) -> TaskId {
        let id = self.next_task_id();
        self.shared.tasks.borrow_mut().insert(TaskEntry {
            id,
            task,
            ready: true,
            deadline: None,
            origin: None,
            keeps_alive: true,
            context: Snapshot::capture(),
        });
        self.record_origin(id);
        self.notify.notify(1);
        id
    }

    /// Record the origin of the queued task `id`. Recording the origin can GC, which traces the
    /// loop, so it happens once the task is in `tasks`, which traces the task's GC pointers, and
    /// while `tasks` isn't borrowed.
    fn record_origin(&self, id: TaskId) {
        let Some(origin) = js::stack::current_origin() else {
            return;
        };
        if let Some(entry) = self.shared.tasks.borrow_mut().entries.get_mut(&id) {
            entry.origin = Some(origin);
        }
    }

    /// Queue a task with a timer deadline.
    ///
    /// The task will become ready when [`advance_timers`](Self::advance_timers)
    /// detects that the deadline has passed. For `setTimeout(fn, 0)` or
    /// similar, use a deadline of `Instant::now()`.
    pub fn queue_timer(&self, task: Box<dyn Task>, deadline: Instant) -> TaskId {
        let id = self.next_task_id();
        self.insert_timer(id, task, deadline, None, true, Snapshot::capture());
        self.record_origin(id);
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
        keeps_alive: bool,
        context: Snapshot,
    ) {
        self.shared.tasks.borrow_mut().insert(TaskEntry {
            id,
            task,
            ready: false,
            deadline: Some(deadline),
            origin,
            keeps_alive,
            context,
        });
        self.notify.notify(1);
    }

    /// Cancel a queued task, removing it from the event loop.
    ///
    /// Returns `true` if the task was found and removed. A task that is not
    /// in the queue (already run, or currently being executed by the driver)
    /// is left alone.
    pub fn cancel_if_queued(&self, id: TaskId) -> bool {
        self.shared.tasks.borrow_mut().remove(id).is_some()
    }

    /// Queue a `setTimeout`/`setInterval` task under a fresh JS timer id.
    pub fn queue_js_timer(
        &self,
        deadline: Instant,
        make_task: impl FnOnce(u64) -> Box<dyn Task>,
    ) -> u64 {
        self.queue_js_timer_with_id(None, deadline, make_task)
    }

    /// Queue a `setTimeout`/`setInterval` task under the JS timer id `id`, which must not belong
    /// to a queued timer, or under a fresh id if `id` is `None`. Returns the id.
    pub fn queue_js_timer_with_id(
        &self,
        id: Option<u64>,
        deadline: Instant,
        make_task: impl FnOnce(u64) -> Box<dyn Task>,
    ) -> u64 {
        let timer_id = id.unwrap_or_else(reserve_js_timer_id);
        let task = self.next_task_id();
        let origin = js::stack::current_origin();
        let context = Snapshot::capture();
        let made = make_task(timer_id);
        self.insert_timer(task, made, deadline, origin.clone(), true, context.clone());
        self.shared.js_timers.borrow_mut().insert(
            timer_id,
            JsTimer {
                task,
                origin,
                keeps_alive: true,
                context,
            },
        );
        self.shared.register_js_timer(timer_id);
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
        let (origin, keeps_alive, context) = match self.shared.js_timers.borrow().get(&timer_id) {
            Some(timer) if timer.task == id => (
                timer.origin.clone(),
                timer.keeps_alive,
                timer.context.clone(),
            ),
            _ => return,
        };
        self.insert_timer(id, task, deadline, origin, keeps_alive, context);
    }

    /// Run `op` on the tasks of the loop that holds the JS timer `timer_id`, and return its
    /// result: this loop's if it holds the timer, and otherwise those of the live loop
    /// [`JS_TIMER_LOOPS`] maps it to, whose driver is woken afterwards. Returns `false` if no live
    /// loop holds the timer.
    fn with_js_timer_loop(&self, timer_id: u64, op: impl FnOnce(&Rc<LoopTasks>) -> bool) -> bool {
        if self.shared.js_timers.borrow().contains_key(&timer_id) {
            return op(&self.shared);
        }
        let owner =
            JS_TIMER_LOOPS.with(|loops| loops.borrow().get(&timer_id).and_then(Weak::upgrade));
        let Some(owner) = owner else {
            return false;
        };
        let done = op(&owner);
        owner.notify.notify(1);
        done
    }

    /// Set whether the JS timer `timer_id` keeps the event loop that holds it alive, which it does
    /// when created. An unreferenced timer still runs while other work keeps the loop alive.
    ///
    /// Invalid IDs are ignored.
    pub fn set_js_timer_ref(&self, timer_id: u64, referenced: bool) {
        self.with_js_timer_loop(timer_id, |tasks| {
            tasks.set_js_timer_ref(timer_id, referenced)
        });
    }

    /// Make the queued JS timer `timer_id` due `delay` from now instead, on the event loop that
    /// holds it. Returns `false`, and changes nothing, if no loop holds the timer queued, which
    /// includes a timer whose callback is running.
    pub fn restart_js_timer(&self, timer_id: u64, delay: Duration) -> bool {
        let deadline = Instant::now() + delay;
        self.with_js_timer_loop(timer_id, |tasks| {
            tasks.reschedule_js_timer(timer_id, deadline)
        })
    }

    /// Whether the JS timer `timer_id` is queued or running.
    pub fn has_js_timer(&self, timer_id: u64) -> bool {
        self.shared.js_timers.borrow().contains_key(&timer_id)
    }

    /// The ids of the JS timers that are queued or running and keep the loop alive.
    pub fn referenced_js_timers(&self) -> Vec<u64> {
        self.shared
            .js_timers
            .borrow()
            .iter()
            .filter(|(_, timer)| timer.keeps_alive)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Cancel a JS timer by its `setTimeout`/`setInterval` id, on the event loop that holds it.
    ///
    /// Invalid IDs are silently ignored per spec.
    pub fn clear_js_timer(&self, timer_id: u64) {
        self.with_js_timer_loop(timer_id, |tasks| tasks.clear_js_timer(timer_id));
    }

    /// Release a one-shot JS timer's id after its task `task` fired. An id that was queued again
    /// under another task while `task` ran stays.
    ///
    /// Mirrors HTML's "remove global's map of `setTimeout` and `setInterval`
    /// `IDs[id]`" task substep for non-repeating timers.
    pub fn js_timer_fired(&self, timer_id: u64, task: TaskId) {
        let mut timers = self.shared.js_timers.borrow_mut();
        if timers
            .get(&timer_id)
            .is_some_and(|timer| timer.task == task)
        {
            timers.remove(&timer_id);
            drop(timers);
            self.shared.unregister_js_timer(timer_id);
        }
    }

    /// Mark a queued task as ready to run.
    ///
    /// Has no effect if the task ID is not found (the task may have
    /// already been cancelled or run).
    pub fn signal_ready(&self, id: TaskId) {
        if self.shared.tasks.borrow_mut().mark_ready(id) {
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
        let mut tasks = self.shared.tasks.borrow_mut();
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
        self.shared.tasks.borrow_mut().expire_timers(Instant::now())
    }

    /// Returns the duration until the next timer fires, or `None` if
    /// there are no pending timers.
    ///
    /// A return value of `Duration::ZERO` (or very small) means a timer
    /// is already expired and [`advance_timers`](Self::advance_timers)
    /// should be called.
    pub fn time_to_next_timer(&self) -> Option<Duration> {
        let &(deadline, _) = self.shared.tasks.borrow().timers.first()?;
        Some(deadline.saturating_duration_since(Instant::now()))
    }

    /// Returns `true` if any task (ready or not) in the queue keeps the loop alive. Every task
    /// does except unreferenced JS timers.
    pub fn has_pending(&self) -> bool {
        self.shared.tasks.borrow().keep_alive > 0
    }

    /// Returns `true` if at least one task is in the ready state.
    pub fn has_ready(&self) -> bool {
        !self.shared.tasks.borrow().ready.is_empty()
    }

    /// Returns the number of tasks currently queued (ready or not).
    pub fn len(&self) -> usize {
        self.shared.tasks.borrow().entries.len()
    }

    /// Returns `true` if the event loop has no tasks.
    pub fn is_empty(&self) -> bool {
        self.shared.tasks.borrow().entries.is_empty()
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

    /// `Ok` if this loop holds no [`pending_work`](Self::pending_work), and otherwise `Err`
    /// describing each piece of it and where it was created, for a caller about to take a Wizer
    /// snapshot, which cannot hold that work.
    ///
    /// Timers, referenced and unreferenced, are not reported: a snapshot holds them, and an
    /// instance resumed from it runs each one when the time it had left has passed. The work
    /// that is reported holds host resources, such as I/O in flight, that a resumed instance
    /// doesn't have.
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

    /// Everything that keeps this loop alive except its timers: its tasks without a deadline, its
    /// external interest, the async-promise futures it owns, and its pending sends. Each entry's
    /// origin is set if origins were being recorded when the work was created (see
    /// [`js::stack::record_origins`]).
    ///
    /// Tasks come first, in the order they were queued, then interest, then futures, then sends.
    pub fn pending_work(&self) -> Vec<PendingWork> {
        let tasks = self.shared.tasks.borrow();
        let mut entries: Vec<&TaskEntry> = tasks
            .entries
            .values()
            .filter(|entry| entry.deadline.is_none())
            .collect();
        entries.sort_unstable_by_key(|entry| entry.id.0);
        let mut work: Vec<PendingWork> = entries
            .into_iter()
            .map(|entry| PendingWork {
                kind: PendingKind::Task(entry.task.kind()),
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

/// Run the event loop as [`run_to_completion`] does, except that unreferenced JS timers count as
/// work: the loop keeps running while it holds only those, and returns once it holds no task, no
/// interest and no futures in flight.
///
/// # Safety
///
/// `raw_cx` must be a valid JSContext pointer that remains valid for the
/// lifetime of this future (i.e. the `Runtime` must not be dropped).
pub async unsafe fn run_until_empty<S, F>(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    sleep: S,
) where
    S: Fn(Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    unsafe { drive(raw_cx, event_loop, sleep, |_| false, true).await }
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
    unsafe { drive(raw_cx, event_loop, sleep, should_stop, false).await }
}

/// [`run_until`], which with `keep_unreferenced` also keeps running while the loop holds only
/// unreferenced JS timers.
///
/// # Safety
///
/// As for [`run_until`].
async unsafe fn drive<S, F>(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    sleep: S,
    should_stop: impl Fn(&Scope<'_>) -> bool,
    keep_unreferenced: bool,
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

        // With `keep_unreferenced`, a loop that is `Done` but not empty holds only unreferenced
        // timers, and the await arm below waits for the next one.
        match outcome {
            StepOutcome::Done
                if !have_futures && !(keep_unreferenced && !event_loop.is_empty()) =>
            {
                return
            }
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
                let wait_start = platform::clock::monotonic_ns();
                // Race: first one to complete wins.
                futures_lite::future::or(
                    futures_lite::future::or(timer_wait, notified),
                    drive_futures,
                )
                .await;
                let waited = platform::clock::monotonic_ns().saturating_sub(wait_start);
                event_loop
                    .idle_ns
                    .set(event_loop.idle_ns.get().saturating_add(waited));

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
pub fn handle_and_clear_exception(scope: &Scope<'_>) {
    if !js::exception::is_pending(scope) {
        return;
    }
    if crate::lifecycle::has_hooks() {
        if let Some(error) = js::exception::take_pending(scope) {
            let error = scope.root_value(error.get());
            if crate::lifecycle::uncaught(scope, error, None) {
                return;
            }
            js::exception::throw_value(scope, error);
        }
    }
    let captured = js::error::ExnThrown::capture(scope);
    crate::diagnostics::report(&format!("Uncaught {}", captured.to_string().trim_end()));
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
        if self.started_ns.get().is_none() {
            self.started_ns.set(Some(platform::clock::monotonic_ns()));
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
        batch.extend(
            self.shared
                .tasks
                .borrow()
                .ready
                .iter()
                .map(|&(_, id)| TaskId(id)),
        );
        let mut ran_any = false;
        for &id in &batch {
            // Take the task out before running it, ending the borrow: the
            // task's JS can re-enter this loop (setTimeout, clearTimeout)
            // through `with_active_event_loop`. Re-locate by id — an earlier
            // task in the batch may have cancelled this one.
            let Some(entry) = self.shared.tasks.borrow_mut().remove(id) else {
                continue;
            };
            let previous = entry.context.enter_callback(scope);
            let result = entry.task.run(scope, id);
            // An exception the task threw is handled with the task's async context current.
            if result.is_err() {
                handle_and_clear_exception(scope);
            }
            previous.leave_callback(scope);
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
        for entry in self.shared.tasks.borrow().entries.values() {
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

impl Drop for EventLoop {
    fn drop(&mut self) {
        let ids: Vec<u64> = self.shared.js_timers.borrow().keys().copied().collect();
        for id in ids {
            self.shared.unregister_js_timer(id);
        }
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
/// [`report_unhandled_rejections`], and the microtasks the lifecycle hooks
/// queued while it ran are drained as well. The messages
/// [`crate::diagnostics::report_later`] queued are reported last.
pub fn run_microtasks(scope: &Scope<'_>) {
    debug_assert!(
        !js::exception::is_pending(scope),
        "Cannot run microtasks with pending exception"
    );
    crate::finalization::run_pending(scope);
    loop {
        jobs::run_jobs(scope);
        // Weak-ref set is cleared by run_jobs.
        if !report_unhandled_rejections(scope) || !jobs::has_pending_jobs(scope) {
            break;
        }
    }
    crate::diagnostics::report_queued();
}

/// Call the `rejection_handled` lifecycle hook for each promise that got a handler after this
/// reported its rejection. Then report each promise that was rejected with no handler attached,
/// and still has none, to the `uncaught` lifecycle hook, and with the rejection reason's message
/// and stack (see [`crate::diagnostics`]) if the hook doesn't handle it. The `uncaught` hook runs
/// with the async context that was current when the promise was rejected. Returns whether there
/// was such a promise of either kind.
pub fn report_unhandled_rejections(scope: &Scope<'_>) -> bool {
    let handled = jobs::take_handled_rejections(scope);
    let mut reported = !handled.is_empty();
    for (promise, id) in handled {
        crate::lifecycle::rejection_handled(scope, promise, id);
    }
    let rejections = jobs::take_unhandled_rejections(scope);
    reported |= !rejections.is_empty();
    for rejection in rejections {
        let promise = rejection.promise;
        let Some(reason) = promise.result(scope) else {
            continue;
        };
        let promise_value = js::conversion::ToJSVal::to_jsval_throwing(&promise, scope);
        if let Ok(promise_value) = promise_value {
            let previous = rejection.context.enter();
            let hooked = crate::lifecycle::Rejection {
                promise: promise_value,
                id: rejection.id,
            };
            let handled = crate::lifecycle::uncaught(scope, reason, Some(hooked));
            previous.enter();
            if handled {
                continue;
            }
        }
        js::exception::set_pending(scope, reason, js::native::ExceptionStackBehavior::Capture);
        let captured = js::error::ExnThrown::capture(scope);
        crate::diagnostics::report(&format!(
            "Uncaught (in promise) {}",
            captured.to_string().trim_end()
        ));
    }
    reported
}
