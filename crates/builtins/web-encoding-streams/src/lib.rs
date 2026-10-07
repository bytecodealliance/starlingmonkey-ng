// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `TextEncoderStream` and `TextDecoderStream` interfaces of
//! <https://encoding.spec.whatwg.org/>, over the encoders and decoders of `TextEncoder` and
//! `TextDecoder`.

pub mod text_decoder_stream;
pub mod text_encoder_stream;

use js::gc::scope::Scope;
use js::Object;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    text_encoder_stream::TextEncoderStream::add_to_global(scope, global);
    text_decoder_stream::TextDecoderStream::add_to_global(scope, global);
}
