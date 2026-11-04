// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#textencoderstream>

use core_runtime::{webidl_interface, webidl_methods};
use js::error::{ExnThrown, OutOfMemory, ThrowException};
use js::function::cast_payload;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleValue};
use js::string::StrChars;
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
    let input = JSString::from_value(scope, args.get(0))?;
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
    //
    // (`encoding_rs` runs this loop over the whole chunk, and replaces unpaired surrogates with
    // U+FFFD. It doesn't keep a high surrogate between calls, so we encode the pending one
    // together with the chunk's first code unit, and hold back a high surrogate that ends the
    // chunk.)
    if input.is_empty() {
        return Ok(value::undefined());
    }
    let pending = stream.data_mut().leading_surrogate.take();
    // SAFETY: the closure doesn't call into SpiderMonkey.
    let (output, pending) = unsafe {
        input.with_chars(scope, |chars| match chars {
            StrChars::Latin1(chars) => (encode_latin1(pending, chars), None),
            StrChars::TwoByte(chars) => encode_utf16(pending, chars),
        })
    }?;
    stream.data_mut().leading_surrogate = pending;
    let output = output.ok_or_else(|| OutOfMemory.throw(scope))?;
    if !output.is_empty() {
        let chunk = Uint8Array::with_data(scope, &output)?;
        enqueue_to_transform(scope, &stream, chunk)?;
    }
    Ok(value::undefined())
}
/// Returns the UTF-8 encoding of `chars`, preceded by U+FFFD if `pending` is set, or `None` if
/// allocating it fails. Latin-1 characters aren't surrogates, so `pending` is always unpaired.
fn encode_latin1(pending: Option<u16>, chars: &[u8]) -> Option<Vec<u8>> {
    let head = pending.as_slice();
    let mut output = zeroed_buffer(head.len() * 3 + chars.len() * 2)?;
    let mut written = encoding_rs::mem::convert_utf16_to_utf8(head, &mut output);
    written += encoding_rs::mem::convert_latin1_to_utf8(chars, &mut output[written..]);
    output.truncate(written);
    Some(output)
}
/// Returns the UTF-8 encoding of `chars` preceded by the high surrogate `pending`, or `None` if
/// allocating it fails. A high surrogate at the end of `chars` isn't encoded. It is returned as
/// the new pending high surrogate.
fn encode_utf16(pending: Option<u16>, chars: &[u16]) -> (Option<Vec<u8>>, Option<u16>) {
    let mut head = [0; 2];
    let mut head_len = 0;
    let mut rest = chars;
    if let Some(high) = pending {
        head[0] = high;
        head_len = 1;
        if let Some((&first, tail)) = rest.split_first() {
            if is_trailing_surrogate(first) {
                head[1] = first;
                head_len = 2;
                rest = tail;
            }
        }
    }
    let mut new_pending = None;
    if let Some((&last, init)) = rest.split_last() {
        if is_leading_surrogate(last) {
            new_pending = Some(last);
            rest = init;
        }
    }
    let Some(mut output) = zeroed_buffer((head_len + rest.len()) * 3) else {
        return (None, new_pending);
    };
    let mut written = encoding_rs::mem::convert_utf16_to_utf8(&head[..head_len], &mut output);
    written += encoding_rs::mem::convert_utf16_to_utf8(rest, &mut output[written..]);
    output.truncate(written);
    (Some(output), new_pending)
}
/// Returns `len` zero bytes, or `None` if allocating them fails.
fn zeroed_buffer(len: usize) -> Option<Vec<u8>> {
    if len == 0 {
        return Some(Vec::new());
    }
    let layout = std::alloc::Layout::array::<u8>(len).ok()?;
    // SAFETY: `layout` has a non-zero size.
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    // SAFETY: `ptr` was allocated by the global allocator with `layout`, and its `len` bytes are
    // initialized to zero.
    (!ptr.is_null()).then(|| unsafe { Vec::from_raw_parts(ptr, len, len) })
}

fn is_leading_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_trailing_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
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
