// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Demonstrates defining a JS class with methods using `#[jsclass]` and `#[jsmethods]`.

// `showcase` covers what this example shows, so `cargo test --examples` builds it as an empty
// test harness.
#![cfg(not(test))]

use js::compile::evaluate_with_filename;
use js::error::ExnThrown;
use js::prelude::FromJSVal;
use libstarling::config::RuntimeConfig;
use libstarling::runtime::Runtime;
use libstarling::{jsclass, jsmethods};

// ============================================================================
// Define the Rust struct as a JS class using #[jsclass]
// ============================================================================

#[jsclass]
struct MyClass {
    data: String,
}

// ============================================================================
// Define methods using #[jsmethods]
// ============================================================================

#[jsmethods]
impl MyClass {
    #[constructor]
    fn new(data: String) -> Self {
        Self { data }
    }

    #[getter]
    fn data(&self) -> String {
        self.data.clone()
    }

    #[method(name = "toString")]
    fn describe(&self) -> String {
        format!("MyClass({})", self.data().data)
    }

    #[method(name = "toJSON")]
    fn json(&self) -> Result<String, ExnThrown> {
        Ok(self.data().data.clone())
    }
}

fn main() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let global = scope.global();

    // Register MyClass on the global object
    MyClass::add_to_global(&scope, global);

    // Test toString
    println!("Calling toString");
    let rval = evaluate_with_filename(
        &scope,
        r#"
const a = new MyClass("Hello, world!");
a.toString()
"#,
        "test.js",
        1,
    )
    .expect("toString script failed");
    let result_str = String::from_jsval(&scope, rval, ()).expect("null string");
    assert_eq!(result_str, "MyClass(Hello, world!)");
    println!("  Result: {}", result_str);

    // Test toJSON
    println!("Calling toJSON");
    let rval = evaluate_with_filename(
        &scope,
        r#"
const b = new MyClass("Hello, world!");
JSON.stringify(b)
"#,
        "test2.js",
        1,
    )
    .expect("toJSON script failed");
    let result_str = String::from_jsval(&scope, rval, ()).expect("null string");
    assert_eq!(result_str, r#""Hello, world!""#);
    println!("  Result: {}", result_str);

    // Test data getter
    println!("Calling data getter");
    let rval = evaluate_with_filename(
        &scope,
        r#"
const c = new MyClass("Hello, world!");
c.data
"#,
        "test3.js",
        1,
    )
    .expect("data getter script failed");
    let result_str = String::from_jsval(&scope, rval, ()).expect("null string");
    assert_eq!(result_str, "Hello, world!");
    println!("  Result: {}", result_str);

    println!("All tests passed!");
}
