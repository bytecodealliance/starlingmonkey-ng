// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Implementation-specific checks not covered by the WebCryptoAPI WPT suite: the ordering of
//! `digest()`'s steps where script can run in between, and the error types it rejects with.

#![cfg(not(target_arch = "wasm32"))]

use core_runtime::runtime::{clear_global_initializers, register_global_initializer};
use core_runtime::test_util::eval_out_with_setup;

fn setup() {
    clear_global_initializers();
    register_global_initializer(web_globals::add_to_global);
    register_global_initializer(web_crypto::add_to_global);
}

fn eval_out(code: &str) -> String {
    eval_out_with_setup(setup, code)
}

/// Step 4 (copy the bytes) runs after step 2 (normalize the algorithm), so a `name` getter that
/// detaches the buffer leaves nothing to hash: the result is SHA-256 of the empty message.
#[test]
fn digest_copies_data_after_normalizing() {
    assert_eq!(
        eval_out(
            r#"
            const data = new Uint8Array([1, 2, 3]);
            const alg = { get name() { data.buffer.transfer(); return "SHA-256"; } };
            crypto.subtle.digest(alg, data).then(
                buf => {
                    globalThis.__out = Array.from(new Uint8Array(buf),
                        b => b.toString(16).padStart(2, "0")).join("");
                },
                e => { globalThis.__out = String(e); });
            "#
        ),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

/// Errors from normalizing the algorithm and from converting the arguments reject the returned
/// promise instead of throwing synchronously.
#[test]
fn digest_rejects_instead_of_throwing() {
    assert_eq!(
        eval_out(
            r#"
            const data = new Uint8Array(1);
            const calls = [
                () => crypto.subtle.digest("MD5", data),
                () => crypto.subtle.digest({}, data),
                () => crypto.subtle.digest({ get name() { throw new SyntaxError(); } }, data),
                () => crypto.subtle.digest("SHA-256", "not a buffer"),
                () => crypto.subtle.digest("SHA-256"),
            ];
            Promise.all(calls.map(call => {
                let promise;
                try { promise = call(); } catch (e) { return "threw " + e.name; }
                return promise.then(() => "resolved", e => e.name);
            })).then(names => { globalThis.__out = names.join(); });
            "#
        ),
        "NotSupportedError,TypeError,SyntaxError,TypeError,TypeError"
    );
}

/// `getRandomValues`'s `QuotaExceededError` is the `DOMException` subclass, not a plain
/// `DOMException` with that name.
#[test]
fn get_random_values_throws_quota_exceeded_error_subclass() {
    assert_eq!(
        eval_out(
            r#"
            try {
                crypto.getRandomValues(new Uint8Array(65537));
                globalThis.__out = "no throw";
            } catch (e) {
                globalThis.__out = [e instanceof QuotaExceededError, e instanceof DOMException,
                                    e.name, e.code, e.quota, e.requested].join();
            }
            "#
        ),
        "true,true,QuotaExceededError,22,,"
    );
}
