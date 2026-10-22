// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//! A `Request` or `Response` body's byte source lives in Rust memory, outside the GC heap. The
//! bytes are attributed to the owning object (`sync_body_accounting`), so building many bodies
//! triggers collections the way GC-heap allocation would, instead of growing until the process
//! runs out of memory.

#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::{clear_global_initializers, Runtime};
use js::gc::JSGCParamKey;

/// The number of collections so far.
fn gc_count(scope: &js::gc::scope::Scope<'_>) -> u32 {
    js::gc::get_parameter(scope, JSGCParamKey::JSGC_NUMBER)
}

fn evaluate(scope: &js::gc::scope::Scope<'_>, code: &str) {
    if js::compile::evaluate_with_filename(scope, code, "test.js", 1).is_err() {
        panic!(
            "evaluation threw: {:?}",
            js::error::ExnThrown::capture(scope)
        );
    }
}

/// Builds 96 objects each holding a 1 MiB byte source and returns how many collections that
/// caused. The objects are garbage as soon as they are built, and the JS heap sees only their
/// small wrappers.
fn collections_while_building(constructor: &str) -> u32 {
    clear_global_initializers();
    libstarling::register_builtins();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    evaluate(&scope, "globalThis.body = 'x'.repeat(1 << 20);");
    let before = gc_count(&scope);
    evaluate(
        &scope,
        &format!("for (let i = 0; i < 96; i++) {{ {constructor}; }}"),
    );
    gc_count(&scope) - before
}

#[test]
fn response_bodies_trigger_collections() {
    assert!(collections_while_building("new Response(body)") > 0);
}

#[test]
fn request_bodies_trigger_collections() {
    assert!(
        collections_while_building("new Request('http://example.com/', { method: 'POST', body })")
            > 0
    );
}

#[test]
fn consumed_and_cloned_bodies_keep_the_accounting_balanced() {
    // Consuming takes the bytes out of the object, cloning shares them, and a teed body drops
    // its source. Each path adjusts the attributed amount, and finalization removes what is
    // left. With `debugmozjs`, SpiderMonkey asserts the attribution is balanced at finalization,
    // so this test's value is in running under that feature.
    clear_global_initializers();
    libstarling::register_builtins();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    evaluate(
        &scope,
        r#"
        globalThis.body = 'x'.repeat(1 << 16);
        globalThis.results = [];
        for (let i = 0; i < 64; i++) {
            const response = new Response(body);
            const clone = response.clone();
            results.push(response.text(), clone.arrayBuffer());
            const request = new Request('http://example.com/', { method: 'POST', body });
            request.clone();
            results.push(request.text());
            new Response(body).body.getReader();
        }
        "#,
    );
    js::jobs::run_jobs(&scope);
    js::gc::gc(&scope, js::gc::GCReason::API);
    evaluate(&scope, "globalThis.results = null;");
    js::gc::gc(&scope, js::gc::GCReason::API);
}
