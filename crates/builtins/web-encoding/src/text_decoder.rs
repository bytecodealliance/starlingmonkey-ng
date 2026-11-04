// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#textdecoder>

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::error::{ExnThrown, RangeError};
use js::gc::scope::Scope;
use js::JSString;

use crate::decoder_common::{
    decode_bytes, get_encoding, new_decoder, ArrayBufferViewOrArrayBuffer, TextDecoderOptions,
};

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
        let options = options.unwrap_or_default();

        // Step 1: Let _encoding_ be the result of `getting an encoding` from _label_.
        // Step 2: If _encoding_ is failure or `replacement`, then `throw` a ``RangeError``.
        let encoding = get_encoding(label.as_deref())?;

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
        let mut decode = |bytes: &[u8]| decode_bytes(encoding, decoder, fatal, bytes, last, fresh);
        let decoded = match input {
            Some(ArrayBufferViewOrArrayBuffer::Buffer(input)) => input.with_bytes(decode),
            Some(ArrayBufferViewOrArrayBuffer::View(input)) => input.with_bytes(decode),
            None => decode(&[]),
        };
        drop(data);
        decoded.into_js_string(scope)
    }
}

#[webidl_dictionary]
pub struct TextDecodeOptions {
    #[webidl(default = false)]
    pub stream: bool,
}
