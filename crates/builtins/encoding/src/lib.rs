// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! WHATWG Encoding Standard
//! <https://encoding.spec.whatwg.org/>
//!
//! Provides `TextEncoder`, `TextDecoder`, `TextEncoderStream`, and `TextDecoderStream`.

mod decoder_common;
mod text_decoder;
mod text_decoder_stream;
mod text_encoder;
mod text_encoder_stream;

use js::gc::scope::Scope;
use js::Object;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    text_encoder::TextEncoder::add_to_global(scope, global);
    text_decoder::TextDecoder::add_to_global(scope, global);
    text_decoder_stream::TextDecoderStream::add_to_global(scope, global);
    text_encoder_stream::TextEncoderStream::add_to_global(scope, global);
}
