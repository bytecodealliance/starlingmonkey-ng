// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration test for the async-promise event-loop driver.
//!
//! `fetch` and other async-IO builtins return a JS promise backed by a Rust
//! future (`PromiseFuture`). The event loop's `run_to_completion` must poll those
//! futures with a real waker and settle their promises. This test exercises that
//! path end to end with a tokio-timer-backed future — no networking — covering
//! both resolution and rejection.

#![cfg(not(target_arch = "wasm32"))]

use core_runtime::event_loop::{run_to_completion, with_event_loop, EventLoop};
use core_runtime::runtime::{clear_global_initializers, register_global_initializer, Runtime};
use js::conversion::FromJSVal;
use js::gc::scope::Scope;
use js::promise::PromiseFuture;
use std::time::Duration;

/// A test builtin whose methods return promises backed by tokio-timer futures.
#[core_runtime::jsclass]
struct AsyncTest {}

#[core_runtime::jsmethods]
impl AsyncTest {
    #[constructor]
    fn new() -> Self {
        AsyncTestImpl {}
    }

    /// Resolve with `value` after a short delay.
    #[method]
    fn delayed(&self, value: i32) -> PromiseFuture {
        PromiseFuture::from_value(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok::<i32, String>(value)
        })
    }

    /// Reject with `message` after a short delay.
    #[method]
    fn fail(&self, message: String) -> PromiseFuture {
        PromiseFuture::from_value(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Err::<i32, String>(message)
        })
    }
}

/// A test builtin whose promises' futures keep the event loop alive only while it is referenced.
#[core_runtime::jsclass]
struct RefTest {
    #[no_trace]
    referenced: js::promise::FutureRef,
}

#[core_runtime::jsmethods]
impl RefTest {
    #[constructor]
    fn new() -> Self {
        RefTestImpl {
            referenced: js::promise::FutureRef::default(),
        }
    }

    /// Resolve with `value` after `ms` milliseconds.
    #[method]
    fn later<'r>(
        &self,
        scope: &'r Scope<'_>,
        value: i32,
        ms: i32,
    ) -> Result<js::Promise<'r>, js::error::ExnThrown> {
        let promise = js::Promise::new_pending(scope)?;
        let future = PromiseFuture::from_value(async move {
            tokio::time::sleep(Duration::from_millis(ms as u64)).await;
            Ok::<i32, String>(value)
        });
        promise.spawn_with_ref(future, self.data().referenced.clone());
        Ok(promise)
    }

    #[method]
    fn set_referenced(&self, referenced: bool) {
        self.data().referenced.set_referenced(referenced);
    }
}

fn block_on_event_loop(scope: &Scope<'_>, el: &mut EventLoop) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        let raw_cx = unsafe { scope.raw_cx_no_gc() };
        unsafe { run_to_completion(raw_cx, el, tokio::time::sleep).await }
    });
}

#[test]
fn async_promise_resolves_and_rejects_via_event_loop() {
    clear_global_initializers();
    register_global_initializer(AsyncTest::add_to_global);
    let rt = Runtime::init(&core_runtime::config::RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let mut el = EventLoop::new();

    // The two promises are spawned (their futures queued) during evaluation; they
    // are not event-loop tasks, so only the future driver will settle them.
    {
        with_event_loop(&el, |_| {
            js::compile::evaluate_with_filename(
                &scope,
                r#"
                globalThis.__resolved = "pending";
                globalThis.__rejected = "pending";
                const t = new AsyncTest();
                t.delayed(42).then(v => { globalThis.__resolved = String(v); });
                t.fail("boom").then(
                    () => { globalThis.__rejected = "unexpected-fulfill"; },
                    e => { globalThis.__rejected = `${e.constructor.name}:${e.message}`; },
                );
                "#,
                "<test>",
                1,
            )
            .expect("evaluation failed");
        });
    }

    block_on_event_loop(&scope, &mut el);

    let read = |expr: &str| -> String {
        let v = js::compile::evaluate_with_filename(&scope, expr, "<check>", 1).unwrap();
        String::from_jsval(&scope, v, ()).unwrap()
    };
    assert_eq!(read("globalThis.__resolved"), "42");
    assert_eq!(read("globalThis.__rejected"), "TypeError:boom");
}

#[test]
fn zero_delay_interval_does_not_starve_async_futures() {
    // The classic poll-until-done pattern: a zero-delay interval spinning while a
    // Rust-future-backed promise resolves. Without the HTML nested-timer clamp and
    // one-batch-per-step stepping, the perpetually-ready interval keeps the loop
    // from ever reaching its await branch — the future is never polled, the
    // promise never settles, and this test livelocks.
    clear_global_initializers();
    register_global_initializer(AsyncTest::add_to_global);
    let rt = Runtime::init(&core_runtime::config::RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let mut el = EventLoop::new();

    {
        with_event_loop(&el, |_| {
            js::compile::evaluate_with_filename(
                &scope,
                r#"
                globalThis.__out = "pending";
                let done = false;
                let value = null;
                new AsyncTest().delayed(7).then(v => { done = true; value = v; });
                const id = setInterval(() => {
                    if (done) { clearInterval(id); globalThis.__out = "done:" + value; }
                }, 0);
                "#,
                "<test>",
                1,
            )
            .expect("evaluation failed");
        });
    }

    block_on_event_loop(&scope, &mut el);

    let v = js::compile::evaluate_with_filename(&scope, "globalThis.__out", "<check>", 1).unwrap();
    assert_eq!(String::from_jsval(&scope, v, ()).unwrap(), "done:7");
}

#[test]
fn long_handler_timer_chain_does_not_starve_async_futures() {
    // The case the nested-timer clamp alone cannot save: each handler schedules
    // its successor first and then runs longer than the clamped 4ms delay, so
    // the successor is already expired when the handler ends. Every step has a
    // ready task, the loop never goes idle, and only the Progressed-path future
    // poll keeps the Rust-future-backed promise alive. The iteration cap turns
    // a starved future into an assertion failure instead of a livelock.
    clear_global_initializers();
    register_global_initializer(AsyncTest::add_to_global);
    let rt = Runtime::init(&core_runtime::config::RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let mut el = EventLoop::new();

    {
        with_event_loop(&el, |_| {
            js::compile::evaluate_with_filename(
                &scope,
                r#"
                globalThis.__out = "pending";
                let done = false;
                let value = null;
                let spins = 0;
                new AsyncTest().delayed(7).then(v => { done = true; value = v; });
                function busy(ms) { const end = Date.now() + ms; while (Date.now() < end) {} }
                function spin() {
                    if (done) { globalThis.__out = "done:" + value; return; }
                    if (++spins > 200) { globalThis.__out = "starved"; return; }
                    setTimeout(spin, 0);
                    busy(6);
                }
                spin();
                "#,
                "<test>",
                1,
            )
            .expect("evaluation failed");
        });
    }

    block_on_event_loop(&scope, &mut el);

    let v = js::compile::evaluate_with_filename(&scope, "globalThis.__out", "<check>", 1).unwrap();
    assert_eq!(String::from_jsval(&scope, v, ()).unwrap(), "done:7");
}

/// A future spawned with no event loop active belongs to no single loop: cancelling one loop's
/// futures leaves it in place, and another loop still drives it to completion.
#[test]
fn cancelling_one_loop_keeps_unowned_futures_for_the_others() {
    clear_global_initializers();
    register_global_initializer(AsyncTest::add_to_global);
    let rt = Runtime::init(&core_runtime::config::RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let cancelled = EventLoop::new();
    let mut driving = EventLoop::new();

    js::compile::evaluate_with_filename(
        &scope,
        r#"
        globalThis.__out = "pending";
        new AsyncTest().delayed(7).then(v => { globalThis.__out = String(v); });
        "#,
        "<test>",
        1,
    )
    .expect("evaluation failed");

    cancelled.cancel_pending_futures();
    block_on_event_loop(&scope, &mut driving);

    let v = js::compile::evaluate_with_filename(&scope, "globalThis.__out", "<check>", 1).unwrap();
    assert_eq!(String::from_jsval(&scope, v, ()).unwrap(), "7");
}

/// An unreferenced future doesn't keep the loop alive, and is still settled while a timer does.
/// Referencing it again from a task keeps the loop running until it settles.
#[test]
fn unreferenced_futures_keep_the_loop_alive_only_once_referenced() {
    clear_global_initializers();
    register_global_initializer(RefTest::add_to_global);
    let rt = Runtime::init(&core_runtime::config::RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let read = |expr: &str| -> String {
        let v = js::compile::evaluate_with_filename(&scope, expr, "<check>", 1).unwrap();
        String::from_jsval(&scope, v, ()).unwrap()
    };
    let run = |source: &str| {
        let mut el = EventLoop::new();
        with_event_loop(&el, |_| {
            js::compile::evaluate_with_filename(&scope, source, "<test>", 1)
                .expect("evaluation failed");
        });
        block_on_event_loop(&scope, &mut el);
    };

    run(r#"
        globalThis.__alone = "pending";
        globalThis.__timed = "pending";
        const alone = new RefTest();
        alone.setReferenced(false);
        alone.later(1, 200).then(v => { globalThis.__alone = String(v); });
        const timed = new RefTest();
        timed.setReferenced(false);
        timed.later(2, 1).then(v => { globalThis.__timed = String(v); });
        setTimeout(() => {}, 50);
        "#);
    assert_eq!(read("globalThis.__alone"), "pending");
    assert_eq!(read("globalThis.__timed"), "2");

    run(r#"
        globalThis.__out = "pending";
        const t = new RefTest();
        t.setReferenced(false);
        t.later(3, 30).then(v => { globalThis.__out = String(v); });
        setTimeout(() => t.setReferenced(true), 1);
        "#);
    assert_eq!(read("globalThis.__out"), "3");
}
