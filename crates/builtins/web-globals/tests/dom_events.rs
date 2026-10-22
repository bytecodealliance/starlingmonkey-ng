// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for the DOM Event interfaces: Event, EventTarget, CustomEvent.
//!
//! These tests deliberately cover only behavior that the enabled, passing WPT
//! suite (see `tests/wpt-harness/tests.json`) doesn't exercise. What remains is
//! coverage for behaviors whose only WPT home is a `.html` test, which this
//! headless harness cannot run, and for GC interaction, which WPT cannot
//! express.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::{self, Runtime};
use core_runtime::test_util::eval_with_setup;
use js::conversion::FromJSVal;
use web_globals::events::EventTarget;

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        web_globals::add_to_global(scope, global);
    });
}

fn eval(code: &str) -> String {
    eval_with_setup(setup, code)
}

// ── Event properties not covered by enabled WPT ──

#[test]
fn event_composed_default() {
    // `composed` is not checked by Event-constructors.any.js.
    assert_eq!(eval("new Event('x').composed"), "false");
}

#[test]
fn event_prevent_default_non_cancelable() {
    // preventDefault on a non-cancelable event has no effect. The passive WPT
    // test only exercises the cancelable case.
    assert_eq!(
        eval("var e = new Event('x'); e.preventDefault(); e.defaultPrevented"),
        "false"
    );
}

#[test]
fn event_stop_propagation_and_cancel_bubble() {
    // stopPropagation()/cancelBubble live only in Event-cancelBubble.html.
    assert_eq!(
        eval("var e = new Event('x'); e.stopPropagation(); e.cancelBubble"),
        "true"
    );
    assert_eq!(
        eval("var e = new Event('x'); e.cancelBubble = true; e.cancelBubble"),
        "true"
    );
}

// ── Event.initEvent (only Event-initEvent.html otherwise) ──

#[test]
fn event_init_event() {
    assert_eq!(
        eval(
            "var e = new Event(''); e.initEvent('click', true, false); \
             e.type + ',' + e.bubbles + ',' + e.cancelable"
        ),
        "click,true,false"
    );
}

// ── EventTarget ──

#[test]
fn dispatch_event_at_target_phase() {
    // eventPhase during dispatch is only asserted by Event-dispatch-*.html.
    assert_eq!(
        eval(
            r#"
            var target = new EventTarget();
            var phase = -1;
            target.addEventListener('test', function(e) { phase = e.eventPhase; });
            target.dispatchEvent(new Event('test'));
            phase
            "#
        ),
        "2"
    );
}

#[test]
fn multiple_listeners_called_in_order() {
    // Listener invocation order is only asserted by the non-enabled
    // Event-dispatch-listener-order.window.js.
    assert_eq!(
        eval(
            r#"
            var target = new EventTarget();
            var order = [];
            target.addEventListener('test', function() { order.push(1); });
            target.addEventListener('test', function() { order.push(2); });
            target.addEventListener('test', function() { order.push(3); });
            target.dispatchEvent(new Event('test'));
            order.join(',')
            "#
        ),
        "1,2,3"
    );
}

#[test]
fn listener_only_type_match() {
    assert_eq!(
        eval(
            r#"
            var target = new EventTarget();
            var called = false;
            target.addEventListener('click', function() { called = true; });
            target.dispatchEvent(new Event('other'));
            called
            "#
        ),
        "false"
    );
}

#[test]
fn remove_nonexistent_listener_is_noop() {
    // Should not throw or error.
    assert_eq!(
        eval(
            r#"
            var target = new EventTarget();
            target.removeEventListener('click', function() {});
            "ok"
            "#
        ),
        "ok"
    );
}

#[test]
fn dispatch_already_dispatching_throws_in_listener() {
    // The inner dispatchEvent throws InvalidStateError because the event's
    // dispatch flag is already set. The outer dispatch completes normally
    // because listener exceptions are caught per the spec. Only covered by
    // Event-dispatch-reenter.html otherwise.
    assert_eq!(
        eval(
            r#"
        var target = new EventTarget();
        var event = new Event('test');
        var caught = false;
        target.addEventListener('test', function() {
            try {
                target.dispatchEvent(event);
            } catch(e) {
                caught = e instanceof DOMException && e.name === 'InvalidStateError';
            }
        });
        target.dispatchEvent(event);
        caught
        "#
        ),
        "true"
    );
}

#[test]
fn dispatch_redispatch_used_event() {
    // Re-dispatching an already-dispatched event must succeed; only covered by
    // Event-dispatch-redispatch.html otherwise.
    assert_eq!(
        eval(
            r#"
            var target = new EventTarget();
            var event = new Event('test');
            target.dispatchEvent(event);
            target.dispatchEvent(event);
            "ok"
            "#
        ),
        "ok"
    );
}

/// A `once` listener leaves the event listener list when it runs. JS cannot observe the
/// list's length, so this reads it directly.
#[test]
fn dispatch_removes_once_listeners_from_the_list() {
    runtime::register_global_initializer(web_globals::add_to_global);
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let value = js::compile::evaluate_with_filename(
        &scope,
        "const t = new EventTarget();
         t.addEventListener('x', () => {});
         for (let i = 0; i < 3; i++) {
           t.addEventListener('x', () => {}, { once: true });
           t.dispatchEvent(new Event('x'));
         }
         t",
        "test.js",
        1,
    )
    .expect("script should evaluate");
    let target = EventTarget::from_jsval(&scope, value, ()).expect("an EventTarget");
    assert_eq!(target.listener_count(), 1);
}

// ── CustomEvent ──
//
// The only enabled WPT touching CustomEvent is one assertion in
// Event-constructors.any.js (`detail: 54`) plus a subclass dispatch in
// EventTarget-constructible.any.js. CustomEvent.html is not runnable here, so
// detail typing, defaults, and initCustomEvent are covered below.

#[test]
fn custom_event_extends_event() {
    assert_eq!(
        eval(
            "var e = new CustomEvent('x'); (e instanceof CustomEvent) + ',' + (e instanceof Event)"
        ),
        "true,true"
    );
}

#[test]
fn custom_event_detail() {
    assert_eq!(
        eval(
            "JSON.stringify([new CustomEvent('x').detail, \
                             new CustomEvent('x', { detail: 'hello' }).detail, \
                             new CustomEvent('x', { detail: { a: 1 } }).detail])"
        ),
        r#"[null,"hello",{"a":1}]"#
    );
}

#[test]
fn custom_event_init_custom_event() {
    assert_eq!(
        eval(
            r#"
            var e = new CustomEvent('');
            e.initCustomEvent('test', true, true, 99);
            e.type + ',' + e.bubbles + ',' + e.detail
            "#
        ),
        "test,true,99"
    );
}

/// Listener identity survives a compacting GC, which moves the callback between the listener
/// operations: deduplication and removal still match the relocated callback.
#[test]
fn listener_identity_survives_compacting_gc() {
    use core_runtime::config::RuntimeConfig;
    use core_runtime::runtime::Runtime;
    use js::compile::evaluate_with_filename;
    use js::conversion::FromJSVal;
    use js::gc::{self, GCOptions, GCReason};

    setup();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    // Register one listener. State lives on `globalThis` so it survives across
    // evaluations and the intervening collection.
    evaluate_with_filename(
        &scope,
        r#"
        globalThis.t = new EventTarget();
        globalThis.count = 0;
        globalThis.handler = function() { globalThis.count++; };
        globalThis.t.addEventListener('e', globalThis.handler);
        "#,
        "test.js",
        1,
    )
    .expect("listener setup should evaluate");

    // Force a full, compacting GC. This relocates `handler`.
    gc::prepare_for_full_gc(&scope);
    gc::non_incremental_gc(&scope, GCOptions::Shrink, GCReason::API);

    // After relocation: a duplicate add must still be recognised (dedup), a
    // dispatch must fire the single listener once, and removal must match the
    // relocated callback so the final dispatch doesn't call a listener.
    let val = evaluate_with_filename(
        &scope,
        r#"
        globalThis.t.addEventListener('e', globalThis.handler); // dedup
        globalThis.t.dispatchEvent(new Event('e'));             // fires once
        globalThis.t.removeEventListener('e', globalThis.handler);
        globalThis.t.dispatchEvent(new Event('e'));             // calls no listener
        String(globalThis.count)
        "#,
        "test.js",
        1,
    )
    .expect("dispatch should evaluate");

    assert_eq!(String::from_jsval(&scope, val, ()).unwrap(), "1");
}

// ── The global is an EventTarget ──
//
// Per HTML, WorkerGlobalScope (and thus the ServiceWorkerGlobalScope this
// runtime models) inherits from EventTarget, so `globalThis` *is* an event
// target. Only `.html` harness files assert this, so it is covered here.

#[test]
fn global_is_event_target() {
    assert_eq!(eval("globalThis instanceof EventTarget"), "true");
}

#[test]
fn global_event_target_methods_dispatch() {
    // addEventListener/dispatchEvent called with the global as the receiver must
    // brand-check against the inheritance entry registered for the global class
    // (`register_global_parent`) and resolve to the global's primordial
    // EventTarget data, rather than throwing "incompatible receiver".
    assert_eq!(
        eval(
            r#"
            let fired = 0;
            globalThis.addEventListener('e', () => { fired++; });
            globalThis.dispatchEvent(new Event('e'));
            String(fired)
            "#
        ),
        "1"
    );
}

#[test]
fn global_bare_add_event_listener_resolves_this_to_global() {
    // An unqualified `addEventListener('e', …)` passes `this = undefined` (a
    // global reference's ImplicitThisValue), and SpiderMonkey performs no global
    // substitution for native callees. WebIDL [Global] semantics resolve the
    // undefined receiver to the global — which is an EventTarget — so the bare
    // call (the ServiceWorker `addEventListener('fetch', …)` registration
    // pattern) works without an explicit `globalThis.` receiver.
    assert_eq!(
        eval(
            r#"
            let fired = 0;
            addEventListener('e', () => { fired++; });
            dispatchEvent(new Event('e'));
            String(fired)
            "#
        ),
        "1"
    );
}

#[test]
fn undefined_this_on_non_global_interface_still_throws() {
    // The [Global] substitution is gated on the global actually implementing the
    // interface. The global is an EventTarget, not an Event, so an Event method
    // invoked with an undefined receiver is rejected exactly as without the
    // substitution — the error is unchanged for every non-global interface.
    assert_eq!(
        eval(
            r#"
            let r = 'no throw';
            try { Event.prototype.stopPropagation.call(undefined); }
            catch (e) { r = (e instanceof TypeError) + ':' + e.message; }
            r
            "#
        ),
        "true:'this' is not an object"
    );
}

/// A listener registered directly on the global must survive a compacting GC.
///
/// The global's listener list lives in its primordial private data (slot 0).
/// Unlike a normal instance, that data is traced through the realm's custom
/// `setTrace` op (installed by `install_global_private_trace`), since the global
/// class trace hook is SpiderMonkey's own `JS_GlobalObjectTraceHook`. The
/// closure here is referenced *only* by the global's listener list, so a missing
/// trace would let a full collection reclaim or dangle it. Dropping `rt` at the
/// end also runs `finalize_starling_global`, exercising the finalize glue that
/// frees the listener list.
///
/// WPT cannot express this: it has no control over garbage collection.
#[test]
fn global_listener_survives_compacting_gc() {
    use core_runtime::config::RuntimeConfig;
    use core_runtime::runtime::Runtime;
    use js::compile::evaluate_with_filename;
    use js::conversion::FromJSVal;
    use js::gc::{self, GCOptions, GCReason};

    setup();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    // The closure's only strong reference is the global's listener list.
    evaluate_with_filename(
        &scope,
        r#"
        globalThis.count = 0;
        globalThis.addEventListener('e', function() { globalThis.count++; });
        "#,
        "test.js",
        1,
    )
    .expect("listener setup should evaluate");

    // Force a full, compacting GC. If the global's listener list is not traced,
    // the sole-referenced closure is reclaimed or relocated out from under it.
    gc::prepare_for_full_gc(&scope);
    gc::non_incremental_gc(&scope, GCOptions::Shrink, GCReason::API);

    let val = evaluate_with_filename(
        &scope,
        r#"
        globalThis.dispatchEvent(new Event('e'));
        String(globalThis.count)
        "#,
        "test.js",
        1,
    )
    .expect("dispatch should evaluate");

    assert_eq!(String::from_jsval(&scope, val, ()).unwrap(), "1");
}

/// `addEventListener` accepts `EventListener` objects with a `handleEvent`
/// method, calling it with the object as `this`.
#[test]
fn add_event_listener_accepts_handle_event_objects() {
    assert_eq!(
        eval(
            "const t = new EventTarget();
             const listener = { seen: '', handleEvent(e) { this.seen = e.type; } };
             t.addEventListener('ping', listener);
             t.dispatchEvent(new Event('ping'));
             listener.seen"
        ),
        "ping"
    );
}

/// A non-object callback is a TypeError, while `null` doesn't add a listener.
#[test]
fn add_event_listener_rejects_non_object_callbacks() {
    assert_eq!(
        eval(
            "const t = new EventTarget();
             t.addEventListener('ping', null);
             let threw = false;
             try { t.addEventListener('ping', 42); } catch (e) { threw = e instanceof TypeError; }
             String(threw)"
        ),
        "true"
    );
}

/// A non-boolean, non-object `options` value is converted with `ToBoolean`, so
/// `1` registers a capturing listener that `removeEventListener(.., true)` removes.
#[test]
fn listener_options_convert_primitives_with_to_boolean() {
    assert_eq!(
        eval(
            "const t = new EventTarget();
             let count = 0;
             const f = () => count++;
             t.addEventListener('ping', f, 1);
             t.removeEventListener('ping', f, true);
             t.dispatchEvent(new Event('ping'));
             String(count)"
        ),
        "0"
    );
}

/// An exception thrown while reading an options member propagates instead of
/// being left pending.
#[test]
fn throwing_options_getter_propagates() {
    assert_eq!(
        eval(
            "const t = new EventTarget();
             let msg = '';
             try {
                 t.addEventListener('ping', () => {}, { get capture() { throw new Error('boom'); } });
             } catch (e) { msg = e.message; }
             msg"
        ),
        "boom"
    );
}
