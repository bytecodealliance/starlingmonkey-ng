// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Implementation-specific regressions not covered by the encoding WPT suite.

#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::event_loop::run_microtasks;
use core_runtime::runtime::{clear_global_initializers, register_global_initializer, Runtime};
use core_runtime::test_util::eval_with_setup;
use js::conversion::FromJSVal;

fn setup() {
    clear_global_initializers();
    register_global_initializer(web_globals::add_to_global);
    register_global_initializer(web_encoding::add_to_global);
    register_global_initializer(web_streams::add_to_global);
}

fn eval(code: &str) -> String {
    eval_with_setup(setup, code)
}

fn run_async(code: &str) -> String {
    setup();
    let runtime = Runtime::init(&RuntimeConfig::default()).expect("initialize test runtime");
    let scope = runtime.default_global();
    js::compile::evaluate_with_filename(&scope, code, "test.js", 1).unwrap();
    run_microtasks(&scope);
    let output =
        js::compile::evaluate_with_filename(&scope, "globalThis.__out", "out.js", 1).unwrap();
    String::from_jsval(&scope, output, ()).unwrap()
}

#[test]
fn text_decoder_windows_1252_maps_every_byte() {
    assert_eq!(
        eval(
            "const decoder = new TextDecoder('latin1');
             const codes = text => Array.from(text, char => char.charCodeAt(0)).join(',');
             [decoder.decode(new Uint8Array([0x41, 0xe9, 0xff, 0xa0])),
              decoder.decode(new Uint8Array([0x41, 0x80, 0x9f, 0xe9])),
              decoder.decode(new Uint8Array([0xe9]), { stream: true }) +
                decoder.decode(new Uint8Array([0x80]), { stream: true }) + decoder.decode(),
              decoder.decode(new Uint8Array([]))].map(codes).join(' | ')"
        ),
        "65,233,255,160 | 65,8364,376,233 | 233,8364 | "
    );
}

#[test]
fn text_decoder_label_errors_match() {
    assert_eq!(
        eval(
            r#"
            for (const label of ["invalid-encoding", "replacement", ""]) {
                const errors = [TextDecoder, TextDecoderStream].map(Constructor => {
                    try {
                        new Constructor(label);
                        throw new Error("Expected invalid encoding label to throw");
                    } catch (error) {
                        if (!(error instanceof RangeError)) throw error;
                        return error.message;
                    }
                });
                if (errors[0] !== errors[1] || !errors[0].includes(label)) {
                    throw new Error("Decoder constructor errors differ");
                }
            }
            true
            "#
        ),
        "true"
    );
}

#[test]
fn text_decoder_latin1_fast_paths_gc_safe() {
    assert_eq!(
        eval(
            r#"
            for (const encoding of ["utf-8", "windows-1252"]) {
                for (const fatal of [false, true]) {
                    const decoder = new TextDecoder(encoding, { fatal });
                    for (const length of [0, 1, 24, 25, 64, 256]) {
                        const bytes = new Uint8Array(length);
                        for (let index = 0; index < length; index++) {
                            bytes[index] = encoding === "utf-8"
                                ? 65 + index % 26 : 0xA0 + index % 96;
                        }
                        const padded = new Uint8Array(length + 4);
                        padded.set(bytes, 2);
                        const expected = String.fromCharCode(...bytes);
                        for (const input of [
                            bytes,
                            bytes.buffer,
                            padded.subarray(2, length + 2),
                            new DataView(padded.buffer, 2, length),
                        ]) {
                            if (decoder.decode(input) !== expected) {
                                throw new Error(`Incorrect ${encoding} fast-path output`);
                            }
                        }
                    }
                }
            }
            true
            "#,
        ),
        "true"
    );
}

#[test]
fn text_decoder_stream_windows_1252_gc_safe() {
    let output = run_async(
        r#"
        globalThis.__out = "pending";
        const inputs = [];
        const expected = [];
        for (const length of [1, 24, 25, 64]) {
            const bytes = new Uint8Array(length).fill(0xE9);
            const padded = new Uint8Array(length + 4);
            padded.set(bytes, 2);
            inputs.push(
                bytes,
                bytes.buffer,
                padded.subarray(2, length + 2),
                new DataView(padded.buffer, 2, length),
            );
            expected.push("\u00E9".repeat(length * 4));
        }
        const source = new ReadableStream({
            start(controller) {
                for (const input of inputs) controller.enqueue(input);
                controller.close();
            }
        });
        (async () => {
            const reader = source.pipeThrough(new TextDecoderStream("windows-1252"))
                .getReader();
            const chunks = [];
            for (;;) {
                const { value, done } = await reader.read();
                if (done) break;
                chunks.push(value);
            }
            globalThis.__out = String(chunks.join("") === expected.join(""));
        })().catch(error => { globalThis.__out = String(error); });
        "#,
    );
    assert_eq!(output, "true");
}
