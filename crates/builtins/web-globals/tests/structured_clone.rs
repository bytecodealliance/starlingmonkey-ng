// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::test_util::{eval_with_setup, throws_with_setup};

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        web_globals::add_to_global(scope, global);
    });
}

fn eval(code: &str) -> String {
    eval_with_setup(setup, code)
}

fn throws(code: &str) -> bool {
    throws_with_setup(setup, code)
}

// ── structuredClone exists ──

#[test]
fn structured_clone_exists() {
    assert_eq!(eval("typeof structuredClone"), "function");
}

#[test]
fn structured_clone_length_excludes_optional() {
    // WebIDL `.length` counts required arguments only: the optional `options`
    // dictionary must not be counted.
    assert_eq!(eval("structuredClone.length"), "1");
}

// ── Primitives ──

#[test]
fn clone_undefined() {
    assert_eq!(eval("structuredClone(undefined)"), "undefined");
}

#[test]
fn clone_null() {
    assert_eq!(eval("structuredClone(null)"), "null");
}

#[test]
fn clone_boolean_true() {
    assert_eq!(eval("structuredClone(true)"), "true");
}

#[test]
fn clone_boolean_false() {
    assert_eq!(eval("structuredClone(false)"), "false");
}

#[test]
fn clone_number() {
    assert_eq!(eval("structuredClone(42)"), "42");
}

#[test]
fn clone_negative_zero() {
    assert_eq!(eval("Object.is(structuredClone(-0), -0)"), "true");
}

#[test]
fn clone_nan() {
    assert_eq!(eval("Number.isNaN(structuredClone(NaN))"), "true");
}

#[test]
fn clone_infinity() {
    assert_eq!(eval("structuredClone(Infinity)"), "Infinity");
}

#[test]
fn clone_string() {
    assert_eq!(eval("structuredClone('hello')"), "hello");
}

#[test]
fn clone_empty_string() {
    assert_eq!(eval("structuredClone('')"), "");
}

#[test]
fn clone_bigint() {
    assert_eq!(eval("structuredClone(42n).toString()"), "42");
}

// ── Objects ──

#[test]
fn clone_plain_object() {
    assert_eq!(
        eval("JSON.stringify(structuredClone({a: 1, b: 'hello'}))"),
        r#"{"a":1,"b":"hello"}"#
    );
}

#[test]
fn clone_is_deep_copy() {
    assert_eq!(
        eval("let o = {a: {b: 1}}; let c = structuredClone(o); o.a.b = 2; c.a.b"),
        "1"
    );
}

#[test]
fn clone_preserves_identity() {
    // The clone should be a different object.
    assert_eq!(eval("let o = {}; structuredClone(o) === o"), "false");
}

// ── Arrays ──

#[test]
fn clone_array() {
    assert_eq!(
        eval("JSON.stringify(structuredClone([1, 2, 3]))"),
        "[1,2,3]"
    );
}

#[test]
fn clone_nested_array() {
    assert_eq!(
        eval("JSON.stringify(structuredClone([[1], [2, [3]]]))"),
        "[[1],[2,[3]]]"
    );
}

// ── Date ──

#[test]
fn clone_date() {
    assert_eq!(
        eval("structuredClone(new Date('2024-01-01T00:00:00Z')).toISOString()"),
        "2024-01-01T00:00:00.000Z"
    );
}

// ── RegExp ──

#[test]
fn clone_regexp() {
    assert_eq!(
        eval("let r = structuredClone(/abc/gi); r.source + ',' + r.flags"),
        "abc,gi"
    );
}

// ── Map ──

#[test]
fn clone_map() {
    assert_eq!(
        eval(
            "let m = new Map([['a', 1], ['b', 2]]); \
             let c = structuredClone(m); \
             c.get('a') + ',' + c.get('b') + ',' + (c !== m)"
        ),
        "1,2,true"
    );
}

// ── Set ──

#[test]
fn clone_set() {
    assert_eq!(
        eval(
            "let s = new Set([1, 2, 3]); \
             let c = structuredClone(s); \
             c.has(1) + ',' + c.has(2) + ',' + c.has(3) + ',' + (c !== s)"
        ),
        "true,true,true,true"
    );
}

// ── ArrayBuffer ──

#[test]
fn clone_arraybuffer() {
    assert_eq!(
        eval(
            "let buf = new ArrayBuffer(4); \
             new Uint8Array(buf).set([1, 2, 3, 4]); \
             let clone = structuredClone(buf); \
             let arr = new Uint8Array(clone); \
             arr[0] + ',' + arr[1] + ',' + arr[2] + ',' + arr[3] + ',' + (clone !== buf)"
        ),
        "1,2,3,4,true"
    );
}

// ── TypedArray ──

#[test]
fn clone_uint8array() {
    assert_eq!(
        eval(
            "let a = new Uint8Array([10, 20, 30]); \
             let c = structuredClone(a); \
             c[0] + ',' + c[1] + ',' + c[2] + ',' + (c !== a)"
        ),
        "10,20,30,true"
    );
}

// ── Circular references ──

#[test]
fn clone_circular_reference() {
    assert_eq!(
        eval(
            "let o = {a: 1}; o.self = o; \
             let c = structuredClone(o); \
             c.a + ',' + (c.self === c)"
        ),
        "1,true"
    );
}

// ── Error ──

#[test]
fn clone_error() {
    assert_eq!(
        eval(
            "let e = new Error('test'); \
             let c = structuredClone(e); \
             (c instanceof Error) + ',' + c.message"
        ),
        "true,test"
    );
}

#[test]
fn clone_type_error() {
    assert_eq!(
        eval(
            "let e = new TypeError('bad'); \
             let c = structuredClone(e); \
             (c instanceof TypeError) + ',' + c.message"
        ),
        "true,bad"
    );
}

// ── Non-cloneable types ──

#[test]
fn clone_function_throws() {
    assert!(throws("structuredClone(function() {})"));
}

#[test]
fn clone_symbol_throws() {
    assert!(throws("structuredClone(Symbol('test'))"));
}

// ── Transfer ──

#[test]
fn transfer_arraybuffer() {
    assert_eq!(
        eval(
            "let buf = new ArrayBuffer(4); \
             new Uint8Array(buf).set([1, 2, 3, 4]); \
             let clone = structuredClone(buf, { transfer: [buf] }); \
             new Uint8Array(clone)[0] + ',' + buf.byteLength"
        ),
        "1,0"
    );
}

#[test]
fn transfer_detaches_source() {
    assert!(throws(
        "let buf = new ArrayBuffer(4); \
         structuredClone(buf, { transfer: [buf] }); \
         new Uint8Array(buf)"
    ));
}

// ── Options edge cases ──

#[test]
fn options_null_is_ok() {
    assert_eq!(eval("structuredClone(42, null)"), "42");
}

#[test]
fn options_undefined_is_ok() {
    assert_eq!(eval("structuredClone(42, undefined)"), "42");
}

#[test]
fn options_empty_object_is_ok() {
    assert_eq!(eval("structuredClone(42, {})"), "42");
}

#[test]
fn options_empty_transfer_is_ok() {
    assert_eq!(eval("structuredClone(42, { transfer: [] })"), "42");
}

// ── Wrapped primitives ──

#[test]
fn clone_boolean_object() {
    assert_eq!(
        eval(
            "let b = structuredClone(Object(true)); \
             (typeof b) + ',' + (b == true)"
        ),
        "object,true"
    );
}

#[test]
fn clone_number_object() {
    assert_eq!(
        eval(
            "let n = structuredClone(Object(42)); \
             (typeof n) + ',' + (n == 42)"
        ),
        "object,true"
    );
}

#[test]
fn clone_string_object() {
    assert_eq!(
        eval(
            "let s = structuredClone(Object('hello')); \
             (typeof s) + ',' + (s == 'hello')"
        ),
        "object,true"
    );
}

// ── Shared memory ──

#[test]
fn clone_shared_array_buffer_shares_its_memory() {
    setup();
    let config = core_runtime::config::RuntimeConfig {
        shared_memory: true,
        ..Default::default()
    };
    let rt = core_runtime::runtime::Runtime::init(&config).expect("runtime init");
    let scope = rt.default_global();
    let result = js::compile::evaluate(
        &scope,
        "const sab = new SharedArrayBuffer(4);
         const copy = structuredClone(sab);
         new Uint8Array(sab)[0] = 7;
         [copy instanceof SharedArrayBuffer, copy === sab, new Uint8Array(copy)[0]].join()",
    )
    .expect("cloning a SharedArrayBuffer succeeds");
    let result = <String as js::conversion::FromJSVal>::from_jsval(&scope, result, ()).unwrap();
    assert_eq!(result, "true,false,7");
}
