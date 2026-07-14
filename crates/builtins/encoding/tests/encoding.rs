// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for the Encoding API (TextEncoder, TextDecoder,
//! TextEncoderStream, TextDecoderStream).

use core_runtime::config::RuntimeConfig;
use core_runtime::event_loop::run_microtasks;
use core_runtime::runtime::{clear_global_initializers, register_global_initializer, Runtime};
use core_runtime::test_util::{eval_with_setup, throws_with_setup};
use js::conversion::FromJSVal;

fn setup() {
    clear_global_initializers();
    register_global_initializer(|scope, global| {
        web_globals::add_to_global(scope, global);
    });
    register_global_initializer(|scope, global| {
        encoding::add_to_global(scope, global);
    });
    register_global_initializer(|scope, global| {
        web_streams::add_to_global(scope, global);
    });
}

fn eval(code: &str) -> String {
    eval_with_setup(setup, code)
}

fn throws(code: &str) -> bool {
    throws_with_setup(setup, code)
}

/// Runs async JS code and returns the value of `globalThis.__out` after
/// microtasks have settled.
fn run_async(code: &str) -> String {
    clear_global_initializers();
    register_global_initializer(|scope, global| web_globals::add_to_global(scope, global));
    register_global_initializer(|scope, global| encoding::add_to_global(scope, global));
    register_global_initializer(|scope, global| web_streams::add_to_global(scope, global));
    let rt = Runtime::init(&RuntimeConfig::default()).expect("initialize test runtime");
    let scope = rt.default_global();
    js::compile::evaluate_with_filename(&scope, code, "test.js", 1).ok();
    run_microtasks(&scope);
    let out = js::compile::evaluate_with_filename(&scope, "globalThis.__out", "out.js", 1).unwrap();
    String::from_jsval(&scope, out, ()).unwrap()
}

/// Drains a ReadableStringStream into an array of string chunks.
const DRAIN_STR: &str = r#"
    async function drainStr(stream) {
        const r = stream.getReader();
        const acc = [];
        for (;;) {
            const {value, done} = await r.read();
            if (done) break;
            acc.push(value);
        }
        return acc;
    }
"#;

/// Drains a ReadableStream<Uint8Array> into a flat array of byte numbers.
const DRAIN_BYTES: &str = r#"
    async function drainBytes(stream) {
        const r = stream.getReader();
        const acc = [];
        for (;;) {
            const {value, done} = await r.read();
            if (done) break;
            for (const b of value) acc.push(b);
        }
        return acc;
    }
"#;

// ── TextEncoder existence ──

#[test]
fn text_encoder_exists() {
    assert_eq!(eval("typeof TextEncoder"), "function");
}

#[test]
fn text_encoder_construct() {
    assert_eq!(eval("new TextEncoder() instanceof TextEncoder"), "true");
}

// ── TextEncoder.encoding ──

#[test]
fn text_encoder_encoding() {
    assert_eq!(eval("new TextEncoder().encoding"), "utf-8");
}

// ── TextEncoder.encode() ──

#[test]
fn text_encoder_encode_empty() {
    assert_eq!(eval("new TextEncoder().encode('').length"), "0");
}

#[test]
fn text_encoder_encode_ascii() {
    assert_eq!(
        eval("Array.from(new TextEncoder().encode('abc')).join(',')"),
        "97,98,99"
    );
}

#[test]
fn text_encoder_encode_returns_uint8array() {
    assert_eq!(
        eval("new TextEncoder().encode('a') instanceof Uint8Array"),
        "true"
    );
}

#[test]
fn text_encoder_encode_no_args() {
    // encode() with no arguments encodes empty string
    assert_eq!(eval("new TextEncoder().encode().length"), "0");
}

#[test]
fn text_encoder_encode_multibyte() {
    // "€" is U+20AC, encoded as [0xE2, 0x82, 0xAC] in UTF-8
    assert_eq!(
        eval(r#"Array.from(new TextEncoder().encode('€')).join(',')"#),
        "226,130,172"
    );
}

#[test]
fn text_encoder_encode_emoji() {
    // "😀" is U+1F600, encoded as [0xF0, 0x9F, 0x98, 0x80]
    assert_eq!(
        eval(r#"Array.from(new TextEncoder().encode('😀')).join(',')"#),
        "240,159,152,128"
    );
}

// ── TextEncoder.encodeInto() ──

#[test]
fn text_encoder_encode_into_basic() {
    assert_eq!(
        eval(
            r#"
            const enc = new TextEncoder();
            const buf = new Uint8Array(10);
            const result = enc.encodeInto('abc', buf);
            result.read + ',' + result.written
        "#
        ),
        "3,3"
    );
}

#[test]
fn text_encoder_encode_into_writes_bytes() {
    assert_eq!(
        eval(
            r#"
            const enc = new TextEncoder();
            const buf = new Uint8Array(10);
            enc.encodeInto('abc', buf);
            buf[0] + ',' + buf[1] + ',' + buf[2]
        "#
        ),
        "97,98,99"
    );
}

#[test]
fn text_encoder_encode_into_truncation() {
    // Buffer too small to fit all characters
    assert_eq!(
        eval(
            r#"
            const enc = new TextEncoder();
            const buf = new Uint8Array(2);
            const result = enc.encodeInto('abcdef', buf);
            result.read + ',' + result.written
        "#
        ),
        "2,2"
    );
}

#[test]
fn text_encoder_encode_into_multibyte_truncation() {
    // "€" needs 3 bytes; buffer of 2 can't fit it
    assert_eq!(
        eval(
            r#"
            const enc = new TextEncoder();
            const buf = new Uint8Array(2);
            const result = enc.encodeInto('€', buf);
            result.read + ',' + result.written
        "#
        ),
        "0,0"
    );
}

#[test]
fn text_encoder_encode_into_surrogate_pair_count() {
    // "😀" is U+1F600, takes 4 UTF-8 bytes and 2 UTF-16 code units
    assert_eq!(
        eval(
            r#"
            const enc = new TextEncoder();
            const buf = new Uint8Array(10);
            const result = enc.encodeInto('😀', buf);
            result.read + ',' + result.written
        "#
        ),
        "2,4"
    );
}

// ── TextDecoder existence ──

#[test]
fn text_decoder_exists() {
    assert_eq!(eval("typeof TextDecoder"), "function");
}

#[test]
fn text_decoder_construct() {
    assert_eq!(eval("new TextDecoder() instanceof TextDecoder"), "true");
}

// ── TextDecoder.encoding ──

#[test]
fn text_decoder_encoding_default() {
    assert_eq!(eval("new TextDecoder().encoding"), "utf-8");
}

#[test]
fn text_decoder_encoding_explicit_utf8() {
    assert_eq!(eval("new TextDecoder('utf-8').encoding"), "utf-8");
}

#[test]
fn text_decoder_encoding_label_normalization() {
    // "UTF-8" should be normalized to "utf-8"
    assert_eq!(eval("new TextDecoder('UTF-8').encoding"), "utf-8");
}

#[test]
fn text_decoder_encoding_windows_1252() {
    assert_eq!(
        eval("new TextDecoder('windows-1252').encoding"),
        "windows-1252"
    );
}

// ── TextDecoder.fatal ──

#[test]
fn text_decoder_fatal_default() {
    assert_eq!(eval("new TextDecoder().fatal"), "false");
}

#[test]
fn text_decoder_fatal_true() {
    assert_eq!(
        eval("new TextDecoder('utf-8', { fatal: true }).fatal"),
        "true"
    );
}

// ── TextDecoder.ignoreBOM ──

#[test]
fn text_decoder_ignore_bom_default() {
    assert_eq!(eval("new TextDecoder().ignoreBOM"), "false");
}

#[test]
fn text_decoder_ignore_bom_true() {
    assert_eq!(
        eval("new TextDecoder('utf-8', { ignoreBOM: true }).ignoreBOM"),
        "true"
    );
}

// ── TextDecoder.decode() ──

#[test]
fn text_decoder_decode_utf8() {
    assert_eq!(
        eval(r#"new TextDecoder().decode(new Uint8Array([97, 98, 99]))"#),
        "abc"
    );
}

#[test]
fn text_decoder_decode_no_args() {
    // decode() with no arguments returns empty string
    assert_eq!(eval("new TextDecoder().decode()"), "");
}

#[test]
fn text_decoder_decode_empty_array() {
    assert_eq!(eval("new TextDecoder().decode(new Uint8Array([]))"), "");
}

#[test]
fn text_decoder_decode_multibyte_utf8() {
    // "€" = [0xE2, 0x82, 0xAC]
    assert_eq!(
        eval("new TextDecoder().decode(new Uint8Array([0xE2, 0x82, 0xAC]))"),
        "€"
    );
}

#[test]
fn text_decoder_decode_emoji() {
    // "😀" = [0xF0, 0x9F, 0x98, 0x80]
    assert_eq!(
        eval("new TextDecoder().decode(new Uint8Array([0xF0, 0x9F, 0x98, 0x80]))"),
        "😀"
    );
}

// ── TextDecoder error handling ──

#[test]
fn text_decoder_invalid_label_throws() {
    assert!(throws("new TextDecoder('invalid-encoding')"));
}

#[test]
fn text_decoder_replacement_encoding_throws() {
    // "replacement" is not a valid label for TextDecoder (excluded by spec)
    assert!(throws("new TextDecoder('replacement')"));
}

#[test]
fn text_decoder_fatal_invalid_bytes() {
    // Invalid UTF-8 sequence with fatal: true should throw
    assert!(throws(
        "new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array([0xFF, 0xFE]))"
    ));
}

#[test]
fn text_decoder_replacement_mode_invalid_bytes() {
    // Invalid UTF-8 in replacement mode should produce U+FFFD
    assert_eq!(
        eval(
            r#"
            const decoder = new TextDecoder('utf-8');
            const result = decoder.decode(new Uint8Array([0xFF]));
            result === '\uFFFD'
        "#
        ),
        "true"
    );
}

// ── TextDecoder streaming ──

#[test]
fn text_decoder_streaming() {
    // Multi-byte character split across two decode() calls
    // "€" = [0xE2, 0x82, 0xAC]
    assert_eq!(
        eval(
            r#"
            const decoder = new TextDecoder();
            let result = decoder.decode(new Uint8Array([0xE2, 0x82]), { stream: true });
            result += decoder.decode(new Uint8Array([0xAC]));
            result
        "#
        ),
        "€"
    );
}

// ── TextDecoder with non-UTF-8 encodings ──

#[test]
fn text_decoder_windows_1252() {
    // 0xE9 in windows-1252 is "é"
    assert_eq!(
        eval(r#"new TextDecoder('windows-1252').decode(new Uint8Array([0xE9]))"#),
        "é"
    );
}

// ── WebIDL interface compliance ──

#[test]
fn text_encoder_to_string_tag() {
    assert_eq!(
        eval("Object.prototype.toString.call(new TextEncoder())"),
        "[object TextEncoder]"
    );
}

#[test]
fn text_decoder_to_string_tag() {
    assert_eq!(
        eval("Object.prototype.toString.call(new TextDecoder())"),
        "[object TextDecoder]"
    );
}

// ── TextEncoderStream ──

#[test]
fn text_encoder_stream_exists() {
    assert_eq!(eval("typeof TextEncoderStream"), "function");
}

#[test]
fn text_encoder_stream_encoding() {
    assert_eq!(eval("new TextEncoderStream().encoding"), "utf-8");
}

#[test]
fn text_encoder_stream_readable_writable() {
    assert_eq!(
        eval(
            r#"
            const s = new TextEncoderStream();
            (s.readable instanceof ReadableStream) + ',' + (s.writable instanceof WritableStream)
        "#
        ),
        "true,true"
    );
}

#[test]
fn text_encoder_stream_ascii() {
    let out = run_async(&format!(
        r#"{DRAIN_BYTES}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue("abc"); c.close(); }} }});
        const os = rs.pipeThrough(new TextEncoderStream());
        drainBytes(os).then(a => {{ globalThis.__out = a.join(","); }});
        "#
    ));
    assert_eq!(out, "97,98,99");
}

#[test]
fn text_encoder_stream_multibyte() {
    // "€" = [0xE2, 0x82, 0xAC]
    let out = run_async(&format!(
        r#"{DRAIN_BYTES}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue("€"); c.close(); }} }});
        const os = rs.pipeThrough(new TextEncoderStream());
        drainBytes(os).then(a => {{ globalThis.__out = a.join(","); }});
        "#
    ));
    assert_eq!(out, "226,130,172");
}

#[test]
fn text_encoder_stream_unpaired_surrogate() {
    // Lone leading surrogate produces replacement character [0xEF, 0xBF, 0xBD]
    let out = run_async(&format!(
        r#"{DRAIN_BYTES}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue("\uD83D"); c.close(); }} }});
        const os = rs.pipeThrough(new TextEncoderStream());
        drainBytes(os).then(a => {{ globalThis.__out = a.join(","); }});
        "#
    ));
    assert_eq!(out, "239,191,189");
}

#[test]
fn text_encoder_stream_surrogate_pair_split() {
    // Surrogates in separate chunks should still produce the correct encoding
    let out = run_async(&format!(
        r#"{DRAIN_BYTES}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue("\uD83D"); c.enqueue("\uDE00"); c.close(); }} }});
        const os = rs.pipeThrough(new TextEncoderStream());
        drainBytes(os).then(a => {{ globalThis.__out = a.join(","); }});
        "#
    ));
    assert_eq!(out, "240,159,152,128");
}

// ── TextDecoderStream ──

#[test]
fn text_decoder_stream_exists() {
    assert_eq!(eval("typeof TextDecoderStream"), "function");
}

#[test]
fn text_decoder_stream_encoding() {
    assert_eq!(eval("new TextDecoderStream().encoding"), "utf-8");
}

#[test]
fn text_decoder_stream_fatal() {
    assert_eq!(eval("new TextDecoderStream().fatal"), "false");
}

#[test]
fn text_decoder_stream_ignore_bom() {
    assert_eq!(eval("new TextDecoderStream().ignoreBOM"), "false");
}

#[test]
fn text_decoder_stream_readable_writable() {
    assert_eq!(
        eval(
            r#"
            const s = new TextDecoderStream();
            (s.readable instanceof ReadableStream) + ',' + (s.writable instanceof WritableStream)
        "#
        ),
        "true,true"
    );
}

#[test]
fn text_decoder_stream_invalid_label_throws() {
    assert!(throws("new TextDecoderStream('invalid-encoding')"));
}

#[test]
fn text_decoder_stream_ascii() {
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([97, 98, 99])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream());
        drainStr(os).then(a => {{ globalThis.__out = a.join("|"); }});
        "#
    ));
    assert_eq!(out, "abc");
}

#[test]
fn text_decoder_stream_multibyte() {
    // "€" = [0xE2, 0x82, 0xAC]
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xE2, 0x82, 0xAC])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream());
        drainStr(os).then(a => {{ globalThis.__out = a.join("|"); }});
        "#
    ));
    assert_eq!(out, "€");
}

#[test]
fn text_decoder_stream_split_multibyte() {
    // "€" = [0xE2, 0x82, 0xAC], split across two chunks
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xE2, 0x82])); c.enqueue(new Uint8Array([0xAC])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream());
        drainStr(os).then(a => {{ globalThis.__out = a.join("|"); }});
        "#
    ));
    assert_eq!(out, "€");
}

#[test]
fn text_decoder_stream_split_4byte() {
    // "😀" = [0xF0, 0x9F, 0x98, 0x80], split after first byte
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xF0])); c.enqueue(new Uint8Array([0x9F, 0x98, 0x80])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream());
        drainStr(os).then(a => {{ globalThis.__out = a.join("|"); }});
        "#
    ));
    assert_eq!(out, "😀");
}

#[test]
fn text_decoder_stream_incomplete_at_eof() {
    // Incomplete 4-byte sequence at EOF produces replacement character(s)
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xF0, 0x9F, 0x98])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream());
        drainStr(os).then(a => {{ globalThis.__out = a.join("").length; }});
        "#
    ));
    assert_eq!(out, "1");
}

#[test]
fn text_decoder_stream_fatal_invalid_bytes() {
    // fatal: true should reject the readable stream on invalid bytes
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xFF, 0xFE])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream("utf-8", {{ fatal: true }}));
        drainStr(os).then(
            () => {{ globalThis.__out = "fulfilled"; }},
            e => {{ globalThis.__out = "rejected:" + e.name; }}
        );
        "#
    ));
    assert_eq!(out, "rejected:TypeError");
}

// -- BOM handling --

#[test]
fn text_decoder_stream_bom_default() {
    // ignoreBOM: false (default) → BOM is removed
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xEF, 0xBB, 0xBF, 97])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream());
        drainStr(os).then(a => {{ globalThis.__out = a.join(""); }});
        "#
    ));
    assert_eq!(out, "a");
}

#[test]
fn text_decoder_stream_bom_ignored() {
    // ignoreBOM: true → BOM is kept as U+FEFF
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xEF, 0xBB, 0xBF, 97])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream("utf-8", {{ ignoreBOM: true }}));
        drainStr(os).then(a => {{ globalThis.__out = a[0].charCodeAt(0) + "," + a[0][1]; }});
        "#
    ));
    assert_eq!(out, "65279,a");
}

#[test]
fn text_decoder_stream_bom_split_across_chunks() {
    // BOM bytes split across two chunks, ignoreBOM: true → BOM kept as U+FEFF
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{
            c.enqueue(new Uint8Array([0xEF, 0xBB]));
            c.enqueue(new Uint8Array([0xBF, 97]));
            c.close();
        }} }});
        const os = rs.pipeThrough(new TextDecoderStream("utf-8", {{ ignoreBOM: true }}));
        drainStr(os).then(a => {{ globalThis.__out = a[0].charCodeAt(0) + "," + a[0][1]; }});
        "#
    ));
    assert_eq!(out, "65279,a");
}

#[test]
fn text_decoder_stream_bom_only() {
    // BOM only, ignoreBOM: true → emits U+FEFF
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xEF, 0xBB, 0xBF])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream("utf-8", {{ ignoreBOM: true }}));
        drainStr(os).then(a => {{ globalThis.__out = a[0].charCodeAt(0); }});
        "#
    ));
    assert_eq!(out, "65279");
}

#[test]
fn text_decoder_stream_bom_only_default() {
    // BOM only, ignoreBOM: false (default) → no output
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xEF, 0xBB, 0xBF])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream());
        drainStr(os).then(a => {{ globalThis.__out = a.length; }});
        "#
    ));
    assert_eq!(out, "0");
}

#[test]
fn text_decoder_stream_windows_1252() {
    // 0xE9 in windows-1252 is "é"
    let out = run_async(&format!(
        r#"{DRAIN_STR}
        globalThis.__out = "pending";
        const rs = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array([0xE9])); c.close(); }} }});
        const os = rs.pipeThrough(new TextDecoderStream("windows-1252"));
        drainStr(os).then(a => {{ globalThis.__out = a.join("|"); }});
        "#
    ));
    assert_eq!(out, "é");
}
