// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#textdecoderstream>
//!
//! TextDecoderStream is a TransformStream that decodes bytes to strings using
//! a configurable encoding. It maintains decoder state across chunks to handle
//! incomplete multi-byte sequences at chunk boundaries.

use core_runtime::{webidl_interface, webidl_methods};
use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::function::cast_payload;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleValue};
use js::{value, JSString};

use web_streams::readable::ReadableStream;
use web_streams::writable::WritableStream;
use web_streams::{AlgorithmArg, TransformStream, TransformStreamImpl};

use crate::decoder_common::{new_decoder, TextDecoderOptions};
use crate::text_decoder::{decode_bytes_to_js_string, ArrayBufferViewOrArrayBuffer};

/// <https://encoding.spec.whatwg.org/#textdecoderstream>
#[webidl_interface]
pub struct TextDecoderStream {
    /// <https://encoding.spec.whatwg.org/#textdecoder-encoding>
    /// (always `Some` after construction).
    #[no_trace]
    encoding: Option<&'static encoding_rs::Encoding>,

    /// <https://encoding.spec.whatwg.org/#textdecoder-error-mode>
    /// true = "fatal" mode (throw on error), false = "replacement" mode.
    fatal: bool,

    /// <https://encoding.spec.whatwg.org/#textdecoder-ignore-bom>
    ignore_bom: bool,

    // Note: The spec defines an I/O queue (https://encoding.spec.whatwg.org/#textdecodercommon-i-o-queue)
    // to buffer incomplete multi-byte sequences across chunks. We omit it because encoding_rs's
    // Decoder is stateful and tracks partial sequences internally. Per the Decoder contract
    // (https://docs.rs/encoding_rs/0.8/encoding_rs/struct.Decoder.html), `src` is only
    // partially consumed on `OutputFull` or `Malformed`; since we pre-allocate to prevent
    // `OutputFull` and treat `Malformed` as a fatal error, the result is always `InputEmpty`
    // with `read == src.len()`. Passing each chunk's bytes directly to the decoder is
    // equivalent to the spec's queue-then-process pattern.
    /// <https://encoding.spec.whatwg.org/#textdecodercommon-decoder>
    /// The decoder instance, preserving state between chunks.
    #[no_trace]
    decoder: Option<encoding_rs::Decoder>,

    /// The underlying TransformStream.
    transform: Heap<TransformStreamImpl>,
}

#[webidl_methods]
impl TextDecoderStream {
    /// <https://encoding.spec.whatwg.org/#dom-textdecoderstream>
    /// 4. Set up a text decoder stream with this, encoding, errorMode, and
    ///    options["ignoreBOM"].
    ///
    /// "Set up a text decoder stream" initializes all internal slots and creates
    /// the TransformStream with the decode and flush algorithms.
    #[constructor]
    fn new(
        &self,
        scope: &Scope<'_>,
        label: Option<String>,
        options: Option<TextDecoderOptions>,
    ) -> Result<(), ExnThrown> {
        // Step 1: Let encoding be the result of getting an encoding from label.
        // Step 2: If encoding is failure or replacement, then throw a RangeError.
        let label = label.as_deref().unwrap_or("utf-8");
        let encoding = encoding_rs::Encoding::for_label_no_replacement(label.as_bytes())
            .ok_or_else(|| {
                js::error::throw_range_error(scope, c"The encoding label provided is invalid.")
            })?;

        // Step 3: Let errorMode be "fatal" if options["fatal"] is true; otherwise "replacement".
        let options = options.unwrap_or_default();
        let fatal = options.fatal;
        let ignore_bom = options.ignore_bom;

        // Step 4: Set up a text decoder stream with this, encoding, errorMode, and
        // options["ignoreBOM"].
        // Step 4.1: Assert: encoding is not replacement.
        // Step 4.2: Set stream's encoding to encoding.
        self.data_mut().encoding = Some(encoding);
        // Step 4.3: Set stream's error mode to errorMode.
        self.data_mut().fatal = fatal;
        // Step 4.4: Set stream's ignore BOM to ignoreBOM.
        self.data_mut().ignore_bom = ignore_bom;
        // Step 4.5: Set stream’s decoder to a new instance of encoding’s decoder.
        self.data_mut().decoder = Some(new_decoder(encoding, ignore_bom));
        // Step 4.6: Set stream's I/O queue to a new I/O queue.
        // Implementation note: this is not stored separately because `encoding_rs::Decoder`
        // buffers incomplete sequences internally; see the comment on the struct definition.

        // Step 4.7: Let transformAlgorithm be an algorithm which takes a chunk argument and runs
        // the decode and enqueue a chunk algorithm with stream and chunk.
        let transform = AlgorithmArg::native(scope, decoder_transform_cb, self)?;
        // Step 4.8: Let flushAlgorithm be an algorithm which takes no arguments and runs the
        // flush and enqueue algorithm with stream.
        let flush = AlgorithmArg::native(scope, decoder_flush_cb, self)?;
        // Step 4.9: Let transformStream be a new TransformStream.
        // Step 4.10: Set up transformStream with transformAlgorithm set to transformAlgorithm and
        // flushAlgorithm set to flushAlgorithm.
        let transform_stream =
            TransformStream::new_native(scope, transform, flush, AlgorithmArg::None)?;
        // Step 4.11: Set stream’s [[TransformStream]] to transformStream.
        self.data_mut().transform.set(transform_stream);

        Ok(())
    }

    /// <https://encoding.spec.whatwg.org/#dom-textdecoder-encoding>
    #[getter]
    fn encoding(&self) -> String {
        // Step 1: Return this’s encoding’s name, ASCII lowercased.
        let encoding = self.data().encoding.expect("set by the constructor");
        encoding.name().to_ascii_lowercase()
    }

    /// <https://encoding.spec.whatwg.org/#dom-textdecoder-fatal>
    #[getter]
    fn fatal(&self) -> bool {
        self.data().fatal
    }

    /// <https://encoding.spec.whatwg.org/#dom-textdecoder-ignorebom>
    #[getter(name = "ignoreBOM")]
    fn ignore_bom(&self) -> bool {
        self.data().ignore_bom
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

/// The <https://encoding.spec.whatwg.org/#decode-and-enqueue-a-chunk> algorithm.
fn decoder_transform_cb(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TextDecoderStream>(scope, payload);

    // Step 1: Let bufferSource be the result of converting chunk to an AllowSharedBufferSource.
    let buffer_source = ArrayBufferViewOrArrayBuffer::from_jsval_throwing(scope, args.get(0), ())?;
    let output = {
        // Step 2: Push a copy of bufferSource to decoder’s I/O queue.
        // Step 3: Let output be the I/O queue of scalar values « end-of-queue ».
        // Step 4: While true:
        // Step 4.1: Let item be the result of reading from decoder’s I/O queue.
        // Step 4.2: If item is end-of-queue:
        // Step 4.2.1: Let outputChunk be the result of running serialize I/O queue with decoder
        // and output.
        // Step 4.2.2: If outputChunk is not the empty string, then enqueue outputChunk in
        // decoder’s TransformStream.
        // Step 4.2.3: Return.
        // Step 4.3: Let result be the result of processing an item with item, decoder’s decoder,
        // decoder’s I/O queue, output, and decoder’s error mode.
        // Step 4.4: If result is error, then throw a TypeError.
        // Implementation note: the Encoding Standard permits avoiding the copy when input cannot
        // affect future decode calls. The slice is consumed synchronously here and
        // `encoding_rs::Decoder` retains only its own state. `decode_bytes_to_js_string` performs
        // steps 3-4 with `last = false`, preserving decoder state for the next chunk.
        // SAFETY: `buffer_source` is rooted, and the helper consumes the slice or copies it into
        // Rust-owned output before any JS allocation can GC and move the input buffer.
        let bytes = match buffer_source {
            ArrayBufferViewOrArrayBuffer::View(view) => unsafe { view.bytes() },
            ArrayBufferViewOrArrayBuffer::Buffer(buffer) => unsafe { buffer.bytes() },
        };
        let mut data = stream.data_mut();
        let encoding = data.encoding.expect("encoding must be initialized");
        let fatal = data.fatal;
        let decoder = data.decoder.as_mut().expect("decoder must be initialized");
        decode_bytes_to_js_string(scope, encoding, decoder, fatal, bytes, false, false)?
    };

    if !output.is_empty() {
        enqueue_to_transform(scope, &stream, output)?;
    }

    // Step 4.2.3: Return.
    Ok(value::undefined())
}

/// The <https://encoding.spec.whatwg.org/#flush-and-enqueue> algorithm.
fn decoder_flush_cb(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TextDecoderStream>(scope, payload);

    // Step 1: Let output be the I/O queue of scalar values « end-of-queue ».
    // Step 2: While true:
    // Step 2.1: Let item be the result of reading from decoder’s I/O queue.
    // Step 2.2: Let result be the result of processing an item with item, decoder’s decoder,
    // decoder’s I/O queue, output, and decoder’s error mode.
    // Step 2.3: If result is finished:
    // Step 2.3.1: Let outputChunk be the result of running serialize I/O queue with decoder and
    // output.
    // Step 2.3.2: If outputChunk is not the empty string, then enqueue outputChunk in decoder’s
    // TransformStream.
    // Step 2.3.3: Return.
    // Step 2.4: Otherwise, if result is error, throw a TypeError.
    // Implementation note: `encoding_rs::Decoder` holds incomplete input internally, so an empty
    // slice with `last = true` supplies end-of-queue and processes steps 2.1-2.4 to completion.
    let output = {
        let mut data = stream.data_mut();
        let encoding = data.encoding.expect("encoding must be initialized");
        let fatal = data.fatal;
        let decoder = data.decoder.as_mut().expect("decoder must be initialized");
        decode_bytes_to_js_string(scope, encoding, decoder, fatal, &[], true, false)?
    };

    if !output.is_empty() {
        enqueue_to_transform(scope, &stream, output)?;
    }

    // Step 2.3.3: Return.
    Ok(value::undefined())
}

fn enqueue_to_transform(
    scope: &Scope<'_>,
    stream: &TextDecoderStream<'_>,
    chunk: JSString<'_>,
) -> Result<(), ExnThrown> {
    let transform = stream.data().transform.get(scope);
    transform.enqueue(scope, scope.root_value(chunk.as_value()))
}
