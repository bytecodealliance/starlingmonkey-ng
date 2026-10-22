// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for the event loop.

#[cfg(not(target_arch = "wasm32"))]
use core_runtime::event_loop::run_to_completion;
use core_runtime::event_loop::timer::install_timer_globals;
use core_runtime::event_loop::{with_event_loop, EventLoop, StepOutcome, Task};
use core_runtime::runtime::Runtime;
use js::gc::scope::Scope;

use js::error::ExnThrown;
use platform::clock::Instant;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// Evaluate `source`, panicking if it throws.
fn eval<'s>(scope: &'s Scope<'_>, source: &str) -> js::prelude::HandleValue<'s> {
    js::compile::evaluate(scope, source).expect("evaluation threw")
}

/// [`eval`] `source`, and convert the result to a string.
fn eval_string(scope: &Scope<'_>, source: &str) -> String {
    let value = eval(scope, source);
    js::conversion::FromJSVal::from_jsval(scope, value, ()).expect("not a string")
}

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

    // ---- Basic EventLoop operations ----
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
        }));

        assert!(el.has_pending());
        assert!(!el.has_ready());
        assert_eq!(el.len(), 1);

        // The task isn't ready yet, so `pop_ready` returns `None`.
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

    // ---- Cancel ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        let id = el.queue_ready(Box::new(CounterTask {
            counter: counter.clone(),
        }));

        assert!(el.cancel_if_queued(id));
        assert!(el.is_empty());
        assert_eq!(counter.get(), 0);

        // Cancel of non-existent ID returns false.
        assert!(!el.cancel_if_queued(id));
    }

    // ---- Timer tasks ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        // Queue a timer 5ms in the future.
        let _id = el.queue_timer(
            Box::new(CounterTask {
                counter: counter.clone(),
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

    // ---- timer ids are unique per global across concurrent loops ----
    //
    // Two event loops driving the same global (the WASIp3 concurrent-invocation
    // shape) share the global's id namespace, so their `setTimeout` ids must not
    // collide.
    {
        let el_a = EventLoop::new();
        let el_b = EventLoop::new();

        let start = || eval(&scope, "setTimeout(function() {}, 100000)").to_number();
        let id_a = with_event_loop(&el_a, |_| start());
        let id_b = with_event_loop(&el_b, |_| start());
        assert_ne!(
            id_a, id_b,
            "timer ids from concurrent event loops on the same global must be unique"
        );
    }

    // ---- another loop clears a loop's JS timers ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let owner = EventLoop::new();
        let other = EventLoop::new();
        let eval = |source: &str| eval(&scope, source);
        let start = |source: &str| with_event_loop(&owner, |_| eval(source)).to_number() as u64;

        let interval = start("setInterval(function() {}, 100000)");
        with_event_loop(&other, |_| eval(&format!("clearInterval({interval})")));
        assert!(
            owner.is_empty(),
            "clearInterval from another loop left the interval queued"
        );

        let timeout = start("setTimeout(function() {}, 100000)");
        with_event_loop(&other, |_| eval(&format!("clearTimeout({timeout})")));
        assert!(
            owner.is_empty(),
            "clearTimeout from another loop left the timeout queued"
        );

        // The loop that holds a timer is woken when another loop clears it: `run_to_completion`
        // returns rather than waiting for the cleared interval.
        let interval = start("setInterval(function() {}, 100000)");
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                // SAFETY: as in `block_on_event_loop`.
                let raw_cx = unsafe { scope.raw_cx_no_gc() };
                let drive = async {
                    unsafe { run_to_completion(raw_cx, &owner, tokio::time::sleep).await }
                };
                let clear = async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    with_event_loop(&other, |_| eval(&format!("clearInterval({interval})")));
                };
                tokio::time::timeout(
                    Duration::from_secs(10),
                    futures_lite::future::zip(drive, clear),
                )
                .await
                .expect("the loop that held a cleared interval kept waiting for it");
            });

        // A timer whose loop ended is ignored.
        let orphan = start("setTimeout(function() {}, 100000)");
        drop(owner);
        with_event_loop(&other, |el| el.clear_js_timer(&scope, orphan));
        assert!(other.is_empty());
    }

    // ---- another global's clearTimeout leaves a global's JS timers alone ----
    {
        let el = EventLoop::new();
        let timeout = with_event_loop(&el, |_| {
            eval(&scope, "setTimeout(function() {}, 100000)").to_number()
        });
        let interval = with_event_loop(&el, |_| {
            eval(&scope, "setInterval(function() {}, 100000)").to_number()
        });
        {
            let other_global = rt.new_global();
            with_event_loop(&el, |_| {
                eval(
                    &other_global,
                    &format!("clearTimeout({timeout}); clearInterval({interval})"),
                )
            });
        }
        assert_eq!(el.len(), 2, "another global cleared this global's timers");
        with_event_loop(&el, |_| {
            eval(
                &scope,
                &format!("clearTimeout({timeout}); clearInterval({interval})"),
            )
        });
        assert!(el.is_empty(), "the creating global's clears were ignored");
    }

    // ---- setTimeout forwards extra arguments to the callback ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        with_event_loop(&el, |_| {
            eval(
                &scope,
                "globalThis._timerArgs = null; \
                 setTimeout(function(a, b, c) { globalThis._timerArgs = [a, b, c].join(','); }, 1, 'x', 42, true);",
            );
        });

        block_on_event_loop(&scope, &mut el);

        let s = eval_string(&scope, "String(globalThis._timerArgs)");
        assert_eq!(
            s, "x,42,true",
            "extra setTimeout arguments must be forwarded"
        );
    }

    // ---- handler is converted before the timeout (WebIDL order) ----
    {
        let el = EventLoop::new();

        with_event_loop(&el, |_| {
            eval(
                &scope,
                "globalThis._convOrder = []; \
                 globalThis._convTid = setTimeout(\
                   { toString() { globalThis._convOrder.push('handler'); return ''; } }, \
                   { valueOf() { globalThis._convOrder.push('timeout'); return 1e9; } });",
            );
        });

        let s = eval_string(&scope, "globalThis._convOrder.join(',')");
        assert_eq!(
            s, "handler,timeout",
            "WebIDL requires left-to-right argument conversion"
        );

        // Drain the queued (far-future) timer so it can't leak into later tests.
        with_event_loop(&el, |_| eval(&scope, "clearTimeout(globalThis._convTid)"));
        assert!(el.is_empty(), "cleanup clearTimeout should empty the loop");
    }

    // ---- a throwing interval callback keeps repeating ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        with_event_loop(&el, |_| {
            eval(
                &scope,
                "globalThis._ivTicks = 0; \
                 globalThis._iv = setInterval(function() { \
                   globalThis._ivTicks++; \
                   if (globalThis._ivTicks === 1) { throw new Error('boom'); } \
                   if (globalThis._ivTicks >= 3) { clearInterval(globalThis._iv); } \
                 }, 1);",
            );
        });

        block_on_event_loop(&scope, &mut el);

        assert_eq!(
            eval_string(&scope, "String(globalThis._ivTicks)"),
            "3",
            "a throwing callback must not kill the interval (spec: report + repeat)"
        );
    }

    // ---- JS timer ids are spec-shaped, and clearing reaches only timers ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();
        let counter = Rc::new(Cell::new(0));

        // An internal (non-JS) timer task on the same loop: clearTimeout must
        // not be able to reach it, whatever ids JS throws at it.
        el.queue_timer(
            Box::new(CounterTask {
                counter: Rc::clone(&counter),
            }),
            Instant::now(),
        );

        with_event_loop(&el, |_| {
            eval(
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
            );
        });

        block_on_event_loop(&scope, &mut el);

        assert_eq!(
            counter.get(),
            1,
            "internal tasks must be unreachable from clearTimeout"
        );
        let s = eval_string(
            &scope,
            "[globalThis._t1Fired, globalThis._ivTicks2, globalThis._t3Fired].join(',')",
        );
        assert_eq!(
            s, "true,2,false",
            "junk clears must be no-ops, a pre-cleared id must not poison a \
             later interval, and a string id must coerce and clear"
        );
    }

    // ---- expired timers fire in deadline order ----
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        with_event_loop(&el, |_| {
            eval(
                &scope,
                // `late` is registered last so that its deadline follows the
                // others' however long each `setTimeout` call itself takes.
                "globalThis._fireOrder = []; \
                 setTimeout(function() { globalThis._fireOrder.push('early'); }, 1); \
                 globalThis._goner = setTimeout(function() { globalThis._fireOrder.push('goner'); }, 1); \
                 setTimeout(function() { globalThis._fireOrder.push('early2'); }, 1); \
                 setTimeout(function() { globalThis._fireOrder.push('late'); }, 2); \
                 clearTimeout(globalThis._goner);",
            );
        });

        // Let every deadline pass before the loop runs, so one `advance_timers`
        // call marks them all ready at once. The order then comes from the
        // deadlines, not from queue positions, which the `clearTimeout`'s
        // `swap_remove` perturbed.
        std::thread::sleep(Duration::from_millis(10));
        block_on_event_loop(&scope, &mut el);

        let s = eval_string(&scope, "globalThis._fireOrder.join(',')");
        assert_eq!(
            s, "early,early2,late",
            "expired timers must fire in deadline order, ties in scheduling \
             order, regardless of cancellations"
        );
    }

    // ---- interval arguments survive moving GC across re-queues ----
    // While a task runs it is out of the loop's task list and untraced, so the
    // re-queued interval must rebuild its argument Heaps from scope roots.
    // Compacting on every allocation makes a stale Heap fail deterministically.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut el = EventLoop::new();

        with_event_loop(&el, |_| {
            eval(
                &scope,
                "globalThis._gcArgSeen = []; \
                 globalThis._gcIv = setInterval(function(tag) { \
                   globalThis._gcArgSeen.push(tag.name); \
                   if (globalThis._gcArgSeen.length >= 3) clearInterval(globalThis._gcIv); \
                 }, 1, { name: 'payload' });",
            );
        });
        #[cfg(feature = "debugmozjs")]
        unsafe {
            js::gc::SetGCZeal(scope.raw_cx_no_gc(), 14, 1)
        };
        block_on_event_loop(&scope, &mut el);
        #[cfg(feature = "debugmozjs")]
        unsafe {
            js::gc::SetGCZeal(scope.raw_cx_no_gc(), 0, 0)
        };

        let s = eval_string(&scope, "globalThis._gcArgSeen.join(',')");
        assert_eq!(
            s, "payload,payload,payload",
            "interval arguments must survive compacting GC between re-queues"
        );
    }

    // ---- step() returns Idle when tasks exist but none ready ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));
        assert!(!el.is_alive());

        // Queue a task but don't signal it ready. The pending task keeps the loop alive.
        let _id = el.queue(Box::new(CounterTask {
            counter: counter.clone(),
        }));
        assert!(el.is_alive());

        let outcome = el.step(&scope);
        assert_eq!(outcome, StepOutcome::Idle);
        assert_eq!(counter.get(), 0);
    }

    // ---- step() returns Idle with interest but no tasks ----
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

    // ---- step() runs timer tasks after advance ----
    {
        let el = EventLoop::new();
        let counter = Rc::new(Cell::new(0u32));

        // Queue a timer with deadline in the past.
        el.queue_timer(
            Box::new(CounterTask {
                counter: counter.clone(),
            }),
            Instant::now(),
        );

        // step() advances timers internally.
        let outcome = el.step(&scope);
        assert_eq!(outcome, StepOutcome::Progressed);
        assert_eq!(counter.get(), 1);
        assert!(el.is_empty());
    }

    // ---- run_to_completion respects interest tracking ----
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
}
