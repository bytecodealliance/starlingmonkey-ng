// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Helpers shared by the native `component-model` test binaries.

#![allow(dead_code)]

use js::conversion::{FromJSVal, ToJSVal};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;

/// Force a full, non-incremental GC so a rooting bug surfaces as a crash under
/// `debugmozjs` rather than silently passing.
///
/// This collects unreachable objects but does not necessarily relocate
/// surviving ones. Relocation needs a compacting zeal mode, which
/// `imports_native` sets for the cases that depend on it.
pub fn force_gc(scope: &Scope<'_>) {
    js::gc::gc(scope, js::gc::GCReason::DEBUG_GC);
}

/// Build a rooted number value handle for use as a call argument.
pub fn num<'s>(scope: &'s Scope<'_>, n: i32) -> HandleValue<'s> {
    n.to_jsval(scope).unwrap()
}

/// Read a number out of a value (int32 or double).
pub fn as_number(v: Value) -> f64 {
    if v.is_int32() {
        v.to_int32() as f64
    } else if v.is_double() {
        v.to_double()
    } else {
        panic!("value is not a number");
    }
}

/// Convert a value to a Rust `String` via `ToString`.
pub fn as_string<'s>(scope: &'s Scope<'_>, v: impl ToJSVal<'s>) -> String {
    let v = v.to_jsval(scope).unwrap();
    String::from_jsval(scope, v, ()).expect("ToString succeeds")
}
