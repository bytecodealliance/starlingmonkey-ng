// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Compressors and decompressors for the deflate, zlib, gzip, Brotli and zstd formats, which take
//! their input in pieces and limit the output each call produces.
//!
//! The codecs use the `flate2` crate's zlib engine, the `brotli` crate and the `ruzstd` crate.
//! `ruzstd` compresses at one level, which is close to zstd's level 1. The zstd compressors and
//! decompressors produce their output once their input ends, so flushes have no effect on them.
//!
//! Errors have the messages, code names and numbers of Node.js's zlib errors.

use brotli::enc::encode::{
    BrotliEncoderOperation, BrotliEncoderParameter, BrotliEncoderStateStruct,
};
use brotli::enc::StandardAlloc;
use brotli::{BrotliDecompressStream, BrotliResult, BrotliState};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

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

/// The size of the output buffer each step of the engine fills.
const CHUNK: usize = 16 * 1024;

/// A compression format, or the decompression of one. `Unzip` decompresses gzip or zlib,
/// whichever the input starts with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
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

impl Format {
    pub fn decompresses(self) -> bool {
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

/// How much of the compressed data a call to [`Codec::process`] produces.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Flush {
    /// Only what the engine doesn't buffer.
    #[default]
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
pub struct ZlibError {
    pub message: String,
    pub code: &'static str,
    pub errno: Option<i32>,
}

impl ZlibError {
    fn data(message: Option<&str>) -> Self {
        ZlibError {
            message: message.unwrap_or("invalid data").to_string(),
            code: "Z_DATA_ERROR",
            errno: Some(-3),
        }
    }

    fn stream(message: Option<&str>) -> Self {
        ZlibError {
            message: message.unwrap_or("stream error").to_string(),
            code: "Z_STREAM_ERROR",
            errno: Some(-2),
        }
    }

    /// The error of a decompression that failed with `error`: `Z_NEED_DICT` for data that needs a
    /// dictionary, and a [`ZlibError::data`] error otherwise.
    fn decompress(error: &flate2::DecompressError) -> Self {
        if error.needs_dictionary().is_some() {
            return ZlibError {
                message: "Missing dictionary".into(),
                code: "Z_NEED_DICT",
                errno: Some(2),
            };
        }
        ZlibError::data(error.message())
    }

    fn unexpected_end() -> Self {
        ZlibError {
            message: "unexpected end of file".into(),
            code: "Z_BUF_ERROR",
            errno: Some(-5),
        }
    }

    /// The error of input after the end of a compressed stream.
    pub fn trailing_data() -> Self {
        ZlibError {
            message: "Trailing junk found after the end of the compressed stream".into(),
            code: "ERR_TRAILING_JUNK_AFTER_STREAM_END",
            errno: None,
        }
    }

    /// Whether the codec appended all output of its input before failing, which it does for
    /// [`ZlibError::trailing_data`] and for the end of input in the middle of a compressed stream.
    pub fn follows_output(&self) -> bool {
        matches!(
            self.code,
            "Z_BUF_ERROR" | "ERR_TRAILING_JUNK_AFTER_STREAM_END"
        )
    }

    pub fn too_large(message: String) -> Self {
        ZlibError {
            message,
            code: "ERR_BUFFER_TOO_LARGE",
            errno: None,
        }
    }

    /// The `RangeError` without a code of an output buffer that can't be allocated.
    fn out_of_memory() -> Self {
        ZlibError {
            message: "Array buffer allocation failed".into(),
            code: "",
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
pub struct Params {
    /// The zlib compression level, -1 for the default.
    pub level: i32,
    /// The Brotli parameters, as `BROTLI_PARAM_*` keys and their values.
    pub brotli: Vec<(u32, u32)>,
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
    /// The zstd input, which is compressed as one frame once it ends, and then the frame and the
    /// count of its bytes produced.
    ZstdCompress {
        pending: Vec<u8>,
        frame: Option<(Vec<u8>, usize)>,
    },
    /// The zstd input, which is decompressed once it ends, and then the decoder of its frame.
    ZstdDecompress {
        pending: Vec<u8>,
        frame: Option<Box<ZstdFrame>>,
    },
}

/// A compressor or decompressor that takes its input in pieces.
pub struct Codec {
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
    /// Whether input after the end of the compressed stream is an error, and a gzip stream has
    /// one member.
    single_stream: bool,
}

impl Default for Codec {
    fn default() -> Self {
        Codec::closed(Format::Deflate, Params::default())
    }
}

impl Codec {
    pub fn new(format: Format, params: &Params) -> Self {
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
            Format::ZstdCompress => Some(Engine::ZstdCompress {
                pending: Vec::new(),
                frame: None,
            }),
            Format::ZstdDecompress => Some(Engine::ZstdDecompress {
                pending: Vec::new(),
                frame: None,
            }),
        };
        Codec {
            format,
            created: format,
            params: params.clone(),
            engine,
            held: Vec::new(),
            ended: false,
            single_stream: false,
        }
    }

    /// A codec of `format` for which input after the end of the compressed stream is an error
    /// that [`ZlibError::trailing_data`] describes, and whose gzip decompressor takes one member.
    pub fn new_single_stream(format: Format, params: &Params) -> Self {
        Codec {
            single_stream: true,
            ..Codec::new(format, params)
        }
    }

    /// A codec without an engine, which holds no compression state.
    pub fn closed(format: Format, params: Params) -> Self {
        Codec {
            format,
            created: format,
            params,
            engine: None,
            held: Vec::new(),
            ended: false,
            single_stream: false,
        }
    }

    /// The format, which an `Unzip` codec replaces with the format it detects.
    pub fn format(&self) -> Format {
        self.format
    }

    /// The format the codec was created for.
    pub fn created(&self) -> Format {
        self.created
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Set the zlib compression level of the codec and its `params`. A zlib compressor keeps its
    /// level, and `params` keep theirs, when `flate2` fails to change the level.
    pub fn set_level(&mut self, level: i32) {
        let changed = match self.engine.as_mut() {
            Some(Engine::Compress(engine)) => engine.set_level(compression(level)).is_ok(),
            _ => true,
        };
        if changed {
            self.params.level = level;
        }
    }

    /// Process `input` with `flush`, and append at most `limit` bytes of output to `output`. With
    /// `Flush::Finish`, a decompressor that hasn't reached the end of a compressed stream fails
    /// once it produced all output. Decompressors ignore the other flushes.
    ///
    /// A codec from [`Codec::new_single_stream`] fails with [`ZlibError::trailing_data`] when
    /// input follows the end of the compressed stream, after appending the output of the input
    /// before it.
    pub fn process(
        &mut self,
        input: &[u8],
        flush: Flush,
        limit: usize,
        output: &mut Vec<u8>,
    ) -> Result<Progress, ZlibError> {
        let finish = flush == Flush::Finish;
        let start = output.len();
        if self.single_stream && self.ended && !input.is_empty() {
            return Err(ZlibError::trailing_data());
        }
        if self.engine.is_none() {
            let mut head = self.held.clone();
            head.extend(input.iter().take(2));
            if head.len() < 2 && !finish {
                self.held.extend_from_slice(input);
                return Ok(Progress::done(input.len()));
            }
            let format = if head.starts_with(&[0x1f, 0x8b]) {
                Format::Gunzip
            } else {
                Format::Inflate
            };
            // At most one byte is held, which is too short for a header and produces no output.
            let held = std::mem::take(&mut self.held);
            *self = Codec {
                created: self.created,
                single_stream: self.single_stream,
                ..Codec::new(format, &self.params)
            };
            self.decompress(&held, limit, output)?;
        }
        let room = limit.saturating_sub(output.len() - start);
        let progress = if let Some(Engine::Decompress(_)) = self.engine {
            self.decompress(input, room, output)?
        } else {
            match self.engine.as_mut() {
                // A flush before the first input produces nothing, and leaves the engine able to
                // change its level.
                Some(Engine::Compress(engine))
                    if input.is_empty() && engine.total_in() == 0 && !finish =>
                {
                    Progress::done(0)
                }
                Some(Engine::Compress(engine)) => deflate(engine, input, flush, room, output)?,
                Some(Engine::BrotliCompress(state)) => {
                    brotli_compress(state, input, flush, room, output)?
                }
                Some(Engine::BrotliDecompress(_)) if self.ended => Progress::done(input.len()),
                Some(Engine::BrotliDecompress(state)) => {
                    let (progress, ended) = brotli_decompress(state, input, room, output)?;
                    self.ended = ended;
                    progress
                }
                Some(Engine::ZstdCompress { pending, frame }) => {
                    if frame.is_none() {
                        hold(pending, input)?;
                        if !finish {
                            return Ok(Progress::done(input.len()));
                        }
                        let compressed = ruzstd::encoding::compress_to_vec(
                            pending.as_slice(),
                            ruzstd::encoding::CompressionLevel::Fastest,
                        );
                        *pending = Vec::new();
                        *frame = Some((compressed, 0));
                    }
                    let (compressed, read) = frame.as_mut().expect("compressed above");
                    let count = room.min(compressed.len() - *read);
                    hold(output, &compressed[*read..*read + count])?;
                    *read += count;
                    Progress {
                        consumed: input.len(),
                        done: *read == compressed.len(),
                    }
                }
                Some(Engine::ZstdDecompress { .. }) if self.ended => Progress::done(input.len()),
                Some(Engine::ZstdDecompress { pending, frame }) => {
                    if frame.is_none() {
                        hold(pending, input)?;
                        if !finish {
                            return Ok(Progress::done(input.len()));
                        }
                        *frame = Some(zstd_frame(pending)?);
                    }
                    let frame = frame.as_mut().expect("created above");
                    let done = zstd_decompress(frame, pending, room, output)?;
                    self.ended = done;
                    Progress {
                        consumed: input.len(),
                        done,
                    }
                }
                Some(Engine::Decompress(_)) | None => {
                    unreachable!("the engine was created above")
                }
            }
        };
        if finish && progress.done && !self.ended && self.format.decompresses() {
            return Err(ZlibError::unexpected_end());
        }
        if self.single_stream && self.ended && progress.consumed < input.len() {
            return Err(ZlibError::trailing_data());
        }
        Ok(progress)
    }

    /// Decompress `input` into at most `room` bytes of `output`. After the end of a gzip member, a
    /// non-zero byte starts another member, and a zero byte ends the input. Bytes after the end of
    /// other formats' streams are ignored.
    fn decompress(
        &mut self,
        input: &[u8],
        mut room: usize,
        output: &mut Vec<u8>,
    ) -> Result<Progress, ZlibError> {
        let mut consumed = 0;
        loop {
            if self.ended {
                let rest = &input[consumed..];
                if self.format != Format::Gunzip
                    || self.single_stream
                    || rest.first().is_none_or(|&byte| byte == 0)
                {
                    return Ok(Progress::done(consumed));
                }
                *self = Codec {
                    created: self.created,
                    single_stream: self.single_stream,
                    ..Codec::new(Format::Gunzip, &self.params)
                };
            }
            if room == 0 {
                return Ok(Progress::partial(consumed));
            }
            let Some(Engine::Decompress(engine)) = self.engine.as_mut() else {
                unreachable!("decompressing formats have a decompressor");
            };
            let start = grow(output, room)?;
            let space = output.len() - start;
            let (before_in, before_out) = (engine.total_in(), engine.total_out());
            let status = engine.decompress(
                &input[consumed..],
                &mut output[start..],
                FlushDecompress::None,
            );
            let produced = (engine.total_out() - before_out) as usize;
            let taken = (engine.total_in() - before_in) as usize;
            output.truncate(start + produced);
            let status = status.map_err(|error| ZlibError::decompress(&error))?;
            consumed += taken;
            room -= produced;
            if status == Status::StreamEnd {
                self.ended = true;
            } else if (produced < space && consumed == input.len()) || (produced == 0 && taken == 0)
            {
                return Ok(Progress::done(consumed));
            }
        }
    }
}

/// What a call to [`Codec::process`] did: the count of input bytes the engine took, and whether
/// it produced all the output of the input and the flush. When it didn't, the output reached the
/// limit, and a call with the rest of the input and the same flush continues.
#[derive(Clone, Copy)]
pub struct Progress {
    pub consumed: usize,
    pub done: bool,
}

impl Progress {
    fn done(consumed: usize) -> Self {
        Progress {
            consumed,
            done: true,
        }
    }

    fn partial(consumed: usize) -> Self {
        Progress {
            consumed,
            done: false,
        }
    }
}

/// Append zero bytes to `output` for an engine to write into, as many as `room` allows but at most
/// [`CHUNK`], and return where they start. Fails if allocating them fails.
fn grow(output: &mut Vec<u8>, room: usize) -> Result<usize, ZlibError> {
    let length = room.min(CHUNK);
    output
        .try_reserve(length)
        .map_err(|_| ZlibError::out_of_memory())?;
    let start = output.len();
    output.resize(start + length, 0);
    Ok(start)
}

/// Append `bytes` to `buffer`, or fail if allocating the room fails.
fn hold(buffer: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ZlibError> {
    buffer
        .try_reserve(bytes.len())
        .map_err(|_| ZlibError::out_of_memory())?;
    buffer.extend_from_slice(bytes);
    Ok(())
}

/// Compress `input` with the zlib engine `engine` and `flush` into at most `room` bytes of
/// `output`.
fn deflate(
    engine: &mut Compress,
    input: &[u8],
    flush: Flush,
    mut room: usize,
    output: &mut Vec<u8>,
) -> Result<Progress, ZlibError> {
    let flush = match flush {
        Flush::None => FlushCompress::None,
        Flush::Sync => FlushCompress::Sync,
        Flush::Full => FlushCompress::Full,
        Flush::Finish => FlushCompress::Finish,
    };
    let mut consumed = 0;
    loop {
        if room == 0 {
            return Ok(Progress::partial(consumed));
        }
        let start = grow(output, room)?;
        let space = output.len() - start;
        let (before_in, before_out) = (engine.total_in(), engine.total_out());
        let status = engine.compress(&input[consumed..], &mut output[start..], flush);
        let produced = (engine.total_out() - before_out) as usize;
        output.truncate(start + produced);
        let status = status.map_err(|error| ZlibError::stream(error.message()))?;
        consumed += (engine.total_in() - before_in) as usize;
        room -= produced;
        // The engine is done once it consumed all input and left output space unused.
        if status == Status::StreamEnd || (consumed == input.len() && produced < space) {
            return Ok(Progress::done(consumed));
        }
    }
}

/// Compress `input` with the Brotli encoder `state` into at most `room` bytes of `output`, with
/// the operation `flush` selects: `BROTLI_OPERATION_PROCESS` for `Flush::None`,
/// `BROTLI_OPERATION_FINISH` for `Flush::Finish`, and `BROTLI_OPERATION_FLUSH` otherwise.
fn brotli_compress(
    state: &mut BrotliEncoderStateStruct<StandardAlloc>,
    mut input: &[u8],
    flush: Flush,
    mut room: usize,
    output: &mut Vec<u8>,
) -> Result<Progress, ZlibError> {
    let operation = match flush {
        Flush::None if input.is_empty() => return Ok(Progress::done(0)),
        Flush::None => BrotliEncoderOperation::BROTLI_OPERATION_PROCESS,
        Flush::Sync | Flush::Full => BrotliEncoderOperation::BROTLI_OPERATION_FLUSH,
        Flush::Finish => BrotliEncoderOperation::BROTLI_OPERATION_FINISH,
    };
    let finish = flush == Flush::Finish;
    let total = input.len();
    loop {
        if room == 0 {
            return Ok(Progress::partial(total - input.len()));
        }
        let start = grow(output, room)?;
        let mut available_in = input.len();
        let mut input_offset = 0;
        let mut available_out = output.len() - start;
        let mut output_offset = 0;
        let compressed = state.compress_stream(
            operation,
            &mut available_in,
            input,
            &mut input_offset,
            &mut available_out,
            &mut output[start..],
            &mut output_offset,
            &mut None,
            &mut |_, _, _, _| (),
        );
        output.truncate(start + output_offset);
        input = &input[input_offset..];
        room -= output_offset;
        if !compressed {
            return Err(ZlibError::data(Some("Compression failed")));
        }
        let done = if finish {
            state.is_finished()
        } else {
            input.is_empty() && !state.has_more_output()
        };
        if done {
            return Ok(Progress::done(total - input.len()));
        }
    }
}

/// Decompress `input` with the Brotli decoder `state` into at most `room` bytes of `output`.
/// Returns the progress and whether the compressed stream ended. Bytes after its end are ignored.
fn brotli_decompress(
    state: &mut BrotliDecoder,
    mut input: &[u8],
    mut room: usize,
    output: &mut Vec<u8>,
) -> Result<(Progress, bool), ZlibError> {
    let total = input.len();
    loop {
        if room == 0 {
            return Ok((Progress::partial(total - input.len()), false));
        }
        let start = grow(output, room)?;
        let mut available_in = input.len();
        let mut input_offset = 0;
        let mut available_out = output.len() - start;
        let mut output_offset = 0;
        let mut total_out = 0;
        let result = BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            input,
            &mut available_out,
            &mut output_offset,
            &mut output[start..],
            &mut total_out,
            state,
        );
        output.truncate(start + output_offset);
        input = &input[input_offset..];
        room -= output_offset;
        match result {
            BrotliResult::ResultSuccess => return Ok((Progress::done(total - input.len()), true)),
            BrotliResult::NeedsMoreInput => {
                return Ok((Progress::done(total - input.len()), false));
            }
            BrotliResult::NeedsMoreOutput => {}
            BrotliResult::ResultFailure => {
                return Err(ZlibError::brotli(state.error_code as i32));
            }
        }
    }
}

/// The zstd frame a decompressor decodes: the decoder, the count of input bytes it read, whether
/// the frame header declares the size of the content, and the count of bytes produced.
struct ZstdFrame {
    decoder: FrameDecoder,
    read: usize,
    declares_size: bool,
    produced: u64,
}

/// The decoder of the first zstd frame in `input`, as Node.js decodes it. Skippable frames before
/// it are skipped.
fn zstd_frame(input: &[u8]) -> Result<Box<ZstdFrame>, ZlibError> {
    let mut rest = input;
    let mut decoder = FrameDecoder::new();
    loop {
        if rest.is_empty() {
            return Err(ZlibError::unexpected_end());
        }
        // A frame starts with the magic number 0xFD2FB528, and a skippable frame with one from
        // 0x184D2A50 to 0x184D2A5F, both little-endian.
        let head = &rest[..rest.len().min(4)];
        let frame = head == &[0x28, 0xb5, 0x2f, 0xfd][..head.len()];
        let skippable = head[0] & 0xf0 == 0x50 && head[1..] == [0x2a, 0x4d, 0x18][..head.len() - 1];
        if !frame && !skippable {
            return Err(ZlibError::zstd_code(ZSTD_PREFIX_UNKNOWN));
        }
        // The frame header descriptor's `Frame_Content_Size_flag` and `Single_Segment_flag`
        // tell whether the header holds the size of the frame's content.
        let descriptor = rest.get(4).copied().unwrap_or(0);
        let declares_size = frame && (descriptor >> 6 != 0 || descriptor & 0x20 != 0);
        // Its reserved bit must be 0.
        if frame && descriptor & 0x08 != 0 {
            return Err(ZlibError::zstd_code(ZSTD_FRAME_PARAMETER_UNSUPPORTED));
        }
        match decoder.init(&mut rest) {
            Ok(()) => {
                return Ok(Box::new(ZstdFrame {
                    decoder,
                    read: input.len() - rest.len(),
                    declares_size,
                    produced: 0,
                }))
            }
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length,
                ..
            })) => {
                rest = rest
                    .get(length as usize..)
                    .ok_or_else(ZlibError::unexpected_end)?;
            }
            Err(error) => return Err(ZlibError::zstd(&error)),
        }
    }
}

/// Decode `frame` of `input` into at most `room` bytes of `output`. Returns whether the frame
/// ended. The bytes after it are ignored.
fn zstd_decompress(
    frame: &mut ZstdFrame,
    input: &[u8],
    mut room: usize,
    output: &mut Vec<u8>,
) -> Result<bool, ZlibError> {
    loop {
        if room == 0 {
            return Ok(false);
        }
        let available = frame.decoder.can_collect();
        if available > 0 {
            let start = grow(output, room.min(available))?;
            let read = std::io::Read::read(&mut frame.decoder, &mut output[start..]);
            let count = read.as_ref().copied().unwrap_or(0);
            output.truncate(start + count);
            read.map_err(|_| ZlibError::zstd_code(ZSTD_CORRUPTION_DETECTED))?;
            room -= count;
            frame.produced += count as u64;
            continue;
        }
        if frame.decoder.is_finished() {
            if frame.declares_size && frame.produced != frame.decoder.content_size() {
                return Err(ZlibError::zstd_code(ZSTD_CORRUPTION_DETECTED));
            }
            if let (Some(expected), Some(actual)) = (
                frame.decoder.get_checksum_from_data(),
                frame.decoder.get_calculated_checksum(),
            ) {
                if expected != actual {
                    return Err(ZlibError::zstd_code(ZSTD_CHECKSUM_WRONG));
                }
            }
            return Ok(true);
        }
        let mut source = &input[frame.read..];
        frame
            .decoder
            .decode_blocks(&mut source, BlockDecodingStrategy::UptoBytes(CHUNK))
            .map_err(|error| ZlibError::zstd(&error))?;
        frame.read = input.len() - source.len();
    }
}
