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
//! `clearTimeout`, and `clearInterval` to a global object. These functions
//! interact with the event loop via [`with_active_event_loop`].

use platform::clock::Instant;
use std::time::Duration;

use js::error::ExnThrown;
use js::gc::handle::RootedHeap;
use js::native::Value;
use js::prelude::HandleValue;

use js::gc::scope::Scope;

use super::{with_active_event_loop, Task, TaskId};

/// A timer's handler, per WebIDL `TimerHandler = (Function or DOMString)`: a JS function to call, or
/// a string to evaluate as a classic script when the timer fires.
enum TimerHandler {
    Function {
        callback: RootedHeap<js::function::Callable>,
        /// The `setTimeout`/`setInterval` arguments after the timeout, passed
        /// to the callback on every invocation (HTML timer initialization
        /// steps: "invoke handler given arguments").
        args: Vec<RootedHeap<Value>>,
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
        } = *self;

        // Run the handler with the global as `this`: call the function, or evaluate the string
        // as a classic script in the global scope.
        // Timers the handler schedules nest one level below this task (see
        // `CURRENT_TIMER_NESTING`); restore the previous level afterwards.
        let previous_nesting = CURRENT_TIMER_NESTING.replace(nesting_level);
        let failed = match &handler {
            TimerHandler::Function { callback, args } => {
                let cb = callback.get(scope);
                let arg_handles: Vec<_> = args.iter().map(|arg| arg.get(scope)).collect();
                js::Function::call(scope, scope.global(), cb, &arg_handles).is_err()
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
        // clearInterval), requeue_js_timer skips it. The repeat re-enters the timer initialization
        // steps, so the nested-timer clamp applies with this task's nesting level, and the
        // re-queued task is one level deeper.
        if let Some(interval) = interval {
            let delay = clamp_nested_timeout(nesting_level, interval);
            let new_task = TimerTask {
                handler,
                interval: Some(interval),
                nesting_level: nesting_level.saturating_add(1),
                timer_id,
            };
            with_active_event_loop(|el| {
                el.requeue_js_timer(id, Box::new(new_task), Instant::now() + delay);
            });
        } else {
            // One-shot: release the timer id, mirroring HTML's "remove
            // global's map of setTimeout and setInterval IDs[id]" substep
            // after the handler ran.
            with_active_event_loop(|el| el.js_timer_fired(id));
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

/// Install `setTimeout`, `setInterval`, `clearTimeout`, and `clearInterval`
/// on a global object.
pub fn install_timer_globals(scope: &Scope<'_>, global: js::Object<'_>) {
    timer_globals::add_to_global(scope, global);
}

/// WebIDL `TimerHandler = (Function or DOMString)`.
#[js::macros::webidl_union]
pub enum TimerHandlerArg<'a> {
    Function(js::Callable<'a>),
    String(String),
}

/// The timer methods of `WindowOrWorkerGlobalScope`.
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
    pub fn clear_timeout(scope: &Scope<'_>, id: Option<u64>) {
        // `remove` `this`'s `map of setTimeout and setInterval IDs`[_id_].
        super::with_active_event_loop(|el| el.clear_js_timer(scope, id.unwrap_or(0)));
    }

    /// <https://html.spec.whatwg.org/#dom-clearinterval>
    pub fn clear_interval(scope: &Scope<'_>, id: Option<u64>) {
        // `remove` `this`'s `map of setTimeout and setInterval IDs`[_id_].
        super::with_active_event_loop(|el| el.clear_js_timer(scope, id.unwrap_or(0)));
    }
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
    let extra_args: Vec<RootedHeap<Value>> = match &callable {
        Some(_) => arguments
            .iter()
            .map(|arg| RootedHeap::from(arg.get()))
            .collect(),
        None => Vec::new(),
    };

    // Queue on the current event loop.
    let timer_id = with_active_event_loop(|el| {
        el.queue_js_timer(scope, deadline, |timer_id| {
            let handler = match callable {
                Some(callback) => TimerHandler::Function {
                    callback: RootedHeap::new(callback),
                    args: extra_args,
                },
                // Not callable: run the handler's string as code. The HTML timer initialization
                // steps create a new classic script from it each time the timer fires, so the
                // string is kept rather than a compiled script.
                None => TimerHandler::Code(
                    code.expect("non-callable handler was converted to a string above"),
                ),
            };
            Box::new(TimerTask {
                handler,
                interval,
                nesting_level: task_nesting,
                timer_id,
            })
        })
    });

    timer_id.ok_or_else(|| {
        let what = if repeat { "setInterval" } else { "setTimeout" };
        super::throw_no_event_loop(scope, what)
    })
}
