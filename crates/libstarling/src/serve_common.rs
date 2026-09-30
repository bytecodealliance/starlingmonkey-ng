// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The per-request dispatch core shared by the `serve_native` and
//! `serve_wasm` serve paths: build the JS `Request`, dispatch the `fetch`
//! event on the request's own event loop, drive the loop until
//! `respondWith`/`waitUntil` settle, and take the response parts. Only the
//! transport I/O around it differs per target.

use core_runtime::event_loop::{run_until, with_event_loop, EventLoop};
use core_runtime::invocation::InvocationState;
use fetch_event::fetch_event::FetchEvent;
use js::conversion::ToJSVal;
use web_globals::events::algorithms::ScriptStackState;

use std::time::{Duration, Instant};
use web_fetch::request::Request;
use web_globals::dom_exception::DOMException;
use web_globals::signals::abort_controller::{AbortController, AbortControllerImpl};

pub(crate) const NO_FETCH_LISTENER: &str =
    "no `fetch` listener added during evaluation of top-level module";

/// How long a served request may run, from
/// [`RuntimeConfig`](core_runtime::config::RuntimeConfig). `None` means no limit.
#[derive(Clone, Copy, Default)]
pub(crate) struct ServeTimeouts {
    /// Waiting for `respondWith` to settle, after which the request gets a `500`.
    pub(crate) dispatch: Option<Duration>,
    /// Max time between starting to send a response and finishing sending the body.
    pub(crate) response_body: Option<Duration>,
    /// Waiting for the `waitUntil` promises, counted from the response sending being done.
    pub(crate) waituntil: Option<Duration>,
    /// Max end-to-end request processing time, regardless of how long individual phases take.
    pub(crate) end_to_end: Option<Duration>,
}

impl ServeTimeouts {
    pub(crate) fn from_config(config: &core_runtime::config::RuntimeConfig) -> Self {
        Self {
            dispatch: config.dispatch_timeout(),
            response_body: config.response_body_timeout(),
            waituntil: config.waituntil_timeout(),
            end_to_end: config.end_to_end_timeout(),
        }
    }

    /// Start one request's `end_to_end` window. Called where the request's own work begins: after
    /// the head is read, before invoking the JS handler.
    pub(crate) fn start_clock(&self) -> RequestClock {
        RequestClock {
            timeouts: *self,
            deadline: self.end_to_end.map(|limit| Instant::now() + limit),
        }
    }
}

/// One request's [`ServeTimeouts`], with the `end_to_end` window running.
///
/// Each phase takes its limit from the respective `*_time_limit` method and enforces it itself, since
/// aborting means different things depending on the phase.
pub(crate) struct RequestClock {
    timeouts: ServeTimeouts,
    /// When the `end_to_end` window closes; `None` for no limit.
    deadline: Option<Instant>,
}

impl RequestClock {
    /// What is left of the `end_to_end` window, zero once it has expired.
    pub(crate) fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// The smaller of a phase's own limit and [`remaining`](Self::remaining).
    fn phase_time_limit(&self, phase: Option<Duration>) -> Option<Duration> {
        match (phase, self.remaining()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (limit, None) | (None, limit) => limit,
        }
    }

    pub(crate) fn dispatch_time_limit(&self) -> Option<Duration> {
        self.phase_time_limit(self.timeouts.dispatch)
    }

    pub(crate) fn response_body_time_limit(&self) -> Option<Duration> {
        self.phase_time_limit(self.timeouts.response_body)
    }

    pub(crate) fn waituntil_time_limit(&self) -> Option<Duration> {
        self.phase_time_limit(self.timeouts.waituntil)
    }

    /// The time limit for the body of an error response: the `response_body` limit on its own, not
    /// narrowed by what is left of the `end_to_end` window.
    ///
    /// Used to ensure that sending out an error response succeeds if the reason it has to be sent
    /// is that producing a response timed out.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn error_body_time_limit(&self) -> Option<Duration> {
        self.timeouts.response_body
    }
}

/// Run `work` under `limit`: `Some(output)` if it finished, `None` if `time_limit` was reached
/// and `work` interrupted as a result.
pub(crate) async fn with_timeout<S, F, W>(
    sleep: &S,
    time_limit: Option<Duration>,
    work: W,
) -> Option<W::Output>
where
    S: Fn(Duration) -> F,
    F: std::future::Future<Output = ()>,
    W: std::future::Future,
{
    let Some(limit) = time_limit else {
        return Some(work.await);
    };
    futures_lite::future::or(async { Some(work.await) }, async {
        sleep(limit).await;
        None
    })
    .await
}

/// Finish a request's work once its response body is sent or truncated: wait out any async tasks
/// registered as promises passed to [`FetchEvent::waitUntil`], under the `waituntil` time limit,
/// then release whatever the loop still has in flight.
///
/// Per [Extend Lifetime Promises], only `waitUntil` keeps a request alive past its response.
/// Once all promises passed to `waitUntil` have resolved, the event loop ends, and all unawaited
/// async tasks, such as timers or outgoing `fetch` calls, are abandoned.
///
/// [Extend Lifetime Promises]: https://w3c.github.io/ServiceWorker/#extendableevent-extend-lifetime-promises
///
/// # Safety
///
/// `raw_cx` must be valid, with the request's realm entered, for the duration of the call.
pub(crate) async unsafe fn drain_lifetime_work<S, F>(
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
    sleep: S,
    clock: &RequestClock,
) where
    S: Fn(Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    // Over once no `waitUntil` promise is outstanding, or once no async work is outstanding anymore
    // that could cause any pending promises to be settled.
    let lifetime = unsafe { run_until(raw_cx, event_loop, &sleep, |_| !event_loop.has_interest()) };
    let _ = with_timeout(&sleep, clock.waituntil_time_limit(), lifetime).await;

    // The request's lifetime ended, and its event loop won't be run anymore, so we release all
    // pending futures to ensure they don't leak.
    event_loop.cancel_pending_futures();
}

/// Map a `Response`'s status to a value the HTTP wire can represent, replacing
/// anything outside the valid status-code range (e.g. `Response.error()`'s `0`)
/// with `500`. RFC 9110 status codes are three digits in `100..=599`.
pub(crate) fn normalize_http_status(status: u16) -> u16 {
    if (100..=599).contains(&status) {
        status
    } else {
        500
    }
}

/// A response ready for the wire: status, headers, and the body to send.
pub(crate) type ResponseParts = (u16, Vec<(String, String)>, platform::http::OutgoingBody);

/// What one dispatched `fetch` event left behind for the transport.
pub(crate) struct DispatchedFetch<'s> {
    /// The response parts (status, headers, send body), or `None` for the algorithm's `network
    /// error`, which the transport sends its 500 for.
    pub(crate) response: Option<ResponseParts>,
    /// The request's `AbortController` (step 17.4.2), handed back so the transport can run step
    /// 17.4.20 for the aborts only it sees: the clock or the connection breaking under the
    /// response write.
    abort_controller: AbortController<'s>,
}

impl DispatchedFetch<'_> {
    /// The abort controller as a `RootedHeap`, so the transport layer can signal it asynchronously.
    pub(crate) fn rooted_abort_controller(
        &self,
    ) -> js::gc::handle::RootedHeap<AbortControllerImpl> {
        js::gc::handle::RootedHeap::new(self.abort_controller)
    }
}

/// [Create Fetch Event and Dispatch](https://w3c.github.io/ServiceWorker/#create-fetch-event-and-dispatch)
/// step 17.4.20, telling a handler still working on the request that its fetch is over.
/// `name`/`message` are a stand-in for the fetch controller's abort reason.
pub(crate) fn signal_request_abort(
    scope: &js::gc::scope::Scope<'_>,
    event_loop: &EventLoop,
    abort_controller: &AbortController<'_>,
    name: &'static str,
    message: &str,
) {
    with_event_loop(event_loop, |_| {
        let reason = DOMException::new(scope, message.into(), name.into())
            .and_then(|exception| exception.to_jsval_throwing(scope))
            .unwrap_or_else(|_| js::exception::take_pending_or_undefined(scope));
        if abort_controller
            .abort(scope, reason, ScriptStackState::Empty)
            .is_err()
        {
            js::exception::report_and_clear(scope, "signalling abort on a request");
        }
    });
}

/// The request's header fields as the list a `Headers` object holds. The fetch spec stores those
/// as ordered name/value pairs, not as a map, so the fields are flattened back out here.
pub(crate) fn header_list(headers: &http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                platform::http::isomorphic_decode(value.as_bytes()),
            )
        })
        .collect()
}

/// The server side of
/// [Create Fetch Event and Dispatch](https://w3c.github.io/ServiceWorker/#create-fetch-event-and-dispatch)
/// (inlined): dispatch an incoming request as a `fetch` event on `invocation`'s event loop and
/// return the response parts (status, headers, send body), or `None` for the algorithm's `network
/// error` (steps 21.1.2 and 22.2).
///
/// The algorithm is split over three places:
/// - here: the locals of steps 1–8, the task of step 17.4, and the outcome of steps 21–24.
/// - [`FetchEvent`]: creating and dispatching the event (steps 17.4.1 and 17.4.4–17.4.14), and
///   the flags `respondWith` and `preventDefault` set on it (steps 17.4.15, 17.4.16 and 17.4.19).
/// - the transport, `serve_native` and `serve_wasm`: step 17.4.20 for the aborts only visible to
///   the transport layer, and sending a `500` response for a `network error`.
pub(crate) async fn dispatch_fetch<'s, S, F>(
    scope: &'s js::gc::scope::Scope<'_>,
    invocation: &InvocationState,
    method: String,
    url: String,
    headers: http::HeaderMap,
    incoming_body: Option<platform::http::IncomingBody>,
    content_length: Option<u64>,
    sleep: S,
    clock: &RequestClock,
) -> Option<DispatchedFetch<'s>>
where
    S: Fn(Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    // Step 1: Let _response_ be null.
    // Step 2: Let _eventCanceled_ be false.
    // Both are state on the event here: its `potential response` and its `canceled flag`, read
    // back below.
    // Step 3: Let _client_ be _request_’s `client`.
    // Null: an incoming request has no client it came from.
    // Step 4: Let _activeWorker_ be _registration_’s `active worker`.
    // At least for now, StarlingMonkey only has a single worker at any time.
    // Step 5: Let _eventHandled_ be null.
    // Created with the event, in `FetchEvent::create_for_request` (step 17.2).
    // Step 6: Let _handleFetchFailed_ be false.
    // Implicit in returning `None`.
    // Step 7: Let _respondWithEntered_ be false.
    // Retrieved using `FetchEvent::respond_with_entered`.
    // Step 8: Let _networkError_ be a `network error`.
    // Expressed as a `DispatchedFetch` with no response, which the transport sends a `500` for.
    // Step 9: If _raceResponse_ is not null:
    // Step 9.1: Set _networkError_’s `service worker timing info` to _timingInfo_.
    // Navigation preload is not implemented, so _raceResponse_ is always null.
    // Step 10: Let _shouldSoftUpdate_ be true if any of the following are true, and false
    //     otherwise:
    //   - _request_ is a `non-subresource request`.
    //   - _request_ is a `subresource request` and _registration_ is `stale`.
    // There is no registration, and the script is fixed for the server's lifetime, so there is
    // nothing to soft-update.
    // Step 11: If the result of running the `Should Skip Event` algorithm with "fetch" and
    //     _activeWorker_ is true, then:
    // Step 11.1: If _shouldSoftUpdate_ is true, then `in parallel` run the `Soft Update` algorithm
    //     with _registration_.
    // Step 11.2: Return null.
    // N/A, see step 10.
    // Step 12: If _activeWorker_’s `all fetch listeners are empty flag` is set:
    // Step 12.1: `In parallel`:
    // Step 12.1.1: If _activeWorker_’s `state` is "activating", then wait for _activeWorker_’s
    //     `state` to become "activated".
    // Step 12.1.2: Run the `Run Service Worker` algorithm with _activeWorker_.
    // Step 12.1.3: If _shouldSoftUpdate_ is true, then run the `Soft Update` algorithm with
    //     _registration_.
    // Step 12.2: Return null.
    // Both null returns mean "no listener to run, go to the network instead", which this server
    // has no equivalent for. The startup paths refuse a script with no `fetch` listener wherever
    // they can (see [`NO_FETCH_LISTENER`]). Where they can't, the event is dispatched all the
    // same, and the missing listener is reported on step 24's `None` path.
    // Step 13: If _useHighResPerformanceTimers_ is true, then set _useHighResPerformanceTimers_ to
    //     _activeWorker_’s `global object`’s `cross-origin isolated capability`.
    // Step 14: Let _timingInfo_’s `start time` be the `coarsened shared current time` given
    //     _useHighResPerformanceTimers_.
    // N/A: No service worker timing info is collected for now.
    // Step 15: If _activeWorker_’s `state` is "`activating`", wait for _activeWorker_’s `state`
    //     to become "`activated`".
    // Step 16: If the result of running the `Run Service Worker` algorithm with _activeWorker_ is
    //     _failure_, then set _handleFetchFailed_ to true.
    // N/A: The script is evaluated before this function is called.
    // Step 17: Else:
    // Step 17.1: Set _workerRealm_ to the `relevant realm` of the _activeWorker_’s `global
    //     object`.
    // `scope`'s realm, already entered.
    // Step 17.2: Set _eventHandled_ to `a new promise` in _workerRealm_.
    // Created with the event, in `FetchEvent::create_for_request`.
    // Step 17.3: If _raceResponse_ is not null, `set` _activeWorker_’s `global object`’s `race
    //     response map`[_request_] to _raceResponse_.
    // N/A, see step 9.
    // Step 17.4: `Queue a task` _task_ to run the following substeps: If _task_ is discarded, set
    //     _handleFetchFailed_ to true. The _task_ _must_ use _activeWorker_’s `event loop` and
    //     the `handle fetch task source`.
    // The task runs inline, on the request's own event loop. Discarding it is the timeout at step
    // 17.4.16.1.
    // TODO: make this translation lazy, applied when headers are first changed by content.
    // TODO: and on wasm, don't even read headers from the host until necessary.
    let header_list = header_list(&headers);
    let event = with_event_loop(invocation.event_loop(), |_| {
        let event = (|| {
            // Step 17.4.2: Let _abortController_ be a `new` `AbortController` object with
            //     _workerRealm_.
            let abort_controller = AbortController::new(scope).ok()?;
            // Step 17.4.3: Let _requestObject_ be the result of `creating` a `Request` object,
            //     given _request_, a new `Headers` object’s `guard` which is "`immutable`",
            //     _abortController_’s `signal`, and _workerRealm_.
            let request = Request::from_incoming(
                scope,
                &method,
                &url,
                header_list,
                incoming_body,
                content_length,
                abort_controller.signal(scope),
            )
            .ok()?;
            // Steps 17.4.1 and 17.4.4–17.4.14: create the event, initialize its attributes, and
            // dispatch it at the global. Implemented in `FetchEvent::create_for_request` and
            // `FetchEvent::dispatch`.
            // Step 17.4.1 runs after step 17.4.3 here, since creating the event takes the request
            // object that step 17.4.6 initializes it with.
            let event = FetchEvent::dispatch(scope, request, ScriptStackState::Empty).ok()?;
            Some((event, abort_controller))
        })();
        js::exception::report_and_clear(scope, "serve dispatch");
        event
    });
    let (event, abort_controller) = event?;

    // Step 17.4.15: If _e_’s `respond-with entered flag` is set, set _respondWithEntered_ to
    //     true.
    // Read from the event directly.
    // Step 17.4.16: If _e_’s `wait to respond flag` is set, then:
    // Step 17.4.16.1: Wait until _e_’s `wait to respond flag` is unset.
    // Step 18: Wait for _task_ to have executed or for _handleFetchFailed_ to be true.
    //
    // The timeout implements step 17.4's provision for discarding the task, which the spec leaves
    // to the user agent. Discarding leaves `wait to respond` set, so the read at step 17.4.16.3
    // finds no `Response`.
    let drive = unsafe {
        run_until(
            scope.cx_mut().raw_cx(),
            invocation.event_loop(),
            &sleep,
            |_| !event.wait_to_respond_set(),
        )
    };
    let timed_out = with_timeout(&sleep, clock.dispatch_time_limit(), drive)
        .await
        .is_none();
    // Report any exception left pending before reading the response (the JSAPI
    // calls below assert no-pending).
    js::exception::report_and_clear(scope, "serve request");

    // Step 17.4.20: If _fetchController_ `state` is "`terminated`" or "`aborted`", then:
    // Step 17.4.20.1: Let _deserializedError_ be the result of `deserialize a serialized abort
    //     reason` given _fetchController_’s `serialized abort reason` and _workerRealm_.
    // Step 17.4.20.2: `Queue a task` to `signal abort` on _abortController_ with
    //     _deserializedError_.
    // Run before steps 17.4.16.2 to 17.4.19, so that the read below picks up a response the
    // handler gives in reaction to the abort.
    //
    // Timing out is one way to become "terminated". The transport sees the other ways and signals
    // them itself, on the controller it gets back from
    // [`DispatchedFetch::rooted_abort_controller`]. A disconnect during the dispatch does not
    // count: on the wire it is indistinguishable from a legitimate half-close.
    if timed_out {
        signal_request_abort(
            scope,
            invocation.event_loop(),
            &abort_controller,
            "TimeoutError",
            "the fetch event did not respond within the dispatch or end-to-end timeout",
        );
        // Only after the abort, so a handler reacting to its `signal` still responds through the
        // normal path.
        event.abandon_respond();
    }

    // Step 17.4.16.2: If _e_’s `respond-with error flag` is set, set _handleFetchFailed_ to true.
    // Step 17.4.16.3: Else, set _response_ to _e_’s `potential response`.
    // `None` covers both arms: the error flag being set, and `respondWith` never having been
    // called at all.
    let response = event.potential_response(scope);
    // Step 17.4.17: If _response_ is null, _request_’s `body` is not null, and _request_’s
    //     `body`’s `source` is null, then:
    // Step 17.4.17.1: If _request_’s `body` is `unusable`, set _handleFetchFailed_ to true.
    // Step 17.4.17.2: Else, `cancel` _request_’s `body` with undefined.
    // An unusable body is already among the failures the `None` above stands for. Disposing of an
    // unread body is the transport's job, since only it has what the connection still needs.
    // Step 17.4.18: If _response_ is not null, then set _response_’s `service worker timing info`
    //     to _timingInfo_.
    // N/A, see steps 13 and 14.
    // Step 17.4.19: If _e_’s `canceled flag` is set, set _eventCanceled_ to true.
    // Stays a flag on the event, read at step 21.1 through `is_canceled`.
    // Step 19: If _shouldSoftUpdate_ is true, then `in parallel` run the `Soft Update` algorithm
    //     with _registration_.
    // N/A, see step 10.
    // Step 20: If _activeWorker_’s `global object`’s `race response map`[_request_] `exists`,
    //     `remove` _activeWorker_’s `global object`’s `race response map`[_request_].
    // N/A, see step 9.

    // Step 21: If _respondWithEntered_ is false, then:
    // Step 21.1: If _eventCanceled_ is true, then:
    // Step 21.1.1: If _eventHandled_ is not null, then `reject` _eventHandled_ with a
    //     "`NetworkError`" `DOMException` in _workerRealm_.
    // Step 21.1.2: Return _networkError_.
    // Step 21.2: If _eventHandled_ is not null, then `resolve` _eventHandled_.
    // Step 21.3: If _raceResponse_ is not null, and _raceResponse_’s `value` is not null, then:
    // Step 21.3.1: Wait until _raceResponse_’s `value` is not "`pending`".
    // Step 21.3.2: If _raceResponse_’s `value` is a `response`, return _raceResponse_’s
    //     `value`.
    // Step 21.4: Return null.
    // Step 22: If _handleFetchFailed_ is true, then:
    // Step 22.1: If _eventHandled_ is not null, then `reject` _eventHandled_ with a
    //     "`NetworkError`" `DOMException` in _workerRealm_.
    // Step 22.2: Return _networkError_.
    // Step 23: If _eventHandled_ is not null, then `resolve` _eventHandled_.
    // Steps 21-23 are handled under `event.settle_handled`, which rejects with a network error if
    // there's no response, and otherwise resolves. We don't differentiate between the failure
    // modes, and step 21.3 doesn't apply.
    with_event_loop(invocation.event_loop(), |_| {
        event.settle_handled(scope, response.is_some());
        core_runtime::event_loop::run_microtasks(scope);
    });
    // Step 24: Return _response_.
    let response = match response {
        Some(response) => response,
        None => {
            // TODO: this should probably move into the `event_handled` promise rejection instead.
            // The `network error` of steps 21.1.2 and 22.2. Which of the failures produced it
            // only shows up here, in the log.
            if timed_out {
                eprintln!(
                    "serve: the fetch event did not respond within the dispatch or end-to-end \
                     timeout; answering with a network error"
                );
            } else if event.respond_with_error_set() {
                eprintln!(
                    "serve: the promise passed to respondWith did not produce a Response; \
                     answering with a network error"
                );
            } else if event.is_canceled() && !event.respond_with_entered() {
                eprintln!(
                    "serve: the fetch event was canceled with preventDefault() and not responded \
                     to; answering with a network error"
                );
            } else if !FetchEvent::has_listener(scope) {
                // The script never registered a handler at all. The startup paths refuse that
                // outright where they can (see [`NO_FETCH_LISTENER`]), so reaching here means they
                // couldn't: a per-request evaluation under `--serve-isolated`, a snapshot of a
                // script that exports `run` instead, or a host that owns the instance's lifecycle
                // and leaves the guest no way to decline.
                eprintln!("serve: {NO_FETCH_LISTENER}; answering with a network error");
            } else {
                eprintln!(
                    "serve: no fetch handler responded to the request (respondWith was not \
                     called); answering with a network error"
                );
            }
            return Some(DispatchedFetch {
                response: None,
                abort_controller,
            });
        }
    };
    // The headers as they were when `respondWith`'s promise settled. Reading the live `Response`
    // instead would let a handler that kept a reference to it edit them after the response was
    // final.
    let headers = event
        .take_potential_response_headers()
        .expect("a potential response is set with its headers");
    // `Response.error()` and opaque-redirect responses have status 0, which
    // would serialize as the protocol-illegal status line `HTTP/1.1 0`.
    let status = normalize_http_status(response.status());
    // Taking the send body runs JS for a `ReadableStream` body (the pump's first read, or the
    // cancel where there is no read to come). With the loop active and jobs drained right away, its
    // futures attach here and no reaction is left behind.
    let head_request = method.eq_ignore_ascii_case("HEAD");
    let body = with_event_loop(invocation.event_loop(), |_| {
        let body = response.take_send_body(scope, !head_request);
        core_runtime::event_loop::run_microtasks(scope);
        body
    });
    Some(DispatchedFetch {
        response: Some((status, headers, body)),
        abort_controller,
    })
}

/// How sending a response body ended, as far as the transport can tell.
pub(crate) enum BodySendOutcome {
    /// The body reached the client, or there was nothing left to report it about.
    Sent,
    /// The response-body or end-to-end time limit expired first.
    TimedOut,
    /// The transport could not deliver it. The message is for the log.
    ConnectionLost(String),
    /// The body ended before the `Content-Length` the handler declared for it. The response is
    /// truncated, and the connection is closed after the current request.
    Truncated,
}

/// Tell a handler still working on the request how sending its response body ended, per
/// [`dispatch_fetch`]'s step 17.4.20. An outcome that needs no abort, or a dispatch that produced
/// no controller to signal, leaves the loop alone. `scope`'s realm must be the one the request was
/// dispatched in.
pub(crate) fn signal_body_outcome(
    scope: &js::gc::scope::Scope<'_>,
    event_loop: &EventLoop,
    abort_controller: Option<&js::gc::handle::RootedHeap<AbortControllerImpl>>,
    outcome: BodySendOutcome,
) {
    let (Some((name, message)), Some(controller)) = (abort_for_outcome(outcome), abort_controller)
    else {
        return;
    };
    let controller = controller.get(scope);
    signal_request_abort(scope, event_loop, &controller, name, message);
}

/// The `Create Fetch Event and Dispatch` step 17.4.20 abort a body send calls for, as
/// `(DOMException name, message)`, or `None` where the send needs no abort.
///
/// The reason is logged here as well, since the response is already on its way and there is
/// no status left to report a failed, stalled, or abandoned body with.
fn abort_for_outcome(outcome: BodySendOutcome) -> Option<(&'static str, &'static str)> {
    match outcome {
        BodySendOutcome::Sent => None,
        BodySendOutcome::TimedOut => {
            eprintln!(
                "serve: the response body was not fully sent within the response-body or \
                 end-to-end timeout"
            );
            Some((
                "TimeoutError",
                "the response body was not fully sent within the response-body or end-to-end \
                 timeout",
            ))
        }
        BodySendOutcome::ConnectionLost(message) => {
            eprintln!("serve: {message}");
            Some((
                "AbortError",
                "the connection was lost while the response was being written",
            ))
        }
        BodySendOutcome::Truncated => {
            eprintln!(
                "serve: the response body ended before the `Content-Length` it declared, so the \
                 response is truncated"
            );
            Some((
                "AbortError",
                "the response body ended before the `Content-Length` it declared",
            ))
        }
    }
}

/// A response reduced to what may go on the wire: the header fields that may be sent, the body to
/// send, and the content length.
pub(crate) struct WireResponse {
    pub(crate) status: u16,
    pub(crate) headers: http::HeaderMap,
    /// The content to send, empty where the status or the method rules content out.
    pub(crate) body: platform::http::OutgoingBody,
    /// The response's `Content-Length`. For non-streaming bodies, it's derived from the buffer.
    /// For forwarded incoming bodies, the incoming `Content-Length` header is used. For bodies
    /// backed by a JS `ReadableStream`, the guest-set `Content-Length` header is used, if any.
    /// For a `HEAD`, which sends no content to derive one from, the guest-set header comes first
    /// and the buffer length is used if there is no header (RFC 9110 §9.3.2).
    /// `None` means no `Content-Length` header will be set on the outgoing response.
    ///
    /// The declared length is enforced at the transport layer, with chunks exceeding it being
    /// ignored. If the guest produces fewer bytes than declared, an error is reported and the
    /// connection is closed.
    ///
    /// Always `None` for body-less status responses.
    pub(crate) declared_length: Option<u64>,
}

/// Whether a status is framed by the status alone: no `Content-Length`, no `Transfer-Encoding`, no
/// body (RFC 9110 §8.6, RFC 9112 §6.2). On a `304` a `Content-Length: 0` would contradict the
/// length of the response being revalidated.
///
/// `1xx` is defensive: `Response`'s constructor refuses any status outside 200–599.
fn status_forbids_content(status: u16) -> bool {
    matches!(status, 100..=199 | 204 | 304)
}

/// Whether a header is set by the transport, not the handler.
fn is_transport_owned_header(name: &http::HeaderName) -> bool {
    *name == http::header::CONTENT_LENGTH
        || *name == http::header::TRANSFER_ENCODING
        || *name == http::header::CONNECTION
}

/// The header fields of a dispatched response that may go on the wire.
///
/// `Content-Length`, `Transfer-Encoding` and `Connection` are dropped, because they must be set to
/// correct values for transport compliance, instead of potentially incorrect values content
/// provided.
///
/// A name that is not a token or a value including a control character is dropped as well. Those
/// would let a response split into two on the wire. JS-set headers are validated, but one proxied
/// from a `fetch()` response has not been.
fn wire_headers(headers: Vec<(String, String)>) -> http::HeaderMap {
    let mut fields = http::HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        // Values are `ByteString`s, one code unit per wire byte, so they are isomorphic-encoded
        // rather than UTF-8-encoded.
        let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_bytes(&platform::http::isomorphic_encode(&value)),
        ) else {
            continue;
        };
        if is_transport_owned_header(&name) {
            continue;
        }
        fields.append(name, value);
    }
    fields
}

/// The `Content-Length` a handler declared for its response.
///
/// Only digits-only values are accepted, limited to `u64:MAX`. That in particular means that if
/// guest code added multiple `Content-Length` headers, they're ignored entirely, because they'd
/// be represented as a CSV list at this point.
/// Note: RFC 9112 §6.3 would permit a list as long as all values are identical. We don't.
fn declared_content_length(headers: &[(String, String)]) -> Option<u64> {
    let mut fields = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"));
    let (_, value) = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Prepare a dispatched response for sending: the fields [`wire_headers`] allows, no content where
/// the status or the method rules it out, and the body length, if known.
///
/// See [`WireResponse::declared_length`] for how the declared length is derived.
///
/// A `HEAD` still declares the length of the body a `GET` would have produced, where that is known
/// without producing the content. RFC 9110 §9.3.2 allows omitting it otherwise.
pub(crate) fn prepare_wire_response(
    head_request: bool,
    status: u16,
    headers: Vec<(String, String)>,
    body: platform::http::OutgoingBody,
) -> WireResponse {
    let declared = declared_content_length(&headers);
    let headers = wire_headers(headers);

    // The status outranks both the handler's body and the method. A `HEAD` returning `204` is
    // still a `204`.
    if status_forbids_content(status) {
        return WireResponse {
            status,
            headers,
            body: platform::http::OutgoingBody::Bytes(bytes::Bytes::new()),
            declared_length: None,
        };
    }

    // The length of content already in memory, which is the only kind we can measure without
    // producing it.
    let in_memory = match &body {
        platform::http::OutgoingBody::Bytes(bytes) => Some(bytes.len() as u64),
        _ => None,
    };
    // For a `HEAD` request, prioritize a guest-provided `Content-length` header if set, and only
    // fall back to `in_memory` otherwise. Since no actual response body will be sent, guest code
    // might not have produced the same response body it would have for a `GET` request.
    let declared_length = if head_request {
        declared.or(in_memory)
    } else {
        match &body {
            platform::http::OutgoingBody::Bytes(_) => in_memory,
            platform::http::OutgoingBody::Stream(_) | platform::http::OutgoingBody::Host(_) => {
                declared
            }
            platform::http::OutgoingBody::Consumed => None,
        }
    };

    WireResponse {
        status,
        headers,
        body: if head_request {
            platform::http::OutgoingBody::Bytes(bytes::Bytes::new())
        } else {
            body
        },
        declared_length,
    }
}

#[cfg(test)]
mod wire_response_tests {
    use super::*;
    use platform::http::OutgoingBody;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    fn names(response: &WireResponse) -> Vec<String> {
        response
            .headers
            .iter()
            .map(|(name, _)| name.as_str().to_string())
            .collect()
    }

    #[test]
    fn framing_headers_are_dropped_whoever_set_them() {
        let response = prepare_wire_response(
            false,
            200,
            headers(&[
                ("Content-Length", "10000"),
                ("transfer-encoding", "chunked"),
                ("CONNECTION", "keep-alive"),
                ("x-keep", "me"),
            ]),
            OutgoingBody::Bytes(bytes::Bytes::from_static(b"short")),
        );
        assert_eq!(names(&response), ["x-keep"]);
    }

    #[test]
    fn headers_that_could_split_the_response_are_dropped() {
        let response = prepare_wire_response(
            false,
            200,
            headers(&[
                ("x-cr", "a\rb"),
                ("x-lf", "a\nb"),
                ("x-nul", "a\0b"),
                ("x-na\rme", "fine"),
                ("x-keep", "me"),
            ]),
            OutgoingBody::Bytes(bytes::Bytes::new()),
        );
        assert_eq!(names(&response), ["x-keep"]);
    }

    /// The body a response sends, for a test that only ever builds in-memory ones.
    fn content(response: &WireResponse) -> &[u8] {
        match &response.body {
            OutgoingBody::Bytes(bytes) => bytes,
            _ => panic!("the test built an in-memory body"),
        }
    }

    #[test]
    fn a_bodiless_status_has_no_content_even_for_a_get() {
        for status in [204, 304] {
            let response = prepare_wire_response(
                false,
                status,
                headers(&[("x-keep", "me")]),
                OutgoingBody::Bytes(bytes::Bytes::from_static(b"ignored")),
            );
            assert!(content(&response).is_empty(), "{status}");
            assert_eq!(response.declared_length, None, "{status}");
            assert_eq!(names(&response), ["x-keep"], "{status}");
        }
    }

    #[test]
    fn a_head_request_declares_the_length_it_would_have_sent() {
        let response = prepare_wire_response(
            true,
            200,
            Vec::new(),
            OutgoingBody::Bytes(bytes::Bytes::from_static(b"BODYBYTES")),
        );
        assert!(content(&response).is_empty());
        assert_eq!(response.declared_length, Some(9));
    }

    #[test]
    fn a_head_request_declares_no_length_it_would_have_to_produce() {
        // A streamed body's length is only knowable by generating the content.
        let (_sender, streamed) = platform::http::body_channel();
        let response = prepare_wire_response(true, 200, Vec::new(), streamed);
        assert!(content(&response).is_empty());
        assert_eq!(response.declared_length, None);
    }

    /// A streamed body has no length until it is produced, so the handler's declaration is the
    /// only thing that can frame it. The transport then enforces it against the body.
    #[test]
    fn a_streamed_body_takes_the_declared_length() {
        let (_sender, streamed) = platform::http::body_channel();
        let response =
            prepare_wire_response(false, 200, headers(&[("content-length", "42")]), streamed);
        assert_eq!(response.declared_length, Some(42));
        assert_eq!(names(&response), Vec::<String>::new());
    }

    /// Content already in memory is measured. A declaration that disagrees with it would frame the
    /// response as something other than what it sends.
    #[test]
    fn an_in_memory_body_is_measured_not_declared() {
        let response = prepare_wire_response(
            false,
            200,
            headers(&[("content-length", "10000")]),
            OutgoingBody::Bytes(bytes::Bytes::from_static(b"short")),
        );
        assert_eq!(response.declared_length, Some(5));
        assert_eq!(content(&response), b"short");
    }

    /// Only an unambiguous `1*DIGIT` frames a response. A `Headers` object joins repeated fields
    /// into one comma-separated value, which lands here as a value that is not a number.
    #[test]
    fn a_declared_length_that_is_not_one_number_is_refused() {
        for value in ["", "x", "10000, 10000", " 5", "+5", "5.0", "0x5", "-1"] {
            let (_sender, streamed) = platform::http::body_channel();
            let response =
                prepare_wire_response(false, 200, headers(&[("content-length", value)]), streamed);
            assert_eq!(response.declared_length, None, "{value:?}");
        }
    }

    /// RFC 9110 §9.3.2: a `HEAD` may report the length its `GET` would have had.
    #[test]
    fn a_head_declares_the_length_it_was_given() {
        let response = prepare_wire_response(
            true,
            200,
            headers(&[("content-length", "12345")]),
            OutgoingBody::Bytes(bytes::Bytes::new()),
        );
        assert!(content(&response).is_empty());
        assert_eq!(response.declared_length, Some(12345));
    }

    /// The status still outranks the declaration, so nothing frames a response that has no
    /// content.
    #[test]
    fn a_bodiless_status_ignores_a_declared_length() {
        let (_sender, streamed) = platform::http::body_channel();
        let response =
            prepare_wire_response(false, 204, headers(&[("content-length", "42")]), streamed);
        assert_eq!(response.declared_length, None);
    }

    #[test]
    fn an_in_memory_body_declares_its_length() {
        let response = prepare_wire_response(
            false,
            200,
            Vec::new(),
            OutgoingBody::Bytes(bytes::Bytes::from_static(b"BODYBYTES")),
        );
        assert_eq!(content(&response), b"BODYBYTES");
        assert_eq!(response.declared_length, Some(9));
    }

    #[test]
    fn a_bodiless_status_outranks_the_method() {
        let response = prepare_wire_response(
            true,
            204,
            Vec::new(),
            OutgoingBody::Bytes(bytes::Bytes::from_static(b"ignored")),
        );
        assert!(content(&response).is_empty());
        assert_eq!(response.declared_length, None);
    }
}
