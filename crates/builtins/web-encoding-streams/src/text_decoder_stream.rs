// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://encoding.spec.whatwg.org/#interface-textdecoderstream>

use core_runtime::{webidl_interface, webidl_methods, webidl_union};
use js::conversion::{ConversionError, FromJSVal};
use js::error::{BindingSite, ExnThrown, MemberKind, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{ArrayBuffer, ArrayBufferView, JSString, SharedArrayBuffer};
use web_globals::encoding::{decode_to_utf16, encoding_for_label, new_decoder, TextDecoderOptions};
use web_streams::readable::ReadableStream;
use web_streams::transform::transform_stream::{TransformStream, TransformStreamImpl};
use web_streams::writable::WritableStream;
use web_streams::AlgorithmArg;

/// <https://webidl.spec.whatwg.org/#AllowSharedBufferSource>
#[webidl_union]
pub enum AllowSharedBufferSource<'a> {
    View(ArrayBufferView<'a>),
    Buffer(ArrayBuffer<'a>),
    Shared(SharedArrayBuffer<'a>),
}

/// <https://encoding.spec.whatwg.org/#textdecoderstream>
#[webidl_interface]
pub struct TextDecoderStream {
    /// <https://encoding.spec.whatwg.org/#textdecodercommon-encoding>, set by the constructor.
    #[no_trace]
    encoding: Option<&'static encoding_rs::Encoding>,
    /// <https://encoding.spec.whatwg.org/#textdecodercommon-decoder>, which also holds the
    /// `I/O queue` and `BOM seen` state. Set by the constructor.
    #[no_trace]
    decoder: Option<encoding_rs::Decoder>,
    /// <https://encoding.spec.whatwg.org/#textdecodercommon-error-mode>: whether it is "fatal".
    fatal: bool,
    /// <https://encoding.spec.whatwg.org/#textdecodercommon-ignore-bom>
    ignore_bom: bool,
    /// <https://streams.spec.whatwg.org/#generictransformstream-transform>, set by the
    /// constructor.
    transform: Option<Heap<TransformStreamImpl>>,
}

#[webidl_methods]
impl TextDecoderStream {
    /// <https://encoding.spec.whatwg.org/#dom-textdecoderstream>
    #[constructor]
    fn new(
        &self,
        scope: &Scope<'_>,
        label: Option<String>,
        options: Option<TextDecoderOptions>,
    ) -> Result<(), ExnThrown> {
        let label = label.as_deref().unwrap_or("utf-8");
        let options = options.unwrap_or_default();
        // Step 1: Let _encoding_ be the result of `getting an encoding` from _label_.
        // Step 2: If _encoding_ is failure or `replacement`, then `throw` a ``RangeError``.
        let site = js::class::constructor_site::<TextDecoderStreamImpl>();
        let encoding = encoding_for_label(label, site).map_err(|error| error.throw(scope))?;
        // Step 3: Let _errorMode_ be "`fatal`" if _options_["``fatal``"] is true; otherwise
        //         "`replacement`".
        // Step 4: `Set up a text decoder stream` with `this`, _encoding_, _errorMode_, and
        //         _options_["``ignoreBOM``"].
        self.set_up_a_text_decoder_stream(scope, encoding, options.fatal, options.ignore_bom)
    }

    /// <https://encoding.spec.whatwg.org/#set-up-a-text-decoder-stream>
    fn set_up_a_text_decoder_stream(
        &self,
        scope: &Scope<'_>,
        encoding: &'static encoding_rs::Encoding,
        fatal: bool,
        ignore_bom: bool,
    ) -> Result<(), ExnThrown> {
        // Step 1: Assert: _encoding_ is not `replacement`.
        debug_assert!(encoding != encoding_rs::REPLACEMENT);
        {
            let mut data = self.data_mut();
            // Step 2: Set _stream_'s `encoding` to _encoding_.
            data.encoding = Some(encoding);
            // Step 3: Set _stream_'s `error mode` to _errorMode_.
            data.fatal = fatal;
            // Step 4: Set _stream_'s `ignore BOM` to _ignoreBOM_.
            data.ignore_bom = ignore_bom;
            // Step 5: Set _stream_'s `decoder` to a new instance of _encoding_'s `decoder`.
            // Step 6: Set _stream_'s `I/O queue` to a new `I/O queue`.
            data.decoder = Some(new_decoder(encoding, ignore_bom));
        }
        // Step 7: Let _transformAlgorithm_ be an algorithm which takes a _chunk_ argument and runs
        //         the `decode and enqueue a chunk` algorithm with _stream_ and _chunk_.
        let transform_algorithm = AlgorithmArg::native(scope, transform_algorithm, *self)?;
        // Step 8: Let _flushAlgorithm_ be an algorithm which takes no arguments and runs the
        //         `flush and enqueue` algorithm with _stream_.
        let flush_algorithm = AlgorithmArg::native(scope, flush_algorithm, *self)?;
        // Step 9: Let _transformStream_ be a `new` ``TransformStream``.
        // Step 10: `Set up` _transformStream_ with `transformAlgorithm` set to
        //          _transformAlgorithm_ and `flushAlgorithm` set to _flushAlgorithm_.
        let transform = TransformStream::new_native(
            scope,
            transform_algorithm,
            flush_algorithm,
            AlgorithmArg::None,
        )?;
        // Step 11: Set _stream_'s `transform` to _transformStream_.
        self.data_mut().transform.set(transform);
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

    /// Decode the bytes of `input`, or no bytes if it is `None`, with the stream's decoder, `last`
    /// at the end of the input, and enqueue the output in the stream's transform if it isn't
    /// empty. Throws a `TypeError` for malformed input in the "fatal" error mode.
    fn decode_and_enqueue(
        &self,
        scope: &Scope<'_>,
        input: Option<AllowSharedBufferSource<'_>>,
        last: bool,
    ) -> Result<(), ExnThrown> {
        let (output, encoding) = {
            let mut data = self.data_mut();
            let fatal = data.fatal;
            let encoding = data.encoding.expect("set by the constructor");
            let decoder = data.decoder.as_mut().expect("set by the constructor");
            let mut decode = |bytes: &[u8]| decode_to_utf16(decoder, bytes, last, fatal);
            let output = match input {
                Some(AllowSharedBufferSource::View(view)) => view.with_bytes(decode),
                Some(AllowSharedBufferSource::Buffer(buffer)) => buffer.with_bytes(decode),
                Some(AllowSharedBufferSource::Shared(buffer)) => buffer.with_bytes(decode),
                None => decode(&[]),
            };
            (output, encoding)
        };
        let site = BindingSite {
            interface: <TextDecoderStreamImpl as js::class::ClassDef>::NAME,
            member: "",
            kind: MemberKind::Method,
        };
        let output = output.map_err(|error| error.throw(scope, site, encoding))?;
        if output.is_empty() {
            return Ok(());
        }
        let chunk = JSString::from_utf16(scope, &output)?;
        self.transform(scope)
            .enqueue(scope, scope.root_value(chunk.as_value()))
    }

    /// <https://encoding.spec.whatwg.org/#decode-and-enqueue-a-chunk>
    fn decode_and_enqueue_a_chunk(
        &self,
        scope: &Scope<'_>,
        chunk: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        // Step 1: Let _bufferSource_ be the result of `converting` _chunk_ to an
        //         ``AllowSharedBufferSource``.
        let buffer_source =
            AllowSharedBufferSource::from_jsval(scope, chunk, ()).map_err(|error| match error {
                ConversionError::ExnPending => ExnThrown,
                ConversionError::Failure(_) => js::error::throw_type_error(
                    scope,
                    c"The chunk is not an ArrayBuffer, SharedArrayBuffer or ArrayBufferView",
                ),
            })?;
        // Step 2: `Push` a `copy of` _bufferSource_ to _decoder_'s `I/O queue`.
        // We decode from _bufferSource_'s bytes before anything can run JavaScript, and nothing
        // retains them.
        // Step 3: Let _output_ be the `I/O queue` of scalar values « `end-of-queue` ».
        // Step 4: While true:
        // Step 4.1: Let _item_ be the result of `reading` from _decoder_'s `I/O queue`.
        // Step 4.2: If _item_ is `end-of-queue`:
        // Step 4.2.1: Let _outputChunk_ be the result of running `serialize I/O queue` with
        //             _decoder_ and _output_.
        // Step 4.2.2: If _outputChunk_ is not the empty string, then `enqueue` _outputChunk_ in
        //             _decoder_'s `transform`.
        // Step 4.2.3: Return.
        // Step 4.3: Let _result_ be the result of `processing an item` with _item_, _decoder_'s
        //           `decoder`, _decoder_'s `I/O queue`, _output_, and _decoder_'s `error mode`.
        // Step 4.4: If _result_ is `error`, then `throw` a ``TypeError``.
        // One `encoding_rs` call processes every item. The decoder's BOM removal performs
        // `serialize I/O queue`'s BOM handling.
        self.decode_and_enqueue(scope, Some(buffer_source), false)
    }

    /// <https://encoding.spec.whatwg.org/#flush-and-enqueue>
    fn flush_and_enqueue(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: Let _output_ be the `I/O queue` of scalar values « `end-of-queue` ».
        // Step 2: While true:
        // Step 2.1: Let _item_ be the result of `reading` from _decoder_'s `I/O queue`.
        // Step 2.2: Let _result_ be the result of `processing an item` with _item_, _decoder_'s
        //           `decoder`, _decoder_'s `I/O queue`, _output_, and _decoder_'s `error mode`.
        // Step 2.3: If _result_ is `finished`:
        // Step 2.3.1: Let _outputChunk_ be the result of running `serialize I/O queue` with
        //             _decoder_ and _output_.
        // Step 2.3.2: If _outputChunk_ is not the empty string, then `enqueue` _outputChunk_ in
        //             _decoder_'s `transform`.
        // Step 2.3.3: Return.
        // Step 2.4: Otherwise, if _result_ is `error`, `throw` a ``TypeError``.
        // One `encoding_rs` call with `last` set processes `end-of-queue`.
        self.decode_and_enqueue(scope, None, true)
    }
}

/// The `transformAlgorithm` of a `TextDecoderStream`, whose payload is the stream.
fn transform_algorithm(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TextDecoderStream>(scope, payload);
    stream.decode_and_enqueue_a_chunk(scope, args.get(0))?;
    Ok(js::value::undefined())
}

/// The `flushAlgorithm` of a `TextDecoderStream`, whose payload is the stream.
fn flush_algorithm(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let stream = cast_payload::<TextDecoderStream>(scope, payload);
    stream.flush_and_enqueue(scope)?;
    Ok(js::value::undefined())
}
