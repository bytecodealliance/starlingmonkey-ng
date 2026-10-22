// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for the DOM Abort interfaces: AbortController, AbortSignal.
//!
//! Abort/reason/throwIfAborted/event semantics, `AbortSignal.any()`, and
//! `AbortSignal.timeout()` are thoroughly covered by the enabled, passing WPT
//! tests `dom/abort/event.any.js`, `dom/abort/AbortSignal.any.js`,
//! `dom/abort/abort-signal-any.any.js`, and `dom/abort/timeout.any.js` (see
//! `tests/wpt-harness/tests.json`), so that ground is not retread here. What
//! remains is coverage WPT does not give us in this harness:
//!
//! - `onabort` getter identity and the set-to-null path, which `event.any.js`
//!   does not assert.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::test_util::eval_with_setup;
use js::conversion::FromJSVal;
use web_globals::signals::AbortSignal;

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        web_globals::add_to_global(scope, global);
        // Test-only hook: `__addAbortAlgorithm(signal, fn)` registers `fn` as a
        // native abort algorithm — the internal API builtins like fetch use —
        // so tests can exercise algorithm semantics that have no public JS
        // surface (notably: a throwing algorithm must not short-circuit the
        // remaining algorithms, the `abort` event, or dependent signals).
        let helper = js::Function::new_callback(
            scope,
            c"__addAbortAlgorithm",
            2,
            |scope, args, _| {
                let signal = AbortSignal::from_jsval_throwing(scope, args.get(0), ())?;
                let callback = js::Function::from_jsval_throwing(scope, args.get(1), ())?;
                web_globals::signals::algorithms::add_abort_algorithm(&signal, &callback);
                Ok(js::value::undefined())
            },
            (),
        )
        .expect("create __addAbortAlgorithm");
        global
            .set_property(scope, c"__addAbortAlgorithm", helper)
            .expect("install __addAbortAlgorithm");
    });
}

fn eval(code: &str) -> String {
    eval_with_setup(setup, code)
}

// ── onabort event handler ──

#[test]
fn onabort_getter_returns_handler() {
    assert_eq!(
        eval(
            r#"
            var ac = new AbortController();
            var initial = ac.signal.onabort;
            var fn = function() {};
            ac.signal.onabort = fn;
            (initial === null) + "," + (ac.signal.onabort === fn)
            "#
        ),
        "true,true"
    );
}

#[test]
fn onabort_set_null_removes_handler() {
    assert_eq!(
        eval(
            r#"
            var ac = new AbortController();
            var called = false;
            ac.signal.onabort = function() { called = true; };
            ac.signal.onabort = null;
            ac.abort();
            called
            "#
        ),
        "false"
    );
}

/// `onabort` keeps any object, callable or not, and is null for any other value. A handler that
/// isn't callable does nothing when the event is dispatched.
#[test]
fn onabort_keeps_objects_and_nulls_other_values() {
    assert_eq!(
        eval(
            r#"
            var ac = new AbortController();
            var object = {};
            var results = [];
            ac.signal.onabort = object;
            results.push(ac.signal.onabort === object);
            ac.signal.onabort = 5;
            results.push(ac.signal.onabort);
            ac.signal.onabort = object;
            var called = false;
            ac.signal.addEventListener("abort", function() { called = true; });
            ac.abort();
            results.push(called);
            results.join()
            "#
        ),
        "true,,true"
    );
}

/// Replacing `onabort` keeps its listener's place among the event listeners, and the handler's
/// listener is separate from a listener of the same function added with `addEventListener`.
#[test]
fn onabort_listener_keeps_its_place_and_identity() {
    assert_eq!(
        eval(
            r#"
            var ac = new AbortController();
            var order = [];
            var shared = function() { order.push("shared"); };
            ac.signal.onabort = function() { order.push("a"); };
            ac.signal.addEventListener("abort", function() { order.push("b"); });
            ac.signal.onabort = function() { order.push("c"); };
            ac.signal.addEventListener("abort", shared);
            ac.signal.removeEventListener("abort", shared);
            ac.signal.addEventListener("abort", shared);
            ac.signal.onabort = shared;
            ac.signal.removeEventListener("abort", shared);
            ac.abort();
            order.join()
            "#
        ),
        "shared,b"
    );
}

// ── Abort algorithms

#[test]
fn throwing_abort_algorithm_does_not_short_circuit() {
    // A throwing algorithm is reported, and must not keep the remaining
    // algorithms or the `abort` event from running (run-the-abort-steps is
    // infallible in the spec).
    assert_eq!(
        eval(
            r#"
            var ac = new AbortController();
            var events = [];
            __addAbortAlgorithm(ac.signal, function() { throw new Error("algo-1 boom"); });
            __addAbortAlgorithm(ac.signal, function() { events.push("algo-2"); });
            ac.signal.addEventListener("abort", function() { events.push("event"); });
            ac.abort();
            events.join(",")
            "#
        ),
        "algo-2,event"
    );
}

#[test]
fn throwing_abort_algorithm_still_aborts_dependents() {
    // Signal-abort step 6: dependent signals' abort steps run even when one of
    // the parent's algorithms throws.
    assert_eq!(
        eval(
            r#"
            var ac = new AbortController();
            var dep = AbortSignal.any([ac.signal]);
            var events = [];
            __addAbortAlgorithm(ac.signal, function() { throw new Error("boom"); });
            dep.addEventListener("abort", function() { events.push("dep-event"); });
            ac.abort();
            events.push("dep-aborted=" + dep.aborted);
            events.join(",")
            "#
        ),
        "dep-event,dep-aborted=true"
    );
}

#[test]
fn any_with_huge_array_like_length_throws_not_crashes() {
    // A hostile `length` isn't used to pre-size an allocation. The first element isn't an
    // `AbortSignal`, so this throws a `TypeError`.
    assert_eq!(
        eval(
            "try { AbortSignal.any({ length: 1e20 }); 'no-throw' } \
             catch (e) { e instanceof TypeError }"
        ),
        "true"
    );
}
