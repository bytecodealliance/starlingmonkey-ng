// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#textdecoder>

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods, webidl_union};
use js::error::{ExnThrown, RangeError};
use js::gc::scope::Scope;
use js::{ArrayBuffer, ArrayBufferView, JSString};

/// <https://encoding.spec.whatwg.org/#textdecoder>
#[webidl_interface]
pub struct TextDecoder {
    /// <https://encoding.spec.whatwg.org/#textdecoder-encoding>
    /// (always `Some` after construction).
    #[no_trace]
    encoding: Option<&'static encoding_rs::Encoding>,
    /// <https://encoding.spec.whatwg.org/#textdecoder-decoder>, which also holds the I/O queue
    /// and BOM seen state. `None` until the first `decode()` call.
    #[no_trace]
    decoder: Option<encoding_rs::Decoder>,
    /// <https://encoding.spec.whatwg.org/#textdecoder-error-mode>: whether it is "fatal".
    fatal: bool,
    /// <https://encoding.spec.whatwg.org/#textdecoder-ignore-bom>
    ignore_bom: bool,
    /// <https://encoding.spec.whatwg.org/#textdecoder-do-not-flush-flag>
    do_not_flush: bool,
}

#[webidl_methods]
impl TextDecoder<'_> {
    /// <https://encoding.spec.whatwg.org/#dom-textdecoder>
    #[constructor]
    fn new(
        &self,
        label: Option<String>,
        options: Option<TextDecoderOptions>,
    ) -> Result<(), RangeError> {
        let label = label.as_deref().unwrap_or("utf-8");
        let options = options.unwrap_or_default();

        // Step 1: Let _encoding_ be the result of `getting an encoding` from _label_.
        // Step 2: If _encoding_ is failure or `replacement`, then `throw` a ``RangeError``.
        let encoding = encoding_for_label(label)?;

        let mut data = self.data_mut();
        // Step 3: Set `this`'s `encoding` to _encoding_.
        data.encoding = Some(encoding);
        // Step 4: If _options_["``fatal``"] is true, then set `this`'s `error mode` to
        //         "`fatal`".
        data.fatal = options.fatal;
        // Step 5: Set `this`'s `ignore BOM` to _options_["``ignoreBOM``"].
        data.ignore_bom = options.ignore_bom;

        Ok(())
    }

    /// <https://encoding.spec.whatwg.org/#dom-textdecoder-encoding>
    #[getter]
    fn encoding(&self) -> String {
        // Step 1: Return `this`'s `encoding`'s `name`, `ASCII lowercased`.
        let encoding = self.data().encoding.expect("set by the constructor");
        encoding.name().to_ascii_lowercase()
    }

    /// <https://encoding.spec.whatwg.org/#dom-textdecoder-fatal>
    #[getter(name = "fatal")]
    fn get_fatal(&self) -> bool {
        // Step 1: Return true if `this`'s `error mode` is "`fatal`"; otherwise false.
        self.data().fatal
    }

    /// <https://encoding.spec.whatwg.org/#dom-textdecoder-ignorebom>
    #[getter(name = "ignoreBOM")]
    fn ignore_bom(&self) -> bool {
        // Step 1: Return `this`'s `ignore BOM`.
        self.data().ignore_bom
    }

    /// <https://encoding.spec.whatwg.org/#dom-textdecoder-decode>
    #[method]
    fn decode<'r>(
        &self,
        scope: &'r Scope<'_>,
        input: Option<ArrayBufferViewOrArrayBuffer<'_>>,
        options: Option<TextDecodeOptions>,
    ) -> Result<JSString<'r>, ExnThrown> {
        let mut data = self.data_mut();
        let encoding = data.encoding.expect("set by the constructor");
        // Step 1: If `this`'s `do not flush` is false, then set `this`'s `decoder` to a new
        //         instance of `this`'s `encoding`'s `decoder`, `this`'s `I/O queue` to the `I/O
        //         queue` of bytes « `end-of-queue` », and `this`'s `BOM seen` to false.
        //         (The `encoding_rs` decoder holds the I/O queue and BOM seen state.)
        let fresh = !data.do_not_flush;
        if fresh {
            data.decoder = Some(new_decoder(encoding, data.ignore_bom));
        }
        // Step 2: Set `this`'s `do not flush` to _options_["``stream``"].
        data.do_not_flush = options.is_some_and(|o| o.stream);
        let last = !data.do_not_flush;
        let fatal = data.fatal;
        let decoder = data.decoder.as_mut().expect("set by step 1");

        // Step 3: If _input_ is given, then `push` a `copy of` _input_ to `this`'s `I/O queue`.
        //         (We decode directly from _input_'s bytes. Nothing retains them after this call.)
        // SAFETY: `input` is rooted via the Uint8Array handle and the slice
        // is only borrowed while `encoding_rs` decodes it and `latin1_string`
        // copies it, which can't trigger GC or detach the underlying buffer.
        let bytes: &[u8] = match input {
            Some(ArrayBufferViewOrArrayBuffer::Buffer(input)) => unsafe { input.bytes() },
            Some(ArrayBufferViewOrArrayBuffer::View(input)) => unsafe { input.bytes() },
            None => &[],
        };

        // windows-1252 decodes every byte outside 0x80..=0x9F to the code point of the same
        // value, and its decoder keeps no state between calls, so such input is its own
        // Latin-1 output.
        if encoding == encoding_rs::WINDOWS_1252 && !bytes.iter().any(|b| (0x80..=0x9F).contains(b))
        {
            return latin1_string(scope, bytes);
        }
        // A fresh decoder that decodes its whole input at once turns ASCII into the same
        // characters in every ASCII-compatible encoding.
        if fresh
            && last
            && encoding.is_ascii_compatible()
            && encoding_rs::Encoding::ascii_valid_up_to(bytes) == bytes.len()
        {
            return latin1_string(scope, bytes);
        }

        // Step 4: Let _output_ be the `I/O queue` of scalar values « `end-of-queue` ».
        // Step 5: While true:
        // Step 5.1: Let _item_ be the result of `reading` from `this`'s `I/O queue`.
        // Step 5.2: If _item_ is `end-of-queue` and `this`'s `do not flush` is true, then return
        //           the result of running `serialize I/O queue` with `this` and _output_.
        // Step 5.3: Otherwise:
        // Step 5.3.1: Let _result_ be the result of `processing an item` with _item_, `this`'s
        //             `decoder`, `this`'s `I/O queue`, _output_, and `this`'s `error mode`.
        // Step 5.3.2: If _result_ is `finished`, then return the result of running `serialize
        //             I/O queue` with `this` and _output_.
        // Step 5.3.3: Otherwise, if _result_ is `error`, `throw` a ``TypeError``.
        //
        // (One `encoding_rs` call processes every item. `last` is true when `end-of-queue` is
        // processed rather than returned at step 5.2. The decoder's BOM removal performs
        // `serialize I/O queue`'s BOM handling. We decode to UTF-16, which SpiderMonkey copies
        // as is, or narrows to Latin-1 if every code unit fits.)
        let output = decode_to_utf16(decoder, bytes, last, fatal);
        drop(data);
        JSString::from_utf16(scope, &output.map_err(|error| error.throw(scope))?)
    }
}

/// A string of the Latin-1 characters `bytes`. The bytes are copied before the string is created,
/// because creating it can run a GC, which moves the bytes a typed array holds inline.
fn latin1_string<'r>(scope: &'r Scope<'_>, bytes: &[u8]) -> Result<JSString<'r>, ExnThrown> {
    let mut copy = Vec::new();
    if copy.try_reserve_exact(bytes.len()).is_err() {
        return Err(DecodeError::OutOfMemory.throw(scope));
    }
    copy.extend_from_slice(bytes);
    JSString::from_latin1(scope, &copy)
}

/// The encoding `label` names, or a `RangeError` if it names none or `replacement`.
pub fn encoding_for_label(label: &str) -> Result<&'static encoding_rs::Encoding, RangeError> {
    encoding_rs::Encoding::for_label_no_replacement(label.as_bytes()).ok_or_else(|| {
        RangeError(format!(
            "The encoding label provided ('{label}') is invalid."
        ))
    })
}

/// A new decoder of `encoding`, which removes a byte order mark at the start of its input unless
/// `ignore_bom` is true.
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

/// Why [`decode_to_utf16`] failed.
pub enum DecodeError {
    /// The input is malformed, in the "fatal" error mode.
    Malformed,
    /// The output can't be allocated.
    OutOfMemory,
}

impl DecodeError {
    /// Throw the `TypeError` of malformed input, or report the failed allocation.
    pub fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        match self {
            DecodeError::Malformed => {
                js::error::throw_type_error(scope, c"The encoded data was not valid.")
            }
            DecodeError::OutOfMemory => {
                js::exception::report_out_of_memory(scope);
                ExnThrown
            }
        }
    }
}

/// Decode `bytes` with `decoder` to UTF-16. `last` is true for the end of the input, which the
/// decoder then flushes. With `fatal`, malformed input fails, and otherwise it becomes U+FFFD.
pub fn decode_to_utf16(
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

#[webidl_dictionary]
pub struct TextDecodeOptions {
    #[webidl(default = false)]
    pub stream: bool,
}

#[webidl_dictionary]
#[derive(Default)]
pub struct TextDecoderOptions {
    #[webidl(default = false)]
    pub fatal: bool,
    #[webidl(default = false, name = "ignoreBOM")]
    pub ignore_bom: bool,
}
