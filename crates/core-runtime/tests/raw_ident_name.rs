// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A method/getter named with a raw identifier (a Rust keyword like `type`)
//! must map to that keyword in JS, not to `rType` after camel-casing.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::test_util::eval_with_setup;
use core_runtime::{jsclass, jsmethods};

#[jsclass]
struct Node {}

#[jsmethods]
impl Node {
    #[constructor]
    fn construct() -> Self {
        Self {}
    }

    #[getter]
    fn r#type(&self) -> String {
        "element".to_string()
    }

    #[method]
    fn r#match(&self, other: String) -> bool {
        other == "element"
    }
}

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        Node::add_to_global(scope, global);
    });
}

#[test]
fn raw_identifiers_use_the_keyword_name() {
    assert_eq!(
        eval_with_setup(setup, "`${new Node().type} ${new Node().match('element')}`"),
        "element true"
    );
}
