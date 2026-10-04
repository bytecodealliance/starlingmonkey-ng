// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://fetch.spec.whatwg.org/#body-mixin>
//!
//! The `Body` interface mixin, which both `Request` and `Response` include.

use crate::algorithms::{self, Body, BodySource, ConsumeType};
use crate::incoming_body::{
    consume_host_body, materialize_host_body, HostBackedBodyOwner, HostBodySourceImpl,
};
use core_runtime::webidl_union;
use js::error::{throw_type_error, ExnThrown};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{ArrayBuffer, ArrayBufferView, Promise};
use platform::http::IncomingBody;
use std::ffi::CStr;
use web_streams::readable::readable_stream::ReadableStream;
use web_streams::readable::readable_stream::ReadableStreamImpl;
use web_url::url_search_params::URLSearchParams;

/// The body state of a `Request` or `Response` object.
#[js::must_root]
#[derive(core_runtime::Traceable, Default)]
pub(crate) struct BodyStorage {
    /// The body record, or `None` for a null body.
    #[no_trace]
    pub(crate) record: Option<Body>,
    /// The body's `ReadableStream`: a user-provided stream, or one materialized lazily from the
    /// body's byte source or host body on `.body` access.
    pub(crate) stream: Option<Heap<ReadableStreamImpl>>,
    /// The body as received from the host, until the first of `consume`/`.body` takes it.
    #[no_trace]
    pub(crate) host: Option<IncomingBody>,
    /// The `.body` stream's native host source, kept so an abort can cancel an in-flight host
    /// read. Set when `.body` materializes a host-backed stream.
    pub(crate) source: Option<Heap<HostBodySourceImpl>>,
    /// The record's byte source, attributed to the owning object for the GC. Kept in sync by
    /// [`sync_accounting`](Self::sync_accounting).
    #[no_trace]
    pub(crate) accounted: js::gc::AssociatedMemory,
}

impl BodyStorage {
    /// A body with `record` and, if given, the stream it already has.
    pub(crate) fn new(record: Option<Body>, stream: Option<ReadableStream<'_>>) -> Self {
        Self {
            record,
            stream: stream.map(Heap::from),
            ..Default::default()
        }
    }

    /// Attribute the record's byte source to `object`, the object owning this storage, for the
    /// GC, replacing the previous amount. Called wherever the byte source is set, taken, or
    /// dropped.
    pub(crate) fn sync_accounting(&mut self, object: js::Object<'_>) {
        let bytes = match &self.record {
            Some(Body {
                source: BodySource::Bytes(bytes),
                ..
            }) => bytes.len(),
            _ => 0,
        };
        self.accounted.set(object, bytes);
    }
}

#[webidl_union]
pub enum BodyInit<'a> {
    Stream(ReadableStream<'a>),
    Blob(web_files::blob::Blob<'a>),
    ArrayBuffer(ArrayBuffer<'a>),
    ArrayBufferView(ArrayBufferView<'a>),
    // FormData, // Not yet supported.
    URLSearchParams(URLSearchParams<'a>),
    USVString(String),
}

/// <https://fetch.spec.whatwg.org/#body-mixin>
///
/// [`HostBackedBodyOwner`] supplies the storage this builds on: the body record, the
/// `.body` stream slot, and the not-yet-read host body.
pub(crate) trait BodyMixin: HostBackedBodyOwner + Sized {
    /// The `TypeError` message when this object is `unusable`. Per-interface
    /// only because it names the interface.
    const UNUSABLE_MESSAGE: &'static CStr;

    /// The `TypeError` message for the not-yet-supported `textStream()`.
    const TEXT_STREAM_UNSUPPORTED: &'static CStr;

    /// Store the `.body` stream materialized from the body's byte source.
    fn set_body_stream(&self, stream: ReadableStream<'_>) {
        self.with_body(|body| {
            debug_assert!(body.stream.is_none());
            body.stream = Some(Heap::from(stream));
        });
    }

    /// Mark the body's byte source as read — the `disturbed` of the stream it
    /// would have materialized into.
    fn set_source_disturbed(&self) {
        self.with_body(|body| {
            if let Some(record) = body.record.as_mut() {
                record.source_disturbed = true;
            }
        });
    }

    /// Run once the body has been consumed. `Response` overrides it to detach
    /// its `fetch` abort algorithm, which can no longer act on a read body.
    fn on_body_consumed(&self, _scope: &Scope<'_>) {}

    /// <https://fetch.spec.whatwg.org/#body-unusable>
    /// An object including the Body interface mixin is unusable if its body is
    /// non-null and its body's stream is disturbed or locked.
    fn is_unusable(&self, scope: &Scope<'_>) -> bool {
        let Some(body) = self.body_record() else {
            return false;
        };
        // A byte source that was read has no stream to ask; the flag stands in for its
        // `disturbed`.
        if body.source_disturbed {
            return true;
        }
        match self.body_stream(scope) {
            Some(stream) => stream.is_disturbed() || stream.is_locked(),
            None => false,
        }
    }

    /// Throw a `TypeError` naming this interface if it is `unusable`. The
    /// shared Step 1 of `clone()` and of `consume body`'s callers.
    fn throw_if_unusable(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        if self.is_unusable(scope) {
            return Err(throw_type_error(scope, Self::UNUSABLE_MESSAGE));
        }
        Ok(())
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-body>
    fn body<'r>(&self, scope: &'r Scope<'_>) -> Result<Option<ReadableStream<'r>>, ExnThrown> {
        // Step 1: Return null if `this`’s `body` is null; otherwise `this`’s `body`’s `stream`.
        // A body still on the host (an incoming server request, or a `fetch` response) materializes
        // its stream from the host on first access.
        materialize_host_body(scope, self)?;
        let Some(body) = self.body_record() else {
            return Ok(None);
        };
        if let Some(stream) = self.body_stream(scope) {
            return Ok(Some(stream));
        }
        // `extract a body` Steps 4 and 12, deferred: a byte source materializes its stream here,
        // on first access, rather than when the body was extracted.
        let stream = match &body.source {
            // An already-consumed byte source materializes its stream in the consumed state —
            // closed, locked, disturbed — observably the stream that existed all along and was
            // fully read by `text()`/…; a fresh full-content stream here would let the body be
            // read twice.
            BodySource::Bytes(_) if body.source_disturbed => ReadableStream::new_consumed(scope)?,
            BodySource::Bytes(bytes) => ReadableStream::from_bytes(scope, bytes)?,
            // No stream and a null source: `extract a body` never produces this, so there is no
            // body to expose.
            BodySource::Null => return Ok(None),
        };
        self.set_body_stream(stream);
        Ok(Some(stream))
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-bodyused>
    fn body_used(&self, scope: &Scope<'_>) -> bool {
        // Step 1: Return true if `this`’s `body` is non-null and `this`’s `body`’s `stream` is
        //     `disturbed`; otherwise false.
        match self.body_record() {
            None => false,
            Some(body) => {
                body.source_disturbed
                    || self
                        .body_stream(scope)
                        .is_some_and(|stream| stream.is_disturbed())
            }
        }
    }

    /// Run `consume body` with this object and the `convertBytesToJSValue` that
    /// `conversion` names — the shared body of `arrayBuffer()`, `blob()`,
    /// `bytes()`, `formData()`, `json()` and `text()`.
    ///
    /// A body still unread on the host is read straight from it , with no `ReadableStream` in
    /// between. Otherwise, `consume body` reads the byte source or the materialized stream.
    fn consume<'r>(
        &self,
        scope: &'r Scope<'_>,
        conversion: ConsumeType,
    ) -> Result<Promise<'r>, ExnThrown> {
        if let Some(host_body) = self.take_host_body() {
            self.set_source_disturbed();
            self.on_body_consumed(scope);
            return consume_host_body(scope, host_body, conversion);
        }
        self.on_body_consumed(scope);
        algorithms::consume_body(scope, self, conversion)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-arraybuffer>
    fn array_buffer<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Return the result of running `consume body` with `this` and the following step
        //     given a `byte sequence` _bytes_: return the result of `creating an ArrayBuffer` from
        //     _bytes_ in `this`’s `relevant realm`. The above method can reject with a
        //     `RangeError`.
        self.consume(scope, ConsumeType::ArrayBuffer)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-blob>
    fn blob<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Return the result of running `consume body` with `this` and the following step
        //     given a `byte sequence` _bytes_: return a `Blob` whose contents are _bytes_ and
        //     whose `type` attribute is the result of `get the MIME type` with `this`.
        self.consume(scope, ConsumeType::Blob)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-bytes>
    fn bytes<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Return the result of running `consume body` with `this` and the following step
        //     given a `byte sequence` _bytes_: return the result of `creating a Uint8Array` from
        //     _bytes_ in `this`’s `relevant realm`. The above method can reject with a
        //     `RangeError`.
        self.consume(scope, ConsumeType::Bytes)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-formdata>
    fn form_data<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _mimeType_ be the result of `get the MIME type` with `this`.
        // Step 2: If _mimeType_ is non-null, then switch on _mimeType_’s `essence` and run the
        //     corresponding steps:
        // Step 2 "`multipart/form-data`".1: Parse _bytes_, using the value of the `boundary`
        //     parameter from _mimeType_, per the rules set forth in Returning Values from Forms:
        //     multipart/form-data. `[RFC7578]` ...
        // Step 2 "`multipart/form-data`".2: If that fails for some reason, then `throw` a
        //     `TypeError`.
        // Step 2 "`multipart/form-data`".3: Return a new `FormData`` object, appending each
        //     `entry`, resulting from the parsing operation, to its `entry list`.
        // Step 2 "`application/x-www-form-urlencoded`".1: Let _entries_ be the result of `parsing`
        //     _bytes_.
        // Step 2 "`application/x-www-form-urlencoded`".2: Return a new ``FormData`` object whose
        //     `entry list` is _entries_.
        // Step 3: `Throw` a ``TypeError``.
        // ``FormData`` is not yet available in this runtime; the body is still consumed (disturbing
        // it) per `consume body`, then the conversion rejects. The full `multipart/form-data`
        // parsing rules land with it.
        self.consume(scope, ConsumeType::FormData)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-json>
    fn json<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Return the result of running `consume body` with `this` and `parse JSON from
        //     bytes`. The above method can reject with a ``SyntaxError``.
        self.consume(scope, ConsumeType::Json)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-text>
    fn text<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Return the result of running `consume body` with `this` and `UTF-8 decode`.
        self.consume(scope, ConsumeType::Text)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-textstream>
    fn text_stream<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        // returns WebIDL: ReadableStream
        // Step 1: If `this` is `unusable`, then `throw` a ``TypeError``.
        // Step 2: If `this`’s `body` is null:
        // Step 2.1: Let _emptyStream_ be a new ``ReadableStream`` in `this`’s `relevant realm`.
        // Step 2.2: `Set up` _emptyStream_.
        // Step 2.3: `Close` _emptyStream_.
        // Step 2.4: Return _emptyStream_.
        // Step 3: Let _stream_ be `this`’s `body`’s `stream`.
        // Step 4: Let _decoder_ be a new ``TextDecoderStream`` object in `this`’s `relevant realm`.
        // Step 5: `Set up` _decoder_ with `UTF-8`. This is done regardless of the presence or the
        //     value of a ``Content-Type`` header and regardless of the presence or the value of a
        //     ``charset`` parameter.
        // Step 6: Return the result of _stream_, `piped through` _decoder_.
        // `textStream()` depends on `TextDecoderStream` piping, deferred to a later milestone;
        // throw for now.
        Err(throw_type_error(scope, Self::TEXT_STREAM_UNSUPPORTED))
    }
}
