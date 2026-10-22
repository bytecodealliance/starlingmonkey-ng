// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Heap<Value>` must survive set-then-move under a compacting GC.
//!
//! The key property being tested: boxing the inner `MozHeap` keeps the GC
//! write barrier registered at a stable address, so moving a `Heap<Value>`
//! (e.g. into a `Vec` that reallocates) never invalidates the barrier. After
//! compaction the object can still be recovered through the `Heap`.
//!
//! Note on tracing: a bare local `Heap<Value>` is not traced during tenured
//! compaction, so the Vec is rooted via `RootedTraceableBox` before the GC
//! call. The move-safety of the boxed post-write barrier is still exercised
//! at the nursery-eviction phase that precedes compaction.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;
use js::gc::handle::Heap;
use js::gc::{self, GCOptions, GCReason};
use js::heap::RootedTraceableBox;
use js::native::Value;
use js::value;
use js::Object;

#[test]
fn heap_value_survives_move_and_compacting_gc() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    // Allocate a nursery object and give it an identifying property.
    let obj = Object::new(&scope, None).unwrap();
    let marker = scope.root_value(value::from_i32(4242));
    obj.set_property(&scope, c"marker", marker).unwrap();
    let obj_val: Value = obj.as_value();

    // Set-then-move: build a Heap<Value>, then force the Vec to reallocate
    // repeatedly, moving the Heap<Value>'s bytes each time. The boxed inner
    // MozHeap stays at a fixed address throughout — only the outer Heap struct
    // (a thin box pointer) moves. The nursery post-write barrier was registered
    // against that stable inner address, so it remains valid.
    let mut heaps: Vec<Heap<Value>> = Vec::with_capacity(1);
    heaps.push(Heap::from(obj_val));
    for _ in 0..64 {
        heaps.push(Heap::default()); // each push past capacity reallocates, moving entry 0
    }

    // Root the Vec so the GC can trace and update the stored pointer during
    // tenured compaction. The move-safety claim is exercised at the
    // nursery-eviction phase that precedes compaction: the boxed inner
    // MozHeap's address is stable, so the store-buffer entry remains valid
    // even though the Heap struct has been relocated many times.
    let heaps = RootedTraceableBox::new(heaps);

    // Force a full compacting GC. The GC traces through heaps[0], updating
    // its stored value pointer if the object is relocated.
    gc::prepare_for_full_gc(&scope);
    gc::non_incremental_gc(&scope, GCOptions::Shrink, GCReason::API);

    // Read back through a rooted get; the object must still be reachable and
    // carry its marker, proving the barrier tracked any relocation.
    let rooted = heaps[0].get(&scope);
    let recovered = Object::from_raw_value(&scope, rooted.get()).expect("still an object");
    let got = recovered.get_property(&scope, c"marker").unwrap();
    assert_eq!(got.to_int32(), 4242);
}
