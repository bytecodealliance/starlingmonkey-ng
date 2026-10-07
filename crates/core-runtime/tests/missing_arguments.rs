// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A missing argument past a function's required count converts as `undefined`, whatever its
//! type, and a missing required argument throws.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::jsnamespace;
use core_runtime::test_util::{eval_with_setup, throws_with_setup};
use js::gc::scope::Scope;

#[jsnamespace(name = "lenient", node_args)]
mod lenient {
    pub fn number(n: u32) -> u32 {
        n
    }

    pub fn text(s: String) -> String {
        s
    }
}

#[jsnamespace(name = "strict")]
mod strict {
    pub fn number(n: u32) -> u32 {
        n
    }
}

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope: &Scope<'_>, global| {
        lenient::add_to_global(scope, global);
        strict::add_to_global(scope, global);
    });
}

#[test]
fn node_args_functions_convert_missing_arguments_as_undefined() {
    assert_eq!(
        eval_with_setup(setup, "[lenient.number(), lenient.text()].join()"),
        "0,undefined"
    );
}

#[test]
fn a_missing_required_argument_throws() {
    assert!(throws_with_setup(setup, "strict.number()"));
}
