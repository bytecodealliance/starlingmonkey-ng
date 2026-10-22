// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `EventLoop::ensure_idle_for_snapshot`, and the origins a runtime configured for a snapshot
//! records for the work it reports.

use core_runtime::config::RuntimeConfig;
use core_runtime::event_loop::timer::install_timer_globals;
use core_runtime::event_loop::{with_event_loop, EventLoop};
use core_runtime::runtime::Runtime;
use js::compile::evaluate_with_filename;
use js::gc::scope::Scope;

/// Evaluate `code` from `filename` with `event_loop` active.
fn eval_on(scope: &Scope<'_>, event_loop: &EventLoop, filename: &str, code: &str) {
    with_event_loop(event_loop, |_| {
        evaluate_with_filename(scope, code, filename, 1)
            .unwrap_or_else(|_| panic!("{filename} threw"));
    });
}

#[test]
fn pending_work_is_reported_with_its_origin() {
    let config = RuntimeConfig {
        pre_initialize: true,
        ..Default::default()
    };
    let rt = Runtime::init(&config).expect("runtime init");
    let scope = rt.default_global();
    install_timer_globals(&scope, scope.global());

    // A loop without tasks or futures can be snapshotted.
    assert!(EventLoop::new().ensure_idle_for_snapshot().is_ok());

    // A timer is reported with the stack that created it, and clearing it leaves the loop idle.
    {
        let el = EventLoop::new();
        eval_on(
            &scope,
            &el,
            "arm.js",
            "function arm() {\n  return setTimeout(() => {}, 60000);\n}\nglobalThis.timer = arm();",
        );
        let message = el.ensure_idle_for_snapshot().unwrap_err();
        assert!(
            message.contains("a pending `timeout` timer, created at:\n      arm@arm.js:2:"),
            "{message}"
        );
        assert!(message.contains("\n      @arm.js:4:"), "{message}");
        eval_on(&scope, &el, "clear.js", "clearTimeout(globalThis.timer);");
        assert!(el.ensure_idle_for_snapshot().is_ok());
    }

    // An interval keeps the origin of the call that created it each time it re-queues itself.
    {
        let el = EventLoop::new();
        eval_on(
            &scope,
            &el,
            "interval.js",
            "globalThis.ticks = 0;\nglobalThis.interval = setInterval(() => { globalThis.ticks++; }, 0);",
        );
        while evaluate_with_filename(&scope, "globalThis.ticks", "ticks.js", 1)
            .unwrap()
            .to_int32()
            == 0
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
            with_event_loop(&el, |el| el.step(&scope));
        }
        let message = el.ensure_idle_for_snapshot().unwrap_err();
        assert!(
            message.contains("a pending `interval` timer, created at:\n      @interval.js:2:"),
            "{message}"
        );
        eval_on(
            &scope,
            &el,
            "clear.js",
            "clearInterval(globalThis.interval);",
        );
        assert!(el.ensure_idle_for_snapshot().is_ok());
    }

    // Interest acquired with no script running has no origin, and releasing it leaves the loop
    // idle.
    {
        let el = EventLoop::new();
        let interest = el.acquire_interest_handle();
        let message = el.ensure_idle_for_snapshot().unwrap_err();
        assert!(
            message.contains(
                "an operation that keeps the event loop alive, created with no JavaScript on the \
                 stack"
            ),
            "{message}"
        );
        drop(interest);
        assert!(el.ensure_idle_for_snapshot().is_ok());
    }

    // Once recording stops, work is still reported, without an origin.
    {
        // SAFETY: stopping the recording has no precondition.
        unsafe { js::stack::record_origins(None) };
        let el = EventLoop::new();
        eval_on(
            &scope,
            &el,
            "late.js",
            "globalThis.late = setTimeout(() => {}, 60000);",
        );
        let message = el.ensure_idle_for_snapshot().unwrap_err();
        assert!(
            message.contains("a pending `timeout` timer, created with no JavaScript on the stack"),
            "{message}"
        );
        eval_on(&scope, &el, "clear.js", "clearTimeout(globalThis.late);");
    }
}
