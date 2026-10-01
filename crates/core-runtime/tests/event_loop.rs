// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for the event loop.
//!
//! Tests are grouped in a single test function because `JSEngine` can
//! only be initialized once per process, and must run with --test-threads=1.

#[cfg(not(target_arch = "wasm32"))]
use core_runtime::event_loop::run_to_completion;
use core_runtime::event_loop::timer::install_timer_globals;
use core_runtime::event_loop::{run_microtasks, with_event_loop, EventLoop, StepOutcome, Task};
use core_runtime::runtime::Runtime;
use js::gc::scope::Scope;
use js::native::JSTracer;

use js::error::ExnThrown;
use platform::clock::Instant;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// Helper: create a tokio current-thread runtime with timer support and
/// block on `run_to_completion` using `tokio::time::sleep`.
#[cfg(not(target_arch = "wasm32"))]
fn block_on_event_loop(scope: &Scope<'_>, el: &mut EventLoop) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        // SAFETY: the scope (and its Runtime) must outlive this future.
        // Since the caller owns both and `block_on` runs synchronously on
        // the same thread, this is guaranteed.
        let raw_cx = unsafe { scope.raw_cx_no_gc() };
        unsafe { run_to_completion(raw_cx, el, tokio::time::sleep).await }
    });
}

// ---------------------------------------------------------------------------
// Custom task for testing
// ---------------------------------------------------------------------------

/// A simple task that increments a shared counter when run.
struct CounterTask {
    counter: Rc<Cell<u32>>,
    #[allow(dead_code)]
    label: &'static str,
}

impl Task for CounterTask {
    fn kind(&self) -> &'static str {
        "counter"
    }

    fn run(
        self: Box<Self>,
        _scope: &Scope<'_>,
        _id: core_runtime::event_loop::TaskId,
    ) -> Result<(), ExnThrown> {
        self.counter.set(self.counter.get() + 1);
        Ok(())
    }

    fn trace(&self, _trc: *mut JSTracer) {}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_event_loop() {
    let rt = Runtime::init(&core_runtime::config::RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    // Install timer globals for timer tests.
    install_timer_globals(&scope, scope.global());

    // ---- Test 1: Basic EventLoop operations ----
    {
        let el = EventLoop::new();

        // Empty loop.
        assert!(el.is_empty());
        assert!(!el.has_pending());
        assert!(!el.has_ready());
        assert_eq!(el.len(), 0);

        // Queue a task.
        let counter = Rc::new(Cell::new(0u32));
        let id = el.queue(Box::new(CounterTask {
            counter: counter.clone(),
            label: "task1",
        }));

        assert!(el.has_pending());
        assert!(!el.has_ready());
        assert_eq!(el.len(), 1);

        // Not ready yet — pop_ready should return None.
        assert!(el.pop_ready().is_none());

        // Signal it ready.
        el.signal_ready(id);
        assert!(el.has_ready());

        // Pop and run it.
        let (popped_id, task) = el.pop_ready().unwrap();
        assert_eq!(popped_id, id);
        assert_eq!(counter.get(), 0);
        task.run(&scope, popped_id).unwrap();
        assert_eq!(counter.get(), 1);

        // Now the loop is empty.
        assert!(el.is_empty());
    }

    // ---- Test 2: queue_ready (immediately ready) ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "ready1",
        }));

        assert!(el.has_ready());
        let (id, task) = el.pop_ready().unwrap();
        task.run(&scope, id).unwrap();
        assert_eq!(counter.get(), 1);
        assert!(el.is_empty());
    }

    // ---- Test 3: Cancel ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        let id = el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "cancelled",
        }));

        assert!(el.cancel_if_queued(id));
        assert!(el.is_empty());
        assert_eq!(counter.get(), 0);

        // Cancel of non-existent ID returns false.
        assert!(!el.cancel_if_queued(id));
    }

    // ---- Test 4: Timer tasks ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        // Queue a timer 5ms in the future.
        let _id = el.queue_timer(
            Box::new(CounterTask {
                counter: counter.clone(),
                label: "timer1",
            }),
            Instant::now() + Duration::from_millis(5),
        );

        assert!(el.has_pending());
        assert!(!el.has_ready());

        // advance_timers should NOT mark it ready yet.
        assert_eq!(el.advance_timers(), 0);
        assert!(!el.has_ready());

        // time_to_next_timer should be Some and > 0.
        let wait = el.time_to_next_timer();
        assert!(wait.is_some());

        // Sleep past the deadline.
        std::thread::sleep(Duration::from_millis(10));

        // Now advance_timers should mark it ready.
        assert_eq!(el.advance_timers(), 1);
        assert!(el.has_ready());

        let (id, task) = el.pop_ready().unwrap();
        task.run(&scope, id).unwrap();
        assert_eq!(counter.get(), 1);
    }

    // ---- Test 5: Multiple tasks, ordering ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        let id1 = el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "first",
        }));
        let id2 = el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "second",
        }));
        let id3 = el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "third",
        }));

        // Pop and run all three.
        let mut ids = Vec::new();
        while let Some((id, task)) = el.pop_ready() {
            ids.push(id);
            task.run(&scope, id).unwrap();
        }

        assert_eq!(counter.get(), 3);
        assert!(el.is_empty());
        // All three IDs should be unique.
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
        assert!(ids.contains(&id3));
    }

    // ---- Test 6: run_to_completion with immediate tasks ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let counter = Rc::new(Cell::new(0u32));
        let mut el = EventLoop::new();

        el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "native1",
        }));
        el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "native2",
        }));

        block_on_event_loop(&scope, &mut el);
        assert_eq!(counter.get(), 2);
        assert!(el.is_empty());
    }

    // ---- Test 7: run_to_completion with timer ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let counter = Rc::new(Cell::new(0u32));
        let mut el = EventLoop::new();

        el.queue_timer(
            Box::new(CounterTask {
                counter: counter.clone(),
                label: "delayed",
            }),
            Instant::now() + Duration::from_millis(5),
        );

        block_on_event_loop(&scope, &mut el);
        assert_eq!(counter.get(), 1);
        assert!(el.is_empty());
    }

    // ---- Test 8: setTimeout from JavaScript ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        // Evaluate JS with the event loop thread-local set so setTimeout works.
        {
            with_event_loop(&el, |_| {
                let ok = js::compile::evaluate_with_filename(
                    &scope,
                    "globalThis._timerFired = false; setTimeout(function() { globalThis._timerFired = true; }, 1);",
                    "<test>",
                    1,
                );
                assert!(ok.is_ok(), "setTimeout JS evaluation failed");
            });
        }

        // The timer task should be queued.
        assert!(el.has_pending());

        // Run the event loop to fire the timer.
        block_on_event_loop(&scope, &mut el);

        // Verify the timer callback ran.
        let result = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._timerFired",
            "<test-check>",
            1,
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        assert!(
            val.is_boolean() && val.to_boolean(),
            "setTimeout callback should have fired"
        );
    }

    // ---- Test 8a: timer ids are unique per global across concurrent loops ----
    //
    // Two event loops driving the same global (the WASIp3 concurrent-invocation
    // shape) must not hand out colliding `setTimeout` ids: HTML's id namespace
    // is per-global, so a `clearTimeout` from one invocation must never resolve
    // to a different invocation's timer.
    {
        let el_a = EventLoop::new();
        let el_b = EventLoop::new();

        with_event_loop(&el_a, |_| {
            let ok = js::compile::evaluate_with_filename(
                &scope,
                "globalThis._idA = setTimeout(function() {}, 100000);",
                "<test-id-a>",
                1,
            );
            assert!(ok.is_ok(), "setTimeout in loop A failed");
        });
        with_event_loop(&el_b, |_| {
            let ok = js::compile::evaluate_with_filename(
                &scope,
                "globalThis._idB = setTimeout(function() {}, 100000);",
                "<test-id-b>",
                1,
            );
            assert!(ok.is_ok(), "setTimeout in loop B failed");
        });

        let read_id = |name: &str| -> i32 {
            let r = js::compile::evaluate_with_filename(&scope, name, "<test-id-read>", 1).unwrap();
            if r.is_int32() {
                r.to_int32()
            } else {
                r.to_double() as i32
            }
        };
        let id_a = read_id("globalThis._idA");
        let id_b = read_id("globalThis._idB");
        assert_ne!(
            id_a, id_b,
            "timer ids from concurrent event loops on the same global must be unique"
        );
    }

    // ---- Test 8b: setTimeout forwards extra arguments to the callback ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        {
            with_event_loop(&el, |_| {
                let ok = js::compile::evaluate_with_filename(
                    &scope,
                    "globalThis._timerArgs = null; \
                     setTimeout(function(a, b, c) { globalThis._timerArgs = [a, b, c].join(','); }, 1, 'x', 42, true);",
                    "<test-args>",
                    1,
                );
                assert!(ok.is_ok(), "setTimeout-with-args JS evaluation failed");
            });
        }

        block_on_event_loop(&scope, &mut el);

        let result = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._timerArgs",
            "<test-args-check>",
            1,
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        assert!(val.is_string(), "timer args should have been recorded");
        let s = js::JSString::from_value(&scope, val)
            .unwrap()
            .to_utf8(&scope)
            .unwrap();
        assert_eq!(
            s, "x,42,true",
            "extra setTimeout arguments must be forwarded"
        );
    }

    // ---- Test 8c: handler is converted before the timeout (WebIDL order) ----
    {
        let el = EventLoop::new();

        {
            with_event_loop(&el, |_| {
                let ok = js::compile::evaluate_with_filename(
                    &scope,
                    "globalThis._convOrder = []; \
                     globalThis._convTid = setTimeout(\
                       { toString() { globalThis._convOrder.push('handler'); return ''; } }, \
                       { valueOf() { globalThis._convOrder.push('timeout'); return 1e9; } });",
                    "<test-conv-order>",
                    1,
                );
                assert!(ok.is_ok(), "setTimeout with converting args failed");
            });
        }

        let result = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._convOrder.join(',')",
            "<test-conv-order-check>",
            1,
        );
        let val = result.unwrap();
        let s = js::JSString::from_value(&scope, val)
            .unwrap()
            .to_utf8(&scope)
            .unwrap();
        assert_eq!(
            s, "handler,timeout",
            "WebIDL requires left-to-right argument conversion"
        );

        // Drain the queued (far-future) timer so it can't leak into later tests.
        let cleared = {
            with_event_loop(&el, |_| {
                js::compile::evaluate_with_filename(
                    &scope,
                    "clearTimeout(globalThis._convTid)",
                    "<cleanup>",
                    1,
                )
                .is_ok()
            })
        };
        assert!(cleared);
        assert!(el.is_empty(), "cleanup clearTimeout should empty the loop");
    }

    // ---- Test 8d: a throwing interval callback keeps repeating ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        {
            with_event_loop(&el, |_| {
                let ok = js::compile::evaluate_with_filename(
                    &scope,
                    "globalThis._ivTicks = 0; \
                     globalThis._iv = setInterval(function() { \
                       globalThis._ivTicks++; \
                       if (globalThis._ivTicks === 1) { throw new Error('boom'); } \
                       if (globalThis._ivTicks >= 3) { clearInterval(globalThis._iv); } \
                     }, 1);",
                    "<test-throwing-interval>",
                    1,
                );
                assert!(ok.is_ok(), "setInterval JS evaluation failed");
            });
        }

        block_on_event_loop(&scope, &mut el);

        let result = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._ivTicks",
            "<test-throwing-interval-check>",
            1,
        );
        let val = result.unwrap();
        assert!(val.is_int32());
        assert_eq!(
            val.to_int32(),
            3,
            "a throwing callback must not kill the interval (spec: report + repeat)"
        );
    }

    // ---- Test 8e: JS timer ids are spec-shaped; clearing is timer-only ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();
        let counter = Rc::new(Cell::new(0));

        // An internal (non-JS) timer task on the same loop: clearTimeout must
        // not be able to reach it, whatever ids JS throws at it.
        el.queue_timer(
            Box::new(CounterTask {
                counter: Rc::clone(&counter),
                label: "internal",
            }),
            Instant::now(),
        );

        {
            with_event_loop(&el, |_| {
                let ok = js::compile::evaluate_with_filename(
                    &scope,
                    "globalThis._t1Fired = false; \
                     globalThis._tid1 = setTimeout(function() { globalThis._t1Fired = true; }, 1); \
                     if (globalThis._tid1 <= 0) throw new Error('timer id must be > 0, got ' + globalThis._tid1); \
                     clearTimeout(globalThis._tid1 + 1); \
                     globalThis._ivTicks2 = 0; \
                     globalThis._iv2 = setInterval(function() { \
                       globalThis._ivTicks2++; \
                       if (globalThis._ivTicks2 >= 2) clearInterval(globalThis._iv2); \
                     }, 1); \
                     clearTimeout(-0.5); clearTimeout(0); clearTimeout(999999); clearTimeout(-7); \
                     globalThis._t3Fired = false; \
                     globalThis._tid3 = setTimeout(function() { globalThis._t3Fired = true; }, 1); \
                     clearTimeout(String(globalThis._tid3));",
                    "<test-timer-ids>",
                    1,
                );
                assert!(ok.is_ok(), "timer-id JS evaluation failed");
            });
        }

        block_on_event_loop(&scope, &mut el);

        assert_eq!(
            counter.get(),
            1,
            "internal tasks must be unreachable from clearTimeout"
        );
        let result = js::compile::evaluate_with_filename(
            &scope,
            "[globalThis._t1Fired, globalThis._ivTicks2, globalThis._t3Fired].join(',')",
            "<test-timer-ids-check>",
            1,
        );
        let val = result.unwrap();
        let s = js::JSString::from_value(&scope, val)
            .unwrap()
            .to_utf8(&scope)
            .unwrap();
        assert_eq!(
            s, "true,2,false",
            "junk clears must be no-ops, a pre-cleared id must not poison a \
             later interval, and a string id must coerce and clear"
        );
    }

    // ---- Test 8f: expired timers fire in deadline order ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        {
            with_event_loop(&el, |_| {
                let ok = js::compile::evaluate_with_filename(
                    &scope,
                    // `late` is registered last so that its deadline follows the
                    // others' however long each `setTimeout` call itself takes.
                    "globalThis._fireOrder = []; \
                     setTimeout(function() { globalThis._fireOrder.push('early'); }, 10); \
                     globalThis._goner = setTimeout(function() { globalThis._fireOrder.push('goner'); }, 10); \
                     setTimeout(function() { globalThis._fireOrder.push('early2'); }, 10); \
                     setTimeout(function() { globalThis._fireOrder.push('late'); }, 40); \
                     clearTimeout(globalThis._goner);",
                    "<test-deadline-order>",
                    1,
                );
                assert!(ok.is_ok(), "deadline-order JS evaluation failed");
            });
        }

        // Let every deadline pass before the loop runs, so one advance_timers
        // call marks them all ready at once — the order must then come from
        // the deadlines, not from queue positions (which the clearTimeout's
        // swap_remove perturbed).
        std::thread::sleep(Duration::from_millis(60));
        block_on_event_loop(&scope, &mut el);

        let result = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._fireOrder.join(',')",
            "<test-deadline-order-check>",
            1,
        );
        let val = result.unwrap();
        let s = js::JSString::from_value(&scope, val)
            .unwrap()
            .to_utf8(&scope)
            .unwrap();
        assert_eq!(
            s, "early,early2,late",
            "expired timers must fire in deadline order, ties in scheduling \
             order, regardless of cancellations"
        );
    }

    // ---- Test 8g: interval arguments survive moving GC across re-queues ----
    // While a task runs it is out of the loop's task list and untraced, so the
    // re-queued interval must rebuild its argument Heaps from scope roots.
    // Compacting on every allocation makes a stale Heap fail deterministically.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        {
            with_event_loop(&el, |_| {
                let ok = js::compile::evaluate_with_filename(
                    &scope,
                    "globalThis._gcArgSeen = []; \
                     globalThis._gcIv = setInterval(function(tag) { \
                       globalThis._gcArgSeen.push(tag.name); \
                       if (globalThis._gcArgSeen.length >= 3) clearInterval(globalThis._gcIv); \
                     }, 1, { name: 'payload' });",
                    "<test-gc-args>",
                    1,
                );
                assert!(ok.is_ok(), "interval-with-args JS evaluation failed");
            });
        }
        #[cfg(feature = "debugmozjs")]
        unsafe {
            js::gc::SetGCZeal(scope.raw_cx_no_gc(), 14, 1)
        };
        block_on_event_loop(&scope, &mut el);
        #[cfg(feature = "debugmozjs")]
        unsafe {
            js::gc::SetGCZeal(scope.raw_cx_no_gc(), 0, 0)
        };

        let result = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._gcArgSeen.join(',')",
            "<test-gc-args-check>",
            1,
        );
        let val = result.unwrap();
        let s = js::JSString::from_value(&scope, val)
            .unwrap()
            .to_utf8(&scope)
            .unwrap();
        assert_eq!(
            s, "payload,payload,payload",
            "interval arguments must survive compacting GC between re-queues"
        );
    }

    // ---- Test 9: clearTimeout cancels a timer ----
    {
        let el = EventLoop::new();

        {
            with_event_loop(&el, |_| {
                let ok = js::compile::evaluate_with_filename(
                    &scope,
                    "globalThis._cleared = true; var tid = setTimeout(function() { globalThis._cleared = false; }, 1); clearTimeout(tid);",
                    "<test-clear>",
                    1,
                );
                assert!(ok.is_ok());
            });
        }

        // After clearTimeout, the event loop should be empty.
        assert!(el.is_empty(), "clearTimeout should have removed the timer");

        // Verify the callback did NOT run.
        let result = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._cleared",
            "<test-check-clear>",
            1,
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        assert!(
            val.is_boolean() && val.to_boolean(),
            "clearTimeout should prevent callback"
        );
    }

    // ---- Test 10: setInterval fires multiple times ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        {
            with_event_loop(&el, |_| {
                let ok = js::compile::evaluate_with_filename(
                    &scope,
                    r#"
                    globalThis._intervalCount = 0;
                    globalThis._intervalId = setInterval(function() {
                        globalThis._intervalCount++;
                        if (globalThis._intervalCount >= 3) {
                            clearInterval(globalThis._intervalId);
                        }
                    }, 1);
                    "#,
                    "<test-interval>",
                    1,
                );
                assert!(ok.is_ok());
            });
        }

        assert!(el.has_pending());

        block_on_event_loop(&scope, &mut el);

        // Verify the interval fired exactly 3 times and then stopped.
        let result = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._intervalCount",
            "<test-check-interval>",
            1,
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        let count = if val.is_int32() {
            val.to_int32()
        } else {
            val.to_double() as i32
        };
        assert_eq!(count, 3, "setInterval should have fired exactly 3 times");
    }

    // ---- Test 11: Event loop with microtasks (promise reactions) ----
    {
        // Evaluate JS that creates a resolved promise — the .then callback
        // should run during run_microtasks.
        let ok = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._promiseResult = 0; Promise.resolve(42).then(v => { globalThis._promiseResult = v; });",
            "<test-promise>",
            1,
        );
        assert!(ok.is_ok());

        // Run microtasks to process the promise.
        run_microtasks(&scope);

        let result = js::compile::evaluate_with_filename(
            &scope,
            "globalThis._promiseResult",
            "<test-check-promise>",
            1,
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        let n = if val.is_int32() {
            val.to_int32()
        } else {
            val.to_double() as i32
        };
        assert_eq!(n, 42, "Promise.resolve .then should have run");
    }

    // ---- Test 12: step() returns Done on empty event loop ----
    {
        let el = EventLoop::new();
        let outcome = el.step(&scope);
        assert_eq!(outcome, StepOutcome::Done);
    }

    // ---- Test 13: step() returns Progressed when tasks run ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "step-prog",
        }));

        let outcome = el.step(&scope);
        assert_eq!(outcome, StepOutcome::Progressed);
        assert_eq!(counter.get(), 1);
    }

    // ---- Test 14: step() returns Idle when tasks exist but none ready ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        // Queue a task but don't signal it ready.
        let _id = el.queue(Box::new(CounterTask {
            counter: counter.clone(),
            label: "step-idle",
        }));

        let outcome = el.step(&scope);
        assert_eq!(outcome, StepOutcome::Idle);
        assert_eq!(counter.get(), 0);
    }

    // ---- Test 15: step() returns Idle with interest but no tasks ----
    {
        let el = EventLoop::new();

        let interest = el.acquire_interest_handle();
        assert!(el.has_interest());
        assert!(el.is_alive());

        let outcome = el.step(&scope);
        assert_eq!(outcome, StepOutcome::Idle);

        interest.release();
        assert!(!el.has_interest());

        let outcome = el.step(&scope);
        assert_eq!(outcome, StepOutcome::Done);
    }

    // ---- Test 16: is_alive combines interest and pending ----
    {
        let el = EventLoop::new();

        assert!(!el.is_alive());

        // Interest alone keeps it alive.
        let interest = el.acquire_interest_handle();
        assert!(el.is_alive());
        drop(interest);
        assert!(!el.is_alive());

        // Pending tasks alone keep it alive.
        let counter = Rc::new(Cell::new(0u32));
        el.queue(Box::new(CounterTask {
            counter: counter.clone(),
            label: "alive-test",
        }));
        assert!(el.is_alive());
    }

    // ---- Test 17: step() runs timer tasks after advance ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        // Queue a timer with deadline in the past.
        el.queue_timer(
            Box::new(CounterTask {
                counter: counter.clone(),
                label: "step-timer",
            }),
            Instant::now(),
        );

        // step() advances timers internally.
        let outcome = el.step(&scope);
        assert_eq!(outcome, StepOutcome::Progressed);
        assert_eq!(counter.get(), 1);
        assert!(el.is_empty());
    }

    // ---- Test 18: run_to_completion respects interest tracking ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        // Acquire interest, queue a task that releases it (by dropping the handle).
        let interest = el.acquire_interest_handle();

        struct ReleaseInterestTask {
            interest: Option<core_runtime::event_loop::InterestHandle>,
        }
        impl Task for ReleaseInterestTask {
            fn kind(&self) -> &'static str {
                "release-interest"
            }
            fn run(
                mut self: Box<Self>,
                _scope: &Scope<'_>,
                _id: core_runtime::event_loop::TaskId,
            ) -> Result<(), ExnThrown> {
                drop(self.interest.take());
                Ok(())
            }
            fn trace(&self, _trc: *mut JSTracer) {}
        }

        el.queue_ready(Box::new(ReleaseInterestTask {
            interest: Some(interest),
        }));

        // With interest, the native driver should run the task and then
        // exit once interest drops to zero and no tasks remain.
        block_on_event_loop(&scope, &mut el);
        assert!(!el.has_interest());
        assert!(el.is_empty());
    }

    // ---- Test: a suspended loop runs no tasks ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));
        el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "suspended",
        }));

        core_runtime::event_loop::set_suspended(true);
        assert!(core_runtime::event_loop::is_suspended());
        assert_eq!(el.step(&scope), StepOutcome::Idle);
        assert_eq!(counter.get(), 0);

        core_runtime::event_loop::set_suspended(false);
        assert!(!core_runtime::event_loop::is_suspended());
        assert_eq!(el.step(&scope), StepOutcome::Progressed);
        assert_eq!(counter.get(), 1);

        // A suspended loop with nothing left reports Done.
        core_runtime::event_loop::set_suspended(true);
        assert_eq!(el.step(&scope), StepOutcome::Done);
        core_runtime::event_loop::set_suspended(false);
    }

    // ---- Test: run_until waits out a suspension ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));
        el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
            label: "run-until-suspended",
        }));
        // SAFETY: the scope (and its Runtime) outlives the future, which is driven to completion
        // on this thread below.
        let raw_cx = unsafe { scope.raw_cx_no_gc() };
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        tokio_rt.block_on(async {
            core_runtime::event_loop::set_suspended(true);
            let run = unsafe {
                core_runtime::event_loop::run_until(raw_cx, &el, tokio::time::sleep, |_| false)
            };
            let mut run = std::pin::pin!(run);
            assert!(futures_lite::future::poll_once(&mut run).await.is_none());
            assert_eq!(counter.get(), 0);

            core_runtime::event_loop::set_suspended(false);
            run.await;
            assert_eq!(counter.get(), 1);
        });
    }
}

#[test]
fn blocking_is_not_allowed_without_an_event_loop() {
    assert!(core_runtime::event_loop::blocking_allowed());
    core_runtime::event_loop::without_event_loop("a test reason", || {
        assert!(!core_runtime::event_loop::blocking_allowed());
    });
    assert!(core_runtime::event_loop::blocking_allowed());
}
