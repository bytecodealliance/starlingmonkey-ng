// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The server-side dispatch path: create a `FetchEvent` for a request, run a JS
//! `fetch` handler against it, drive a **per-request** event loop, and read the
//! response the handler produced with `respondWith`.

#![cfg(not(target_arch = "wasm32"))]

use core_runtime::event_loop::{run_to_completion, with_event_loop, EventLoop};
use core_runtime::runtime::{clear_global_initializers, Runtime};
use fetch_event::fetch_event::FetchEvent;
use js::conversion::FromJSVal;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use web_fetch::request::Request;
use web_globals::events::algorithms::ScriptStackState;

/// Evaluate `src` and return its value.
fn eval<'s>(scope: &'s Scope, src: &str) -> HandleValue<'s> {
    match js::compile::evaluate_with_filename(scope, src, "<dispatch-test>", 1) {
        Ok(v) => v,
        Err(_) => panic!(
            "eval error for input: {src}: {}",
            js::error::ExnThrown::capture(scope)
        ),
    }
}

/// A runtime with the builtins registered, ready for a global to be created from it. Every test
/// here starts this way, and each has to clear the initializers first: they accumulate in a
/// process-wide registry, so a test that inherited the previous test's initializers would register
/// each builtin twice over.
///
/// The caller enters the global itself and calls [`install_globals`] on it — the scope borrows the
/// runtime, so the two cannot be returned together.
fn test_runtime() -> std::rc::Rc<Runtime> {
    clear_global_initializers();
    libstarling::register_builtins();
    Runtime::init(&core_runtime::config::RuntimeConfig::default()).expect("runtime init")
}

/// Register the builtins on `scope`'s global, plus the `mark` helper these tests respond with.
///
/// `respondWith` takes a WebIDL `Promise<Response>`, so a bare string is rejected rather than
/// captured. Tests that only need to tell one response from another — which is most of them, since
/// what they exercise is the dispatch plumbing — label a `Response` with `mark('…')` and read the
/// label back with [`response_marker`]. It rides on `statusText` because that reads back
/// synchronously; a body would have to be awaited.
fn install_globals(scope: &Scope) {
    fetch_event::add_to_global(scope, scope.global());
    eval(
        scope,
        "globalThis.mark = (text) => new Response(null, { statusText: text });",
    );
}

/// The label of an event's `potential response`, or `""` if it has none — because the handler
/// never responded, or because what it responded with failed the `Response` check.
fn response_marker(scope: &Scope, event: &FetchEvent) -> String {
    event
        .potential_response(scope)
        .map(|response| response.status_text().to_string())
        .unwrap_or_default()
}

/// Build a current-thread tokio runtime (sockets/timers enabled) for driving loops.
fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Create a `FetchEvent` for `request_src` and run `handler_src` against it with `el` active, so a
/// respondWith/waitUntil reaction (which releases the loop's interest) attaches to `el`. Returns
/// the event; the caller drives `el` to completion, then reads the response.
fn prepare_dispatch<'s>(
    scope: &'s Scope,
    el: &EventLoop,
    handler_src: &str,
    request_src: &str,
) -> FetchEvent<'s> {
    {
        with_event_loop(el, |_| {
            let request = eval(scope, request_src);
            let handler = eval(scope, handler_src);
            let request = Request::from_jsval(scope, request, ()).unwrap();
            let event = FetchEvent::create_for_request(scope, request).expect("create event");
            // Calling the handler directly stands in for dispatch; set the dispatch flag
            // around it like `FetchEvent::dispatch` does, so respondWith's step-2 guard passes.
            event.start_dispatching();
            js::Function::call(scope, HandleValue::undefined(), handler, &[event])
                .expect("handler call");
            // "Clean up after running script": the microtask checkpoint a real dispatch performs
            // after each listener returns, while the event is still dispatching. These tests stand
            // in for a transport, which dispatches with an empty JavaScript execution context
            // stack (web-globals `events::algorithms::ScriptStack`); without it this helper would
            // let a `respondWith` from a handler-queued microtask see a cleared dispatch flag,
            // which the serve path does not.
            js::jobs::run_jobs(scope);
            event.stop_dispatching();
            // Drain microtasks with the loop active so a respondWith/waitUntil reaction that
            // releases the loop's interest runs against this loop (a bare run_jobs with no active
            // loop would no-op the release and the loop would never reach idle).
            js::jobs::run_jobs(scope);
            event
        })
    }
}

/// Dispatch one request on a fresh event loop and return `(potential response,
/// globalThis.__probe)` — the probe (a string, "" if unset) lets a test observe work that ran while
/// the loop stayed alive.
fn dispatch(handler_src: &str, request_src: &str) -> (String, String) {
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);
    let rawcx = unsafe { scope.cx_mut().raw_cx() };

    let el = EventLoop::new();
    let event = prepare_dispatch(&scope, &el, handler_src, request_src);

    tokio_rt()
        .block_on(async { unsafe { run_to_completion(rawcx, &el, tokio::time::sleep).await } });

    let response = response_marker(&scope, &event);
    let probe = String::from_jsval(
        &scope,
        eval(
            &scope,
            "globalThis.__probe === undefined ? '' : String(globalThis.__probe)",
        ),
        (),
    )
    .unwrap();
    (response, probe)
}

#[test]
fn responding_with_something_that_is_not_a_response_is_a_network_error() {
    // `respondWith` takes a WebIDL `Promise<Response>`, so a value of the wrong type is accepted at
    // the call and rejects the promise once it settles. That reaches step 9, which sets the same
    // `respond-with error flag` step 10.1 describes — so a wrong type and an author's own rejection
    // are one outcome, and the request gets a network error either way.
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);
    let rawcx = unsafe { scope.cx_mut().raw_cx() };

    for handler in [
        "(e) => e.respondWith('a string is not a Response')",
        "(e) => e.respondWith(new Promise(r => setTimeout(() => r({}), 2)))",
        "(e) => e.respondWith(Promise.reject(new RangeError('the author rejected')))",
    ] {
        let el = EventLoop::new();
        let event = prepare_dispatch(&scope, &el, handler, "new Request('http://example.com/')");
        tokio_rt()
            .block_on(async { unsafe { run_to_completion(rawcx, &el, tokio::time::sleep).await } });

        assert_eq!(response_marker(&scope, &event), "", "{handler}");
        assert!(event.respond_with_error_set(), "{handler}");
    }
}

#[test]
fn describing_a_rejection_runs_author_code_only_after_the_respond_state_is_settled() {
    // The rejection warning stringifies the reason, which calls the author's `toString`. That is
    // author code running inside the reaction, so it must not see a half-settled respond state:
    // by the time it runs, the `respond-with error flag` is set, the lifetime promise is accounted
    // for and the interest released. A second `respondWith` throwing is what shows that — the
    // `respond-with entered flag` is still set and the response is final.
    //
    // The event is nonetheless still *active*: the reaction runs in the microtask checkpoint after
    // the listener returns, which is inside the dispatch, so the `dispatch flag` has not been
    // cleared yet and `waitUntil` legitimately succeeds (WPT
    // `extendable-event-async-waituntil.js`, `no-current-extension-different-microtask`).
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"
        globalThis.log = [];
        addEventListener('fetch', (e) => {
            e.respondWith(Promise.reject({
                toString() {
                    try { e.waitUntil(Promise.resolve()); log.push('extended'); }
                    catch (err) { log.push('waitUntil:' + err.name); }
                    try { e.respondWith(mark('again')); log.push('responded again'); }
                    catch (err) { log.push('respondWith:' + err.name); }
                    return 'a reason with a toString';
                },
            }));
        });
        "#,
    );

    let event = dispatch_once(&scope, "new Request('http://example.com/')");
    assert!(event.respond_with_error_set());
    // The failed response stands: nothing the reaction did replaced it.
    assert_eq!(response_marker(&scope, &event), "");

    let log = eval(&scope, "log.join(',')");
    let log = String::from_jsval(&scope, log, ()).unwrap();
    assert_eq!(log, "extended,respondWith:InvalidStateError");
}

#[test]
fn concurrent_requests_use_isolated_event_loops() {
    // Two requests, each dispatched on its own event loop, driven concurrently. Each handler
    // responds via its own timer; correct per-request isolation means each request gets exactly its
    // own response — one loop's timer/lifetime does not bleed into the other.
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);
    let rawcx = unsafe { scope.cx_mut().raw_cx() };

    let el_a = EventLoop::new();
    let el_b = EventLoop::new();
    let event_a = prepare_dispatch(
        &scope,
        &el_a,
        "(e) => e.respondWith(new Promise(r => setTimeout(() => r(mark('A:' + e.request.url)), 10)))",
        "new Request('http://example.com/a')",
    );
    let event_b = prepare_dispatch(
        &scope,
        &el_b,
        "(e) => e.respondWith(new Promise(r => setTimeout(() => r(mark('B:' + e.request.url)), 4)))",
        "new Request('http://example.com/b')",
    );

    tokio_rt().block_on(async {
        let a = unsafe { run_to_completion(rawcx, &el_a, tokio::time::sleep) };
        let b = unsafe { run_to_completion(rawcx, &el_b, tokio::time::sleep) };
        // Drive both per-request loops concurrently to completion.
        futures_lite::future::zip(a, b).await;
    });

    let resp_a = response_marker(&scope, &event_a);
    let resp_b = response_marker(&scope, &event_b);
    assert_eq!(resp_a, "A:http://example.com/a");
    assert_eq!(resp_b, "B:http://example.com/b");
}

#[test]
fn the_promise_attributes_return_the_same_object_every_access() {
    // `preloadResponse` and `handled` are WebIDL promise attributes, which return the value they
    // were initialized to — the *same* object each time. Both are built once, when the event is,
    // and handed back on every read; wrapping per access instead would make each of these false.
    // Note: we don't implement `preloadResponse`, but if we ever do, this test will stay correct.
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"
        globalThis.log = [];
        const same = (label, event) => log.push(
            `${label}:${event.handled === event.handled} ` +
            `${event.preloadResponse === event.preloadResponse}`);

        // The constructor takes a scope to build the promises a member left out needs. That is a
        // Rust-side parameter, and must not show up in the constructor's declared arity.
        log.push(`length:${FetchEvent.length}`);

        // A script-constructed event: `FetchEventInit` declares neither promise member, so both
        // attributes come from the constructor, once.
        same('constructed', new FetchEvent('fetch', { request: new Request('http://example.com/') }));

        // And on a dispatched event, whose attributes the runtime initializes.
        addEventListener('fetch', (e) => { same('dispatched', e); e.respondWith(mark('ok')); });
        "#,
    );
    dispatch_once(&scope, "new Request('http://example.com/')");

    let log = eval(&scope, "log.join(',')");
    let log = String::from_jsval(&scope, log, ()).unwrap();
    assert_eq!(log, "length:2,constructed:true true,dispatched:true true");
}

#[test]
fn the_dispatched_event_is_trusted_and_cancelable() {
    // `Create Fetch Event and Dispatch` creates the event with `create an event` (step 17.4.1),
    // which makes it trusted, and initializes `cancelable` to true (step 17.4.5). Both are
    // script-visible, and `isTrusted` additionally gates respondWith/waitUntil.
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"addEventListener('fetch', (e) => {
             globalThis.flags = `${e.isTrusted} ${e.cancelable} ${e.defaultPrevented}`;
             e.preventDefault();
             globalThis.flags += ` ${e.defaultPrevented}`;
             e.respondWith(mark('responded'));
           });"#,
    );

    let event = dispatch_once(&scope, "new Request('http://example.com/')");
    let flags = String::from_jsval(&scope, eval(&scope, "globalThis.flags"), ()).unwrap();
    assert_eq!(flags, "true true false true");
    // `preventDefault` is what the canceled flag is for: with a cancelable event it takes effect,
    // and `Create Fetch Event and Dispatch` step 17.4.19 reads it back.
    assert!(event.is_canceled());
    assert!(event.respond_with_entered());
}

#[test]
fn a_script_constructed_event_cannot_extend_its_lifetime() {
    // `add lifetime promise` step 1: an event whose `isTrusted` is false cannot be extended. A
    // script can construct a `FetchEvent` and dispatch it at the global — `dispatchEvent` makes it
    // untrusted — but its listeners must not be able to respond to it or hold the worker open.
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"const outcome = (fn) => { try { fn(); return 'ok'; } catch (e) { return e.name; } };
           addEventListener('fetch', (e) => {
             globalThis.results = [
               e.isTrusted,
               outcome(() => e.waitUntil(Promise.resolve())),
               outcome(() => e.respondWith(mark('nope'))),
             ].join(' ');
           });
           dispatchEvent(new FetchEvent('fetch', { request: new Request('http://example.com/') }));"#,
    );

    let results = String::from_jsval(&scope, eval(&scope, "globalThis.results"), ()).unwrap();
    assert_eq!(results, "false InvalidStateError InvalidStateError");
}

/// Dispatch `request_src` against the already-initialized `scope` on a fresh event loop and return
/// the event after driving the loop to completion.
fn dispatch_once<'s>(scope: &'s Scope, request_src: &str) -> FetchEvent<'s> {
    let el = EventLoop::new();
    let event = {
        with_event_loop(&el, |_| {
            let request = eval(scope, request_src);
            let request = Request::from_jsval(scope, request, ()).unwrap();
            let event =
                FetchEvent::dispatch(scope, request, ScriptStackState::Empty).expect("dispatch");
            event
        })
    };
    let rawcx = unsafe { scope.cx_mut().raw_cx() };
    tokio_rt()
        .block_on(async { unsafe { run_to_completion(rawcx, &el, tokio::time::sleep).await } });
    event
}

#[test]
fn listener_delivery_survives_a_throwing_listener() {
    // Inner-invoke semantics: a throwing listener is reported, the remaining listeners still run
    // (so the request still gets its response), a `once` listener fires exactly once across
    // dispatches, and `this` is the global.
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"
        globalThis.log = [];
        addEventListener('fetch', () => { throw new Error('listener boom'); });
        addEventListener('fetch', function () { log.push('once this===global: ' + (this === globalThis)); }, { once: true });
        addEventListener('fetch', (e) => e.respondWith(mark('resp:' + e.request.url)));
        "#,
    );

    let event = dispatch_once(&scope, "new Request('http://example.com/one')");
    let response = response_marker(&scope, &event);
    assert_eq!(response, "resp:http://example.com/one");

    // Second dispatch: the throwing listener throws again (reported, not fatal), the once
    // listener is gone, the responder still answers.
    let event = dispatch_once(&scope, "new Request('http://example.com/two')");
    let response = response_marker(&scope, &event);
    assert_eq!(response, "resp:http://example.com/two");

    let log = eval(&scope, "log.join(',')");
    let log = String::from_jsval(&scope, log, ()).unwrap();
    assert_eq!(log, "once this===global: true");
}

#[test]
fn respond_with_state_machine_guards() {
    // Steps 2–3 of respondWith: a second call throws InvalidStateError, and a call
    // outside dispatch (from a timer, after dispatch returned) throws too.
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"
        globalThis.log = [];
        addEventListener('fetch', (e) => {
            e.respondWith(mark('first'));
            try { e.respondWith(mark('second')); log.push('second-ok'); }
            catch (err) { log.push('second:' + err.name); }
            setTimeout(() => {
                const late = () => {
                    try { e.respondWith(mark('late')); log.push('late-ok'); }
                    catch (err) { log.push('late:' + err.name); }
                };
                late();
            }, 1);
        });
        "#,
    );

    let event = dispatch_once(&scope, "new Request('http://example.com/req')");
    let response = response_marker(&scope, &event);
    assert_eq!(response, "first");

    let log = eval(&scope, "log.join(',')");
    let log = String::from_jsval(&scope, log, ()).unwrap();
    assert_eq!(log, "second:InvalidStateError,late:InvalidStateError");
}

#[test]
fn the_promise_argument_is_converted_before_the_guards_run() {
    // WebIDL converts an operation's arguments before running any of its steps, and converting to
    // `Promise<T>` resolves a fresh promise with the value — which reads a thenable's `then`. So a
    // `respondWith` that goes on to throw `InvalidStateError` has already touched its argument.
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"
        globalThis.log = [];
        const thenable = { get then() { log.push('then-read'); return undefined; } };
        addEventListener('fetch', (e) => {
            e.respondWith(mark('first'));
            try { e.respondWith(thenable); } catch (err) { log.push('threw:' + err.name); }
        });
        "#,
    );

    let event = dispatch_once(&scope, "new Request('http://example.com/req')");
    let response = response_marker(&scope, &event);
    assert_eq!(response, "first");

    let log = eval(&scope, "log.join(',')");
    let log = String::from_jsval(&scope, log, ()).unwrap();
    assert_eq!(log, "then-read,threw:InvalidStateError");
}

#[test]
fn lifetime_interest_releases_on_the_acquiring_loop() {
    // Request A's respondWith promise is resolved from request B's timer: the settle
    // reaction runs during B's turn (all requests share one realm and one microtask
    // queue), and must release A's loop interest — not the active loop B's, which
    // would underflow B's counter (a panic) and leave A's interest held (a hang).
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);
    let rawcx = unsafe { scope.cx_mut().raw_cx() };

    let el_a = EventLoop::new();
    let el_b = EventLoop::new();
    let event_a = prepare_dispatch(
        &scope,
        &el_a,
        "(e) => e.respondWith(new Promise(r => { globalThis.__resolveA = r; }))",
        "new Request('http://example.com/a')",
    );
    let event_b = prepare_dispatch(
        &scope,
        &el_b,
        "(e) => e.respondWith(new Promise(r => setTimeout(() => { globalThis.__resolveA(mark('A:done')); r(mark('B:done')); }, 5)))",
        "new Request('http://example.com/b')",
    );

    tokio_rt().block_on(async {
        let a = unsafe { run_to_completion(rawcx, &el_a, tokio::time::sleep) };
        let b = unsafe { run_to_completion(rawcx, &el_b, tokio::time::sleep) };
        futures_lite::future::zip(a, b).await;
    });

    let resp_a = response_marker(&scope, &event_a);
    let resp_b = response_marker(&scope, &event_b);
    assert_eq!(resp_a, "A:done");
    assert_eq!(resp_b, "B:done");
}

/// `respondWith` stops propagation on its own: step 5 sets the `stop propagation` and `stop
/// immediate propagation` flags, so a later `fetch` listener never runs. WPT
/// `resources/fetch-event-respond-with-stops-propagation-worker.js` asserts this.
#[test]
fn respond_with_alone_stops_listener_delivery() {
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"
        addEventListener('fetch', (e) => { e.respondWith(mark('first')); });
        addEventListener('fetch', () => { globalThis.secondRan = true; });
        "#,
    );

    let event = dispatch_once(&scope, "new Request('http://example.com/req')");
    assert_eq!(response_marker(&scope, &event), "first");

    let second = eval(&scope, "typeof globalThis.secondRan");
    let second = String::from_jsval(&scope, second, ()).unwrap();
    assert_eq!(
        second, "undefined",
        "respondWith must stop propagation without an explicit stopImmediatePropagation call"
    );
}

/// The script-facing constructor surface, as WPT's
/// `resources/interface-requirements-worker.sub.js` pins it. None of this is reachable through the
/// serve path — the runtime builds its events with `create_for_request` — but it is the shape a
/// WebIDL change can alter without any dispatch test noticing.
#[test]
fn the_constructor_surface_matches_the_idl() {
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    let results = eval(
        &scope,
        r#"(() => {
            const outcome = (fn) => { try { return String(fn()); } catch (e) { return e.name; } };
            const request = new Request('http://example.com/x');
            return [
                // `request` is a required member, so all three of these are TypeErrors.
                outcome(() => new FetchEvent('t')),
                outcome(() => new FetchEvent('t', {})),
                outcome(() => new FetchEvent('t', { request: null })),
                // ExtendableEvent takes a bare type.
                outcome(() => new ExtendableEvent('E').type),
                // Dictionary defaults: an EventInit without either flag leaves both flags false.
                outcome(() => new FetchEvent('t', { request }).bubbles),
                outcome(() => new FetchEvent('t', { request }).cancelable),
                outcome(() => new FetchEvent('t', { request, cancelable: true }).cancelable),
                // clientId defaults to the empty string and round-trips.
                outcome(() => JSON.stringify(new FetchEvent('t', { request }).clientId)),
                outcome(() => new FetchEvent('t', { request, clientId: 'cid' }).clientId),
                // The request the event was built with is the one it hands back.
                outcome(() => new FetchEvent('t', { request }).request.url),
                // Interface members that must NOT exist (WPT `historical.https.any.js` and the
                // same interface-requirements worker).
                outcome(() => 'targetClientId' in FetchEvent.prototype),
                outcome(() => 'isReload' in FetchEvent.prototype),
                // Browser-only globals a worker scope must not expose.
                outcome(() => typeof XMLHttpRequest),
                outcome(() => typeof URL.createObjectURL),
            ].join('|');
        })()"#,
    );
    let results = String::from_jsval(&scope, results, ()).unwrap();

    assert_eq!(
        results,
        [
            "TypeError",
            "TypeError",
            "TypeError",
            "E",
            "false",
            "false",
            "true",
            "\"\"",
            "cid",
            "http://example.com/x",
            "false",
            "false",
            "undefined",
            "undefined",
        ]
        .join("|")
    );
}

/// A microtask queued by the handler is still inside the dispatch, so `respondWith` from one has
/// to work: WPT `fetch-event-async-respond-with.https.html`, against the task case that must throw
/// (`respond_with_state_machine_guards`).
///
/// Goes through `FetchEvent::dispatch` rather than the handler-called-directly shortcut, since the
/// checkpoint lives in the dispatch itself.
#[test]
fn respond_with_from_a_microtask_is_still_within_dispatch() {
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"addEventListener('fetch', (e) => {
            Promise.resolve().then(() => {
                try {
                    e.respondWith(mark('from-a-microtask'));
                    globalThis.__probe = 'no throw';
                } catch (err) {
                    globalThis.__probe = err.name;
                }
            });
        });"#,
    );

    let event = dispatch_once(&scope, "new Request('http://example.com/req')");

    let probe = String::from_jsval(&scope, eval(&scope, "String(globalThis.__probe)"), ()).unwrap();
    assert_eq!(
        probe, "no throw",
        "respondWith must not throw in a microtask"
    );
    assert_eq!(response_marker(&scope, &event), "from-a-microtask");
}

/// The `waitUntil` half, on the same checkpoint. WPT
/// `resources/extendable-event-async-waituntil.js`, `no-current-extension-different-microtask`.
///
/// No `respondWith` here on purpose: an outstanding one would keep the pending-promise count above
/// zero, so the event would be `active` without the dispatch flag ever being consulted — the case
/// the test below covers.
#[test]
fn wait_until_from_a_microtask_is_still_within_dispatch() {
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"addEventListener('fetch', (e) => {
            Promise.resolve().then(() => {
                try {
                    e.waitUntil(Promise.resolve());
                    globalThis.__probe = 'no throw';
                } catch (err) {
                    globalThis.__probe = err.name;
                }
            });
        });"#,
    );

    dispatch_once(&scope, "new Request('http://example.com/req')");

    let probe = String::from_jsval(&scope, eval(&scope, "String(globalThis.__probe)"), ()).unwrap();
    assert_eq!(probe, "no throw", "waitUntil must not throw in a microtask");
}

/// A `waitUntil` from a later *task* throws, where one from a microtask does not: by then the
/// dispatch flag is cleared and every lifetime promise has settled, so the event is no longer
/// `active` — `add lifetime promise` step 2, and the spec note that says exactly this.
#[test]
fn wait_until_from_a_later_task_throws() {
    let (_, probe) = dispatch(
        r#"(e) => {
            e.respondWith(mark('answered'));
            setTimeout(() => {
                try {
                    e.waitUntil(Promise.resolve());
                    globalThis.__probe = 'no throw';
                } catch (err) {
                    globalThis.__probe = err.name;
                }
            }, 1);
        }"#,
        "new Request('http://example.com/req')",
    );

    assert_eq!(probe, "InvalidStateError");
}

/// Chaining `waitUntil` off a lifetime promise that is itself settling keeps the event alive: the
/// author's reaction runs before the decrement that would end the extension, so the count never
/// reaches zero in between. WPT `resources/extendable-event-async-waituntil.js`,
/// `during-event-handler-and-microtask`.
#[test]
fn wait_until_can_be_chained_off_a_settling_lifetime_promise() {
    let (response, probe) = dispatch(
        r#"(e) => {
            e.respondWith(mark('answered'));
            const p = new Promise((resolve) => setTimeout(resolve, 1));
            e.waitUntil(p);
            p.then(() => {
                try {
                    e.waitUntil(Promise.resolve());
                    globalThis.__probe = 'no throw';
                } catch (err) {
                    globalThis.__probe = err.name;
                }
            });
        }"#,
        "new Request('http://example.com/req')",
    );

    assert_eq!(response, "answered");
    assert_eq!(probe, "no throw");
}

/// The other side of `active`: a `waitUntil` from a later microtask is fine while the `respondWith`
/// promise is still unsettled, because that promise is itself a lifetime promise and the pending
/// count keeps the event active regardless of the dispatch flag.
///
/// WPT `resources/extendable-event-async-waituntil.js`, fetch case
/// `pending-respondwith-async-waituntil`.
#[test]
fn wait_until_in_a_microtask_is_allowed_while_respond_with_is_pending() {
    let (response, probe) = dispatch(
        r#"(e) => {
            // Settles only on a timer, so it is still pending when the microtask below runs.
            e.respondWith(new Promise((resolve) => setTimeout(() => resolve(mark('answered')), 2)));
            Promise.resolve().then(() => {
                try {
                    e.waitUntil(Promise.resolve());
                    globalThis.__probe = 'no throw';
                } catch (err) {
                    globalThis.__probe = err.name;
                }
            });
        }"#,
        "new Request('http://example.com/req')",
    );

    assert_eq!(
        probe, "no throw",
        "a pending respondWith keeps the event active for waitUntil"
    );
    assert_eq!(response, "answered");
}

/// Every lifetime promise releases its own loop interest, so a handler calling `waitUntil` more
/// than once still lets the request finish. Sharing one settle reaction across an event's promises
/// leaks every handle after the first — the response still goes out, but the request's loop never
/// completes and its in-flight slot is never freed.
///
/// The assertion is `run_to_completion` returning at all; this hangs if an interest leaks.
#[test]
fn each_lifetime_promise_releases_its_own_interest() {
    let (response, probe) = dispatch(
        r#"(e) => {
            e.respondWith(mark('answered'));
            // Three lifetime promises on one event, settling at different times.
            e.waitUntil(Promise.resolve());
            e.waitUntil(new Promise((resolve) => setTimeout(resolve, 1)));
            e.waitUntil(Promise.resolve().then(() => {
                globalThis.__probe = 'all three ran';
            }));
        }"#,
        "new Request('http://example.com/req')",
    );

    assert_eq!(response, "answered");
    assert_eq!(probe, "all three ran");
}

/// The `handled` promise settles the way [`FetchEvent::settle_handled`] documents: resolved once a
/// `Response` is on its way, rejected with a `NetworkError` otherwise — including the declined case,
/// where the spec resolves (step 21.2) because it has a network to fall back to and this has none.
///
/// The rejection's message is the only thing that distinguishes a failed `respondWith` from a
/// handler that never responded, so both are pinned here.
#[test]
fn handled_settles_by_whether_a_response_is_on_its_way() {
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    let rawcx = unsafe { scope.cx_mut().raw_cx() };
    // Subscribing inside the handler is the only way to observe the settle: `handled` is settled
    // after the dispatch is over, and its reactions run in the checkpoint that follows.
    let subscribe = r#"(e) => {
        e.handled.then(
            (value) => { globalThis.__probe = 'resolved:' + value; },
            (err) => { globalThis.__probe = err.name + ':' + err.message; },
        );
    "#;
    for (handler_tail, responded, expected) in [
        (
            "e.respondWith(mark('answered')); }",
            true,
            "resolved:undefined",
        ),
        (
            "e.respondWith(Promise.reject(new RangeError('no'))); }",
            false,
            "NetworkError:the promise passed to respondWith did not produce a Response",
        ),
        (
            "}",
            false,
            "NetworkError:the fetch event was not responded to",
        ),
    ] {
        eval(&scope, "globalThis.__probe = undefined;");
        let el = EventLoop::new();
        let event = prepare_dispatch(
            &scope,
            &el,
            &format!("{subscribe}{handler_tail}"),
            "new Request('http://example.com/req')",
        );
        tokio_rt()
            .block_on(async { unsafe { run_to_completion(rawcx, &el, tokio::time::sleep).await } });
        event.settle_handled(&scope, responded);
        js::jobs::run_jobs(&scope);

        let probe =
            String::from_jsval(&scope, eval(&scope, "String(globalThis.__probe)"), ()).unwrap();
        assert_eq!(probe, expected, "{handler_tail}");
    }
}

/// Settling twice does nothing: the serve path settles `handled` once per request, but the event
/// outlives that call through `waitUntil` work, and a second settle must not replace the outcome
/// the client was told about.
#[test]
fn handled_settles_only_once() {
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"addEventListener('fetch', (e) => {
            e.handled.then(
                () => { globalThis.__probe = 'resolved'; },
                (err) => { globalThis.__probe = err.name; },
            );
        });"#,
    );

    let event = dispatch_once(&scope, "new Request('http://example.com/req')");
    event.settle_handled(&scope, false);
    js::jobs::run_jobs(&scope);
    event.settle_handled(&scope, true);
    js::jobs::run_jobs(&scope);

    let probe = String::from_jsval(&scope, eval(&scope, "String(globalThis.__probe)"), ()).unwrap();
    assert_eq!(probe, "NetworkError", "the first settle stands");
}

/// `respondWith` step 10.2.2.1: a `Response` whose body is already disturbed or locked is
/// `unusable`, which sets the `respond-with error flag` and leaves no potential response — the
/// request gets a network error rather than a half-read body.
#[test]
fn responding_with_an_unusable_body_is_a_network_error() {
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);
    let rawcx = unsafe { scope.cx_mut().raw_cx() };

    for handler in [
        // Disturbed: reading it started before respondWith saw it.
        "(e) => { const r = new Response('payload', { statusText: 'marked' }); r.text(); \
         e.respondWith(r); }",
        // Locked: a reader is still attached.
        "(e) => { const r = new Response('payload', { statusText: 'marked' }); \
         r.body.getReader(); e.respondWith(r); }",
    ] {
        let el = EventLoop::new();
        let event = prepare_dispatch(&scope, &el, handler, "new Request('http://example.com/')");
        tokio_rt()
            .block_on(async { unsafe { run_to_completion(rawcx, &el, tokio::time::sleep).await } });

        assert_eq!(response_marker(&scope, &event), "", "{handler}");
        assert!(event.respond_with_error_set(), "{handler}");
    }
}

/// Taking the response's body is step 10.2.2's transform, and it happens when the `respondWith`
/// promise settles rather than when the transport reads, so a handler that keeps a reference sees
/// `bodyUsed` flip before a single byte goes out.
#[test]
fn responding_marks_the_body_used_at_settle_time() {
    let rt = test_runtime();
    let scope = rt.default_global();
    install_globals(&scope);

    eval(
        &scope,
        r#"addEventListener('fetch', (e) => {
            globalThis.kept = new Response('payload', { statusText: 'marked' });
            e.respondWith(globalThis.kept);
        });"#,
    );

    let event = dispatch_once(&scope, "new Request('http://example.com/req')");
    assert_eq!(response_marker(&scope, &event), "marked");

    let used =
        String::from_jsval(&scope, eval(&scope, "String(globalThis.kept.bodyUsed)"), ()).unwrap();
    assert_eq!(used, "true", "no byte has been transmitted yet");
}

/// Once `respondWith`'s promise settles, a `.body` stream the handler already
/// materialized is locked for sending, so author code can no longer read it.
#[test]
fn respond_with_locks_a_materialized_body_stream() {
    let (_, probe) = dispatch(
        "(event) => {
            const response = new Response(new ReadableStream({
                start(c) { c.enqueue(new Uint8Array([1])); c.close(); },
            }));
            response.body;
            event.respondWith(response);
            event.waitUntil(new Promise(resolve => setTimeout(() => {
                try { response.body.getReader(); globalThis.__probe = 'readable'; }
                catch (e) { globalThis.__probe = `${e.constructor.name},${response.bodyUsed}`; }
                resolve();
            }, 0)));
        }",
        "new Request('http://example.com/')",
    );
    assert_eq!(probe, "TypeError,true");
}
