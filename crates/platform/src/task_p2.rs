// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The WASIp2 backend of [`task`](crate::task): a single-threaded executor whose reactor waits on
//! `wasi:io/poll` pollables.
//!
//! The executor's state lives for the whole instance, not for one [`block_on`] call. Futures that
//! wait on a pollable own it, so a future created during one export call, such as a `fetch` body
//! a script keeps in a global, can be polled again during a later one.
//!
//! [`block_on`] returns once its future and every task started with [`spawn`] have completed.
//! Tasks started with [`spawn_background`] don't hold it up. They make progress during every
//! [`block_on`] call, and their remaining work stays queued in between.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use async_task::Runnable;
use wasip2::io::poll::Pollable;

/// The executor's state. A wasm32-wasip2 instance has one thread, and its thread-locals live as
/// long as the instance.
struct Executor {
    /// Tasks woken and waiting to run.
    ready: RefCell<VecDeque<Runnable>>,
    /// The pollables that pending [`WaitFor`] futures wait on, keyed by the future's id.
    waiters: RefCell<HashMap<u64, Waiter>>,
    /// The id the next [`WaitFor`] gets.
    next_waiter: Cell<u64>,
    /// The number of tasks started with [`spawn`] that haven't completed yet.
    foreground: Cell<usize>,
    /// Whether a [`block_on`] call is running.
    running: Cell<bool>,
}

#[derive(Clone)]
struct Waiter {
    pollable: Rc<Pollable>,
    waker: Waker,
    /// Set once `wasi:io/poll.poll` reported `pollable` ready.
    fired: Rc<Cell<bool>>,
}

thread_local! {
    static EXECUTOR: Executor = Executor {
        ready: RefCell::new(VecDeque::new()),
        waiters: RefCell::new(HashMap::new()),
        next_waiter: Cell::new(0),
        foreground: Cell::new(0),
        running: Cell::new(false),
    };
}

fn schedule(runnable: Runnable) {
    EXECUTOR.with(|executor| executor.ready.borrow_mut().push_back(runnable));
}

/// Decrements [`Executor::foreground`] when a task started with [`spawn`] completes or is
/// dropped unfinished.
struct ForegroundGuard;

impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        EXECUTOR.with(|executor| executor.foreground.set(executor.foreground.get() - 1));
    }
}

/// Run `future` to completion on a task of its own. The [`block_on`] call it was started from, or
/// the next one if it was started outside of one, returns only once it has completed.
pub fn spawn(future: impl Future<Output = ()> + 'static) {
    EXECUTOR.with(|executor| executor.foreground.set(executor.foreground.get() + 1));
    let guard = ForegroundGuard;
    spawn_background(async move {
        future.await;
        drop(guard);
    });
}

/// Run `future` to completion on a task of its own, which makes progress during every
/// [`block_on`] call until it has completed. No [`block_on`] call waits for it.
pub fn spawn_background(future: impl Future<Output = ()> + 'static) {
    // SAFETY: `future` and its output are `'static`, and the instance has one thread, so the
    // non-`Send` future and the `schedule` function are only ever used on it.
    let (runnable, task) = unsafe { async_task::spawn_unchecked(future, schedule) };
    task.detach();
    runnable.schedule();
}

/// Run `future` and every task started with [`spawn`] to completion, waiting on the host for
/// readiness whenever none of them can make progress.
///
/// Panics when called from inside another `block_on` call, and when every unfinished task waits
/// on an event that no pollable can signal, since nothing could ever wake one of them again.
pub fn block_on<F: Future>(future: F) -> F::Output {
    EXECUTOR.with(|executor| {
        assert!(
            !executor.running.replace(true),
            "`block_on` was called from inside another `block_on` call"
        );
    });
    let output = Rc::new(RefCell::new(None));
    let root = {
        let output = Rc::clone(&output);
        async move {
            *output.borrow_mut() = Some(future.await);
        }
    };
    EXECUTOR.with(|executor| executor.foreground.set(executor.foreground.get() + 1));
    let guard = ForegroundGuard;
    let root = async move {
        root.await;
        drop(guard);
    };
    // SAFETY: the loop below runs until the root task has completed, so the task never outlives
    // `future`, which may borrow from the caller's frame. The instance has one thread.
    let (runnable, task) = unsafe { async_task::spawn_unchecked(root, schedule) };
    task.detach();
    runnable.schedule();

    loop {
        let round = EXECUTOR.with(|executor| executor.ready.borrow().len());
        for _ in 0..round {
            let Some(runnable) = EXECUTOR.with(|executor| executor.ready.borrow_mut().pop_front())
            else {
                break;
            };
            runnable.run();
        }
        let (ready, foreground, waiting) = EXECUTOR.with(|executor| {
            (
                !executor.ready.borrow().is_empty(),
                executor.foreground.get(),
                !executor.waiters.borrow().is_empty(),
            )
        });
        if foreground == 0 {
            break;
        }
        match (ready, waiting) {
            // Tasks kept waking each other: check for host events without blocking, so tasks
            // waiting on the host aren't starved.
            (true, true) => check_pollables(false),
            (true, false) => {}
            (false, true) => check_pollables(true),
            (false, false) => panic!(
                "every unfinished task waits on an event only another task can signal, and no \
                 task can run: the program is deadlocked"
            ),
        }
    }
    EXECUTOR.with(|executor| executor.running.set(false));
    let output = output.borrow_mut().take();
    output.expect("the root task completed")
}

/// Call `wasi:io/poll.poll` on every pollable a [`WaitFor`] waits on, and wake those reported
/// ready. If `block` isn't set, a pollable that is already ready is added, so the call returns
/// immediately.
fn check_pollables(block: bool) {
    let waiters: Vec<(u64, Waiter)> = EXECUTOR.with(|executor| {
        executor
            .waiters
            .borrow()
            .iter()
            .map(|(id, waiter)| (*id, waiter.clone()))
            .collect()
    });
    let immediate = (!block).then(|| wasip2::clocks::monotonic_clock::subscribe_duration(0));
    let mut targets: Vec<&Pollable> = waiters
        .iter()
        .map(|(_, waiter)| &*waiter.pollable)
        .collect();
    targets.extend(immediate.as_ref());
    for index in wasip2::io::poll::poll(&targets) {
        let Some((id, waiter)) = waiters.get(index as usize) else {
            continue;
        };
        waiter.fired.set(true);
        EXECUTOR.with(|executor| executor.waiters.borrow_mut().remove(id));
        waiter.waker.wake_by_ref();
    }
}

/// Wait until `pollable` is ready.
pub fn wait_for(pollable: Pollable) -> WaitFor {
    let id = EXECUTOR.with(|executor| {
        let id = executor.next_waiter.get();
        executor.next_waiter.set(id + 1);
        id
    });
    WaitFor {
        pollable: Rc::new(pollable),
        id,
        fired: Rc::new(Cell::new(false)),
        registered: false,
    }
}

/// The future [`wait_for`] returns. Dropping it drops the pollable.
pub struct WaitFor {
    pollable: Rc<Pollable>,
    id: u64,
    fired: Rc<Cell<bool>>,
    registered: bool,
}

impl Future for WaitFor {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.fired.get() {
            return Poll::Ready(());
        }
        // A pollable that is ready before the first wait needs no round trip through the reactor.
        if !self.registered && self.pollable.ready() {
            self.fired.set(true);
            return Poll::Ready(());
        }
        self.registered = true;
        let waiter = Waiter {
            pollable: Rc::clone(&self.pollable),
            waker: cx.waker().clone(),
            fired: Rc::clone(&self.fired),
        };
        EXECUTOR.with(|executor| executor.waiters.borrow_mut().insert(self.id, waiter));
        Poll::Pending
    }
}

impl Drop for WaitFor {
    fn drop(&mut self) {
        if self.registered {
            // Removing the entry drops the reactor's copy of the pollable, so the pollable is gone
            // once this returns. A pollable obtained from a stream must be gone before the stream
            // is dropped.
            EXECUTOR.with(|executor| executor.waiters.borrow_mut().remove(&self.id));
        }
    }
}
