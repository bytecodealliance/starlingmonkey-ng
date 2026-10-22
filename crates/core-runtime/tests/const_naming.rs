// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Constants keep their declared Rust name in every macro that exposes them, while functions are
//! camelCased (`safe_divide` → `safeDivide`): `PI` stays `PI`, since JS spells constants in
//! SCREAMING_CASE too. `typedef_macros` covers the constants of `#[jsclass]` and
//! `#[webidl_interface]`.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::test_util::{eval_module_out_with_setup, eval_with_setup};
use core_runtime::{jsglobals, jsmodule, jsnamespace, webidl_namespace};

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

// ============================================================================
// Derived JS names for the containers themselves
//
// A `mod` block's own name is camelCased to derive the JS-visible name it is reached by: the
// module's import specifier and the namespace's global property. An explicit `name = "..."` is
// used verbatim.
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

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        const_globals::add_to_global(scope, global);
        const_ns::add_to_global(scope, global);
        const_webidl_ns::add_to_global(scope, global);
        multi_word_ns::add_to_global(scope, global);
        const_module::register(scope);
        multi_word_module::register(scope);
        ignored_module_name::register(scope);
    });
}

#[test]
fn module_consts_keep_their_declared_name() {
    assert_eq!(
        eval_module_out_with_setup(
            setup,
            "import * as m from 'constModule';
             globalThis.__out =
                 [m.PI, m.MAX_VALUE, typeof m.pi, typeof m.maxValue, m.safeDivide(10, 4)].join();"
        ),
        "3.5,1000,undefined,undefined,2.5"
    );
}

#[test]
fn global_consts_keep_their_declared_name() {
    assert_eq!(
        eval_with_setup(
            setup,
            "[APP_NAME, MAX_RETRIES, typeof globalThis.appName, typeof globalThis.maxRetries,
              formatName('x')].join()"
        ),
        "starling,3,undefined,undefined,x"
    );
}

#[test]
fn namespace_consts_keep_their_declared_name() {
    assert_eq!(
        eval_with_setup(
            setup,
            "[constNs.MEDIA_ERR, constNs.identityValue(7), ConstWebIDL.HIERARCHY_REQUEST_ERR,
              ConstWebIDL.escapeValue('y')].join()"
        ),
        "4,7,3,y"
    );
}

#[test]
fn container_names_are_camel_cased_unless_explicit() {
    assert_eq!(
        eval_module_out_with_setup(
            setup,
            "import { ANSWER as a } from 'multiWordModule';
             import { ANSWER as b } from 'explicit_module_name';
             globalThis.__out = [a, b, multiWordNs.ANSWER].join();"
        ),
        "42,7,5"
    );
}
