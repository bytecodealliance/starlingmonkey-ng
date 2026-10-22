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

#[test]
fn structured_clone_is_a_function_of_one_argument() {
    // WebIDL `.length` counts required arguments only, so the optional `options` dictionary isn't
    // counted.
    assert_eq!(
        eval("typeof structuredClone + ',' + structuredClone.length"),
        "function,1"
    );
}

#[test]
fn clone_primitives() {
    assert_eq!(
        eval(
            "[undefined, null, true, false, 42, Infinity, 'hello', '', 42n]
               .map((v) => String(structuredClone(v)))
               .concat(Object.is(structuredClone(-0), -0), Number.isNaN(structuredClone(NaN)))
               .join()"
        ),
        "undefined,null,true,false,42,Infinity,hello,,42,true,true"
    );
}

#[test]
fn clone_wrapped_primitives() {
    assert_eq!(
        eval(
            "[Object(true), Object(42), Object('hello')]
               .map((v) => { const c = structuredClone(v); return typeof c + ':' + (c == v.valueOf()); })
               .join()"
        ),
        "object:true,object:true,object:true"
    );
}

#[test]
fn clone_objects_and_arrays_deeply() {
    assert_eq!(
        eval(
            "let o = {a: {b: 1}, s: 'hello', arr: [[1], [2, [3]]]}; \
             let c = structuredClone(o); \
             o.a.b = 2; \
             [c !== o, c.a.b, c.s, JSON.stringify(c.arr)].join(' ')"
        ),
        "true 1 hello [[1],[2,[3]]]"
    );
}

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

#[test]
fn clone_date_and_regexp() {
    assert_eq!(
        eval(
            "let r = structuredClone(/abc/gi); \
             structuredClone(new Date('2024-01-01T00:00:00Z')).toISOString() + ',' + \
             r.source + ',' + r.flags"
        ),
        "2024-01-01T00:00:00.000Z,abc,gi"
    );
}

#[test]
fn clone_map_and_set() {
    assert_eq!(
        eval(
            "let m = new Map([['a', 1], ['b', 2]]); \
             let s = new Set([1, 2, 3]); \
             let cm = structuredClone(m); \
             let cs = structuredClone(s); \
             [cm.get('a'), cm.get('b'), cm !== m, cs.has(1), cs.has(2), cs.has(3), cs !== s].join()"
        ),
        "1,2,true,true,true,true,true"
    );
}

#[test]
fn clone_arraybuffer_and_typed_array() {
    assert_eq!(
        eval(
            "let buf = new ArrayBuffer(4); \
             new Uint8Array(buf).set([1, 2, 3, 4]); \
             let clone = structuredClone(buf); \
             let a = new Uint8Array([10, 20, 30]); \
             let c = structuredClone(a); \
             [...new Uint8Array(clone), clone !== buf, ...c, c !== a].join()"
        ),
        "1,2,3,4,true,10,20,30,true"
    );
}

#[test]
fn clone_errors() {
    assert_eq!(
        eval(
            "[new Error('test'), new TypeError('bad')] \
               .map((e) => { const c = structuredClone(e); \
                             return (c instanceof e.constructor) + ':' + c.message; }) \
               .join()"
        ),
        "true:test,true:bad"
    );
}

#[test]
fn clone_functions_and_symbols_throws() {
    assert!(throws("structuredClone(function() {})"));
    assert!(throws("structuredClone(Symbol('test'))"));
}

#[test]
fn transfer_arraybuffer_detaches_the_source() {
    assert_eq!(
        eval(
            "let buf = new ArrayBuffer(4); \
             new Uint8Array(buf).set([1, 2, 3, 4]); \
             let clone = structuredClone(buf, { transfer: [buf] }); \
             new Uint8Array(clone)[0] + ',' + buf.byteLength"
        ),
        "1,0"
    );
    assert!(throws(
        "let buf = new ArrayBuffer(4); \
         structuredClone(buf, { transfer: [buf] }); \
         new Uint8Array(buf)"
    ));
}

#[test]
fn options_can_be_null_undefined_or_empty() {
    assert_eq!(
        eval(
            "[structuredClone(42, null), structuredClone(42, undefined), structuredClone(42, {}), \
              structuredClone(42, { transfer: [] })].join()"
        ),
        "42,42,42,42"
    );
}
