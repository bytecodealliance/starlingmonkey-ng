// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Native serve mode: run the content script once (it registers a `fetch`
//! handler with `addEventListener("fetch", …)`), then serve HTTP/1.1 with
//! [`hyper`], dispatching each request as a `fetch` event and responding with the
//! handler's response.
//!
//! Each request runs on its own [`EventLoop`](core_runtime::event_loop::EventLoop), matching the
//! ServiceWorker concurrency model, and requests are served concurrently on the single-threaded
//! async runtime, interleaving at `await` points as each steps its own loop.
//!
//! hyper writes a response body by polling it, and requires it to be `'static`, so a body cannot
//! hold the rooting scope its chunks are produced in. A request therefore hands hyper a
//! [`WireBody`] that reads from a channel. The JS-side work that fills the channel is registered
//! with the serve loop, which drives it until it completes.

#![cfg(not(target_arch = "wasm32"))]

use crate::serve_common::{
    drain_lifetime_work, prepare_wire_response, signal_body_outcome, with_timeout, BodySendOutcome,
    RequestClock, ServeTimeouts, WireResponse,
};
use core_runtime::config::{RuntimeConfig, ServeLimits};
use core_runtime::event_loop::{run_to_completion, run_until_evaluated, EventLoop};
use core_runtime::invocation::{InvocationState, OwnedInvocation};
use core_runtime::runtime::Runtime;
use futures_channel::mpsc::UnboundedSender;
use futures_util::stream::{FuturesUnordered, StreamExt};
use hyper::body::{Body, Frame, Incoming};
use js::gc::scope::{EnteredRealm, RootScope};
use platform::http::OutgoingBody;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use web_globals::signals::abort_controller::AbortControllerImpl;

/// The content script, kept for `--serve-isolated` to re-evaluate into each request's global.
struct ContentScript {
    source: String,
    filename: String,
    module_mode: bool,
}

/// Where a served request's JS runs.
enum RequestHandlingMode {
    /// In the process-wide global, whose realm the serve loop holds entered for the server's life.
    Shared,
    /// In a global of the request's own, with the content script evaluated into it first.
    Isolated(ContentScript),
}

/// Everything a connection needs that outlives the requests it serves: the runtime to dispatch in,
/// where each request's JS runs, and the time limits under which each request runs.
struct ServeContext {
    runtime: Rc<Runtime>,
    request_handling_mode: RequestHandlingMode,
    timeouts: ServeTimeouts,
    limits: ServeLimits,
}

impl ServeContext {
    /// Whether a connection may serve more than one request. Isolated mode holds a request's own
    /// realm entered across the work that outlives its response, so nothing of one request may
    /// overlap the next.
    fn keep_alive(&self) -> bool {
        matches!(self.request_handling_mode, RequestHandlingMode::Shared)
    }

    /// The guard a request holds for its realm: an isolated request's own global, rooted and with
    /// its realm entered until the request's last work drops the guard. Only `Some()` in isolated
    /// mode, otherwise the runtime's default realm is used for all requests.
    fn enter_request_realm(&self) -> Option<RootScope<'_, EnteredRealm>> {
        match &self.request_handling_mode {
            RequestHandlingMode::Shared => None,
            RequestHandlingMode::Isolated(_) => Some(self.runtime.new_global()),
        }
    }

    /// The content script to evaluate into each request's own global. `None` in shared mode,
    /// where the script already ran at startup.
    fn per_request_script(&self) -> Option<&ContentScript> {
        match &self.request_handling_mode {
            RequestHandlingMode::Shared => None,
            RequestHandlingMode::Isolated(script) => Some(script),
        }
    }
}

/// Bind `127.0.0.1:port`, or a port the system picks when `port` is `0`, and run the serve loop on
/// it until the process is killed. See [`serve_with_shutdown`].
pub fn serve(config: RuntimeConfig, port: u16) -> Result<(), String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("Failed to bind 127.0.0.1:{port}: {e}"))?;
    serve_with_shutdown(config, listener, std::future::pending())
}

/// Run the serve loop on `listener`: print the listener's address on stderr, evaluate the script
/// (registering handlers), then accept and dispatch requests until `shutdown` resolves. Requests
/// still in flight at that point are abandoned. A caller that shuts the server down this way, such
/// as a test, lets the runtime drop cleanly.
pub fn serve_with_shutdown(
    config: RuntimeConfig,
    listener: std::net::TcpListener,
    shutdown: impl Future<Output = ()>,
) -> Result<(), String> {
    config.validate_serve_timeouts()?;
    config.serve_limits.validate()?;
    // Isolated mode has no process-wide content script: the script belongs to each request's own
    // global, and every request evaluates it there. Evaluating it here as well would run it (and
    // its side effects) one extra time, in a global that never serves a request, so that mode
    // only sets up the runtime itself.
    let (runtime, startup) = if config.serve_isolated {
        (Runtime::init(&config)?, None)
    } else {
        let (runtime, invocation, evaluation) = core_runtime::setup_for_serve(config.clone())?;
        (runtime, Some((invocation, evaluation)))
    };
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to create tokio runtime: {e}"))?;
    tokio_rt.block_on(run_server(runtime, startup, config, listener, shutdown))
}

/// One step of the accept loop.
enum Step {
    Accepted(std::io::Result<(TcpStream, std::net::SocketAddr)>),
    ConnectionDone,
    /// A registered piece of request work finished, so the capacity check runs again.
    WorkDone,
    Shutdown,
}

async fn run_server(
    runtime: Rc<Runtime>,
    startup: Option<(InvocationState, core_runtime::ScriptEvaluation)>,
    config: RuntimeConfig,
    listener: std::net::TcpListener,
    shutdown: impl Future<Output = ()>,
) -> Result<(), String> {
    // An isolated request serves from a global of its own, not the default global, but the
    // default global's realm is still the one this loop holds entered.
    let server_realm = runtime.default_global();
    // SAFETY: `server_realm` (and the `Runtime` it borrows) lives for the whole serve loop below.
    let raw_cx = unsafe { server_realm.raw_cx_no_gc() };

    let dispatch = if config.serve_isolated {
        // Under `--serve-isolated` every request gets its own global, which needs the content
        // script run in it to register a handler. Read the source once here rather than per
        // request.
        let (source, filename) = core_runtime::content_script(&config)?;
        RequestHandlingMode::Isolated(ContentScript {
            source,
            filename,
            module_mode: config.module_mode(),
        })
    } else {
        RequestHandlingMode::Shared
    };

    let bound = listener
        .local_addr()
        .map_err(|e| format!("Failed to read the listener's bound address: {e}"))?;
    let listener = listener
        .set_nonblocking(true)
        .and_then(|()| TcpListener::from_std(listener))
        .map_err(|e| format!("Failed to listen on {bound}: {e}"))?;
    eprintln!("serving on http://{bound}");

    // Let the script finish evaluating before serving, so a handler registered after a top-level
    // `await` is in place for the first request. Binding first leaves a client that connects
    // meanwhile in the listen backlog rather than refused.
    //
    // The invocation then lives on for the server's lifetime, driven by the accept loop and
    // GC-traced throughout, so whatever timers the script started keep firing.
    //
    // Note: isolated mode has no script here. Each request evaluates its own.
    let mut background = startup
        .map(|(state, evaluation)| (OwnedInvocation::new(runtime.clone(), state), evaluation));
    if let Some((invocation, evaluation)) = background.as_mut() {
        unsafe {
            run_until_evaluated(
                raw_cx,
                invocation.state().event_loop(),
                tokio::time::sleep,
                evaluation,
            )
            .await
        };
        // Checked only once the script has finished, since a handler may be registered after a
        // top-level `await`. Isolated mode has no script here to check. Its per-request
        // evaluation sends a 500 instead.
        evaluation.rejection(&server_realm, "Script evaluation failed")?;
        if !fetch_event::fetch_event::FetchEvent::has_listener(&server_realm) {
            return Err(crate::serve_common::NO_FETCH_LISTENER.to_string());
        }
    }
    // The background loop is borrowed for the rest of the function. `background` is not touched
    // again, only released and dropped on the way out. Isolated mode has none of this: with a
    // per-request global entered for each request, there is no single realm a background loop
    // could be stepped in.
    let background_loop = background
        .as_mut()
        .map(|(invocation, _)| invocation.state().event_loop());

    // We clone rather than move the caller's handle. `server_realm` points into this runtime, and
    // a local holding the last reference would free the runtime on the way out ahead of
    // `server_realm`, which is dropped last of the locals. Leaving the parameter binding as the
    // owner keeps the runtime alive past every local, since parameters drop last of all.
    let context = ServeContext {
        runtime: runtime.clone(),
        request_handling_mode: dispatch,
        timeouts: ServeTimeouts::from_config(&config),
        limits: config.serve_limits,
    };
    // Connections run concurrently on the single thread, interleaving at their loops' await
    // points. Isolated mode serializes instead, whatever `--max-connections` is set to: each
    // request holds its own realm entered across awaits, and two in flight would drop those
    // entries out of order, corrupting the realm stack.
    let max_in_flight = if context.keep_alive() {
        context.limits.max_connections
    } else {
        1
    };
    // The work requests leave behind once their response head is handed to hyper (see
    // [`BackgroundWork`]). The loop below drives it until it completes.
    let (work_sender, mut registered) = futures_channel::mpsc::unbounded();
    let mut work: FuturesUnordered<BackgroundWork> = FuturesUnordered::new();

    // Each iteration races the shutdown signal, a new connection, the registered request work,
    // an in-flight connection finishing and the script's background work, so all make progress.
    let mut in_flight = FuturesUnordered::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        // Take work registered since the work arm below last ran, so the capacity check sees it.
        while let Ok(item) = registered.try_recv() {
            work.push(item);
        }
        // Isolated mode also waits for the registered work to ensure requests are fully
        // sequentialized.
        let at_capacity =
            in_flight.len() >= max_in_flight || (!context.keep_alive() && !work.is_empty());
        let nothing_in_flight = in_flight.is_empty();
        let step = futures_lite::future::or(
            async {
                shutdown.as_mut().await;
                Step::Shutdown
            },
            futures_lite::future::or(
                async {
                    // Stop accepting while at the concurrency cap. New connections wait in
                    // the listen backlog until a slot frees up.
                    if at_capacity {
                        std::future::pending::<()>().await;
                    }
                    Step::Accepted(listener.accept().await)
                },
                futures_lite::future::or(
                    async {
                        // Drive the registered request work. Polled before the connections' arm,
                        // so a close signal this work sends reaches its connection before hyper
                        // is polled and can start on a request already sitting in its read
                        // buffer. A completed item yields a step, so the capacity check above
                        // sees the work set shrink.
                        std::future::poll_fn(|cx| {
                            while let Poll::Ready(Some(item)) = registered.poll_next_unpin(cx) {
                                work.push(item);
                            }
                            // An empty set reports `None` rather than parking. The `registered`
                            // poll above has already scheduled a wake for when new work arrives.
                            match work.poll_next_unpin(cx) {
                                Poll::Ready(Some(())) => Poll::Ready(Step::WorkDone),
                                Poll::Ready(None) | Poll::Pending => Poll::Pending,
                            }
                        })
                        .await
                    },
                    futures_lite::future::or(
                        async {
                            // `in_flight.next()` resolves immediately with `None` on an empty
                            // set, so this arm parks until a connection is in flight.
                            if nothing_in_flight {
                                std::future::pending::<()>().await;
                            }
                            in_flight.next().await;
                            Step::ConnectionDone
                        },
                        async {
                            // Keep the content script's own loop running alongside serving, so a
                            // `setInterval` it left running actually fires. This arm never yields
                            // a `Step`: it runs for as long as the server does, and its timers
                            // wake this task when they come due. A `setInterval(…, 0)` cannot
                            // starve the accept loop, since the re-queued timer is clamped to a
                            // non-zero delay.
                            // TODO: same as in wasm: should reconsider this and abort work not
                            // added to waitUntil.
                            if let Some(event_loop) = background_loop {
                                // SAFETY: `raw_cx` is valid, with the serve loop's realm entered,
                                // for the server's life.
                                unsafe {
                                    run_to_completion(raw_cx, event_loop, tokio::time::sleep).await
                                };
                            }
                            std::future::pending::<Step>().await
                        },
                    ),
                ),
            ),
        )
        .await;
        match step {
            Step::Shutdown => {
                // The script's own loop is abandoned here with whatever it still had in flight
                // (e.g. a pending `fetch` or a timer), so it is released while the JSContext is
                // alive rather than at thread teardown. See [`RequestLoop`] for what that means.
                // The requests' loops release themselves as `work` and `in_flight` drop.
                if let Some(event_loop) = background_loop {
                    event_loop.cancel_pending_futures();
                }
                return Ok(());
            }
            Step::Accepted(Ok((stream, _))) => {
                in_flight.push(serve_connection(&context, work_sender.clone(), stream))
            }
            Step::Accepted(Err(e)) => {
                // A per-connection failure (ECONNABORTED/ECONNRESET during the handshake,
                // EMFILE under fd pressure) must not tear the server down. Log, sleep briefly
                // so a persistent condition doesn't spin hot, and keep accepting.
                eprintln!("serve: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Step::ConnectionDone | Step::WorkDone => {}
        }
    }
}

/// A request's event loop, released once all work related to the request is done or abandoned.
///
/// [`EventLoop::cancel_pending_futures`] has to run while the JSContext is alive. The paths that
/// finish normally run it themselves (see [`drain_lifetime_work`]). The `Drop` impl runs when the
/// server is shutting down while event loops are active.
struct RequestLoop(OwnedInvocation);

impl RequestLoop {
    fn new(runtime: &Rc<Runtime>) -> Self {
        RequestLoop(OwnedInvocation::new(
            runtime.clone(),
            InvocationState::new(),
        ))
    }

    fn state(&self) -> &InvocationState {
        self.0.state()
    }

    fn event_loop(&self) -> &EventLoop {
        self.0.state().event_loop()
    }
}

impl Drop for RequestLoop {
    fn drop(&mut self) {
        // `cancel_pending_futures` is idempotent, so it's safe to always run this.
        self.0.state().event_loop().cancel_pending_futures();
    }
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// Work on a per-request event loop after the response headers have been sent: streaming the
/// response body and handling `waitUntil`-registered async tasks.
type BackgroundWork<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// The context for processing an incoming request.
struct RequestContext<'a> {
    /// Global server configuration and state.
    context: &'a ServeContext,
    /// The channel by which the request registers background work to complete after sending the
    /// response headers.
    background_work_sender: UnboundedSender<BackgroundWork<'a>>,
    /// Channel to signal when the connection must be closed instead of accepting additional
    /// incoming requests.
    signal_close: UnboundedSender<()>,
}

/// Serve one connection: hand it to hyper, which reads its requests in sequence and dispatches
/// each through [`serve_request`]. Returns when the connection closes. The work its requests
/// registered with the serve loop keeps running: whatever response body hyper still held is
/// dropped here, so its producer receives `ConnectionLost` rather than waiting forever.
async fn serve_connection<'a>(
    context: &'a ServeContext,
    background_work_sender: UnboundedSender<BackgroundWork<'a>>,
    stream: TcpStream,
) {
    let (signal_close, mut closed) = futures_channel::mpsc::unbounded();
    let request_context = RequestContext {
        context,
        background_work_sender,
        signal_close,
    };

    let limits = context.limits;
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(hyper_util::rt::TokioTimer::new())
        .keep_alive(context.keep_alive())
        .max_headers(limits.max_request_headers)
        .max_buf_size(limits.max_connection_buffer_size.bytes() as usize);
    // One timer limits both a client that stalls part-way through sending a head and one that
    // leaves a kept-alive connection idle without starting the next request. hyper closes the
    // connection when it elapses.
    if let Some(silence) = head_read_time_limit(limits) {
        builder.header_read_timeout(silence);
    }

    let mut serving = std::pin::pin!(builder.serve_connection(
        hyper_util::rt::TokioIo::new(stream),
        hyper::service::service_fn(|request| serve_request(&request_context, request)),
    ));
    // An unfinished response body or a request body left unread results in the connection being
    // closed: it stops taking requests and ends once the one in flight is done.
    let mut ending = false;
    let served = std::future::poll_fn(|cx| {
        // Checked before hyper is polled, so a close signal sent by this connection's request
        // work takes effect before hyper can start on a request already sitting in its read buffer.
        if !ending {
            if let Poll::Ready(Some(())) = closed.poll_next_unpin(cx) {
                serving.as_mut().graceful_shutdown();
                ending = true;
            }
        }
        serving.as_mut().poll(cx)
    })
    .await;
    if let Err(e) = served {
        // Ignore clients that disconnect mid-request.
        if !e.is_incomplete_message() {
            eprintln!("serve: connection error: {e}");
        }
    }
}

/// How long a client may stay silent before its connection is closed: the shorter of the read and
/// keep-alive time limits, since one timer covers a stalled head and an idle connection alike.
fn head_read_time_limit(limits: ServeLimits) -> Option<Duration> {
    match (limits.request_read_timeout(), limits.keepalive_timeout()) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (limit, None) | (None, limit) => limit,
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Serve one request: dispatch it as a `fetch` event on its own event loop and respond with the
/// handler's response. The work that continues past the response head (body streaming and the
/// `waitUntil` drain) is registered with the serve loop.
///
/// Never fails: a request that cannot be dispatched gets a status instead, so the
/// error type is `Infallible`.
async fn serve_request<'a>(
    request_context: &RequestContext<'a>,
    request: hyper::Request<Incoming>,
) -> Result<hyper::Response<WireBody>, std::convert::Infallible> {
    let context = request_context.context;
    let limits = context.limits;

    let (mut head, incoming) = request.into_parts();
    if let Err(status) = reject_multiple_host_headers(&head) {
        return Ok(status_response(status));
    }
    let content_length = match declared_body_length(&head, limits) {
        Ok(length) => length,
        // A declared length over the cap is refused here, before the handler runs.
        Err(status) => return Ok(status_response(status)),
    };

    // Everything from here on (isolated mode's startup, the dispatch, the response) is this
    // request's own work, so its end-to-end deadline starts here.
    let clock = context.timeouts.start_clock();

    let invocation = RequestLoop::new(&context.runtime);
    let realm = context.enter_request_realm();

    if let Some(script) = context.per_request_script() {
        if let Err(status) =
            evaluate_into_request_global(context, &invocation, script, &clock).await
        {
            return Ok(status_response(status));
        }
    }

    let (incoming_body, mut incoming_pump) = if has_body(&head, content_length) {
        let (body, pump) =
            request_body_and_pump(incoming, limits, request_context.signal_close.clone());
        (Some(body), pump)
    } else {
        (None, BodyPump(None))
    };

    let head_request = head.method == hyper::Method::HEAD;
    let (abort_controller, response_parts) = {
        // The dispatch's rooting scope. The response parts taken from it are plain data, and the
        // abort controller is re-rooted into a `RootedHeap`, so nothing rooted here is needed
        // past this block.
        // SAFETY: the request's realm is entered (see `enter_request_realm`).
        let scope = unsafe { context.runtime.scope() };
        let dispatch = crate::serve_common::dispatch_fetch(
            &scope,
            invocation.state(),
            head.method.to_string(),
            request_url(&head),
            std::mem::take(&mut head.headers),
            incoming_body,
            content_length,
            tokio::time::sleep,
            &clock,
        );
        // The pump side never completes: `alongside` parks forever once the body is read.
        let dispatched =
            futures_lite::future::or(async { Some(dispatch.await) }, incoming_pump.alongside())
                .await
                .expect("only the dispatch side of the race completes");
        let abort_controller = dispatched.as_ref().map(|d| d.rooted_abort_controller());
        (
            abort_controller,
            dispatched.and_then(|mut d| d.response.take()),
        )
    };

    let (response, producer) = match response_parts {
        Some((status, headers, outgoing_body)) => {
            let WireResponse {
                status,
                headers,
                body,
                declared_length,
            } = prepare_wire_response(head_request, status, headers, outgoing_body);
            let (wire_body, producer) =
                create_body_channel(body, declared_length, clock.response_body_time_limit());
            (
                build_response(status, headers, head_request, declared_length, wire_body),
                producer,
            )
        }
        // No handler, no call to `respondWith`, or the latter didn't result in a `Response`.
        None => (status_response(500), None),
    };

    // Everything past the response head (streaming the bodies, the `waitUntil` drain) runs as
    // work registered with the serve loop.
    let work: BackgroundWork<'a> = Box::pin(finish_request(
        realm,
        context,
        invocation,
        abort_controller,
        producer,
        incoming_pump,
        clock,
        request_context.signal_close.clone(),
    ));
    // The serve loop outlives its requests, so this can only fail during server shutdown.
    let _ = request_context.background_work_sender.unbounded_send(work);

    Ok(response)
}

/// Evaluate the content script into a request's own global, so the handler it registers is in
/// place before the event is dispatched. `Err` is the status to respond with instead.
async fn evaluate_into_request_global(
    context: &ServeContext,
    invocation: &RequestLoop,
    script: &ContentScript,
    clock: &RequestClock,
) -> Result<(), u16> {
    // Module objects are cached per global, so a new global per request requires clearing the
    // registry first, because for now the registry doesn't key modules on the global.
    if script.module_mode {
        context.runtime.clear_module_registry();
    }
    // SAFETY: the caller holds the request's realm entered, and the module loader was initialized
    // by `Runtime::init`.
    let (raw_cx, evaluated) = unsafe {
        let scope = context.runtime.scope();
        let raw_cx = scope.raw_cx_no_gc();
        let evaluated = core_runtime::evaluate_content_script(
            &scope,
            invocation.event_loop(),
            &script.source,
            &script.filename,
            script.module_mode,
        );
        (raw_cx, evaluated)
    };
    let evaluation = match evaluated {
        Ok(evaluation) => evaluation,
        Err(message) => {
            eprintln!("serve: content script evaluation failed: {message}");
            return Err(500);
        }
    };
    // Run the event loop until top-level-await'ed promises have settled.
    //
    // SAFETY: as above.
    let startup = unsafe {
        run_until_evaluated(
            raw_cx,
            invocation.event_loop(),
            tokio::time::sleep,
            &evaluation,
        )
    };
    if with_timeout(&tokio::time::sleep, clock.remaining(), startup)
        .await
        .is_none()
    {
        eprintln!("serve: the content script's startup ran into the end-to-end timeout");
        return Err(500);
    }
    // SAFETY: the caller holds the request's realm entered.
    let scope = unsafe { js::gc::scope::RootScope::from_current_realm(raw_cx) };
    if let Err(message) = evaluation.rejection(&scope, "Script evaluation failed") {
        eprintln!("serve: content script evaluation failed: {message}");
        return Err(500);
    }
    Ok(())
}

/// Finish a request once the response headers have been sent: ensure incoming and outgoing bodies
/// are streamed and `waitUntil` registered promises are settled.
///
/// `_realm` is the first parameter so it drops last: the request's realm outlives the release of
/// its event loop.
async fn finish_request(
    _realm: Option<RootScope<'_, EnteredRealm>>,
    context: &ServeContext,
    invocation: RequestLoop,
    abort_controller: Option<js::gc::handle::RootedHeap<AbortControllerImpl>>,
    producer: Option<BodyProducer>,
    mut incoming_pump: BodyPump,
    clock: RequestClock,
    signal_connection_close: UnboundedSender<()>,
) {
    // SAFETY: the request's realm is entered: `_realm` holds an isolated request's own realm,
    // and the serve loop holds the shared realm entered for the server's life.
    let raw_cx = unsafe { context.runtime.scope().raw_cx_no_gc() };

    if let Some(producer) = producer {
        let sending = send_response_body(producer, raw_cx, invocation.event_loop());
        // The pump side never completes: `alongside` parks forever once the body is read.
        let outcome =
            futures_lite::future::or(async { Some(sending.await) }, incoming_pump.alongside())
                .await
                .expect("only the sending side of the race completes");
        // Close the connection if the body could not fully be sent.
        if !matches!(outcome, BodySendOutcome::Sent) {
            let _ = signal_connection_close.unbounded_send(());
        }
        // SAFETY: as above.
        let scope = unsafe { context.runtime.scope() };
        signal_body_outcome(
            &scope,
            invocation.event_loop(),
            abort_controller.as_ref(),
            outcome,
        );
    }

    // SAFETY: as above.
    let lifetime =
        unsafe { drain_lifetime_work(raw_cx, invocation.event_loop(), tokio::time::sleep, &clock) };
    // After the response body, keep the event loop running for as long as interest in it is
    // signaled, and finish reading the incoming body. `zip` waits for both.
    futures_lite::future::zip(lifetime, incoming_pump.drain()).await;
}

/// RFC 9112 §3.2 requires a `400` for more than one Host field: an intermediary that routes by
/// the first and a server that routes by the last disagree about where the request was addressed.
fn reject_multiple_host_headers(head: &http::request::Parts) -> Result<(), u16> {
    if head.headers.get_all(http::header::HOST).iter().count() > 1 {
        return Err(400);
    }
    Ok(())
}

/// Whether the request has a body.
fn has_body(head: &http::request::Parts, content_length: Option<u64>) -> bool {
    content_length.is_some() || head.headers.contains_key(http::header::TRANSFER_ENCODING)
}

/// The value of the incoming request's `Content-Length` header, or `None` if it's absent.
///
/// Returns `Err(status_code)` if the value is malformed or the length limit is exceeded.
fn declared_body_length(
    head: &http::request::Parts,
    limits: ServeLimits,
) -> Result<Option<u64>, u16> {
    let Some(value) = head.headers.get(http::header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let length: u64 = value
        .to_str()
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or(400u16)?;
    if length > limits.max_request_body_bytes.bytes() {
        return Err(413);
    }
    Ok((length > 0).then_some(length))
}

/// The request's absolute URL.
///
/// If the `uri` field is already absolute, it's returned as-is. Otherwise, the absolute URL is
/// constructed as `"http://{http::header::HOST}{uri}"`, or `"http://localhost{uri}"`, if the `Host`
/// header is absent.
fn request_url(head: &http::request::Parts) -> String {
    if head.uri.scheme().is_some() {
        return head.uri.to_string();
    }
    let host = head
        .headers
        .get(http::header::HOST)
        .and_then(|host| host.to_str().ok())
        .unwrap_or("localhost");
    let target = head
        .uri
        .path_and_query()
        .map(|target| target.as_str())
        .unwrap_or("/");
    format!("http://{host}{target}")
}

/// Build an empty response with just a status, typically for an error response.
fn status_response(status: u16) -> hyper::Response<WireBody> {
    let message = reason_phrase(status);
    hyper::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "text/plain")
        .body(WireBody::Complete(bytes::Bytes::from_static(
            message.as_bytes(),
        )))
        .expect("a status and two fixed fields build a valid response")
}

/// The reason phrase for the statuses of the server's internal responses.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        _ => "",
    }
}

/// The future that reads the incoming request body ([`pump_request_body`]), driven alongside the
/// request's other work via [`BodyPump::alongside`] and to its end via [`BodyPump::drain`].
struct BodyPump(Option<Pin<Box<dyn Future<Output = ()>>>>);

impl BodyPump {
    /// Read the rest of the body.
    async fn drain(&mut self) {
        if let Some(pump) = self.0.as_mut() {
            pump.await;
            self.0 = None;
        }
    }

    /// Read the rest of the body, then wait forever. Raced against another future, this keeps
    /// the body draining while that future runs, without ending the race when the body finishes.
    async fn alongside<T>(&mut self) -> T {
        self.drain().await;
        std::future::pending().await
    }
}

/// Set up a `BodyPump` for `incoming` and return the pump and the receiving end of the channel
/// it operates on.
fn request_body_and_pump(
    incoming: Incoming,
    limits: ServeLimits,
    signal_connection_close: UnboundedSender<()>,
) -> (platform::http::IncomingBody, BodyPump) {
    let (sender, body) = platform::http::incoming_body_channel();
    (
        body,
        BodyPump(Some(Box::pin(pump_request_body(
            incoming,
            sender,
            limits,
            signal_connection_close,
        )))),
    )
}

/// The sending side of the channel `BodyPump` operates on.
///
/// If the channel is closed, or the time limit for reading the body has elapsed, the rest of the
/// body is discarded, up to [`ServeLimits::max_body_drain_bytes`]. That way, the connection can
/// still be used for the next request.
enum BodySink {
    Reading(platform::http::IncomingBodySender),
    Discarding { budget: u64 },
}

impl BodySink {
    /// Take one decoded chunk. `Err` means the discard budget ran out and the connection must
    /// close.
    async fn accept(&mut self, chunk: bytes::Bytes, limits: ServeLimits) -> Result<(), String> {
        let chunk_length = chunk.len() as u64;
        if let BodySink::Reading(sender) = self {
            let sent = with_timeout(
                &tokio::time::sleep,
                limits.request_read_timeout(),
                sender.send_chunk(chunk),
            )
            .await;
            match sent {
                // Chunk sent successfully.
                Some(true) => return Ok(()),
                // Channel closed, or the time limit for reading the body has elapsed.
                Some(false) | None => {
                    *self = BodySink::Discarding {
                        budget: limits.max_body_drain_bytes.bytes(),
                    }
                }
            }
        }
        let BodySink::Discarding { budget } = self else {
            unreachable!("either the full chunk was read, or we're discarding");
        };
        *budget = budget.checked_sub(chunk_length).ok_or_else(|| {
            "the rest of the unread request body exceeds the drain budget".to_string()
        })?;
        Ok(())
    }

    /// Give up on the body: abort the handler's stream with `message`, and signal the incoming
    /// connection to close instead of accepting additional incoming requests.
    async fn fail(&mut self, signal_connection_close: &UnboundedSender<()>, message: String) {
        let _ = signal_connection_close.unbounded_send(());
        if let BodySink::Reading(sender) = self {
            sender.send_error(message).await;
        }
    }
}

/// Read chunks from the incoming request body, and either send them on via `sender`, or discard
/// them.
///
/// See [`BodySink`] for details.
async fn pump_request_body(
    mut incoming: Incoming,
    sender: platform::http::IncomingBodySender,
    limits: ServeLimits,
    signal_connection_close: UnboundedSender<()>,
) {
    let mut sink = BodySink::Reading(sender);
    let mut read = 0u64;
    loop {
        let frame = with_timeout(
            &tokio::time::sleep,
            limits.request_read_timeout(),
            std::future::poll_fn(|cx| Pin::new(&mut incoming).poll_frame(cx)),
        )
        .await;
        let chunk = match frame {
            None => {
                sink.fail(
                    &signal_connection_close,
                    "the request body stalled".to_string(),
                )
                .await;
                return;
            }
            Some(None) => return,
            Some(Some(Err(e))) => {
                sink.fail(
                    &signal_connection_close,
                    format!("the request body failed mid-stream: {e}"),
                )
                .await;
                return;
            }
            Some(Some(Ok(frame))) => match frame.into_data() {
                Ok(chunk) => chunk,
                // We're ignoring trailers for the time being.
                Err(frame) => {
                    debug_assert!(frame.is_trailers());
                    continue;
                }
            },
        };
        read += chunk.len() as u64;
        if read > limits.max_request_body_bytes.bytes() {
            sink.fail(
                &signal_connection_close,
                format!(
                    "the request body exceeds the configured limit of {}",
                    limits.max_request_body_bytes
                ),
            )
            .await;
            return;
        }
        // Split so that a frame larger than `BODY_READ_BYTES` cannot occupy a body-channel slot
        // whole. An unread body then buffers at most the channel's capacity times
        // `BODY_READ_BYTES`, whatever sizes frames arrive in.
        for chunk in split_chunk(chunk) {
            if let Err(message) = sink.accept(chunk, limits).await {
                sink.fail(&signal_connection_close, message).await;
                return;
            }
        }
    }
}

/// How much of a request body is handed on at a time.
// TODO: should consider matching Hyper's adaptive chunk sizing instead.
const BODY_READ_BYTES: usize = 32 * 1024;

/// Split a frame into pieces of at most [`BODY_READ_BYTES`].
fn split_chunk(mut chunk: bytes::Bytes) -> impl Iterator<Item = bytes::Bytes> {
    std::iter::from_fn(move || {
        (!chunk.is_empty()).then(|| chunk.split_to(chunk.len().min(BODY_READ_BYTES)))
    })
}

// ---------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------

/// How much of a body already in memory is handed to hyper at a time.
const WIRE_CHUNK_BYTES: usize = 64 * 1024;

/// The error a streamed [`WireBody`] yields to hyper when its body cannot be completed. It causes
/// hyper to close the connection without completing the message framing, so the client can detect
/// the truncation instead of treating the bytes received so far as a complete body
/// (RFC 9112 §7.1).
#[derive(Debug)]
struct BodyError(String);

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BodyError {}

/// The reason a body ended before all of its content was sent, shared between the producer and
/// the [`WireBody`] hyper polls.
///
/// The producer sets it before closing the channel. When the channel ends, [`WireBody`] checks
/// it: a set reason ends the body with an error, and an empty one ends it normally.
type BodyAbort = Rc<std::cell::RefCell<Option<String>>>;

/// The response body hyper writes.
enum WireBody {
    /// A body already in memory. Empty if the status or the method leaves no content to send.
    Complete(bytes::Bytes),
    /// Chunks the connection's background work feeds in as the handler's body produces them.
    Streamed {
        chunks: futures_channel::mpsc::Receiver<Result<bytes::Bytes, BodyError>>,
        /// The length the body is known to have. When present, the response is framed by
        /// `Content-Length` rather than chunked.
        length: Option<u64>,
        /// How much of `length` hyper has taken. hyper does not poll a body of a declared
        /// length past that length, so the `Drop` impl below treats a body that delivered all of
        /// `length` as sent whole.
        delivered: u64,
        abort: BodyAbort,
        /// How the send ended. Used by the producer to determine whether the handler's body
        /// must be aborted. Taken when the body reaches its end, so a body still holding one was
        /// dropped early.
        outcome: Option<futures_channel::oneshot::Sender<BodySendOutcome>>,
    },
}

impl WireBody {
    fn report(&mut self, outcome: BodySendOutcome) {
        if let WireBody::Streamed { outcome: slot, .. } = self {
            if let Some(slot) = slot.take() {
                let _ = slot.send(outcome);
            }
        }
    }
}

impl Body for WireBody {
    type Data = bytes::Bytes;
    type Error = BodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<bytes::Bytes>, BodyError>>> {
        match &mut *self {
            WireBody::Complete(bytes) => {
                if bytes.is_empty() {
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Ok(Frame::data(std::mem::take(bytes)))))
            }
            WireBody::Streamed {
                chunks,
                abort,
                delivered,
                length,
                ..
            } => {
                let declared = *length;
                match chunks.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(chunk))) => {
                        // hyper writes at most the declared length, so content past it never
                        // reaches the client and must not count towards what was delivered.
                        *delivered = delivered.saturating_add(chunk.len() as u64);
                        if let Some(declared) = declared {
                            *delivered = (*delivered).min(declared);
                        }
                        Poll::Ready(Some(Ok(Frame::data(chunk))))
                    }
                    Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
                    Poll::Ready(None) => {
                        let aborted = abort.borrow_mut().take();
                        // A body that ends under a declared length leaves hyper mid-message, which
                        // causes it to close the connection.
                        let short = declared.is_some_and(|declared| *delivered < declared);
                        match aborted {
                            Some(message) => Poll::Ready(Some(Err(BodyError(message)))),
                            None => {
                                self.report(if short {
                                    BodySendOutcome::Truncated
                                } else {
                                    BodySendOutcome::Sent
                                });
                                Poll::Ready(None)
                            }
                        }
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            WireBody::Complete(bytes) => bytes.is_empty(),
            WireBody::Streamed { .. } => false,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            WireBody::Complete(bytes) => http_body::SizeHint::with_exact(bytes.len() as u64),
            WireBody::Streamed { length, .. } => match length {
                Some(length) => http_body::SizeHint::with_exact(*length),
                None => http_body::SizeHint::default(),
            },
        }
    }
}

impl Drop for WireBody {
    fn drop(&mut self) {
        // hyper stops polling a body of a declared length once it has taken that much, and
        // drops it rather than reading it to its end. A body that delivered all of its length was
        // therefore sent whole.
        if let WireBody::Streamed {
            length: Some(length),
            delivered,
            ..
        } = self
        {
            if delivered >= length {
                self.report(BodySendOutcome::Sent);
                return;
            }
        }
        self.report(BodySendOutcome::ConnectionLost(
            "the connection was lost while the response body was being written".to_string(),
        ));
    }
}

/// The producing half of a streamed [`WireBody`]: the handler's body, and the channel its chunks
/// are sent on.
struct BodyProducer {
    body: OutgoingBody,
    chunks: futures_channel::mpsc::Sender<Result<bytes::Bytes, BodyError>>,
    abort: BodyAbort,
    outcome: futures_channel::oneshot::Receiver<BodySendOutcome>,
    /// How long the body has to reach the client.
    bound: Option<Duration>,
    /// The `Content-Length` the response is framed by, which the body may not exceed.
    declared_length: Option<u64>,
}

/// Create a channel by which handler-produced body chunks are sent to hyper, which then sends
/// them out over the network. The receiving end is wrapped in [`WireBody`], along with the
/// `declared_length`. The sending side is only created for non-empty bodies.
fn create_body_channel(
    body: OutgoingBody,
    declared_length: Option<u64>,
    bound: Option<Duration>,
) -> (WireBody, Option<BodyProducer>) {
    match body {
        OutgoingBody::Consumed => (WireBody::Complete(bytes::Bytes::new()), None),
        OutgoingBody::Bytes(bytes) if bytes.is_empty() => (WireBody::Complete(bytes), None),
        body => {
            let (sender, receiver) = futures_channel::mpsc::channel(0);
            let (outcome_tx, outcome_rx) = futures_channel::oneshot::channel();
            let abort: BodyAbort = Rc::new(std::cell::RefCell::new(None));
            (
                WireBody::Streamed {
                    chunks: receiver,
                    length: declared_length,
                    delivered: 0,
                    abort: abort.clone(),
                    outcome: Some(outcome_tx),
                },
                Some(BodyProducer {
                    body,
                    chunks: sender,
                    abort,
                    outcome: outcome_rx,
                    bound,
                    declared_length,
                }),
            )
        }
    }
}

/// Build the response head hyper sends: the status, the headers the handler set, and, for a
/// `HEAD` request, an explicit `Content-Length`.
fn build_response(
    status: u16,
    mut headers: http::HeaderMap,
    head_request: bool,
    declared_length: Option<u64>,
    body: WireBody,
) -> hyper::Response<WireBody> {
    // When a body is sent, hyper sets `Content-Length` from its size hint. A `HEAD` response
    // sends no body, so the length the body would have is set manually here when it is known.
    if let (true, Some(length)) = (head_request, declared_length) {
        headers.insert(
            http::header::CONTENT_LENGTH,
            http::HeaderValue::from(length),
        );
    }
    let mut builder = hyper::Response::builder().status(status);
    *builder
        .headers_mut()
        .expect("a status alone leaves the builder valid") = headers;
    builder
        .body(body)
        .expect("a status alone leaves the builder valid")
}

/// Feed a streamed response body to hyper as the handler's JS produces it, stepping the request's
/// event loop so the producing side runs, and report how the send ended.
async fn send_response_body(
    producer: BodyProducer,
    raw_cx: *mut js::native::RawJSContext,
    event_loop: &EventLoop,
) -> BodySendOutcome {
    let BodyProducer {
        body,
        mut chunks,
        abort,
        outcome,
        bound,
        declared_length,
    } = producer;
    let mut remaining = platform::http::Remaining::new(declared_length);

    let sending = async {
        let drained: Result<(), String> = match body {
            // A stream body's chunks are produced by JS, so the event loop must run while the
            // body drains. The two are raced. If the drain finishes first, the loop has run for
            // as long as the body needed it. If the loop finishes first, nothing can produce
            // another chunk, and waiting on the drain would hang the connection. That happens
            // once a guest reaches a state in which no more async work is pending, but the body
            // `ReadableStream` hasn't been fully served. The drain then goes on with the channel's
            // existing contents, potentially including a chunk it is still handing to hyper.
            OutgoingBody::Stream(mut receiver) => {
                let loop_done = Cell::new(false);
                let mut forwarding = std::pin::pin!(forward_stream(
                    &mut receiver,
                    &mut chunks,
                    &mut remaining,
                    &loop_done
                ));
                let forwarded =
                    futures_lite::future::or(async { Some((&mut forwarding).await) }, async {
                        unsafe { run_to_completion(raw_cx, event_loop, tokio::time::sleep).await };
                        None
                    })
                    .await;
                match forwarded {
                    Some(result) => result,
                    None => {
                        loop_done.set(true);
                        forwarding.await
                    }
                }
            }
            // Every other body is read from something other than the event loop, so it drains on
            // its own.
            body => forward_body(body, &mut chunks, &mut remaining).await,
        };
        drained?;
        // Ending the channel lets hyper finish the body. The outcome reports how the send ended,
        // and arrives once hyper has taken the body to its end.
        chunks.close_channel();
        Ok::<BodySendOutcome, String>(outcome.await.unwrap_or(BodySendOutcome::ConnectionLost(
            "the connection was lost while the response body was being written".to_string(),
        )))
    };

    let failure = match with_timeout(&tokio::time::sleep, bound, sending).await {
        Some(Ok(outcome)) => return outcome,
        Some(Err(message)) => (BodySendOutcome::ConnectionLost(message.clone()), message),
        None => (
            BodySendOutcome::TimedOut,
            "the response body ran out of time".to_string(),
        ),
    };
    // A body that cannot be completed ends with an error rather than a clean channel close. Per
    // RFC 9112 §7.1 a completed message framing marks the body as complete, so a clean close
    // would present the truncation to the client as a successful response.
    let (outcome, message) = failure;
    *abort.borrow_mut() = Some(message);
    chunks.close_channel();
    outcome
}

/// Forward a handler's body chunks to hyper until the body ends, or until it has produced all of
/// `remaining`. `Err` explains why the body cannot be completed: it failed mid-stream, or hyper
/// dropped it.
async fn forward_body(
    body: OutgoingBody,
    chunks: &mut futures_channel::mpsc::Sender<Result<bytes::Bytes, BodyError>>,
    remaining: &mut platform::http::Remaining,
) -> Result<(), String> {
    match body {
        OutgoingBody::Host(mut host_body) => loop {
            match host_body.next_chunk().await {
                Ok(Some(chunk)) => {
                    if !send_chunk(chunks, chunk, remaining).await? {
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(e) => return Err(format!("response body failed mid-stream: {e}")),
            }
        },
        OutgoingBody::Stream(mut receiver) => {
            forward_stream(&mut receiver, chunks, remaining, &Cell::new(false)).await
        }
        // hyper does not report write progress: a chunk it accepted may still be in its write
        // buffer. It does stop accepting chunks while that buffer is above its watermark, so
        // handing the body over in `WIRE_CHUNK_BYTES` chunks allows us to track a bit more closely
        // how many bytes were actually sent out.
        OutgoingBody::Bytes(mut bytes) => {
            while !bytes.is_empty() {
                let chunk = bytes.split_to(bytes.len().min(WIRE_CHUNK_BYTES));
                if !send_chunk(chunks, chunk, remaining).await? {
                    return Ok(());
                }
            }
            Ok(())
        }
        OutgoingBody::Consumed => Ok(()),
    }
}

/// Forward the chunks a handler's JS produces, as its pump enqueues them, until the stream ends.
/// Once `loop_done` is set, the stream ends with an error when the channel has nothing ready.
async fn forward_stream(
    receiver: &mut platform::http::OutgoingBodyReceiver,
    chunks: &mut futures_channel::mpsc::Sender<Result<bytes::Bytes, BodyError>>,
    remaining: &mut platform::http::Remaining,
    loop_done: &Cell<bool>,
) -> Result<(), String> {
    let error = loop {
        // Once the event loop is done, an empty channel that is still open can never receive
        // another chunk.
        let next = futures_lite::future::poll_fn(|cx| match receiver.poll_next_unpin(cx) {
            Poll::Ready(next) => Poll::Ready(Some(next)),
            Poll::Pending if loop_done.get() => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        })
        .await;
        let mut chunk = match next {
            Some(Some(Ok(chunk))) => chunk,
            Some(Some(Err(e))) => break e,
            Some(None) => return Ok(()),
            None => {
                return Err(
                    "the event loop finished while the response body was still open, so \
                     the body can never complete"
                        .to_string(),
                )
            }
        };
        let error = platform::http::coalesce_queued_chunks(receiver, &mut chunk);
        if !send_chunk(chunks, bytes::Bytes::from(chunk), remaining).await? {
            return Ok(());
        }
        if let Some(e) = error {
            break e;
        }
    };
    Err(fail_after_sent_chunks(chunks, &error).await)
}

/// Wait until hyper has taken the last chunk sent to `chunks`, so it is written out before the
/// body fails with `error`, and return the failure's message.
async fn fail_after_sent_chunks(
    chunks: &mut futures_channel::mpsc::Sender<Result<bytes::Bytes, BodyError>>,
    error: &platform::http::Error,
) -> String {
    let _ = futures_lite::future::poll_fn(|cx| chunks.poll_ready(cx)).await;
    format!("response body errored mid-stream: {error}")
}

/// Hand one chunk to hyper, waiting for room, trimmed so the total sent does not exceed the
/// declared length. Empty chunks are dropped, since hyper would treat them as the end of a
/// chunked body.
///
/// `Ok(false)` means the declared length was exceeded, so the handler has produced more content
/// than it declared and nothing further will be sent.
async fn send_chunk(
    chunks: &mut futures_channel::mpsc::Sender<Result<bytes::Bytes, BodyError>>,
    chunk: bytes::Bytes,
    remaining: &mut platform::http::Remaining,
) -> Result<bool, String> {
    let Some(chunk) = remaining.take(chunk) else {
        return Ok(false);
    };
    if chunk.is_empty() {
        return Ok(true);
    }
    if futures_lite::future::poll_fn(|cx| chunks.poll_ready(cx))
        .await
        .is_err()
        || chunks.start_send(Ok(chunk)).is_err()
    {
        return Err(
            "the connection was lost while the response body was being written".to_string(),
        );
    }
    Ok(true)
}
