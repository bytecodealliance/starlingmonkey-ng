// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Interest tracking for the event loop.
//!
//! An [`InterestTracker`] counts external "keep-alive" requests — things
//! that should prevent the event loop from exiting even when no tasks are
//! queued. Builtins acquire an [`InterestHandle`] when they start an
//! operation; dropping the handle releases the interest.
//!
//! The handle owns a reference to the acquiring loop's counter, so the
//! release always lands on the right loop no matter where the handle is
//! dropped. That matters in cases where multiple incoming events are handled with multiple event
//! loops, but sharing the same global object: a promise reaction holding a handle can run during
//! another event loop's turn, and a release routed through "the active loop" would decrement that
//! other loop's counter, hanging the acquiring loop and underflowing the active one.
//!
//! The event loop is considered alive when either the task queue has
//! pending work or the interest count is positive.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use event_listener::Event;

/// State shared between a loop's [`InterestTracker`] and its issued
/// [`InterestHandle`]s.
struct InterestState {
    count: Cell<u32>,
    /// The origin of each held handle acquired while origins were being recorded, by the key in
    /// its [`InterestHandle::origin`]. See [`js::stack::record_origins`].
    origins: RefCell<HashMap<u64, Rc<str>>>,
    next_origin_key: Cell<u64>,
    /// The owning loop's driver notification: a release may happen while the
    /// owning loop is parked in its await branch (the handle dropped during
    /// another loop's turn), and must wake it so it can observe `Done`.
    notify: Rc<Event>,
}

/// Tracks external keep-alive interest in one event loop.
///
/// The counter starts at zero; [`acquire_handle`](Self::acquire_handle)
/// increments it and returns a handle whose drop decrements it. The event
/// loop should keep running as long as `has_interest()` returns true (or
/// there are pending tasks).
pub struct InterestTracker {
    state: Rc<InterestState>,
}

impl InterestTracker {
    /// Create a tracker with zero interest. `notify` is the owning loop's
    /// driver notification, woken on each release.
    pub fn new(notify: Rc<Event>) -> Self {
        Self {
            state: Rc::new(InterestState {
                count: Cell::new(0),
                origins: RefCell::new(HashMap::new()),
                next_origin_key: Cell::new(0),
                notify,
            }),
        }
    }

    /// Register interest — the owning event loop stays alive until the
    /// returned handle is dropped.
    pub fn acquire_handle(&self) -> InterestHandle {
        let count = self.state.count.get();
        self.state
            .count
            .set(count.checked_add(1).expect("interest count overflow"));
        let origin = js::stack::current_origin().map(|origin| {
            let key = self.state.next_origin_key.get();
            self.state.next_origin_key.set(key + 1);
            self.state.origins.borrow_mut().insert(key, origin);
            key
        });
        InterestHandle {
            state: Rc::clone(&self.state),
            origin,
        }
    }

    /// The origins of the held handles acquired while origins were being recorded.
    pub fn origins(&self) -> Vec<Rc<str>> {
        self.state.origins.borrow().values().cloned().collect()
    }

    /// Returns `true` if at least one interest is held.
    pub fn has_interest(&self) -> bool {
        self.state.count.get() > 0
    }

    /// Returns the current interest count.
    pub fn count(&self) -> u32 {
        self.state.count.get()
    }
}

/// A keep-alive on the event loop it was acquired from. Dropping the handle
/// releases the interest on that loop, regardless of which loop (if any)
/// is active at the time, and wakes the loop's driver.
pub struct InterestHandle {
    state: Rc<InterestState>,
    /// The key of this handle's entry in [`InterestState::origins`], if it has one.
    origin: Option<u64>,
}

impl InterestHandle {
    /// Release the interest now. Equivalent to dropping the handle; spelled
    /// out for call sites where the release is the point.
    pub fn release(self) {}
}

impl Drop for InterestHandle {
    fn drop(&mut self) {
        let count = self.state.count.get();
        self.state.count.set(
            count
                .checked_sub(1)
                .expect("InterestHandle outlived its tracker's count"),
        );
        if let Some(key) = self.origin {
            self.state.origins.borrow_mut().remove(&key);
        }
        // Wake the owning loop's driver — interest may have dropped to zero
        // while it was parked.
        self.state.notify.notify(1);
    }
}

impl std::fmt::Debug for InterestTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterestTracker")
            .field("count", &self.count())
            .finish()
    }
}

// Nothing platform-specific in these tests, so skip them on wasm32.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn tracker() -> InterestTracker {
        InterestTracker::new(Rc::new(Event::new()))
    }

    #[test]
    fn acquire_and_release() {
        let tracker = tracker();
        assert_eq!(tracker.count(), 0);
        assert!(!tracker.has_interest());
        let first = tracker.acquire_handle();
        assert!(tracker.has_interest());
        assert_eq!(tracker.count(), 1);

        let second = tracker.acquire_handle();
        assert_eq!(tracker.count(), 2);

        first.release();
        assert_eq!(tracker.count(), 1);
        assert!(tracker.has_interest());

        drop(second);
        assert_eq!(tracker.count(), 0);
        assert!(!tracker.has_interest());
    }

    #[test]
    fn release_targets_its_own_tracker() {
        // A handle dropped while another tracker is "current" still decrements
        // its own tracker — the serve-mode cross-request case.
        let a = tracker();
        let b = tracker();
        let handle_a = a.acquire_handle();
        let _handle_b = b.acquire_handle();
        drop(handle_a);
        assert_eq!(a.count(), 0);
        assert_eq!(b.count(), 1);
    }
}
