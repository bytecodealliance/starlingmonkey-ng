// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `CompressionStream` and `DecompressionStream` interfaces of
//! <https://compression.spec.whatwg.org/>, over the codecs of the `compression` crate.
//!
//! Each call to a codec produces at most [`OUTPUT_LIMIT`] bytes, which become one `Uint8Array`
//! chunk, so the output of a chunk is enqueued while it is produced.

pub mod compression_stream;
pub mod decompression_stream;

use compression::{Flush, Progress, ZlibError};
use js::conversion::FromJSVal;
use js::error::{
    throw_binding_error, BindingError, BindingSite, ExnThrown, MemberKind, ThrowException,
    TypeError,
};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Object, Uint8Array};
use web_globals::encoding::ArrayBufferViewOrArrayBuffer;
use web_streams::transform::transform_stream::TransformStream;

js::webidl_enum! {
    /// <https://compression.spec.whatwg.org/#enumdef-compressionformat>
    pub enum CompressionFormat {
        Brotli => "brotli",
        Deflate => "deflate",
        DeflateRaw => "deflate-raw",
        Gzip => "gzip",
    }
}

/// The most bytes of output one call to a codec produces.
const OUTPUT_LIMIT: usize = 64 * 1024;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    compression_stream::CompressionStream::add_to_global(scope, global);
    decompression_stream::DecompressionStream::add_to_global(scope, global);
}

/// A copy of the bytes of `chunk`, a chunk written to a stream of the class `interface`. A
/// `chunk` that isn't a `BufferSource`, which is an `ArrayBuffer` or a view on one that isn't a
/// `SharedArrayBuffer`, throws the binding error of an invalid argument of the class's transform
/// algorithm, whose member name is empty.
fn buffer_source_bytes(
    scope: &Scope<'_>,
    chunk: HandleValue<'_>,
    interface: &'static str,
) -> Result<Vec<u8>, ExnThrown> {
    // SAFETY: the slices are copied before anything can run a GC.
    let bytes = match ArrayBufferViewOrArrayBuffer::from_jsval(scope, chunk, ()) {
        Ok(ArrayBufferViewOrArrayBuffer::Buffer(buffer)) => unsafe { buffer.bytes() },
        Ok(ArrayBufferViewOrArrayBuffer::View(view)) if !view.is_shared() => unsafe {
            view.bytes()
        },
        Err(js::conversion::ConversionError::ExnPending) => return Err(ExnThrown),
        _ => {
            let site = BindingSite {
                interface,
                member: "",
                kind: MemberKind::Method,
            };
            let error = BindingError::InvalidArgument {
                index: 0,
                value: chunk,
                message: c"The chunk is not an ArrayBuffer or ArrayBufferView",
            };
            return Err(throw_binding_error(scope, &site, error));
        }
    };
    let mut copy = Vec::new();
    if copy.try_reserve_exact(bytes.len()).is_err() {
        js::exception::report_out_of_memory(scope);
        return Err(ExnThrown);
    }
    copy.extend_from_slice(bytes);
    Ok(copy)
}

/// Run a codec on `input` with `flush` until it consumed `input` and produced all output, and
/// enqueue each piece of output in `transform` as a `Uint8Array`. `process` calls the codec's
/// `Codec::process`.
///
/// A codec error throws a `TypeError` with its message. For the errors that follow the output of
/// the input (see [`ZlibError::follows_output`]), that output is enqueued first. For the others,
/// the output of the call that failed isn't enqueued.
fn process_and_enqueue(
    scope: &Scope<'_>,
    transform: &TransformStream<'_>,
    input: &[u8],
    flush: Flush,
    mut process: impl FnMut(&[u8], Flush, usize, &mut Vec<u8>) -> Result<Progress, ZlibError>,
) -> Result<(), ExnThrown> {
    let mut offset = 0;
    loop {
        let mut output = Vec::new();
        let result = process(&input[offset..], flush, OUTPUT_LIMIT, &mut output);
        if let Err(error) = &result {
            if !error.follows_output() {
                output.clear();
            }
        }
        if !output.is_empty() {
            let chunk = Uint8Array::with_data(scope, &output)?;
            transform.enqueue(scope, scope.root_value(chunk.as_value()))?;
        }
        match result {
            Ok(progress) if progress.done => return Ok(()),
            Ok(progress) => offset += progress.consumed,
            Err(error) if error.code.is_empty() => {
                js::exception::report_out_of_memory(scope);
                return Err(ExnThrown);
            }
            Err(error) => return Err(TypeError(error.message).throw(scope)),
        }
    }
}
