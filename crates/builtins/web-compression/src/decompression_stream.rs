// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://compression.spec.whatwg.org/#decompression-stream>

use compression::{Codec, Flush, Format, Params};
use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use web_streams::readable::ReadableStream;
use web_streams::transform::transform_stream::{TransformStream, TransformStreamImpl};
use web_streams::writable::WritableStream;
use web_streams::AlgorithmArg;

use crate::{buffer_source_bytes, process_and_enqueue, CompressionFormat};

/// <https://compression.spec.whatwg.org/#decompressionstream>
#[webidl_interface]
pub struct DecompressionStream {
    /// <https://compression.spec.whatwg.org/#decompressionstream-context>, which holds the
    /// <https://compression.spec.whatwg.org/#decompressionstream-format>.
    #[no_trace]
    context: Codec,
    /// <https://streams.spec.whatwg.org/#generictransformstream-transform>, set by the
    /// constructor.
    transform: Option<Heap<TransformStreamImpl>>,
}

#[webidl_methods]
impl DecompressionStream {
    /// <https://compression.spec.whatwg.org/#dom-decompressionstream-decompressionstream>
    #[constructor]
    fn new(&self, scope: &Scope<'_>, format: CompressionFormat) -> Result<(), ExnThrown> {
        // Step 1: If _format_ is unsupported in ``DecompressionStream``, then throw a
        //         ``TypeError``.
        // The ``CompressionFormat`` conversion throws for the unsupported formats.
        // Step 2: Set `this`'s `format` to _format_.
        let format = match format {
            CompressionFormat::Brotli => Format::BrotliDecompress,
            CompressionFormat::Deflate => Format::Inflate,
            CompressionFormat::DeflateRaw => Format::InflateRaw,
            CompressionFormat::Gzip => Format::Gunzip,
        };
        self.data_mut().context = Codec::new_single_stream(format, &Params::default());
        // Step 3: Let _transformAlgorithm_ be an algorithm which takes a _chunk_ argument and runs
        //         the `decompress and enqueue a chunk` algorithm with `this` and _chunk_.
        let transform_algorithm = AlgorithmArg::native(scope, transform_algorithm, *self)?;
        // Step 4: Let _flushAlgorithm_ be an algorithm which takes no argument and runs the
        //         `decompress flush and enqueue` algorithm with `this`.
        let flush_algorithm = AlgorithmArg::native(scope, flush_algorithm, *self)?;
        // Step 5: Set `this`'s `transform` to a `new` ``TransformStream``.
        // Step 6: `Set up` `this`'s `transform` with `transformAlgorithm` set to
        //         _transformAlgorithm_ and `flushAlgorithm` set to _flushAlgorithm_.
        let transform = TransformStream::new_native(
            scope,
            transform_algorithm,
            flush_algorithm,
            AlgorithmArg::None,
        )?;
        self.data_mut().transform.set(transform);
        Ok(())
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

    /// <https://compression.spec.whatwg.org/#decompress-and-enqueue-a-chunk>
    fn decompress_and_enqueue_a_chunk(
        &self,
        scope: &Scope<'_>,
        chunk: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        // Step 1: If _chunk_ is not a ``BufferSource`` type, then throw a ``TypeError``.
        let input = buffer_source_bytes(scope, chunk)?;
        // Step 2: Let _buffer_ be the result of decompressing _chunk_ with _ds_'s `format` and
        //         `context`. If this results in an error, then throw a ``TypeError``.
        // Step 3: If _buffer_ is empty, return.
        // Step 4: Let _arrays_ be the result of splitting _buffer_ into one or more non-empty
        //         pieces and converting them into ``Uint8Array``s.
        // Step 5: For each ``Uint8Array`` _array_ of _arrays_, `enqueue` _array_ in _ds_'s
        //         `transform`.
        // Step 6: If the end of the compressed input has been reached, and _ds_'s `context` has
        //         not fully consumed _chunk_, then throw a ``TypeError``.
        // A single-stream codec fails for the input after the end of the compressed input, after
        // the output before it.
        let transform = self.transform(scope);
        process_and_enqueue(
            scope,
            &transform,
            &input,
            Flush::None,
            |input, flush, limit, out| self.data_mut().context.process(input, flush, limit, out),
        )
    }

    /// <https://compression.spec.whatwg.org/#decompress-flush-and-enqueue>
    fn decompress_flush_and_enqueue(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: Let _buffer_ be the result of decompressing an empty input with _ds_'s `format`
        //         and `context`, with the finish flag.
        // Step 2: If _buffer_ is not empty:
        // Step 2.1: Let _arrays_ be the result of splitting _buffer_ into one or more non-empty
        //           pieces and converting them into ``Uint8Array``s.
        // Step 2.2: For each ``Uint8Array`` _array_ of _arrays_, `enqueue` _array_ in _ds_'s
        //           `transform`.
        // Step 3: If the end of the compressed input has not been reached, then throw a
        //         ``TypeError``.
        // The codec fails with the finish flag before the end of the compressed input, after the
        // output.
        let transform = self.transform(scope);
        process_and_enqueue(
            scope,
            &transform,
            &[],
            Flush::Finish,
            |input, flush, limit, out| self.data_mut().context.process(input, flush, limit, out),
        )
    }
}

/// The `transformAlgorithm` of a `DecompressionStream`, whose payload is the stream.
fn transform_algorithm(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<DecompressionStream>(scope, payload);
    stream.decompress_and_enqueue_a_chunk(scope, args.get(0))?;
    Ok(js::value::undefined())
}

/// The `flushAlgorithm` of a `DecompressionStream`, whose payload is the stream.
fn flush_algorithm(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<DecompressionStream>(scope, payload);
    stream.decompress_flush_and_enqueue(scope)?;
    Ok(js::value::undefined())
}
