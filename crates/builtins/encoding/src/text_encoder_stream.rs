// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#textencoderstream>

use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::function::cast_payload;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleValue};
use js::{value, JSString, Uint8Array};

use web_streams::readable::ReadableStream;
use web_streams::writable::WritableStream;
use web_streams::{AlgorithmArg, TransformStream, TransformStreamImpl};

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
        // The UTF-8 encoder is stateless, so no explicit assignment is required.
        // Step 2: Let transformAlgorithm be an algorithm which takes a chunk argument and runs
        // the encode and enqueue a chunk algorithm with this and chunk.
        let transform = AlgorithmArg::native(scope, encoder_transform_cb, self)?;
        // Step 3: Let flushAlgorithm be an algorithm which runs the encode and flush algorithm
        // with this.
        let flush = AlgorithmArg::native(scope, encoder_flush_cb, self)?;
        // Step 4: Let transformStream be a new TransformStream.
        // Step 5: Set up transformStream with transformAlgorithm set to transformAlgorithm and
        // flushAlgorithm set to flushAlgorithm.
        let transform_stream =
            TransformStream::new_native(scope, transform, flush, AlgorithmArg::None)?;
        // Step 6: Set this’s [[TransformStream]] to transformStream.
        self.data_mut().transform.set(transform_stream);

        Ok(())
    }

    /// <https://encoding.spec.whatwg.org/#dom-textencoder-encoding>
    #[getter]
    fn encoding(&self) -> &'static str {
        "utf-8"
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
    let stream = cast_payload::<TextEncoderStream>(scope, payload);

    // Step 1: Let input be the result of converting chunk to a DOMString.
    let js_string = JSString::from_value(scope, args.get(0))?;
    // Step 2: Convert input to an I/O queue of code units.
    // Step 3: Let output be the I/O queue of bytes « end-of-queue ».
    // Step 4: While true:
    // Step 4.1: Let item be the result of reading from input.
    // Step 4.2: If item is end-of-queue:
    // Step 4.2.1: Convert output into a byte sequence.
    // Step 4.2.2: If output is not empty:
    // Step 4.2.2.1: Let chunk be the result of creating a Uint8Array object given output and
    // encoder’s relevant realm.
    // Step 4.2.2.2: Enqueue chunk into encoder’s TransformStream.
    // Step 4.2.3: Return.
    // Step 4.3: Let result be the result of executing the convert code unit to scalar value
    // algorithm with encoder, item, and input.
    // Step 4.4: If result is not continue, then process an item with result, encoder’s encoder,
    // input, output, and "fatal".
    // Implementation note: a surrogate pair can only be split at the chunk's edges, so only the
    // first and last code units need the [[pending high surrogate]] handling. Everything between
    // is encoded in bulk by `to_utf8`, which replaces lone surrogates with U+FFFD exactly as
    // steps 4.3 and 4.4 do.
    let mut start = 0;
    let mut end = js_string.len();
    if end == 0 {
        return Ok(value::undefined());
    }
    let mut output = Vec::new();
    let pending = stream.data_mut().leading_surrogate.take();
    if let Some(leading) = pending {
        let first = js_string.char_at(scope, 0)?;
        if is_trailing_surrogate(first) {
            let c = 0x10000 + ((u32::from(leading) - 0xD800) << 10) + (u32::from(first) - 0xDC00);
            push_scalar_value(&mut output, c);
            start = 1;
        } else {
            push_scalar_value(&mut output, 0xFFFD);
        }
    }
    if start < end {
        let last = js_string.char_at(scope, end - 1)?;
        if is_leading_surrogate(last) {
            stream.data_mut().leading_surrogate = Some(last);
            end -= 1;
        }
    }
    if start < end {
        let middle = if start == 0 && end == js_string.len() {
            js_string
        } else {
            js_string.substring(scope, start, end - start)?
        };
        let utf8 = middle.to_utf8(scope)?.into_bytes();
        if output.is_empty() {
            output = utf8;
        } else {
            output.extend_from_slice(&utf8);
        }
    }

    if !output.is_empty() {
        let u8array = Uint8Array::with_data(scope, &output)?;
        enqueue_to_transform(scope, &stream, u8array)?;
    }

    Ok(value::undefined())
}

fn is_leading_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_trailing_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

/// Append the UTF-8 encoding of the scalar value `sv` to `output`.
fn push_scalar_value(output: &mut Vec<u8>, sv: u32) {
    let c = char::from_u32(sv).expect("a scalar value");
    output.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
}

/// The <https://encoding.spec.whatwg.org/#encode-and-flush> algorithm.
fn encoder_flush_cb(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TextEncoderStream>(scope, payload);

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

/// Enqueue a chunk into the TextEncoderStream's TransformStream.
fn enqueue_to_transform(
    scope: &Scope<'_>,
    stream: &TextEncoderStream<'_>,
    chunk: Uint8Array<'_>,
) -> Result<(), ExnThrown> {
    let chunk_val = scope.root_value(chunk.as_value());
    let transform = stream.data().transform.get(scope);
    transform.enqueue(scope, chunk_val)
}
