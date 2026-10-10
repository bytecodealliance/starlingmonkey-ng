// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/FileAPI/#blob-section>

use std::collections::TryReserveError;

use bytes::Bytes;
use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods, webidl_union};
use js::conversion::{Clamp, ToJSVal};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::{ArrayBuffer, ArrayBufferView, Promise, Uint8Array};
use web_streams::readable::readable_stream::ReadableStream;

js::webidl_enum! {
    /// <https://w3c.github.io/FileAPI/#enumdef-endingtype>
    pub enum EndingType {
        Transparent => "transparent",
        Native => "native",
    }
}

/// <https://w3c.github.io/FileAPI/#dfn-BlobPropertyBag>
#[webidl_dictionary]
pub struct BlobPropertyBag {
    pub r#type: Option<String>,
    pub endings: Option<EndingType>,
}

/// <https://w3c.github.io/FileAPI/#typedefdef-blobpart>: `(BufferSource or Blob or USVString)`.
#[webidl_union]
pub enum BlobPart<'a> {
    Blob(Blob<'a>),
    ArrayBuffer(ArrayBuffer<'a>),
    ArrayBufferView(ArrayBufferView<'a>),
    USVString(String),
}

/// <https://w3c.github.io/FileAPI/#blob-section>
#[webidl_interface]
pub struct Blob {
    /// The blob's byte sequence. Slices of a blob share the bytes of the blob they were sliced
    /// from.
    #[no_trace]
    pub bytes: Bytes,
    /// <https://w3c.github.io/FileAPI/#dfn-type>
    #[no_trace]
    pub r#type: String,
}

impl BlobImpl {
    /// A blob of `bytes`, whose type is `blob_type`.
    pub fn with_bytes(bytes: Bytes, blob_type: String) -> Self {
        BlobImpl {
            bytes,
            r#type: blob_type,
        }
    }

    /// <https://w3c.github.io/FileAPI/#constructorBlob>, after the arguments are converted: a blob
    /// of the bytes of `blob_parts` and the type of `options`, or the error of a failed
    /// allocation of the bytes.
    pub fn from_parts(
        blob_parts: Option<Vec<BlobPart<'_>>>,
        options: Option<&BlobPropertyBag>,
    ) -> Result<Self, TryReserveError> {
        // Step 1: If invoked with zero parameters, return a new ``Blob`` object consisting of 0
        //     bytes, with `size` set to 0, and with `type` set to the empty string.
        // (The other steps produce such a blob for no parts and no options too.)
        // Step 2: Let _bytes_ be the result of `processing blob parts` given _blobParts_ and
        //     _options_.
        let endings = options.and_then(|options| options.endings);
        let bytes = process_blob_parts(blob_parts.unwrap_or_default(), endings)?;
        // Step 3: If the ``type`` member of the _options_ argument is not the empty string, run
        //     the following sub-steps:
        // Step 3.1: Let _t_ be the ``type`` dictionary member. If _t_ contains any characters
        //     outside the range U+0020 to U+007E, then set _t_ to the empty string and return
        //     from these substeps.
        // Step 3.2: Convert every character in _t_ to `ASCII lowercase`.
        let t = options
            .and_then(|options| options.r#type.as_deref())
            .map_or_else(String::new, normalize_type);
        // Step 4: Return a ``Blob`` object referring to _bytes_ as its associated `byte
        //     sequence`, with its `size` set to the length of _bytes_, and its `type` set to the
        //     value of _t_ from the substeps above.
        Ok(BlobImpl::with_bytes(bytes, t))
    }
}

/// The type `t` with every character `ASCII lowercase`d, or the empty string if `t` contains a
/// character outside the range U+0020 to U+007E.
pub fn normalize_type(t: &str) -> String {
    if t.chars().any(|c| !('\u{20}'..='\u{7e}').contains(&c)) {
        String::new()
    } else {
        t.to_ascii_lowercase()
    }
}

/// <https://w3c.github.io/FileAPI/#process-blob-parts>, or the error of a failed allocation of
/// the bytes.
pub fn process_blob_parts(
    parts: Vec<BlobPart<'_>>,
    endings: Option<EndingType>,
) -> Result<Bytes, TryReserveError> {
    // Step 1: Let _bytes_ be an empty sequence of bytes.
    let mut bytes = Vec::new();
    // Step 2: For each _element_ in _parts_:
    for element in parts {
        match element {
            // Step 2.1: If _element_ is a ``USVString``, run the following substeps:
            BlobPart::USVString(s) => {
                // Step 2.1.1: Let _s_ be _element_.
                // Step 2.1.2: If the ``endings`` member of _options_ is
                //     "<code>native</code>", set _s_ to the result of `converting line endings
                //     to native` of _element_.
                // Step 2.1.3: Append the result of `UTF-8 encoding` _s_ to _bytes_.
                if endings == Some(EndingType::Native) {
                    append(&mut bytes, convert_line_endings_to_native(&s)?.as_bytes())?;
                } else {
                    append(&mut bytes, s.as_bytes())?;
                }
            }
            // Step 2.2: If _element_ is a ``BufferSource``, `get a copy of the bytes held by the
            //     buffer source`, and append those bytes to _bytes_.
            BlobPart::ArrayBuffer(buffer) => buffer.with_bytes(|data| append(&mut bytes, data))?,
            BlobPart::ArrayBufferView(view) => view.with_bytes(|data| append(&mut bytes, data))?,
            // Step 2.3: If _element_ is a ``Blob``, append the bytes it represents to _bytes_.
            BlobPart::Blob(blob) => append(&mut bytes, &blob.data().bytes)?,
        }
    }
    // Step 3: Return _bytes_.
    Ok(Bytes::from(bytes))
}

/// Append `data` to `bytes`, reserving the room for it fallibly.
fn append(bytes: &mut Vec<u8>, data: &[u8]) -> Result<(), TryReserveError> {
    bytes.try_reserve(data.len())?;
    bytes.extend_from_slice(data);
    Ok(())
}

/// <https://w3c.github.io/FileAPI/#convert-line-endings-to-native>, or the error of a failed
/// allocation of the result.
fn convert_line_endings_to_native(s: &str) -> Result<String, TryReserveError> {
    // Step 1: Let _native line ending_ be the `code point` U+000A LF.
    // Step 2: If the underlying platform’s conventions are to represent newlines as a carriage
    //     return and line feed sequence, set _native line ending_ to the `code point` U+000D CR
    //     followed by the `code point` U+000A LF.
    let native_line_ending = if cfg!(windows) { "\r\n" } else { "\n" };
    // Step 3: Set _result_ to the empty `string`.
    let mut result = String::new();
    result.try_reserve_exact(s.len())?;
    // Step 4: Let _position_ be a `position variable` for _s_, initially pointing at the start
    //     of _s_.
    let mut rest = s;
    let is_newline = |c: char| c == '\n' || c == '\r';
    // Step 5: Let _token_ be the result of `collecting a sequence of code points` that are not
    //     equal to U+000A LF or U+000D CR from _s_ given _position_.
    let end = rest.find(is_newline).unwrap_or(rest.len());
    // Step 6: Append _token_ to _result_.
    result.push_str(&rest[..end]);
    rest = &rest[end..];
    // Step 7: While _position_ is not past the end of _s_:
    while !rest.is_empty() {
        // Step 7.1: If the `code point` at _position_ within _s_ equals U+000D CR:
        if let Some(after) = rest.strip_prefix('\r') {
            // Step 7.1.1: Append _native line ending_ to _result_.
            result.push_str(native_line_ending);
            // Step 7.1.2: Advance _position_ by 1.
            rest = after;
            // Step 7.1.3: If _position_ is not past the end of _s_ and the `code point` at
            //     _position_ within _s_ equals U+000A LF advance _position_ by 1.
            rest = rest.strip_prefix('\n').unwrap_or(rest);
        // Step 7.2: Otherwise if the `code point` at _position_ within _s_ equals U+000A LF,
        //     advance _position_ by 1 and append _native line ending_ to _result_.
        } else if let Some(after) = rest.strip_prefix('\n') {
            rest = after;
            result.push_str(native_line_ending);
        }
        // Step 7.3: Let _token_ be the result of `collecting a sequence of code points` that are
        //     not equal to U+000A LF or U+000D CR from _s_ given _position_.
        let end = rest.find(is_newline).unwrap_or(rest.len());
        // Step 7.4: Append _token_ to _result_.
        result.push_str(&rest[..end]);
        rest = &rest[end..];
    }
    // Step 8: Return _result_.
    Ok(result)
}

/// <https://w3c.github.io/FileAPI/#slice-blob>: the bytes of `bytes` from `start` to `end`, and
/// the type of the blob of them.
fn slice_blob(
    bytes: &Bytes,
    start: Option<i64>,
    end: Option<i64>,
    content_type: Option<String>,
) -> (Bytes, String) {
    // Step 1: Let _originalSize_ be _blob_’s ``size``.
    let original_size = bytes.len() as i64;
    // Step 2: The _start_ parameter, if non-null, is a value for the start point of a `slice
    //     blob` call, and must be treated as a byte-order position, with the zeroth position
    //     representing the first byte. User agents must normalize _start_ according to the
    //     following:
    let relative_start = match start {
        // Step 2 If _start_ is null, let _relativeStart_ be 0.
        None => 0,
        // Step 2 If _start_ is negative, let _relativeStart_ be `max((originalSize + start),
        //     0)`.
        Some(start) if start < 0 => (original_size.saturating_add(start)).max(0),
        // Step 2 Otherwise, let _relativeStart_ be `min(start, originalSize)`.
        Some(start) => start.min(original_size),
    };
    // Step 3: The _end_ parameter, if non-null. is a value for the end point of a `slice blob`
    //     call. User agents must normalize _end_ according to the following:
    let relative_end = match end {
        // Step 3 If _end_ is null, let _relativeEnd_ be _originalSize_.
        None => original_size,
        // Step 3 If _end_ is negative, let _relativeEnd_ be `max((originalSize + end), 0)`.
        Some(end) if end < 0 => (original_size.saturating_add(end)).max(0),
        // Step 3 Otherwise, let _relativeEnd_ be `min(end, originalSize)`.
        Some(end) => end.min(original_size),
    };
    // Step 4: The _contentType_ parameter, if non-null, is used to set the ASCII-encoded string
    //     in lower case representing the media type of the ``Blob``. User agents must normalize
    //     _contentType_ according to the following:
    // Step 4 If _contentType_ is null, let _relativeContentType_ be set to the empty string.
    // Step 4 Otherwise, let _relativeContentType_ be set to _contentType_ and run the substeps
    //     below:
    // Step 4.1: If _relativeContentType_ contains any characters outside the range of U+0020 to
    //     U+007E, then set _relativeContentType_ to the empty string and return from these
    //     substeps.
    // Step 4.2: Convert every character in _relativeContentType_ to `ASCII lowercase`.
    let relative_content_type = content_type
        .as_deref()
        .map_or_else(String::new, normalize_type);
    // Step 5: Let _span_ be `max((relativeEnd - relativeStart), 0)`.
    let span = (relative_end - relative_start).max(0);
    // Step 6: Return a new ``Blob`` object _S_ with the following characteristics:
    // Step 6.1: _S_ refers to _span_ consecutive `byte`s from _blob_’s associated `byte
    //     sequence`, beginning with the `byte` at byte-order position _relativeStart_.
    // Step 6.2: _S_.``size`` = _span_.
    // Step 6.3: _S_.``type`` = _relativeContentType_.
    let start = relative_start as usize;
    (
        bytes.slice(start..start + span as usize),
        relative_content_type,
    )
}

/// <https://encoding.spec.whatwg.org/#utf-8-decode>: strip a leading UTF-8 BOM, then decode,
/// replacing malformed sequences with U+FFFD.
fn utf8_decode<'r>(scope: &'r Scope<'_>, bytes: &[u8]) -> Result<js::JSString<'r>, ExnThrown> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    js::JSString::from_utf8_lossy(scope, bytes)
}

#[webidl_methods]
impl Blob {
    /// <https://w3c.github.io/FileAPI/#constructorBlob>
    #[constructor]
    fn new(
        scope: &Scope<'_>,
        blob_parts: Option<Vec<BlobPart<'_>>>,
        options: Option<&BlobPropertyBag>,
    ) -> Result<Self, ExnThrown> {
        BlobImpl::from_parts(blob_parts, options).map_err(|_| {
            js::exception::report_out_of_memory(scope);
            ExnThrown
        })
    }

    /// <https://w3c.github.io/FileAPI/#dfn-size>
    #[getter]
    fn size(&self) -> u64 {
        // Returns the size of the `byte sequence` in number of bytes.
        self.data().bytes.len() as u64
    }

    /// <https://w3c.github.io/FileAPI/#dfn-type>
    #[getter(name = "type")]
    fn get_type(&self) -> String {
        self.data().r#type.clone()
    }

    /// <https://w3c.github.io/FileAPI/#dfn-slice>
    #[method]
    fn slice<'r>(
        &self,
        scope: &'r Scope<'_>,
        start: Option<Clamp<i64>>,
        end: Option<Clamp<i64>>,
        content_type: Option<String>,
    ) -> Result<Blob<'r>, ExnThrown> {
        // Step 1: Let _sliceStart_, _sliceEnd_, and _sliceContentType_ be null.
        // Step 2: If _start_ is given, set _sliceStart_ to _start_.
        // Step 3: If _end_ is given, set _sliceEnd_ to _end_.
        // Step 4: If _contentType_ is given, set _sliceContentType_ to _contentType_.
        // Step 5: Return the result of `slice blob` given `this`, _sliceStart_, _sliceEnd_, and
        //     _sliceContentType_.
        let (bytes, blob_type) = slice_blob(
            &self.data().bytes,
            start.map(|start| start.0),
            end.map(|end| end.0),
            content_type,
        );
        Blob::create(scope, bytes, blob_type)
    }

    /// <https://w3c.github.io/FileAPI/#dom-blob-stream>
    #[method]
    fn stream<'r>(&self, scope: &'r Scope<'_>) -> Result<ReadableStream<'r>, ExnThrown> {
        // Step 1: Return the result of calling `get stream` on `this`.
        self.get_stream(scope)
    }

    /// <https://w3c.github.io/FileAPI/#dom-blob-text>
    #[method]
    fn text<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _stream_ be the result of calling `get stream` on `this`.
        // Step 2: Let _reader_ be the result of `getting a reader` from _stream_. If that threw
        //     an exception, return a new promise rejected with that exception.
        // Step 3: Let _promise_ be the result of `reading all bytes` from _stream_ with _reader_.
        // Step 4: Return the result of transforming _promise_ by a fulfillment handler that
        //     returns the result of running `UTF-8 decode` on its first argument.
        // (The blob's bytes are in memory, so the promise is resolved with their decoding
        // without a stream.)
        let bytes = self.data().bytes.clone();
        let value = utf8_decode(scope, &bytes)?.to_jsval_throwing(scope)?;
        Promise::new_resolved_with_value(scope, value)
    }

    /// <https://w3c.github.io/FileAPI/#dom-blob-arraybuffer>
    #[method]
    fn array_buffer<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _stream_ be the result of calling `get stream` on `this`.
        // Step 2: Let _reader_ be the result of `getting a reader` from _stream_. If that threw
        //     an exception, return a new promise rejected with that exception.
        // Step 3: Let _promise_ be the result of `reading all bytes` from _stream_ with _reader_.
        // Step 4: Return the result of transforming _promise_ by a fulfillment handler that
        //     returns a new ``ArrayBuffer`` whose contents are its first argument.
        // (The blob's bytes are in memory, so the promise is resolved with their copy without a
        // stream.)
        let buffer = ArrayBuffer::with_data(scope, &self.data().bytes)?;
        let value = scope.root_value(buffer.as_value());
        Promise::new_resolved_with_value(scope, value)
    }

    /// <https://w3c.github.io/FileAPI/#dom-blob-bytes>
    #[method]
    fn bytes<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _stream_ be the result of calling `get stream` on `this`.
        // Step 2: Let _reader_ be the result of `getting a reader` from _stream_. If that threw
        //     an exception, return a new promise rejected with that exception.
        // Step 3: Let _promise_ be the result of `reading all bytes` from _stream_ with _reader_.
        // Step 4: Return the result of transforming _promise_ by a fulfillment handler that
        //     returns a new ``Uint8Array`` wrapping an ``ArrayBuffer`` containing its first
        //     argument.
        // (The blob's bytes are in memory, so the promise is resolved with their copy without a
        // stream.)
        let array = Uint8Array::with_data(scope, &self.data().bytes)?;
        let value = scope.root_value(array.as_value());
        Promise::new_resolved_with_value(scope, value)
    }
}

impl<'s> Blob<'s> {
    /// A new `Blob` object of `bytes`, whose type is `blob_type`.
    pub fn create<'r>(
        scope: &'r Scope<'_>,
        bytes: Bytes,
        blob_type: String,
    ) -> Result<Blob<'r>, ExnThrown> {
        js::class::create_instance_with::<BlobImpl>(scope, |_| {
            BlobImpl::with_bytes(bytes, blob_type)
        })
    }

    /// <https://w3c.github.io/FileAPI/#blob-get-stream>: a readable byte stream of the blob's
    /// bytes, as one chunk.
    pub fn get_stream<'r>(&self, scope: &'r Scope<'_>) -> Result<ReadableStream<'r>, ExnThrown> {
        // Step 1: Let _stream_ be a `new` ``ReadableStream`` created in _blob_’s `relevant
        //     Realm`.
        // Step 2: `Set up` _stream_ with byte reading support.
        // Step 3: Run the following steps `in parallel`:
        // Step 3.1: While not all bytes of _blob_ have been read:
        // Step 3.1.1: Let _bytes_ be the `byte sequence` that results from reading a `chunk` from
        //     _blob_, or failure if a chunk cannot be read.
        // Step 3.1.2: `Queue a global task` on the `file reading task source` given _blob_’s
        //     `relevant global object` to perform the following steps:
        // Step 3.1.2.1: If _bytes_ is failure, then `error` _stream_ with a `failure reason` and
        //     abort these steps.
        // Step 3.1.2.2: Let _chunk_ be a new ``Uint8Array`` wrapping an ``ArrayBuffer``
        //     containing _bytes_. If creating the ``ArrayBuffer`` throws an exception, then
        //     `error` _stream_ with that exception and abort these steps.
        // Step 3.1.2.3: `Enqueue` _chunk_ in _stream_.
        // Step 4: Return _stream_.
        // (The blob's bytes are in memory, so they are enqueued as one chunk right away.)
        let bytes = self.data().bytes.clone();
        ReadableStream::from_bytes_with_byte_reading(scope, &bytes)
    }
}
