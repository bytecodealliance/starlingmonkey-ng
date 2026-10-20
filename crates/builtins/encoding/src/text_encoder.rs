// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#textencoder>

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::conversion::{ConversionError, ToJSVal};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::string::StrChars;
use js::{JSString, Uint8Array};

/// <https://encoding.spec.whatwg.org/#textencoder>
///
/// The TextEncoder interface always encodes to UTF-8.
#[webidl_interface]
pub struct TextEncoder {}

#[webidl_methods]
impl TextEncoder<'_> {
    /// <https://encoding.spec.whatwg.org/#dom-textencoder>
    #[constructor]
    fn new() -> Self {
        // Step 1: (No-op -- TextEncoder has no internal state.)
        TextEncoderImpl {}
    }

    /// <https://encoding.spec.whatwg.org/#dom-textencoder-encoding>
    #[getter]
    fn encoding(&self) -> String {
        // Step 1: Return "utf-8".
        "utf-8".into()
    }

    /// <https://encoding.spec.whatwg.org/#dom-textencoder-encode>
    #[method]
    fn encode<'r>(
        &self,
        scope: &'r Scope<'_>,
        input: Option<String>,
    ) -> Result<Uint8Array<'r>, ExnThrown> {
        // Rust strings are already valid UTF-8; the SpiderMonkey-side conversion
        // has replaced any lone surrogates with U+FFFD, so the bytes here are
        // exactly what the spec calls for.
        let input = input.as_deref().unwrap_or("");
        Uint8Array::with_data(scope, input.as_bytes())
    }

    /// <https://encoding.spec.whatwg.org/#dom-textencoder-encodeinto>
    #[method]
    fn encode_into(
        &self,
        scope: &Scope<'_>,
        source: JSString<'_>,
        destination: Uint8Array,
    ) -> Result<TextEncoderEncodeIntoResult, ExnThrown> {
        // Step 1: Let _read_ be 0.
        // Step 2: Let _written_ be 0.
        // Step 3: Let _encoder_ be an instance of the `UTF-8 encoder`.
        // Step 4: Let _unused_ be the `I/O queue` of scalar values « `end-of-queue` ».
        // Step 5: `Convert` _source_ to an `I/O queue` of scalar values.
        // Step 6: While true:
        // Step 6.1: Let _item_ be the result of `reading` from _source_.
        // Step 6.2: Let _result_ be the result of running _encoder_'s `handler` on _unused_ and
        //           _item_.
        // Step 6.3: If _result_ is `finished`, then `break`.
        // Step 6.4: Otherwise:
        // Step 6.4.1: If _destination_'s `byte length` − _written_ is greater than or equal to
        //             the number of bytes in _result_:
        // Step 6.4.1.1: If _item_ is greater than U+FFFF, then increment _read_ by 2.
        // Step 6.4.1.2: Otherwise, increment _read_ by 1.
        // Step 6.4.1.3: `Write` the bytes in _result_ into _destination_, with _startingOffset_
        //               set to _written_.
        // Step 6.4.1.4: Increment _written_ by the number of bytes in _result_.
        // Step 6.4.2: Otherwise, `break`.
        //
        // (`encoding_rs`' partial converters run this loop over _source_'s characters: they
        // encode whole scalar values while they fit, replace unpaired surrogates with U+FFFD as
        // step 5's conversion does, and return _read_ in code units and _written_ in bytes.)
        //
        // SAFETY: the destination slice is only borrowed for the conversion, which cannot run JS
        // or trigger a GC.
        let dest_buf = unsafe { destination.as_mut_slice() };
        // SAFETY: same as above.
        let (read, written) = unsafe {
            source.with_chars(scope, |chars| match chars {
                StrChars::Latin1(chars) => {
                    encoding_rs::mem::convert_latin1_to_utf8_partial(chars, dest_buf)
                }
                StrChars::TwoByte(chars) => {
                    encoding_rs::mem::convert_utf16_to_utf8_partial(chars, dest_buf)
                }
            })
        }?;

        // Step 7: Return «[ "``read``" → _read_, "``written``" → _written_ ]».
        Ok(TextEncoderEncodeIntoResult {
            read: read as u64,
            written: written as u64,
        })
    }
}

/// <https://encoding.spec.whatwg.org/#dictdef-textencoderencodeintoresult>
#[webidl_dictionary]
pub struct TextEncoderEncodeIntoResult {
    pub read: u64,
    pub written: u64,
}

impl<'s> ToJSVal<'s> for TextEncoderEncodeIntoResult {
    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'s>) -> Result<js::value::Value, ConversionError> {
        let obj = js::Object::new_plain(scope)?;
        let attrs = js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint;
        let read = js::class::get_or_init_property_id(scope, c"read")
            .map_err(|_| ConversionError::ExnPending)?;
        obj.define_value_by_id(scope, read, self.read, attrs)
            .map_err(|_| ConversionError::ExnPending)?;
        let written = js::class::get_or_init_property_id(scope, c"written")
            .map_err(|_| ConversionError::ExnPending)?;
        obj.define_value_by_id(scope, written, self.written, attrs)
            .map_err(|_| ConversionError::ExnPending)?;
        Ok(obj.as_value())
    }
}
