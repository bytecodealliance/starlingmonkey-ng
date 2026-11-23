// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Implementation-specific checks not covered by the WebCryptoAPI WPT suite: the ordering of
//! `digest()`'s steps where script can run in between, when its promise settles, and the error
//! types it rejects with.

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
