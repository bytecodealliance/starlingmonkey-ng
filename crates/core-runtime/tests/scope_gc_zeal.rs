// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! GC zeal tests for scope-based rooting (`HandlePool` + `RootScope`).
//!
//! These tests verify that pool-rooted values survive all GC modes including
//! compaction (which moves objects), generational/nursery collections, and
//! incremental marking.
//!
//! Each section sets its own GC zeal mode.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]
#![cfg(feature = "debugmozjs")]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;
use js::gc::{GCOptions, GCReason, SetGCZeal};
use js::value;

use js::gc;
use js::Array;
use js::Map;
use js::Object;

/// Helper: set GC zeal mode on the scope's context.
///
/// # Safety
///
/// Must only be called when a valid JSContext is available.
unsafe fn set_zeal(scope: &js::gc::scope::Scope<'_>, mode: u8, frequency: u32) {
    SetGCZeal(scope.raw_cx_no_gc(), mode, frequency);
}

/// Helper: reset GC zeal to mode 0 (normal).
unsafe fn reset_zeal(scope: &js::gc::scope::Scope<'_>) {
    SetGCZeal(scope.raw_cx_no_gc(), 0, 0);
}

#[test]
fn test_scope_gc_zeal() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    unsafe {
        // =====================================================================
        // Verification #5: roots in repeated inner scopes stay valid
        // =====================================================================
        // Pages are reused when cursor rewinds. After the first iteration
        // allocates a page, subsequent iterations should reuse it.
        {
            for i in 0..3 {
                let inner = scope.inner_scope();
                let s = js::JSString::from_str(&inner, &format!("scope_{i}")).unwrap();
                let obj = Object::new(&inner, None).unwrap();
                let val = inner.root_value(value::from_i32(i));

                // Verify roots are valid.
                let result = s.to_utf8(&inner).unwrap();
                assert_eq!(result, format!("scope_{i}"));
                assert!(!obj.as_raw().is_null());
                assert_eq!(val.get().to_int32(), i);
                // inner scope drops here — cursor rewinds
            }
        }

        // =====================================================================
        // Verification #6: roots across nested scopes + GC
        // =====================================================================
        {
            // Root 3 strings in the outer scope.
            let mut outer_handles = Vec::new();
            for i in 0..3 {
                let s = js::JSString::from_str(&scope, &format!("root_{i}")).unwrap();
                outer_handles.push((i, s));
            }

            {
                let inner = scope.inner_scope();
                // Root 130 strings in the inner scope, more than one 128-slot page holds.
                let mut inner_handles = Vec::new();
                for i in 3..133 {
                    let s = js::JSString::from_str(&inner, &format!("root_{i}")).unwrap();
                    inner_handles.push((i, s));
                }

                {
                    let inner2 = inner.inner_scope();
                    // Root 10 strings in the innermost scope.
                    let mut inner2_handles = Vec::new();
                    for i in 133..143 {
                        let s = js::JSString::from_str(&inner2, &format!("root_{i}")).unwrap();
                        inner2_handles.push((i, s));
                    }

                    // Trigger GC with all 143 roots live.
                    gc::prepare_for_full_gc(&inner2);
                    gc::non_incremental_gc(&inner2, GCOptions::Normal, GCReason::API);

                    // All 143 should survive.
                    for (i, h) in &outer_handles {
                        let result = h.to_utf8(&inner2).unwrap();
                        assert_eq!(result, format!("root_{i}"));
                    }
                    for (i, h) in &inner_handles {
                        let result = h.to_utf8(&inner2).unwrap();
                        assert_eq!(result, format!("root_{i}"));
                    }
                    for (i, h) in &inner2_handles {
                        let result = h.to_utf8(&inner2).unwrap();
                        assert_eq!(result, format!("root_{i}"));
                    }
                }
                // inner2 drops, releasing roots 133-142.

                // inner + outer roots still valid.
                for (i, h) in &inner_handles {
                    let result = h.to_utf8(&inner).unwrap();
                    assert_eq!(result, format!("root_{i}"));
                }
            }
            // inner drops, releasing roots 3-132.

            // Roots 0-2 still valid.
            for (i, h) in &outer_handles {
                let result = h.to_utf8(&scope).unwrap();
                assert_eq!(result, format!("root_{i}"));
            }
        }

        // =====================================================================
        // GC Zeal #8: Mode 1 (RootsChange) — GC on root set changes
        // =====================================================================
        {
            set_zeal(&scope, 1, 1);

            let inner = scope.inner_scope();

            // Each root_* call modifies the root set, potentially triggering GC.
            let s1 = js::JSString::from_str(&inner, "roots_change_1").unwrap();
            let obj1 = Object::new(&inner, None).unwrap();
            let s2 = js::JSString::from_str(&inner, "roots_change_2").unwrap();
            let v1 = inner.root_value(value::from_i32(42));
            let obj2 = Object::new(&inner, None).unwrap();

            // All roots must survive despite GC on every root set change.
            assert_eq!(s1.to_utf8(&inner).unwrap(), "roots_change_1");
            assert_eq!(s2.to_utf8(&inner).unwrap(), "roots_change_2");
            assert!(!obj1.as_raw().is_null());
            assert!(!obj2.as_raw().is_null());
            assert_eq!(v1.get().to_int32(), 42);

            reset_zeal(&inner);
        }

        // =====================================================================
        // GC Zeal #9: Mode 2 (Alloc) — GC at every allocation
        // =====================================================================
        {
            set_zeal(&scope, 2, 1);

            let inner = scope.inner_scope();

            // Root many values — GC fires on every JS allocation.
            let mut string_handles = Vec::new();
            for i in 0..5 {
                let s = js::JSString::from_str(&inner, &format!("alloc_{i}")).unwrap();
                string_handles.push(s);
            }

            // Evaluate JS that allocates heavily.
            let rval = js::compile::evaluate(
                &inner,
                "var arr = []; for (var i = 0; i < 5; i++) arr.push({x: i}); arr.length",
            )
            .unwrap();
            assert_eq!(rval.to_int32(), 5);

            // All string roots must survive.
            for (i, h) in string_handles.iter().enumerate() {
                let result = h.to_utf8(&inner).unwrap();
                assert_eq!(result, format!("alloc_{i}"));
            }

            reset_zeal(&inner);
        }

        // =====================================================================
        // GC Zeal #10: Mode 4 (VerifierPre) — verify pre-write barriers
        // =====================================================================
        {
            set_zeal(&scope, 4, 1);

            let inner = scope.inner_scope();

            // Root objects and modify their properties. The verifier checks
            // that pre-write barriers fire correctly.
            let obj = Object::new(&inner, None).unwrap();
            let val = inner.root_value(value::from_i32(100));
            obj.set_property(&inner, c"x", val).unwrap();

            let obj2 = Object::new(&inner, None).unwrap();
            let val2 = inner.root_value(value::from_i32(200));
            obj2.set_property(&inner, c"y", val2).unwrap();

            // Overwrite properties to trigger barriers.
            let val3 = inner.root_value(value::from_i32(300));
            obj.set_property(&inner, c"x", val3).unwrap();

            // Read back to verify.
            let got = obj.get_property(&inner, c"x").unwrap();
            assert_eq!(got.to_int32(), 300);
            let got2 = obj2.get_property(&inner, c"y").unwrap();
            assert_eq!(got2.to_int32(), 200);

            reset_zeal(&inner);
        }

        // =====================================================================
        // GC Zeal #11: Mode 7 (GenerationalGC) — frequent nursery collections
        // =====================================================================
        // Nursery-allocated objects must survive and handles must remain valid
        // after tenuring.
        {
            set_zeal(&scope, 7, 1);

            let inner = scope.inner_scope();

            // Allocate objects that start in the nursery.
            let mut objects = Vec::new();
            for i in 0..20 {
                let obj = Object::new(&inner, None).unwrap();
                let val = inner.root_value(value::from_i32(i));
                obj.set_property(&inner, c"idx", val).unwrap();
                objects.push(obj);
            }

            // Allocate more to trigger nursery collections.
            let mut strings = Vec::new();
            for i in 0..20 {
                let s = js::JSString::from_str(&inner, &format!("nursery_{i}")).unwrap();
                strings.push(s);
            }

            // All objects should survive nursery → tenured promotion.
            for (i, obj) in objects.iter().enumerate() {
                let got = obj.get_property(&inner, c"idx").unwrap();
                assert_eq!(got.to_int32(), i as i32, "object {i} property mismatch");
            }

            // All strings should survive.
            for (i, s) in strings.iter().enumerate() {
                let result = s.to_utf8(&inner).unwrap();
                assert_eq!(result, format!("nursery_{i}"));
            }

            reset_zeal(&inner);
        }

        // =====================================================================
        // GC Zeal #12: Mode 11 (IncrementalMarkingValidator) — verify
        // incremental marking
        // =====================================================================
        {
            set_zeal(&scope, 11, 1);

            let inner = scope.inner_scope();

            // Root values during operations that trigger incremental GC slices.
            let mut values = Vec::new();
            for i in 0..30 {
                let val = inner.root_value(value::from_i32(i));
                values.push(val);
            }

            let mut objects = Vec::new();
            for _ in 0..20 {
                let obj = Object::new(&inner, None).unwrap();
                objects.push(obj);
            }

            // Trigger JS execution that allocates (may trigger incremental
            // slices with validation).
            let rval = js::compile::evaluate(
                &inner,
                "var result = 0; for (var i = 0; i < 50; i++) result += i; result",
            )
            .unwrap();
            assert_eq!(rval.to_int32(), 1225);

            // All roots must be consistently marked.
            for (i, v) in values.iter().enumerate() {
                assert_eq!(v.get().to_int32(), i as i32);
            }
            for obj in &objects {
                assert!(!obj.as_raw().is_null());
            }

            reset_zeal(&inner);
        }

        // =====================================================================
        // GC Zeal #13: Mode 14 (Compact) — force compacting GC
        // =====================================================================
        // **MOST CRITICAL**: Compaction moves objects, and our pool stores raw
        // pointers that must be updated by the GC tracer.
        {
            set_zeal(&scope, 14, 1);

            let inner = scope.inner_scope();

            // Create objects and arrays that may be moved by compaction.
            let mut objects = Vec::new();
            for i in 0..5 {
                let obj = Object::new(&inner, None).unwrap();
                let val = inner.root_value(value::from_i32(i * 10));
                obj.set_property(&inner, c"val", val).unwrap();
                objects.push(obj);
            }

            let arr = Array::new(&inner, 5).unwrap();
            let map = Map::new(&inner).unwrap();

            // Trigger operations that cause allocations (and thus compacting
            // GCs with zeal mode 14).
            for i in 0..5 {
                let s = js::JSString::from_str(&inner, &format!("compact_{i}")).unwrap();
                let result = s.to_utf8(&inner).unwrap();
                assert_eq!(result, format!("compact_{i}"));
            }

            // Force a full compacting GC.
            gc::prepare_for_full_gc(&inner);
            gc::non_incremental_gc(&inner, GCOptions::Shrink, GCReason::API);

            // After compaction, all pointer roots must still be valid.
            // If the GC moved objects and our tracer didn't update the pool
            // slots, these accesses would crash or return wrong values.
            for (i, obj) in objects.iter().enumerate() {
                assert!(!obj.as_raw().is_null());
                let got = obj.get_property(&inner, c"val").unwrap();
                assert_eq!(
                    got.to_int32(),
                    (i as i32) * 10,
                    "object {i} property mismatch after compaction"
                );
            }

            // Array and Map should still be valid.
            assert!(!arr.as_raw().is_null());
            assert!(!map.as_raw().is_null());

            // Array::with_contents roots its value copies: the array
            // allocation compacts (zeal 14), and the nursery-fresh string
            // value must be read from the traced buffer, not a stale copy.
            {
                let s = js::JSString::from_str(&inner, "compact_contents").unwrap();
                let sv = inner.root_value(s.as_value());
                let iv = inner.root_value(value::from_i32(7));
                let arr = Array::with_contents(&inner, &[sv, iv]).unwrap();
                assert_eq!(arr.length(&inner).unwrap(), 2);
                let got = arr.get_element(&inner, 0).unwrap();
                let got_str = js::JSString::from_non_null(
                    &inner,
                    std::ptr::NonNull::new(got.to_string()).unwrap(),
                );
                assert_eq!(got_str.to_utf8(&inner).unwrap(), "compact_contents");
                assert_eq!(arr.get_element(&inner, 1).unwrap().to_int32(), 7);
            }

            // Each `&str` element allocates a fresh string when converted, so the
            // strings converted first must already be traced while the later
            // ones allocate.
            {
                let words = ["alpha", "beta", "gamma", "delta"];
                let arr = Array::with_contents(&inner, &words).unwrap();
                for (i, word) in words.iter().enumerate() {
                    let got = arr.get_element(&inner, i as u32).unwrap();
                    let got = js::JSString::from_value(&inner, got).unwrap();
                    assert_eq!(got.to_utf8(&inner).unwrap(), *word);
                }
            }

            reset_zeal(&inner);
        }

        // =====================================================================
        // GC Zeal #14: Mode 2 + nested scopes
        // =====================================================================
        // Root in both parent and child, GC, drop child, GC again, verify
        // parent roots survive.
        {
            set_zeal(&scope, 2, 1);

            let inner = scope.inner_scope();

            // Root in parent scope.
            let parent_obj = Object::new(&inner, None).unwrap();
            let parent_val = inner.root_value(value::from_i32(111));
            parent_obj
                .set_property(&inner, c"source", parent_val)
                .unwrap();

            {
                // Root in child scope.
                let child = inner.inner_scope();
                let child_obj = Object::new(&child, None).unwrap();
                let child_val = child.root_value(value::from_i32(222));
                child_obj
                    .set_property(&child, c"source", child_val)
                    .unwrap();

                // GC with both scopes live.
                gc::prepare_for_full_gc(&child);
                gc::non_incremental_gc(&child, GCOptions::Normal, GCReason::API);

                // Both should survive.
                let got = parent_obj.get_property(&child, c"source").unwrap();
                assert_eq!(got.to_int32(), 111);
                let got = child_obj.get_property(&child, c"source").unwrap();
                assert_eq!(got.to_int32(), 222);
            }
            // Child scope dropped.

            // GC again — only parent roots should be traced.
            gc::prepare_for_full_gc(&inner);
            gc::non_incremental_gc(&inner, GCOptions::Normal, GCReason::API);

            // Parent roots must still be valid.
            let got = parent_obj.get_property(&inner, c"source").unwrap();
            assert_eq!(got.to_int32(), 111);

            reset_zeal(&inner);
        }

        // =====================================================================
        // GC Zeal #15: Mode 2 loop stress test
        // =====================================================================
        // 5 iterations: create inner scope, root, trigger GC, drop scope.
        {
            set_zeal(&scope, 2, 1);

            // Root something in the outer scope to verify it survives
            // all the inner scope churn.
            let outer_str = js::JSString::from_str(&scope, "outer_survivor").unwrap();

            for i in 0..5 {
                let inner = scope.inner_scope();

                let s = js::JSString::from_str(&inner, &format!("loop_{i}")).unwrap();
                let obj = Object::new(&inner, None).unwrap();
                let val = inner.root_value(value::from_i32(i));
                obj.set_property(&inner, c"i", val).unwrap();

                // Trigger GC inside the loop.
                gc::prepare_for_full_gc(&inner);
                gc::non_incremental_gc(&inner, GCOptions::Normal, GCReason::API);

                // Verify inner roots.
                let result = s.to_utf8(&inner).unwrap();
                assert_eq!(result, format!("loop_{i}"));
                let got = obj.get_property(&inner, c"i").unwrap();
                assert_eq!(got.to_int32(), i);

                // Outer root must also survive.
                let outer = outer_str.to_utf8(&inner).unwrap();
                assert_eq!(outer, "outer_survivor");
                // inner scope drops
            }

            // Final check of outer root.
            let outer = outer_str.to_utf8(&scope).unwrap();
            assert_eq!(outer, "outer_survivor");

            reset_zeal(&scope);
        }
    }
}

/// Roots survive a compacting GC when a parent scope roots values while a nested scope is alive,
/// when scopes drop in a different order than they were created in, and when a scope's roots
/// span several pages of the root stack.
#[test]
fn scope_roots_survive_nesting_out_of_order_drops_and_page_spans() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let compact = |s: &js::gc::scope::Scope<'_>| {
        gc::prepare_for_full_gc(s);
        gc::non_incremental_gc(s, GCOptions::Shrink, GCReason::API);
    };

    // The parent roots a value while a nested scope is alive, and again after it dropped.
    let before = js::JSString::from_str(&scope, "before").unwrap();
    let during;
    {
        let inner = scope.inner_scope();
        let inner_value = js::JSString::from_str(&inner, "inner").unwrap();
        during = js::JSString::from_str(&scope, "during").unwrap();
        compact(&inner);
        assert_eq!(inner_value.to_utf8(&inner).unwrap(), "inner");
    }
    let after = js::JSString::from_str(&scope, "after").unwrap();
    compact(&scope);
    assert_eq!(before.to_utf8(&scope).unwrap(), "before");
    assert_eq!(during.to_utf8(&scope).unwrap(), "during");
    assert_eq!(after.to_utf8(&scope).unwrap(), "after");

    // A scope drops while a scope created after it is still alive.
    // SAFETY: a realm is entered, and both scopes drop before `scope` is used again.
    unsafe {
        let raw_cx = scope.raw_cx_no_gc();
        let first = js::gc::scope::RootScope::from_current_realm(raw_cx);
        let second = js::gc::scope::RootScope::from_current_realm(raw_cx);
        let first_value = js::JSString::from_str(&first, "first").unwrap();
        let second_value = js::JSString::from_str(&second, "second").unwrap();
        assert_eq!(first_value.to_utf8(&first).unwrap(), "first");
        drop(first);
        compact(&second);
        assert_eq!(second_value.to_utf8(&second).unwrap(), "second");
    }
    let later = js::JSString::from_str(&scope, "later").unwrap();
    compact(&scope);
    assert_eq!(later.to_utf8(&scope).unwrap(), "later");
    assert_eq!(after.to_utf8(&scope).unwrap(), "after");

    // More roots than fit in one page of the root stack.
    let inner = scope.inner_scope();
    let values: Vec<_> = (0..130)
        .map(|i| js::JSString::from_str(&inner, &format!("value {i}")).unwrap())
        .collect();
    compact(&inner);
    for (i, value) in values.iter().enumerate() {
        assert_eq!(value.to_utf8(&inner).unwrap(), format!("value {i}"));
    }
}
