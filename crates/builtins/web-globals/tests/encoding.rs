// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A `TextDecoder` check that the enabled WPT `encoding/` tests don't make.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::test_util::eval_with_setup;

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        web_globals::add_to_global(scope, global);
    });
}

fn eval(code: &str) -> String {
    eval_with_setup(setup, code)
}

/// windows-1252 input without bytes in 0x80..=0x9F decodes to the same code points, and input
/// with them maps those bytes through the encoding's table, including when a streaming decode
/// alternates between the two.
#[test]
fn text_decoder_windows_1252_maps_every_byte() {
    assert_eq!(
        eval(
            "const d = new TextDecoder('latin1');
             const codes = s => Array.from(s, c => c.charCodeAt(0)).join(',');
             [d.decode(new Uint8Array([0x41, 0xe9, 0xff, 0xa0])),
              d.decode(new Uint8Array([0x41, 0x80, 0x9f, 0xe9])),
              d.decode(new Uint8Array([0xe9]), { stream: true }) +
                d.decode(new Uint8Array([0x80]), { stream: true }) + d.decode(),
              d.decode(new Uint8Array([]))].map(codes).join(' | ')"
        ),
        "65,233,255,160 | 65,8364,376,233 | 233,8364 | "
    );
}
