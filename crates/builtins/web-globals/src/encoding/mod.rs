// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

mod text_decoder;
mod text_encoder;

pub use text_decoder::{
    decode_to_utf16, encoding_for_label, new_decoder, ArrayBufferViewOrArrayBuffer, DecodeError,
    TextDecoderOptions,
};

use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::Object;

/// The realm's `TextEncoder` constructor.
pub fn text_encoder_constructor<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::class_constructor::<text_encoder::TextEncoderImpl>(scope)
}

/// The realm's `TextDecoder` constructor.
pub fn text_decoder_constructor<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::class_constructor::<text_decoder::TextDecoderImpl>(scope)
}

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    text_encoder::TextEncoder::add_to_global(scope, global);
    text_decoder::TextDecoder::add_to_global(scope, global);
}
