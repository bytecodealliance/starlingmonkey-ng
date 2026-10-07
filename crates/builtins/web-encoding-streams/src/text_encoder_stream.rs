// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#interface-textencoderstream>

use std::collections::TryReserveError;

use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::string::StrChars;
use js::{JSString, Uint8Array};
use web_streams::readable::ReadableStream;
use web_streams::transform::transform_stream::{TransformStream, TransformStreamImpl};
use web_streams::writable::WritableStream;
use web_streams::AlgorithmArg;

/// U+FFFD in UTF-8.
const REPLACEMENT_CHARACTER: [u8; 3] = [0xEF, 0xBF, 0xBD];

/// <https://encoding.spec.whatwg.org/#textencoderstream>
#[webidl_interface]
pub struct TextEncoderStream {
    /// <https://encoding.spec.whatwg.org/#textencoderstream-pending-high-surrogate>
    #[no_trace]
    leading_surrogate: Option<u16>,
    /// <https://streams.spec.whatwg.org/#generictransformstream-transform>, set by the
    /// constructor.
    transform: Option<Heap<TransformStreamImpl>>,
}

#[webidl_methods]
impl TextEncoderStream {
    /// <https://encoding.spec.whatwg.org/#dom-textencoderstream>
    #[constructor]
    fn new(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: Set `this`'s `encoder` to an instance of the `UTF-8 encoder`.
        // The encoder keeps no state.
        // Step 2: Let _transformAlgorithm_ be an algorithm which takes a _chunk_ argument and runs
        //         the `encode and enqueue a chunk` algorithm with `this` and _chunk_.
        let transform_algorithm = AlgorithmArg::native(scope, transform_algorithm, *self)?;
        // Step 3: Let _flushAlgorithm_ be an algorithm which runs the `encode and flush`
        //         algorithm with `this`.
        let flush_algorithm = AlgorithmArg::native(scope, flush_algorithm, *self)?;
        // Step 4: Let _transformStream_ be a `new` ``TransformStream``.
        // Step 5: `Set up` _transformStream_ with `transformAlgorithm` set to
        //         _transformAlgorithm_ and `flushAlgorithm` set to _flushAlgorithm_.
        let transform = TransformStream::new_native(
            scope,
            transform_algorithm,
            flush_algorithm,
            AlgorithmArg::None,
        )?;
        // Step 6: Set `this`'s `transform` to _transformStream_.
        self.data_mut().transform.set(transform);
        Ok(())
    }

    /// <https://encoding.spec.whatwg.org/#dom-textencoder-encoding>
    #[getter]
    fn encoding(&self) -> String {
        "utf-8".into()
    }

    /// <https://streams.spec.whatwg.org/#dom-generictransformstream-readable>
    #[getter]
    fn readable<'r>(&self, scope: &'r Scope<'_>) -> ReadableStream<'r> {
        self.transform(scope).readable(scope)
    }

    /// <https://streams.spec.whatwg.org/#dom-generictransformstream-writable>
    #[getter]
    fn writable<'r>(&self, scope: &'r Scope<'_>) -> WritableStream<'r> {
        self.transform(scope).writable(scope)
    }

    fn transform<'r>(&self, scope: &'r Scope<'_>) -> TransformStream<'r> {
        self.data()
            .transform
            .get(scope)
            .expect("set by the constructor")
    }

    /// Enqueue `bytes` as a `Uint8Array` in the stream's transform.
    fn enqueue(&self, scope: &Scope<'_>, bytes: &[u8]) -> Result<(), ExnThrown> {
        let chunk = Uint8Array::with_data(scope, bytes)?;
        let transform = self.transform(scope);
        transform.enqueue(scope, scope.root_value(chunk.as_value()))
    }

    /// <https://encoding.spec.whatwg.org/#encode-and-enqueue-a-chunk>
    fn encode_and_enqueue_a_chunk(
        &self,
        scope: &Scope<'_>,
        chunk: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        // Step 1: Let _input_ be the result of `converting` _chunk_ to a ``DOMString``.
        let input = JSString::from_value(scope, chunk)?;
        // Step 2: `Convert` _input_ to an `I/O queue` of code units.
        // Step 3: Let _output_ be the `I/O queue` of bytes « `end-of-queue` ».
        // Step 4: While true:
        // Step 4.1: Let _item_ be the result of `reading` from _input_.
        // Step 4.2: If _item_ is `end-of-queue`:
        // Step 4.2.1: `Convert` _output_ into a byte sequence.
        // Step 4.2.2: If _output_ is not empty:
        // Step 4.2.2.1: Let _chunk_ be the result of `creating` a ``Uint8Array`` object given
        //               _output_ and _encoder_'s `relevant realm`.
        // Step 4.2.2.2: `Enqueue` _chunk_ into _encoder_'s `transform`.
        // Step 4.2.3: Return.
        // Step 4.3: Let _result_ be the result of executing the `convert code unit to scalar
        //           value` algorithm with _encoder_, _item_ and _input_.
        // Step 4.4: If _result_ is not `continue`, then `process an item` with _result_,
        //           _encoder_'s `encoder`, _input_, _output_, and "`fatal`".
        // `encode_code_units` runs the loop over all of _input_'s code units.
        let leading = self.data().leading_surrogate;
        // SAFETY: `encode_code_units` doesn't call into SpiderMonkey.
        let encoded =
            unsafe { input.with_chars(scope, |chars| encode_code_units(chars, leading)) }?;
        let Ok((output, leading)) = encoded else {
            js::exception::report_out_of_memory(scope);
            return Err(ExnThrown);
        };
        self.data_mut().leading_surrogate = leading;
        if output.is_empty() {
            return Ok(());
        }
        self.enqueue(scope, &output)
    }

    /// <https://encoding.spec.whatwg.org/#encode-and-flush>
    fn encode_and_flush(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: If _encoder_'s `leading surrogate` is non-null:
        if self.data().leading_surrogate.is_some() {
            // Step 1.1: Let _chunk_ be the result of `creating` a ``Uint8Array`` object given «
            //           0xEF, 0xBF, 0xBD » and _encoder_'s `relevant realm`.
            // Step 1.2: `Enqueue` _chunk_ into _encoder_'s `transform`.
            self.enqueue(scope, &REPLACEMENT_CHARACTER)?;
        }
        Ok(())
    }
}

/// The UTF-8 bytes of the code units `chars` following the leading surrogate `leading` of the
/// previous chunk, and the leading surrogate that ends `chars`, which the bytes leave out. Unpaired
/// surrogates become U+FFFD. Fails if the output can't be allocated.
///
/// This is the `encode and enqueue a chunk` loop, which runs
/// <https://encoding.spec.whatwg.org/#convert-code-unit-to-scalar-value> on each code unit.
fn encode_code_units(
    chars: StrChars<'_>,
    leading: Option<u16>,
) -> Result<(Vec<u8>, Option<u16>), TryReserveError> {
    let length = match chars {
        StrChars::Latin1(chars) => chars.len(),
        StrChars::TwoByte(chars) => chars.len(),
    };
    if length == 0 {
        return Ok((Vec::new(), leading));
    }
    // A code unit becomes at most three bytes, and the leading surrogate and a trailing one four.
    let mut output = Vec::new();
    output.try_reserve_exact(length.saturating_mul(3).saturating_add(4))?;
    match chars {
        StrChars::Latin1(chars) => {
            if leading.is_some() {
                // A Latin-1 character isn't a trailing surrogate.
                output.extend_from_slice(&REPLACEMENT_CHARACTER);
            }
            let start = output.len();
            output.resize(start + chars.len() * 2, 0);
            let written = encoding_rs::mem::convert_latin1_to_utf8(chars, &mut output[start..]);
            output.truncate(start + written);
            Ok((output, None))
        }
        StrChars::TwoByte(mut chars) => {
            if let Some(leading) = leading {
                if is_trailing_surrogate(chars[0]) {
                    let pair = char::decode_utf16([leading, chars[0]])
                        .next()
                        .and_then(Result::ok)
                        .expect("a leading and a trailing surrogate form a scalar value");
                    let mut buffer = [0; 4];
                    output.extend_from_slice(pair.encode_utf8(&mut buffer).as_bytes());
                    chars = &chars[1..];
                } else {
                    output.extend_from_slice(&REPLACEMENT_CHARACTER);
                }
            }
            let mut leading = None;
            if let Some((&last, rest)) = chars.split_last() {
                if is_leading_surrogate(last) {
                    leading = Some(last);
                    chars = rest;
                }
            }
            let start = output.len();
            output.resize(start + chars.len() * 3, 0);
            let written = encoding_rs::mem::convert_utf16_to_utf8(chars, &mut output[start..]);
            output.truncate(start + written);
            Ok((output, leading))
        }
    }
}

fn is_leading_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_trailing_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

/// The `transformAlgorithm` of a `TextEncoderStream`, whose payload is the stream.
fn transform_algorithm(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TextEncoderStream>(scope, payload);
    stream.encode_and_enqueue_a_chunk(scope, args.get(0))?;
    Ok(js::value::undefined())
}

/// The `flushAlgorithm` of a `TextEncoderStream`, whose payload is the stream.
fn flush_algorithm(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TextEncoderStream>(scope, payload);
    stream.encode_and_flush(scope)?;
    Ok(js::value::undefined())
}
