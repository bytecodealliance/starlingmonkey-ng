// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

/// Creates a new streaming decoder for the given encoding, with or without BOM handling.
///
/// Per the Encoding spec: ignoreBOM=true means "ignore the BOM's special meaning"
/// (treat it as a regular U+FEFF character). ignoreBOM=false (default) means the
/// BOM is detected and removed.
pub fn new_decoder(
    encoding: &'static encoding_rs::Encoding,
    ignore_bom: bool,
) -> encoding_rs::Decoder {
    if ignore_bom {
        encoding.new_decoder_without_bom_handling()
    } else {
        encoding.new_decoder_with_bom_removal()
    }
}


/// Options dictionary for TextDecoder and TextDecoderStream constructors.
#[core_runtime::webidl_dictionary]
#[derive(Default)]
pub struct TextDecoderOptions {
    /// <https://encoding.spec.whatwg.org/#dom-textdecoderoptions-fatal>
    /// When true, sets the error mode to "fatal", causing the decoder to throw
    /// a TypeError on any malformed input. When false (default), the error mode
    /// is "replacement", replacing malformed sequences with U+FFFD.
    #[webidl(default = false)]
    pub fatal: bool,

    /// <https://encoding.spec.whatwg.org/#dom-textdecoderoptions-ignorebom>
    /// When true, the decoder ignores the byte order mark (BOM). When false
    /// (default), the decoder detects and removes the BOM. For UTF-16 encodings,
    /// the BOM also determines byte order.
    #[webidl(default = false, name = "ignoreBOM")]
    pub ignore_bom: bool,
}
