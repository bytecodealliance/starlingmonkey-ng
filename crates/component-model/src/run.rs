// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Driving a CLI application's `run` export.
//!
//! `wasi:cli/run.run()` dispatches to a JavaScript `run` function the
//! application exports from its main module, resolved as a flat `ns.run` rather
//! than an interface-qualified member. [`run_main_export`] resolves and drives
//! it for the runtime's own `wasi:cli/run` export in an instance whose top level
//! ran before a snapshot: a componentized command, or a script wizened with
//! `wizer-initialize`. [`report_run_outcome`] maps the result.

use core_runtime::event_loop::{run_until, with_event_loop};
use core_runtime::invocation::OwnedInvocation;

use crate::exports::{
    as_promise, promise_is_settled, resolve_export, settled_promise_outcome, Outcome, ResultShape,
};
use crate::init;
use crate::stack::CallStack;

/// The context a failed `run` is reported with, ahead of the error's message,
/// location and stack.
const RUN_FAILED: &str = "run() failed";

/// The outcome of forwarding `wasi:cli/run.run()` to the JS `run` export.
pub enum RunOutcome {
    /// The application exports no `run` function.
    NoRunExport,
    /// `run()` settled successfully.
    Ok,
    /// `run()` threw, rejected, or the runtime trapped driving it.
    Failed(String),
}

/// Reduce a [`RunOutcome`] to a `wasi:cli/run.run()` result, reporting any
/// failure on stderr. `NoRunExport` maps to a clean `Err`, so a caller that
/// instead treats a missing `run` as a plain top-level script must handle it
/// before calling this.
///
/// `Result<(), ()>` is the canonical-ABI shape of `wasi:cli/run.run`'s declared
/// `result`, which has no payloads, so the `result_unit_err` lint is suppressed.
#[allow(clippy::result_unit_err)]
pub fn report_run_outcome(outcome: RunOutcome) -> Result<(), ()> {
    match outcome {
        RunOutcome::Ok => Ok(()),
        RunOutcome::Failed(message) => {
            eprintln!("{message}");
            Err(())
        }
        RunOutcome::NoRunExport => {
            eprintln!("this component exports no `run` function and cannot be run as a CLI tool");
            Err(())
        }
    }
}

/// Whether the loop still has Component Model work in flight, such as an async
/// import's subtask. A bare timer is not, so an abandoned `setInterval` does not
/// keep this true. Stream pumps and future writes are pending sends, which
/// [`drain_async_work`] finishes separately.
pub(crate) fn has_async_work(event_loop: &core_runtime::event_loop::EventLoop) -> bool {
    event_loop.has_interest() || event_loop.has_active_external_async_tasks()
}

/// Where [`run_main_export`] finds the application's main module.
#[derive(Clone, Copy)]
pub enum MainModule {
    /// The main module [`initialize_runtime`](crate::init::initialize_runtime)
    /// evaluated, which must have completed.
    Bootstrap,
    /// The entry module of the runtime whose context this is (see
    /// [`core_runtime::module::entry_namespace`]). Its default global's realm
    /// must be entered, and the runtime kept alive, for as long as the run
    /// lasts.
    Entry(*mut js::native::RawJSContext),
}

impl MainModule {
    /// The context of the runtime the main module belongs to.
    fn raw_cx(self) -> *mut js::native::RawJSContext {
        match self {
            MainModule::Bootstrap => init::raw_cx(),
            MainModule::Entry(raw_cx) => raw_cx,
        }
    }

    /// Run `f` with a scope on the main module's realm and its namespace, or
    /// `None` if there is no main module.
    fn with_namespace<R>(
        self,
        f: impl FnOnce(&js::gc::scope::Scope<'_>, Option<&js::Object<'_>>) -> R,
    ) -> R {
        match self {
            MainModule::Bootstrap => init::with_main_scope(|scope, ns| f(scope, Some(ns))),
            MainModule::Entry(raw_cx) => {
                // SAFETY: the realm of `raw_cx`'s default global is entered, and its runtime
                // alive, as `MainModule::Entry` requires.
                let scope = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
                let ns = core_runtime::module::entry_namespace(&scope);
                f(&scope, ns.as_ref())
            }
        }
    }
}

/// Forward `wasi:cli/run.run()` to the application's exported `run` function.
///
/// Resolves `run` on the namespace of `main` and calls it under `invocation`'s
/// event loop, driving the loop until the returned promise settles. Returns
/// [`RunOutcome::NoRunExport`] when the application exports no `run`, leaving
/// the caller to decide how to treat that.
pub async fn run_main_export(invocation: &OwnedInvocation, main: MainModule) -> RunOutcome {
    // Keep the value `run` returns alive across the awaits below. `CallStack`
    // registers its boxed interior with the live-stacks tracer, so a compacting
    // GC updates the slot in place. Nothing traces a bare `Heap` local.
    let mut stack = CallStack::new();

    // Phase 1: resolve and call `run`, pushing the value it produced onto the
    // traced stack.
    let phase1 = main.with_namespace(|scope, ns| {
        let Some(ns) = ns else {
            return Phase1::NoRunExport;
        };
        with_event_loop(invocation.state().event_loop(), |_| {
            // A missing `run` surfaces as a pending `TypeError`, which is
            // treated as "no run export" rather than a hard failure.
            let resolved = match resolve_export(scope, ns, "run") {
                Ok(resolved) => resolved,
                Err(js::error::ExnThrown) => {
                    js::exception::clear(scope);
                    return Phase1::NoRunExport;
                }
            };

            // `wasi:cli/run.run` is `async func() -> result`, and it takes no
            // arguments. Its result is handled as a plain one, so any throw fails
            // the run with the thrown error, which is reported on stderr.
            let outcome =
                match crate::exports::call_export(scope, resolved, &[], &ResultShape::Plain) {
                    Ok(outcome) => outcome,
                    Err(captured) => return Phase1::Failed(captured.with_context(RUN_FAILED)),
                };

            // May settle a `Promise.resolve()`-style promise before the pending
            // check below.
            core_runtime::event_loop::run_microtasks(scope);

            match outcome {
                Outcome::Returned(value) => {
                    let pending = !promise_is_settled(scope, value.get());
                    stack.push_value(value.get());
                    Phase1::Returned { pending }
                }
                // Unreachable: a plain result absorbs no throw.
                Outcome::Threw(_) => Phase1::Failed("run() returned an error".to_string()),
            }
        })
    });

    match phase1 {
        Phase1::NoRunExport => return RunOutcome::NoRunExport,
        Phase1::Failed(message) => return RunOutcome::Failed(message),
        Phase1::Returned { pending } => {
            if pending {
                // Phase 2: drive this call's loop until `run`'s promise settles.
                drive_until_top_settled(main.raw_cx(), invocation, &stack).await;
            }
        }
    }

    // Phase 3: read the result off the stack. `run_until` also returns when the
    // loop goes idle, so the promise can still be pending.
    let outcome = main.with_namespace(|scope, _ns| {
        match as_promise(scope, stack.pop_value()) {
            Some(promise) if promise.is_pending() => RunOutcome::Failed(
                "run() did not settle: the event loop went idle while the promise it returned \
                 was still pending"
                    .to_string(),
            ),
            Some(promise) if promise.is_rejected() => {
                let reason = promise
                    .result(scope)
                    .expect("a rejected promise has a reason");
                RunOutcome::Failed(crate::exports::rejection_message(scope, reason, RUN_FAILED))
            }
            Some(promise) => match settled_promise_outcome(scope, &promise, &ResultShape::Plain) {
                Ok(Outcome::Returned(_)) => RunOutcome::Ok,
                // Unreachable: a fulfilled promise is returned, and a plain result
                // absorbs no throw.
                Ok(Outcome::Threw(_)) => RunOutcome::Failed("run() returned an error".to_string()),
                Err(message) => RunOutcome::Failed(message),
            },
            // A plain sync `run` returned a non-promise.
            None => RunOutcome::Ok,
        }
    });

    // Phase 4: drive trailing async imports and stream or future pumps before
    // returning.
    drain_async_work(main.raw_cx(), invocation).await;

    outcome
}

/// Drive `invocation`'s event loop until the value on top of `stack` is not a
/// pending promise. `raw_cx` is the context of `invocation`'s runtime, whose
/// default global's realm must be entered.
pub(crate) async fn drive_until_top_settled(
    raw_cx: *mut js::native::RawJSContext,
    invocation: &OwnedInvocation,
    stack: &CallStack,
) {
    // SAFETY: `raw_cx`'s default-global realm is entered, as the caller
    // guarantees, and `invocation`'s `Rc<Runtime>` clone holds the runtime that
    // owns it alive for the whole await.
    unsafe {
        run_until(
            raw_cx,
            invocation.state().event_loop(),
            platform::clock::sleep,
            |scope| promise_is_settled(scope, stack.last()),
        )
        .await;
    }
}

/// Drive `invocation`'s event loop until [`has_async_work`] is false: async
/// import subtasks and stream or future pumps, such as the transfer a stream or
/// future export runs after returning its readable end. The sends registered on
/// the loop, such as the body of a returned `Response`, are finished first (see
/// [`core_runtime::event_loop::finish_pending_sends`]).
///
/// Per the canonical ABI a subtask completes once `task-return` has run and no
/// waitables remain, and bare timers are not waitables. Driving to full idle
/// would let a perpetual `setInterval` pin the caller forever, so work deferred
/// behind a timer is not awaited. It is dropped with `invocation`.
///
/// `raw_cx` is the context of `invocation`'s runtime, whose default global's
/// realm must be entered.
pub(crate) async fn drain_async_work(
    raw_cx: *mut js::native::RawJSContext,
    invocation: &OwnedInvocation,
) {
    let event_loop = invocation.state().event_loop();
    // The work drained can register more sends, such as a stream pump the JS
    // a timer runs starts, and those are finished in turn.
    loop {
        // SAFETY: as in `drive_until_top_settled`.
        unsafe {
            core_runtime::event_loop::finish_pending_sends(
                raw_cx,
                event_loop,
                platform::clock::sleep,
            )
            .await;
            run_until(raw_cx, event_loop, platform::clock::sleep, |_scope| {
                !has_async_work(event_loop) || event_loop.has_pending_sends()
            })
            .await;
        }
        if !event_loop.has_pending_sends() {
            break;
        }
    }
}

/// Phase-1 classification of the value `run` produced.
enum Phase1 {
    /// No `run` export on the namespace.
    NoRunExport,
    /// `run` threw synchronously or trapped, with the message.
    Failed(String),
    /// `run` returned a value, now on top of the traced call stack.
    Returned { pending: bool },
}
