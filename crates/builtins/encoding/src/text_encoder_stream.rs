// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#textencoderstream>

use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleValue};
use js::{value, Function, JSString, Object, Uint8Array};

use std::collections::VecDeque;
use web_streams::readable::ReadableStream;
use web_streams::{TransformStream, TransformStreamImpl};
use web_streams::writable::WritableStream;

/// Extract raw UTF-16 code units from a JS string, preserving surrogates.
///
/// <https://encoding.spec.whatwg.org/#encode-and-enqueue-a-chunk> step 1:
/// "Let input be the result of converting chunk to a DOMString."
fn js_string_to_utf16(scope: &Scope<'_>, js_string: &JSString) -> Vec<u16> {
    let length = js_string.len();
    if length == 0 {
        return Vec::new();
    }

    let mut result = Vec::with_capacity(length);
    for i in 0..length {
        result.push(js_string.char_at(scope, i).unwrap_or(0xFFFD));
    }
    result
}

/// <https://encoding.spec.whatwg.org/#textencoderstream>
#[webidl_interface]
pub struct TextEncoderStream {
    /// <https://encoding.spec.whatwg.org/#textencoderstream-pending-high-surrogate>
    /// Represents the slot [[pending high surrogate]].
    leading_surrogate: Option<u16>,

    /// The underlying TransformStream that handles the streaming pipeline.
    transform: Heap<TransformStreamImpl>,
}

#[webidl_methods]
impl TextEncoderStream {
    /// <https://encoding.spec.whatwg.org/#dom-textencoderstream>
    ///
    /// The `new TextEncoderStream()` constructor steps:
    #[constructor]
    fn new(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: Set this’s [[encoder]] to an instance of the UTF-8 encoder.
        // (Note: The UTF-8 encoder is stateless, so no explicit assignment is required).

        let self_payload = scope.root_value(self.as_value());

        // Step 2: Let transformAlgorithm be an algorithm which takes a chunk argument 
        // and runs the encode and enqueue a chunk algorithm with this and chunk.
        let transform_cb = Function::new_callback(
            scope,
            c"textEncoderTransform",
            1,
            encoder_transform_cb,
            self_payload,
        )?;

        // Step 3: Let flushAlgorithm be an algorithm which takes no arguments 
        // and runs the encode and flush algorithm with this.
        let flush_cb = Function::new_callback(
            scope,
            c"textEncoderFlush",
            0,
            encoder_flush_cb,
            self_payload,
        )?;

        // Step 4: Let transformStream be a new TransformStream.
        // Step 5: Set up transformStream with transformAlgorithm set to transformAlgorithm 
        // and flushAlgorithm set to flushAlgorithm.
        // We pass both algorithms as properties on a plain transformer object so that the
        // TransformStream constructor sees a real transformer and does not set the
        // identity-transform shortcut — otherwise `pipeTo` would bypass encoding for native
        // byte sources.
        let transformer = Object::new_plain(scope)?;
        transformer.set_property(scope, c"transform", transform_cb)?;
        transformer.set_property(scope, c"flush", flush_cb)?;
        let transform_stream = TransformStream::new(scope, Some(scope.root_value(transformer.as_value())), None, None)?;

        // Step 6: Set this’s [[TransformStream]] to transformStream.
        self.data_mut().transform.set(transform_stream);

        // Step 7: Set this’s [[pending high surrogate]] to null.
        self.data_mut().leading_surrogate = None;

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
        self.data().transform.get(scope).readable(scope)
    }

    /// <https://streams.spec.whatwg.org/#dom-generictransformstream-writable>
    #[getter]
    fn writable<'r>(&self, scope: &'r Scope<'_>) -> WritableStream<'r> {
        self.data().transform.get(scope).writable(scope)
    }
}

/// The <https://encoding.spec.whatwg.org/#encode-and-enqueue-a-chunk> algorithm.
fn encoder_transform_cb(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream: TextEncoderStream<'_> = Object::from_value(scope, *payload)
        .expect("payload is an object")
        .cast::<TextEncoderStream<'_>>()
        .expect("payload is a TextEncoderStream");

    // Step 1: Let input be the result of converting chunk to a DOMString.
    let js_string = JSString::from_value(scope, args.get(0))?;

    // Step 2: Convert input into an I/O queue of code units.
    let mut input = VecDeque::from(js_string_to_utf16(scope, &js_string));

    // Step 3: Let output be the I/O queue of bytes « end-of-queue ».
    let mut output: Vec<u8> = Vec::new();

    // Step 4: While true:
    loop {
        // Step 4.1: Let item be the result of reading from input.
        let item = input.pop_front();

        // Step 4.2: If item is end-of-queue:
        let Some(item) = item else {
            // Step 4.2.1: Convert output into a byte sequence.
            // Step 4.2.2: If output is not empty, then enqueue Uint8Array(output) in encoder’s [[TransformStream]].
            if !output.is_empty() {
                let u8array = Uint8Array::with_data(scope, &output)?;
                enqueue_to_transform(scope, &stream, u8array)?;
            }
            // Step 4.2.3: Return.
            break;
        };

        // Step 4.3: Let result be the result of running convert code unit to scalar value with encoder, item, and input.
        // Scope the mutable borrow so it is dropped before any JS reentry (enqueue_to_transform).
        let result = {
            let mut data = stream.data_mut();
            convert_code_unit_to_scalar_value(&mut data, item, &mut input)
        };

        // Step 4.4: If result is not continue, then process an item with result, encoder’s encoder, input, output, and "fatal".
        // (Note: For UTF-8, "processing an item" is encoding the scalar value to its byte sequence and appending to output)
        match result {
            EncodeResult::Continue => {
                // Do nothing, continue the loop.
            }
            EncodeResult::Scalar(sv) => {
                encode_scalar_value(&mut output, sv);
            }
        }
    }

    Ok(value::undefined())
}

/// The <https://encoding.spec.whatwg.org/#encode-and-flush> algorithm.
fn encoder_flush_cb(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream: TextEncoderStream<'_> = Object::from_value(scope, *payload)
        .expect("payload is an object")
        .cast::<TextEncoderStream<'_>>()
        .expect("payload is a TextEncoderStream");

    // Step 1: If encoder’s [[pending high surrogate]] is non-null:
    // Scope the mutable borrow so it is dropped before any JS reentry.
    let has_pending = {
        let mut data = stream.data_mut();
        data.leading_surrogate.take().is_some()
    };
    if has_pending {
        // Step 1.1: Let chunk be a Uint8Array object wrapping « 0xEF, 0xBF, 0xBD ». (U+FFFD in UTF-8)
        let chunk = Uint8Array::with_data(scope, &[0xEF, 0xBF, 0xBD])?;
        // Step 1.2: Enqueue chunk in encoder’s [[TransformStream]].
        enqueue_to_transform(scope, &stream, chunk)?;
    }

    Ok(value::undefined())
}

/// Result of the convert code unit to scalar value algorithm.
enum EncodeResult {
    /// Return `continue` per spec step 2.
    Continue,
    /// A scalar value to encode.
    Scalar(u32),
}

/// The <https://encoding.spec.whatwg.org/#convert-code-unit-to-scalar-value> algorithm.
fn convert_code_unit_to_scalar_value(
    data: &mut TextEncoderStreamImpl,
    item: u16,
    input: &mut VecDeque<u16>,
) -> EncodeResult {
    // Step 1: If encoder’s leading surrogate is non-null:
    if let Some(leading) = data.leading_surrogate.take() {
        // Step 1.1: Let high be encoder’s leading surrogate.
        // Step 1.2: Set encoder’s leading surrogate to null. (Done above by take())

        // Step 1.3: If item is a trailing surrogate (U+DC00 to U+DFFF), then return 
        // the scalar value from surrogates given leadingSurrogate and item.
        if (0xDC00..=0xDFFF).contains(&item) {
            // To obtain a scalar value from surrogates, given a leading surrogate leading and a trailing surrogate trailing,
            // return 0x10000 + ((leading − 0xD800) << 10) + (trailing − 0xDC00).
            let sv = 0x10000 + ((leading as u32 - 0xD800) << 10) + (item as u32 - 0xDC00);
            return EncodeResult::Scalar(sv);
        }

        // Step 1.4: Restore item to input.
        input.push_front(item);

        // Step 1.5: Return U+FFFD.
        return EncodeResult::Scalar(0xFFFD);
    }

    // Step 2: If item is a leading surrogate (U+D800 to U+DBFF):
    if (0xD800..=0xDBFF).contains(&item) {
        // Step 2.1: Set encoder’s [[pending high surrogate]] to item.
        data.leading_surrogate = Some(item);
        // Step 2.2: Return continue.
        return EncodeResult::Continue;
    }

    // Step 3: If item is a trailing surrogate (U+DC00 to U+DFFF), then return U+FFFD.
    if (0xDC00..=0xDFFF).contains(&item) {
        return EncodeResult::Scalar(0xFFFD);
    }

    // Step 4: Return the scalar value item.
    EncodeResult::Scalar(item as u32)
}

/// Encode a scalar value as UTF-8 and append to output.
fn encode_scalar_value(output: &mut Vec<u8>, sv: u32) {
    let mut buf = [0u8; 4];
    let c = char::from_u32(sv).unwrap_or('\u{FFFD}');
    let len = c.encode_utf8(&mut buf).len();
    output.extend_from_slice(&buf[..len]);
}

/// Enqueue a chunk into the TextEncoderStream's TransformStream.
fn enqueue_to_transform(
    scope: &Scope<'_>,
    stream: &TextEncoderStream<'_>,
    chunk: Uint8Array<'_>,
) -> Result<(), ExnThrown> {
    let chunk_val = scope.root_value(chunk.as_value());
    stream.data().transform.get(scope).enqueue(scope, chunk_val)
}
