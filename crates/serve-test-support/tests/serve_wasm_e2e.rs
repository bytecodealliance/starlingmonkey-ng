// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end tests of wasm serve mode: the component under a real `wasi:http` host
//! (`wasmtime serve`), driven over the same sockets the native serve tests use, so the two suites
//! assert the same observables against the two transports.
//!
//! A case that fails here is a wasm-side bug unless the behavior is the host's to decide — request
//! parsing, connection management, instance lifecycle — in which case the divergence is commented
//! at the code site and the test pins whatever contract the guest does have.
//!
//! Skipped, loudly, unless `STARLING_WASM_COMPONENT` names the component to test and `wasmtime`
//! is on PATH; `just test-serve-wasm` builds the component and runs the suite.

#![cfg(not(target_arch = "wasm32"))]

use serve_test_support as common;

use common::read_until_eof;
use common::wasm_serve::{Ready, Serve};
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

/// Counts the requests one instance has served, so a test can tell whether it is still talking to
/// the instance it was talking to before. `/ready` is left out of the count, since the harness
/// polls it an unpredictable number of times.
const REQUEST_COUNTER: &str = r#"globalThis.served = 0;
addEventListener('fetch', (event) => {
    if (new URL(event.request.url).pathname === '/ready') {
        event.respondWith(new Response('ready'));
        return;
    }
    event.respondWith(new Response(String(++globalThis.served)));
});"#;

/// The premise the rest of this suite rests on, and the one the serve code is written for: a WASIp3
/// host keeps sending requests to an instance rather than retiring it, so they share a global —
/// as they do in the native server's default mode.
#[test]
fn sequential_requests_share_one_instance() {
    let Some(server) = Serve::new(18402)
        .reusing_one_instance()
        .script(REQUEST_COUNTER)
        .start()
    else {
        return;
    };
    // Each request goes out only once the instance has reported an idle event loop, so it meets
    // the instance that served the one before rather than a fresh one raised beside a busy one.
    // Nothing to wait for between these. A handler that only responds parks nowhere, so it never
    // reports an idle loop, and the retry reads back whether the host had counted the instance
    // free again in time.
    let counts = met_in_one_instance(&server, || {
        vec![
            server.get("/count"),
            server.get("/count"),
            server.get("/count"),
        ]
    });
    // The counter is the instance's own, so a retried run continues from where the one before it
    // left off. What matters is that the three requests counted in step.
    let first: u32 = counts[0].parse().expect("a count");
    assert_eq!(
        counts,
        [
            first.to_string(),
            (first + 1).to_string(),
            (first + 2).to_string()
        ]
    );
}

/// The other side of that contract: reuse is the host's choice, and a host that declines it hands
/// every request a global of its own — the property the native server offers as `--serve-isolated`
/// and the WPT serve harness asks `wasmtime serve` for.
#[test]
fn a_host_that_retires_instances_gives_each_request_a_fresh_global() {
    let Some(server) = Serve::new(18403)
        .wasmtime_flags(["--max-instance-reuse-count", "1"])
        .script(REQUEST_COUNTER)
        .start()
    else {
        return;
    };
    assert_eq!(
        [
            server.get("/count"),
            server.get("/count"),
            server.get("/count")
        ],
        ["1", "1", "1"]
    );
}

/// A `setInterval` in a shared-global script is background work the script expects to keep running
/// — a cache refresh, a metrics flush — so the content script's own event loop has to go on being
/// driven between requests, as it is on the native server
/// (`a_repeating_timer_keeps_running_between_requests`).
#[test]
fn a_repeating_timer_keeps_running_between_requests() {
    const TICKER: &str = r#"globalThis.ticks = 0;
    setInterval(() => { globalThis.ticks += 1; }, 20);
    addEventListener('fetch', (event) => {
        event.respondWith(new Response('ticks=' + globalThis.ticks));
    });"#;
    let Some(server) = Serve::new(18404)
        .reusing_one_instance()
        .script(TICKER)
        .start()
    else {
        return;
    };

    let ticks = |server: &common::wasm_serve::WasmServer| -> u32 {
        let body = server.get("/");
        body.strip_prefix("ticks=")
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("a tick count, got {body:?}"))
    };

    let first = ticks(&server);
    // Idle time with no requests at all: the interval has to fire on its own.
    std::thread::sleep(Duration::from_millis(500));
    let second = ticks(&server);
    assert!(
        second > first,
        "the interval must fire while the instance idles (went {first} -> {second})"
    );
}

/// A content script that leaves a `fetch` in flight when evaluation ends — here to an upstream that
/// never responds, so it stays in flight for the instance's life. Its future is registered on the
/// startup loop, and a loop may not be dropped while it holds one (see
/// `EventLoop::cancel_pending_futures`). The instance has to go on serving either way.
#[test]
fn an_unawaited_top_level_fetch_does_not_break_the_instance() {
    let upstream = common::start_silent_upstream();
    let handler = format!(
        "fetch('http://127.0.0.1:{upstream}/');
        addEventListener('fetch', (event) => event.respondWith(new Response('served')));"
    );
    let Some(server) = Serve::new(18405).script(&handler).start() else {
        return;
    };

    assert_eq!(server.get("/"), "served");
    assert_eq!(server.get("/"), "served");
}

/// A server that cannot stand its runtime up responds to every request, and what went wrong is for
/// operator: the client gets the same bare 500 the native path sends, and the detail — which names
/// host paths — goes to the log.
#[test]
fn a_runtime_that_will_not_start_keeps_its_reasons_off_the_wire() {
    let Some(server) = Serve::new(18406)
        .flags(["--legacy-script", "not-a-real-script.js"])
        .ready(Ready::AnyResponse)
        .start()
    else {
        return;
    };

    assert!(
        server
            .full_request("GET", "/", "")
            .starts_with("HTTP/1.1 500"),
        "{}",
        server.full_request("GET", "/", "")
    );
    assert_eq!(server.get("/"), "Internal Server Error");
    assert!(
        server.wait_for_marker("not-a-real-script.js", Duration::from_secs(5)),
        "{}",
        server.log()
    );
}

/// A runtime whose startup failed responds to every later request with a 500 without running the
/// startup again or logging the failure again: a reused instance evaluates the content script
/// once.
#[test]
fn a_failed_startup_is_not_retried_by_later_requests() {
    let Some(server) = Serve::new(18461)
        .reusing_one_instance()
        .script("console.error('startup-ran'); throw new Error('startup failed');")
        .ready(Ready::AnyResponse)
        .start()
    else {
        return;
    };

    let (first, second) = met_in_one_instance(&server, || {
        let first = server.full_request("GET", "/", "");
        (first, server.full_request("GET", "/", ""))
    });
    assert!(first.starts_with("HTTP/1.1 500"), "{first}");
    assert!(second.starts_with("HTTP/1.1 500"), "{second}");

    let log = server.log();
    let (instances, _) = server.instances_since(0);
    let distinct: std::collections::HashSet<_> = instances.iter().collect();
    assert_eq!(
        log.matches("startup-ran").count(),
        distinct.len(),
        "each instance must run the failing startup once, not once per request: {log}"
    );
    assert_eq!(
        log.matches("could not be started").count(),
        distinct.len(),
        "each instance must log its failed startup once: {log}"
    );
}

/// A `fetch` whose request has a header the host refuses rejects with a `TypeError` naming
/// that header. `http2-settings` is allowed by Fetch but refused by `wasi:http`.
#[test]
fn a_fetch_with_a_header_the_host_refuses_rejects_naming_it() {
    let upstream = common::start_upstream("UPSTREAM-BODY");
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            if (new URL(event.request.url).pathname === '/ready') {{
                event.respondWith(new Response('ready'));
                return;
            }}
            event.respondWith(
                fetch('http://127.0.0.1:{upstream}/', {{ headers: {{ 'http2-settings': 'AAMAAABk' }} }})
                    .then(() => new Response('sent'), (e) => new Response(e.name + ': ' + e.message))
            );
        }});"#
    );
    let Some(server) = Serve::new(18462).script(&handler).start() else {
        return;
    };

    let body = server.get("/");
    assert!(body.starts_with("TypeError: "), "{body}");
    assert!(body.contains("http2-settings"), "{body}");
}

/// An unusual status is the handler's to choose: it reaches the client as itself, and the host
/// must not act on it behind the handler's back — a `421` invites a client to retry elsewhere, not
/// the server to re-dispatch (`serve_delivers_an_unusual_status_without_retrying_it`). That the
/// instance dispatched exactly once is only assertable because it is reused.
#[test]
fn serve_delivers_an_unusual_status_without_retrying_it() {
    const HANDLER: &str = r#"globalThis.dispatches = 0;
    addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        if (path === '/count') {
            event.respondWith(new Response('dispatched ' + globalThis.dispatches + ' times'));
            return;
        }
        globalThis.dispatches++;
        event.respondWith(new Response('misdirected', { status: 421 }));
    });"#;
    let Some(server) = Serve::new(18407)
        .reusing_one_instance()
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let response = server.full_request("GET", "/", "");
    assert!(
        response.starts_with("HTTP/1.1 421 "),
        "a 421 must reach the client as a 421: {response}"
    );
    assert!(
        response.contains("misdirected"),
        "the body must survive too: {response}"
    );
    assert_eq!(
        server.get("/count"),
        "dispatched 1 times",
        "a 421 must not be retried behind the handler's back"
    );
}

/// A handler returning `Response.error()` — a network-error response with status 0 — must not
/// serialize the protocol-illegal status line `HTTP/1.1 0`; the status is surfaced as a 500
/// (`serve_maps_invalid_status_to_500`).
#[test]
fn serve_maps_invalid_status_to_500() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        event.respondWith(Response.error());
    });"#;
    let Some(server) = Serve::new(18408).script(HANDLER).start() else {
        return;
    };

    let response = server.full_request("GET", "/", "");
    assert!(response.starts_with("HTTP/1.1 500"), "got: {response}");
}

/// A script that registers no `fetch` listener can only ever serve 500s. Native refuses to serve
/// at all (`a_script_with_no_fetch_listener_refuses_to_serve`) and wizer refuses to snapshot it,
/// but a wasm server cannot decline the host's requests, so it reports the reason once and the
/// 500s that
/// follow each say why too.
#[test]
fn a_script_with_no_fetch_listener_reports_itself_and_answers_500s() {
    let Some(server) = Serve::new(18409)
        .script("globalThis.ready = true;")
        .ready(Ready::AnyResponse)
        .start()
    else {
        return;
    };

    let response = server.full_request("GET", "/", "");
    assert!(response.starts_with("HTTP/1.1 500"), "{response}");
    assert!(
        server.wait_for_marker("no `fetch` listener", Duration::from_secs(5)),
        "{}",
        server.log()
    );
    assert!(
        server.wait_for_marker("answering with a network error", Duration::from_secs(5)),
        "{}",
        server.log()
    );
}

/// A top-level `await` that rejects after the script registered its listener fails startup: the
/// rejection is logged once, and every request is answered with a 500.
#[test]
fn a_late_top_level_rejection_fails_startup() {
    let Some(server) = Serve::new(18474)
        .module(
            "entry.mjs",
            "addEventListener('fetch', (event) => event.respondWith(new Response('served')));
             await new Promise((resolve) => setTimeout(resolve, 10));
             throw new Error('late configuration failure');",
        )
        .ready(Ready::AnyResponse)
        .start()
    else {
        return;
    };

    for _ in 0..2 {
        let response = server.full_request("GET", "/", "");
        assert!(response.starts_with("HTTP/1.1 500"), "{response}");
    }
    assert!(
        server.wait_for_marker("late configuration failure", Duration::from_secs(5)),
        "{}",
        server.log()
    );
    // Logged when startup fails, not again for each request.
    let log = server.log();
    assert_eq!(
        log.matches("top-level `await` rejected").count(),
        1,
        "{log}"
    );
}

/// The shared-global half of reuse: what one request leaves on `globalThis` — and in a module it
/// imported — is there for the next, the exact inverse of the native server's `--serve-isolated`
/// mode (`isolated_requests_do_not_share_global_state`,
/// `isolated_requests_re_evaluate_imported_modules`). Startup happens once for all of them: the
/// module graph is evaluated on the first request and cached, so `ensure_started` has nothing left
/// to do for the rest.
#[test]
fn a_reused_instance_shares_its_global_and_starts_up_once() {
    const ENTRY: &str = r#"import { state } from './dep.mjs';
    console.error('startup-ran');
    globalThis.observed = 'no-request-yet';
    addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        if (path === '/observed') {
            event.respondWith(new Response(globalThis.observed + ' imports=' + state.count));
            return;
        }
        globalThis.observed = 'seen:' + path;
        state.count += 1;
        event.respondWith(new Response('ok'));
    });"#;
    let Some(server) = Serve::new(18410)
        .reusing_one_instance()
        .file("dep.mjs", "export const state = { count: 0 };")
        .module("entry.mjs", ENTRY)
        .start()
    else {
        return;
    };

    // The state below is the instance's own, so all four requests have to reach the same one.
    let (a, first, b, second) = met_in_one_instance(&server, || {
        let a = server.get("/a");
        let first = server.get("/observed");
        let b = server.get("/b");
        (a, first, b, server.get("/observed"))
    });
    assert_eq!([a, b], ["ok", "ok"]);
    assert_eq!(first, "seen:/a imports=1");
    assert_eq!(second, "seen:/b imports=2");

    let log = server.log();
    assert_eq!(
        log.matches("startup-ran").count(),
        1,
        "the module graph must be evaluated once, not per request: {log}"
    );
}

/// Per-request state must not survive the request. Here the first request's signal is aborted by
/// its response-body clock, and the next request has to arrive with a `FetchEvent`, a `Request` and
/// an `AbortSignal` of its own — an instance that handed on the aborted one would tell every later
/// handler its fetch was already over.
#[test]
fn a_later_request_gets_a_fresh_event_and_signal() {
    const HANDLER: &str = r#"globalThis.previous = null;
    addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        if (path === '/check') {
            const before = globalThis.previous;
            event.respondWith(new Response(
                'sameEvent=' + (event === before)
                + ' sameSignal=' + (event.request.signal === before.request.signal)
                + ' aborted=' + event.request.signal.aborted
                + ' previousAborted=' + before.request.signal.aborted));
            return;
        }
        globalThis.previous = event;
        event.request.signal.addEventListener('abort', () => console.error('first-aborted'));
        event.respondWith(new Response(new ReadableStream({
            start(c) { setInterval(() => c.enqueue(new TextEncoder().encode('tick ')), 50); },
        })));
    });"#;
    let Some(server) = Serve::new(18411)
        .reusing_one_instance()
        .flags(["--response-body-timeout", "1"])
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    read_until_eof(&mut stream, Duration::from_secs(20)).expect("the timeout should end the body");
    // The abort is raised from the drain task, after the response is already gone, so the next
    // request has to wait for it — otherwise "not aborted" would only mean "not aborted yet".
    assert!(
        server.wait_for_marker("first-aborted", Duration::from_secs(10)),
        "{}",
        server.log()
    );

    assert_eq!(
        server.get("/check"),
        "sameEvent=false sameSignal=false aborted=false previousAborted=true"
    );
}

/// Each request's timeouts are its own window, opened when it starts. A request that spends its
/// whole `--response-body-timeout` must leave the next one a full window. A clock carried over
/// would truncate every later response the moment it started.
#[test]
fn a_later_request_gets_its_own_response_body_window() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        if (path === '/endless') {
            event.respondWith(new Response(new ReadableStream({
                start(c) { setInterval(() => c.enqueue(new TextEncoder().encode('tick ')), 50); },
            })));
            return;
        }
        // Three chunks over ~900ms: comfortably inside a fresh 3s window, and nowhere near one
        // that a previous request had already spent.
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                let sent = 0;
                const timer = setInterval(() => {
                    c.enqueue(new TextEncoder().encode('chunk' + sent + ' '));
                    if (++sent === 3) {
                        clearInterval(timer);
                        c.close();
                    }
                }, 300);
            },
        })));
    });"#;
    let Some(server) = Serve::new(18412)
        .reusing_one_instance()
        .flags(["--response-body-timeout", "3"])
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    stream
        .write_all(b"GET /endless HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    read_until_eof(&mut stream, Duration::from_secs(20)).expect("the timeout should end the body");

    assert_eq!(server.get("/slow"), "chunk0 chunk1 chunk2 ");
}

/// A handler that proxies `/fetch` from an upstream slow enough to still be in flight while
/// another request's post-response work finishes, and serves `/lifetime` at once while leaving
/// two seconds of `waitUntil` work behind it.
fn overlapping_work_handler(upstream: u16) -> String {
    format!(
        r#"addEventListener('fetch', (event) => {{
        const path = new URL(event.request.url).pathname;
        if (path === '/ready') {{
            event.respondWith(new Response('ready'));
            return;
        }}
        if (path === '/lifetime') {{
            event.waitUntil(new Promise((resolve) => setTimeout(() => {{
                console.error('lifetime-done');
                resolve();
            }}, 2000)));
            event.respondWith(new Response('lifetime-started'));
            return;
        }}
        if (path === '/endless') {{
            event.request.signal.addEventListener('abort', () => console.error(
                'endless-aborted:' + event.request.signal.reason.name));
            event.respondWith(new Response(new ReadableStream({{
                start(c) {{ setInterval(() => c.enqueue(new TextEncoder().encode('tick ')), 50); }},
            }})));
            return;
        }}
        event.respondWith(fetch('http://127.0.0.1:{upstream}/')
            .then((r) => r.text())
            .then((text) => new Response('proxied:' + text)));
    }});"#
    )
}

/// A request's post-response work outlives its response and runs alongside whatever comes next. So
/// when the first request's `waitUntil` window closes and its drain releases the loop's remaining
/// futures, it must release only its own: the `loop_id` every async-promise future is attributed to
/// is what keeps the second request's in-flight `fetch` out of it.
#[test]
fn post_response_work_does_not_disturb_the_next_request() {
    let upstream = common::start_slow_upstream(Duration::from_secs(3), "upstream-body");
    let Some(server) = Serve::new(18413)
        .reusing_one_instance()
        .script(&overlapping_work_handler(upstream))
        .start()
    else {
        return;
    };

    // Both have to run in one instance for the first request's drain to reach the second.
    let (started, proxied) = met_in_one_instance(&server, || {
        // Answered at once, with two seconds of lifetime work left running behind it.
        let started = server.get("/lifetime");
        // Its own `fetch` outlasts that window, so the first request's drain ends, releasing what
        // its loop still holds, while this one is still waiting on the upstream.
        (started, server.get("/fetch"))
    });
    assert_eq!(started, "lifetime-started");
    assert_eq!(proxied, "proxied:upstream-body");
    assert!(
        server.wait_for_marker("lifetime-done", Duration::from_secs(5)),
        "{}",
        server.log()
    );
}

/// The same overlap with the harsher ending: the first request's response-body clock runs out —
/// aborting its signal from its drain task, on the realm the second request is running JS in —
/// while the second is parked mid-dispatch. Each has to see only its own outcome.
#[test]
fn a_truncated_response_body_does_not_disturb_a_concurrent_request() {
    let upstream = common::start_slow_upstream(Duration::from_millis(2500), "upstream-body");
    let Some(server) = Serve::new(18414)
        .reusing_one_instance()
        .flags(["--response-body-timeout", "1"])
        .script(&overlapping_work_handler(upstream))
        .start()
    else {
        return;
    };

    let mut endless = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    endless
        .write_all(b"GET /endless HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();

    let port = server.port();
    let proxied = std::thread::spawn(move || {
        common::full_request_within(port, "GET", "/fetch", "", Duration::from_secs(20))
    });

    read_until_eof(&mut endless, Duration::from_secs(20)).expect("the timeout should end the body");
    assert!(
        server.wait_for_marker("endless-aborted:TimeoutError", Duration::from_secs(10)),
        "{}",
        server.log()
    );

    let proxied = proxied.join().unwrap().expect("the proxied request");
    assert!(
        proxied.contains("proxied:upstream-body"),
        "the cut must leave the concurrent request alone: {proxied}"
    );
}

/// An id drawn once per instance, so a concurrency test can assert its requests really did meet in
/// one instance rather than being handed a new one each — which would make every "they did not
/// interfere" assertion vacuous.
const INSTANCE_ID: &str = "globalThis.instance = String(Math.random()).slice(2, 8);";

/// The `instance=<id>` a handler built from [`INSTANCE_ID`] reports.
fn instance_of(response: &str) -> &str {
    response
        .split("instance=")
        .nth(1)
        .unwrap_or_else(|| panic!("no instance id in {response:?}"))
        .split_whitespace()
        .next()
        .unwrap_or_default()
}

/// Fail unless every response came from the same instance — otherwise the requests never met, and
/// whatever the test asserts about them not interfering proves nothing.
fn assert_met_in_one_instance(responses: &[&str]) {
    let ids: Vec<&str> = responses.iter().map(|r| instance_of(r)).collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] == pair[1]),
        "the requests were served by different instances ({ids:?}), so they never interleaved"
    );
}

/// How long to let a request settle onto its timer before starting the next: `wasmtime serve` hands
/// a request to a running instance only while that instance is parked, and starts a fresh one
/// beside it otherwise.
const STAGGER: Duration = Duration::from_millis(250);

/// Run `attempt` until every request it made went to one instance, and return what it produced.
///
/// A serving host hands a request to an instance it counts as free at the moment the request
/// lands, and raises a fresh one beside an instance that has handed control back but has not been
/// counted yet. Nothing outside the host marks that moment, so a case whose requests have to meet
/// cannot force it. `await_admission` and the settle after it make it near-certain, and this runs
/// the case again when it did not hold. Reuse never happening at all still fails.
///
/// `attempt` must not assert: an attempt that panics cannot be run again.
fn met_in_one_instance<T>(
    server: &common::wasm_serve::WasmServer,
    mut attempt: impl FnMut() -> T,
) -> T {
    const TRIES: usize = 5;
    for over in 0..TRIES {
        let from = server.log_len();
        let produced = attempt();
        let (instances, _) = server.instances_since(from);
        if instances.windows(2).all(|pair| pair[0] == pair[1]) {
            return produced;
        }
        assert!(
            over + 1 < TRIES,
            "no try put every request in one instance (last: {instances:?})\n{}",
            server.log()
        );
    }
    unreachable!("the loop returns or asserts")
}

/// Wait until a handler has parked on `gate` and the instance has reported an idle event loop, so
/// the next request meets an instance blocked on I/O rather than one running guest code, which
/// would be given an instance of its own.
///
/// The gate arrival alone is not that moment. The request reaches the gate while the handler is
/// still running, as does anything the guest writes to its log.
fn await_admission(
    gate: &common::HeldUpstream,
    server: &common::wasm_serve::WasmServer,
    seen: &mut std::collections::HashSet<String>,
    tag: &str,
) {
    assert!(
        gate.await_arrival(IDLE_PATIENCE),
        "{tag} never reached the gate, so the next request had nothing to join\n{}",
        server.log()
    );
    server.await_new_idle(seen, IDLE_PATIENCE);
}

/// How long to wait for an instance to report itself idle.
const IDLE_PATIENCE: Duration = Duration::from_secs(60);

/// Ask `port` for `path` on a thread, so several requests can be in flight at once.
fn spawn_request(port: u16, path: &'static str) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        common::full_request_within(port, "GET", path, "", WORK_PATIENCE)
            .unwrap_or_else(|| panic!("no response to {path}"))
    })
}

/// Requests that park on a timer are handed to the instance already serving another, so two of them
/// run interleaved in one realm. Each has to come back with its own status, headers and body, and
/// with the request state — event, request, URL — it was dispatched with; a streamed response runs
/// alongside, since its pump goes on producing on its own loop while the other request dispatches.
#[test]
fn concurrent_requests_in_one_instance_keep_their_own_state() {
    let gate = common::start_held_upstream("go");
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
        const url = new URL(event.request.url);
        if (url.pathname === '/ready') {{
            event.respondWith(new Response('ready'));
            return;
        }}
        const tag = url.searchParams.get('tag');
        const report = () => 'path=' + url.pathname + ' tag=' + tag
            + ' method=' + event.request.method + ' instance=' + globalThis.instance;
        // Every request parks on the gate before doing anything else. The harness starts the next
        // one once this request has reached the gate, which is when this instance is blocked on
        // the response with nothing left on its stack. A serving host hands an instance a further
        // request only in that state.
        const admitted = fetch('http://127.0.0.1:{gate}/hold');
        if (url.pathname === '/stream') {{
            // The stream is built during dispatch rather than once `admitted` settles. A source
            // that starts in a later task leaves the request's loop with nothing outstanding, and
            // the loop then finishes while the body is still open. The timer idles until then.
            let go = false;
            admitted.then(() => {{ go = true; }});
            event.respondWith(new Response(new ReadableStream({{
                start(c) {{
                    let sent = 0;
                    const timer = setInterval(() => {{
                        if (!go) return;
                        c.enqueue(new TextEncoder().encode(sent === 0 ? report() : ' more' + sent));
                        if (++sent === 4) {{
                            clearInterval(timer);
                            c.close();
                        }}
                    }}, 300);
                }},
            }}), {{ status: 207, headers: {{ 'x-tag': tag }} }}));
            return;
        }}
        event.respondWith(admitted.then(() => new Promise((resolve) => setTimeout(
            () => resolve(new Response(report(), {{
                status: Number(url.searchParams.get('status')),
                headers: {{ 'x-tag': tag }},
            }})),
            Number(url.searchParams.get('delay'))))));
    }});"#,
        gate = gate.port()
    );
    let Some(server) = Serve::new(18415)
        .reusing_one_instance()
        // Pinned to the ordinary collector. Every request is sent only once the instance has
        // reported an idle loop, but under mode 4 the verifier wakes it again between that report
        // and the request landing, often enough that the host gives the request an instance of its
        // own.
        .without_gc_zeal()
        .script(&format!("{INSTANCE_ID}\n{handler}"))
        .start()
    else {
        return;
    };

    // Each request is started once the one before it has reached the gate, so it meets an instance
    // blocked on the gate's response with nothing left on its stack.
    let mut seen = server.idle_so_far();
    let streamed = spawn_request(server.port(), "/stream?tag=gamma");
    await_admission(&gate, &server, &mut seen, "gamma");
    let slow = spawn_request(server.port(), "/plain?tag=alpha&delay=1200&status=201");
    await_admission(&gate, &server, &mut seen, "alpha");
    let quick = spawn_request(server.port(), "/plain?tag=beta&delay=400&status=202");
    await_admission(&gate, &server, &mut seen, "beta");
    gate.release();

    let slow = slow.join().unwrap();
    let quick = quick.join().unwrap();
    let streamed = streamed.join().unwrap();

    assert!(slow.starts_with("HTTP/1.1 201 "), "{slow}");
    assert!(slow.contains("x-tag: alpha"), "{slow}");
    assert!(slow.contains("path=/plain tag=alpha method=GET"), "{slow}");

    assert!(quick.starts_with("HTTP/1.1 202 "), "{quick}");
    assert!(quick.contains("x-tag: beta"), "{quick}");
    assert!(quick.contains("path=/plain tag=beta method=GET"), "{quick}");

    assert!(streamed.starts_with("HTTP/1.1 207 "), "{streamed}");
    assert!(streamed.contains("x-tag: gamma"), "{streamed}");
    let streamed_body = common::dechunk(streamed.split_once("\r\n\r\n").unwrap().1);
    assert!(
        streamed_body.starts_with("path=/stream tag=gamma method=GET"),
        "{streamed_body}"
    );
    assert!(
        streamed_body.ends_with(" more1 more2 more3"),
        "{streamed_body}"
    );

    assert_met_in_one_instance(&[&slow, &quick, &streamed_body]);
}

/// The realm-stack worry the `RUNTIME` thread-local is written around: entering the global's realm
/// per request would mean `JSAutoRealm` scopes dropping out of order under interleaving, restoring
/// the wrong realm under whichever requests are still in flight. Requests here are deliberately
/// deeply interleaved — each running JS and allocating between staggered awaits — so a regression
/// to per-request realm entry has every chance to put the wrong realm under someone.
///
/// Held to five concurrent requests: past `--max-instance-concurrent-reuse-count`'s default of 16
/// the host would spread them over instances, and interleaving is the whole point.
///
/// Its own patience, since the GC-stress variant runs the same JS with the collector in the way.
const WORK_PATIENCE: Duration = Duration::from_secs(180);

/// Rounds of await-then-allocate each request runs, and how much each round allocates. The rounds
/// are enough that every request is still working while the others are. The allocation is enough
/// to give the GC-stress variant's collector real work to do in the middle of one request's JS.
const ROUNDS: usize = 12;
const PER_ROUND: usize = 1000;

fn interleaved_requests_all_get_their_own_answer(port: u16, zeal: Option<&str>) {
    let gate = common::start_held_upstream("go");
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
        const url = new URL(event.request.url);
        if (url.pathname === '/ready') {{
            event.respondWith(new Response('ready'));
            return;
        }}
        const n = Number(url.searchParams.get('n'));
        event.respondWith((async () => {{
            // Park on the gate before doing anything else. The harness starts the next request
            // once this one has reached it, which is when this instance is blocked on the response
            // with nothing left on its stack. A serving host hands an instance a further request
            // only in that state.
            await fetch('http://127.0.0.1:{gate}/hold');
            let allocated = 0;
            for (let round = 0; round < 12; round++) {{
                // Skewed per request and per round, so the interleaving is not in lockstep.
                await new Promise((r) => setTimeout(r, 120 + ((n * 7 + round * 13) % 60)));
                const held = [];
                for (let i = 0; i < 1000; i++) held.push({{ n, round, pad: 'x'.repeat(16) }});
                allocated += held.length;
            }}
            return new Response('n=' + n + ' allocated=' + allocated
                + ' instance=' + globalThis.instance);
        }})());
    }});"#,
        gate = gate.port()
    );
    // `zeal` is this case's collector, not the run's. Twelve rounds of allocate-then-await across
    // five requests make many event loop turns over a large live heap, and mode 4 (verify
    // pre-barriers) walks that heap at every one, measured at roughly 8 seconds per turn. Mode 2
    // and the ordinary collector both finish the whole workload in about five.
    let server = Serve::new(port)
        .reusing_one_instance()
        .without_gc_zeal()
        .script(&format!("{INSTANCE_ID}\n{handler}"));
    let Some(server) = zeal.into_iter().fold(server, Serve::gc_zeal).start() else {
        return;
    };

    let port = server.port();
    let mut pending = Vec::new();
    let mut seen = server.idle_so_far();
    for n in 0..5 {
        pending.push(std::thread::spawn(move || {
            let response = common::full_request_within(
                port,
                "GET",
                &format!("/work?n={n}"),
                "",
                WORK_PATIENCE,
            )
            .unwrap_or_else(|| panic!("no response for n={n}"));
            common::wasm_serve::message_body(&response)
        }));
        await_admission(&gate, &server, &mut seen, &n.to_string());
    }
    // Every request is parked in one instance. Released together, they run interleaved.
    gate.release();
    let answers: Vec<String> = pending
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    let allocated = ROUNDS * PER_ROUND;
    for (n, answer) in answers.iter().enumerate() {
        assert_eq!(
            *answer,
            format!(
                "n={n} allocated={allocated} instance={}",
                instance_of(&answers[0])
            )
        );
    }
    assert_met_in_one_instance(&answers.iter().map(String::as_str).collect::<Vec<_>>());

    // Still serving after all that, rather than having taken the process down with it.
    let after = common::full_request_within(port, "GET", "/work?n=9", "", WORK_PATIENCE)
        .expect("the server must still be serving");
    assert_eq!(
        common::wasm_serve::message_body(&after),
        format!(
            "n=9 allocated={allocated} instance={}",
            instance_of(&answers[0])
        )
    );
}

#[test]
fn deeply_interleaved_requests_keep_the_realm_stack_intact() {
    interleaved_requests_all_get_their_own_answer(18416, None);
}

/// The same interleaving with the collector running constantly (zeal mode 2: collect every N
/// allocations). GC landing in the middle of one request's JS while others are parked is exactly
/// where a rooting or realm mistake surfaces, and nothing else in this suite arranges it.
#[test]
fn deeply_interleaved_requests_survive_gc_stress() {
    interleaved_requests_all_get_their_own_answer(18417, Some("2,200"));
}

/// A content script that is still evaluating for a second and a half after the instance starts, so
/// the requests that arrive meanwhile have to wait it out. Its instance id is drawn before the
/// `await`, so a request served from a half-evaluated script would still name the instance it ran
/// in — what marks the script as fully evaluated is the listener, registered after.
/// A content script whose startup waits on a [`common::HeldUpstream`] rather than on a timer. The
/// upstream is handed the request the guest made reaching its top-level `await`, which pins
/// exactly when the instance got there.
fn slow_startup(gate: u16) -> String {
    format!(
        r#"globalThis.instance = String(Math.random()).slice(2, 8);
console.error('evaluating:' + globalThis.instance);
await fetch('http://127.0.0.1:{gate}/hold');
addEventListener('fetch', (event) => {{
    event.respondWith(new Response('served instance=' + globalThis.instance));
}});"#
    )
}

/// Requests that arrive during an instance's startup wait it out and dispatch once it is done.
/// None may reach a half-evaluated script: the listener is registered after the top-level `await`,
/// so one that did would get a 500 rather than the handler's body. Startup runs once
/// for the instance rather than once per request that waited on it.
///
/// Each request after the first is sent only once the instance has reported an idle event loop, so
/// all four land in it and one startup serves them all.
#[test]
fn concurrent_first_requests_all_wait_out_one_startup() {
    let gate = common::start_held_upstream("go");
    let Some(server) = Serve::new(18418)
        .reusing_one_instance()
        // Not `/ready`: a readiness probe would itself be the first request and drive the very
        // startup this test is about.
        .ready(Ready::Listening)
        .module("startup.mjs", &slow_startup(gate.port()))
        .start()
    else {
        return;
    };

    // The first request raises the instance. The rest follow one at a time, each once the instance
    // has reported an idle event loop. Taking on a request makes the instance run guest code, and a
    // request arriving then is given an instance of its own.
    let mut seen = server.idle_so_far();
    let mut pending = vec![spawn_request(server.port(), "/first")];
    assert!(
        gate.await_arrival(IDLE_PATIENCE),
        "the first request must have raised an instance and reached its top-level await\n{}",
        server.log()
    );
    for _ in 0..3 {
        // The instance that took the request before this one has parked again, so this one meets
        // it waiting rather than running.
        server.await_new_idle(&mut seen, IDLE_PATIENCE);
        pending.push(spawn_request(server.port(), "/first"));
    }
    server.await_new_idle(&mut seen, IDLE_PATIENCE);
    // All four are in, so let startup finish and they dispatch.
    gate.release();
    let answers: Vec<String> = pending
        .into_iter()
        .map(|handle| common::wasm_serve::message_body(&handle.join().unwrap()))
        .collect();

    for answer in &answers {
        assert!(
            answer.starts_with("served instance="),
            "a request reached a half-evaluated script: {answer:?}\n{}",
            server.log()
        );
    }
    let log = server.log();
    let mut instances: Vec<&str> = answers.iter().map(|answer| instance_of(answer)).collect();
    instances.sort_unstable();
    instances.dedup();
    for instance in &instances {
        assert_eq!(
            log.matches(&format!("evaluating:{instance}")).count(),
            1,
            "startup must run once for instance {instance}, not once per request: {log}"
        );
    }
    // Each request after the first met an instance that was up and idle, so it waited that
    // instance's startup out rather than raising one of its own.
    assert_eq!(
        instances.len(),
        1,
        "every request must have waited out the one startup ({instances:?}): {log}"
    );
}

/// The client driving startup going away mid-way must not strand the instance: whether the host
/// cancels that request's future — in which case `ensure_started`'s drop guard puts the startup
/// loop back for someone else to drive — or lets it run to the end, a later request has to be
/// served. Without the guard the first outcome would leave `Startup::Driving` set forever and every
/// later request spinning in its wait arm. wasmtime 46 takes the second path — the loss surfaces
/// only afterwards, as the response body write failing — so what this pins is the outcome rather
/// than the guard itself.
#[test]
fn a_client_that_leaves_mid_startup_does_not_strand_the_instance() {
    let gate = common::start_held_upstream("go");
    let Some(server) = Serve::new(18419)
        .reusing_one_instance()
        .ready(Ready::Listening)
        .module("startup.mjs", &slow_startup(gate.port()))
        .start()
    else {
        return;
    };

    {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        stream
            .write_all(b"GET /gone HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        // Dropped once the script is inside its top-level `await`, so the connection is lost while
        // this request is the one driving startup.
        assert!(
            gate.await_arrival(Duration::from_secs(60)),
            "the request must have reached the script's top-level await\n{}",
            server.log()
        );
    }
    gate.release();

    assert!(
        server.get("/after").starts_with("served instance="),
        "{}",
        server.log()
    );
}

/// A wizened instance serves many requests, so the repairs a resumed instance needs have to happen
/// exactly once, on the first of them. The main one rebases the monotonic clock's origin, which is
/// anchored to the process the snapshot was taken in. Running them again on a later request would rebase
/// the clock under a handler that had already read it, so `performance.now()` is checked for going
/// forwards across requests rather than merely being plausible on the first. The shared global
/// carries across requests here exactly as it does without a snapshot.
#[test]
fn a_wizened_instance_keeps_its_clock_and_global_across_requests() {
    const HANDLER: &str = r#"const atStartup = performance.now();
    globalThis.served = 0;
    addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        event.respondWith(new Response('n=' + (++globalThis.served)
            + ' startup=' + atStartup.toFixed(0) + ' now=' + performance.now().toFixed(0)));
    });"#;
    let Some(server) = Serve::new(18420)
        .reusing_one_instance()
        .wizen()
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let mut previous = 0.0;
    for expected in 1..=3 {
        // Spaced out so a rebased clock is unmistakable rather than within the few milliseconds
        // back-to-back requests are apart — and still far inside the pinned idle timeout.
        std::thread::sleep(Duration::from_millis(300));
        let body = server.get("/served");
        let field = |name: &str| -> f64 {
            body.split(&format!("{name}="))
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("no {name} in {body:?}"))
        };
        assert!(body.starts_with(&format!("n={expected} ")), "{body}");
        // A clock still reading from the snapshot's time origin would be hours out.
        let (startup, now) = (field("startup"), field("now"));
        assert!((0.0..60_000.0).contains(&startup), "{body}");
        assert!((0.0..60_000.0).contains(&now), "{body}");
        assert!(
            now >= previous,
            "the clock went backwards between requests, so it was rebased more than once: \
             {previous} then {body:?}"
        );
        previous = now;
    }
}

/// A header value is a byte sequence, not text: Fetch isomorphic-encodes it, so `U+00DF U+00C0
/// U+00BF` has to leave as `DF C0 BF` rather than as their UTF-8 encodings
/// (`a_response_header_value_is_sent_as_bytes_not_utf8`). The neighbouring header is there to catch
/// the *other* way this can go wrong: a header list the host refuses must not take the rest with
/// it.
#[test]
fn a_response_header_value_is_sent_as_bytes_not_utf8() {
    let handler = "addEventListener('fetch', (event) => {\n\
        if (new URL(event.request.url).pathname === '/ready') {\n\
            event.respondWith(new Response('ready'));\n\
            return;\n\
        }\n\
        event.respondWith(new Response('x', { headers: {\n\
            'x-test': '\u{df}\u{c0}\u{bf}',\n\
            'x-other': 'kept',\n\
        } }));\n\
    });";
    let Some(server) = Serve::new(18421).script(handler).start() else {
        return;
    };

    let raw = common::raw_response_within(server.port(), "GET", "/", "", Duration::from_secs(20))
        .expect("a complete response");
    let needle = b"x-test: \xdf\xc0\xbf\r\n";
    assert!(
        raw.windows(needle.len()).any(|window| window == needle),
        "the header must be the three bytes DF C0 BF, got: {:?}",
        String::from_utf8_lossy(&raw)
    );
    assert!(
        raw.windows(14).any(|window| window == b"x-other: kept\r"),
        "the other headers must survive: {:?}",
        String::from_utf8_lossy(&raw)
    );
}

/// `wasi:http` refuses the connection-management headers it writes itself, which
/// `prepare_wire_response` does not filter and a handler — or an upstream whose response is being
/// proxied — may perfectly well set. Only that header may be lost.
#[test]
fn a_response_header_the_host_refuses_does_not_take_the_others_with_it() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        event.respondWith(new Response('body', { headers: {
            'x-before': 'kept',
            'keep-alive': 'timeout=5',
            'x-after': 'kept',
        } }));
    });"#;
    let Some(server) = Serve::new(18422).script(HANDLER).start() else {
        return;
    };

    let response = server.full_request("GET", "/", "");
    assert!(response.contains("x-before: kept"), "{response}");
    assert!(response.contains("x-after: kept"), "{response}");
    assert!(
        !response.to_lowercase().contains("keep-alive"),
        "the refused header must not be on the wire: {response}"
    );
    assert_eq!(
        common::wasm_serve::message_body(&response),
        "body",
        "{response}"
    );
    assert!(
        server.wait_for_marker(
            "dropping the response header `keep-alive`",
            Duration::from_secs(5)
        ),
        "{}",
        server.log()
    );
}

/// A header carrying a byte that would split the response never gets as far as the transport:
/// `Headers` refuses it at construction (`a_response_header_value_with_a_null_byte_is_refused`).
#[test]
fn a_response_header_value_with_a_null_byte_is_refused() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        let outcome;
        try {
            new Response('x', { headers: { 'x-test': 'b\0r' } });
            outcome = 'constructed';
        } catch (e) {
            outcome = e.name;
        }
        event.respondWith(new Response(outcome));
    });"#;
    let Some(server) = Serve::new(18424).script(HANDLER).start() else {
        return;
    };

    assert_eq!(server.get("/"), "TypeError");
}

/// What the wire's headers look like from JS: a field that arrived in mixed case is found by any
/// casing, the incoming set is immutable — it describes what was received, which nothing can change
/// after the fact — and headers put on the response come back out with `set-cookie` kept as
/// separate fields while everything else combines
/// (`serve_exposes_incoming_headers_and_keeps_them_immutable`).
#[test]
fn serve_exposes_incoming_headers_and_keeps_them_immutable() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const incoming = event.request.headers;
        if (new URL(event.request.url).pathname === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        let immutable = 'mutable';
        try {
            incoming.delete('user-agent');
        } catch (e) {
            immutable = e.constructor.name;
        }
        const response = new Response('ok', {
            headers: [
                ['x-seen', incoming.get('EXAMPLE-HEADER') ?? 'missing'],
                ['x-agent', incoming.get('User-Agent') ?? 'missing'],
                ['x-immutable', immutable],
            ],
        });
        response.headers.append('set-cookie', 'A');
        response.headers.append('set-cookie', 'B');
        response.headers.append('another', 'A');
        response.headers.append('another', 'B');
        event.respondWith(response);
    });"#;
    let Some(server) = Serve::new(18423).script(HANDLER).start() else {
        return;
    };

    let response = common::raw_request_within(
        server.port(),
        b"GET / HTTP/1.1\r\nHost: x\r\neXample-hEader: Header Value\r\nUser-Agent: test-agent\r\n\
          Connection: close\r\n\r\n",
        Duration::from_secs(20),
    )
    .expect("a complete response");

    assert!(
        response.contains("x-seen: Header Value"),
        "a mixed-case field must be found by any casing: {response}"
    );
    assert!(response.contains("x-agent: test-agent"), "got: {response}");
    assert!(
        response.contains("x-immutable: TypeError"),
        "an incoming request's headers must not be mutable: {response}"
    );
    // `set-cookie` must stay one field per cookie: combining two cookies into one field changes
    // what they mean. Other repeats may go out either as separate fields or as one comma list —
    // RFC 9110 §5.2 makes those equivalent — but both values have to survive.
    assert_eq!(
        response.matches("set-cookie: ").count(),
        2,
        "each cookie needs a field of its own: {response}"
    );
    assert!(response.contains("set-cookie: A"), "got: {response}");
    assert!(response.contains("set-cookie: B"), "got: {response}");
    assert!(
        response.contains("another: A, B")
            || (response.contains("another: A") && response.contains("another: B")),
        "both values of a repeated header must reach the client: {response}"
    );
}

/// A `ReadableStream` whose `start()` neither enqueues nor closes leaves the pump's read pending in
/// JS, which no loop can ever run: the body cannot complete, and nothing — no timeout is configured
/// here — would otherwise end the request. It has to be ended as the truncation it is, without the
/// terminating chunk (`an_abandoned_response_body_does_not_hang_the_connection`), and the instance
/// has to go on serving meanwhile (`an_abandoned_response_body_does_not_wedge_an_isolated_server`,
/// which under instance reuse is a real assertion rather than a formality).
#[test]
fn an_abandoned_response_body_does_not_hang_the_request() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/abandoned') {
            event.respondWith(new Response(new ReadableStream({
                start(_c) { /* never enqueues, never closes */ },
            })));
            return;
        }
        event.respondWith(new Response('still serving'));
    });"#;
    let Some(server) = Serve::new(18425)
        .reusing_one_instance()
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let port = server.port();
    let abandoned = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let response =
            common::full_request_within(port, "GET", "/abandoned", "", Duration::from_secs(30));
        (response, started.elapsed())
    });

    // The wedged request must not take the instance's other work with it.
    std::thread::sleep(STAGGER);
    assert_eq!(server.get("/ok"), "still serving");

    let (response, elapsed) = abandoned.join().unwrap();
    let response = response.expect("an abandoned body must not hold the connection open");
    assert!(
        elapsed < Duration::from_secs(20),
        "an abandoned body must not hold the connection open (took {elapsed:?})"
    );
    // Whether the head made it out before the body aborted is the host's call — it has no bytes to
    // flush alongside it here — so what is asserted is only that nothing looks complete.
    assert!(
        !response.ends_with("0\r\n\r\n"),
        "a body that never completed must not be terminated as if it had: {response:?}"
    );

    assert_eq!(server.get("/ok"), "still serving");
}

/// A body produced entirely in `start()` leaves the loop with nothing left to run, exactly as an
/// abandoned one does — so the two have to be told apart by whether the stream was closed, which is
/// what `finish_abandoned_body` takes what the channel holds in order to see. A complete body gets
/// its terminator; an abandoned one does not.
///
/// What the abandoned half does *not* assert is that the bytes it did produce arrive: the guest
/// hands them over, but a host that has not flushed them when the body then aborts may discard them
/// — see the comment where `dispatch_request` hands the response over.
#[test]
fn an_abandoned_body_is_told_apart_from_a_complete_one() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const encoder = new TextEncoder();
        const closing = new URL(event.request.url).pathname === '/complete';
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                c.enqueue(encoder.encode('hello '));
                c.enqueue(encoder.encode('world'));
                if (closing) {
                    c.close();
                }
            },
        })));
    });"#;
    let Some(server) = Serve::new(18426)
        .reusing_one_instance()
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let complete = server.full_request("GET", "/complete", "");
    assert!(complete.starts_with("HTTP/1.1 200"), "{complete}");
    assert!(
        complete.ends_with("hello world\r\n0\r\n\r\n") || complete.ends_with("world\r\n0\r\n\r\n"),
        "a complete body must be terminated: {complete:?}"
    );

    let abandoned = server.full_request("GET", "/abandoned", "");
    assert!(
        !abandoned.ends_with("0\r\n\r\n"),
        "an abandoned body must not be terminated as if it had completed: {abandoned:?}"
    );
}

/// `--dispatch-timeout` bounds a `respondWith` that never settles — without one, the default, the
/// request would wait for as long as the handler takes
/// (`a_dispatch_timeout_answers_a_never_settling_respond_with`). Giving up is step 17.4.20's
/// "terminated", so the handler is told through its request signal, and the `waitUntil` window is
/// its own on the 500 path as on any other.
#[test]
fn a_dispatch_timeout_answers_a_never_settling_respond_with() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        event.request.signal.addEventListener('abort', () =>
            console.error('dispatch-abort:' + event.request.signal.reason.name));
        event.waitUntil(new Promise((resolve) => setTimeout(() => {
            console.error('lifetime-after-the-500');
            resolve();
        }, 300)));
        event.respondWith(new Promise(() => {}));
    });"#;
    let Some(server) = Serve::new(18427)
        .flags(["--dispatch-timeout", "1", "--waituntil-timeout", "2"])
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let started = std::time::Instant::now();
    let response = server.full_request("GET", "/never", "");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the timeout should have answered the request, not the harness"
    );
    assert!(response.starts_with("HTTP/1.1 500"), "{response}");
    assert!(response.contains("Internal Server Error"), "{response}");
    assert!(
        server.wait_for_marker("dispatch-abort:TimeoutError", Duration::from_secs(5)),
        "{}",
        server.log()
    );
    assert!(
        server.wait_for_marker("lifetime-after-the-500", Duration::from_secs(5)),
        "{}",
        server.log()
    );
}

/// `--waituntil-timeout` bounds the work a request keeps alive past its response. Work that fits
/// inside the bound finishes, and work that overruns it ends with the loop it runs on.
#[test]
fn a_waituntil_timeout_ends_only_the_work_that_overruns_it() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        const label = path.slice(1);
        event.waitUntil(new Promise((resolve) => setTimeout(() => {
            console.error('finished:' + label);
            resolve();
        }, path === '/short' ? 300 : 5000)));
        event.respondWith(new Response('started'));
    });"#;
    let Some(server) = Serve::new(18428)
        .flags(["--waituntil-timeout", "1"])
        .script(HANDLER)
        .start()
    else {
        return;
    };

    assert_eq!(server.get("/short"), "started");
    assert!(
        server.wait_for_marker("finished:short", Duration::from_secs(5)),
        "lifetime work inside the window must finish: {}",
        server.log()
    );

    assert_eq!(server.get("/long"), "started");
    assert!(
        !server.wait_for_marker("finished:long", Duration::from_secs(4)),
        "lifetime work past the window must be cut: {}",
        server.log()
    );
}

/// `--end-to-end-timeout` covers every phase, whether or not that phase's own flag was given: it
/// truncates a body still streaming (`an_end_to_end_timeout_truncates_a_streaming_body`), bounds a
/// `respondWith` that never settles (`an_end_to_end_timeout_answers_a_never_settling_dispatch`),
/// and drops a request's leftover lifetime work along with the loop carrying it
/// (`an_end_to_end_timeout_stops_leftover_lifetime_work`).
#[test]
fn an_end_to_end_timeout_bounds_every_phase() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        if (path === '/stream') {
            event.respondWith(new Response(new ReadableStream({
                start(c) { setInterval(() => c.enqueue(new TextEncoder().encode('tick ')), 50); },
            })));
            return;
        }
        if (path === '/never') {
            event.respondWith(new Promise(() => {}));
            return;
        }
        if (path === '/ticks') {
            event.respondWith(new Response(String(globalThis.ticks)));
            return;
        }
        globalThis.ticks = 0;
        setInterval(() => { globalThis.ticks += 1; }, 20);
        event.waitUntil(new Promise(() => {}));
        event.respondWith(new Response('started'));
    });"#;
    let Some(server) = Serve::new(18429)
        .reusing_one_instance()
        .flags(["--end-to-end-timeout", "1"])
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let streamed = server.full_request("GET", "/stream", "");
    assert!(
        !streamed.ends_with("0\r\n\r\n"),
        "a cut body must not carry the complete-body terminator: {streamed:?}"
    );

    // The 500 has to reach the client in full: its body is measured against the `response_body`
    // limit alone (`RequestClock::error_body_bound`), since the window that prompted it is spent.
    let never = server.full_request("GET", "/never", "");
    assert!(
        never.starts_with("HTTP/1.1 500"),
        "an abandoned dispatch answers 500: {never:?}"
    );
    assert!(
        never.contains("Internal Server Error"),
        "the answer needs its body too: {never:?}"
    );

    assert_eq!(server.get("/leftover"), "started");
    // Well past the deadline, the interval must have stopped: two samples with a gap between them
    // read the same count.
    std::thread::sleep(Duration::from_millis(2000));
    let first = server.get("/ticks");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        server.get("/ticks"),
        first,
        "the deadline must drop the request's loop, timers and all"
    );
}

/// Where the end-to-end clock opens differs from native, and deliberately: on wasm it starts after
/// `ensure_started`, since standing the instance up is the host's business and it bounds that
/// itself (`serve_wasm.rs`'s `dispatch_request`). So a content script that spends a second and a
/// half on a top-level `await` does not spend the first request's window. There is no wasm analog
/// of `an_end_to_end_timeout_bounds_isolated_startup`, and this case replaces it.
#[test]
fn a_slow_startup_stays_outside_the_first_requests_end_to_end_window() {
    const SCRIPT: &str = r#"await new Promise((resolve) => setTimeout(resolve, 1500));
    addEventListener('fetch', (event) => {
        event.respondWith(new Promise((resolve) =>
            setTimeout(() => resolve(new Response('served')), 600)));
    });"#;
    let Some(server) = Serve::new(18430)
        .ready(Ready::Listening)
        .flags(["--end-to-end-timeout", "1"])
        .module("startup.mjs", SCRIPT)
        .start()
    else {
        return;
    };

    // 1.5s of startup plus 600ms of handler, against a 1s window that only the latter is inside.
    assert_eq!(server.get("/first"), "served");
}

/// A configuration that cannot be satisfied — a phase allowed to outlast the whole request — is
/// refused. The native server refuses to start at all; a wasm one cannot decline the host's
/// requests, so it responds to each of them with the same bare 500 any unstartable runtime gets, and
/// reports why in the log.
#[test]
fn a_contradictory_timeout_config_refuses_to_serve() {
    let Some(server) = Serve::new(18431)
        .flags([
            "--end-to-end-timeout",
            "5",
            "--waituntil-timeout",
            "6",
            "--legacy-script",
            "handler.js",
        ])
        .file(
            "handler.js",
            "addEventListener('fetch', (e) => e.respondWith(new Response('never reached')));",
        )
        .ready(Ready::AnyResponse)
        .start()
    else {
        return;
    };

    assert!(
        server
            .full_request("GET", "/", "")
            .starts_with("HTTP/1.1 500"),
        "{}",
        server.full_request("GET", "/", "")
    );
    assert_eq!(server.get("/"), "Internal Server Error");
    let log = server.log();
    assert!(
        log.contains("--waituntil-timeout") && log.contains("--end-to-end-timeout"),
        "the log must name both flags: {log}"
    );
}

/// `wasmtime serve` forwards a `HEAD` to the guest and sends what the guest produced: no body, and
/// the `Content-Length` a `GET` would have had, which `prepare_wire_response` can supply only for an
/// in-memory body, since learning a streamed one's length means producing it
/// (`serve_answers_a_head_request_without_a_body`).
#[test]
fn serve_answers_a_head_request_without_a_body() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        if (new URL(event.request.url).pathname === '/stream') {
            event.respondWith(new Response(new ReadableStream({
                start(c) {
                    c.enqueue(new TextEncoder().encode('BODYBYTES'));
                    c.close();
                },
            })));
            return;
        }
        event.respondWith(new Response('BODYBYTES', { status: 201 }));
    });"#;
    let Some(server) = Serve::new(18432).script(HANDLER).start() else {
        return;
    };

    let response = server.full_request("HEAD", "/", "");
    let (head, body) = response.split_once("\r\n\r\n").expect("a complete head");
    assert!(
        head.starts_with("HTTP/1.1 201 "),
        "the head is still the handler's: {response}"
    );
    assert_eq!(
        body, "",
        "a HEAD response must carry no body, but got: {response}"
    );
    assert!(
        head.to_lowercase().contains("content-length: 9"),
        "a HEAD must declare the length a GET would have sent: {head}"
    );

    let streamed = server.full_request("HEAD", "/stream", "");
    let (head, body) = streamed.split_once("\r\n\r\n").expect("a complete head");
    assert_eq!(body, "", "got: {streamed}");
    assert!(
        !head.to_lowercase().contains("content-length"),
        "a streamed body's length is only knowable by producing it: {head}"
    );
}

/// A `204 No Content` is framed by its status: no `Content-Length`, no `Transfer-Encoding` (RFC
/// 9110 §8.6, RFC 9112 §6.2). The same holds for `304`
/// (`a_bodiless_status_has_no_framing_headers`).
#[test]
fn a_bodiless_status_has_no_framing_headers() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const status = Number(new URL(event.request.url).pathname.slice(1));
        event.respondWith(new Response(null, { status: status || 200 }));
    });"#;
    let Some(server) = Serve::new(18433).script(HANDLER).start() else {
        return;
    };

    for status in [204, 304] {
        let response = server.full_request("GET", &format!("/{status}"), "");
        let head = response
            .split_once("\r\n\r\n")
            .map_or(&*response, |(head, _)| head)
            .to_lowercase();
        assert!(
            !head.contains("content-length"),
            "a {status} must not be framed with Content-Length: {response}"
        );
        assert!(
            !head.contains("transfer-encoding"),
            "a {status} must not be framed with Transfer-Encoding: {response}"
        );
    }
}

/// A handler's own `Content-Length` never frames content already in memory: `prepare_wire_response`
/// measures the body instead
/// (`serve_frames_a_response_by_its_body_not_the_handlers_content_length`). A streamed body has no
/// length until it is produced, so there the declaration does frame it — see
/// `a_streamed_body_is_framed_by_its_declared_content_length`.
///
/// Where native then writes a `Content-Length` of its own, `wasi:http` picks the framing itself and
/// chooses chunked — the host writes the framing bytes, which is exactly the division of labour the
/// `prepare_wire_response` call in `serve_wasm.rs` describes. What the two targets share, and what
/// this asserts, is that the handler's claim is gone and the body is intact.
#[test]
fn a_fixed_length_body_is_framed_by_its_length() {
    // Native declares the length of an in-memory body rather than chunking it (`serve.rs`,
    // `serve_delivers_a_response_body`), so the guest declares it here too: the host frames by
    // what the guest hands it, and with no length that is chunked for every such response.
    let Some(server) = Serve::new(18436)
        .script("addEventListener('fetch', (e) => e.respondWith(new Response('BODYBYTES')));")
        .start()
    else {
        return;
    };

    let response = server.full_request("GET", "/", "");
    let head = response
        .split_once("\r\n\r\n")
        .expect("a complete response")
        .0
        .to_lowercase();
    assert!(head.contains("content-length: 9"), "got: {head}");
    assert!(!head.contains("transfer-encoding"), "got: {head}");
    assert_eq!(server.get("/"), "BODYBYTES");
}

#[test]
fn serve_frames_a_response_by_its_body_not_the_handlers_content_length() {
    const HANDLER: &str = r#"const cases = {
        // Longer than the body.
        '/larger': () => new Response('short', { headers: { 'content-length': '10000' } }),
        // Not a number at all.
        '/bogus': () => new Response('short', { headers: { 'content-length': 'test' } }),
        // Two of them, which combine into one comma-joined value.
        '/duplicate': () => {
            const headers = new Headers();
            headers.append('content-length', '10000');
            headers.append('content-length', '10000');
            return new Response('short', { headers });
        },
    };
    addEventListener('fetch', (event) => {
        const make = cases[new URL(event.request.url).pathname];
        event.respondWith(make ? make() : new Response('ready'));
    });"#;
    let Some(server) = Serve::new(18434).script(HANDLER).start() else {
        return;
    };

    for path in ["/larger", "/bogus", "/duplicate"] {
        let response = server.full_request("GET", path, "");
        let head = response
            .split_once("\r\n\r\n")
            .expect("a complete response")
            .0;
        assert_eq!(server.request("GET", path, ""), "short", "body for {path}");
        assert!(
            !head.contains("10000") && !head.to_lowercase().contains("content-length: test"),
            "the handler's own length must not reach the wire for {path}: {head}"
        );
    }
}

/// A streamed body's length is not knowable before it is produced, so a handler that declares one
/// frames its own response by it, on this target as on native
/// (`serve_frames_a_streamed_body_by_its_declared_content_length`). The guest writes the body here,
/// so it is the guest that holds it to the declared length: content past it is never handed to the
/// host. A declaration that is not a single `1*DIGIT` is refused and the host frames as it likes.
#[test]
fn a_streamed_body_is_framed_by_its_declared_content_length() {
    const HANDLER: &str = r#"function stream(parts) {
        return new ReadableStream({
            start(c) {
                for (const part of parts) c.enqueue(new TextEncoder().encode(part));
                c.close();
            },
        });
    }
    const cases = {
        '/exact': () => new Response(stream(['abc', 'de']), {
            headers: { 'content-length': '5' },
        }),
        '/longer': () => new Response(stream(['abcde', 'XXXXX']), {
            headers: { 'content-length': '5' },
        }),
        // Less than it declared: the guest ends the body as an abort, so the host cannot present
        // the truncation as a complete response.
        '/shorter': () => new Response(stream(['ab']), {
            headers: { 'content-length': '5' },
        }),
        '/bogus': () => new Response(stream(['abcde']), {
            headers: { 'content-length': 'test' },
        }),
    };
    addEventListener('fetch', (event) => {
        const make = cases[new URL(event.request.url).pathname];
        event.respondWith(make ? make() : new Response('ready'));
    });"#;
    let Some(server) = Serve::new(18436).script(HANDLER).start() else {
        return;
    };

    for path in ["/exact", "/longer"] {
        let response = server.full_request("GET", path, "");
        let (head, body) = response
            .split_once("\r\n\r\n")
            .expect("a complete response");
        assert!(
            head.to_lowercase().contains("content-length: 5"),
            "the declared length must frame {path}: {head}"
        );
        assert!(
            !head.to_lowercase().contains("transfer-encoding"),
            "got for {path}: {head}"
        );
        assert_eq!(body, "abcde", "body for {path}: {response}");
    }

    let short = server.full_request("GET", "/shorter", "");
    let sent = short
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");
    assert!(
        sent.len() < 5,
        "the body must stop short of the length it declared: {short:?}"
    );

    let response = server.full_request("GET", "/bogus", "");
    let head = response
        .split_once("\r\n\r\n")
        .expect("a complete response")
        .0;
    assert!(!head.contains("test"), "got: {head}");
    assert_eq!(server.get("/bogus"), "abcde");
}

/// A handler responding with a `fetch` response hands the transport an `OutgoingBody::Host`: the
/// upstream's body stream goes to the host untouched, with no guest writer in between. Two
/// concurrent proxies must each get the upstream's body, since `respondWith` resolves from a
/// `fetch` future whose reaction has to release *its own* request loop's interest
/// (`serve_proxies_via_fetch`).
#[test]
fn serve_proxies_via_fetch() {
    let upstream = common::start_upstream("UPSTREAM-BODY");
    let handler = format!(
        "addEventListener('fetch', (event) => \
             event.respondWith(fetch('http://127.0.0.1:{upstream}/')));"
    );
    let Some(server) = Serve::new(18435)
        .reusing_one_instance()
        .script(&handler)
        .start()
    else {
        return;
    };

    assert!(server.get("/").contains("UPSTREAM-BODY"));

    let a = spawn_request(server.port(), "/a");
    std::thread::sleep(STAGGER);
    let b = spawn_request(server.port(), "/b");
    assert!(a.join().unwrap().contains("UPSTREAM-BODY"));
    assert!(b.join().unwrap().contains("UPSTREAM-BODY"));
}

/// The upstream body forwarded as a stream rather than collected first: the handler builds its own
/// `Response` around `upstream.body`, and the chunks — which the upstream sends with pauses between
/// them — have to reach the client (`serve_forwards_an_upstream_body_as_a_stream`).
#[test]
fn serve_forwards_an_upstream_body_as_a_stream() {
    let upstream = common::start_chunked_upstream(&["one ", "two ", "three"]);
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            event.respondWith((async () => {{
                const upstream = await fetch('http://127.0.0.1:{upstream}/');
                return new Response(upstream.body, upstream);
            }})());
        }});"#
    );
    let Some(server) = Serve::new(18436).script(&handler).start() else {
        return;
    };

    assert_eq!(server.get("/"), "one two three");
}

/// A proxied response body is out of `--response-body-timeout`'s reach, and deliberately: the
/// handler responded with a `fetch` response, whose body is a host stream the host pumps itself, and
/// holding onto it to bound it would mean copying every chunk of every proxied body through the
/// guest (see `body_contents`). How long a transfer it is itself performing may take is the host's
/// to limit — the native path bounds every body only because it performs every write.
///
/// The one place this suite asserts a divergence *as* the behavior, so it is written to fail if
/// that changes silently in either direction.
#[test]
fn a_proxied_body_outlives_the_response_body_timeout() {
    let upstream = common::start_endless_upstream(Duration::from_millis(50));
    let handler = format!(
        "addEventListener('fetch', (event) => \
             event.respondWith(fetch('http://127.0.0.1:{upstream}/')));"
    );
    let Some(server) = Serve::new(18437)
        .flags(["--response-body-timeout", "1"])
        // Every route proxies the endless upstream, so no probe of it could ever come back.
        .ready(Ready::Listening)
        .script(&handler)
        .start()
    else {
        return;
    };

    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    // Several times over the bound, and still streaming.
    let ended = read_until_eof(&mut stream, Duration::from_secs(5));
    assert!(
        ended.is_none(),
        "the guest must not cut a body the host is pumping: {:?}",
        ended.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    );
}

/// An upstream that dies mid-body has to reach the client as the truncation it is, not as a body
/// that ended cleanly a few bytes short: the terminating chunk asserts completeness (RFC 9112
/// §7.1). The piped trailers carry that, the guest never having seen a byte of the body.
#[test]
fn a_dying_upstream_truncates_the_proxied_body() {
    let upstream = common::start_truncated_upstream(&["one ", "two "]);
    let handler = format!(
        "addEventListener('fetch', (event) => \
             event.respondWith(fetch('http://127.0.0.1:{upstream}/')));"
    );
    let Some(server) = Serve::new(18438)
        .ready(Ready::AnyResponse)
        .script(&handler)
        .start()
    else {
        return;
    };

    let response = server.full_request("GET", "/", "");
    assert!(
        response.contains("one ") && response.contains("two "),
        "what the upstream did send must arrive: {response:?}"
    );
    assert!(
        !response.ends_with("0\r\n\r\n"),
        "a truncated upstream must not be forwarded as a complete body: {response:?}"
    );
}

/// A response body that errors after sending bytes: the client gets what was produced, and then
/// the body ends *without* the terminating chunk, so a truncation cannot be mistaken for a complete
/// response (`a_response_stream_that_errors_mid_flight_leaves_the_body_unterminated`).
///
/// The error comes from a timer, and a generous one: a body cannot outlive its request's event
/// loop going idle, so the stream has to hold the loop open until it errors rather than wait for
/// the client to say when. The delay only has to outlast the pump reaching the wire, which a GC
/// zeal build makes far slower than an ordinary one.
#[test]
fn a_response_stream_that_errors_mid_flight_leaves_the_body_unterminated() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const encoder = new TextEncoder();
        // A route of its own for the readiness probe. The body below is slow to end, and a probe
        // waiting for one would not see a response inside its window.
        if (new URL(event.request.url).pathname === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                c.enqueue(encoder.encode('first-chunk-'));
                setTimeout(() => c.error(new Error('stream broke')), 1000);
            },
        })));
    });"#;
    let Some(server) = Serve::new(18440).script(HANDLER).start() else {
        return;
    };

    let errored = server.full_request("GET", "/mid-flight", "");
    let (_, body) = errored.split_once("\r\n\r\n").expect("a complete head");
    assert!(
        body.contains("first-chunk-"),
        "the bytes produced before the error must still arrive: {errored}"
    );
    assert!(
        !body.ends_with("0\r\n\r\n"),
        "an errored body must not be terminated as if it were complete: {errored}"
    );
}

/// A stream that enqueues something that is not a `Uint8Array` aborts its body before a single byte
/// is written (WPT
/// `resources/fetch-event-respond-with-response-body-with-invalid-chunk-worker.js`).
///
/// Host-owned divergence: native writes the head itself, so it sends a `200` and then closes
/// unterminated; `wasmtime serve` has not committed the head by the time the body aborts and sends
/// nothing at all. Which of the two a client sees is the host's call — see the comment where
/// `dispatch_request` hands the response over — and what both guarantee is what is asserted here:
/// the client cannot come away with a response that looks complete.
#[test]
fn a_response_stream_with_an_invalid_chunk_is_not_answered_as_a_success() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                c.enqueue('a string, not a Uint8Array');
                c.close();
            },
        })));
    });"#;
    let Some(server) = Serve::new(18445).script(HANDLER).start() else {
        return;
    };

    // The control, since the response under test is nothing at all: the same server sends a
    // normal request, so an empty reply below is the invalid chunk's doing and not a server that
    // never came up.
    assert_eq!(server.get("/ready"), "ready");

    let response = server.full_request("GET", "/invalid-chunk", "");
    assert!(
        response.is_empty() || !response.ends_with("0\r\n\r\n"),
        "a body that never produced a valid chunk must reach the client as nothing at all, or as \
         an unterminated one — never as a complete response: {response:?}"
    );
}

/// An empty chunk is a chunk like any other on this side: it neither ends the body nor corrupts the
/// framing (`serve_streams_an_empty_chunk_without_ending_the_body`). Worth pinning because
/// `spawn_body_writer`'s dropped-reader check cannot see a drop through one — an empty write leaves
/// an empty remainder either way, so what happens to it turns on everything except that
/// check.
#[test]
fn serve_streams_an_empty_chunk_without_ending_the_body() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const encoder = new TextEncoder();
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                c.enqueue(new Uint8Array(0));
                c.enqueue(encoder.encode('chunk one '));
                c.enqueue(new Uint8Array(0));
                c.enqueue(encoder.encode('chunk two'));
                c.close();
            },
        })));
    });"#;
    let Some(server) = Serve::new(18441)
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    assert_eq!(server.get("/"), "chunk one chunk two");
}

/// A streamed response's head reaches the client when it is ready, not when the first chunk is: a
/// handler that streams may take a while to produce anything, and the status and headers are what
/// the client acts on meanwhile (`a_streamed_response_sends_its_head_before_the_first_chunk`).
#[test]
fn a_streamed_response_sends_its_head_before_the_first_chunk() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const stream = new ReadableStream({
            start(c) {
                setTimeout(() => {
                    c.enqueue(new TextEncoder().encode('late'));
                    c.close();
                }, 2000);
            },
        });
        event.respondWith(new Response(stream, { headers: { 'x-marker': 'early' } }));
    });"#;
    let Some(server) = Serve::new(18442)
        // Every route takes two seconds to produce anything, so no probe of it could come back
        // inside the harness's own patience.
        .ready(Ready::Listening)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let started = std::time::Instant::now();

    // Read just enough to have seen the head, and time how long that took.
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut buf = [0u8; 256];
    let read = std::io::Read::read(&mut stream, &mut buf).unwrap();
    let elapsed = started.elapsed();
    let head = String::from_utf8_lossy(&buf[..read]).to_string();

    assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
    assert!(head.contains("x-marker: early"), "got: {head}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "the head must not wait for the first chunk (took {elapsed:?})"
    );
}

/// A body produced by a timer is served to completion: the pump runs on the request's own loop, and
/// that loop has to keep being driven for as long as the body needs it
/// (`an_interval_driven_stream_is_served_to_completion`).
#[test]
fn an_interval_driven_stream_is_served_to_completion() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        let ticks = 0;
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                const id = setInterval(() => {
                    c.enqueue(new TextEncoder().encode('tick ' + ++ticks + ' '));
                    if (ticks === 5) {
                        clearInterval(id);
                        c.close();
                    }
                }, 50);
            },
        })));
    });"#;
    let Some(server) = Serve::new(18443)
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    assert_eq!(server.get("/"), "tick 1 tick 2 tick 3 tick 4 tick 5 ");
}

/// A streamed response must not be held open by `waitUntil` work either: the body's pump runs on
/// the request's loop, so the send drives that loop — but only for as long as the body needs it, or
/// the client waits on an open connection for lifetime work it has no interest in
/// (`wait_until_does_not_delay_a_streamed_response`).
#[test]
fn wait_until_does_not_delay_a_streamed_response() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const stream = new ReadableStream({
            start(c) {
                c.enqueue(new TextEncoder().encode('streamed body'));
                c.close();
            },
        });
        event.respondWith(new Response(stream));
        event.waitUntil(new Promise((resolve) => setTimeout(resolve, 4000)));
    });"#;
    let Some(server) = Serve::new(18444)
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let started = std::time::Instant::now();
    let body = server.get("/");
    let elapsed = started.elapsed();
    assert_eq!(body, "streamed body");
    assert!(
        elapsed < Duration::from_secs(3),
        "a streamed response must not wait for waitUntil work (took {elapsed:?})"
    );
}

/// A handler reading `request.body` sees chunks as they arrive, not after the upload ends: the
/// first chunk's echo has to come back before the rest is even sent
/// (`serve_streams_an_incoming_request_body`).
#[test]
fn serve_streams_an_incoming_request_body() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/ready') {
            event.respondWith(new Response('ready'));
            return;
        }
        const reader = event.request.body.getReader();
        event.respondWith(new Response(new ReadableStream({
            async pull(controller) {
                const { done, value } = await reader.read();
                if (done) {
                    controller.close();
                    return;
                }
                controller.enqueue(value);
            },
        })));
    });"#;
    let Some(server) = Serve::new(18446).script(HANDLER).start() else {
        return;
    };

    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .write_all(
            b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        )
        .unwrap();

    /// Read from `stream` until the response body so far contains `expected`.
    fn read_until(stream: &mut TcpStream, raw: &mut Vec<u8>, expected: &str) -> String {
        for _ in 0..200 {
            let body = String::from_utf8_lossy(raw)
                .split_once("\r\n\r\n")
                .map(|(_, body)| common::dechunk(body))
                .unwrap_or_default();
            if body.contains(expected) {
                return body;
            }
            let mut buf = [0u8; 1024];
            let read = std::io::Read::read(stream, &mut buf).unwrap();
            assert_ne!(read, 0, "connection closed while waiting for {expected:?}");
            raw.extend_from_slice(&buf[..read]);
        }
        panic!("never received {expected:?}");
    }

    let mut raw = Vec::new();
    // First chunk: the echo must come back before the rest of the request is sent.
    stream.write_all(b"5\r\nfirst\r\n").unwrap();
    read_until(&mut stream, &mut raw, "first");

    // Only now send the second chunk and the terminator.
    stream.write_all(b"6\r\nsecond\r\n0\r\n\r\n").unwrap();
    let body = read_until(&mut stream, &mut raw, "second");
    assert_eq!(body, "firstsecond", "got: {body}");
}

/// Which methods reach the handler with a body. `wasi:http` gives a request with no body the same
/// empty stream as one whose body is empty, so `body_framing` goes by the method: `GET` gets
/// `null`, and a `POST` without a body gets an empty stream rather than `null`.
///
/// Host-owned divergence: native reads the framing headers the client sent, so a bodyless `POST`
/// there gets `null`.
#[test]
fn a_bodyless_post_still_reaches_the_handler_with_a_body() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const body = event.request.body;
        event.respondWith(new Response(body === null ? 'null' : 'stream'));
    });"#;
    let Some(server) = Serve::new(18460)
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    assert_eq!(server.request("GET", "/", ""), "null");
    assert_eq!(server.request("POST", "/", ""), "stream");
    assert_eq!(server.request("POST", "/", "payload"), "stream");
}

/// A chunked upload reaches the handler decoded: the host undoes the framing, and `body_framing`
/// exposes a body where there is no `Content-Length` to go by, since `wasi:http` does not pass
/// `Transfer-Encoding` on to the guest (`serve_decodes_a_chunked_request_body`).
///
/// Host-owned divergence: native also refuses `Transfer-Encoding` together with `Content-Length`
/// (RFC 9112 §6.3, a request-smuggling vector) with a 400. On wasm the host parses the request, so
/// no such ambiguity reaches the guest, as `body_framing`'s own comment records, and refusing it is
/// the host's business, not something the guest could do differently.
#[test]
fn serve_decodes_a_chunked_request_body() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        event.respondWith(event.request.text().then((body) => new Response('got:' + body)));
    });"#;
    let Some(server) = Serve::new(18447)
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    // "hello world" as two chunks: "hello " (0x6) then "world" (0x5).
    let response = common::raw_request_within(
        server.port(),
        b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
          6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n",
        Duration::from_secs(20),
    )
    .expect("a complete response");
    assert!(response.contains("got:hello world"), "got: {response}");

    // Both framings at once, with bytes after the chunked terminator that only the `Content-Length`
    // reading would take for body: the smuggled request either way is the one that gets two
    // different response out of two hops. Native refuses it outright with a 400. `wasmtime serve`
    // resolves it in favour of the chunked framing, which RFC 9112 §6.3 also allows a server to do
    // — what neither may do is let those bytes through as a body.
    let smuggle = common::raw_request_within(
        server.port(),
        b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\
          Connection: close\r\n\r\n0\r\n\r\nSMUGGLED",
        Duration::from_secs(20),
    )
    .unwrap_or_default();
    assert!(
        !smuggle.contains("SMUGGLED"),
        "the second framing's bytes must not reach the handler: {smuggle:?}"
    );
    assert!(
        smuggle.is_empty() || common::wasm_serve::message_body(&smuggle) == "got:",
        "an ambiguous request is either refused or read one way: {smuggle:?}"
    );
}

/// A client that closes mid-body must not present the truncation to the handler as a complete
/// body: the request's stream errors, so `text()` rejects (`serve_rejects_a_truncated_request_body`).
/// On wasm that abort arrives through the incoming body's trailers, which is the only channel a
/// `wasi:http` body has for saying it ended badly rather than early.
#[test]
fn serve_rejects_a_truncated_request_body() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        event.respondWith(event.request.text().then(
            (body) => new Response('complete:' + body),
            () => new Response('truncated'),
        ));
    });"#;
    let Some(server) = Serve::new(18448)
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    // Set before the half-close below, and read by hand rather than through `read_until_eof`:
    // macOS refuses a read timeout on a socket whose write half is already shut down, and
    // `read_until_eof` re-sets one each iteration to keep its patience a total.
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    // Declare ten bytes, send four, then close the sending side.
    stream
        .write_all(
            b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\nConnection: close\r\n\r\nfour",
        )
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    while let Ok(read) = std::io::Read::read(&mut stream, &mut buf) {
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..read]);
    }
    let response = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        response.contains("truncated"),
        "a truncated upload must make the handler's read reject: {response:?}"
    );
}

/// An incoming request body used directly as the response body is handed to the wire without being
/// pumped through JS — the incoming-to-outgoing shortcut, which applies because the incoming body
/// is a host body rather than bytes (`serve_pipes_an_incoming_request_body_to_the_response`).
#[test]
fn serve_pipes_an_incoming_request_body_to_the_response() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        event.respondWith(new Response(event.request.body ?? 'no body'));
    });"#;
    let Some(server) = Serve::new(18449)
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    assert_eq!(server.request("POST", "/", "hello world"), "hello world");
}

/// The same body going two ways at once: forwarded upstream as an outgoing request's body while
/// the handler reads the other half of the `tee`
/// (`serve_forwards_an_incoming_body_upstream_while_reading_it`). Both halves have to see the whole
/// upload, which is travelling from one connection to another while a second reader pulls on it.
#[test]
fn serve_forwards_an_incoming_body_upstream_while_reading_it() {
    let upstream = common::start_echo_upstream();
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            event.respondWith((async () => {{
                const [forward, inspect] = event.request.body.tee();
                const sent = fetch('http://127.0.0.1:{upstream}/', {{
                    method: 'POST',
                    body: forward,
                    duplex: 'half',
                }});
                let seen = '';
                const reader = inspect.getReader();
                const decoder = new TextDecoder();
                while (true) {{
                    const {{ done, value }} = await reader.read();
                    if (done) break;
                    seen += decoder.decode(value, {{ stream: true }});
                }}
                const echoed = await (await sent).text();
                return new Response('upstream=' + echoed + ' local=' + seen);
            }})());
        }});"#
    );
    let Some(server) = Serve::new(18450)
        .ready(Ready::AnyResponse)
        .script(&handler)
        .start()
    else {
        return;
    };

    assert_eq!(
        server.request("POST", "/", "payload bytes"),
        "upstream=payload bytes local=payload bytes"
    );
}

/// `clone()` on an incoming request tees its body, and both halves have to see all of it
/// (`serve_clones_an_incoming_request_body`).
#[test]
fn serve_clones_an_incoming_request_body() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const copy = event.request.clone();
        event.respondWith(Promise.all([event.request.text(), copy.text()])
            .then(([a, b]) => new Response(a + '|' + b)));
    });"#;
    let Some(server) = Serve::new(18451)
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    assert_eq!(
        server.request("POST", "/", "payload"),
        "payload|payload",
        "both halves of the tee must see the body"
    );
}

/// `respondWith(response)` consumes the response's body, so `bodyUsed` reads true afterwards — and
/// responding with one whose body is already gone is a network error, refused before the head is
/// committed since there is no way to report it after (`respond_with_marks_the_response_body_used`,
/// `responding_with_a_used_stream_backed_response_is_a_network_error`). Both read the flag back on
/// a later request, which the reused instance is what makes possible.
#[test]
fn respond_with_marks_the_response_body_used() {
    const HANDLER: &str = r#"globalThis.stored = null;
    addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/check') {
            event.respondWith(new Response('bodyUsed: ' + globalThis.stored.bodyUsed));
            return;
        }
        if (path === '/reuse') {
            event.respondWith(globalThis.stored);
            return;
        }
        globalThis.stored = new Response(new ReadableStream({
            start(c) {
                c.enqueue(new TextEncoder().encode('payload'));
                c.close();
            },
        }));
        event.respondWith(globalThis.stored);
    });"#;
    let Some(server) = Serve::new(18452)
        .reusing_one_instance()
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    assert_eq!(server.get("/"), "payload");
    assert_eq!(server.get("/check"), "bodyUsed: true");

    let reused = server.full_request("GET", "/reuse", "");
    assert!(
        reused.starts_with("HTTP/1.1 500 "),
        "a used body must be refused before the head is committed, but got: {reused}"
    );
}

/// `event.handled` reports how the request ended, and is the same promise every time it is read
/// (`fetch_event_handled_reports_the_outcome`). Every way of failing to produce a `Response` is a
/// network error here — there is no network to fall back to — including never responding at all,
/// which a browser resolves.
#[test]
fn fetch_event_handled_reports_the_outcome() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/check') {
            // Taken rather than read, so each case below reads its own outcome instead of
            // whichever one the case before it left behind.
            event.respondWith(new Response(globalThis.state ?? 'never settled'));
            globalThis.state = undefined;
            return;
        }
        const identical = event.handled === event.handled;
        event.handled.then(
            () => { globalThis.state = 'resolved,same=' + identical; },
            (e) => { globalThis.state = 'rejected:' + e.name + ',same=' + identical; });
        if (url.pathname === '/responds' || url.pathname === '/ready') {
            event.respondWith(new Response('ok'));
        } else if (url.pathname === '/rejects') {
            event.respondWith(Promise.reject(new Error('no response for you')));
        } else if (url.pathname === '/invalid') {
            event.respondWith(Promise.resolve('a string is not a Response'));
        }
        // /silent falls through without calling respondWith at all.
    });"#;
    let Some(server) = Serve::new(18453)
        .reusing_one_instance()
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    // `handled` settles as the request's outcome becomes final, and its reactions run while the
    // loop is drained afterwards, so ask until they have recorded something.
    let settled = |path: &str| {
        let _ = server.full_request("GET", path, "");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let state = server.get("/check");
            if state != "never settled" || std::time::Instant::now() >= deadline {
                return state;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // Each case reads back what its own request recorded, so every request here has to reach the
    // same instance.
    let outcomes = met_in_one_instance(&server, || {
        [
            settled("/responds"),
            settled("/rejects"),
            settled("/invalid"),
            settled("/silent"),
        ]
    });
    assert_eq!(outcomes[0], "resolved,same=true");
    // A respondWith whose promise rejects is a network error, and `handled` reports it as one.
    assert_eq!(outcomes[1], "rejected:NetworkError,same=true");
    // A promise that settles with something that is not a `Response` is the same outcome, reached
    // through the `respond-with error flag` rather than a rejection.
    assert_eq!(outcomes[2], "rejected:NetworkError,same=true");
    // So is never responding at all.
    assert_eq!(outcomes[3], "rejected:NetworkError,same=true");
}

/// A handler that throws instead of responding gets a 500 rather than a hung connection
/// or an empty 200, and the throw does not take the dispatch down: a listener after it still gets
/// the event (`a_throwing_handler_is_answered_with_500`). The same holds for `preventDefault`,
/// which cancels the event without stopping a later listener from responding — and, on its own,
/// leaves nothing to respond with (`prevent_default_does_not_stop_a_later_listener_responding`).
#[test]
fn a_failing_listener_does_not_stop_a_later_one_responding() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/boom') {
            throw new Error('handler blew up');
        }
        if (new URL(event.request.url).pathname === '/cancel-only') {
            event.preventDefault();
        }
    });
    addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/boom' || path === '/cancel-only') {
            return;
        }
        event.respondWith(new Response(path === '/ready' ? 'ready' : 'recovered'));
    });"#;
    let Some(server) = Serve::new(18454).script(HANDLER).start() else {
        return;
    };

    assert!(
        server
            .full_request("GET", "/boom", "")
            .starts_with("HTTP/1.1 500"),
        "a throwing listener is answered with a 500"
    );
    assert!(
        server
            .full_request("GET", "/cancel-only", "")
            .starts_with("HTTP/1.1 500 "),
        "cancelling without responding is a network error"
    );
    // Neither takes the dispatch down: a listener after them still gets the event and responds.
    assert_eq!(server.get("/ok"), "recovered");
}

/// A `waitUntil` promise that rejects is the handler's business, not the response's
/// (`a_rejected_wait_until_does_not_break_the_response`).
#[test]
fn a_rejected_wait_until_does_not_break_the_response() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        event.waitUntil(Promise.reject(new Error('waitUntil failed')));
        event.respondWith(new Response('answered anyway'));
    });"#;
    let Some(server) = Serve::new(18455)
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    assert_eq!(server.get("/"), "answered anyway");
}

/// `waitUntil` may only extend an event that is still `active`. Once the event is over there is
/// nothing left to extend, since the request has been served, so it throws rather than silently
/// doing nothing (`wait_until_throws_once_the_event_is_over`). The body left open for a moment
/// gives the late call something to run in.
#[test]
fn wait_until_throws_once_the_event_is_over() {
    const HANDLER: &str = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/check') {
            event.respondWith(new Response(globalThis.late ?? 'never ran'));
            return;
        }
        let during = 'allowed';
        try { event.waitUntil(Promise.resolve()); } catch (e) { during = e.name; }
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                c.enqueue(new TextEncoder().encode('during=' + during));
                setTimeout(() => {
                    try {
                        event.waitUntil(Promise.resolve());
                        globalThis.late = 'allowed';
                    } catch (e) {
                        globalThis.late = e.name;
                    }
                    c.close();
                }, 20);
            },
        })));
    });"#;
    let Some(server) = Serve::new(18456)
        .reusing_one_instance()
        .ready(Ready::AnyResponse)
        .script(HANDLER)
        .start()
    else {
        return;
    };

    assert_eq!(server.get("/"), "during=allowed");
    // The timer runs while the request's loop is drained after its response, so give it time.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(server.get("/check"), "InvalidStateError");
}

/// What keeps a request alive past its response is `waitUntil` and nothing else: a bare timer goes
/// with the request's loop rather than firing (`a_bare_timer_does_not_extend_the_event_lifetime`),
/// and so does a `fetch` the handler started and never awaited
/// (`an_unawaited_fetch_does_not_extend_the_event_lifetime`), even one whose upstream responds at
/// once. This is also the "empty event loop" half of what a reused instance must give each request:
/// what one leaves behind must not still be running under the next.
#[test]
fn nothing_but_wait_until_extends_the_event_lifetime() {
    // Slower to reply than the response takes to send, so its `then` running means the loop
    // outlived the request. The loop is driven while the body goes out, as a streamed body needs,
    // and an upstream that responds inside that window settles without the event's lifetime having
    // been extended.
    let upstream = common::start_slow_upstream(Duration::from_secs(1), "UPSTREAM-BODY");
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            const path = new URL(event.request.url).pathname;
            if (path === '/check') {{
                event.respondWith(new Response(globalThis.ran ?? 'never ran'));
                return;
            }}
            setTimeout(() => {{
                globalThis.ran = 'the timer ran';
                console.error('leftover-timer-ran');
            }}, 300);
            fetch('http://127.0.0.1:{upstream}/').then(() => console.error('leftover-fetch-settled'));
            event.respondWith(new Response('answered'));
        }});"#
    );
    let Some(server) = Serve::new(18457)
        .reusing_one_instance()
        .ready(Ready::AnyResponse)
        .script(&handler)
        .start()
    else {
        return;
    };

    // `/check` reads what the first request left behind, so both have to reach one instance.
    let (answered, ran) = met_in_one_instance(&server, || {
        let answered = server.get("/");
        // Well past both the timer's delay and the time the upstream takes to reply: a loop still
        // being driven would have run them by now.
        std::thread::sleep(Duration::from_millis(1500));
        (answered, server.get("/check"))
    });
    assert_eq!(answered, "answered");
    assert_eq!(ran, "never ran");
    assert!(
        !server.log().contains("leftover-timer-ran"),
        "{}",
        server.log()
    );
    assert!(
        !server.log().contains("leftover-fetch-settled"),
        "{}",
        server.log()
    );
}

/// The first request waits for the content script's top-level `await`, so a handler registered
/// after one is in place for it — including on a snapshot, where wizer captures the script
/// mid-evaluation and the resumed instance's first request drives the rest (`test-wizer.sh`'s
/// `awaited.mjs` case, extended here with the ordering the marker pins).
#[test]
fn a_wizened_instance_waits_for_the_scripts_top_level_await() {
    // The ordering is recorded in a global and reported in the response rather than through the
    // usual stderr markers: writing to stdio before the snapshot is taken leaves the resumed
    // instance with a stale stream handle, and every later write traps (see the wizer item in
    // `docs/wasm-serve-parity-todo.md`).
    const SCRIPT: &str = r#"globalThis.order = ['evaluating'];
    await new Promise((resolve) => setTimeout(resolve, 300));
    globalThis.order.push('evaluated');
    addEventListener('fetch', (event) => {
        globalThis.order.push('dispatched');
        event.respondWith(new Response(globalThis.order.join(',')));
    });"#;
    let Some(server) = Serve::new(18458)
        .wizen()
        .ready(Ready::Listening)
        .module("awaited.mjs", SCRIPT)
        .start()
    else {
        return;
    };

    assert_eq!(server.get("/first"), "evaluating,evaluated,dispatched");
}

/// A handler fetching its own server: guest-out through `wasi:http`'s client, host-in through the
/// same listener (`a_handler_can_fetch_its_own_server`).
///
/// The inner request goes through a [`common::ForwardingGate`] rather than straight back, because
/// the guest is the client here: the request leaves the moment the handler calls `fetch`, which is
/// while the handler is still running. Holding it at the gate until the instance reports an idle
/// event loop makes it arrive while that instance is blocked on the fetch.
#[test]
fn a_handler_can_fetch_its_own_server() {
    // The handler addresses itself by port rather than by `request.url`'s origin: what the guest
    // sees there is built from the `Host` header, which carries no port here.
    const PORT: u16 = 18459;
    let gate = common::start_forwarding_gate(PORT);
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            const path = new URL(event.request.url).pathname;
            if (path === '/inner') {{
                event.respondWith(new Response('inner instance=' + globalThis.instance));
                return;
            }}
            event.respondWith(fetch('http://127.0.0.1:{gate}/inner')
                .then((r) => r.text())
                .then((text) => new Response('outer saw [' + text + '] instance='
                    + globalThis.instance)));
        }});"#,
        gate = gate.port(),
    );
    // No readiness probe, so `/outer` is the request that stands the instance up and the only
    // instance there is. A probe would leave a second one idle beside it, and the inner request
    // could be handed to either.
    let Some(server) = Serve::new(PORT)
        .reusing_one_instance()
        .ready(Ready::Listening)
        .script(&format!("{INSTANCE_ID}\n{handler}"))
        .start()
    else {
        return;
    };

    let port = server.port();
    let body = met_in_one_instance(&server, || {
        let mut seen = server.idle_so_far();
        let outer = std::thread::spawn(move || {
            common::full_request_within(port, "GET", "/outer", "", WORK_PATIENCE)
                .expect("the outer request must be answered")
        });
        // The handler has issued its fetch and has nothing left to run, so the instance is blocked
        // on the response when the inner request arrives.
        assert!(
            gate.await_arrival(IDLE_PATIENCE),
            "the handler must have fetched its own server\n{}",
            server.log()
        );
        server.await_new_idle(&mut seen, IDLE_PATIENCE);
        gate.release();
        common::wasm_serve::message_body(&outer.join().unwrap())
    });
    assert!(
        body.starts_with("outer saw [inner instance="),
        "the reentrant request must be served: {body:?}"
    );
    let ids: Vec<&str> = body
        .split("instance=")
        .skip(1)
        .map(|rest| rest.split(|c: char| !c.is_ascii_digit()).next().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "both halves report an instance: {body:?}");
    assert_eq!(
        ids[0],
        ids[1],
        "the reentrant request must land in the instance parked on it: {body:?}\n{}",
        server.log()
    );
}

/// How long the two cases below wait for what the guest reports after its response is gone. The
/// work itself takes two seconds, and the rest is room for a machine running the whole suite.
const MARKER_PATIENCE: Duration = Duration::from_secs(60);

/// A handler whose response body never ends, reporting the request signal's abort and its
/// `waitUntil` work to `stderr` — both happen after the response is gone, so no response is left to
/// carry them.
const ENDLESS_STREAM: &str = r#"addEventListener('fetch', (event) => {
    const path = new URL(event.request.url).pathname;
    if (path === '/ready') {
        event.respondWith(new Response('ready'));
        return;
    }
    event.request.signal.addEventListener('abort', () => {
        console.error('abort-reason:' + (event.request.signal.reason?.name ?? 'unknown'));
    });
    event.waitUntil(new Promise((resolve) => setTimeout(() => {
        console.error('lifetime-work-finished');
        resolve();
    }, 2000)));
    event.respondWith(new Response(new ReadableStream({
        start(c) { setInterval(() => c.enqueue(new TextEncoder().encode('tick ')), 50); },
    })));
});"#;

/// The guest never performs the response write itself, so how a send ending badly reaches the
/// handler — step 17.4.20's abort, raised from the drain task off the body writer's reported
/// outcome — exists only against a real host. Here the body burns its 1s window, and the writer's
/// verdict must come back as a `TimeoutError` on the request's signal, with the `waitUntil` window
/// still its own: the 2s of lifetime work finishes inside its 4s bound, as on the native path
/// (`wait_until_gets_its_own_window_after_the_body_is_truncated`).
#[test]
fn a_spent_response_body_clock_aborts_the_request_signal() {
    let Some(server) = Serve::new(18400)
        // The lifetime work runs after the response is gone, past the point the host counts the
        // request as done. Under the default one-second idle timeout the host may drop the
        // instance while that work is still running, and the marker never arrives.
        .reusing_one_instance()
        // The bound only has to clear the 2s of lifetime work, with room for a loaded machine.
        // Under test is that truncating the body left the window intact.
        .flags(["--response-body-timeout", "1", "--waituntil-timeout", "20"])
        .script(ENDLESS_STREAM)
        .start()
    else {
        return;
    };

    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    read_until_eof(&mut stream, Duration::from_secs(20)).expect("the timeout should end the body");

    assert!(
        server.wait_for_marker("abort-reason:TimeoutError", MARKER_PATIENCE),
        "{}",
        server.log()
    );
    assert!(
        server.wait_for_marker("lifetime-work-finished", MARKER_PATIENCE),
        "{}",
        server.log()
    );
}

/// The other way a send ends badly, with no clock involved: the client walks away mid-stream, the
/// host drops the body stream's read end, and the writer's verdict must come back as an
/// `AbortError`. This is the wasm side of `a_lost_connection_aborts_the_request_signal`. Losing
/// the client leaves the `waitUntil` window intact, as truncating the body does above.
#[test]
fn a_client_hanging_up_mid_body_aborts_the_request_signal() {
    let Some(server) = Serve::new(18401)
        // The instance has to outlive the response for the same reason as above.
        .reusing_one_instance()
        // Clears the 2s of lifetime work with room for a loaded machine. See above.
        .flags(["--waituntil-timeout", "20"])
        .script(ENDLESS_STREAM)
        .start()
    else {
        return;
    };

    {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        // Enough of the body to have the writer well into the stream, then drop the connection.
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut buf = [0u8; 1024];
        std::io::Read::read(&mut stream, &mut buf).unwrap();
    }

    assert!(
        server.wait_for_marker("abort-reason:AbortError", MARKER_PATIENCE),
        "{}",
        server.log()
    );
    assert!(
        server.wait_for_marker("lifetime-work-finished", MARKER_PATIENCE),
        "{}",
        server.log()
    );
}

/// Requests interleaved in one instance each keep their `AsyncLocalStorage` store across host I/O,
/// a timer and a settled-promise `await`, with the store they `run` their handler in.
#[test]
fn concurrent_requests_keep_their_own_async_local_storage() {
    let gate = common::start_held_upstream("go");
    let handler = format!(
        r#"import {{ AsyncLocalStorage }} from 'node:async_hooks';
    {INSTANCE_ID}
    const storage = new AsyncLocalStorage();
    addEventListener('fetch', (event) => {{
        const url = new URL(event.request.url);
        if (url.pathname === '/ready') {{
            event.respondWith(new Response('ready'));
            return;
        }}
        const tag = url.searchParams.get('tag');
        event.respondWith(storage.run(tag, async () => {{
            await fetch('http://127.0.0.1:{gate}/hold');
            const delay = Number(url.searchParams.get('delay'));
            await new Promise((resolve) => setTimeout(resolve, delay));
            await null;
            return new Response('store=' + storage.getStore() + ' instance=' + globalThis.instance);
        }}));
    }});"#,
        gate = gate.port()
    );
    let Some(server) = Serve::new(18478)
        .reusing_one_instance()
        .without_gc_zeal()
        .flags(["--node-compat"])
        .module("handler.mjs", &handler)
        .start()
    else {
        return;
    };

    let mut seen = server.idle_so_far();
    let slow = spawn_request(server.port(), "/?tag=alpha&delay=600");
    await_admission(&gate, &server, &mut seen, "alpha");
    let quick = spawn_request(server.port(), "/?tag=beta&delay=100");
    await_admission(&gate, &server, &mut seen, "beta");
    gate.release();

    let slow = slow.join().unwrap();
    let quick = quick.join().unwrap();
    assert!(slow.contains("store=alpha "), "{slow}");
    assert!(quick.contains("store=beta "), "{quick}");
    assert_met_in_one_instance(&[&slow, &quick]);
}

/// With `--node-compat`, an `http.createServer` server responds to the requests a `wasi:http` host
/// sends: with the request's method, path and body, and with a response written whole or in
/// pieces.
#[test]
fn node_http_servers_respond_to_wasi_http_requests() {
    let handler = r#"import http from 'node:http';
    http.createServer((req, res) => {
        if (req.url === '/stream') {
            res.writeHead(201, { 'X-A': '1' });
            res.write('a');
            setTimeout(() => res.end('bc'), 10);
            return;
        }
        let body = '';
        req.setEncoding('utf8');
        req.on('data', (chunk) => (body += chunk));
        req.on('end', () => res.end(`${req.method} ${req.url} ${body}`));
    }).listen(8080);"#;
    let Some(server) = Serve::new(18481)
        .flags(["--node-compat"])
        .module("handler.mjs", handler)
        .start()
    else {
        return;
    };
    assert_eq!(
        server.request("POST", "/echo?q=1", "hi"),
        "POST /echo?q=1 hi"
    );
    let stream = server.full_request("GET", "/stream", "");
    assert!(stream.starts_with("HTTP/1.1 201"), "{stream}");
    assert!(stream.to_ascii_lowercase().contains("x-a: 1"), "{stream}");
    assert_eq!(server.request("GET", "/stream", ""), "abc");
}

/// With `--node-compat`, the callback and promise functions of `node:fs` and `fs.createReadStream`
/// read files while the component serves requests, where the synchronous functions are refused.
#[test]
fn node_fs_reads_files_while_serving() {
    let handler = r#"import fs from 'node:fs';
    import fsp from 'node:fs/promises';
    import http from 'node:http';
    http.createServer((req, res) => {
        const fail = (error) => res.end(`error ${error.code}: ${error.message}`);
        if (req.url === '/callback') {
            fs.stat('data.txt', (error, stats) => {
                if (error) return fail(error);
                fs.readFile('/cwd/data.txt', 'utf8', (error, text) => {
                    if (error) return fail(error);
                    res.end(`${stats.size} ${text}`);
                });
            });
        } else if (req.url === '/promise') {
            fsp.readFile('data.txt', 'utf8').then((text) => res.end(text), fail);
        } else if (req.url === '/stream') {
            fs.createReadStream('data.txt', { highWaterMark: 4 }).on('error', fail).pipe(res);
        } else if (req.url === '/sync') {
            try {
                res.end(fs.readFileSync('data.txt', 'utf8'));
            } catch (error) {
                fail(error);
            }
        } else {
            res.end('ready');
        }
    }).listen(8080);"#;
    let Some(server) = Serve::new(18482)
        .flags(["--node-compat"])
        .module("handler.mjs", handler)
        .file("data.txt", "file contents")
        .start()
    else {
        return;
    };
    assert_eq!(server.request("GET", "/callback", ""), "13 file contents");
    assert_eq!(server.request("GET", "/promise", ""), "file contents");
    assert_eq!(server.request("GET", "/stream", ""), "file contents");
    let refused = server.request("GET", "/sync", "");
    assert!(
        refused.starts_with("error ERR_BLOCKING_NOT_ALLOWED"),
        "{refused}"
    );
}
