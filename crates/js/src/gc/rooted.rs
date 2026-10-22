// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The registry of the heap cells [`RootedHeap`](super::handle::RootedHeap)s own, traced as roots
//! of every collection, minor ones included.
//!
//! The cells are kept in a [`Slab`], which reuses the slots of unregistered cells, and each
//! `RootedHeap` keeps the index of its slot. Registering and unregistering a cell take constant
//! time whatever order the cells are dropped in, and tracing walks the slots in order.

use std::cell::RefCell;
use std::ptr::NonNull;

use slab::Slab;

use crate::heap::Trace;
use crate::native::JSTracer;

/// This module's share of the crate's thread-local state. See [`crate::tls`].
pub(crate) struct RootedTls {
    /// The registered cells, by slot.
    slots: RefCell<Slab<NonNull<dyn Trace>>>,
}

impl RootedTls {
    pub(crate) const fn new() -> Self {
        Self {
            slots: RefCell::new(Slab::new()),
        }
    }
}

/// Register `cell` to be traced until [`unregister`] is called with the returned slot.
///
/// Returns `None` once the thread's state was destroyed, at thread exit on native targets. Nothing
/// can trace a cell then.
///
/// # Safety
///
/// `cell` must stay valid, at the same address, until it is unregistered.
pub(crate) unsafe fn register(cell: NonNull<dyn Trace>) -> Option<u32> {
    crate::tls::try_with(|tls| {
        let slot = tls.rooted.slots.borrow_mut().insert(cell);
        u32::try_from(slot).expect("too many rooted heap cells")
    })
}

/// Stop tracing the cell registered in `slot` by [`register`]. Does nothing once the thread's state
/// was destroyed.
pub(crate) fn unregister(slot: u32) {
    let _ = crate::tls::try_with(|tls| {
        tls.rooted.slots.borrow_mut().try_remove(slot as usize);
    });
}

/// Trace every registered cell.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer`, and this must run from a GC root tracer, with JS execution
/// paused.
pub(crate) unsafe fn trace(trc: *mut JSTracer) {
    crate::tls::with(|tls| {
        // `as_ptr` bypasses `RefCell` borrow tracking: GC runs with JS execution paused, and
        // registration never runs JS, so no borrow of the slots is in use.
        let slots = unsafe { &*tls.rooted.slots.as_ptr() };
        for (_, cell) in slots.iter() {
            // SAFETY: a registered cell stays valid until it is unregistered, and `trc` is valid
            // per this function's contract.
            unsafe { cell.as_ref().trace(trc) };
        }
    });
}
