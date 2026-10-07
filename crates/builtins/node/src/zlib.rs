// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:zlib`'s deflate, gzip, Brotli and zstd formats, and `crc32`. See
//! <https://nodejs.org/api/zlib.html>.
//!
//! The streams, the callback functions and the synchronous functions share one codec of the
//! `compression` crate. The callback functions compress or decompress synchronously and call back
//! in a microtask. The zip archive functions are stubs.
//!
//! `params` leaves the level unchanged when the engine has already compressed input and the new
//! level uses a different zlib strategy function, because `flate2` changes the level without an
//! output buffer.
//!
//! The zstd compression level has no effect, and flushes have no effect on zstd compressors and
//! decompressors.

use compression::{Codec, Flush, Format, Params, ZlibError};
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::{HandleId, Value};
use js::prelude::HandleValue;
use js::Object;

use crate::buffer::{encode_string, new_buffer, Encoding, MAX_LENGTH};
use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};
use crate::stream::state::{StreamState, StreamStateImpl};
use crate::support::{next_tick_call, Props};

/// `zlib.constants`, as Node.js v24 defines them.
const CONSTANTS: [(&str, f64); 170] = [
    ("Z_NO_FLUSH", 0.0),
    ("Z_PARTIAL_FLUSH", 1.0),
    ("Z_SYNC_FLUSH", 2.0),
    ("Z_FULL_FLUSH", 3.0),
    ("Z_FINISH", 4.0),
    ("Z_BLOCK", 5.0),
    ("Z_OK", 0.0),
    ("Z_STREAM_END", 1.0),
    ("Z_NEED_DICT", 2.0),
    ("Z_ERRNO", -1.0),
    ("Z_STREAM_ERROR", -2.0),
    ("Z_DATA_ERROR", -3.0),
    ("Z_MEM_ERROR", -4.0),
    ("Z_BUF_ERROR", -5.0),
    ("Z_VERSION_ERROR", -6.0),
    ("Z_NO_COMPRESSION", 0.0),
    ("Z_BEST_SPEED", 1.0),
    ("Z_BEST_COMPRESSION", 9.0),
    ("Z_DEFAULT_COMPRESSION", -1.0),
    ("Z_FILTERED", 1.0),
    ("Z_HUFFMAN_ONLY", 2.0),
    ("Z_RLE", 3.0),
    ("Z_FIXED", 4.0),
    ("Z_DEFAULT_STRATEGY", 0.0),
    ("ZLIB_VERNUM", 4800.0),
    ("DEFLATE", 1.0),
    ("INFLATE", 2.0),
    ("GZIP", 3.0),
    ("GUNZIP", 4.0),
    ("DEFLATERAW", 5.0),
    ("INFLATERAW", 6.0),
    ("UNZIP", 7.0),
    ("BROTLI_DECODE", 8.0),
    ("BROTLI_ENCODE", 9.0),
    ("ZSTD_DECOMPRESS", 11.0),
    ("ZSTD_COMPRESS", 10.0),
    ("Z_MIN_WINDOWBITS", 8.0),
    ("Z_MAX_WINDOWBITS", 15.0),
    ("Z_DEFAULT_WINDOWBITS", 15.0),
    ("Z_MIN_CHUNK", 64.0),
    ("Z_MAX_CHUNK", f64::INFINITY),
    ("Z_DEFAULT_CHUNK", 16384.0),
    ("Z_MIN_MEMLEVEL", 1.0),
    ("Z_MAX_MEMLEVEL", 9.0),
    ("Z_DEFAULT_MEMLEVEL", 8.0),
    ("Z_MIN_LEVEL", -1.0),
    ("Z_MAX_LEVEL", 9.0),
    ("Z_DEFAULT_LEVEL", -1.0),
    ("BROTLI_OPERATION_PROCESS", 0.0),
    ("BROTLI_OPERATION_FLUSH", 1.0),
    ("BROTLI_OPERATION_FINISH", 2.0),
    ("BROTLI_OPERATION_EMIT_METADATA", 3.0),
    ("BROTLI_PARAM_MODE", 0.0),
    ("BROTLI_MODE_GENERIC", 0.0),
    ("BROTLI_MODE_TEXT", 1.0),
    ("BROTLI_MODE_FONT", 2.0),
    ("BROTLI_DEFAULT_MODE", 0.0),
    ("BROTLI_PARAM_QUALITY", 1.0),
    ("BROTLI_MIN_QUALITY", 0.0),
    ("BROTLI_MAX_QUALITY", 11.0),
    ("BROTLI_DEFAULT_QUALITY", 11.0),
    ("BROTLI_PARAM_LGWIN", 2.0),
    ("BROTLI_MIN_WINDOW_BITS", 10.0),
    ("BROTLI_MAX_WINDOW_BITS", 24.0),
    ("BROTLI_LARGE_MAX_WINDOW_BITS", 30.0),
    ("BROTLI_DEFAULT_WINDOW", 22.0),
    ("BROTLI_PARAM_LGBLOCK", 3.0),
    ("BROTLI_MIN_INPUT_BLOCK_BITS", 16.0),
    ("BROTLI_MAX_INPUT_BLOCK_BITS", 24.0),
    ("BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING", 4.0),
    ("BROTLI_PARAM_SIZE_HINT", 5.0),
    ("BROTLI_PARAM_LARGE_WINDOW", 6.0),
    ("BROTLI_PARAM_NPOSTFIX", 7.0),
    ("BROTLI_PARAM_NDIRECT", 8.0),
    ("BROTLI_DECODER_RESULT_ERROR", 0.0),
    ("BROTLI_DECODER_RESULT_SUCCESS", 1.0),
    ("BROTLI_DECODER_RESULT_NEEDS_MORE_INPUT", 2.0),
    ("BROTLI_DECODER_RESULT_NEEDS_MORE_OUTPUT", 3.0),
    ("BROTLI_DECODER_PARAM_DISABLE_RING_BUFFER_REALLOCATION", 0.0),
    ("BROTLI_DECODER_PARAM_LARGE_WINDOW", 1.0),
    ("BROTLI_DECODER_NO_ERROR", 0.0),
    ("BROTLI_DECODER_SUCCESS", 1.0),
    ("BROTLI_DECODER_NEEDS_MORE_INPUT", 2.0),
    ("BROTLI_DECODER_NEEDS_MORE_OUTPUT", 3.0),
    ("BROTLI_DECODER_ERROR_FORMAT_EXUBERANT_NIBBLE", -1.0),
    ("BROTLI_DECODER_ERROR_FORMAT_RESERVED", -2.0),
    ("BROTLI_DECODER_ERROR_FORMAT_EXUBERANT_META_NIBBLE", -3.0),
    ("BROTLI_DECODER_ERROR_FORMAT_SIMPLE_HUFFMAN_ALPHABET", -4.0),
    ("BROTLI_DECODER_ERROR_FORMAT_SIMPLE_HUFFMAN_SAME", -5.0),
    ("BROTLI_DECODER_ERROR_FORMAT_CL_SPACE", -6.0),
    ("BROTLI_DECODER_ERROR_FORMAT_HUFFMAN_SPACE", -7.0),
    ("BROTLI_DECODER_ERROR_FORMAT_CONTEXT_MAP_REPEAT", -8.0),
    ("BROTLI_DECODER_ERROR_FORMAT_BLOCK_LENGTH_1", -9.0),
    ("BROTLI_DECODER_ERROR_FORMAT_BLOCK_LENGTH_2", -10.0),
    ("BROTLI_DECODER_ERROR_FORMAT_TRANSFORM", -11.0),
    ("BROTLI_DECODER_ERROR_FORMAT_DICTIONARY", -12.0),
    ("BROTLI_DECODER_ERROR_FORMAT_WINDOW_BITS", -13.0),
    ("BROTLI_DECODER_ERROR_FORMAT_PADDING_1", -14.0),
    ("BROTLI_DECODER_ERROR_FORMAT_PADDING_2", -15.0),
    ("BROTLI_DECODER_ERROR_FORMAT_DISTANCE", -16.0),
    ("BROTLI_DECODER_ERROR_DICTIONARY_NOT_SET", -19.0),
    ("BROTLI_DECODER_ERROR_INVALID_ARGUMENTS", -20.0),
    ("BROTLI_DECODER_ERROR_ALLOC_CONTEXT_MODES", -21.0),
    ("BROTLI_DECODER_ERROR_ALLOC_TREE_GROUPS", -22.0),
    ("BROTLI_DECODER_ERROR_ALLOC_CONTEXT_MAP", -25.0),
    ("BROTLI_DECODER_ERROR_ALLOC_RING_BUFFER_1", -26.0),
    ("BROTLI_DECODER_ERROR_ALLOC_RING_BUFFER_2", -27.0),
    ("BROTLI_DECODER_ERROR_ALLOC_BLOCK_TYPE_TREES", -30.0),
    ("BROTLI_DECODER_ERROR_UNREACHABLE", -31.0),
    ("ZSTD_e_continue", 0.0),
    ("ZSTD_e_flush", 1.0),
    ("ZSTD_e_end", 2.0),
    ("ZSTD_fast", 1.0),
    ("ZSTD_dfast", 2.0),
    ("ZSTD_greedy", 3.0),
    ("ZSTD_lazy", 4.0),
    ("ZSTD_lazy2", 5.0),
    ("ZSTD_btlazy2", 6.0),
    ("ZSTD_btopt", 7.0),
    ("ZSTD_btultra", 8.0),
    ("ZSTD_btultra2", 9.0),
    ("ZSTD_c_compressionLevel", 100.0),
    ("ZSTD_c_windowLog", 101.0),
    ("ZSTD_c_hashLog", 102.0),
    ("ZSTD_c_chainLog", 103.0),
    ("ZSTD_c_searchLog", 104.0),
    ("ZSTD_c_minMatch", 105.0),
    ("ZSTD_c_targetLength", 106.0),
    ("ZSTD_c_strategy", 107.0),
    ("ZSTD_c_enableLongDistanceMatching", 160.0),
    ("ZSTD_c_ldmHashLog", 161.0),
    ("ZSTD_c_ldmMinMatch", 162.0),
    ("ZSTD_c_ldmBucketSizeLog", 163.0),
    ("ZSTD_c_ldmHashRateLog", 164.0),
    ("ZSTD_c_contentSizeFlag", 200.0),
    ("ZSTD_c_checksumFlag", 201.0),
    ("ZSTD_c_dictIDFlag", 202.0),
    ("ZSTD_c_nbWorkers", 400.0),
    ("ZSTD_c_jobSize", 401.0),
    ("ZSTD_c_overlapLog", 402.0),
    ("ZSTD_d_windowLogMax", 100.0),
    ("ZSTD_CLEVEL_DEFAULT", 3.0),
    ("ZSTD_error_no_error", 0.0),
    ("ZSTD_error_GENERIC", 1.0),
    ("ZSTD_error_prefix_unknown", 10.0),
    ("ZSTD_error_version_unsupported", 12.0),
    ("ZSTD_error_frameParameter_unsupported", 14.0),
    ("ZSTD_error_frameParameter_windowTooLarge", 16.0),
    ("ZSTD_error_corruption_detected", 20.0),
    ("ZSTD_error_checksum_wrong", 22.0),
    ("ZSTD_error_literals_headerWrong", 24.0),
    ("ZSTD_error_dictionary_corrupted", 30.0),
    ("ZSTD_error_dictionary_wrong", 32.0),
    ("ZSTD_error_dictionaryCreation_failed", 34.0),
    ("ZSTD_error_parameter_unsupported", 40.0),
    ("ZSTD_error_parameter_combination_unsupported", 41.0),
    ("ZSTD_error_parameter_outOfBound", 42.0),
    ("ZSTD_error_tableLog_tooLarge", 44.0),
    ("ZSTD_error_maxSymbolValue_tooLarge", 46.0),
    ("ZSTD_error_maxSymbolValue_tooSmall", 48.0),
    ("ZSTD_error_stabilityCondition_notRespected", 50.0),
    ("ZSTD_error_stage_wrong", 60.0),
    ("ZSTD_error_init_missing", 62.0),
    ("ZSTD_error_memory_allocation", 64.0),
    ("ZSTD_error_workSpace_tooSmall", 66.0),
    ("ZSTD_error_dstSize_tooSmall", 70.0),
    ("ZSTD_error_srcSize_wrong", 72.0),
    ("ZSTD_error_dstBuffer_null", 74.0),
    ("ZSTD_error_noForwardProgress_destFull", 80.0),
    ("ZSTD_error_noForwardProgress_inputEmpty", 82.0),
];

/// The zlib return codes, by name.
const CODES: [(&str, i32); 9] = [
    ("Z_OK", 0),
    ("Z_STREAM_END", 1),
    ("Z_NEED_DICT", 2),
    ("Z_ERRNO", -1),
    ("Z_STREAM_ERROR", -2),
    ("Z_DATA_ERROR", -3),
    ("Z_MEM_ERROR", -4),
    ("Z_BUF_ERROR", -5),
    ("Z_VERSION_ERROR", -6),
];

/// Whether `code` names `Z_NEED_DICT` or a negative constant of [`CONSTANTS`], the zlib and Brotli
/// failures.
pub(crate) fn is_failure_code(code: &str) -> bool {
    CONSTANTS
        .iter()
        .any(|&(name, value)| name == code && (value < 0.0 || name == "Z_NEED_DICT"))
}

/// The formats that share flush constants and options.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Zlib,
    Brotli,
    Zstd,
}

/// The Node.js properties of a [`Format`].
trait FormatExt {
    fn family(self) -> Family;

    /// Node.js's `_mode` of a zlib stream of this format, its `binding` mode constant.
    fn mode(self) -> f64;
}

impl FormatExt for Format {
    fn family(self) -> Family {
        match self {
            Format::BrotliCompress | Format::BrotliDecompress => Family::Brotli,
            Format::ZstdCompress | Format::ZstdDecompress => Family::Zstd,
            _ => Family::Zlib,
        }
    }

    fn mode(self) -> f64 {
        match self {
            Format::Deflate => 1.0,
            Format::Inflate => 2.0,
            Format::Gzip => 3.0,
            Format::Gunzip => 4.0,
            Format::DeflateRaw => 5.0,
            Format::InflateRaw => 6.0,
            Format::Unzip => 7.0,
            Format::BrotliDecompress => 8.0,
            Format::BrotliCompress => 9.0,
            Format::ZstdCompress => 10.0,
            Format::ZstdDecompress => 11.0,
        }
    }
}

/// The JS errors of a [`ZlibError`].
trait ZlibErrorExt {
    /// The error as a JS `Error` with `code` and `errno`.
    fn into_value<'s>(self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown>;

    fn throw(self, scope: &Scope<'_>) -> ExnThrown;
}

impl ZlibErrorExt for ZlibError {
    fn into_value<'s>(self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
        if self.code.is_empty() {
            let kind = js::error::JSExnType::JSEXN_RANGEERR;
            return js::error::new_error(scope, kind, &self.message);
        }
        let kind = match self.errno {
            Some(_) => ErrorKind::Error,
            None => ErrorKind::RangeError,
        };
        let error = NodeError::new(kind, self.code, self.message).to_value(scope)?;
        if let Some(errno) = self.errno {
            Object::from_value(scope, *error)
                .map_err(|e| e.throw(scope))?
                .set_property(scope, c"errno", errno)?;
        }
        Ok(error)
    }

    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        if let Ok(error) = self.into_value(scope) {
            js::exception::set_pending(scope, error, js::native::ExceptionStackBehavior::Capture);
        }
        ExnThrown
    }
}

impl Family {
    /// The flush constants Node.js's `flush`, `finishFlush` and `flush()` take: the lowest, the
    /// highest, the default flush, the default finishing flush, and the flush `flush()` defaults to.
    fn flush_constants(self) -> (f64, f64, f64, f64, f64) {
        match self {
            // `Z_NO_FLUSH` to `Z_BLOCK`, `Z_FINISH` and `Z_FULL_FLUSH`.
            Family::Zlib => (0.0, 5.0, 0.0, 4.0, 3.0),
            // `BROTLI_OPERATION_PROCESS` to `BROTLI_OPERATION_EMIT_METADATA`,
            // `BROTLI_OPERATION_FINISH` and `BROTLI_OPERATION_FLUSH`.
            Family::Brotli => (0.0, 3.0, 0.0, 2.0, 1.0),
            // `ZSTD_e_continue` to `ZSTD_e_end`, `ZSTD_e_end` and `ZSTD_e_flush`.
            Family::Zstd => (0.0, 2.0, 0.0, 2.0, 1.0),
        }
    }

    /// The flush for the flush constant `kind` of this family. For zlib, `Z_NO_FLUSH` is `None`,
    /// `Z_PARTIAL_FLUSH`, `Z_SYNC_FLUSH` and `Z_BLOCK` are `Sync`, and the others are `Full`. For
    /// Brotli and zstd, 0 is `None`, and the others are `Sync`.
    fn flush(self, kind: f64) -> Flush {
        match (self, kind as i32) {
            (_, 0) => Flush::None,
            (Family::Zlib, 1 | 2 | 5) => Flush::Sync,
            (Family::Zlib, _) => Flush::Full,
            _ => Flush::Sync,
        }
    }

    /// Node.js's `_flushBoundIdx`: the index of this family's flush bounds.
    fn bound_index(self) -> f64 {
        match self {
            Family::Zlib => 0.0,
            Family::Brotli => 1.0,
            Family::Zstd => 2.0,
        }
    }

    /// The flush for the flush constant `kind`, where the family's default finishing flush
    /// constant ends the stream.
    fn finish_flush(self, kind: f64) -> Flush {
        if kind == self.flush_constants().3 {
            Flush::Finish
        } else {
            self.flush(kind)
        }
    }
}

/// The flush constant of `a` and `b` that flushes more, as Node.js's `maxFlush` compares them:
/// by the rank of the zlib constants `Z_NO_FLUSH`, `Z_BLOCK`, `Z_PARTIAL_FLUSH`, `Z_SYNC_FLUSH`,
/// `Z_FULL_FLUSH` and `Z_FINISH`, in this order, for every family.
fn max_flush(a: f64, b: f64) -> f64 {
    let rank = |kind: f64| {
        [0.0, 5.0, 1.0, 2.0, 3.0, 4.0]
            .iter()
            .position(|&flag| flag == kind)
    };
    if rank(a) > rank(b) {
        a
    } else {
        b
    }
}

/// The bytes of `data`: a string as UTF-8, or the bytes of a `Buffer`, `TypedArray`, `DataView`
/// or `ArrayBuffer`.
fn input_bytes(scope: &Scope<'_>, data: HandleValue<'_>) -> Result<Vec<u8>, Thrown> {
    if data.is_string() {
        let string = js::JSString::from_value(scope, data)?;
        return Ok(encode_string(scope, string, Encoding::Utf8)?);
    }
    let bytes = match Object::from_value(scope, *data) {
        Ok(object) => js::typedarray::copy_buffer_source_bytes(scope, object)?,
        Err(_) => None,
    };
    bytes.ok_or_else(|| {
        NodeError::invalid_arg_type(
            scope,
            "buffer",
            "of type string or an instance of Buffer, TypedArray, DataView, or ArrayBuffer",
            data,
        )
        .into()
    })
}

/// Node.js's `checkRangesOrGetDefault`: the number `value`, or `default` if it is `undefined` or
/// `NaN`. Other values are an error unless they are a finite number from `min` to `max`.
fn range_or_default(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    min: f64,
    max: f64,
    default: f64,
) -> Result<f64, Thrown> {
    if value.is_undefined() || (value.is_number() && value.to_number().is_nan()) {
        return Ok(default);
    }
    if !value.is_number() {
        return Err(NodeError::invalid_arg_type(scope, name, "of type number", value).into());
    }
    let number = value.to_number();
    if number.is_infinite() {
        return Err(NodeError::out_of_range(name, "a finite number", &number.to_string()).into());
    }
    if number < min || number > max {
        let range = format!(">= {min} and <= {max}");
        return Err(NodeError::out_of_range(name, &range, &number.to_string()).into());
    }
    Ok(number)
}

/// The options of a zlib stream or conversion that the codec uses.
struct ZlibOptions {
    params: Params,
    /// The flush constant each written chunk is processed with, `options.flush`.
    flush: f64,
    chunk_size: f64,
    /// The zlib strategy, `options.strategy`.
    strategy: f64,
    /// The largest output a synchronous conversion produces.
    max_output_length: f64,
    /// The flush constant a conversion ends with, `Z_FINISH`, `BROTLI_OPERATION_FINISH` or
    /// `ZSTD_e_end` by default.
    finish_flush: f64,
}

/// The `ZSTD_c_*` parameters.
const ZSTD_COMPRESS_PARAMS: [u32; 19] = [
    100, 101, 102, 103, 104, 105, 106, 107, 160, 161, 162, 163, 164, 200, 201, 202, 400, 401, 402,
];

/// The keys and values of `options.params`, validated as Node.js's `Brotli` and `Zstd`
/// constructors validate them: a key must be a number from 0 to `max_key`, which is
/// `invalid_code`, an error with the message `invalid_message` after the key, otherwise, and a
/// value must be a number or a boolean.
fn codec_params(
    scope: &Scope<'_>,
    options: Object<'_>,
    max_key: f64,
    invalid_code: &'static str,
    invalid_message: &str,
) -> Result<Vec<(u32, u32)>, Thrown> {
    let params = options.get_property(scope, c"params")?;
    let Some(params) = Some(params)
        .filter(|params| bool::from_jsval(scope, *params, ()).unwrap_or(false))
        .and_then(|params| Object::from_value(scope, *params).ok())
    else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for key in params.own_enumerable_keys(scope)? {
        let name = String::from_jsval(scope, key, ())?;
        let number = if name.trim().is_empty() {
            0.0
        } else {
            name.trim().parse::<f64>().unwrap_or(f64::NAN)
        };
        if number.is_nan() || !(0.0..=max_key).contains(&number) || number.fract() != 0.0 {
            let message = format!("{name} {invalid_message}");
            return Err(NodeError::new(ErrorKind::RangeError, invalid_code, message).into());
        }
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        let value = params.get_property_by_id(scope, id)?;
        let value = if value.is_boolean() {
            u32::from(value.to_boolean())
        } else if value.is_number() {
            value.to_number() as u32
        } else {
            let name = "options.params[key]";
            return Err(NodeError::invalid_arg_type(scope, name, "of type number", value).into());
        };
        result.push((number as u32, value));
    }
    Ok(result)
}

/// The options of `format` in `options`, validated as Node.js's `Zlib`, `Brotli`, `Zstd` and
/// `ZlibBase` constructors validate them: `windowBits`, `level`, `memLevel`, `strategy` and
/// `dictionary` for zlib, `params` for Brotli and zstd, and `chunkSize`, `flush`, `finishFlush`
/// and `maxOutputLength`. The `dictionary` of Brotli and zstd and zstd's `pledgedSrcSize` are
/// ignored.
fn zlib_options(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
    format: Format,
) -> Result<ZlibOptions, Thrown> {
    let family = format.family();
    let (min_flush, max_flush, default_flush, default_finish, _) = family.flush_constants();
    let mut result = ZlibOptions {
        params: Params::default(),
        flush: default_flush,
        chunk_size: 16384.0,
        strategy: 0.0,
        max_output_length: MAX_LENGTH,
        finish_flush: default_finish,
    };
    let Some(options) = options
        .filter(|options| bool::from_jsval(scope, *options, ()).unwrap_or(false))
        .and_then(|options| Object::from_value(scope, *options).ok())
    else {
        return Ok(result);
    };
    match family {
        Family::Zlib => {
            (result.params.level, result.strategy) = zlib_format_options(scope, options, format)?
        }
        Family::Brotli => {
            let message = "is not a valid Brotli parameter";
            result.params.brotli =
                codec_params(scope, options, 8.0, "ERR_BROTLI_INVALID_PARAM", message)?;
        }
        Family::Zstd => {
            let compress = format == Format::ZstdCompress;
            let max_key = if compress { 402.0 } else { 100.0 };
            let message = "is not a valid zstd parameter";
            let params = codec_params(scope, options, max_key, "ERR_ZSTD_INVALID_PARAM", message)?;
            let known = |key: u32| {
                if compress {
                    ZSTD_COMPRESS_PARAMS.contains(&key)
                } else {
                    key == 100
                }
            };
            // `ZSTD_c_windowLog` and `ZSTD_d_windowLogMax` take 0 for the default, or 10 to 31.
            let valid_window = |key: u32, value: u32| {
                key != if compress { 101 } else { 100 } || value == 0 || (10..=31).contains(&value)
            };
            if params
                .iter()
                .any(|&(key, value)| !known(key) || !valid_window(key, value))
            {
                let code = "ERR_ZLIB_INITIALIZATION_FAILED";
                return Err(
                    NodeError::new(ErrorKind::Error, code, "Setting parameter failed").into(),
                );
            }
        }
    }
    let chunk_size = options.get_property(scope, c"chunkSize")?;
    let chunk_size = range_or_default(
        scope,
        chunk_size,
        "options.chunkSize",
        f64::MIN,
        f64::MAX,
        16384.0,
    )?;
    if chunk_size < 64.0 {
        return Err(
            NodeError::out_of_range("options.chunkSize", ">= 64", &chunk_size.to_string()).into(),
        );
    }
    result.chunk_size = chunk_size;
    let flush = options.get_property(scope, c"flush")?;
    result.flush = range_or_default(
        scope,
        flush,
        "options.flush",
        min_flush,
        max_flush,
        default_flush,
    )?;
    let finish_flush = options.get_property(scope, c"finishFlush")?;
    result.finish_flush = range_or_default(
        scope,
        finish_flush,
        "options.finishFlush",
        min_flush,
        max_flush,
        default_finish,
    )?;
    let max = options.get_property(scope, c"maxOutputLength")?;
    result.max_output_length = range_or_default(
        scope,
        max,
        "options.maxOutputLength",
        1.0,
        MAX_LENGTH,
        MAX_LENGTH,
    )?;
    Ok(result)
}

/// The zlib options of `format` in `options`, validated as Node.js's `Zlib` constructor
/// validates them: `windowBits`, `level`, `memLevel`, `strategy` and `dictionary`. Returns the
/// level and the strategy.
fn zlib_format_options(
    scope: &Scope<'_>,
    options: Object<'_>,
    format: Format,
) -> Result<(i32, f64), Thrown> {
    let window_bits = options.get_property(scope, c"windowBits")?;
    let decompressing = matches!(format, Format::Inflate | Format::Gunzip | Format::Unzip);
    let unset = window_bits.is_null_or_undefined()
        || (window_bits.is_number() && window_bits.to_number() == 0.0);
    if !(unset && decompressing) {
        let min = if format == Format::Gzip { 9.0 } else { 8.0 };
        range_or_default(scope, window_bits, "options.windowBits", min, 15.0, 15.0)?;
    }
    let level = options.get_property(scope, c"level")?;
    let level = range_or_default(scope, level, "options.level", -1.0, 9.0, -1.0)? as i32;
    let mem_level = options.get_property(scope, c"memLevel")?;
    range_or_default(scope, mem_level, "options.memLevel", 1.0, 9.0, 8.0)?;
    let strategy = options.get_property(scope, c"strategy")?;
    let strategy = range_or_default(scope, strategy, "options.strategy", 0.0, 4.0, 0.0)?;
    let dictionary = options.get_property(scope, c"dictionary")?;
    let is_buffer_source =
        Object::from_value(scope, dictionary).is_ok_and(js::typedarray::is_buffer_source);
    if !dictionary.is_undefined() && !is_buffer_source {
        let expected = "an instance of Buffer, TypedArray, DataView, or ArrayBuffer";
        return Err(
            NodeError::invalid_arg_type(scope, "options.dictionary", expected, dictionary).into(),
        );
    }
    Ok((level, strategy))
}

/// Compress or decompress all of `data` in `format`, in steps of `chunkSize` bytes of output, as
/// Node.js's `processChunkSync` does. The conversion fails with `ERR_BUFFER_TOO_LARGE` after the
/// first step that takes the output past `maxOutputLength`. A codec error in that step takes
/// precedence.
fn convert<'s>(
    scope: &'s Scope<'_>,
    format: Format,
    data: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Result<Object<'s>, ZlibError>, Thrown> {
    let options = zlib_options(scope, options, format)?;
    let bytes = input_bytes(scope, data)?;
    let mut codec = Codec::new(format, &options.params);
    let flush = format.family().finish_flush(options.finish_flush);
    let max = options.max_output_length as usize;
    let mut output = Vec::new();
    let mut offset = 0;
    loop {
        let step = codec.process(
            &bytes[offset..],
            flush,
            options.chunk_size as usize,
            &mut output,
        );
        let progress = match step {
            Ok(progress) => progress,
            Err(error) => return Ok(Err(error)),
        };
        offset += progress.consumed;
        if output.len() > max {
            let message = format!("Cannot create a Buffer larger than {max} bytes");
            return Ok(Err(ZlibError::too_large(message)));
        }
        if progress.done {
            return Ok(Ok(new_buffer(scope, &output)?));
        }
    }
}

/// The body of the synchronous functions, such as `deflateSync`.
fn convert_sync<'s>(
    scope: &'s Scope<'_>,
    format: Format,
    data: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    convert(scope, format, data, options)?.map_err(|error| error.throw(scope).into())
}

/// The body of the callback functions, such as `deflate`: compress or decompress `data` in
/// `format`, and call the callback, the last of `rest`, with the error or the result in a
/// microtask. `rest` may start with options.
fn convert_async(
    scope: &Scope<'_>,
    format: Format,
    data: HandleValue<'_>,
    rest: &[HandleValue<'_>],
) -> Result<(), Thrown> {
    let undefined = scope.root_value(js::value::undefined());
    let (options, callback) = match rest {
        [] => (None, undefined),
        [callback] => (None, *callback),
        [options, callback, ..] => (Some(*options), *callback),
    };
    validate_function(scope, callback, "callback")?;
    match convert(scope, format, data, options)? {
        Ok(output) => {
            let null = scope.root_value(js::value::null());
            next_tick_call(scope, callback, &[null, output.to_jsval_throwing(scope)?])?
        }
        Err(error) => next_tick_call(scope, callback, &[error.into_value(scope)?])?,
    }
    Ok(())
}

/// The state of a zlib stream: the state of the stream, the codec, the flush constants written
/// chunks and the last chunk are processed with, and the count of bytes written, which
/// `bytesWritten` mirrors.
#[core_runtime::jsclass(hidden, extends = StreamState)]
pub(crate) struct ZlibState {
    parent: StreamStateImpl,
    #[no_trace]
    codec: Codec,
    #[no_trace]
    default_flush: f64,
    #[no_trace]
    finish_flush: f64,
    #[no_trace]
    bytes_written: f64,
    /// Whether `_destroy` freed the codec, which `_handle` being `null` mirrors.
    #[no_trace]
    closed: bool,
    /// The level and strategy of a zlib format stream, which `_level` and `_strategy` mirror, as
    /// `params` last set them. `None` is `undefined`.
    #[no_trace]
    level: Option<f64>,
    #[no_trace]
    strategy: Option<f64>,
    /// The largest buffer the stream pushes, `options.chunkSize`.
    #[no_trace]
    chunk_size: usize,
    /// The count of bytes pushed of the current output buffer, Node's `_outOffset`.
    #[no_trace]
    out_offset: usize,
    /// The input of the write that waits for the readable side to be read, and the offset of the
    /// bytes not processed yet.
    #[no_trace]
    paused_input: Vec<u8>,
    #[no_trace]
    paused_offset: usize,
    /// The flush of the waiting write.
    #[no_trace]
    paused_flush: Flush,
    /// The callback of the waiting write, or `None` if no write waits.
    paused_callback: Option<Heap<Value>>,
}

#[core_runtime::jsmethods]
impl ZlibState {}

/// Whether `stream` is a zlib stream whose codec `_destroy` freed.
fn is_closed(scope: &Scope<'_>, stream: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(zlib_class::try_state_of(scope, stream)?.is_some_and(|state| state.data().closed))
}

/// The state of the zlib stream `stream`, or `ERR_INVALID_THIS` if it isn't one or its codec was
/// freed.
fn open_state<'s>(scope: &'s Scope<'_>, stream: Object<'s>) -> Result<ZlibState<'s>, Thrown> {
    zlib_class::try_state_of(scope, stream)?
        .filter(|state| !state.data().closed)
        .ok_or_else(|| {
            NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_THIS",
                "Value of \"this\" must be a zlib stream",
            )
            .into()
        })
}

/// Process `input` from `offset` with the codec of `stream` and `flush`, push the output in
/// buffers of at most `chunkSize` bytes, and add the bytes processed to `bytesWritten`, as Node.js's
/// `processCallback` does. Once all output is pushed, `callback` is called without arguments. It
/// is called with the error if the codec fails, and without arguments as soon as the stream is
/// destroyed. When a push fills the readable side and output remains, the write waits until the
/// readable side is read, and [`resume_write`] continues it.
fn process_write(
    scope: &Scope<'_>,
    stream: Object<'_>,
    input: Vec<u8>,
    mut offset: usize,
    flush: Flush,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let undefined = HandleValue::undefined();
    loop {
        let Some(state) = zlib_class::try_state_of(scope, stream)? else {
            return Ok(());
        };
        if state.data().closed {
            js::Function::call(scope, undefined, callback, js::function::EmptyArgs)?;
            return Ok(());
        }
        let mut output = Vec::new();
        let (result, written) = {
            let mut data = state.data_mut();
            let data = &mut *data;
            let limit = data.chunk_size - data.out_offset;
            let result = data
                .codec
                .process(&input[offset..], flush, limit, &mut output);
            if let Ok(progress) = result {
                data.bytes_written += progress.consumed as f64;
                data.out_offset += output.len();
                if !progress.done || data.out_offset >= data.chunk_size {
                    data.out_offset = 0;
                }
            }
            (result, data.bytes_written)
        };
        let progress = match result {
            Ok(progress) => progress,
            Err(error) => {
                let error = error.into_value(scope)?;
                js::Function::call(scope, undefined, callback, &[error])?;
                return Ok(());
            }
        };
        offset += progress.consumed;
        stream.set_property(scope, c"bytesWritten", written)?;
        let mut full = false;
        if !output.is_empty() {
            let buffer = new_buffer(scope, &output)?.to_jsval_throwing(scope)?;
            let pushed = js::Function::call_by_name(scope, stream.handle(), c"push", &[buffer])?;
            full = !js::conversion::to_boolean(pushed);
        }
        if progress.done || is_closed(scope, stream)? {
            js::Function::call(scope, undefined, callback, js::function::EmptyArgs)?;
            return Ok(());
        }
        if full {
            {
                let mut data = state.data_mut();
                data.paused_input = input;
                data.paused_offset = offset;
                data.paused_flush = flush;
                data.paused_callback = Some(Heap::from(callback.get()));
            }
            // `Transform.prototype._read` calls the stored callback once the readable side is
            // read.
            let resume = js::Function::new_callback(scope, c"", 0, resume_write, stream)?;
            let resume = (*resume).to_jsval_throwing(scope)?;
            if let Some(stream_state) = crate::stream::state::try_state(scope, stream)? {
                stream_state.data_mut().transform_callback = Some(Heap::from(resume.get()));
            }
            return Ok(());
        }
    }
}

/// The callback `_read` calls for a write that waits, whose payload is the stream: continue the
/// write in a tick.
fn resume_write(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    js::jobs::queue_native_tick(scope, continue_write, payload, &[])?;
    Ok(js::value::undefined())
}

/// Continue the write of the stream `payload` that waits, if one still does.
fn continue_write(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let Some(state) = zlib_class::try_state_of(scope, stream)? else {
        return Ok(js::value::undefined());
    };
    let (input, offset, flush, callback) = {
        let mut data = state.data_mut();
        let callback = data
            .paused_callback
            .as_ref()
            .map(|callback| callback.get(scope));
        data.paused_callback = None;
        (
            std::mem::take(&mut data.paused_input),
            data.paused_offset,
            data.paused_flush,
            callback,
        )
    };
    if let Some(callback) = callback {
        process_write(scope, stream, input, offset, flush, callback)?;
    }
    Ok(js::value::undefined())
}

/// Call the stream callback `callback` with the error of `result`, or with no arguments if it
/// succeeded.
fn complete(
    scope: &Scope<'_>,
    callback: HandleValue<'_>,
    result: Result<(), Thrown>,
) -> Result<(), ExnThrown> {
    let undefined = scope.root_value(js::value::undefined());
    let error = match result {
        Ok(()) => None,
        Err(thrown) => {
            thrown.throw(scope);
            Some(js::exception::take_pending_or_undefined(scope))
        }
    };
    js::Function::call(scope, undefined, callback, error.as_slice())?;
    Ok(())
}

/// The key of the property that marks the empty chunks [`flush_stream`] writes, whose value is
/// the zlib flush constant.
fn flush_flag<'s>(scope: &'s Scope<'_>) -> Result<HandleId<'s>, ExnThrown> {
    static FLUSH_FLAG: js::symbol::CachedSymbol = js::symbol::CachedSymbol::new(c"kFlushFlag");
    FLUSH_FLAG.id(scope)
}

/// The flush the chunk `chunk` written to `stream` is processed with, as Node.js's `_transform`
/// chooses it: the flush constant of the chunk's flush flag, or the stream's `flush` option, and
/// at least the stream's `finishFlush` for the last chunk written before `end()`.
fn chunk_flush(
    scope: &Scope<'_>,
    stream: Object<'_>,
    chunk: HandleValue<'_>,
    length: usize,
) -> Result<Flush, Thrown> {
    let (family, default_flush, finish_flush) = {
        let state = open_state(scope, stream)?;
        let data = state.data();
        (
            data.codec.format().family(),
            data.default_flush,
            data.finish_flush,
        )
    };
    let mut kind = default_flush;
    if let Ok(chunk) = Object::from_value(scope, *chunk) {
        let flag = chunk.get_property_by_id(scope, flush_flag(scope)?)?;
        if flag.is_number() {
            kind = flag.to_number();
        }
    }
    if stream.flag(scope, c"writableEnded")?
        && stream.num(scope, c"writableLength")? == length as f64
    {
        kind = max_flush(kind, finish_flush);
    }
    Ok(family.finish_flush(kind))
}

/// Flush `stream` with the zlib flush constant `kind` after the chunks written before, and then
/// call `callback`. After `end`, `callback` is called once the stream ended.
fn flush_stream(
    scope: &Scope<'_>,
    stream: Object<'_>,
    kind: f64,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if stream.flag(scope, c"writableFinished")? {
        next_tick_call(scope, callback, &[])?;
    } else if stream.flag(scope, c"writableEnded")? {
        if js::function::is_callable(callback) {
            js::Function::call_by_name(
                scope,
                stream.handle(),
                c"once",
                &[js::class::get_or_init_atom(scope, c"end")?, callback],
            )?;
        }
    } else {
        let marker = new_buffer(scope, &[])?;
        marker.set_property_by_id(scope, flush_flag(scope)?, kind)?;
        let marker = marker.to_jsval_throwing(scope)?;
        js::Function::call_by_name(scope, stream.handle(), c"write", &[marker, callback])?;
    }
    Ok(())
}

/// The payload of [`apply_params`]: the stream, the level and strategy `params` sets, and the
/// callback of `params`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ParamsRequest {
    stream: Heap<js::object::Object>,
    #[no_trace]
    level: Option<f64>,
    #[no_trace]
    strategy: Option<f64>,
    callback: Heap<Value>,
}

#[core_runtime::jsmethods]
impl ParamsRequest {}

/// The flush callback of `params`, whose payload is a [`ParamsRequest`]: set the codec's level,
/// with `undefined` and `NaN` as 0, then, unless the stream was destroyed, record the level and
/// strategy and call the callback, as Node.js's `paramsAfterFlushCallback` does.
fn apply_params(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = cast_payload::<ParamsRequest>(scope, payload);
    let stream = request.data().stream.get(scope);
    let (level, strategy) = (request.data().level, request.data().strategy);
    let callback = request.data().callback.get(scope);
    let state = open_state(scope, stream).map_err(|thrown| thrown.throw(scope))?;
    state
        .data_mut()
        .codec
        .set_level(level.unwrap_or(0.0) as i32);
    if stream.flag(scope, c"destroyed")? {
        return Ok(js::value::undefined());
    }
    {
        let mut data = state.data_mut();
        data.level = level;
        data.strategy = strategy;
    }
    let number_or_undefined = |value: Option<f64>| match value {
        Some(value) => value.to_jsval_throwing(scope),
        None => Ok(HandleValue::undefined()),
    };
    stream.set_property(scope, c"_level", number_or_undefined(level)?)?;
    stream.set_property(scope, c"_strategy", number_or_undefined(strategy)?)?;
    if js::function::is_callable(callback) {
        js::Function::call(
            scope,
            js::value::undefined(),
            callback,
            js::function::EmptyArgs,
        )?;
    }
    Ok(js::value::undefined())
}

/// The body of the zlib stream constructors: a `Transform` with a codec for `format`.
fn init_stream(
    scope: &Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
    format: Format,
) -> Result<(), Thrown> {
    crate::stream::init_transform(scope, stream, options)?;
    let options = zlib_options(scope, options, format)?;
    let codec = Codec::new(format, &options.params);
    let family = format.family();
    let (_, _, _, _, full_flush) = family.flush_constants();
    {
        let state = zlib_class::state_of(scope, stream)?;
        let mut data = state.data_mut();
        data.codec = codec;
        data.default_flush = options.flush;
        data.finish_flush = options.finish_flush;
        data.bytes_written = 0.0;
        data.closed = false;
        if family == Family::Zlib {
            data.level = Some(f64::from(options.params.level));
            data.strategy = Some(options.strategy);
        }
        data.chunk_size = options.chunk_size as usize;
        data.out_offset = 0;
    }
    stream.set_property(scope, c"_handle", Object::new_plain(scope)?)?;
    stream.set_property(scope, c"_outOffset", 0)?;
    stream.set_property(scope, c"_chunkSize", options.chunk_size)?;
    stream.set_property(scope, c"_defaultFlushFlag", options.flush)?;
    stream.set_property(scope, c"_finishFlushFlag", options.finish_flush)?;
    stream.set_property(scope, c"_defaultFullFlushFlag", full_flush)?;
    stream.set_property(scope, c"_flushBoundIdx", family.bound_index())?;
    stream.set_property(scope, c"_info", js::value::undefined())?;
    stream.set_property(scope, c"_maxOutputLength", options.max_output_length)?;
    stream.set_property(scope, c"_rejectGarbageAfterEnd", false)?;
    if family == Family::Zlib {
        stream.set_property(scope, c"_level", f64::from(options.params.level))?;
        stream.set_property(scope, c"_strategy", options.strategy)?;
        stream.set_property(scope, c"_mode", format.mode())?;
    }
    stream.set_property(scope, c"bytesWritten", 0)?;
    Ok(())
}

/// The base of the zlib stream classes. It isn't exported.
#[core_runtime::jsfunction_class(
    name = "Zlib",
    extends = "crate::stream::transform",
    state = "ZlibState"
)]
pub mod zlib_class {
    use super::*;

    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_stream(scope, this, options, Format::Deflate)
    }

    #[method(name = "_transform")]
    pub fn transform_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        _encoding: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        if is_closed(scope, this)? {
            return next_tick_call(scope, callback, &[]);
        }
        let input = input_bytes(scope, chunk).and_then(|bytes| {
            let flush = chunk_flush(scope, this, chunk, bytes.len())?;
            Ok((bytes, flush))
        });
        match input {
            Ok((bytes, flush)) => process_write(scope, this, bytes, 0, flush, callback),
            Err(error) => complete(scope, callback, Err(error)),
        }
    }

    #[method(name = "_flush")]
    pub fn flush_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        if is_closed(scope, this)? {
            return next_tick_call(scope, callback, &[]);
        }
        let flush = open_state(scope, this).map(|state| {
            let data = state.data();
            let kind = max_flush(data.default_flush, data.finish_flush);
            data.codec.format().family().finish_flush(kind)
        });
        match flush {
            Ok(flush) => process_write(scope, this, Vec::new(), 0, flush, callback),
            Err(error) => complete(scope, callback, Err(error)),
        }
    }

    /// Node's `_destroy`: free the codec, set `_handle` to `null`, and call `callback` with
    /// `error`.
    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        if let Some(state) = zlib_class::try_state_of(scope, this)? {
            let mut data = state.data_mut();
            let params = data.codec.params().clone();
            data.codec = Codec::closed(data.codec.created(), params);
            data.closed = true;
            data.paused_input = Vec::new();
            data.paused_callback = None;
        }
        this.set_property(scope, c"_handle", js::value::null())?;
        js::Function::call(scope, js::value::undefined(), callback, &[error])?;
        Ok(())
    }

    /// <https://nodejs.org/api/zlib.html#zlibflushkind-callback>: `kind` defaults to
    /// `Z_FULL_FLUSH`, or `BROTLI_OPERATION_FLUSH` or `ZSTD_e_flush`.
    pub fn flush(
        scope: &Scope<'_>,
        this: Object<'_>,
        kind: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let (kind, callback) = match kind {
            Some(kind) if js::function::is_callable(kind) => (None, Some(kind)),
            _ => (kind, callback),
        };
        // A destroyed stream has no handle, and fails the write below.
        let family = if is_closed(scope, this)? {
            Family::Zlib
        } else {
            open_state(scope, this)?.data().codec.format().family()
        };
        let full_flush = family.flush_constants().4;
        let kind = kind
            .filter(|kind| kind.is_number())
            .map_or(full_flush, |kind| kind.to_number());
        let callback = callback.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        Ok(flush_stream(scope, this, kind, callback)?)
    }

    /// <https://nodejs.org/api/zlib.html#zlibparamslevel-strategy-callback>: set the level of a
    /// compressor after a `Z_SYNC_FLUSH` of the chunks written before. `level` and `strategy` are
    /// validated as Node.js validates them. If both equal the ones set last, nothing is flushed,
    /// and `callback` is called in a tick. The strategy has no effect on the codec.
    #[method(length = 3)]
    pub fn params(
        scope: &Scope<'_>,
        this: Object<'_>,
        level: HandleValue<'_>,
        strategy: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        range_or_default(scope, level, "level", -1.0, 9.0, 0.0)?;
        range_or_default(scope, strategy, "strategy", 0.0, 4.0, 0.0)?;
        let number = |value: HandleValue<'_>| (!value.is_undefined()).then(|| value.to_number());
        let (level, strategy) = (number(level), number(strategy));
        let callback = callback.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        let unchanged = {
            let state = zlib_class::state_of(scope, this)?;
            let data = state.data();
            data.level == level && data.strategy == strategy
        };
        if unchanged {
            validate_function(scope, callback, "callback")?;
            next_tick_call(scope, callback, &[])?;
            return Ok(());
        }
        let request =
            js::class::create_instance_with::<ParamsRequestImpl>(scope, |_| ParamsRequestImpl {
                stream: Heap::from(this),
                level,
                strategy,
                callback: Heap::from(callback.get()),
            })?;
        let done = js::Function::new_callback(scope, c"", 0, apply_params, request)?;
        Ok(flush_stream(
            scope,
            this,
            2.0,
            done.to_jsval_throwing(scope)?,
        )?)
    }

    /// <https://nodejs.org/api/zlib.html#zlibclosecallback>: `callback` is called once the
    /// stream finished, closed or failed, as `stream.finished` calls it.
    pub fn close(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if let Some(callback) = callback.filter(|callback| js::conversion::to_boolean(*callback)) {
            let stream = this.to_jsval_throwing(scope)?;
            crate::stream::end_of_stream(scope, stream, None, callback)?;
        }
        js::Function::call_by_name(scope, this.handle(), c"destroy", js::function::EmptyArgs)?;
        Ok(())
    }

    /// <https://nodejs.org/api/zlib.html#zlibreset>
    pub fn reset(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let state = open_state(scope, this)?;
        let mut data = state.data_mut();
        let params = data.codec.params().clone();
        data.codec = Codec::new(data.codec.created(), &params);
        Ok(())
    }
}

/// Define the zlib stream class `$name` for `$format` in the module `$module`.
macro_rules! zlib_stream_class {
    ($module:ident, $name:literal, $format:ident) => {
        #[core_runtime::jsfunction_class(name = $name, extends = "super::zlib_class", self_construct)]
        pub mod $module {
            use super::*;

            #[constructor(length = 1)]
            pub fn init(
                scope: &Scope<'_>,
                this: Object<'_>,
                options: Option<HandleValue<'_>>,
            ) -> Result<(), Thrown> {
                super::init_stream(scope, this, options, Format::$format)
            }
        }
    };
}

zlib_stream_class!(deflate_class, "Deflate", Deflate);
zlib_stream_class!(inflate_class, "Inflate", Inflate);
zlib_stream_class!(gzip_class, "Gzip", Gzip);
zlib_stream_class!(gunzip_class, "Gunzip", Gunzip);
zlib_stream_class!(deflate_raw_class, "DeflateRaw", DeflateRaw);
zlib_stream_class!(inflate_raw_class, "InflateRaw", InflateRaw);
zlib_stream_class!(unzip_class, "Unzip", Unzip);
zlib_stream_class!(brotli_compress_class, "BrotliCompress", BrotliCompress);
zlib_stream_class!(
    brotli_decompress_class,
    "BrotliDecompress",
    BrotliDecompress
);
zlib_stream_class!(zstd_compress_class, "ZstdCompress", ZstdCompress);
zlib_stream_class!(zstd_decompress_class, "ZstdDecompress", ZstdDecompress);

/// Create the zlib classes in the current realm.
pub(crate) fn register_classes(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    zlib_class::register(scope)?;
    deflate_class::register(scope)?;
    inflate_class::register(scope)?;
    gzip_class::register(scope)?;
    gunzip_class::register(scope)?;
    deflate_raw_class::register(scope)?;
    inflate_raw_class::register(scope)?;
    unzip_class::register(scope)?;
    brotli_compress_class::register(scope)?;
    brotli_decompress_class::register(scope)?;
    zstd_compress_class::register(scope)?;
    zstd_decompress_class::register(scope)?;
    Ok(())
}

/// A new instance of the class `class`, constructed with `options`.
fn create<'s>(
    scope: &'s Scope<'_>,
    class: Object<'s>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let options = scope.root_value(options.map_or(js::value::undefined(), |options| options.get()));
    js::Function::construct(scope, class, &[options])
}

#[core_runtime::jsmodule(node_args, name = "node:zlib", default_namespace)]
pub mod module {
    use super::*;

    pub use super::brotli_compress_class as BrotliCompress;
    pub use super::brotli_decompress_class as BrotliDecompress;
    pub use super::deflate_class as Deflate;
    pub use super::deflate_raw_class as DeflateRaw;
    pub use super::gunzip_class as Gunzip;
    pub use super::gzip_class as Gzip;
    pub use super::inflate_class as Inflate;
    pub use super::inflate_raw_class as InflateRaw;
    pub use super::unzip_class as Unzip;
    pub use super::zstd_compress_class as ZstdCompress;
    pub use super::zstd_decompress_class as ZstdDecompress;

    #[function(length = 2)]
    pub fn deflate_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Deflate, buffer, options)
    }

    #[function(length = 2)]
    pub fn inflate_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Inflate, buffer, options)
    }

    #[function(length = 2)]
    pub fn gzip_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Gzip, buffer, options)
    }

    #[function(length = 2)]
    pub fn gunzip_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Gunzip, buffer, options)
    }

    #[function(length = 2)]
    pub fn deflate_raw_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::DeflateRaw, buffer, options)
    }

    #[function(length = 2)]
    pub fn inflate_raw_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::InflateRaw, buffer, options)
    }

    #[function(length = 2)]
    pub fn unzip_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Unzip, buffer, options)
    }

    #[function(length = 2)]
    pub fn brotli_compress_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::BrotliCompress, buffer, options)
    }

    #[function(length = 2)]
    pub fn brotli_decompress_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::BrotliDecompress, buffer, options)
    }

    #[function(length = 2)]
    pub fn zstd_compress_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::ZstdCompress, buffer, options)
    }

    #[function(length = 2)]
    pub fn zstd_decompress_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::ZstdDecompress, buffer, options)
    }

    #[function(length = 3)]
    pub fn deflate(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Deflate, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn inflate(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Inflate, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn gzip(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Gzip, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn gunzip(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Gunzip, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn deflate_raw(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::DeflateRaw, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn inflate_raw(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::InflateRaw, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn unzip(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Unzip, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn brotli_compress(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::BrotliCompress, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn brotli_decompress(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::BrotliDecompress, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn zstd_compress(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::ZstdCompress, buffer, &rest)
    }

    #[function(length = 3)]
    pub fn zstd_decompress(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::ZstdDecompress, buffer, &rest)
    }

    #[function(length = 1)]
    pub fn create_deflate<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::deflate_class::class_constructor(scope)?,
            options,
        )
    }

    #[function(length = 1)]
    pub fn create_inflate<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::inflate_class::class_constructor(scope)?,
            options,
        )
    }

    #[function(length = 1)]
    pub fn create_gzip<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(scope, super::gzip_class::class_constructor(scope)?, options)
    }

    #[function(length = 1)]
    pub fn create_gunzip<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::gunzip_class::class_constructor(scope)?,
            options,
        )
    }

    #[function(length = 1)]
    pub fn create_deflate_raw<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::deflate_raw_class::class_constructor(scope)?,
            options,
        )
    }

    #[function(length = 1)]
    pub fn create_inflate_raw<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::inflate_raw_class::class_constructor(scope)?,
            options,
        )
    }

    #[function(length = 1)]
    pub fn create_unzip<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::unzip_class::class_constructor(scope)?,
            options,
        )
    }

    #[function(length = 1)]
    pub fn create_brotli_compress<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::brotli_compress_class::class_constructor(scope)?,
            options,
        )
    }

    #[function(length = 1)]
    pub fn create_brotli_decompress<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::brotli_decompress_class::class_constructor(scope)?,
            options,
        )
    }

    #[function(length = 1)]
    pub fn create_zstd_compress<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::zstd_compress_class::class_constructor(scope)?,
            options,
        )
    }

    #[function(length = 1)]
    pub fn create_zstd_decompress<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(
            scope,
            super::zstd_decompress_class::class_constructor(scope)?,
            options,
        )
    }

    /// <https://nodejs.org/api/zlib.html#zlibcrc32data-value>
    pub fn crc32(
        scope: &Scope<'_>,
        data: HandleValue<'_>,
        value: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        let bytes = input_bytes(scope, data)?;
        let initial = match value.filter(|value| !value.is_undefined()) {
            None => 0.0,
            Some(value) => {
                crate::errors::validate_integer(scope, value, "value", 0.0, 4294967295.0)?
            }
        };
        let mut hasher = crc32fast::Hasher::new_with_initial(initial as u32);
        hasher.update(&bytes);
        Ok(f64::from(hasher.finalize()))
    }
}

/// Register `node:zlib` with `constants`, and with `codes`, which maps each code name to its
/// number and each number to its name.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let tables = || -> Result<(HandleValue<'_>, HandleValue<'_>), ExnThrown> {
        let key = |name: &str| std::ffi::CString::new(name).expect("the names have no NULs");
        let constants = Object::new_plain(scope)?;
        for (constant, value) in CONSTANTS {
            constants.set_property_by_name(scope, &key(constant), value)?;
        }
        let codes = Object::new_plain(scope)?;
        for (code, value) in CODES {
            codes.set_property_by_name(scope, &key(code), value)?;
            codes.set_property_by_name(scope, &key(&value.to_string()), code)?;
        }
        Ok((
            constants.to_jsval_throwing(scope)?,
            codes.to_jsval_throwing(scope)?,
        ))
    };
    let Ok((constants, codes)) = tables() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(
        scope,
        name,
        &[("constants", constants), ("codes", codes)],
    )
}
