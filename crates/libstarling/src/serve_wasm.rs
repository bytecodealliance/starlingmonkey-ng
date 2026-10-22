// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Wasm serve mode: the `wasi:http/handler` export's body. The runtime is created once, running
//! the content script that registers the `fetch` handlers, and reused across requests, each
//! dispatched as a `fetch` event on its own [`EventLoop`].
//!
//! A WASIp3 host keeps sending requests to an instance rather than retiring one per request:
//! sequentially, and concurrently whenever every request in flight is parked on I/O or a timer.
//! Requests therefore share one global, and are separated by their event loops rather than their
//! realms.
//!
//! Shares the dispatch core in `serve_common` with `serve_native`, and the request/response
//! bridging in `platform::http`.

#![cfg(target_arch = "wasm32")]

use crate::serve_common::ServeTimeouts;
use core_runtime::config::RuntimeConfig;
use core_runtime::event_loop::{run_until_evaluated, EventLoop};
use core_runtime::invocation::{InvocationState, OwnedInvocation};
use core_runtime::runtime::Runtime;
use platform::http::OutgoingBody;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicU64;
use wasip3::http::types::{ErrorCode, Request as WasiRequest, Response as WasiResponse};

js::instance_local! {
    /// The JS runtime and its raw context, created once. The single global's realm is entered for
    /// the whole process rather than per request: a per-request `default_global()` would enter
    /// via a `JSAutoRealm`, and those drop non-LIFO under request interleaving, restoring the
    /// wrong (or no) current realm for other in-flight requests.
    static RUNTIME: RefCell<Option<(Rc<Runtime>, *mut js::native::RawJSContext, ServeTimeouts)>> =
        const { RefCell::new(None) };

    /// Why [`runtime`] failed to start the runtime, or why the content script's top-level `await`
    /// rejected (see [`ensure_started`]). Every later request fails with the same error instead
    /// of repeating the whole bootstrap, since a bootstrap per request is far heavier than serving
    /// from a running instance.
    static STARTUP_FAILURE: RefCell<Option<String>> = const { RefCell::new(None) };

    /// The content script's startup event loop, holding its top-level async work. [`runtime`]
    /// creates it undriven, since it must stay synchronous so concurrent first requests can't
    /// race into two runtimes. The first request drives it to completion via [`ensure_started`]
    /// before dispatching.
    static STARTUP: RefCell<Startup> = const { RefCell::new(Startup::Done) };

    /// Signalled on every [`STARTUP`] transition a waiting request cares about: driven to
    /// completion, or handed back by a driver that was cancelled. Every transition out of
    /// `Driving` must notify, or a waiter in [`ensure_started`] sleeps through it.
    static STARTUP_CHANGED: event_listener::Event = const { event_listener::Event::new() };

    /// Set while a Wizer snapshot is being taken, so it is set in the snapshot and the resumed
    /// instance can tell it came from one. See [`fix_up_after_resume`].
    static RESUMED_FROM_SNAPSHOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The startup event loop's lifecycle.
enum Startup {
    /// Not yet driven. The invocation stays registered for GC tracing.
    Pending(OwnedInvocation, core_runtime::ScriptEvaluation),
    /// A request is currently driving it. Concurrent requests wait.
    Driving,
    /// Driven to completion, failed (see [`STARTUP_FAILURE`]), or there never was one.
    Done,
}

/// Store the runtime this instance serves from, and hold a reference for the life of the instance.
///
/// The held reference is never released, so the runtime is never dropped. Nothing here tears an
/// instance down except thread-local destruction, and `Runtime::drop` reaches thread-locals in the
/// `js` crate (the scope handle pool among them) that the same destruction may already have run,
/// which traps. Dropping it wouldn't reclaim any memory either, since the instance's memory goes
/// away whole. `serve_native` runs the real `Drop`.
fn install_runtime(pair: (Rc<Runtime>, *mut js::native::RawJSContext, ServeTimeouts)) {
    std::mem::forget(Rc::clone(&pair.0));
    RUNTIME.with(|cell| *cell.borrow_mut() = Some(pair));
}

/// Get the runtime and its context, creating them on first use: run the content script
/// (registering `fetch` handlers) and enter the global realm persistently. Synchronous (no
/// `await`), so concurrent first requests can't race into two runtimes.
///
/// A failed startup is final: this returns `Err` on every later call without retrying. The
/// failure is logged once, by the call that ran into it. It names host paths and the server's own
/// internals, so it goes to the log rather than to a client.
fn runtime() -> Result<(Rc<Runtime>, *mut js::native::RawJSContext, ServeTimeouts), StartupFailed> {
    let failed_before = STARTUP_FAILURE.with(|cell| cell.borrow().is_some());
    runtime_for(false).map_err(|message| {
        if !failed_before {
            eprintln!("serve: the runtime could not be started: {message}");
        }
        StartupFailed
    })
}

/// Like [`runtime`], but creates a runtime configured for a Wizer snapshot if `pre_initialize` is
/// set and no runtime exists yet.
fn runtime_for(
    pre_initialize: bool,
) -> Result<(Rc<Runtime>, *mut js::native::RawJSContext, ServeTimeouts), String> {
    if let Some(pair) = RUNTIME.with(|cell| cell.borrow().clone()) {
        return Ok(pair);
    }
    if let Some(failure) = STARTUP_FAILURE.with(|cell| cell.borrow().clone()) {
        return Err(failure);
    }
    start_runtime(pre_initialize).inspect_err(|failure| {
        STARTUP_FAILURE.with(|cell| *cell.borrow_mut() = Some(failure.clone()));
    })
}

/// Create the runtime for [`runtime_for`].
fn start_runtime(
    pre_initialize: bool,
) -> Result<(Rc<Runtime>, *mut js::native::RawJSContext, ServeTimeouts), String> {
    // `wasmtime serve` passes the guest no arguments, so the HTTP entry point is configured
    // through `STARLINGMONKEY_CONFIG` instead (an empty one yields the defaults: `./index.js`).
    let mut config = RuntimeConfig::from_env().map_err(|e| e.to_string())?;
    config.pre_initialize = pre_initialize;
    config.validate_serve_timeouts()?;
    super::apply_pre_init_config(&config)?;
    crate::register_builtins();
    let timeouts = ServeTimeouts::from_config(&config);
    let (runtime, invocation, evaluation) = core_runtime::setup_for_serve(config)?;
    // Keep the startup loop (the script's leftover top-level async work) for the first request
    // to drive, since this function must stay synchronous. Owning it keeps its tasks GC-traced
    // in the meantime.
    let invocation = OwnedInvocation::new(runtime.clone(), invocation);
    STARTUP.with(|cell| *cell.borrow_mut() = Startup::Pending(invocation, evaluation));
    // Enter the global's realm and keep it entered for the process lifetime by leaking the scope (a
    // single one, harmless for a long-running server). The raw context stays valid as long as the
    // runtime (held in this thread-local) lives.
    let scope = runtime.default_global();
    // SAFETY: the context lives as long as `runtime`, which `install_runtime` keeps for the
    // process lifetime. The drivers `raw_cx` is handed to root what they hold.
    let raw_cx = unsafe { scope.raw_cx_no_gc() };
    std::mem::forget(scope);
    let pair = (runtime, raw_cx, timeouts);
    install_runtime(pair.clone());
    Ok(pair)
}

/// Prepare this instance's state to be captured in a Wizer snapshot. Every caller is a Wizer
/// entry point, and calls this last.
///
/// Buffered standard output is flushed, then wasi-libc closes every file descriptor it holds,
/// including stdio and the preopened directories. Their host handles do not exist in the resumed
/// instance, and libc opens stdio and the preopens again on first use there. Recording where
/// asynchronous work is created stops, so the resumed instance does not record it.
///
/// The resumed instance's first request runs the resume fixups (see [`fix_up_after_resume`]),
/// which advance its clocks by the monotonic clock reading taken here. The reading is taken last,
/// since one taken afterwards would sit past the recorded one and so still be in the resumed
/// instance's future.
pub fn prepare_for_snapshot() {
    unsafe extern "C" {
        fn fflush(stream: *mut std::ffi::c_void) -> std::ffi::c_int;
        fn __wasilibc_reset_preopens();
    }

    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: `fflush(NULL)` flushes every open C stdio stream. `__wasilibc_reset_preopens` takes
    // libc's own locks, and nothing holds a descriptor across this call.
    unsafe {
        fflush(std::ptr::null_mut());
        __wasilibc_reset_preopens();
    }
    // SAFETY: stopping the recording has no precondition.
    unsafe { js::stack::record_origins(None) };

    RESUMED_FROM_SNAPSHOT.with(|resumed| resumed.set(true));
    platform::clock::record_snapshot_reading();
}

/// Initializes the runtime until it's ready for Wizer snapshotting.
///
/// This entails initializing the JS runtime, registering builtins, running the top-level script to
/// completion, including a top-level `await`, and checking whether the result is a valid snapshot
/// input state. The script must register a `fetch` listener, and must leave no asynchronous work
/// behind once its top level has finished.
pub async fn pre_initialize() -> Result<(), String> {
    let (_runtime, raw_cx, _) = runtime_for(true)?;
    let pending = STARTUP.with(|cell| {
        let mut state = cell.borrow_mut();
        match std::mem::replace(&mut *state, Startup::Driving) {
            Startup::Pending(invocation, evaluation) => Some((invocation, evaluation)),
            other => {
                *state = other;
                None
            }
        }
    });
    let Some((invocation, evaluation)) = pending else {
        // Nothing to evaluate: `runtime` was already stood up, so this is a second call.
        prepare_for_snapshot();
        return Ok(());
    };
    // SAFETY: `runtime` entered the default global's realm for the process lifetime.
    let scope = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
    // The loop is driven only while the top level is unfinished, so work it merely queued, such
    // as a timer, is refused below rather than run under Wizer.
    if !evaluation.is_finished(&scope) {
        drive_startup(raw_cx, invocation.state().event_loop(), &evaluation).await;
    }
    evaluation.settled(&scope, "Script evaluation failed")?;
    // Throw an error instead of creating a snapshot that can't possibly serve requests.
    if evaluated_without_listener(&scope, &evaluation) {
        return Err(crate::serve_common::NO_FETCH_LISTENER.to_string());
    }
    invocation.state().event_loop().ensure_idle_for_snapshot()?;
    // Define every standard class the global has not resolved yet, so the snapshot holds them
    // all and an instance restored from it defines none of them on first use.
    js::class::enumerate_standard_classes(&scope, scope.global().handle())
        .map_err(|_| "defining the standard classes failed".to_string())?;
    STARTUP.with(|cell| *cell.borrow_mut() = Startup::Done);
    Ok(())
}

/// Whether the content script has finished evaluating without registering a `fetch` listener, in
/// which case every request it could ever receive is a 500 (see
/// [`NO_FETCH_LISTENER`](crate::serve_common::NO_FETCH_LISTENER)). A script still evaluating (a
/// top-level `await` that never settles) may still register a listener and cannot be judged.
fn evaluated_without_listener(
    scope: &js::gc::scope::Scope<'_>,
    evaluation: &core_runtime::ScriptEvaluation,
) -> bool {
    evaluation.is_finished(scope) && !fetch_event::fetch_event::FetchEvent::has_listener(scope)
}

/// Some state, such as process time origins, needs fixing up after snapshot resumption.
///
/// The monotonic clock comes first: its offset puts the timestamps held by the snapshot in the
/// resumed instance's past, and both the engine's timing and every fixup below read the clock
/// through it.
fn fix_up_after_resume() {
    if RESUMED_FROM_SNAPSHOT.with(|resumed| resumed.replace(false)) {
        js::clock::advance_monotonic_clock(platform::clock::resume_from_snapshot());
        core_runtime::runtime::run_resume_fixups();
    }
}

/// The error [`runtime`] and [`ensure_started`] return once startup failed.
struct StartupFailed;

/// Let the content script finish evaluating before dispatching, so a handler registered after a
/// top-level `await` is in place for the first request. Returns immediately once startup is done.
/// A request arriving while another is still driving the loop waits for it.
///
/// `Err` means the content script's top-level `await` rejected. The failure is logged once, when
/// it happens, and the work the script left behind is cancelled. It is final, so every later
/// request gets `Err` too.
async fn ensure_started(raw_cx: *mut js::native::RawJSContext) -> Result<(), StartupFailed> {
    /// Restores `Startup::Pending` if driving is cancelled mid-way (the host dropped the request
    /// future), so the loop's GC registration stays valid and a later request resumes driving.
    struct Driving {
        pending: Option<(OwnedInvocation, core_runtime::ScriptEvaluation)>,
    }
    impl Drop for Driving {
        fn drop(&mut self) {
            if let Some((invocation, evaluation)) = self.pending.take() {
                STARTUP.with(|cell| *cell.borrow_mut() = Startup::Pending(invocation, evaluation));
                // Whoever is waiting has to wake and take over the driving, since this request no
                // longer will.
                STARTUP_CHANGED.with(|changed| changed.notify(usize::MAX));
            }
        }
    }

    loop {
        enum Action {
            Drive(OwnedInvocation, core_runtime::ScriptEvaluation),
            Wait,
            Ready,
        }
        let action = STARTUP.with(|cell| {
            let mut state = cell.borrow_mut();
            match std::mem::replace(&mut *state, Startup::Done) {
                Startup::Pending(invocation, evaluation) => {
                    *state = Startup::Driving;
                    Action::Drive(invocation, evaluation)
                }
                Startup::Driving => {
                    *state = Startup::Driving;
                    Action::Wait
                }
                Startup::Done => Action::Ready,
            }
        });
        match action {
            Action::Ready => {
                return match STARTUP_FAILURE.with(|cell| cell.borrow().is_some()) {
                    true => Err(StartupFailed),
                    false => Ok(()),
                };
            }
            Action::Drive(invocation, evaluation) => {
                let mut driving = Driving {
                    pending: Some((invocation, evaluation)),
                };
                let (invocation, evaluation) = driving.pending.as_mut().expect("just set");
                drive_startup(raw_cx, invocation.state().event_loop(), evaluation).await;
                // SAFETY: `runtime` entered the default global's realm for the process lifetime.
                let scope = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
                let failed = evaluation.rejection(&scope, "Script evaluation failed");
                let (invocation, evaluation) = driving.pending.take().expect("just driven");
                match &failed {
                    Ok(()) => report_missing_fetch_listener(raw_cx, &evaluation),
                    Err(message) => {
                        eprintln!(
                            "serve: the content script's top-level `await` rejected: {message}"
                        );
                        STARTUP_FAILURE.with(|cell| *cell.borrow_mut() = Some(message.clone()));
                    }
                }
                STARTUP.with(|cell| *cell.borrow_mut() = Startup::Done);
                STARTUP_CHANGED.with(|changed| changed.notify(usize::MAX));
                match failed {
                    Ok(()) => keep_startup_loop_running(raw_cx, invocation),
                    Err(_) => invocation.state().event_loop().cancel_pending_futures(),
                }
                return failed.map_err(|_| StartupFailed);
            }
            Action::Wait => {
                // Another request is already driving the startup loop; wait for it to finish.
                let changed = STARTUP_CHANGED.with(event_listener::Event::listen);
                core_runtime::event_loop::trace_idle(std::fmt::from_fn(|f| {
                    static WAITER: AtomicU64 = AtomicU64::new(0);

                    let waiter = WAITER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    write!(f, "startup:{waiter}")
                }));
                changed.await;
            }
        }
    }
}

/// Drive the content script's event loop until the script has finished evaluating. Whatever it
/// started along the way (a timer, an unawaited `fetch`) is left running, on the task
/// [`keep_startup_loop_running`] spawns.
async fn drive_startup(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    evaluation: &core_runtime::ScriptEvaluation,
) {
    // SAFETY: `raw_cx` is valid for the duration of this await, since the runtime outlives the
    // request.
    unsafe {
        run_until_evaluated(raw_cx, event_loop, platform::clock::sleep, evaluation).await;
    }
}

/// Report, once evaluation completes, that the content script registered no `fetch` listener, so
/// the 500 response sent to every request has a stated reason. We can't decline to serve
/// at all, since the host owns the instance's lifecycle. Only applies to non-snapshot configs,
/// since [`pre_initialize`] refuses to snapshot a script without a listener.
fn report_missing_fetch_listener(
    raw_cx: *mut js::native::RawJSContext,
    evaluation: &core_runtime::ScriptEvaluation,
) {
    // SAFETY: `runtime` entered the default global's realm for the process lifetime.
    let scope = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
    if evaluated_without_listener(&scope, evaluation) {
        eprintln!("serve: {}", crate::serve_common::NO_FETCH_LISTENER);
    }
}

/// Keep driving the content script's own event loop, on a task of its own, for as long as it has
/// work: an instance serves many requests, and the script expects a `setInterval` or promise
/// chain its top level left behind to keep making progress between them.
///
/// The task also keeps the loop from being dropped with async-promise futures still pending,
/// which [`EventLoop::cancel_pending_futures`] documents as forbidden: it holds the invocation
/// until [`run_to_completion`] reports the loop empty, which it only does once no future is left
/// in flight. The task lives as long as the script requires, from one poll for a script that
/// left nothing behind to the instance's lifetime for one whose `fetch` never responds.
// TODO: Should reconsider this, it might make more sense to abort async work that's not added to waitUntil.
fn keep_startup_loop_running(raw_cx: *mut js::native::RawJSContext, invocation: OwnedInvocation) {
    wasip3::wit_bindgen::spawn_local(async move {
        // SAFETY: `runtime` entered the default global's realm for the process lifetime, and
        // `raw_cx` outlives this task, since the runtime is held in a process-lifetime
        // thread-local.
        unsafe {
            core_runtime::event_loop::run_to_completion(
                raw_cx,
                invocation.state().event_loop(),
                platform::clock::sleep,
            )
            .await;
        }
        drop(invocation);
    });
}

/// Handle one incoming request: create (or reuse) the runtime, drive the
/// content script's startup loop, then dispatch the request.
pub async fn handle(wasi_request: WasiRequest) -> Result<WasiResponse, ErrorCode> {
    let Ok((runtime, raw_cx, timeouts)) = runtime() else {
        // No runtime means no loop to drain, and no config to take a timeout from.
        return Ok(error_response(500, "Internal Server Error", None).0);
    };
    // Before `ensure_started`, which runs whatever the content script left after a top-level
    // `await`: that is script code, and it must not observe the state a resume still has to
    // repair.
    fix_up_after_resume();
    if ensure_started(raw_cx).await.is_err() {
        return Ok(error_response(500, "Internal Server Error", None).0);
    }
    dispatch_request(runtime, raw_cx, wasi_request, timeouts).await
}

/// Dispatch one incoming request: read the request, fire a `fetch` event on its
/// own event loop, and return the handler's response. The request's loop keeps
/// running to process body streaming and `waitUntil` work on a spawned task after
/// the response is returned.
async fn dispatch_request(
    runtime: Rc<Runtime>,
    raw_cx: *mut js::native::RawJSContext,
    wasi_request: WasiRequest,
    timeouts: ServeTimeouts,
) -> Result<WasiResponse, ErrorCode> {
    let clock = timeouts.start_clock();
    let (method, url, headers, body) = match platform::http::read_incoming_request(wasi_request) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("serve: the request's headers could not be read: {e:?}");
            return Ok(error_response(400, "Bad Request", clock.error_body_time_limit()).0);
        }
    };
    // `wasi:http` always provides a body stream, even for a request that cannot have a body. In
    // that case, we drop the body, so `request.body` correctly returns `null`.
    let (has_body, content_length) = body_framing(&method, &headers);
    let body = has_body.then_some(body);
    let head_request = method.eq_ignore_ascii_case("HEAD");

    let invocation = OwnedInvocation::new(runtime.clone(), InvocationState::new());

    // The realm this request runs in is the process-wide global, which stays entered for the
    // process lifetime (`runtime` leaks the entering scope). There is no per-request global: one
    // instance serves many requests, and they all share this one.
    //
    // SAFETY: the realm is entered for the process lifetime, so it is entered for this dispatch.
    let request_realm = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
    let mut parts = crate::serve_common::dispatch_fetch(
        &request_realm,
        invocation.state(),
        method,
        url,
        headers,
        body,
        content_length,
        platform::clock::sleep,
        &clock,
    )
    .await;

    let abort_controller = parts.as_ref().map(|d| d.rooted_abort_controller());
    let (response, body_done, abandon_body) = match parts.as_mut().and_then(|d| d.response.take()) {
        Some((status, headers, send_body)) => {
            let crate::serve_common::WireResponse {
                status,
                mut headers,
                body,
                declared_length,
            } = crate::serve_common::prepare_wire_response(
                head_request,
                status,
                headers,
                send_body,
            );
            // Without a length to frame by, the host falls back to chunked.
            // Note: `prepare_wire_response` removed any `Content-Length` headers the handler
            // might've added, so this is certain not to be a duplicate.
            if let Some(length) = declared_length {
                headers.insert(
                    http::header::CONTENT_LENGTH,
                    http::HeaderValue::from(length),
                );
            }
            platform::http::build_outgoing_response(
                status,
                headers,
                body,
                clock.response_body_time_limit(),
                declared_length,
            )
        }
        None => error_response(500, "Internal Server Error", clock.error_body_time_limit()),
    };
    // Continue running the event loop until the response body has been fully sent out and all
    // `waitUntil` promises have been settled.
    wasip3::wit_bindgen::spawn_local(async move {
        // SAFETY: the process-lifetime realm is entered, and `raw_cx` outlives this task.
        unsafe {
            let outcome = drive_body_send(
                raw_cx,
                invocation.state().event_loop(),
                platform::clock::sleep,
                &clock,
                body_done,
                || abandon_body.abandon(),
            )
            .await;
            // Signal the aborts only this side of the transport sees.
            let outcome = match outcome {
                Some(Some(platform::http::BodySendOutcome::Sent)) | Some(None) => {
                    crate::serve_common::BodySendOutcome::Sent
                }
                Some(Some(platform::http::BodySendOutcome::TimedOut)) | None => {
                    crate::serve_common::BodySendOutcome::TimedOut
                }
                Some(Some(platform::http::BodySendOutcome::Failed(message))) => {
                    crate::serve_common::BodySendOutcome::ConnectionLost(message)
                }
                Some(Some(platform::http::BodySendOutcome::Truncated)) => {
                    crate::serve_common::BodySendOutcome::Truncated
                }
            };
            {
                // Scoped, so the rooting scope is released before the drain below, which runs for
                // as long as the `waitUntil` window lasts.
                // SAFETY: the process-lifetime realm is entered.
                let scope = js::gc::scope::RootScope::from_current_realm(raw_cx);
                crate::serve_common::signal_body_outcome(
                    &scope,
                    invocation.state().event_loop(),
                    abort_controller.as_ref(),
                    outcome,
                );
            }
            // Lifetime work is drained regardless of how the send ended. The writer's `TimedOut`
            // and the limit above expiring are the same deadline enforced in two places, so on an
            // ordinary body timeout they expire together and either may be reported first. The
            // request's `waitUntil` window must not depend on which.
            crate::serve_common::drain_lifetime_work(
                raw_cx,
                invocation.state().event_loop(),
                platform::clock::sleep,
                &clock,
            )
            .await;
        }
        drop(invocation);
    });

    Ok(response)
}

/// Drive a request's event loop while its response body is being sent. Returns `Some` with
/// `body_done`'s output once the transport reports the body has been sent out, or `None` if the
/// `response_body` time limit expires first.
///
/// If the loop runs out of work before the body has been fully sent out, that means it's
/// effectively abandoned and can't be completed anymore. In that case, `abandon` is called,
/// and must close the transport.
///
/// The timeout here is a backstop: the same limit is applied to `spawn_body_writer`, which reports
/// a timeout through `body_done`, so this timeout should never be hit.
///
/// # Safety
///
/// `raw_cx` must be valid, with the request's realm entered, for the duration of the call.
async unsafe fn drive_body_send<S, F, B, A>(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    sleep: S,
    clock: &crate::serve_common::RequestClock,
    body_done: B,
    abandon: A,
) -> Option<B::Output>
where
    S: Fn(std::time::Duration) -> F,
    F: std::future::Future<Output = ()>,
    B: std::future::Future,
    A: FnOnce(),
{
    let sending = async {
        let mut body_done = std::pin::pin!(body_done);
        let ended = futures_lite::future::or(async { Some(body_done.as_mut().await) }, async {
            // SAFETY: this function's caller keeps `raw_cx` valid for the call.
            unsafe {
                core_runtime::event_loop::run_until(raw_cx, event_loop, &sleep, |_| false).await
            };
            None
        })
        .await;
        match ended {
            Some(output) => output,
            // The loop ran out first. If the body hasn't been sent out completely, the writer
            // has to close it out as an incomplete send.
            None => {
                abandon();
                body_done.await
            }
        }
    };
    crate::serve_common::with_timeout(&sleep, clock.response_body_time_limit(), sending).await
}

/// Whether an incoming request has a body, and its declared length.
///
/// `wasi:http` always provides a body, so we can't know whether the request really did include one
/// or not. Because of that, only `GET` and `HEAD` requests are marked as body-less: they're not
/// allowed to have bodies per RFC 9110 §9.3.
fn body_framing(method: &str, headers: &http::HeaderMap) -> (bool, Option<u64>) {
    // A declared length of zero means no body and no length to report.
    let content_length = headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|length| *length > 0);
    let bodyless_method = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
    (!bodyless_method || content_length.is_some(), content_length)
}

/// A minimal text error response. Its body is written out by a task of its own like any other
/// response's, and bounded by a timeout like any other.
// TODO: unify with `status_response` in `serve_native`.
fn error_response(
    status: u16,
    message: &str,
    body_timeout: Option<std::time::Duration>,
) -> (
    WasiResponse,
    platform::http::BodyDone,
    platform::http::AbandonBody,
) {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain"),
    );
    platform::http::build_outgoing_response(
        status,
        headers,
        OutgoingBody::Bytes(bytes::Bytes::copy_from_slice(message.as_bytes())),
        body_timeout,
        None,
    )
}
