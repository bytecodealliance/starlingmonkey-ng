// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Calling the `run` function of a CLI application.
//!
//! The runtime's `wasi:cli/run` export calls the `run` function exported by
//! the application's main module as a bare export. [`run_main_export`] calls it
//! and runs the event loop until it settles. [`report_run_outcome`] converts the
//! outcome into the result of `wasi:cli/run.run`.

use core_runtime::event_loop::{run_until, with_event_loop};
use core_runtime::invocation::OwnedInvocation;
use js::conversion::FromJSVal;
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;

use crate::exports::{
    promise_is_settled, resolve_export, settled_promise_outcome, Outcome, ResultShape,
};
use crate::init;

/// The context a failed `run` is reported with, ahead of the error's message,
/// location and stack.
const RUN_FAILED: &str = "run() failed";

/// The outcome of calling the application's `run` function.
pub enum RunOutcome {
    /// The application exports no `run` function.
    NoRunExport,
    /// `run()` settled successfully.
    Ok,
    /// `run()` threw, rejected or didn't settle. Holds the error message.
    Failed(String),
}

/// Convert `outcome` into the result of `wasi:cli/run.run`, and print the error
/// message of a failure to stderr. [`RunOutcome::NoRunExport`] is a failure.
///
/// The return type matches the result type of `wasi:cli/run.run`, whose cases
/// have no values.
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

/// Whether `event_loop` has pending component-model work, such as a call to an
/// async import. Timers don't count. Stream and future writes are pending
/// sends, which aren't checked here.
pub(crate) fn has_async_work(event_loop: &core_runtime::event_loop::EventLoop) -> bool {
    event_loop.has_interest() || event_loop.has_active_external_async_tasks()
}

/// Where [`run_main_export`] finds the application's main module.
#[derive(Clone, Copy)]
pub enum MainModule {
    /// The main module evaluated by
    /// [`initialize_runtime`](crate::init::initialize_runtime), which must have
    /// succeeded.
    Bootstrap,
    /// The entry module of the runtime with this context (see
    /// [`core_runtime::module::entry_namespace`]). The realm of the runtime's
    /// default global must stay entered, and the runtime alive, until the run
    /// ends.
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
            MainModule::Bootstrap => init::with_scope(|scope| {
                f(scope, core_runtime::module::entry_namespace(scope).as_ref())
            }),
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

/// Call the `run` function exported by `main` on the event loop of
/// `invocation`, and run the loop until the returned promise settles. The loop
/// is then run until the remaining async work is done (see `drain_async_work`).
///
/// Returns [`RunOutcome::NoRunExport`] if `main` doesn't export `run`.
pub async fn run_main_export(invocation: &OwnedInvocation, main: MainModule) -> RunOutcome {
    // Phase 1: resolve and call `run`, keeping the value it produced rooted
    // across the awaits below.
    let phase1 = main.with_namespace(|scope, ns| {
        let Some(ns) = ns else {
            return Phase1::NoRunExport;
        };
        with_event_loop(invocation.state().event_loop(), |_| {
            // A missing `run` throws a `TypeError`, which means that there is
            // no `run` export.
            let resolved = match resolve_export(scope, ns, "run") {
                Ok(resolved) => resolved,
                Err(js::error::ExnThrown) => {
                    js::exception::clear(scope);
                    return Phase1::NoRunExport;
                }
            };

            // The result of `run` is handled as a plain value, so any exception
            // fails the run.
            let outcome =
                match crate::exports::call_export(scope, resolved, &[], &ResultShape::Plain) {
                    Ok(outcome) => outcome,
                    Err(captured) => return Phase1::Failed(captured.with_context(RUN_FAILED)),
                };

            // Running microtasks can settle the returned promise before the
            // check below.
            core_runtime::event_loop::run_microtasks(scope);

            match outcome {
                Outcome::Returned(value) => Phase1::Returned {
                    pending: !promise_is_settled(scope, value),
                    value: RootedHeap::from(value.get()),
                },
                // Unreachable for a plain result.
                Outcome::Threw(_) => Phase1::Failed("run() returned an error".to_string()),
            }
        })
    });

    let value = match phase1 {
        Phase1::NoRunExport => return RunOutcome::NoRunExport,
        Phase1::Failed(message) => return RunOutcome::Failed(message),
        Phase1::Returned { pending, value } => {
            if pending {
                // Phase 2: run this call's event loop until `run`'s promise
                // settles.
                drive_until_settled(main.raw_cx(), invocation, |scope| value.get(scope)).await;
            }
            value
        }
    };

    // Phase 3: read the result. `run_until` also returns when the loop goes
    // idle, so the promise can still be pending.
    let outcome = main.with_namespace(|scope, _ns| {
        match js::Promise::from_jsval(scope, value.get(scope), ()).ok() {
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
                // Unreachable for a fulfilled promise.
                Ok(Outcome::Threw(_)) => RunOutcome::Failed("run() returned an error".to_string()),
                Err(message) => RunOutcome::Failed(message),
            },
            // A plain sync `run` returned a non-promise.
            None => RunOutcome::Ok,
        }
    });

    // Phase 4: finish the remaining async work.
    drain_async_work(main.raw_cx(), invocation).await;

    outcome
}

/// Run `invocation`'s event loop until the value returned by `value` isn't a
/// pending promise. `raw_cx` is the context of `invocation`'s runtime, whose
/// default global's realm must be entered.
pub(crate) async fn drive_until_settled(
    raw_cx: *mut js::native::RawJSContext,
    invocation: &OwnedInvocation,
    value: impl for<'s> Fn(&'s Scope<'_>) -> HandleValue<'s>,
) {
    // SAFETY: `raw_cx`'s default-global realm is entered, as the caller
    // guarantees, and `invocation`'s `Rc<Runtime>` clone holds the runtime that
    // owns it alive for the whole await.
    unsafe {
        run_until(
            raw_cx,
            invocation.state().event_loop(),
            platform::clock::sleep,
            |scope| promise_is_settled(scope, value(scope)),
        )
        .await;
    }
}

/// Run `invocation`'s event loop until its pending sends are finished (see
/// [`core_runtime::event_loop::finish_pending_sends`]), and [`has_async_work`]
/// returns `false`. Pending sends include the body of a returned `Response`,
/// and the writes of streams and futures returned by an export.
///
/// Timers aren't waited for, so that a `setInterval` that is never cleared
/// doesn't prevent the call from completing. Work that waits for a timer is
/// dropped with `invocation`.
///
/// `raw_cx` is the context of `invocation`'s runtime, whose default global's
/// realm must be entered.
pub(crate) async fn drain_async_work(
    raw_cx: *mut js::native::RawJSContext,
    invocation: &OwnedInvocation,
) {
    let event_loop = invocation.state().event_loop();
    // Running the loop can register more sends, which are then finished as
    // well.
    loop {
        // SAFETY: as in `drive_until_settled`.
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

/// The result of calling `run` in [`run_main_export`].
enum Phase1 {
    /// No `run` export on the namespace.
    NoRunExport,
    /// `run` threw. Holds the error message.
    Failed(String),
    /// `run` returned `value`, which is a still-pending promise if `pending`.
    Returned {
        pending: bool,
        value: RootedHeap<Value>,
    },
}
