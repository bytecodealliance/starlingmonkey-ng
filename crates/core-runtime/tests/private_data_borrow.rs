// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Private-data borrow tracking.
//!
//! Private data lives behind a raw pointer in a reserved slot, so the borrow
//! checker cannot see aliasing of it: `data()` and `data_mut()` both conjure a
//! reference from that pointer, and one JS object is reachable through any
//! number of copyable handles. The hazard is *reentrancy* — a `&mut self`
//! method borrows `this`, then code touches the same object's data again while
//! that borrow is live.
//!
//! `set_private` installs a per-object borrow flag (mirroring `RefCell`) that
//! `data()`/`data_mut()`/`get_this_data*` check, so an overlapping conflicting
//! borrow panics instead of aliasing. Note that we can't test the panic behavior,
//! since the test process is aborted completely because unwinding across the FFI
//! boundary doesn't work.
// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::test_util::eval_with_setup;
use core_runtime::{jsclass, jsmethods};

#[jsclass]
struct Borrowed {
    n: i32,
}

#[jsmethods]
impl Borrowed {
    #[constructor]
    fn construct() -> Self {
        Self { n: 0 }
    }

    /// Holds a `data_mut()` guard on `this`, then reads `other`'s data (a shared
    /// borrow). When JS passes the same object as both `this` and `other`
    /// (`o.combine(o)`), that shared borrow conflicts with the live mutable
    /// borrow on the same object.
    #[method]
    fn combine(&self, other: Borrowed<'_>) -> i32 {
        let mut me = self.data_mut();
        me.n += 1;
        me.n + other.data().n
    }
}

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        Borrowed::add_to_global(scope, global);
    });
}

#[test]
fn sequential_borrows_are_fine() {
    // Non-overlapping borrows (separate calls) never conflict; the second call
    // returns `a.n` after two increments.
    let result = eval_with_setup(
        setup,
        "let a = new Borrowed(); let b = new Borrowed(); a.combine(b); a.combine(b)",
    );
    assert_eq!(result, "2");
}
