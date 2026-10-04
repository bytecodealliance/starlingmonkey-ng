// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Timer tasks for `setTimeout` and `setInterval`.
//!
//! A [`TimerTask`] holds a reference to a JS callback function and fires
//! it when the event loop runs the task. For `setInterval`, the task
//! re-queues itself with the same delay after each execution.
//!
//! # Global registration
//!
//! Call [`install_timer_globals`] to add `setTimeout`, `setInterval`,
//! `clearTimeout`, `clearInterval`, and `queueMicrotask` to a global object.
//! The timer functions interact with the event loop via
//! [`with_active_event_loop`].

use platform::clock::Instant;
use std::time::Duration;

use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::heap::RootedTraceableBox;
use js::native::Value;
use js::prelude::HandleValue;

use js::gc::scope::Scope;

use super::{with_active_event_loop, Task, TaskId};

/// A timer's handler, per WebIDL `TimerHandler = (Function or DOMString)`: a JS function to call, or
/// a string to evaluate as a classic script when the timer fires.
enum TimerHandler {
    Function {
        callback: RootedTraceableBox<Heap<js::function::Callable>>,
        /// The `this` the callback is called with, or `None` for the global.
        this: Option<RootedTraceableBox<Heap<Value>>>,
        /// The `setTimeout`/`setInterval` arguments after the timeout, passed
        /// to the callback on every invocation (HTML timer initialization
        /// steps: "invoke handler given arguments").
        args: Vec<RootedTraceableBox<Heap<Value>>>,
    },
    Code(String),
}

js::instance_local! {
    /// The HTML "timer nesting level" of the timer task currently running on this
    /// thread (0 outside timer tasks). The timer initialization steps read it to
    /// clamp deeply nested zero-delay timers, and each fired timer's handler runs
    /// with it set to that timer's own level.
    static CURRENT_TIMER_NESTING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// HTML timer initialization steps, step 5: "If nesting level is greater than 5,
/// and timeout is less than 4, then set timeout to 4." Without the clamp, a
/// zero-delay timer chain (`setInterval(f, 0)`, recursive `setTimeout(f, 0)`)
/// is perpetually ready: the loop never reaches its await branch, so
/// async-promise futures (fetch I/O) are never polled and never settle.
fn clamp_nested_timeout(nesting_level: u32, delay: Duration) -> Duration {
    const MIN_NESTED: Duration = Duration::from_millis(4);
    if nesting_level > 5 && delay < MIN_NESTED {
        MIN_NESTED
    } else {
        delay
    }
}

/// A timer task that runs its [`TimerHandler`] when it fires.
///
/// For `setInterval`, `interval` is `Some(duration)` and the task
/// re-queues itself with the same [`TaskId`] after each `run()`.
pub struct TimerTask {
    handler: TimerHandler,
    /// If `Some`, this is a repeating timer (`setInterval`) and will
    /// re-queue itself with this delay after each execution.
    interval: Option<Duration>,
    /// The task's HTML "timer nesting level": one more than the level of the
    /// timer task that scheduled it (0 from non-timer code).
    nesting_level: u32,
    /// The JS-visible timer id this task is registered under in the event
    /// loop's map of setTimeout and setInterval IDs.
    timer_id: u64,
    /// Whether the nested-timer clamp applies when an interval repeats.
    clamped: bool,
}

impl TimerTask {
    /// Create a timer task with a function handler and its call arguments.
    fn function(
        callback: js::Callable<'_>,
        args: Vec<RootedTraceableBox<Heap<Value>>>,
        interval: Option<Duration>,
        nesting_level: u32,
        timer_id: u64,
    ) -> Self {
        Self {
            handler: TimerHandler::Function {
                callback: RootedTraceableBox::new(Heap::from(callback)),
                this: None,
                args,
            },
            interval,
            nesting_level,
            timer_id,
            clamped: true,
        }
    }

    /// Create a timer task whose handler is a code string evaluated when it fires.
    fn code(code: String, interval: Option<Duration>, nesting_level: u32, timer_id: u64) -> Self {
        Self {
            handler: TimerHandler::Code(code),
            interval,
            nesting_level,
            timer_id,
            clamped: true,
        }
    }
}

impl Task for TimerTask {
    fn kind(&self) -> &'static str {
        if self.interval.is_some() {
            "interval"
        } else {
            "timeout"
        }
    }

    fn run(self: Box<Self>, scope: &Scope<'_>, id: TaskId) -> Result<(), ExnThrown> {
        let Self {
            handler,
            interval,
            nesting_level,
            timer_id,
            clamped,
        } = *self;

        // Run the handler: call the function with its `this`, or the global if it has none, or
        // evaluate the string as a classic script in the global scope.
        // Timers the handler schedules nest one level below this task (see
        // `CURRENT_TIMER_NESTING`); restore the previous level afterwards.
        let previous_nesting = CURRENT_TIMER_NESTING.replace(nesting_level);
        let started = Instant::now();
        let failed = match &handler {
            TimerHandler::Function {
                callback,
                this,
                args,
            } => {
                let cb = callback.get(scope);
                let arg_handles: Vec<_> = args.iter().map(|arg| arg.get(scope)).collect();
                match this {
                    Some(this) => js::Function::call(scope, this.get(scope), cb, &arg_handles),
                    None => js::Function::call(scope, scope.global(), cb, &arg_handles),
                }
                .is_err()
            }
            TimerHandler::Code(code) => {
                js::compile::evaluate_with_filename(scope, code, "<timer>", 1).is_err()
            }
        };
        CURRENT_TIMER_NESTING.with(|cell| cell.set(previous_nesting));

        // For setInterval: re-queue ourselves (reusing the same handler) with the same delay and ID
        // — even when the handler threw. The spec's task substeps invoke the handler with "report"
        // (the exception is reported, not fatal) and still perform the repeat step, so a transient
        // error must not kill the interval. If the ID was cancelled during the run (via
        // clearInterval), requeue_timer skips it. The repeat re-enters the timer initialization
        // steps, so the nested-timer clamp applies with this task's nesting level, and the
        // re-queued task is one level deeper. A clamped (HTML) interval is due `delay` after the
        // handler returned. An unclamped (Node.js) one is due `delay` after the handler started,
        // as Node.js schedules it.
        if let Some(interval) = interval {
            let delay = if clamped {
                clamp_nested_timeout(nesting_level, interval)
            } else {
                interval
            };
            let new_task = TimerTask {
                handler,
                interval: Some(interval),
                nesting_level: nesting_level.saturating_add(1),
                timer_id,
                clamped,
            };
            let from = if clamped { Instant::now() } else { started };
            with_active_event_loop(|el| {
                el.requeue_js_timer(timer_id, id, Box::new(new_task), from + delay);
            });
        } else {
            // One-shot: release the timer id, mirroring HTML's "remove
            // global's map of setTimeout and setInterval IDs[id]" substep
            // after the handler ran.
            with_active_event_loop(|el| el.js_timer_fired(timer_id, id));
        }

        if failed {
            // The caller (EventLoop::step) reports and clears the exception.
            return Err(ExnThrown);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Timer global functions (setTimeout, setInterval, etc.)
// ---------------------------------------------------------------------------

/// Install `setTimeout`, `setInterval`, `clearTimeout`, `clearInterval`, and
/// `queueMicrotask` on a global object.
pub fn install_timer_globals(scope: &Scope<'_>, global: js::Object<'_>) {
    timer_globals::add_to_global(scope, global);
}

/// WebIDL `TimerHandler = (Function or DOMString)`.
#[js::macros::webidl_union]
pub enum TimerHandlerArg<'a> {
    Function(js::Callable<'a>),
    String(String),
}

/// The timer and microtask methods of `WindowOrWorkerGlobalScope`.
#[js::macros::jsglobals]
mod timer_globals {
    use js::error::ExnThrown;
    use js::gc::scope::Scope;
    use js::prelude::HandleValue;

    /// <https://html.spec.whatwg.org/#dom-settimeout>
    pub fn set_timeout(
        scope: &Scope<'_>,
        handler: super::TimerHandlerArg<'_>,
        timeout: Option<i32>,
        arguments: RestArgs<HandleValue<'_>>,
    ) -> Result<u64, ExnThrown> {
        // Return the result of running the `timer initialization steps` given `this`, _handler_,
        // _timeout_, _arguments_, and false.
        super::timer_initialization_steps(scope, handler, timeout.unwrap_or(0), &arguments, false)
    }

    /// <https://html.spec.whatwg.org/#dom-setinterval>
    pub fn set_interval(
        scope: &Scope<'_>,
        handler: super::TimerHandlerArg<'_>,
        timeout: Option<i32>,
        arguments: RestArgs<HandleValue<'_>>,
    ) -> Result<u64, ExnThrown> {
        // Return the result of running the `timer initialization steps` given `this`, _handler_,
        // _timeout_, _arguments_, and true.
        super::timer_initialization_steps(scope, handler, timeout.unwrap_or(0), &arguments, true)
    }

    /// <https://html.spec.whatwg.org/#dom-cleartimeout>
    pub fn clear_timeout(id: Option<u64>) {
        // `remove` `this`'s `map of setTimeout and setInterval IDs`[_id_].
        super::with_active_event_loop(|el| el.clear_js_timer(id.unwrap_or(0)));
    }

    /// <https://html.spec.whatwg.org/#dom-clearinterval>
    pub fn clear_interval(id: Option<u64>) {
        // `remove` `this`'s `map of setTimeout and setInterval IDs`[_id_].
        super::with_active_event_loop(|el| el.clear_js_timer(id.unwrap_or(0)));
    }

    /// <https://html.spec.whatwg.org/#dom-queuemicrotask>
    pub fn queue_microtask(scope: &Scope<'_>, callback: js::Callable<'_>) -> Result<(), ExnThrown> {
        // Queue a microtask to invoke _callback_ with « » and "report".
        let job = js::Function::new_callback(scope, c"", 0, super::invoke_microtask, *callback)?;
        js::jobs::queue_microtask(scope, &job)
    }
}

/// Report `error` as uncaught in a microtask, as an exception a `queueMicrotask` callback throws is
/// reported.
pub fn rethrow_in_microtask(
    scope: &Scope<'_>,
    error: js::prelude::HandleValue<'_>,
) -> Result<(), js::error::ExnThrown> {
    let thrower = js::Function::new_callback(scope, c"", 0, rethrow, error)?;
    let job = js::Function::new_callback(scope, c"", 0, invoke_microtask, *thrower)?;
    js::jobs::queue_microtask(scope, &job)
}

/// The body of the function [`rethrow_in_microtask`] queues, whose payload is the error it throws.
fn rethrow(
    scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    payload: js::prelude::HandleValue,
) -> Result<js::native::Value, js::error::ExnThrown> {
    Err(js::exception::throw_value(scope, payload))
}

/// The body of a microtask `queueMicrotask` queued, whose payload is its callback: invoke the
/// callback with no arguments, and report an exception it throws.
fn invoke_microtask(
    scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    payload: js::prelude::HandleValue,
) -> Result<js::native::Value, js::error::ExnThrown> {
    let undefined = scope.root_value(js::value::undefined());
    if js::Function::call(scope, undefined, payload, js::function::EmptyArgs).is_err() {
        super::handle_and_clear_exception(scope);
    }
    Ok(js::value::undefined())
}

/// Start a timer on the active event loop that calls `callback` with `this` and `args` after
/// `delay`, and every `delay` after that if `repeat`. The nested-timer clamp doesn't apply. The
/// timer gets the ID `id`, which must not belong to a queued timer, or a new ID if `id` is `None`.
/// `clearTimeout` and `clearInterval` cancel it by that ID.
///
/// Returns the ID, or `None` if no event loop is active.
pub fn start_timer(
    _scope: &Scope<'_>,
    callback: js::Callable<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
    delay: Duration,
    repeat: bool,
    id: Option<u64>,
) -> Option<u64> {
    let nesting_level = CURRENT_TIMER_NESTING.with(|cell| cell.get());
    let args = args
        .iter()
        .map(|arg| RootedTraceableBox::new(Heap::from(arg.get())))
        .collect();
    let this = RootedTraceableBox::new(Heap::from(this.get()));
    let deadline = Instant::now() + delay;
    with_active_event_loop(|el| {
        el.queue_js_timer_with_id(id, deadline, |timer_id| {
            Box::new(TimerTask {
                handler: TimerHandler::Function {
                    callback: RootedTraceableBox::new(Heap::from(callback)),
                    this: Some(this),
                    args,
                },
                interval: repeat.then_some(delay),
                nesting_level: nesting_level.saturating_add(1),
                timer_id,
                clamped: false,
            })
        })
    })
}

/// <https://html.spec.whatwg.org/#timer-initialisation-steps>, for `setTimeout` (`repeat` false)
/// and `setInterval` (`repeat` true).
fn timer_initialization_steps(
    scope: &Scope<'_>,
    handler: TimerHandlerArg<'_>,
    timeout: i32,
    arguments: &js::class::RestArgs<HandleValue<'_>>,
    repeat: bool,
) -> Result<u64, ExnThrown> {
    // A function handler is invoked when the timer fires, and a string one is evaluated as code.
    let (callable, code) = match handler {
        TimerHandlerArg::Function(callable) => (Some(callable), None),
        TimerHandlerArg::String(code) => (None, Some(code)),
    };

    let delay = Duration::from_millis(timeout.max(0) as u64);

    // HTML timer initialization steps: "Let nesting level be the task's timer nesting level"
    // (the currently running timer task's, or 0), clamp the timeout once nested deeper than
    // five levels, and give the new task a level one greater.
    let nesting_level = CURRENT_TIMER_NESTING.with(|cell| cell.get());
    let delay = clamp_nested_timeout(nesting_level, delay);
    let task_nesting = nesting_level.saturating_add(1);

    let deadline = Instant::now() + delay;

    let interval = repeat.then_some(delay);

    // HTML timer initialization steps: "Let arguments be... the rest of the arguments"
    // For the handler form, everything after the timeout is forwarded to the callback on every
    // invocation. For the string form, additional arguments are ignored.
    let extra_args: Vec<RootedTraceableBox<Heap<Value>>> = match &callable {
        Some(_) => arguments
            .iter()
            .map(|arg| RootedTraceableBox::new(Heap::from(arg.get())))
            .collect(),
        None => Vec::new(),
    };

    // Queue on the current event loop.
    let timer_id = with_active_event_loop(|el| {
        el.queue_js_timer(deadline, |timer_id| match callable {
            Some(callback) => Box::new(TimerTask::function(
                callback,
                extra_args,
                interval,
                task_nesting,
                timer_id,
            )),
            None => {
                // Not callable: run the handler's string as code. The HTML timer initialization
                // steps create a new classic script from it each time the timer fires, so the
                // string is kept rather than a compiled script.
                let code = code.expect("non-callable handler was converted to a string above");
                Box::new(TimerTask::code(code, interval, task_nesting, timer_id))
            }
        })
    });

    timer_id.ok_or_else(|| {
        let what = if repeat { "setInterval" } else { "setTimeout" };
        super::throw_no_event_loop(scope, what)
    })
}
