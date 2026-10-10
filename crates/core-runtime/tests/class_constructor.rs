// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `js::class::class_constructor` returns the constructor the class was registered with.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;
use core_runtime::{jsclass, jsmethods};
use js::conversion::ToJSVal;

#[jsclass]
struct Registered {
    value: i32,
}

#[jsmethods]
impl Registered {
    #[constructor]
    fn construct() -> Self {
        Self { value: 1 }
    }
}

#[test]
fn class_constructor_ignores_a_replaced_prototype_constructor() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        Registered::add_to_global(scope, global);
    });
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let original = js::compile::evaluate(
        &scope,
        "const original = Registered;
         Registered.prototype.constructor = function Replaced() {};
         original",
    )
    .expect("the script runs");
    let ctor = js::class::class_constructor::<RegisteredImpl>(&scope).expect("registered");
    let ctor = ctor.to_jsval(&scope).expect("an object converts");
    assert!(js::comparison::same_value(&scope, ctor, original).expect("comparing succeeds"));
}
