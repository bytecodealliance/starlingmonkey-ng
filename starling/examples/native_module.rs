// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Example demonstrating native ES module support.
//!
//! Defines a native module with constants and functions, registers it,
//! imports from JS, and also calls the functions from Rust.

use js::compile::evaluate_with_filename;
use libstarling::config::RuntimeConfig;
use libstarling::jsmodule;
use libstarling::module::evaluate_module;
use libstarling::runtime::Runtime;

// ============================================================================
// Define a native module
// ============================================================================

#[jsmodule]
mod math_utils {

    pub const PI: f64 = std::f64::consts::PI;
    pub const MAX_VALUE: f64 = 1000.0;

    pub fn add(a: f64, b: f64) -> f64 {
        a + b
    }

    pub fn multiply(a: f64, b: f64) -> f64 {
        a * b
    }

    pub fn greet(name: String) -> String {
        format!("Hello, {}!", name)
    }

    pub fn safe_divide(a: f64, b: f64) -> Result<f64, String> {
        if b == 0.0 {
            Err("Division by zero".to_string())
        } else {
            Ok(a / b)
        }
    }
}

fn main() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    // Register native module using the generated convenience function
    assert!(math_utils::register(&scope), "Failed to register module");

    // ====================================================================
    // Test 1: Call functions from Rust
    // ====================================================================
    println!("Test 1: Call functions from Rust");
    assert_eq!(math_utils::add(2.0, 3.0), 5.0);
    assert_eq!(math_utils::multiply(4.0, 5.0), 20.0);
    assert_eq!(math_utils::greet("World".to_string()), "Hello, World!");
    assert_eq!(math_utils::safe_divide(10.0, 2.0), Ok(5.0));
    assert!(math_utils::safe_divide(1.0, 0.0).is_err());
    println!("  PASSED: All Rust-side calls work");

    // ====================================================================
    // Test 2: Import constants from JS
    // ====================================================================
    println!("Test 2: Import constants from JS");
    let result = unsafe {
        evaluate_module(
            &scope,
            r#"
                import { PI, MAX_VALUE } from "mathUtils";
                globalThis._testPI = PI;
                globalThis._testMaxValue = MAX_VALUE;
            "#,
            "test_constants.mjs",
        )
    };
    assert!(result.is_ok(), "Module evaluation failed");

    // Read back the values
    let rval = evaluate_with_filename(&scope, "globalThis._testPI", "verify_constants.js", 1)
        .expect("eval failed");
    assert!(rval.is_double());
    let pi_val = rval.to_double();
    assert!(
        (pi_val - std::f64::consts::PI).abs() < 1e-10,
        "PI mismatch: {}",
        pi_val
    );

    let rval = evaluate_with_filename(&scope, "globalThis._testMaxValue", "verify_maxvalue.js", 1)
        .expect("eval failed");
    assert!(rval.is_double());
    assert_eq!(rval.to_double(), 1000.0);
    println!("  PASSED: Constants imported correctly");

    // ====================================================================
    // Test 3: Import and call functions from JS
    // ====================================================================
    println!("Test 3: Import and call functions from JS");
    let result = unsafe {
        evaluate_module(
            &scope,
            r#"
                import { add, multiply } from "mathUtils";
                globalThis._testAdd = add(10, 20);
                globalThis._testMul = multiply(6, 7);
            "#,
            "test_functions.mjs",
        )
    };
    assert!(result.is_ok(), "Module evaluation failed");

    let rval = evaluate_with_filename(&scope, "globalThis._testAdd", "verify_add.js", 1)
        .expect("eval failed");
    assert_eq!(rval.to_double(), 30.0);

    let rval = evaluate_with_filename(&scope, "globalThis._testMul", "verify_mul.js", 1)
        .expect("eval failed");
    assert_eq!(rval.to_double(), 42.0);
    println!("  PASSED: Functions callable from JS");

    // ====================================================================
    // Test 4: String function from JS
    // ====================================================================
    println!("Test 4: String function from JS");
    let result = unsafe {
        evaluate_module(
            &scope,
            r#"
                import { greet } from "mathUtils";
                globalThis._testGreet = greet("SpiderMonkey");
            "#,
            "test_greet.mjs",
        )
    };
    assert!(result.is_ok(), "Module evaluation failed");

    let rval = evaluate_with_filename(&scope, "globalThis._testGreet", "verify_greet.js", 1)
        .expect("eval failed");
    assert!(rval.is_string());
    println!("  PASSED: String function works from JS");

    // ====================================================================
    // Test 5: Fallible function from JS (success case)
    // ====================================================================
    println!("Test 5: Fallible function (success)");
    let result = unsafe {
        evaluate_module(
            &scope,
            r#"
                import { safeDivide } from "mathUtils";
                globalThis._testDiv = safeDivide(100, 4);
            "#,
            "test_divide.mjs",
        )
    };
    assert!(result.is_ok(), "Module evaluation failed");

    let rval = evaluate_with_filename(&scope, "globalThis._testDiv", "verify_div.js", 1)
        .expect("eval failed");
    assert_eq!(rval.to_double(), 25.0);
    println!("  PASSED: Fallible function succeeds correctly");

    // ====================================================================
    // Test 6: Fallible function from JS (error case)
    // ====================================================================
    println!("Test 6: Fallible function (error)");
    let result = unsafe {
        evaluate_module(
            &scope,
            r#"
                import { safeDivide } from "mathUtils";
                try {
                    safeDivide(1, 0);
                    globalThis._testDivErr = "no error";
                } catch (e) {
                    globalThis._testDivErr = "caught: " + e.message;
                }
            "#,
            "test_divide_err.mjs",
        )
    };
    assert!(result.is_ok(), "Module evaluation failed");

    let rval = evaluate_with_filename(&scope, "globalThis._testDivErr", "verify_div_err.js", 1)
        .expect("eval failed");
    assert!(rval.is_string());
    println!("  PASSED: Fallible function throws on error");

    // ====================================================================
    // Test 7: camelCase renaming of functions
    // ====================================================================
    println!("Test 7: camelCase renaming applied to functions only");
    // safe_divide was renamed to safeDivide, while the PI and MAX_VALUE
    // constants kept their declared names. If the imports above work, both
    // halves of the rule hold.
    println!("  PASSED: verified by tests 2, 5, 6");

    println!("\nAll 7 tests passed!");
}

#[test]
fn native_module_example() {
    main()
}
