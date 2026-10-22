// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Example demonstrating `#[jsglobals]` — installing functions and constants
//! directly onto the JS global object.
//!
//! This is useful for APIs like `atob`, `self`, etc.

// `showcase` covers what this example shows, so `cargo test --examples` builds it as an empty
// test harness.
#![cfg(not(test))]

use js::compile::evaluate_with_filename;
use js::prelude::FromJSVal;
use libstarling::config::RuntimeConfig;
use libstarling::jsglobals;
use libstarling::runtime::Runtime;

// ============================================================================
// Define globals
// ============================================================================

#[jsglobals]
mod my_globals {
    pub const VERSION: &str = "1.0.0";
    pub const MAX_RETRIES: i32 = 3;

    pub fn add(a: f64, b: f64) -> f64 {
        a + b
    }

    pub fn greet(name: String) -> String {
        format!("Hello, {name}!")
    }

    pub fn safe_divide(a: f64, b: f64) -> Result<f64, String> {
        if b == 0.0 {
            Err("Division by zero".to_string())
        } else {
            Ok(a / b)
        }
    }

    /// A variadic global that takes the raw `&CallArgs` and sets its own return
    /// value, returning `Result<(), ExnThrown>`. This "raw" shape is handled by
    /// the same trampoline that backs class methods, so it works for a global
    /// function too.
    pub fn sum_all(
        _scope: &js::gc::scope::Scope<'_>,
        args: &js::native::CallArgs,
    ) -> Result<(), js::error::ExnThrown> {
        let mut total = 0.0;
        for i in 0..args.argc_ {
            let val = args.get(i);
            if val.is_double() {
                total += val.to_double();
            } else if val.is_int32() {
                total += val.to_int32() as f64;
            }
        }
        args.rval().set(js::value::from_f64(total));
        Ok(())
    }
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let global = scope.global();

    // Install globals
    my_globals::add_to_global(&scope, global);

    // ====================================================================
    // Test 1: Rust-side calls still work
    // ====================================================================
    println!("Test 1: Rust-side calls");
    assert_eq!(my_globals::add(2.0, 3.0), 5.0);
    assert_eq!(my_globals::greet("World".into()), "Hello, World!");
    assert_eq!(my_globals::safe_divide(10.0, 2.0), Ok(5.0));
    assert!(my_globals::safe_divide(1.0, 0.0).is_err());
    println!("  PASSED");

    // ====================================================================
    // Test 2: Call global function from JS
    // ====================================================================
    println!("Test 2: add(10, 20) from JS");
    let rval = evaluate_with_filename(&scope, "add(10, 20)", "test.js", 1).expect("add() failed");
    assert_eq!(rval.to_double(), 30.0);
    println!("  PASSED: add(10, 20) = 30");

    // ====================================================================
    // Test 3: String function from JS
    // ====================================================================
    println!("Test 3: greet('SpiderMonkey') from JS");
    let rval = evaluate_with_filename(&scope, "greet('SpiderMonkey')", "test2.js", 1)
        .expect("greet() failed");
    let result_str = String::from_jsval(&scope, rval, ()).expect("null string");
    assert_eq!(result_str, "Hello, SpiderMonkey!");
    println!("  PASSED: greet('SpiderMonkey') = '{}'", result_str);

    // ====================================================================
    // Test 4: Constants available on global
    // ====================================================================
    // Constants keep their declared Rust name — only functions are camelCased.
    println!("Test 4: Global constants");
    let rval = evaluate_with_filename(&scope, "VERSION", "test3.js", 1).expect("VERSION failed");
    let version = String::from_jsval(&scope, rval, ()).expect("null string");
    assert_eq!(version, "1.0.0");
    println!("  PASSED: VERSION = '{}'", version);

    let rval =
        evaluate_with_filename(&scope, "MAX_RETRIES", "test4.js", 1).expect("MAX_RETRIES failed");
    assert!(rval.is_int32());
    assert_eq!(rval.to_int32(), 3);
    println!("  PASSED: MAX_RETRIES = {}", rval.to_int32());

    // ====================================================================
    // Test 5: Error-returning function
    // ====================================================================
    println!("Test 5: safeDivide error handling");
    let rval = evaluate_with_filename(&scope, "safeDivide(10, 2)", "test5.js", 1)
        .expect("safeDivide(10, 2) failed");
    assert_eq!(rval.to_double(), 5.0);
    println!("  PASSED: safeDivide(10, 2) = 5");

    // Division by zero should throw
    let rval = evaluate_with_filename(
        &scope,
        r#"
            try { safeDivide(1, 0); "no error" }
            catch (e) { e.message }
        "#,
        "test6.js",
        1,
    )
    .expect("safeDivide error test failed");
    let err_str = String::from_jsval(&scope, rval, ()).expect("null string");
    assert_eq!(err_str, "Division by zero");
    println!("  PASSED: safeDivide(1, 0) throws '{}'", err_str);

    // ====================================================================
    // Test 6: Variadic raw-`&CallArgs` function returning Result<(), _>
    // ====================================================================
    println!("Test 6: sumAll(1, 2, 3, 4) from JS");
    let rval =
        evaluate_with_filename(&scope, "sumAll(1, 2, 3, 4)", "test7.js", 1).expect("sumAll failed");
    assert_eq!(rval.to_double(), 10.0);
    // No arguments yields 0, and the function still returns undefined-free.
    let rval = evaluate_with_filename(&scope, "sumAll()", "test8.js", 1).expect("sumAll() failed");
    assert_eq!(rval.to_double(), 0.0);
    println!("  PASSED: sumAll(1, 2, 3, 4) = 10, sumAll() = 0");

    println!("\nAll jsglobals tests passed!");
}
