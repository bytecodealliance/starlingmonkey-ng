// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for scope-based rooting (`RootScope`).

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;
use js::value;

#[test]
fn test_scope_rooting() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    // --- GC survival ---
    {
        // Root some values.
        let s = js::JSString::from_str(&scope, "survives gc").unwrap();
        let obj = js::Object::new(&scope, None).unwrap();
        let val = scope.root_value(value::from_i32(99));

        // Trigger GC.
        js::gc::prepare_for_full_gc(&scope);
        js::gc::maybe_gc(&scope);

        // Values should still be valid after GC.
        let result = s.to_utf8(&scope).unwrap();
        assert_eq!(result, "survives gc");
        assert!(!obj.as_raw().is_null());
        assert_eq!(val.get().to_int32(), 99);
    }

    // --- Many roots of the same type ---
    {
        // Root more strings than one 128-slot page of the root stack holds.
        let mut handles = Vec::new();
        for i in 0..130 {
            let s = js::JSString::from_str(&scope, &format!("string_{i}")).unwrap();
            handles.push(s);
        }

        // Trigger a compacting GC to verify all roots are traced.
        js::gc::non_incremental_gc(&scope, js::gc::GCOptions::Shrink, js::gc::GCReason::API);

        // Verify all strings survived.
        for (i, h) in handles.iter().enumerate() {
            let result = h.to_utf8(&scope).unwrap();
            assert_eq!(result, format!("string_{i}"));
        }
    }

    // --- Nested inner scopes ---
    {
        let s0 = js::JSString::from_str(&scope, "level0").unwrap();
        {
            let inner1 = scope.inner_scope();
            let s1 = js::JSString::from_str(&inner1, "level1").unwrap();
            {
                let inner2 = inner1.inner_scope();
                let s2 = js::JSString::from_str(&inner2, "level2").unwrap();

                // All levels accessible.
                assert_eq!(s0.to_utf8(&inner2).unwrap(), "level0");
                assert_eq!(s1.to_utf8(&inner2).unwrap(), "level1");
                assert_eq!(s2.to_utf8(&inner2).unwrap(), "level2");
            }
            // inner2 dropped, s2 released.

            // s0 and s1 still valid.
            assert_eq!(s0.to_utf8(&inner1).unwrap(), "level0");
            assert_eq!(s1.to_utf8(&inner1).unwrap(), "level1");
        }
        // inner1 dropped, s1 released.

        // s0 still valid.
        assert_eq!(s0.to_utf8(&scope).unwrap(), "level0");
    }

    // --- Interleaved parent/inner rooting with GC ---
    //
    // Rooting on the parent scope while an inner scope is alive must not let
    // dropping the inner scope release the parent's roots.
    {
        let inner = scope.inner_scope();

        let p1 = js::JSString::from_str(&scope, "parent-1").unwrap();
        let _i1 = js::JSString::from_str(&inner, "inner-1").unwrap();
        let p2 = js::JSString::from_str(&scope, "parent-2").unwrap();
        let _i2 = js::JSString::from_str(&inner, "inner-2").unwrap();
        let p3 = js::JSString::from_str(&scope, "parent-3").unwrap();

        drop(inner);

        // GC — all parent handles must survive.
        js::gc::prepare_for_full_gc(&scope);
        js::gc::maybe_gc(&scope);

        assert_eq!(p1.to_utf8(&scope).unwrap(), "parent-1");
        assert_eq!(p2.to_utf8(&scope).unwrap(), "parent-2");
        assert_eq!(p3.to_utf8(&scope).unwrap(), "parent-3");
    }
}
