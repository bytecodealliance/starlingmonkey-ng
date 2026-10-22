/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
//@rustc-env:RUSTC_BOOTSTRAP=1

use std::collections::HashMap;

#[crown::unrooted_must_root_lint::must_root]
struct Foo(i32);

#[crown::unrooted_must_root_lint::allow_unrooted_interior]
struct Holder(HashMap<u32, Foo>);

// `HashMap::values` borrows the values, like `HashMap::iter`.
fn sum(holder: &Holder) -> i32 {
    let mut total = 0;
    for foo in holder.0.values() {
        total += foo.0;
    }
    total
}

fn main() {
    let _ = sum;
}
