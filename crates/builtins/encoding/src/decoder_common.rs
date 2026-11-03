// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

use core_runtime::webidl_union;
use js::error::{ExnThrown, OutOfMemory, ThrowException};
use js::gc::scope::Scope;
use js::{ArrayBuffer, ArrayBufferView, JSString};

pub(crate) fn get_encoding(
    label: Option<&str>,
) -> Result<&'static encoding_rs::Encoding, js::error::RangeError> {
    let label = label.unwrap_or("utf-8");
    encoding_rs::Encoding::for_label_no_replacement(label.as_bytes()).ok_or_else(|| {
        js::error::RangeError(format!(
            "The encoding label provided ('{label}') is invalid."
        ))
    })
}

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

pub(crate) fn decode_bytes(
    encoding: &'static encoding_rs::Encoding,
    decoder: &mut encoding_rs::Decoder,
    fatal: bool,
    bytes: &[u8],
    last: bool,
    fresh: bool,
) -> Decoded {
    // windows-1252 decodes every byte outside 0x80..=0x9F to the code point of the same
    // value, and its decoder keeps no state between calls, so such input is its own
    // Latin-1 output.
    if encoding == encoding_rs::WINDOWS_1252 && !bytes.iter().any(|b| (0x80..=0x9F).contains(b)) {
        return Decoded::Latin1(js::typedarray::try_copy(bytes));
    }
    // A fresh decoder that decodes its whole input at once turns ASCII into the same
    // characters in every ASCII-compatible encoding.
    if fresh
        && last
        && encoding.is_ascii_compatible()
        && encoding_rs::Encoding::ascii_valid_up_to(bytes) == bytes.len()
    {
        return Decoded::Latin1(js::typedarray::try_copy(bytes));
    }

    // Step 4: Let _output_ be the `I/O queue` of scalar values « `end-of-queue` ».
    // Step 5: While true:
    // Step 5.1: Let _item_ be the result of `reading` from `this`'s `I/O queue`.
    // Step 5.2: If _item_ is `end-of-queue` and `this`'s `do not flush` is true, then
    //           return the result of running `serialize I/O queue` with `this` and
    //           _output_.
    // Step 5.3: Otherwise:
    // Step 5.3.1: Let _result_ be the result of `processing an item` with _item_,
    //             `this`'s `decoder`, `this`'s `I/O queue`, _output_, and `this`'s `error
    //             mode`.
    // Step 5.3.2: If _result_ is `finished`, then return the result of running
    //             `serialize I/O queue` with `this` and _output_.
    // Step 5.3.3: Otherwise, if _result_ is `error`, `throw` a ``TypeError``.
    //
    // (One `encoding_rs` call processes every item. `last` is true when `end-of-queue`
    // is processed rather than returned at step 5.2. The decoder's BOM removal performs
    // `serialize I/O queue`'s BOM handling. We decode to UTF-16, which SpiderMonkey
    // copies as is, or narrows to Latin-1 if every code unit fits.)
    Decoded::Utf16(decode_to_utf16(decoder, bytes, last, fatal))
}

/// The owned output of decoding bytes, ready to convert to a JS string.
pub(crate) enum Decoded {
    /// A copy of the input, which is its own Latin-1 output. `None` means that allocating the
    /// copy failed.
    Latin1(Option<Vec<u8>>),
    /// The UTF-16 output.
    Utf16(Result<Vec<u16>, DecodeError>),
}

impl Decoded {
    pub(crate) fn into_js_string<'r>(
        self,
        scope: &'r Scope<'_>,
    ) -> Result<JSString<'r>, ExnThrown> {
        match self {
            Self::Latin1(Some(chars)) => JSString::from_latin1(scope, &chars),
            Self::Latin1(None) => Err(OutOfMemory.throw(scope)),
            Self::Utf16(output) => {
                let output = output.map_err(|error| error.throw(scope))?;
                JSString::from_utf16(scope, &output)
            }
        }
    }
}

/// Why [`decode_to_utf16`] failed.
pub(crate) enum DecodeError {
    /// The input is malformed, in the "fatal" error mode.
    Malformed,
    /// The output can't be allocated.
    OutOfMemory,
}

impl DecodeError {
    /// Throw the `TypeError` of malformed input, or report the failed allocation.
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        match self {
            DecodeError::Malformed => {
                js::error::throw_type_error(scope, c"The encoded data was not valid.")
            }
            DecodeError::OutOfMemory => OutOfMemory.throw(scope),
        }
    }
}

/// Decode `bytes` with `decoder` to UTF-16. `last` is true for the end of the input, which the
/// decoder then flushes. With `fatal`, malformed input fails, and otherwise it becomes U+FFFD.
fn decode_to_utf16(
    decoder: &mut encoding_rs::Decoder,
    bytes: &[u8],
    last: bool,
    fatal: bool,
) -> Result<Vec<u16>, DecodeError> {
    let max_len = decoder
        .max_utf16_buffer_length(bytes.len())
        .ok_or(DecodeError::OutOfMemory)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(max_len)
        .map_err(|_| DecodeError::OutOfMemory)?;
    output.resize(max_len, 0);
    let written = if fatal {
        let (result, _read, written) =
            decoder.decode_to_utf16_without_replacement(bytes, &mut output, last);
        match result {
            encoding_rs::DecoderResult::InputEmpty => written,
            encoding_rs::DecoderResult::Malformed(_, _) => return Err(DecodeError::Malformed),
            encoding_rs::DecoderResult::OutputFull => {
                unreachable!("output buffer was pre-allocated to max size");
            }
        }
    } else {
        let (_result, _read, written, _had_errors) =
            decoder.decode_to_utf16(bytes, &mut output, last);
        written
    };
    output.truncate(written);
    Ok(output)
}

#[webidl_union]
pub enum ArrayBufferViewOrArrayBuffer<'a> {
    View(ArrayBufferView<'a>),
    Buffer(ArrayBuffer<'a>),
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
