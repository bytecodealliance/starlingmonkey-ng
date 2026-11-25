// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Implementation-specific checks not covered by the WebCryptoAPI WPT suite: the ordering of
//! `digest()`'s steps where script can run in between, when the operations' promises settle, the
//! error types they reject with, and the parts of HMAC key import WPT doesn't test: its error
//! cases, `length`, and the order of `usages`.

#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::event_loop::{run_microtasks, run_to_completion, with_event_loop};
use core_runtime::invocation::{InvocationGuard, InvocationState};
use core_runtime::runtime::{clear_global_initializers, register_global_initializer, Runtime};
use core_runtime::test_util::eval_out_with_setup;
use js::conversion::FromJSVal;

/// Evaluate `code`, run the event loop until it has no work left, and return
/// `String(globalThis.__out)`. `digest()`'s promises settle in event loop tasks, so the helpers
/// that only drain microtasks can't be used.
fn setup() {
    clear_global_initializers();
    register_global_initializer(web_globals::add_to_global);
    register_global_initializer(web_crypto::add_to_global);
}

fn eval_out(code: &str) -> String {
    setup();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    // The loop's tasks hold GC things (`digest()`'s promise), so it has to belong to an
    // invocation registered with the runtime for the GC to trace them.
    let invocation = InvocationState::new();
    // SAFETY: `invocation` stays at this address until the guard drops at the end of this
    // function.
    let _guard = unsafe { InvocationGuard::new(&rt, &invocation) };
    let el = invocation.event_loop();
    with_event_loop(el, |_| {
        js::compile::evaluate_with_filename(&scope, code, "test.js", 1).expect("evaluating code");
        run_microtasks(&scope);
    });
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a tokio runtime");
    tokio.block_on(async {
        let raw_cx = unsafe { scope.raw_cx_no_gc() };
        unsafe { run_to_completion(raw_cx, el, tokio::time::sleep).await }
    });
    run_microtasks(&scope);
    let out = js::compile::evaluate_with_filename(&scope, "String(globalThis.__out)", "out.js", 1)
        .expect("reading __out");
    String::from_jsval(&scope, out, ()).expect("a string")
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
                () => crypto.subtle.digest("SHA-224", data),
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

/// The promise `digest()` returns settles in a task, so a job that was queued after the call
/// still runs before the promise's reactions do.
#[test]
fn digest_settles_after_jobs_queued_later() {
    assert_eq!(
        eval_out(
            r#"
            const order = [];
            crypto.subtle.digest("SHA-256", new Uint8Array(1)).then(() => {
                order.push("digest");
                globalThis.__out = order.join();
            });
            Promise.resolve().then(() => order.push("job"));
            "#
        ),
        "job,digest"
    );
}

/// Settling the promise takes an event loop task, so `digest()` rejects without an active loop,
/// like the other operations that need one, instead of leaving the promise pending forever.
#[test]
fn digest_rejects_without_an_event_loop() {
    assert_eq!(
        eval_out_with_setup(
            setup,
            r#"
            crypto.subtle.digest("SHA-256", new Uint8Array(1)).then(
                () => { globalThis.__out = "resolved"; },
                e => { globalThis.__out = e.name + ": " + e.message; });
            "#
        ),
        "TypeError: `digest` needs an active event loop"
    );
}

/// `[[usages]]` is the normalized value of the requested usages, so in the spec's order, and
/// `algorithm` and `usages` return the same object every time. (WPT's `symmetric_importKey`
/// checks the usages without their order, and `crypto_key_cached_slots` needs `generateKey`.)
#[test]
fn hmac_key_usages_order_and_cached_objects() {
    assert_eq!(
        eval_out(
            r#"
            (async () => {
                const key = await crypto.subtle.importKey(
                    "raw", new Uint8Array(3), { name: "HMAC", hash: "SHA-256" }, false,
                    ["verify", "sign"]);
                globalThis.__out = [key.usages.join("+"), key.algorithm === key.algorithm,
                                    key.usages === key.usages].join();
            })().catch(e => { globalThis.__out = String(e); });
            "#
        ),
        "sign+verify,true,true"
    );
}

/// The `length` member: the key is the first `length` bits of the data, which must end within
/// its last byte. (WPT only imports HMAC keys without a `length`.)
#[test]
fn hmac_import_length() {
    assert_eq!(
        eval_out(
            r#"
            const importKey = (bytes, length) => crypto.subtle.importKey(
                "raw", new Uint8Array(bytes), { name: "HMAC", hash: "SHA-256", length }, false,
                ["sign"]);
            const sign = async key => new Uint8Array(
                await crypto.subtle.sign("HMAC", key, new Uint8Array(1))).join();
            (async () => {
                // The first 12 bits of 0xffff are the key 0xfff0.
                const short = await importKey([0xff, 0xff], 12);
                const full = await importKey([0xff, 0xf0]);
                // Longer than the data, a byte or more shorter, and zero.
                const errors = await Promise.all([17, 8, 0].map(length =>
                    importKey([1, 2], length).then(() => "resolved", e => e.name)));
                globalThis.__out = [short.algorithm.length, await sign(short) === await sign(full),
                                    ...errors].join();
            })().catch(e => { globalThis.__out = String(e); });
            "#
        ),
        "12,true,DataError,DataError,DataError"
    );
}

/// The checks of an HMAC JWK: each one rejects with a `DataError`. (WPT has no failure tests for
/// HMAC key import.)
#[test]
fn hmac_import_jwk_checks() {
    assert_eq!(
        eval_out(
            r#"
            const importJwk = (fields, extractable = false) => crypto.subtle.importKey(
                "jwk", { kty: "oct", k: "AQID", ...fields }, { name: "HMAC", hash: "SHA-256" },
                extractable, ["sign"]).then(() => "resolved", e => e.name);
            Promise.all([
                importJwk({ kty: "RSA" }),
                importJwk({ k: undefined }),
                importJwk({ k: "AQ==" }),
                importJwk({ k: "A+/B" }),
                importJwk({ alg: "HS512" }),
                importJwk({ use: "enc" }),
                importJwk({ key_ops: ["sign", "sign"] }),
                importJwk({ key_ops: ["verify"] }),
                importJwk({ ext: false }, true),
                importJwk({ use: "sig", key_ops: ["sign", "verify"], ext: false }),
            ]).then(results => { globalThis.__out = results.join(); });
            "#
        ),
        "DataError,DataError,DataError,DataError,DataError,DataError,DataError,DataError,\
         DataError,resolved"
    );
}

/// The other errors of importing an HMAC key: key data of the wrong type for the format, a
/// format HMAC doesn't support, and a usage other than "sign" or "verify". (Not in WPT, as
/// above.)
#[test]
fn hmac_import_errors() {
    assert_eq!(
        eval_out(
            r#"
            const alg = { name: "HMAC", hash: "SHA-256" };
            const bytes = new Uint8Array(3);
            Promise.all([
                crypto.subtle.importKey("raw", { kty: "oct", k: "AQID" }, alg, false, ["sign"]),
                crypto.subtle.importKey("jwk", bytes, alg, false, ["sign"]),
                crypto.subtle.importKey("spki", bytes, alg, false, ["sign"]),
                crypto.subtle.importKey("raw", bytes, alg, false, ["encrypt"]),
            ].map(promise => promise.then(() => "resolved", e => e.name)))
                .then(results => { globalThis.__out = results.join(); });
            "#
        ),
        "TypeError,TypeError,NotSupportedError,SyntaxError"
    );
}

/// An error from the steps that run in parallel rejects the promise in a task, so a job queued
/// after the call still runs first.
#[test]
fn import_key_rejects_after_jobs_queued_later() {
    assert_eq!(
        eval_out(
            r#"
            const order = [];
            crypto.subtle.importKey("raw", new Uint8Array(0), { name: "HMAC", hash: "SHA-256" },
                                    false, ["sign"]).catch(e => {
                order.push(e.name);
                globalThis.__out = order.join();
            });
            Promise.resolve().then(() => order.push("job"));
            "#
        ),
        "job,DataError"
    );
}

/// Converting a `JsonWebKey` gets and converts every member, including those HMAC doesn't read,
/// so a getter on one runs and a bad `oth` rejects with a `TypeError`. (Not in WPT.)
#[test]
fn jwk_converts_every_member() {
    assert_eq!(
        eval_out(
            r#"
            const got = [];
            const importJwk = fields => crypto.subtle.importKey(
                "jwk", { kty: "oct", k: "AQID", ...fields }, { name: "HMAC", hash: "SHA-256" },
                false, ["sign"]).then(() => "resolved", e => e.name);
            Promise.all([
                importJwk({ get crv() { got.push("crv"); } }),
                importJwk({ oth: 1 }),
            ]).then(results => { globalThis.__out = [...got, ...results].join(); });
            "#
        ),
        "crv,resolved,TypeError"
    );
}

/// MD5, supported for compatibility with the C++ StarlingMonkey but not in the spec, so not in
/// WPT: `digest()` with it (matched case-insensitively), HMAC-MD5 keys, which sign as RFC 2104
/// says, and the `DataError` for an HMAC-MD5 JWK.
#[test]
fn md5_digest_and_hmac() {
    assert_eq!(
        eval_out(
            r#"
            const hex = buffer => [...new Uint8Array(buffer)]
                .map(b => b.toString(16).padStart(2, "0")).join("");
            (async () => {
                const abc = new Uint8Array([0x61, 0x62, 0x63]);
                const key = await crypto.subtle.importKey(
                    "raw", new Uint8Array(16).fill(0x0b), { name: "HMAC", hash: "MD5" }, false,
                    ["sign"]);
                const jwk = await crypto.subtle.importKey(
                    "jwk", { kty: "oct", k: "AQID" }, { name: "HMAC", hash: "MD5" }, false,
                    ["sign"]).then(() => "resolved", e => e.name);
                globalThis.__out = [
                    hex(await crypto.subtle.digest("md5", abc)),
                    key.algorithm.hash.name,
                    hex(await crypto.subtle.sign("HMAC", key, new Uint8Array([0x48, 0x69, 0x20, 0x54, 0x68, 0x65, 0x72, 0x65]))),
                    jwk,
                ].join();
            })().catch(e => { globalThis.__out = String(e); });
            "#
        ),
        "900150983cd24fb0d6963f7d28e17f72,MD5,9294727a3638bb1c13f48ef8158bfc9d,DataError"
    );
}
