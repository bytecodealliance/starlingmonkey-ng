// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `EventLoop::ensure_idle_for_snapshot`, and the origins a runtime configured for a snapshot
//! records for the work it reports.
//!
//! The cases are grouped in a single test function because `JSEngine` can only be initialized
//! once per process.

use core_runtime::config::RuntimeConfig;
use core_runtime::event_loop::timer::install_timer_globals;
use core_runtime::event_loop::{with_event_loop, EventLoop, Task, TaskId};
use core_runtime::runtime::Runtime;
use js::compile::evaluate_with_filename;
use js::error::ExnThrown;
use js::gc::scope::Scope;

/// Evaluate `code` from `filename` with `event_loop` active.
fn eval_on(scope: &Scope<'_>, event_loop: &EventLoop, filename: &str, code: &str) {
    with_event_loop(event_loop, |_| {
        evaluate_with_filename(scope, code, filename, 1)
            .unwrap_or_else(|_| panic!("{filename} threw"));
    });
}

/// A task that does nothing, waiting to be signalled ready.
struct Waiting;

impl Task for Waiting {
    fn kind(&self) -> &'static str {
        "waiting"
    }

    fn run(self: Box<Self>, _scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
        Ok(())
    }
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

    // A loop that holds nothing can be wizened.
    assert!(EventLoop::new().ensure_idle_for_snapshot().is_ok());

    // Timers are not reported: a timeout, an interval that has fired and re-queued itself, and an
    // unreferenced timer.
    {
        let el = EventLoop::new();
        eval_on(
            &scope,
            &el,
            "timers.js",
            "globalThis.ticks = 0;\n\
             globalThis.timeout = setTimeout(() => {}, 60000);\n\
             globalThis.interval = setInterval(() => { globalThis.ticks++; }, 0);\n\
             globalThis.unref = setTimeout(() => {}, 60000);",
        );
        let id = evaluate_with_filename(&scope, "globalThis.unref", "id.js", 1)
            .unwrap()
            .to_number() as u64;
        el.set_js_timer_ref(id, false);
        while evaluate_with_filename(&scope, "globalThis.ticks", "ticks.js", 1)
            .unwrap()
            .to_int32()
            == 0
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
            with_event_loop(&el, |el| el.step(&scope));
        }
        assert!(el.ensure_idle_for_snapshot().is_ok());
        assert_eq!(el.len(), 3, "the timers stay queued");
    }

    // A task without a deadline is reported, and a timer queued with it is not.
    {
        let el = EventLoop::new();
        eval_on(
            &scope,
            &el,
            "timer.js",
            "globalThis.timer = setTimeout(() => {}, 60000);",
        );
        let task = el.queue(Box::new(Waiting));
        let message = el.ensure_idle_for_snapshot().unwrap_err();
        assert!(
            message.contains("a queued `waiting` task, created with no JavaScript on the stack"),
            "{message}"
        );
        assert!(!message.contains("timer"), "{message}");
        assert!(el.cancel_if_queued(task));
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
}
