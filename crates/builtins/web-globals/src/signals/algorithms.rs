// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Abort signal algorithms from <https://dom.spec.whatwg.org>.

use platform::clock::Instant;
use std::time::Duration;

use core_runtime::event_loop::{with_active_event_loop, Task, TaskId};
use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::JSTracer;
use js::prelude::HandleValue;
use js::Function;

use super::abort_signal::{AbortAlgorithm, AbortSignal, AbortSignalImpl};
use crate::dom_exception::DOMException;
use crate::events;
use crate::events::algorithms::ScriptStackState;
use crate::events::event_target::EventTarget;

/// <https://dom.spec.whatwg.org/#create-a-dependent-abort-signal>
///
/// To create a dependent abort signal, given a list `signals`, returns a new
/// `AbortSignal` that is aborted when any of the source signals is aborted.
///
/// The spec's links between source and dependent signals are weak references. Here they are
/// traced `Heap` fields, which a tracing GC still collects when the linked signals are all
/// unreachable. A reachable source signal keeps every signal in its `dependent_signals` alive.
pub fn create_dependent_abort_signal<'r>(
    scope: &'r Scope<'_>,
    signals: &[AbortSignal<'_>],
) -> Result<AbortSignal<'r>, ExnThrown> {
    // Step 1: Let _resultSignal_ be a new object implementing _signalInterface_ using _realm_.
    let result = AbortSignal::new(scope)?;

    // Step 2: `For each` _signal_ of _signals_: if _signal_ is `aborted`, then set
    //         _resultSignal_'s `abort reason` to _signal_'s `abort reason` and return
    //         _resultSignal_.
    for signal in signals {
        if !signal.data().abort_reason.is_undefined() {
            let reason = signal.data().abort_reason.get(scope);
            result.data_mut().abort_reason.set(reason.get());
            return Ok(result);
        }
    }

    // Step 3: Set _resultSignal_'s `dependent` to true.
    result.data_mut().dependent = true;

    // Step 4: `For each` _signal_ of _signals_:
    for signal in signals {
        // Step 4.1: If _signal_'s `dependent` is false, then:
        if !signal.data().dependent {
            // Step 4.1.1: `Append` _signal_ to _resultSignal_'s `source signals`.
            result.data_mut().source_signals.push(Heap::from(*signal));
            // Step 4.1.2: `Append` _resultSignal_ to _signal_'s `dependent signals`.
            signal.data_mut().dependent_signals.push(Heap::from(result));
        } else {
            // Step 4.2: Otherwise, `for each` _sourceSignal_ of _signal_'s `source signals`:
            let sources: Vec<AbortSignal<'_>> = signal
                .data()
                .source_signals
                .iter()
                .map(|h| h.get(scope))
                .collect();
            for source in sources {
                // Step 4.2.1: `Assert`: _sourceSignal_ is not `aborted` and not `dependent`.
                // Step 4.2.2: If _resultSignal_'s `source signals` `does not contain`
                //             _sourceSignal_, then:
                let contains = result
                    .data()
                    .source_signals
                    .iter()
                    .any(|h| source.eq_heap(h));
                if !contains {
                    // Step 4.2.2.1: `Append` _sourceSignal_ to _resultSignal_'s `source signals`.
                    result.data_mut().source_signals.push(Heap::from(source));
                    // Step 4.2.2.2: `Append` _resultSignal_ to _sourceSignal_'s
                    //               `dependent signals`.
                    source.data_mut().dependent_signals.push(Heap::from(result));
                }
            }
        }
    }

    // Step 5: Return _resultSignal_.
    Ok(result)
}

/// <https://dom.spec.whatwg.org/#abortsignal-signal-abort>
///
/// To signal abort, given an AbortSignal object signal and an optional reason.
pub(crate) fn signal_abort<'s>(
    scope: &'s Scope<'_>,
    signal: &AbortSignal<'_>,
    reason: impl ToJSVal<'s>,
    script_stack_state: ScriptStackState,
) -> Result<(), ExnThrown> {
    // Step 1: If _signal_ is `aborted`, then return.
    if !signal.data().abort_reason.is_undefined() {
        return Ok(());
    }

    // Step 2: Set _signal_'s `abort reason` to _reason_ if it is given; otherwise to a new
    //         "``AbortError``" ``DOMException``.
    let reason = reason.to_jsval_throwing(scope)?;
    if !reason.is_undefined() {
        signal.data_mut().abort_reason.set(reason.get());
    } else {
        let err_val = DOMException::new(
            scope,
            "signal is aborted without reason".into(),
            "AbortError".into(),
        )?;
        signal.data_mut().abort_reason.set(err_val.as_value());
    }

    // Step 3: Let _dependentSignalsToAbort_ be a new `list`.
    let mut dependent_signals_to_abort: Vec<AbortSignal<'_>> = Vec::new();

    // Step 4: `For each` _dependentSignal_ of _signal_'s `dependent signals`: If
    //         _dependentSignal_ is not `aborted`: Set _dependentSignal_'s `abort reason` to
    //         _signal_'s `abort reason`. `Append` _dependentSignal_ to _dependentSignalsToAbort_.
    let abort_reason = signal.data().abort_reason.get(scope);
    let dependents: Vec<AbortSignal<'_>> = signal
        .data()
        .dependent_signals
        .iter()
        .map(|h| h.get(scope))
        .collect();
    for dependent in dependents {
        if dependent.data().abort_reason.is_undefined() {
            dependent.data_mut().abort_reason.set(abort_reason.get());
            dependent_signals_to_abort.push(dependent);
        }
    }

    // Step 5: `Run the abort steps` for _signal_.
    run_the_abort_steps(scope, signal, script_stack_state)?;

    // Step 6: `For each` _dependentSignal_ of _dependentSignalsToAbort_, `run the abort steps` for
    //         _dependentSignal_.
    for dependent in &dependent_signals_to_abort {
        run_the_abort_steps(scope, dependent, script_stack_state)?;
    }

    Ok(())
}

/// <https://dom.spec.whatwg.org/#run-the-abort-steps>
///
/// To run the abort steps for an AbortSignal _signal_.
fn run_the_abort_steps(
    scope: &Scope<'_>,
    signal: &AbortSignal<'_>,
    script_stack_state: ScriptStackState,
) -> Result<(), ExnThrown> {
    // Snapshot the abort algorithms, then run step two before step 1 to avoid reentrant signals
    // creating loops.
    enum Pending<'s> {
        RemoveListener(EventTarget<'s>, u64),
        RunSteps(Function<'s>),
    }
    let pending: Vec<Pending<'_>> = signal
        .data()
        .abort_algorithms
        .iter()
        .map(|algorithm| match algorithm {
            AbortAlgorithm::RemoveListener { held, listener_id } => {
                Pending::RemoveListener(held.get(scope), *listener_id)
            }
            AbortAlgorithm::RunSteps(callback) => Pending::RunSteps(callback.get(scope)),
        })
        .collect();

    // Step 2: `Empty` _signal_'s `abort algorithms` (before running, per above).
    signal.data_mut().abort_algorithms.clear();

    let reason = signal.data().abort_reason.get(scope);
    // Step 1: `For each` _algorithm_ of _signal_'s `abort algorithms`: run _algorithm_.
    for algorithm in &pending {
        match algorithm {
            // Remove the event listener this algorithm was added for.
            Pending::RemoveListener(target, listener_id) => {
                events::algorithms::remove_an_event_listener_by_id(target, *listener_id);
            }
            // Call the registered callback with the signal's abort reason.
            Pending::RunSteps(callback) => {
                // An abort algorithm is infallible in the spec. A throwing
                // callback is reported and must not keep the remaining
                // algorithms, the `abort` event (step 3), or dependent
                // signals' abort steps (signal abort step 6) from running.
                if Function::call(scope, HandleValue::undefined(), callback, &[reason]).is_err() {
                    js::exception::report_and_clear(scope, "abort algorithm");
                }
                script_stack_state.clean_up_after_running_script(scope);
            }
        }
    }

    // Step 3: `Fire an event` named ``abort`` at _signal_.
    events::algorithms::fire_an_event(scope, "abort", signal, script_stack_state)?;
    Ok(())
}

/// <https://dom.spec.whatwg.org/#abortsignal-add>
///
/// Add an abort algorithm to `signal` that removes the event listener
/// identified by `listener_id` from `target` when the signal is aborted. This
/// is the abort step registered by "add an event listener" step 6.
pub(crate) fn add_listener_removal_algorithm(
    signal: &AbortSignal<'_>,
    target: &EventTarget<'_>,
    listener_id: u64,
) {
    // Step 1: If _signal_ is `aborted`, then return.
    if signal.aborted() {
        return;
    }

    // Step 2: `Append` _algorithm_ to _signal_'s `abort algorithms`.
    signal
        .data_mut()
        .abort_algorithms
        .push(AbortAlgorithm::RemoveListener {
            held: Heap::from(*target),
            listener_id,
        });
}

/// <https://dom.spec.whatwg.org/#abortsignal-add>
///
/// Add an abort algorithm to `signal` that calls `callback` (with the abort
/// reason as its argument) when the signal is aborted. For callers such as
/// `fetch` that react to abort.
// TODO: add support for native abort algorithms, with a GC-traced state argument.
pub fn add_abort_algorithm(signal: &AbortSignal<'_>, callback: &Function<'_>) {
    // Step 1: If _signal_ is `aborted`, then return.
    if signal.aborted() {
        return;
    }

    // Step 2: `Append` _algorithm_ to _signal_'s `abort algorithms`.
    signal
        .data_mut()
        .abort_algorithms
        .push(AbortAlgorithm::RunSteps(Heap::from(*callback)));
}

/// How many abort algorithms are registered on `signal` and, transitively, on
/// the dependent signals derived from it.
///
/// Mainly useful in tests that check that abort algorithms don't accumulate over time.
pub fn abort_algorithm_count_including_dependents(
    scope: &Scope<'_>,
    signal: &AbortSignal<'_>,
) -> usize {
    let dependents: Vec<AbortSignal<'_>> = signal
        .data()
        .dependent_signals
        .iter()
        .map(|dependent| dependent.get(scope))
        .collect();
    let own = signal.data().abort_algorithms.len();
    own + dependents
        .iter()
        .map(|dependent| abort_algorithm_count_including_dependents(scope, dependent))
        .sum::<usize>()
}

/// <https://dom.spec.whatwg.org/#abortsignal-remove>
///
/// Remove the run-steps abort algorithm registered for `callback` (matched by
/// object identity). For callers such as pipeTo that detach their abort handler
/// when they finish before the signal aborts. A no-op if the signal already
/// aborted (its abort algorithms were emptied when it ran the abort steps).
pub fn remove_abort_algorithm(signal: &AbortSignal<'_>, callback: &js::Function<'_>) {
    signal
        .data_mut()
        .abort_algorithms
        .retain(|algo| match algo {
            AbortAlgorithm::RunSteps(cb) => !cb.eq_stack(callback),
            AbortAlgorithm::RemoveListener { .. } => true,
        });
}

/// An event-loop task that signals abort on a timeout signal with a
/// "TimeoutError" `DOMException`.
///
/// Holding the signal here keeps it alive until the timeout fires, satisfying
/// the spec requirement that there be a strong reference from the global to a
/// timeout signal for the duration of the timeout.
///
/// The `Heap` is kept alive by the event loop tracing live tasks via
/// [`Task::trace`], so the task itself lives unrooted in the task queue.
#[js::allow_unrooted_interior]
struct AbortTimeoutTask {
    signal: Heap<AbortSignalImpl>,
}

impl Task for AbortTimeoutTask {
    fn kind(&self) -> &'static str {
        "abort-timeout"
    }

    fn run(self: Box<Self>, scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
        let signal: AbortSignal<'_> = self.signal.take(scope);
        // `Queue a global task` [...] to `signal abort` given _signal_ and a new
        // "``TimeoutError``" ``DOMException``.
        let reason = DOMException::new(scope, "signal timed out".into(), "TimeoutError".into())?;
        signal_abort(scope, &signal, reason, ScriptStackState::Empty)
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn trace(&self, trc: *mut JSTracer) {
        unsafe {
            self.signal.trace(trc);
        }
    }
}

/// <https://dom.spec.whatwg.org/#dom-abortsignal-timeout>
///
/// Steps 2-3 of the `AbortSignal.timeout()` algorithm: run steps after a
/// timeout to signal abort with a "TimeoutError" DOMException.
pub(crate) fn schedule_abort_timeout(signal: &AbortSignal<'_>, milliseconds: f64) {
    let delay = Duration::from_millis(milliseconds.max(0.0) as u64);
    let deadline = Instant::now() + delay;
    let task = AbortTimeoutTask {
        signal: Heap::from(*signal),
    };
    // If there is no active event loop (e.g. outside a runtime invocation), the
    // timeout simply never fires.
    with_active_event_loop(|el| {
        el.queue_timer(Box::new(task), deadline);
    });
}
