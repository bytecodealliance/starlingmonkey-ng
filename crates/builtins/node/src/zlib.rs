// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:zlib`'s deflate and gzip formats, and `crc32`. See <https://nodejs.org/api/zlib.html>.
//!
//! The streams, the callback functions and the synchronous functions share one codec over the
//! `flate2` crate's zlib engine. The callback functions compress or decompress synchronously and
//! call back in a microtask. Brotli, zstd and the zip archive functions are stubs.
//!
//! `params` leaves the level unchanged when the engine has already compressed input and the new
//! level uses a different zlib strategy function, because `flate2` changes the level without an
//! output buffer.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::{HandleId, Value};
use js::prelude::HandleValue;
use js::Object;

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

impl Flush {
    /// The flush for the zlib flush constant `kind`: `Z_NO_FLUSH` is `None`, `Z_PARTIAL_FLUSH`,
    /// `Z_SYNC_FLUSH` and `Z_BLOCK` are `Sync`, and the others are `Full`.
    fn from_kind(kind: f64) -> Self {
        match kind as i32 {
            0 => Flush::None,
            1 | 2 | 5 => Flush::Sync,
            _ => Flush::Full,
        }
    }
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

/// The zlib engine of a [`Codec`].
enum Engine {
    Compress(Compress),
    Decompress(Decompress),
}

/// A compressor or decompressor that takes its input in pieces.
pub(crate) struct Codec {
    /// The format, which `Unzip` replaces with the format it detects.
    format: Format,
    /// The format the codec was created for, which `reset` restores.
    created: Format,
    level: i32,
    /// The engine, or `None` for `Unzip` until two bytes have arrived.
    engine: Option<Engine>,
    /// The input `Unzip` buffers until two bytes have arrived.
    held: Vec<u8>,
    /// Whether the decompressor reached the end of a compressed stream.
    ended: bool,
}

impl Default for Codec {
    fn default() -> Self {
        Codec::new(Format::Deflate, -1)
    }
}

impl Codec {
    fn new(format: Format, level: i32) -> Self {
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
        };
        Codec {
            format,
            created: format,
            level,
            engine,
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
                ..Codec::new(format, self.level)
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
            None => unreachable!("the engine was created above"),
        }
        if finish && !self.ended && !matches!(self.engine, Some(Engine::Compress(_))) {
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
                    ..Codec::new(Format::Gunzip, self.level)
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
    /// The compression level, -1 for the default.
    level: i32,
    /// The largest output a synchronous conversion produces.
    max_output_length: f64,
    /// The zlib flush constant a conversion ends with, `Z_FINISH` by default.
    finish_flush: f64,
}

/// The options of `format` in `options`, validated as Node.js's `Zlib` and `ZlibBase`
/// constructors validate them: `windowBits`, `level`, `memLevel`, `strategy`, `dictionary`,
/// `chunkSize`, `flush`, `finishFlush` and `maxOutputLength`.
fn zlib_options(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
    format: Format,
) -> Result<ZlibOptions, Thrown> {
    let mut result = ZlibOptions {
        level: -1,
        max_output_length: MAX_LENGTH,
        finish_flush: 4.0,
    };
    let Some(options) = options
        .filter(|options| bool::from_jsval(scope, *options, ()).unwrap_or(false))
        .and_then(|options| Object::from_value(scope, *options).ok())
    else {
        return Ok(result);
    };
    let window_bits = options.get_property(scope, c"windowBits")?;
    let decompressing = matches!(format, Format::Inflate | Format::Gunzip | Format::Unzip);
    let unset = window_bits.is_null_or_undefined()
        || (window_bits.is_number() && window_bits.to_number() == 0.0);
    if !(unset && decompressing) {
        let min = if format == Format::Gzip { 9.0 } else { 8.0 };
        range_or_default(scope, window_bits, "options.windowBits", min, 15.0, 15.0)?;
    }
    let level = options.get_property(scope, c"level")?;
    result.level = range_or_default(scope, level, "options.level", -1.0, 9.0, -1.0)? as i32;
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
    range_or_default(scope, flush, "options.flush", 0.0, 5.0, 0.0)?;
    let finish_flush = options.get_property(scope, c"finishFlush")?;
    result.finish_flush =
        range_or_default(scope, finish_flush, "options.finishFlush", 0.0, 5.0, 4.0)?;
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

/// Compress or decompress all of `data` in `format`.
fn convert<'s>(
    scope: &'s Scope<'_>,
    format: Format,
    data: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<Result<Object<'s>, ZlibError>, Thrown> {
    let options = zlib_options(scope, options, format)?;
    let bytes = input_bytes(scope, data)?;
    let mut codec = Codec::new(format, options.level);
    let flush = if options.finish_flush == 4.0 {
        Flush::Finish
    } else {
        Flush::from_kind(options.finish_flush)
    };
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

/// The flush the chunk `chunk` requests: the flush of its flush flag, or `Flush::None`.
fn chunk_flush(scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<Flush, ExnThrown> {
    let Ok(chunk) = Object::from_value(scope, *chunk) else {
        return Ok(Flush::None);
    };
    let kind = chunk.get_property_by_id(scope, flush_flag(scope)?)?;
    Ok(if kind.is_number() {
        Flush::from_kind(kind.to_number())
    } else {
        Flush::None
    })
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
        state.codec.level = level;
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
    let codec = Codec::new(format, zlib_options(scope, options, format)?.level);
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
        let result = chunk_flush(scope, chunk)
            .map_err(Thrown::from)
            .and_then(|flush| {
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
        let result = write_to(scope, this, &[], Flush::Finish);
        complete(scope, callback, result)
    }

    /// <https://nodejs.org/api/zlib.html#zlibflushkind-callback>: `kind` defaults to
    /// `Z_FULL_FLUSH`.
    pub fn flush(
        scope: &Scope<'_>,
        this: Object<'_>,
        kind: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let (kind, callback) = match kind {
            Some(kind) if is_function(scope, kind) => (None, Some(kind)),
            _ => (kind, callback),
        };
        let kind = kind
            .filter(|kind| kind.is_number())
            .map_or(3.0, |kind| kind.to_number());
        let callback = callback.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        flush_stream(scope, this, kind, callback)
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

    /// <https://nodejs.org/api/zlib.html#zlibclosecallback>
    pub fn close(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        if let Some(callback) = callback.filter(|callback| !callback.is_undefined()) {
            let close = string(scope, "close")?;
            call_method(scope, this, c"once", &[close, callback])?;
        }
        call_method(scope, this, c"destroy", &[])?;
        Ok(())
    }

    /// <https://nodejs.org/api/zlib.html#zlibreset>
    pub fn reset(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let handle = handle_of(scope, this)?;
        let mut state = handle.data_mut();
        state.codec = Codec::new(state.codec.created, state.codec.level);
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

/// Register the classes of `node:zlib` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    ZlibHandle::add_to_global(scope, global);
    zlib_class::register(scope)?;
    deflate_class::register(scope)?;
    inflate_class::register(scope)?;
    gzip_class::register(scope)?;
    gunzip_class::register(scope)?;
    deflate_raw_class::register(scope)?;
    inflate_raw_class::register(scope)?;
    unzip_class::register(scope)?;
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

    pub use super::deflate_class as Deflate;
    pub use super::deflate_raw_class as DeflateRaw;
    pub use super::gunzip_class as Gunzip;
    pub use super::gzip_class as Gzip;
    pub use super::inflate_class as Inflate;
    pub use super::inflate_raw_class as InflateRaw;
    pub use super::unzip_class as Unzip;

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
