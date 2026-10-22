// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Multi-global `ClassRegistry` tracing.
//!
//! The `js` crate traces every realm's class registry, no matter how many
//! globals a runtime creates. This test creates a second global and verifies
//! that the first realm's registered prototypes are still updated by a
//! compacting GC: the registry's prototype pointer must agree with the
//! JS-visible `Probe.prototype` (which normal GC tracing keeps current), and
//! Rust-side instance minting must still work.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]
#![cfg(feature = "debugmozjs")]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;
use core_runtime::{jsclass, jsmethods};
use js::gc::{self, GCOptions, GCReason, SetGCZeal};

#[jsclass]
struct Probe {
    tag: i32,
}

#[jsmethods]
impl Probe {
    #[constructor]
    fn construct() -> Self {
        Self { tag: 7 }
    }

    #[getter]
    fn tag(&self) -> i32 {
        self.data().tag
    }
}

#[test]
fn displaced_global_registry_survives_compacting_gc() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        Probe::add_to_global(scope, global);
    });
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");

    // Realm A: the initial default global.
    let scope_a = rt.default_global();

    // Realm B displaces A as the runtime's default global.
    drop(rt.new_global());

    // Provoke compacting GCs that relocate *all* arenas: zeal mode 14
    // (Compact) at frequency 1 turns every allocation into a DEBUG_GC-reason
    // compacting collection, and only DEBUG_GC GCs relocate unconditionally.
    // A's registry entries must be updated through the registry tracer.
    unsafe { SetGCZeal(scope_a.raw_cx_no_gc(), 14, 1) };
    {
        let inner = scope_a.inner_scope();
        for i in 0..8 {
            let s = js::JSString::from_str(&inner, &format!("mover_{i}")).unwrap();
            assert_eq!(s.to_utf8(&inner).unwrap(), format!("mover_{i}"));
        }
    }
    gc::prepare_for_full_gc(&scope_a);
    gc::non_incremental_gc(&scope_a, GCOptions::Shrink, GCReason::API);
    unsafe { SetGCZeal(scope_a.raw_cx_no_gc(), 0, 0) };

    // The registry's idea of `Probe.prototype` must agree with the
    // JS-visible one.
    let proto_reg = js::class::get_prototype_object_for::<ProbeImpl>(&scope_a)
        .expect("Probe prototype registered in realm A");
    let ctor = scope_a.global().get_property(&scope_a, c"Probe").unwrap();
    let ctor = js::Object::from_raw_value(&scope_a, ctor.get()).unwrap();
    let proto_js = ctor.get_property(&scope_a, c"prototype").unwrap();
    assert!(proto_js.is_object());
    assert_eq!(
        proto_reg.as_raw(),
        proto_js.to_object(),
        "realm A's registry prototype went stale across compacting GC"
    );

    // Rust-side minting in realm A goes through the registry prototype.
    let probe = js::class::create_instance_with::<ProbeImpl>(&scope_a, |_| ProbeImpl { tag: 9 })
        .expect("create_instance_with in realm A");
    assert_eq!(probe.data().tag, 9);
}
