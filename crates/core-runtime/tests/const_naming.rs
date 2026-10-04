// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Constants keep their declared Rust name in every macro that exposes them.
//!
//! Functions are camelCased (`safe_divide` → `safeDivide`), but constants are
//! not: `PI` stays `PI`. JS spells constants in SCREAMING_CASE too, so the Rust
//! name is already the right one, and camelCasing it produced names like `pi`
//! and `maxValue` that matched neither convention.
//!
//! `#[jsclass]`/`#[webidl_interface]` constants have always behaved this way
//! (see `typedef_macros::constant_tests`); these tests pin the same rule for
//! `#[jsmodule]`, `#[jsglobals]`, `#[jsnamespace]`, and `#[webidl_namespace]`.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::module::evaluate_module;
use core_runtime::runtime::Runtime;
use core_runtime::test_util::eval_with_setup;
use core_runtime::{jsglobals, jsmodule, jsnamespace, webidl_namespace};
use js::conversion::FromJSVal;

#[jsmodule]
mod const_module {
    pub const PI: f64 = 3.5;
    pub const MAX_VALUE: f64 = 1000.0;

    pub fn safe_divide(a: f64, b: f64) -> f64 {
        a / b
    }
}

#[jsglobals]
mod const_globals {
    pub const APP_NAME: &str = "starling";
    pub const MAX_RETRIES: i32 = 3;

    pub fn format_name(name: String) -> String {
        name
    }
}

#[jsnamespace(name = "constNs")]
mod const_ns {
    pub const MEDIA_ERR: i32 = 4;

    pub fn identity_value(v: f64) -> f64 {
        v
    }
}

#[webidl_namespace(name = "ConstWebIDL")]
mod const_webidl_ns {
    pub const HIERARCHY_REQUEST_ERR: i32 = 3;

    pub fn escape_value(v: String) -> String {
        v
    }
}

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        const_globals::add_to_global(scope, global);
        const_ns::add_to_global(scope, global);
        const_webidl_ns::add_to_global(scope, global);
        const_module::register(scope);
    });
}

fn eval(code: &str) -> String {
    eval_with_setup(setup, code)
}

/// Evaluate a module that imports from `const_module` (specifier `constModule`)
/// and assigns to `globalThis._result`, then read that value back as a string.
///
/// `import()` can't be used from `eval_with_setup` — it returns a promise the
/// helper would stringify as `[object Promise]` — so this drives a static
/// import through `evaluate_module` the way `starling/examples/native_module.rs`
/// does.
fn eval_module(body: &str) -> String {
    setup();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let source = format!("import * as m from \"constModule\";\nglobalThis._result = {body};");
    // SAFETY: `scope` outlives the evaluation, and the module registry was
    // populated by `setup` before any JS ran.
    unsafe { evaluate_module(&scope, &source, "const_test.mjs") }.expect("module eval failed");
    let val = js::compile::evaluate_with_filename(&scope, "globalThis._result", "read.js", 1)
        .expect("readback failed");
    String::from_jsval(&scope, val, ()).expect("null string")
}

// ============================================================================
// #[jsmodule]
// ============================================================================

#[test]
fn module_const_keeps_declared_name() {
    assert_eq!(eval_module("m.PI"), "3.5");
}

#[test]
fn module_multiword_const_keeps_declared_name() {
    assert_eq!(eval_module("m.MAX_VALUE"), "1000");
}

#[test]
fn module_camel_cased_const_name_is_absent() {
    assert_eq!(
        eval_module("typeof m.pi + ',' + typeof m.maxValue"),
        "undefined,undefined"
    );
}

#[test]
fn module_function_is_still_camel_cased() {
    assert_eq!(eval_module("m.safeDivide(10, 4)"), "2.5");
}

// ============================================================================
// #[jsglobals]
// ============================================================================

#[test]
fn global_const_keeps_declared_name() {
    assert_eq!(eval("APP_NAME"), "starling");
}

#[test]
fn global_multiword_const_keeps_declared_name() {
    assert_eq!(eval("MAX_RETRIES"), "3");
}

#[test]
fn global_camel_cased_const_name_is_absent() {
    assert_eq!(
        eval("typeof globalThis.appName + ',' + typeof globalThis.maxRetries"),
        "undefined,undefined"
    );
}

#[test]
fn global_function_is_still_camel_cased() {
    assert_eq!(eval("formatName('x')"), "x");
}

// ============================================================================
// #[jsnamespace] / #[webidl_namespace] — already correct, pinned against regression
// ============================================================================

#[test]
fn namespace_const_keeps_declared_name() {
    assert_eq!(eval("constNs.MEDIA_ERR"), "4");
}

#[test]
fn namespace_function_is_still_camel_cased() {
    assert_eq!(eval("constNs.identityValue(7)"), "7");
}

#[test]
fn webidl_namespace_const_keeps_declared_name() {
    assert_eq!(eval("ConstWebIDL.HIERARCHY_REQUEST_ERR"), "3");
}

#[test]
fn webidl_namespace_function_is_still_camel_cased() {
    assert_eq!(eval("ConstWebIDL.escapeValue('y')"), "y");
}

// ============================================================================
// Derived JS names for the containers themselves
//
// A `mod` block's own name is camelCased to derive the JS-visible name it is
// reached by — the module's import specifier and the namespace's global
// property — matching how the functions inside it are renamed. An explicit
// `name = "..."` is always used verbatim.
// ============================================================================

#[jsmodule]
mod multi_word_module {
    pub const ANSWER: i32 = 42;
}

#[jsmodule(name = "explicit_module_name")]
mod ignored_module_name {
    pub const ANSWER: i32 = 7;
}

#[jsnamespace]
mod multi_word_ns {
    pub const ANSWER: i32 = 5;
}

fn setup_derived_names() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        multi_word_ns::add_to_global(scope, global);
        multi_word_module::register(scope);
        ignored_module_name::register(scope);
    });
}

/// Import from `specifier` and read `ANSWER` back as a string.
fn eval_import(specifier: &str) -> String {
    setup_derived_names();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let source = format!("import {{ ANSWER }} from \"{specifier}\";\nglobalThis._result = ANSWER;");
    // SAFETY: `scope` outlives the evaluation, and the module registry was
    // populated by `setup_derived_names` before any JS ran.
    unsafe { evaluate_module(&scope, &source, "specifier_test.mjs") }.expect("module eval failed");
    let val = js::compile::evaluate_with_filename(&scope, "globalThis._result", "read.js", 1)
        .expect("readback failed");
    String::from_jsval(&scope, val, ()).expect("null string")
}

#[test]
fn module_specifier_is_camel_cased() {
    assert_eq!(eval_import("multiWordModule"), "42");
}

#[test]
fn explicit_module_name_is_used_verbatim() {
    assert_eq!(eval_import("explicit_module_name"), "7");
}

#[test]
fn namespace_global_name_is_camel_cased() {
    setup_derived_names();
    assert_eq!(
        eval_with_setup(setup_derived_names, "multiWordNs.ANSWER"),
        "5"
    );
}
