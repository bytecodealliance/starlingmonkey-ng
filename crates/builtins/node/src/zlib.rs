// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:zlib`'s deflate, gzip, Brotli and zstd formats, and `crc32`. See
//! <https://nodejs.org/api/zlib.html>.
//!
//! The streams, the callback functions and the synchronous functions share one codec over the
//! `flate2` crate's zlib engine, the `brotli` crate and the `ruzstd` crate. The callback functions
//! compress or decompress synchronously and call back in a microtask. The zip archive functions are
//! stubs.
//!
//! `params` leaves the level unchanged when the engine has already compressed input and the new
//! level uses a different zlib strategy function, because `flate2` changes the level without an
//! output buffer.
//!
//! `ruzstd` compresses at one level, which is close to zstd's level 1, so the zstd compression
//! level has no effect. The zstd compressors and decompressors produce their output once their
//! input ends, so flushes have no effect on them.

use brotli::enc::encode::{
    BrotliEncoderOperation, BrotliEncoderParameter, BrotliEncoderStateStruct,
};
use brotli::enc::StandardAlloc;
use brotli::{BrotliDecompressStream, BrotliResult, BrotliState};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::{HandleId, Value};
use js::prelude::HandleValue;
use js::Object;
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

use crate::buffer::{encode_string, new_buffer, Encoding, MAX_LENGTH};
use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};
use crate::stream::{
    call_method, closure, error_value, is_function, next_tick_call, string, take_exception,
    Captures, Props,
};

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

/// Node.js's code names of the Brotli decoder's errors: `ERR_` and the name of the constant without
/// `BROTLI_DECODER`, by error code.
const BROTLI_ERROR_CODES: [(i32, &str); 25] = [
    (-1, "ERR__ERROR_FORMAT_EXUBERANT_NIBBLE"),
    (-2, "ERR__ERROR_FORMAT_RESERVED"),
    (-3, "ERR__ERROR_FORMAT_EXUBERANT_META_NIBBLE"),
    (-4, "ERR__ERROR_FORMAT_SIMPLE_HUFFMAN_ALPHABET"),
    (-5, "ERR__ERROR_FORMAT_SIMPLE_HUFFMAN_SAME"),
    (-6, "ERR__ERROR_FORMAT_CL_SPACE"),
    (-7, "ERR__ERROR_FORMAT_HUFFMAN_SPACE"),
    (-8, "ERR__ERROR_FORMAT_CONTEXT_MAP_REPEAT"),
    (-9, "ERR__ERROR_FORMAT_BLOCK_LENGTH_1"),
    (-10, "ERR__ERROR_FORMAT_BLOCK_LENGTH_2"),
    (-11, "ERR__ERROR_FORMAT_TRANSFORM"),
    (-12, "ERR__ERROR_FORMAT_DICTIONARY"),
    (-13, "ERR__ERROR_FORMAT_WINDOW_BITS"),
    (-14, "ERR__ERROR_FORMAT_PADDING_1"),
    (-15, "ERR__ERROR_FORMAT_PADDING_2"),
    (-16, "ERR__ERROR_FORMAT_DISTANCE"),
    (-19, "ERR__ERROR_DICTIONARY_NOT_SET"),
    (-20, "ERR__ERROR_INVALID_ARGUMENTS"),
    (-21, "ERR__ERROR_ALLOC_CONTEXT_MODES"),
    (-22, "ERR__ERROR_ALLOC_TREE_GROUPS"),
    (-25, "ERR__ERROR_ALLOC_CONTEXT_MAP"),
    (-26, "ERR__ERROR_ALLOC_RING_BUFFER_1"),
    (-27, "ERR__ERROR_ALLOC_RING_BUFFER_2"),
    (-30, "ERR__ERROR_ALLOC_BLOCK_TYPE_TREES"),
    (-31, "ERR__ERROR_UNREACHABLE"),
];

/// zstd's errors for input that doesn't start with a frame, for corrupt frames, for frame headers
/// with the reserved bit set and for frames whose checksum doesn't match their content: the
/// message, the code name and its number.
const ZSTD_PREFIX_UNKNOWN: (&str, &str, i32) =
    ("Unknown frame descriptor", "ZSTD_error_prefix_unknown", 10);
const ZSTD_CORRUPTION_DETECTED: (&str, &str, i32) = (
    "Data corruption detected",
    "ZSTD_error_corruption_detected",
    20,
);
const ZSTD_FRAME_PARAMETER_UNSUPPORTED: (&str, &str, i32) = (
    "Unsupported frame parameter",
    "ZSTD_error_frameParameter_unsupported",
    14,
);
const ZSTD_CHECKSUM_WRONG: (&str, &str, i32) = (
    "Restored data doesn't match checksum",
    "ZSTD_error_checksum_wrong",
    22,
);

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

/// The size of the output buffer each step of the engine fills.
const CHUNK: usize = 16 * 1024;

/// A compression format, or the decompression of one. `Unzip` decompresses gzip or zlib,
/// whichever the input starts with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Deflate,
    Inflate,
    Gzip,
    Gunzip,
    DeflateRaw,
    InflateRaw,
    Unzip,
    BrotliCompress,
    BrotliDecompress,
    ZstdCompress,
    ZstdDecompress,
}

/// The formats that share flush constants and options.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Zlib,
    Brotli,
    Zstd,
}

impl Format {
    fn family(self) -> Family {
        match self {
            Format::BrotliCompress | Format::BrotliDecompress => Family::Brotli,
            Format::ZstdCompress | Format::ZstdDecompress => Family::Zstd,
            _ => Family::Zlib,
        }
    }

    fn decompresses(self) -> bool {
        matches!(
            self,
            Format::Inflate
                | Format::Gunzip
                | Format::InflateRaw
                | Format::Unzip
                | Format::BrotliDecompress
                | Format::ZstdDecompress
        )
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

    /// The flush that ends a conversion whose `finishFlush` is `kind`.
    fn finish_flush(self, kind: f64) -> Flush {
        if kind == self.flush_constants().3 {
            Flush::Finish
        } else {
            self.flush(kind)
        }
    }
}

/// How much of the compressed data a call to [`Codec::process`] produces.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flush {
    /// Only what the engine doesn't buffer.
    None,
    /// Everything up to the end of the input, as `Z_SYNC_FLUSH` does.
    Sync,
    /// Everything up to the end of the input, as `Z_FULL_FLUSH` does.
    Full,
    /// Everything, ending the compressed stream.
    Finish,
}

/// A zlib error: the message, the code name, such as `Z_DATA_ERROR`, and its number, or
/// `ERR_BUFFER_TOO_LARGE`, a `RangeError` without a number.
struct ZlibError {
    message: String,
    code: &'static str,
    errno: Option<i32>,
}

impl ZlibError {
    fn data(message: Option<&str>) -> Self {
        ZlibError {
            message: message.unwrap_or("invalid data").to_string(),
            code: "Z_DATA_ERROR",
            errno: Some(-3),
        }
    }

    fn unexpected_end() -> Self {
        ZlibError {
            message: "unexpected end of file".into(),
            code: "Z_BUF_ERROR",
            errno: Some(-5),
        }
    }

    fn too_large(message: String) -> Self {
        ZlibError {
            message,
            code: "ERR_BUFFER_TOO_LARGE",
            errno: None,
        }
    }

    /// The error of a Brotli decoder that failed with the error code `code`.
    fn brotli(code: i32) -> Self {
        let name = BROTLI_ERROR_CODES
            .iter()
            .find(|&&(value, _)| value == code)
            .map_or("ERR__ERROR_UNREACHABLE", |&(_, name)| name);
        ZlibError {
            message: "Decompression failed".into(),
            code: name,
            errno: Some(code),
        }
    }

    /// The zstd error `code`: its message, code name and number.
    fn zstd_code((message, code, errno): (&str, &'static str, i32)) -> Self {
        ZlibError {
            message: message.into(),
            code,
            errno: Some(errno),
        }
    }

    /// The error of a zstd decoder that failed with `error`.
    fn zstd(error: &FrameDecoderError) -> Self {
        let truncated =
            std::iter::successors(Some(error as &(dyn std::error::Error + 'static)), |error| {
                error.source()
            })
            .any(|error| {
                error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof)
            });
        if truncated {
            return ZlibError::unexpected_end();
        }
        ZlibError::zstd_code(match error {
            FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::BadMagicNumber(_)) => {
                ZSTD_PREFIX_UNKNOWN
            }
            FrameDecoderError::WindowSizeTooBig { .. } => (
                "Frame requires too much memory for decoding",
                "ZSTD_error_frameParameter_windowTooLarge",
                16,
            ),
            _ => ZSTD_CORRUPTION_DETECTED,
        })
    }

    /// The error as a JS `Error` with `code` and `errno`.
    fn into_value<'s>(self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
        let kind = match self.errno {
            Some(_) => ErrorKind::Error,
            None => ErrorKind::RangeError,
        };
        let error = error_value(scope, NodeError::new(kind, self.code, self.message))?;
        if let Some(errno) = self.errno {
            Object::from_value(scope, *error)
                .map_err(|e| e.throw(scope))?
                .set_property(scope, c"errno", errno)?;
        }
        Ok(error)
    }
}

impl ThrowException for ZlibError {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        if let Ok(error) = self.into_value(scope) {
            js::exception::set_pending(scope, error, js::native::ExceptionStackBehavior::Capture);
        }
        ExnThrown
    }
}

/// The compression level for the `level` option `level`, where values outside 0 to 9 select
/// the default.
fn compression(level: i32) -> Compression {
    match u32::try_from(level) {
        Ok(level) if level <= 9 => Compression::new(level),
        _ => Compression::default(),
    }
}

/// The options a [`Codec`] is created with.
#[derive(Clone)]
struct Params {
    /// The zlib compression level, -1 for the default.
    level: i32,
    /// The Brotli parameters, as `BROTLI_PARAM_*` keys and their values.
    brotli: Vec<(u32, u32)>,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            level: -1,
            brotli: Vec::new(),
        }
    }
}

/// The Brotli encoder parameter with the `BROTLI_PARAM_*` value `key`. `BROTLI_PARAM_NPOSTFIX` and
/// `BROTLI_PARAM_NDIRECT` have none, so they have no effect.
fn brotli_parameter(key: u32) -> Option<BrotliEncoderParameter> {
    Some(match key {
        0 => BrotliEncoderParameter::BROTLI_PARAM_MODE,
        1 => BrotliEncoderParameter::BROTLI_PARAM_QUALITY,
        2 => BrotliEncoderParameter::BROTLI_PARAM_LGWIN,
        3 => BrotliEncoderParameter::BROTLI_PARAM_LGBLOCK,
        4 => BrotliEncoderParameter::BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING,
        5 => BrotliEncoderParameter::BROTLI_PARAM_SIZE_HINT,
        6 => BrotliEncoderParameter::BROTLI_PARAM_LARGE_WINDOW,
        _ => return None,
    })
}

/// The Brotli decoder's state.
type BrotliDecoder = BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>;

/// The engine of a [`Codec`].
enum Engine {
    Compress(Compress),
    Decompress(Decompress),
    BrotliCompress(Box<BrotliEncoderStateStruct<StandardAlloc>>),
    BrotliDecompress(Box<BrotliDecoder>),
    /// The zstd input, which is compressed as one frame once it ends.
    ZstdCompress(Vec<u8>),
    /// The zstd input, which is decompressed once it ends.
    ZstdDecompress(Vec<u8>),
}

/// A compressor or decompressor that takes its input in pieces.
pub(crate) struct Codec {
    /// The format, which `Unzip` replaces with the format it detects.
    format: Format,
    /// The format the codec was created for, which `reset` restores.
    created: Format,
    params: Params,
    /// The engine, or `None` for `Unzip` until two bytes have arrived.
    engine: Option<Engine>,
    /// The input `Unzip` buffers until two bytes have arrived.
    held: Vec<u8>,
    /// Whether the decompressor reached the end of a compressed stream.
    ended: bool,
}

impl Default for Codec {
    fn default() -> Self {
        Codec::new(Format::Deflate, &Params::default())
    }
}

impl Codec {
    fn new(format: Format, params: &Params) -> Self {
        let level = params.level;
        let compress = |engine| Some(Engine::Compress(engine));
        let decompress = |engine| Some(Engine::Decompress(engine));
        let engine = match format {
            Format::Deflate => {
                compress(Compress::new_with_window_bits(compression(level), true, 15))
            }
            Format::Gzip => compress(Compress::new_gzip(compression(level), 15)),
            Format::DeflateRaw => compress(Compress::new_with_window_bits(
                compression(level),
                false,
                15,
            )),
            Format::Inflate => decompress(Decompress::new_with_window_bits(true, 15)),
            Format::Gunzip => decompress(Decompress::new_gzip(15)),
            Format::InflateRaw => decompress(Decompress::new_with_window_bits(false, 15)),
            Format::Unzip => None,
            Format::BrotliCompress => {
                let mut state = BrotliEncoderStateStruct::new(StandardAlloc::default());
                for &(key, value) in &params.brotli {
                    if let Some(parameter) = brotli_parameter(key) {
                        state.set_parameter(parameter, value);
                    }
                }
                Some(Engine::BrotliCompress(Box::new(state)))
            }
            Format::BrotliDecompress => Some(Engine::BrotliDecompress(Box::new(BrotliState::new(
                StandardAlloc::default(),
                StandardAlloc::default(),
                StandardAlloc::default(),
            )))),
            Format::ZstdCompress => Some(Engine::ZstdCompress(Vec::new())),
            Format::ZstdDecompress => Some(Engine::ZstdDecompress(Vec::new())),
        };
        Codec {
            format,
            created: format,
            params: params.clone(),
            engine,
            held: Vec::new(),
            ended: false,
        }
    }

    /// A codec without an engine, which holds no compression state.
    fn closed(format: Format, params: Params) -> Self {
        Codec {
            format,
            created: format,
            params,
            engine: None,
            held: Vec::new(),
            ended: false,
        }
    }

    /// Process `input` and return the output. With `Flush::Finish`, a decompressor that hasn't
    /// reached the end of a compressed stream fails. Decompressors ignore the other flushes.
    fn process(&mut self, input: &[u8], flush: Flush) -> Result<Vec<u8>, ZlibError> {
        let finish = flush == Flush::Finish;
        let held;
        let mut input = input;
        if self.engine.is_none() {
            self.held.extend_from_slice(input);
            if self.held.len() < 2 && !finish {
                return Ok(Vec::new());
            }
            held = std::mem::take(&mut self.held);
            let format = if held.starts_with(&[0x1f, 0x8b]) {
                Format::Gunzip
            } else {
                Format::Inflate
            };
            *self = Codec {
                created: self.created,
                ..Codec::new(format, &self.params)
            };
            input = &held;
        }
        let mut output = Vec::new();
        match self.engine.as_mut() {
            // A flush before the first input produces nothing, and leaves the engine able to
            // change its level.
            Some(Engine::Compress(engine))
                if input.is_empty() && engine.total_in() == 0 && !finish => {}
            Some(Engine::Compress(engine)) => {
                let flush = match flush {
                    Flush::None => FlushCompress::None,
                    Flush::Sync => FlushCompress::Sync,
                    Flush::Full => FlushCompress::Full,
                    Flush::Finish => FlushCompress::Finish,
                };
                loop {
                    let before = engine.total_in();
                    output.reserve(CHUNK);
                    let status = engine
                        .compress_vec(input, &mut output, flush)
                        .map_err(|error| ZlibError::data(error.message()))?;
                    input = &input[(engine.total_in() - before) as usize..];
                    // The engine is done once it consumed all input and left output space
                    // unused.
                    if status == Status::StreamEnd
                        || (input.is_empty() && output.len() < output.capacity())
                    {
                        break;
                    }
                }
            }
            Some(Engine::Decompress(_)) => self.decompress(input, &mut output)?,
            Some(Engine::BrotliCompress(state)) => {
                brotli_compress(state, input, flush, &mut output)?
            }
            Some(Engine::BrotliDecompress(state)) => {
                if !self.ended {
                    self.ended = brotli_decompress(state, input, &mut output)?;
                }
            }
            Some(Engine::ZstdCompress(held)) => {
                held.extend_from_slice(input);
                if finish {
                    output = ruzstd::encoding::compress_to_vec(
                        held.as_slice(),
                        ruzstd::encoding::CompressionLevel::Fastest,
                    );
                    held.clear();
                }
            }
            Some(Engine::ZstdDecompress(held)) => {
                held.extend_from_slice(input);
                if finish {
                    zstd_decompress(held, &mut output)?;
                    held.clear();
                    self.ended = true;
                }
            }
            None => unreachable!("the engine was created above"),
        }
        if finish && !self.ended && self.format.decompresses() {
            return Err(ZlibError::unexpected_end());
        }
        Ok(output)
    }

    /// Decompress `input` into `output`. After the end of a gzip member, a non-zero byte starts
    /// another member, and a zero byte ends the input. Bytes after the end of other formats'
    /// streams are ignored.
    fn decompress(&mut self, mut input: &[u8], output: &mut Vec<u8>) -> Result<(), ZlibError> {
        // A call that fills the output can leave output in the decompressor, which further calls
        // without input produce.
        let mut filled = false;
        while !input.is_empty() || filled {
            if self.ended {
                if self.format != Format::Gunzip || input[0] == 0 {
                    break;
                }
                *self = Codec {
                    created: self.created,
                    ..Codec::new(Format::Gunzip, &self.params)
                };
            }
            let Some(Engine::Decompress(engine)) = self.engine.as_mut() else {
                unreachable!("decompressing formats have a decompressor");
            };
            let (before_in, before_out) = (engine.total_in(), engine.total_out());
            output.reserve(CHUNK);
            let space = output.capacity() - output.len();
            let status = engine
                .decompress_vec(input, output, FlushDecompress::None)
                .map_err(|error| ZlibError::data(error.message()))?;
            input = &input[(engine.total_in() - before_in) as usize..];
            filled = (engine.total_out() - before_out) as usize == space;
            if status == Status::StreamEnd {
                self.ended = true;
                filled = false;
            } else if engine.total_in() == before_in && engine.total_out() == before_out {
                break;
            }
        }
        Ok(())
    }
}

/// Compress `input` with the Brotli encoder `state` into `output`, with the operation `flush`
/// selects: `BROTLI_OPERATION_PROCESS` for `Flush::None`, `BROTLI_OPERATION_FINISH` for
/// `Flush::Finish`, and `BROTLI_OPERATION_FLUSH` otherwise.
fn brotli_compress(
    state: &mut BrotliEncoderStateStruct<StandardAlloc>,
    mut input: &[u8],
    flush: Flush,
    output: &mut Vec<u8>,
) -> Result<(), ZlibError> {
    let operation = match flush {
        Flush::None if input.is_empty() => return Ok(()),
        Flush::None => BrotliEncoderOperation::BROTLI_OPERATION_PROCESS,
        Flush::Sync | Flush::Full => BrotliEncoderOperation::BROTLI_OPERATION_FLUSH,
        Flush::Finish => BrotliEncoderOperation::BROTLI_OPERATION_FINISH,
    };
    let finish = flush == Flush::Finish;
    let mut buffer = vec![0; CHUNK];
    loop {
        let mut available_in = input.len();
        let mut input_offset = 0;
        let mut available_out = buffer.len();
        let mut output_offset = 0;
        let compressed = state.compress_stream(
            operation,
            &mut available_in,
            input,
            &mut input_offset,
            &mut available_out,
            &mut buffer,
            &mut output_offset,
            &mut None,
            &mut |_, _, _, _| (),
        );
        output.extend_from_slice(&buffer[..output_offset]);
        input = &input[input_offset..];
        if !compressed {
            return Err(ZlibError::data(Some("Compression failed")));
        }
        let done = if finish {
            state.is_finished()
        } else {
            input.is_empty() && !state.has_more_output()
        };
        if done {
            return Ok(());
        }
    }
}

/// Decompress `input` with the Brotli decoder `state` into `output`. Returns whether the
/// compressed stream ended. Bytes after its end are ignored.
fn brotli_decompress(
    state: &mut BrotliDecoder,
    mut input: &[u8],
    output: &mut Vec<u8>,
) -> Result<bool, ZlibError> {
    let mut buffer = vec![0; CHUNK];
    loop {
        let mut available_in = input.len();
        let mut input_offset = 0;
        let mut available_out = buffer.len();
        let mut output_offset = 0;
        let mut total_out = 0;
        let result = BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            input,
            &mut available_out,
            &mut output_offset,
            &mut buffer,
            &mut total_out,
            state,
        );
        output.extend_from_slice(&buffer[..output_offset]);
        input = &input[input_offset..];
        match result {
            BrotliResult::ResultSuccess => return Ok(true),
            BrotliResult::NeedsMoreInput => return Ok(false),
            BrotliResult::NeedsMoreOutput => {}
            BrotliResult::ResultFailure => {
                return Err(ZlibError::brotli(state.error_code as i32));
            }
        }
    }
}

/// Decompress the first zstd frame in `input` into `output`, as Node.js does. Skippable frames
/// before it are skipped, and the bytes after it are ignored.
fn zstd_decompress(mut input: &[u8], output: &mut Vec<u8>) -> Result<(), ZlibError> {
    let mut decoder = FrameDecoder::new();
    let mut declares_size;
    loop {
        if input.is_empty() {
            return Err(ZlibError::unexpected_end());
        }
        // A frame starts with the magic number 0xFD2FB528, and a skippable frame with one from
        // 0x184D2A50 to 0x184D2A5F, both little-endian.
        let head = &input[..input.len().min(4)];
        let frame = head == &[0x28, 0xb5, 0x2f, 0xfd][..head.len()];
        let skippable = head[0] & 0xf0 == 0x50 && head[1..] == [0x2a, 0x4d, 0x18][..head.len() - 1];
        if !frame && !skippable {
            return Err(ZlibError::zstd_code(ZSTD_PREFIX_UNKNOWN));
        }
        // The frame header descriptor's `Frame_Content_Size_flag` and `Single_Segment_flag`
        // tell whether the header holds the size of the frame's content.
        let descriptor = input.get(4).copied().unwrap_or(0);
        declares_size = frame && (descriptor >> 6 != 0 || descriptor & 0x20 != 0);
        // Its reserved bit must be 0.
        if frame && descriptor & 0x08 != 0 {
            return Err(ZlibError::zstd_code(ZSTD_FRAME_PARAMETER_UNSUPPORTED));
        }
        match decoder.init(&mut input) {
            Ok(()) => break,
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length,
                ..
            })) => {
                input = input
                    .get(length as usize..)
                    .ok_or_else(ZlibError::unexpected_end)?;
            }
            Err(error) => return Err(ZlibError::zstd(&error)),
        }
    }
    decoder
        .decode_blocks(&mut input, BlockDecodingStrategy::All)
        .map_err(|error| ZlibError::zstd(&error))?;
    if !decoder.is_finished() {
        return Err(ZlibError::unexpected_end());
    }
    let content = decoder.collect().unwrap_or_default();
    if declares_size && content.len() as u64 != decoder.content_size() {
        return Err(ZlibError::zstd_code(ZSTD_CORRUPTION_DETECTED));
    }
    if let (Some(expected), Some(actual)) = (
        decoder.get_checksum_from_data(),
        decoder.get_calculated_checksum(),
    ) {
        if expected != actual {
            return Err(ZlibError::zstd_code(ZSTD_CHECKSUM_WRONG));
        }
    }
    output.extend(content);
    Ok(())
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
        Family::Zlib => result.params.level = zlib_format_options(scope, options, format)?,
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
    let flush = options.get_property(scope, c"flush")?;
    range_or_default(
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
/// level.
fn zlib_format_options(
    scope: &Scope<'_>,
    options: Object<'_>,
    format: Format,
) -> Result<i32, Thrown> {
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
    range_or_default(scope, strategy, "options.strategy", 0.0, 4.0, 0.0)?;
    let dictionary = options.get_property(scope, c"dictionary")?;
    let is_buffer_source =
        Object::from_value(scope, dictionary).is_ok_and(js::typedarray::is_buffer_source);
    if !dictionary.is_undefined() && !is_buffer_source {
        let expected = "an instance of Buffer, TypedArray, DataView, or ArrayBuffer";
        return Err(
            NodeError::invalid_arg_type(scope, "options.dictionary", expected, dictionary).into(),
        );
    }
    Ok(level)
}

/// Compress or decompress all of `data` in `format`.
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
    Ok(match codec.process(&bytes, flush) {
        Ok(output) if output.len() as f64 > options.max_output_length => {
            let message = format!(
                "Cannot create a Buffer larger than {} bytes",
                options.max_output_length
            );
            Err(ZlibError::too_large(message))
        }
        Ok(output) => Ok(new_buffer(scope, &output)?),
        Err(error) => Err(error),
    })
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

/// The state of a zlib stream, in its `_handle` property.
#[core_runtime::jsclass(hidden)]
pub struct ZlibHandle {
    #[no_trace]
    codec: Codec,
}

#[core_runtime::jsmethods]
impl ZlibHandle {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// The handle of the zlib stream `stream`.
fn handle_of<'s>(scope: &'s Scope<'_>, stream: Object<'_>) -> Result<ZlibHandle<'s>, Thrown> {
    let invalid = || {
        NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_THIS",
            "Value of \"this\" must be a zlib stream",
        )
    };
    let handle = stream.obj(scope, c"_handle")?.ok_or_else(invalid)?;
    handle.cast::<ZlibHandle>().map_err(|_| invalid().into())
}

/// Process `input` with the codec of `stream`, push the output, and add the input's length to
/// `bytesWritten`.
fn write_to(
    scope: &Scope<'_>,
    stream: Object<'_>,
    input: &[u8],
    flush: Flush,
) -> Result<(), Thrown> {
    let result = handle_of(scope, stream)?
        .data_mut()
        .codec
        .process(input, flush);
    let output = result.map_err(|error| Thrown::from(error.throw(scope)))?;
    let written = stream.num(scope, c"bytesWritten")?;
    stream.set_property(scope, c"bytesWritten", written + input.len() as f64)?;
    if !output.is_empty() {
        let buffer = new_buffer(scope, &output)?.to_jsval_throwing(scope)?;
        call_method(scope, stream, c"push", &[buffer])?;
    }
    Ok(())
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
            Some(take_exception(scope))
        }
    };
    js::Function::call(scope, undefined, callback, error.as_slice())?;
    Ok(())
}

/// The key of the property that marks the empty chunks [`flush_stream`] writes, whose value is
/// the zlib flush constant.
fn flush_flag<'s>(scope: &'s Scope<'_>) -> Result<HandleId<'s>, ExnThrown> {
    let symbol = crate::symbol(scope, "nodejs.zlib.flushFlag", true)?;
    Ok(scope.root_id(js::id::value_to_id(scope, symbol)?))
}

/// The flush the chunk `chunk` written to `stream` requests: the flush of its flush flag, or
/// `Flush::None`.
fn chunk_flush(
    scope: &Scope<'_>,
    stream: Object<'_>,
    chunk: HandleValue<'_>,
) -> Result<Flush, Thrown> {
    let Ok(chunk) = Object::from_value(scope, *chunk) else {
        return Ok(Flush::None);
    };
    let kind = chunk.get_property_by_id(scope, flush_flag(scope)?)?;
    if !kind.is_number() {
        return Ok(Flush::None);
    }
    let family = handle_of(scope, stream)?.data().codec.format.family();
    Ok(family.flush(kind.to_number()))
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
        if is_function(scope, callback) {
            call_method(scope, stream, c"once", &[string(scope, "end")?, callback])?;
        }
    } else {
        let marker = new_buffer(scope, &[])?;
        marker.set_property_by_id(scope, flush_flag(scope)?, kind)?;
        let marker = marker.to_jsval_throwing(scope)?;
        call_method(scope, stream, c"write", &[marker, callback])?;
    }
    Ok(())
}

/// The flush callback of `params`, whose captures are `[stream, level, callback]`: set the level
/// and call the callback.
fn apply_params(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let level = captures.get(1)?.to_number() as i32;
    let handle = handle_of(scope, captures.obj(0)?).map_err(|thrown| thrown.throw(scope))?;
    let mut state = handle.data_mut();
    let changed = match state.codec.engine.as_mut() {
        Some(Engine::Compress(engine)) => engine.set_level(compression(level)).is_ok(),
        _ => true,
    };
    if changed {
        state.codec.params.level = level;
    }
    drop(state);
    let callback = captures.get(2)?;
    if is_function(scope, callback) {
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
    let codec = Codec::new(format, &zlib_options(scope, options, format)?.params);
    let handle =
        js::class::create_instance_with::<ZlibHandleImpl>(scope, |_| ZlibHandleImpl { codec })?;
    stream.set_property(scope, c"_handle", handle.to_jsval_throwing(scope)?)?;
    stream.set_property(scope, c"bytesWritten", 0)?;
    Ok(())
}

/// The base of the zlib stream classes. It isn't exported.
#[core_runtime::jsfunction_class(name = "Zlib", extends = "crate::stream::transform")]
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
        if this.val(scope, c"_handle")?.is_null() {
            return next_tick_call(scope, callback, &[]);
        }
        let result = chunk_flush(scope, this, chunk).and_then(|flush| {
            let bytes = input_bytes(scope, chunk)?;
            write_to(scope, this, &bytes, flush)
        });
        complete(scope, callback, result)
    }

    #[method(name = "_flush")]
    pub fn flush_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        if this.val(scope, c"_handle")?.is_null() {
            return next_tick_call(scope, callback, &[]);
        }
        let result = write_to(scope, this, &[], Flush::Finish);
        complete(scope, callback, result)
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
        if let Some(handle) = this.obj(scope, c"_handle")? {
            if let Ok(handle) = handle.cast::<ZlibHandle>() {
                let mut state = handle.data_mut();
                let params = state.codec.params.clone();
                state.codec = Codec::closed(state.codec.created, params);
            }
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
            Some(kind) if is_function(scope, kind) => (None, Some(kind)),
            _ => (kind, callback),
        };
        // A destroyed stream has no handle, and fails the write below.
        let family = if this.val(scope, c"_handle")?.is_null() {
            Family::Zlib
        } else {
            handle_of(scope, this)?.data().codec.format.family()
        };
        let full_flush = family.flush_constants().4;
        let kind = kind
            .filter(|kind| kind.is_number())
            .map_or(full_flush, |kind| kind.to_number());
        let callback = callback.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        Ok(flush_stream(scope, this, kind, callback)?)
    }

    /// <https://nodejs.org/api/zlib.html#zlibparamslevel-strategy-callback>: set the level of a
    /// compressor after a `Z_SYNC_FLUSH` of the chunks written before. The strategy is ignored.
    pub fn params(
        scope: &Scope<'_>,
        this: Object<'_>,
        level: f64,
        _strategy: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let done = closure(
            scope,
            0,
            apply_params,
            &[
                this.to_jsval_raw_throwing(scope)?,
                js::value::from_f64(level),
                callback.map_or(js::value::undefined(), |callback| callback.get()),
            ],
        )?;
        flush_stream(scope, this, 2.0, done.to_jsval_throwing(scope)?)
    }

    /// <https://nodejs.org/api/zlib.html#zlibclosecallback>: `callback` is called once the
    /// stream finished, closed or failed, as `stream.finished` calls it.
    pub fn close(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if let Some(callback) = callback.filter(|callback| crate::stream::truthy(scope, *callback))
        {
            let stream = this.to_jsval_throwing(scope)?;
            crate::stream::end_of_stream(scope, stream, None, callback)?;
        }
        call_method(scope, this, c"destroy", &[])?;
        Ok(())
    }

    /// <https://nodejs.org/api/zlib.html#zlibreset>
    pub fn reset(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let handle = handle_of(scope, this)?;
        let mut state = handle.data_mut();
        let params = state.codec.params.clone();
        state.codec = Codec::new(state.codec.created, &params);
        Ok(())
    }
}

/// Define the zlib stream class `$name` for `$format` in the module `$module`.
macro_rules! zlib_stream_class {
    ($module:ident, $name:literal, $format:ident) => {
        #[core_runtime::jsfunction_class(name = $name, extends = "super::zlib_class", self_construct)]
        pub mod $module {
            use super::*;

            #[constructor]
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

    pub fn deflate_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Deflate, buffer, options)
    }

    pub fn inflate_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Inflate, buffer, options)
    }

    pub fn gzip_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Gzip, buffer, options)
    }

    pub fn gunzip_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Gunzip, buffer, options)
    }

    pub fn deflate_raw_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::DeflateRaw, buffer, options)
    }

    pub fn inflate_raw_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::InflateRaw, buffer, options)
    }

    pub fn unzip_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::Unzip, buffer, options)
    }

    pub fn brotli_compress_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::BrotliCompress, buffer, options)
    }

    pub fn brotli_decompress_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::BrotliDecompress, buffer, options)
    }

    pub fn zstd_compress_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::ZstdCompress, buffer, options)
    }

    pub fn zstd_decompress_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        convert_sync(scope, Format::ZstdDecompress, buffer, options)
    }

    pub fn deflate(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Deflate, buffer, &rest)
    }

    pub fn inflate(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Inflate, buffer, &rest)
    }

    pub fn gzip(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Gzip, buffer, &rest)
    }

    pub fn gunzip(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Gunzip, buffer, &rest)
    }

    pub fn deflate_raw(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::DeflateRaw, buffer, &rest)
    }

    pub fn inflate_raw(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::InflateRaw, buffer, &rest)
    }

    pub fn unzip(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::Unzip, buffer, &rest)
    }

    pub fn brotli_compress(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::BrotliCompress, buffer, &rest)
    }

    pub fn brotli_decompress(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::BrotliDecompress, buffer, &rest)
    }

    pub fn zstd_compress(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::ZstdCompress, buffer, &rest)
    }

    pub fn zstd_decompress(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        rest: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        convert_async(scope, Format::ZstdDecompress, buffer, &rest)
    }

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

    pub fn create_gzip<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        create(scope, super::gzip_class::class_constructor(scope)?, options)
    }

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

/// Register the stream classes of `node:zlib`.
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
