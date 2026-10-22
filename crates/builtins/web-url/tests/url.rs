// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::test_util::eval_with_setup;

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        web_url::add_to_global(scope, global);
    });
}

fn eval(code: &str) -> String {
    eval_with_setup(setup, code)
}

// The WPT `url/` tests cover the rest of `URL` and `URLSearchParams`.

#[test]
fn url_search_params_iterator_is_not_a_global() {
    assert_eq!(
        eval("typeof globalThis['URLSearchParams Iterator']"),
        "undefined"
    );
    assert_eq!(
        eval("Object.prototype.toString.call(new URLSearchParams('a=1').entries())"),
        "[object URLSearchParams Iterator]"
    );
}

#[test]
fn url_to_string_returns_the_href() {
    assert_eq!(
        eval("new URL('https://example.com/path?q=1#h').toString()"),
        "https://example.com/path?q=1#h"
    );
}

#[test]
fn url_constructor_rejects_an_invalid_base() {
    assert_eq!(
        eval("try { new URL('/path', 'not a url'); 'no-throw' } catch (e) { e.constructor.name }"),
        "TypeError"
    );
}

#[test]
fn url_search_params_keys_and_values() {
    assert_eq!(
        eval(
            "let p = new URLSearchParams('a=1&b=2'); \
             JSON.stringify([Array.from(p.keys()), Array.from(p.values())])"
        ),
        "[[\"a\",\"b\"],[\"1\",\"2\"]]"
    );
}
