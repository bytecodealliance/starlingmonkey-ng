// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `#[derive(Traceable)]` on an enum traces each variant's non-`#[no_trace]` fields. A compacting
//! GC relocates the held object, so the `Heap` inside the variant must be traced for its stored
//! pointer to be updated.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;
use core_runtime::Traceable;
use js::gc::handle::Heap;
use js::gc::{self, GCOptions, GCReason};
use js::heap::RootedTraceableBox;
use js::native::Value;
use js::value;
use js::Object;

#[derive(Traceable)]
#[allow(dead_code)]
enum Slot {
    Empty,
    Held(Heap<Value>),
    Pair {
        left: Heap<Value>,
        right: Heap<Value>,
    },
}

/// A type with no `Trace` impl: a generic `Traceable` whose parameter appears
/// only behind `#[no_trace]` must not require its parameter to be `Trace`.
struct NotTraced;

/// A generic `Traceable` whose parameter `K` is used solely in a `#[no_trace]`
/// field. The derive must bound only the parameters in *traced* fields, so this
/// compiles for `K = NotTraced` (which is not `Trace`). If the derive
/// over-constrained every parameter with `Trace`, `Cache<NotTraced>` below would
/// fail to compile.
#[derive(Traceable)]
#[allow(dead_code)]
struct Cache<K> {
    #[no_trace]
    key: K,
    val: Heap<Value>,
}

// Requiring `Cache<NotTraced>: Trace` is the assertion: the derived impl is
// `impl<K> Trace for Cache<K>` (no `K: Trace`), so it holds even though
// `NotTraced` is not `Trace`. With an unconditional `K: Trace` bound this would
// fail to compile.
fn _assert_trace<T: js::heap::Trace>() {}
const _: fn() = || _assert_trace::<Cache<NotTraced>>();

#[test]
fn traceable_enum_traces_variant_fields() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    let obj = Object::new(&scope, None).unwrap();
    let marker = scope.root_value(value::from_i32(7777));
    obj.set_property(&scope, c"marker", marker).unwrap();

    let slot = RootedTraceableBox::new(Slot::Held(Heap::from(obj.as_value())));

    // Force a compacting GC that relocates the held object. The derived trace
    // must update the `Heap` inside `Slot::Held` to the new location.
    gc::prepare_for_full_gc(&scope);
    gc::non_incremental_gc(&scope, GCOptions::Shrink, GCReason::API);

    match &*slot {
        Slot::Held(h) => {
            let rooted = h.get(&scope);
            let recovered = Object::from_raw_value(&scope, rooted.get()).expect("still an object");
            let got = recovered.get_property(&scope, c"marker").unwrap();
            assert_eq!(got.to_int32(), 7777);
        }
        _ => panic!("wrong variant"),
    }
}
