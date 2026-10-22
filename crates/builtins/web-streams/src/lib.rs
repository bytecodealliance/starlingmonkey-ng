// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

pub(crate) mod algorithms;
pub mod queuing;
pub mod readable;
pub(crate) mod support;
pub mod transform;
pub mod writable;

pub use support::AlgorithmArg;

use js::gc::scope::Scope;
use js::Object;
use readable::readable_stream::ReadableStreamImpl;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    readable::readable_stream::ReadableStream::add_to_global(scope, global);
    readable::async_iterator::ReadableStreamAsyncIterator::add_to_global(scope, global);
    readable::default_reader::DefaultReader::add_to_global(scope, global);
    readable::byob_reader::BYOBReader::add_to_global(scope, global);
    readable::default_controller::ReadableStreamDefaultController::add_to_global(scope, global);
    readable::byte_stream_controller::ReadableByteStreamController::add_to_global(scope, global);
    readable::byob_request::ReadableStreamBYOBRequest::add_to_global(scope, global);
    writable::writable_stream::WritableStream::add_to_global(scope, global);
    writable::default_writer::WritableStreamDefaultWriter::add_to_global(scope, global);
    writable::default_controller::WritableStreamDefaultController::add_to_global(scope, global);
    transform::transform_stream::TransformStream::add_to_global(scope, global);
    transform::transform_stream_default_controller::TransformStreamDefaultController::add_to_global(
        scope, global,
    );
    queuing::ByteLengthQueuingStrategy::add_to_global(scope, global);
    queuing::CountQueuingStrategy::add_to_global(scope, global);

    // Internal classes holding the state of tee, pipe, from-iterable and read-all-bytes
    // operations. Creating instances needs their prototypes registered, and `hidden` keeps
    // their constructors off the global.
    readable::algorithms::TeeState::add_to_global(scope, global);
    readable::algorithms::ByteTeeState::add_to_global(scope, global);
    readable::algorithms::PipeState::add_to_global(scope, global);
    readable::algorithms::FromIterableState::add_to_global(scope, global);
    readable::read_all_bytes::ReadAllBytesState::add_to_global(scope, global);
    js::iteration::AsyncIteratorRecord::add_to_global(scope, global);

    // Chain the async iterator's prototype under `%AsyncIteratorPrototype%` so it
    // inherits `[Symbol.asyncIterator]` (returning `this`).
    if let Some(proto) = js::class::get_prototype_object_for::<
        readable::async_iterator::ReadableStreamAsyncIteratorImpl,
    >(scope)
    {
        if let Ok(async_proto) = js::class::get_async_iterator_prototype(scope) {
            let _ = proto.set_prototype(scope, async_proto);
        }
        // The default async iterator prototype has no `constructor` property
        // (per WebIDL §3.7.10.1, only `next`/`return`).
        let _ = proto.delete_property(scope, c"constructor");
    }

    // WebIDL `async iterable<chunk>`: `ReadableStream.prototype[@@asyncIterator]`
    // is the same function as `values`, installed non-enumerable per WebIDL.
    js::class::add_symbol_alias::<ReadableStreamImpl>(
        scope,
        c"values",
        js::native::SymbolCode::asyncIterator,
    );

    // Byte streams construct `ArrayBuffer`, `Uint8Array`, and `DataView` directly
    // (auto-allocation and `ReadableStreamBYOBRequest.view`). Resolve those standard
    // constructors now, while no stream state is live, so their lazy
    // first-construction never runs mid-operation with byte-stream objects
    // reachable. That trips a GC tracer assertion under moving GC.
    for key in [
        js::class_spec::JSProtoKey::JSProto_ArrayBuffer,
        js::class_spec::JSProtoKey::JSProto_Uint8Array,
        js::class_spec::JSProtoKey::JSProto_DataView,
    ] {
        let _ = js::class::get_class_object(scope, key);
    }
}
