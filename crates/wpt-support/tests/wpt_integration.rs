// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for the WPT support globals (`evalScript`, `__setLocation`).

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::test_util::eval_with_setup;

fn eval_wpt(code: &str) -> String {
    eval_with_setup(
        || {
            libstarling::register_builtins();
            libstarling::runtime::register_global_initializer(wpt_support::add_to_global);
        },
        code,
    )
}

/// `evalScript` places `let` bindings in a non-syntactic scope, so they are visible to later
/// `evalScript` calls.
#[test]
fn eval_script_shares_let_bindings() {
    assert_eq!(
        eval_wpt("evalScript('let wptFoo = 42;'); evalScript('wptFoo.toString()')"),
        "42"
    );
}

#[test]
fn set_location_rejects_an_invalid_url() {
    assert_eq!(
        eval_wpt(
            "try { __setLocation('not a url'); 'no-throw' } \
             catch (e) { (e instanceof TypeError) + ',' + e.message.includes('invalid URL') }"
        ),
        "true,true"
    );
}

/// After `__setLocation` runs, every `WorkerLocation` accessor reports the URL's components.
#[test]
fn set_location_sets_every_location_accessor() {
    assert_eq!(
        eval_wpt(
            "__setLocation('http://web-platform.test:8000/foo/bar.any.js?x=1'); \
             [location.href, location.origin, location.pathname, location.search].join('|')"
        ),
        "http://web-platform.test:8000/foo/bar.any.js?x=1|\
         http://web-platform.test:8000|/foo/bar.any.js|?x=1"
    );
}
