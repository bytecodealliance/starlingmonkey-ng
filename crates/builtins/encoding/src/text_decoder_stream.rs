// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#textdecoderstream>
//!
//! TextDecoderStream is a TransformStream that decodes bytes to strings using
//! a configurable encoding. It maintains decoder state across chunks to handle
//! incomplete multi-byte sequences at chunk boundaries.

use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::{CallbackArgs, HandleValue};
use js::{ArrayBuffer, ArrayBufferView, Function, Object, value};

use web_streams::readable::ReadableStream;
use web_streams::{TransformStream, TransformStreamImpl};
use web_streams::writable::WritableStream;

use crate::decoder_common::{new_decoder, TextDecoderOptions};

/// <https://encoding.spec.whatwg.org/#textdecoderstream>
#[webidl_interface]
pub struct TextDecoderStream {
    /// The encoding_rs encoding (always `Some` after construction).
    #[no_trace]
    encoding: Option<&'static encoding_rs::Encoding>,

    /// The encoding name, ASCII lowercased.
    encoding_name: String,

    /// <https://encoding.spec.whatwg.org/#textdecoder-error-mode>
    /// true = "fatal" mode (throw on error), false = "replacement" mode.
    fatal: bool,

    /// <https://encoding.spec.whatwg.org/#textdecoder-ignore-bom>
    #[no_trace(name = "ignoreBOM")]
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
        let label = label.as_deref().unwrap_or("utf-8");
        let encoding =
            encoding_rs::Encoding::for_label_no_replacement(label.as_bytes()).ok_or_else(|| {
                js::error::throw_range_error(scope, c"The encoding label provided is invalid.")
            })?;

        // Step 2: If encoding is failure or replacement, then throw a RangeError.
        if encoding == encoding_rs::REPLACEMENT {
            return Err(js::error::throw_range_error(
                scope,
                c"The replacement or failure encoding cannot be used.",
            ));
        }

        // Step 3: Let errorMode be "fatal" if options["fatal"] is true; otherwise "replacement".
        let options = options.unwrap_or_default();
        let fatal = options.fatal;
        let ignore_bom = options.ignore_bom;

        // Step 4: Set up a text decoder stream with this, encoding, errorMode, and options["ignoreBOM"].
        // Step 4.1: Assert: encoding is not replacement. (checked above when we throw)
        // Step 4.2 Set stream's encoding to encoding.
        self.data_mut().encoding = Some(encoding);
        self.data_mut().encoding_name = encoding.name().to_ascii_lowercase();
        // Step 4.3 Set stream's error mode to errorMode.
        self.data_mut().fatal = fatal;
        // Step 4.4 Set stream's ignore BOM to options["ignoreBOM"].
        self.data_mut().ignore_bom = ignore_bom;
        // Step 4.5 Set stream’s decoder to a new instance of encoding’s decoder.
        self.data_mut().decoder = Some(new_decoder(encoding, ignore_bom));
        // Step 4.6: Set stream's I/O queue to a new I/O queue.
        // (Omitted — encoding_rs's Decoder buffers partial sequences internally;
        // see the comment on the struct definition.)

        let self_payload = scope.root_value(self.as_value());
        // Step 4.7: Let transformAlgorithm be an algorithm which takes a chunk argument and runs the decode and enqueue a chunk algorithm with stream and chunk.
        let transform_cb = Function::new_callback(
            scope,
            c"textDecoderTransform",
            1,
            decoder_transform_cb,
            self_payload,
        )?;

        // Step 4.8: Let flushAlgorithm be an algorithm which takes no arguments and runs the flush and enqueue algorithm with stream.
        let flush_cb = Function::new_callback(
            scope,
            c"textDecoderFlush",
            0,
            decoder_flush_cb,
            self_payload,
        )?;

        // Step 4.9: Let transformStream be a new TransformStream.
         // Step 4.10: Set up transformStream with transformAlgorithm set to transformAlgorithm and flushAlgorithm set to flushAlgorithm.
        // We pass both algorithms as properties on a plain transformer object so that the
        // TransformStream constructor sees a real transformer and does not set the
        // identity-transform shortcut — otherwise `pipeTo` would bypass decoding for native
        // byte sources.
        let transformer = Object::new_plain(scope)?;
        transformer.set_property(scope, c"transform", transform_cb)?;
        transformer.set_property(scope, c"flush", flush_cb)?;
        let transform_stream = TransformStream::new(scope, Some(scope.root_value(transformer.as_value())), None, None)?; 
        
        // Step 4.11: Set stream’s transform to transformStream.
        self.data_mut().transform.set(transform_stream);

        Ok(())
    }

    /// <https://encoding.spec.whatwg.org/#dom-textdecoder-encoding>
    #[getter]
    fn encoding(&self) -> String {
        self.data().encoding_name.clone()
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
    let stream: TextDecoderStream<'_> = Object::from_value(scope, *payload)
        .expect("payload is an object")
        .cast::<TextDecoderStream<'_>>()
        .expect("payload is a TextDecoderStream");

    // Step 1: Let bufferSource be the result of converting chunk to an AllowSharedBufferSource.
    let chunk = args.get(0);
    let bytes = buffer_source_to_bytes(scope, chunk)?;

    // Step 2: Push a copy of bufferSource to decoder's I/O queue.
    // Step 3: Let output be the I/O queue of scalar values « end-of-queue ».
    // Step 4: Process I/O queue.
    // (Steps 2-4 are collapsed: per the encoding_rs Decoder contract
    // (https://docs.rs/encoding_rs/0.8/encoding_rs/struct.Decoder.html), `src` is fully
    // consumed when the result is `InputEmpty`, which is guaranteed here by pre-allocating
    // via `max_utf8_buffer_length`. The Decoder tracks partial multi-byte sequences
    // internally, so we pass the chunk directly without queuing.)
    let mut output = String::new();

    if !bytes.is_empty() {
        let mut data = stream.data_mut();
        let fatal = data.fatal;
        let decoder = data.decoder.as_mut().expect("decoder must be initialized");

        if fatal {
            // Step 4.3: Process through fatal decoder.
            if let Some(needed) = decoder.max_utf8_buffer_length_without_replacement(bytes.len()) {
                output.reserve(needed);
            }
            let (result, _read) =
                decoder.decode_to_string_without_replacement(&bytes, &mut output, false);

            match result {
                encoding_rs::DecoderResult::InputEmpty => {}
                // Step 4.4: If error, throw a TypeError.
                encoding_rs::DecoderResult::Malformed(_, _) => {
                    return Err(js::error::throw_type_error(
                        scope,
                        c"The encoded data was not valid.",
                    ));
                }
                encoding_rs::DecoderResult::OutputFull => {
                    unreachable!("Output buffer was too small despite max_utf8_buffer_length_without_replacement allocation");
                }
            }
        } else {
            // Step 4.3: Process through replacement decoder.
            if let Some(needed) = decoder.max_utf8_buffer_length(bytes.len()) {
                output.reserve(needed);
            }
            match decoder.decode_to_string(&bytes, &mut output, false) {
                (encoding_rs::CoderResult::InputEmpty, _, _) => {}
                (encoding_rs::CoderResult::OutputFull, _, _) => {
                    unreachable!("Output buffer was too small despite max_utf8_buffer_length allocation");
                }
            }
        }
    }

    // Step 4.2.1 & 4.2.2: If outputChunk is not the empty string, then enqueue outputChunk.
    if !output.is_empty() {
        let chunk_val = js::JSString::from_str(scope, &output)
            .map_err(|_| js::error::throw_type_error(scope, c"unable to create string"))?;
        stream
            .data()
            .transform
            .get(scope)
            .enqueue(scope, scope.root_value(chunk_val.as_value()))?;
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
    let stream: TextDecoderStream<'_> = Object::from_value(scope, *payload)
        .expect("payload is an object")
        .cast::<TextDecoderStream<'_>>()
        .expect("payload is a TextDecoderStream");

    // Step 1: Let output be the I/O queue of scalar values « end-of-queue ».
    // (No I/O queue to drain — per the encoding_rs Decoder contract
    // (https://docs.rs/encoding_rs/0.8/encoding_rs/struct.Decoder.html), partial sequences
    // are held in the Decoder's internal state. We pass an empty slice with `last=true`
    // to flush that state.)
    let mut output = String::new();

    {
        let mut data = stream.data_mut();
        let fatal = data.fatal;
        let decoder = data.decoder.as_mut().expect("decoder must be initialized");

        // Step 2: While true:
        // (Instead of a manual byte-by-byte loop, we pass `last = true` to encoding_rs.
        // This instructs the native decoder to finalize, process any dangling bytes,
        // and reset its shift or multibyte state. This acts as our loop termination).
        if fatal {
            // Pre-reserve using the without-replacement bound.
            if let Some(needed) = decoder.max_utf8_buffer_length_without_replacement(0) {
                output.reserve(needed);
            }
            // Step 2.2: Let result be processing an item with item, decoder, output, and error mode.
            let (result, _read) =
                decoder.decode_to_string_without_replacement(&[], &mut output, true);

            match result {
                // Step 2.3: If result is finished
                encoding_rs::DecoderResult::InputEmpty => {}
                // Step 2.4: If result is error, throw a TypeError.
                encoding_rs::DecoderResult::Malformed(_, _) => {
                    return Err(js::error::throw_type_error(
                        scope,
                        c"The encoded data was not valid.",
                    ));
                }
                encoding_rs::DecoderResult::OutputFull => {
                    unreachable!("Output buffer was too small during fatal flush");
                }
            }
        } else {
            // Pre-reserve using the with-replacement bound (accounts for pending U+FFFD output).
            if let Some(needed) = decoder.max_utf8_buffer_length(0) {
                output.reserve(needed);
            }
            // Step 2.2: Let result be processing an item with item, decoder, output, and error mode.
            match decoder.decode_to_string(&[], &mut output, true) {
                (encoding_rs::CoderResult::InputEmpty, _, _) => {}
                (encoding_rs::CoderResult::OutputFull, _, _) => {
                    unreachable!("Output buffer was too small during replacement flush");
                }
            }
        }
    }

    // Step 2.3.1: Let outputChunk be the result of running serialize I/O queue with decoder and output.(handled by encoding_rs)
    // Step 2.3.2: If outputChunk is not the empty string, then enqueue outputChunk in decoder's transform.
    if !output.is_empty() {
        let chunk_val = js::JSString::from_str(scope, &output)
            .map_err(|_| js::error::throw_type_error(scope, c"unable to create string"))?;
        stream
            .data()
            .transform
            .get(scope)
            .enqueue(scope, scope.root_value(chunk_val.as_value()))?;
    }

    // Step 2.3.3: Return.
    Ok(value::undefined())
}

fn buffer_source_to_bytes(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Vec<u8>, ExnThrown> {
    let obj = Object::from_value(scope, *value).map_err(|_| {
        js::error::throw_type_error(scope, c"argument is not an ArrayBuffer or ArrayBufferView")
    })?;

    // Try ArrayBufferView first (typed arrays, DataView).
    if let Ok(view) = obj.cast::<ArrayBufferView<'_>>() {
        // SAFETY: `view` is rooted; we copy the bytes immediately so the
        // borrow doesn't outlive any GC-triggering operation. The copy also
        // satisfies the spec requirement for SharedArrayBuffer-backed views.
        return Ok(unsafe { view.bytes() }.to_vec());
    }

    // Try ArrayBuffer.
    if let Ok(buf) = obj.cast::<ArrayBuffer<'_>>() {
        // SAFETY: `buf` is rooted; same constraints as above.
        // Note: If the ArrayBuffer is detached, bytes() returns an empty slice.
        return Ok(unsafe { buf.bytes() }.to_vec());
    }

    // Not a valid buffer source.
    Err(js::error::throw_type_error(
        scope,
        c"argument is not an ArrayBuffer or ArrayBufferView",
    ))
}
