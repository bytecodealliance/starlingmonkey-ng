// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Time limits for running JavaScript.
//!
//! [`with_time_limit`] runs a closure and terminates the JavaScript it runs once a time limit has
//! passed. A thread waits for the limit and requests an interrupt of the context, and the
//! interrupt callback [`install`] registers terminates the running script. On wasm, which has no
//! threads, no limit is ever reached.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::gc::scope::Scope;
use crate::native::RawJSContext;

crate::instance_local! {
    /// The limits of the [`with_time_limit`] calls that are running, the innermost last.
    static LIMITS: RefCell<Vec<Arc<Limit>>> = const { RefCell::new(Vec::new()) };
}

/// The state of the limit of a [`with_time_limit`] call.
#[derive(Default)]
struct Limit {
    /// Whether the limit has passed, which the timer thread sets.
    reached: AtomicBool,
    /// Whether the interrupt callback terminated JavaScript after the limit was reached.
    terminated: AtomicBool,
}

/// Register the interrupt callback that terminates the JavaScript running on `cx` once the limit
/// of a [`with_time_limit`] call that runs it has been reached. Call once per context, before
/// [`with_time_limit`] is called.
pub fn install(cx: &mut mozjs::context::JSContext) {
    // SAFETY: the callback is a valid function for the life of the process.
    let added = unsafe { mozjs::rust::wrappers2::JS_AddInterruptCallback(cx, Some(on_interrupt)) };
    assert!(
        added,
        "adding an interrupt callback can only fail due to OOM"
    );
}

/// The interrupt callback: `false`, which terminates the running JavaScript, if the limit of a
/// running [`with_time_limit`] call has been reached, and `true` otherwise. Marks each reached
/// limit as having terminated JavaScript.
unsafe extern "C" fn on_interrupt(_cx: *mut RawJSContext) -> bool {
    LIMITS.with(|limits| {
        let mut terminate = false;
        for limit in limits.borrow().iter() {
            if limit.reached.load(Ordering::SeqCst) {
                limit.terminated.store(true, Ordering::SeqCst);
                terminate = true;
            }
        }
        !terminate
    })
}

/// Run `f`, which runs JavaScript on the context of `scope`, and terminate that JavaScript once
/// `limit` has passed. Returns the result of `f` and whether reaching the limit terminated
/// JavaScript that `f` ran.
///
/// Terminated JavaScript fails without a pending exception, which every native function and
/// every enclosing `with_time_limit` call it unwinds through sees as an uncatchable error. With
/// nested calls, the JavaScript stops at the first limit reached, and only the call whose limit
/// was reached reports it. A limit reached while `f` runs no JavaScript, or after `f` finished,
/// has no effect.
pub fn with_time_limit<R>(scope: &Scope<'_>, limit: Duration, f: impl FnOnce() -> R) -> (R, bool) {
    let state = Arc::new(Limit::default());
    LIMITS.with(|limits| limits.borrow_mut().push(state.clone()));
    let timer = Timer::start(scope, limit, state.clone());
    let result = f();
    if let Some(timer) = timer {
        timer.stop();
    }
    LIMITS.with(|limits| {
        let mut limits = limits.borrow_mut();
        if let Some(index) = limits.iter().rposition(|entry| Arc::ptr_eq(entry, &state)) {
            limits.remove(index);
        }
    });
    (result, state.terminated.load(Ordering::SeqCst))
}

/// A thread that marks the limit of a [`with_time_limit`] call as reached and requests an interrupt
/// of its context once the limit has passed, unless it is stopped first.
#[cfg(not(target_family = "wasm"))]
struct Timer {
    stop: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

/// No timer runs on wasm, which has no threads.
#[cfg(target_family = "wasm")]
enum Timer {}

/// The address of a context, which the timer thread passes to `JS_RequestInterruptCallback`,
/// which may be called from another thread.
#[cfg(not(target_family = "wasm"))]
struct ContextAddress(*mut RawJSContext);

// SAFETY: the timer thread only passes the address to `JS_RequestInterruptCallback`.
#[cfg(not(target_family = "wasm"))]
unsafe impl Send for ContextAddress {}

impl Timer {
    /// Start a timer for the context of `scope`. Returns `None` if no thread could be started.
    #[cfg(not(target_family = "wasm"))]
    fn start(scope: &Scope<'_>, limit: Duration, state: Arc<Limit>) -> Option<Timer> {
        // SAFETY: the address is only used to request an interrupt, and the thread is joined
        // before `with_time_limit` returns, while the context is alive.
        let cx = ContextAddress(unsafe { scope.raw_cx_no_gc() });
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("js-time-limit".into())
            .spawn(move || {
                let cx = cx;
                if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(limit)
                {
                    state.reached.store(true, Ordering::SeqCst);
                    // SAFETY: the context is alive until this thread is joined.
                    unsafe { mozjs::jsapi::JS_RequestInterruptCallback(cx.0) };
                }
            })
            .ok()?;
        Some(Timer { stop, thread })
    }

    #[cfg(target_family = "wasm")]
    fn start(_scope: &Scope<'_>, _limit: Duration, _state: Arc<Limit>) -> Option<Timer> {
        None
    }

    /// Stop the timer, and wait for its thread to end.
    fn stop(self) {
        #[cfg(not(target_family = "wasm"))]
        {
            drop(self.stop);
            let _ = self.thread.join();
        }
        #[cfg(target_family = "wasm")]
        match self {}
    }
}
