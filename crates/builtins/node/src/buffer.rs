// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:buffer` and the `Buffer` global. See <https://nodejs.org/api/buffer.html>.
//!
//! A `Buffer` is a `Uint8Array` whose prototype is `Buffer.prototype`, which inherits from
//! `Uint8Array.prototype`. `Buffer`'s methods also accept plain `Uint8Array`s as `this`, as Node's
//! do. `Buffer(...)` and `new Buffer(...)` are the deprecated forms of `Buffer.from` and
//! `Buffer.alloc`.

use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::string::StrChars as Chars;
use js::ArrayBufferView;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};

/// The largest `Buffer` the runtime creates, `buffer.constants.MAX_LENGTH`.
pub const MAX_LENGTH: f64 = 2147483647.0;

/// The longest string the engine can create, `buffer.constants.MAX_STRING_LENGTH`.
pub const MAX_STRING_LENGTH: f64 = 1073741822.0;

// ============================================================================
// Encodings
// ============================================================================

/// A character encoding `Buffer` converts strings with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Utf8,
    Utf16le,
    Latin1,
    Ascii,
    Base64,
    Base64Url,
    Hex,
}

impl Encoding {
    /// The encoding `name` names, case-insensitively, as Node accepts it.
    pub fn parse(name: &str) -> Option<Self> {
        const NAMES: [(&str, Encoding); 12] = [
            ("utf8", Encoding::Utf8),
            ("utf-8", Encoding::Utf8),
            ("ucs2", Encoding::Utf16le),
            ("ucs-2", Encoding::Utf16le),
            ("utf16le", Encoding::Utf16le),
            ("utf-16le", Encoding::Utf16le),
            ("latin1", Encoding::Latin1),
            ("binary", Encoding::Latin1),
            ("ascii", Encoding::Ascii),
            ("base64", Encoding::Base64),
            ("base64url", Encoding::Base64Url),
            ("hex", Encoding::Hex),
        ];
        NAMES
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, encoding)| *encoding)
    }
}

/// Whether ICU's conversion callbacks drop `code` instead of substituting it, ICU's
/// `IS_DEFAULT_IGNORABLE_CODE_POINT`.
fn is_default_ignorable(code: u32) -> bool {
    matches!(
        code,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F
            | 0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFF8
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

/// The value of base64 digit `c` in either alphabet, or `None` if it isn't one.
fn base64_digit(c: u8) -> Option<u8> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        _ => return None,
    })
}

/// Decode base64 or base64url `chars` as Node does: characters outside both alphabets are skipped,
/// decoding stops at the first `=`, and missing padding is allowed.
fn decode_base64(chars: impl Iterator<Item = u32>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0;
    for c in chars {
        if c == u32::from(b'=') {
            break;
        }
        let Some(digit) = u8::try_from(c).ok().and_then(base64_digit) else {
            continue;
        };
        accumulator = (accumulator << 6) | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    out
}

/// Decode hex `chars` as Node does: pairs of hex digits, up to the first pair that isn't one.
fn decode_hex(chars: impl Iterator<Item = u32>) -> Vec<u8> {
    let digits: Vec<u32> = chars.collect();
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.as_chunks::<2>().0 {
        let high = char::from_u32(pair[0]).and_then(|c| c.to_digit(16));
        let low = char::from_u32(pair[1]).and_then(|c| c.to_digit(16));
        match (high, low) {
            (Some(high), Some(low)) => out.push((high * 16 + low) as u8),
            _ => break,
        }
    }
    out
}

/// The bytes `string` encodes to in `encoding`.
pub fn encode_string(
    scope: &Scope<'_>,
    string: js::JSString<'_>,
    encoding: Encoding,
) -> Result<Vec<u8>, ExnThrown> {
    if encoding == Encoding::Utf8 {
        return Ok(string.to_utf8(scope)?.into_bytes());
    }
    // SAFETY: the closure makes no call into SpiderMonkey.
    let bytes = unsafe {
        string.with_chars(scope, |chars| match (encoding, chars) {
            (Encoding::Latin1 | Encoding::Ascii, Chars::Latin1(bytes)) => bytes.to_vec(),
            (Encoding::Latin1 | Encoding::Ascii, Chars::TwoByte(units)) => {
                units.iter().map(|unit| *unit as u8).collect()
            }
            (Encoding::Utf16le, Chars::Latin1(bytes)) => {
                bytes.iter().flat_map(|byte| [*byte, 0]).collect()
            }
            (Encoding::Utf16le, Chars::TwoByte(units)) => {
                units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
            }
            (Encoding::Base64 | Encoding::Base64Url, Chars::Latin1(bytes)) => {
                decode_base64(bytes.iter().map(|byte| u32::from(*byte)))
            }
            // Node.js decodes the low byte of each code unit of a two-byte string.
            (Encoding::Base64 | Encoding::Base64Url, Chars::TwoByte(units)) => {
                decode_base64(units.iter().map(|unit| u32::from(*unit as u8)))
            }
            (Encoding::Hex, Chars::Latin1(bytes)) => {
                decode_hex(bytes.iter().map(|byte| u32::from(*byte)))
            }
            (Encoding::Hex, Chars::TwoByte(units)) => {
                decode_hex(units.iter().map(|unit| u32::from(*unit as u8)))
            }
            (Encoding::Utf8, _) => unreachable!("handled above"),
        })
    }?;
    Ok(bytes)
}

/// The lowercase hexadecimal digits of `bytes`, two per byte.
pub(crate) fn hex_digits(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[usize::from(byte >> 4)],
                DIGITS[usize::from(byte & 0xf)],
            ]
        })
        .collect()
}

/// `bytes` decoded as `encoding` into a JS string.
pub fn decode_bytes<'s>(
    scope: &'s Scope<'_>,
    bytes: &[u8],
    encoding: Encoding,
) -> Result<js::JSString<'s>, ExnThrown> {
    let length = match encoding {
        Encoding::Hex => bytes.len() * 2,
        Encoding::Base64 | Encoding::Base64Url => bytes.len().div_ceil(3) * 4,
        Encoding::Utf16le => bytes.len() / 2,
        Encoding::Utf8 | Encoding::Latin1 | Encoding::Ascii => bytes.len(),
    };
    if length as f64 > MAX_STRING_LENGTH {
        return Err(NodeError::new(
            ErrorKind::Error,
            "ERR_STRING_TOO_LONG",
            format!(
                "Cannot create a string longer than 0x{:x} characters",
                MAX_STRING_LENGTH as u64
            ),
        )
        .throw(scope));
    }
    match encoding {
        Encoding::Utf8 => js::JSString::from_utf8_lossy(scope, bytes),
        Encoding::Latin1 => js::JSString::from_latin1(scope, bytes),
        Encoding::Ascii => {
            let ascii: Vec<u8> = bytes.iter().map(|byte| byte & 0x7f).collect();
            js::JSString::from_latin1(scope, &ascii)
        }
        Encoding::Utf16le => {
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            js::JSString::from_utf16(scope, &units)
        }
        Encoding::Base64 | Encoding::Base64Url => {
            let url = encoding == Encoding::Base64Url;
            let Some(text) = web_globals::base64::encode(bytes, url) else {
                js::exception::report_out_of_memory(scope);
                return Err(ExnThrown);
            };
            js::JSString::from_str(scope, &text)
        }
        Encoding::Hex => js::JSString::from_latin1(scope, &hex_digits(bytes)),
    }
}

// ============================================================================
// Buffer objects
// ============================================================================

/// `Buffer.prototype` in the current realm, as `Buffer` was created with it.
fn buffer_prototype<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    buffer::prototype_object(scope)
}

/// A new `Buffer` holding a copy of `bytes`.
pub fn new_buffer<'s>(scope: &'s Scope<'_>, bytes: &[u8]) -> Result<Object<'s>, ExnThrown> {
    let array = js::Uint8Array::with_data(scope, bytes)?;
    let array: Object<'s> = *array.as_array_buffer_view();
    array.set_prototype(scope, buffer_prototype(scope)?.handle())?;
    Ok(array)
}

/// A new zero-filled `Buffer` of `length` bytes.
fn new_zeroed_buffer<'s>(scope: &'s Scope<'_>, length: usize) -> Result<Object<'s>, ExnThrown> {
    let array = js::Uint8Array::new(scope, length)?;
    let array: Object<'s> = *array.as_array_buffer_view();
    array.set_prototype(scope, buffer_prototype(scope)?.handle())?;
    Ok(array)
}

/// A new `Buffer` of `length` bytes that `fill` writes. `fill` must not call into SpiderMonkey.
pub(crate) fn new_buffer_with<'s>(
    scope: &'s Scope<'_>,
    length: usize,
    fill: impl FnOnce(&mut [u8]) -> Result<(), ExnThrown> + Send,
) -> Result<Object<'s>, ExnThrown> {
    let buffer = new_zeroed_buffer(scope, length)?;
    let view = ArrayBufferView::from_object(buffer).ok_or(ExnThrown)?;
    view.with_bytes_mut(fill)?;
    Ok(buffer)
}

/// A new `Buffer` of the UTF-8 encoding of `string`.
fn utf8_buffer<'s>(
    scope: &'s Scope<'_>,
    string: js::JSString<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let length = string.utf8_length(scope)?;
    let buffer = new_zeroed_buffer(scope, length)?;
    let view = ArrayBufferView::from_object(buffer).ok_or(ExnThrown)?;
    string.write_utf8(scope, view, 0..length)?;
    Ok(buffer)
}

/// A new `Buffer` viewing `length` bytes of `buffer` from `offset`.
fn buffer_view<'s>(
    scope: &'s Scope<'_>,
    buffer: js::ArrayBuffer<'_>,
    offset: usize,
    length: usize,
) -> Result<Object<'s>, ExnThrown> {
    let array = js::Uint8Array::with_buffer(scope, buffer, offset, length)?;
    let array: Object<'s> = *array.as_array_buffer_view();
    array.set_prototype(scope, buffer_prototype(scope)?.handle())?;
    Ok(array)
}

/// `value` as a view of bytes, if it is an `ArrayBufferView`.
fn as_view<'s>(scope: &'s Scope<'_>, value: HandleValue<'_>) -> Option<ArrayBufferView<'s>> {
    Object::from_value(scope, *value)
        .ok()
        .and_then(ArrayBufferView::from_object)
}

/// `this` as a view of bytes, or `ERR_INVALID_ARG_TYPE`.
fn this_view(this: Object<'_>) -> Result<ArrayBufferView<'_>, Thrown> {
    ArrayBufferView::from_object(this).ok_or_else(|| {
        NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_ARG_TYPE",
            "The \"this\" argument must be an instance of Buffer or Uint8Array",
        )
        .into()
    })
}

/// Node.js's `adjustOffset`: a `start` or `end` argument converted to a number, truncated, counted
/// from the end if negative, and clamped into `0..=length`. `NaN` is 0.
fn adjust_offset(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    length: usize,
) -> Result<usize, ExnThrown> {
    let number = to_number(scope, value)?.trunc();
    Ok(if number.is_nan() {
        0
    } else if number < 0.0 {
        (length as f64 + number).max(0.0) as usize
    } else {
        number.min(length as f64) as usize
    })
}

/// `value`, unless it is missing or `undefined`.
fn defined(value: Option<HandleValue<'_>>) -> Option<HandleValue<'_>> {
    value.filter(|value| !value.is_undefined())
}

/// The largest offset Node.js's `validateOffset` accepts by default, `Number.MAX_SAFE_INTEGER`.
const MAX_OFFSET: f64 = crate::errors::MAX_SAFE_INTEGER;

/// `value` converted to a number, as JS's unary `+` converts it.
fn to_number(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, ExnThrown> {
    f64::from_jsval(scope, value, ()).map_err(|_| ExnThrown)
}

/// `number` converted to a byte, as storing it in a `Uint8Array` converts it.
fn to_uint8(number: f64) -> u8 {
    if number.is_finite() {
        number.trunc().rem_euclid(256.0) as u8
    } else {
        0
    }
}

/// Node.js's `toInteger(value, default)`: an integer `value` as it is, other values converted to a
/// number and rounded down, and `default` for `NaN` and numbers outside the safe integer range.
fn to_integer(scope: &Scope<'_>, value: HandleValue<'_>, default: f64) -> Result<f64, ExnThrown> {
    if value.is_number() && value.to_number().fract() == 0.0 {
        return Ok(value.to_number());
    }
    let number = to_number(scope, value)?;
    if number.is_nan() || number.abs() > MAX_OFFSET {
        return Ok(default);
    }
    Ok(number.floor())
}

/// Node.js's `getEncodingOps(encoding)`: the encoding `String(value)` names, case-insensitively.
fn lookup_encoding(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<Encoding>, ExnThrown> {
    let name = js::JSString::from_value(scope, value)?.to_utf8(scope)?;
    Ok(Encoding::parse(&name))
}

/// `ERR_UNKNOWN_ENCODING` for the encoding argument `value`.
pub(crate) fn unknown_encoding(scope: &Scope<'_>, value: HandleValue<'_>) -> Thrown {
    let name = js::JSString::from_value(scope, value)
        .and_then(|name| name.to_utf8(scope))
        .unwrap_or_default();
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_UNKNOWN_ENCODING",
        format!("Unknown encoding: {name}"),
    )
    .into()
}

/// The encoding a falsy or `"utf8"` `value` selects, UTF-8, or the one [`lookup_encoding`] finds,
/// or `ERR_UNKNOWN_ENCODING`.
fn encoding_or_utf8(scope: &Scope<'_>, value: Option<HandleValue<'_>>) -> Result<Encoding, Thrown> {
    match value.filter(|value| js::conversion::to_boolean(*value)) {
        None => Ok(Encoding::Utf8),
        Some(value) => lookup_encoding(scope, value)?.ok_or_else(|| unknown_encoding(scope, value)),
    }
}

/// `ERR_BUFFER_OUT_OF_BOUNDS`, naming the argument `name` if given.
fn out_of_bounds(name: Option<&str>) -> Thrown {
    let message = match name {
        Some(name) => format!("\"{name}\" is outside of buffer bounds"),
        None => "Attempt to access memory outside buffer bounds".into(),
    };
    NodeError::new(ErrorKind::RangeError, "ERR_BUFFER_OUT_OF_BOUNDS", message).into()
}

/// `value` as a `Uint8Array`, including a `Buffer`, or `ERR_INVALID_ARG_TYPE` naming the argument
/// `name`.
fn uint8_array_arg<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<ArrayBufferView<'s>, Thrown> {
    as_view(scope, value)
        .filter(|view| view.view_kind() == js::typedarray::ViewKind::Uint8)
        .ok_or_else(|| {
            NodeError::invalid_arg_type(scope, name, "an instance of Buffer or Uint8Array", value)
                .into()
        })
}

/// Whether `object` has `Buffer.prototype` on its prototype chain.
fn is_buffer(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    let proto = buffer_prototype(scope)?;
    let mut current = object.get_prototype(scope)?;
    while let Some(candidate) = current {
        if candidate.as_raw() == proto.as_raw() {
            return Ok(true);
        }
        current = candidate.get_prototype(scope)?;
    }
    Ok(false)
}

/// nbytes's `SearchString`: the index of the first occurrence of `needle` in `haystack` at or after
/// `start`, or with `forward` false, of the last occurrence at or before it. `utf16` restricts
/// matches to even positions.
fn search_bytes(
    haystack: &[u8],
    needle: &[u8],
    start: usize,
    forward: bool,
    utf16: bool,
) -> Option<usize> {
    let aligned = |at: &usize| !utf16 || at.is_multiple_of(2);
    if forward {
        let start = start.min(haystack.len());
        memchr::memmem::find_iter(&haystack[start..], needle)
            .map(|at| start + at)
            .find(aligned)
    } else {
        let end = start.saturating_add(needle.len()).min(haystack.len());
        memchr::memmem::rfind_iter(&haystack[..end], needle).find(aligned)
    }
}

/// `bytes` as little-endian UTF-16 code units, without a trailing odd byte.
fn utf16_units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect()
}

/// Node.js's `IndexOfOffset`: the position a search for a needle of `needle_length` bytes starts
/// at in a haystack of `length` bytes for the `offset` argument, or `-1` if nothing can match.
fn index_of_offset(length: usize, offset: i64, needle_length: usize, forward: bool) -> i64 {
    let length = length as i64;
    let needle_length = needle_length as i64;
    if offset < 0 {
        if offset + length >= 0 {
            length + offset
        } else if forward || needle_length == 0 {
            0
        } else {
            -1
        }
    } else if offset + needle_length <= length {
        offset
    } else if needle_length == 0 {
        length
    } else if forward {
        -1
    } else {
        length - 1
    }
}

/// What `indexOf` and its relatives search for.
enum Needle {
    /// A byte, from a number argument.
    Byte(u8),
    /// The bytes of a string in one of the encodings searched as text, or of a `Uint8Array` or a
    /// string in another encoding. UTF-16 needles match at even positions only.
    Bytes {
        bytes: Vec<u8>,
        utf16: bool,
        text: bool,
    },
}

/// Node.js's `IndexOfString`, `IndexOfBuffer` and `IndexOfNumber`: the position of `needle` in
/// `haystack[..end]`, searching from `offset`, or `-1`.
fn search_haystack(haystack: &[u8], needle: &Needle, offset: i64, end: i64, forward: bool) -> i64 {
    let (bytes, utf16, text) = match needle {
        Needle::Byte(byte) => {
            let start = index_of_offset(haystack.len(), offset, 1, forward);
            if start <= -1 || haystack.is_empty() {
                return -1;
            }
            let start = start as usize;
            let search_end = end.clamp(0, haystack.len() as i64) as usize;
            let found = if forward {
                if start >= search_end {
                    return -1;
                }
                haystack[start..search_end]
                    .iter()
                    .position(|candidate| candidate == byte)
                    .map(|index| start + index)
            } else {
                haystack[..(start + 1).min(search_end)]
                    .iter()
                    .rposition(|candidate| candidate == byte)
            };
            return found.map_or(-1, |index| index as i64);
        }
        Needle::Bytes { bytes, utf16, text } => (bytes, *utf16, *text),
    };
    let haystack_length = if utf16 && text {
        haystack.len() & !1
    } else {
        haystack.len()
    };
    let mut search_end = end.clamp(0, haystack_length as i64) as usize;
    if utf16 {
        search_end &= !1;
    }
    let start = index_of_offset(haystack_length, offset, bytes.len(), forward);
    if bytes.is_empty() {
        return start.min(search_end as i64);
    }
    if haystack_length == 0 || start <= -1 {
        return -1;
    }
    let mut start = start as usize;
    if start >= search_end {
        if forward || search_end == 0 {
            return -1;
        }
        start = search_end - 1;
    }
    if (forward && bytes.len() + start > search_end) || bytes.len() > search_end {
        return -1;
    }
    let found = if utf16 {
        if search_end < 2 || bytes.len() < 2 {
            return -1;
        }
        let needle = &bytes[..bytes.len() & !1];
        search_bytes(&haystack[..search_end], needle, start & !1, forward, true)
    } else {
        search_bytes(&haystack[..search_end], bytes, start, forward, false)
    };
    found.map_or(-1, |index| index as i64)
}

/// The shared body of `indexOf`, `lastIndexOf` and `includes`: Node.js's `bidirectionalIndexOf`.
/// `end` may be the encoding instead, and so may `byte_offset`.
fn index_of(
    scope: &Scope<'_>,
    this: Object<'_>,
    value: HandleValue<'_>,
    byte_offset: Option<HandleValue<'_>>,
    end: Option<HandleValue<'_>>,
    encoding: Option<HandleValue<'_>>,
    forward: bool,
) -> Result<i64, Thrown> {
    let view = this_view(this)?;
    let length = view.byte_length();
    let (end, mut encoding) = match end.filter(|end| !end.is_undefined()) {
        Some(end) if end.is_string() => (length as f64, Some(end)),
        Some(end) => (to_number(scope, end)?, encoding),
        None => (length as f64, encoding),
    };
    let mut offset = f64::NAN;
    if let Some(byte_offset) = byte_offset {
        if byte_offset.is_string() {
            encoding = Some(byte_offset);
        } else {
            offset = to_number(scope, byte_offset)?.clamp(-2147483648.0, 2147483647.0);
        }
    }
    if offset.is_nan() {
        offset = if forward { 0.0 } else { length as f64 };
    }
    let integer = |number: f64| {
        if number.is_nan() {
            0
        } else {
            number.trunc() as i64
        }
    };
    let (offset, end) = (integer(offset), integer(end));
    if value.is_number() {
        let needle = Needle::Byte(to_uint8(value.to_number()));
        return Ok(
            view.with_bytes(|haystack| search_haystack(haystack, &needle, offset, end, forward))
        );
    }
    let encoding_value = encoding.filter(|encoding| !encoding.is_undefined());
    let found = match encoding_value {
        None => Some(Encoding::Utf8),
        Some(encoding) => lookup_encoding(scope, encoding)?,
    };
    let needle = if value.is_string() {
        let Some(found) = found else {
            return Err(unknown_encoding(
                scope,
                encoding_value.expect("set if not found"),
            ));
        };
        let string = js::JSString::from_value(scope, value)?;
        let text = matches!(found, Encoding::Utf8 | Encoding::Utf16le | Encoding::Latin1);
        let encoded = encode_string(scope, string, found)?;
        Needle::Bytes {
            bytes: encoded,
            utf16: found == Encoding::Utf16le,
            text,
        }
    } else if let Ok(view) = uint8_array_arg(scope, value, "value") {
        Needle::Bytes {
            bytes: view.copy_bytes(scope)?,
            utf16: found == Some(Encoding::Utf16le),
            text: false,
        }
    } else {
        return Err(NodeError::invalid_arg_type(
            scope,
            "value",
            "one of type number or string or an instance of Buffer or Uint8Array",
            value,
        )
        .into());
    };
    Ok(view.with_bytes(|haystack| search_haystack(haystack, &needle, offset, end, forward)))
}

// ============================================================================
// Number reads and writes
// ============================================================================

/// The number formats of the `read*` and `write*` methods.
#[derive(Clone, Copy)]
enum NumberFormat {
    U8,
    I8,
    U16(Endian),
    I16(Endian),
    U32(Endian),
    I32(Endian),
    F32(Endian),
    F64(Endian),
    BigU64(Endian),
    BigI64(Endian),
    /// An unsigned integer of 1 to 6 bytes, the width given as an argument.
    UVar(Endian),
    /// A signed integer of 1 to 6 bytes, the width given as an argument.
    IVar(Endian),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

impl NumberFormat {
    /// The format's width in bytes, for the fixed-width ones.
    fn width(self) -> Option<usize> {
        match self {
            NumberFormat::U8 | NumberFormat::I8 => Some(1),
            NumberFormat::U16(_) | NumberFormat::I16(_) => Some(2),
            NumberFormat::U32(_) | NumberFormat::I32(_) | NumberFormat::F32(_) => Some(4),
            NumberFormat::F64(_) | NumberFormat::BigU64(_) | NumberFormat::BigI64(_) => Some(8),
            NumberFormat::UVar(_) | NumberFormat::IVar(_) => None,
        }
    }

    fn endian(self) -> Endian {
        match self {
            NumberFormat::U8 | NumberFormat::I8 => Endian::Little,
            NumberFormat::U16(e)
            | NumberFormat::I16(e)
            | NumberFormat::U32(e)
            | NumberFormat::I32(e)
            | NumberFormat::F32(e)
            | NumberFormat::F64(e)
            | NumberFormat::BigU64(e)
            | NumberFormat::BigI64(e)
            | NumberFormat::UVar(e)
            | NumberFormat::IVar(e) => e,
        }
    }
}

/// `bytes` in `endian` order as an integer, sign-extended if `signed`.
fn read_integer(bytes: &[u8], endian: Endian, signed: bool) -> i128 {
    let mut value: i128 = 0;
    let mut push = |byte: &u8| value = (value << 8) | i128::from(*byte);
    match endian {
        Endian::Little => bytes.iter().rev().for_each(&mut push),
        Endian::Big => bytes.iter().for_each(&mut push),
    }
    if signed && !bytes.is_empty() {
        let bits = bytes.len() * 8;
        if value & (1 << (bits - 1)) != 0 {
            value -= 1 << bits;
        }
    }
    value
}

/// The low `width` bytes of `value` in `endian` order, at the start of the array.
fn integer_bytes(value: i128, width: usize, endian: Endian) -> [u8; 8] {
    let mut bytes = [0; 8];
    for (index, byte) in bytes[..width].iter_mut().enumerate() {
        let shift = match endian {
            Endian::Little => index,
            Endian::Big => width - 1 - index,
        };
        *byte = (value >> (8 * shift)) as u8;
    }
    bytes
}

/// One `read*` or `write*` method.
struct NumberMethod {
    name: &'static CStr,
    format: NumberFormat,
    write: bool,
}

impl NumberMethod {
    fn length(&self) -> u32 {
        match (self.write, self.format) {
            (false, NumberFormat::UVar(_) | NumberFormat::IVar(_)) => 2,
            (true, NumberFormat::UVar(_) | NumberFormat::IVar(_)) => 3,
            (
                false,
                NumberFormat::F32(_)
                | NumberFormat::F64(_)
                | NumberFormat::BigU64(_)
                | NumberFormat::BigI64(_),
            ) => 0,
            _ => 1,
        }
    }
}

macro_rules! number_methods {
    ($($read:literal, $write:literal => $format:expr;)*) => {
        &[$(
            NumberMethod { name: $read, format: $format, write: false },
            NumberMethod { name: $write, format: $format, write: true },
        )*]
    };
}

use Endian::{Big, Little};
use NumberFormat::*;

/// Every `read*` and `write*` method, including the `Uint` aliases of the `UInt` ones.
const NUMBER_METHODS: &[NumberMethod] = number_methods! {
    c"readUInt8", c"writeUInt8" => U8;
    c"readUint8", c"writeUint8" => U8;
    c"readInt8", c"writeInt8" => I8;
    c"readUInt16LE", c"writeUInt16LE" => U16(Little);
    c"readUint16LE", c"writeUint16LE" => U16(Little);
    c"readUInt16BE", c"writeUInt16BE" => U16(Big);
    c"readUint16BE", c"writeUint16BE" => U16(Big);
    c"readInt16LE", c"writeInt16LE" => I16(Little);
    c"readInt16BE", c"writeInt16BE" => I16(Big);
    c"readUInt32LE", c"writeUInt32LE" => U32(Little);
    c"readUint32LE", c"writeUint32LE" => U32(Little);
    c"readUInt32BE", c"writeUInt32BE" => U32(Big);
    c"readUint32BE", c"writeUint32BE" => U32(Big);
    c"readInt32LE", c"writeInt32LE" => I32(Little);
    c"readInt32BE", c"writeInt32BE" => I32(Big);
    c"readFloatLE", c"writeFloatLE" => F32(Little);
    c"readFloatBE", c"writeFloatBE" => F32(Big);
    c"readDoubleLE", c"writeDoubleLE" => F64(Little);
    c"readDoubleBE", c"writeDoubleBE" => F64(Big);
    c"readBigUInt64LE", c"writeBigUInt64LE" => BigU64(Little);
    c"readBigUint64LE", c"writeBigUint64LE" => BigU64(Little);
    c"readBigUInt64BE", c"writeBigUInt64BE" => BigU64(Big);
    c"readBigUint64BE", c"writeBigUint64BE" => BigU64(Big);
    c"readBigInt64LE", c"writeBigInt64LE" => BigI64(Little);
    c"readBigInt64BE", c"writeBigInt64BE" => BigI64(Big);
    c"readUIntLE", c"writeUIntLE" => UVar(Little);
    c"readUintLE", c"writeUintLE" => UVar(Little);
    c"readUIntBE", c"writeUIntBE" => UVar(Big);
    c"readUintBE", c"writeUintBE" => UVar(Big);
    c"readIntLE", c"writeIntLE" => IVar(Little);
    c"readIntBE", c"writeIntBE" => IVar(Big);
};

/// The width argument of `readUIntLE` and its relatives: 1 to 6. Other values are an error, as
/// Node.js's `boundsError` reports it.
fn byte_length_arg(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<usize, Thrown> {
    let width = value.is_number().then(|| value.to_number());
    if let Some(width) = width.filter(|width| matches!(*width, 1.0..=6.0) && width.fract() == 0.0) {
        return Ok(width as usize);
    }
    Err(bounds_error(scope, value, 6, Some("byteLength")))
}

/// Node.js's `boundsError(value, max, name)`, for an offset or width argument `value` that is out
/// of bounds: `ERR_INVALID_ARG_TYPE` for a non-number, `ERR_OUT_OF_RANGE` for a non-integer,
/// `ERR_BUFFER_OUT_OF_BOUNDS` for a negative `max`, which means nothing fits, and otherwise
/// `ERR_OUT_OF_RANGE` for an integer outside the range. `name` defaults to `offset`.
fn bounds_error(scope: &Scope<'_>, value: HandleValue<'_>, max: i64, name: Option<&str>) -> Thrown {
    let label = name.unwrap_or("offset");
    if !value.is_number() {
        return NodeError::invalid_arg_type(scope, label, "of type number", value).into();
    }
    let number = value.to_number();
    if number.floor() != number {
        return NodeError::out_of_range(
            label,
            "an integer",
            &crate::errors::out_of_range_received(number),
        )
        .into();
    }
    if max < 0 {
        return out_of_bounds(None);
    }
    let min = if name.is_some() { 1 } else { 0 };
    NodeError::out_of_range(
        label,
        &format!(">= {min} and <= {max}"),
        &crate::errors::out_of_range_received(number),
    )
    .into()
}

/// Node.js's `checkBounds`: the offset argument of a read or write of `width` bytes in a buffer of
/// `length` bytes, a number such that the bytes from it fit.
fn number_offset(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    width: usize,
    length: usize,
) -> Result<usize, Thrown> {
    if !value.is_number() {
        return Err(NodeError::invalid_arg_type(scope, "offset", "of type number", value).into());
    }
    let offset = value.to_number();
    if offset.fract() == 0.0 && offset >= 0.0 && offset + width as f64 <= length as f64 {
        return Ok(offset as usize);
    }
    Err(bounds_error(
        scope,
        value,
        length as i64 - width as i64,
        None,
    ))
}

/// The body of every `read*` method, whose payload is its index in [`NUMBER_METHODS`].
fn read_number(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let method = &NUMBER_METHODS[payload.to_int32() as usize];
    let run = || -> Result<Value, Thrown> {
        let this = Object::from_value(scope, args.this())?;
        let view = this_view(this)?;
        let offset = args.get(0);
        let width = match method.format.width() {
            Some(width) => width,
            None if offset.is_undefined() => {
                let error = NodeError::invalid_arg_type(scope, "offset", "of type number", offset);
                return Err(error.into());
            }
            None => byte_length_arg(scope, args.get(1))?,
        };
        let offset = if offset.is_undefined() {
            scope.root_value(js::value::from_f64(0.0))
        } else {
            offset
        };
        let offset = number_offset(scope, offset, width, view.byte_length())?;
        let mut copy = [0; 8];
        view.with_bytes(|bytes| copy[..width].copy_from_slice(&bytes[offset..offset + width]));
        let bytes = &copy[..width];
        let endian = method.format.endian();
        Ok(match method.format {
            NumberFormat::U8
            | NumberFormat::U16(_)
            | NumberFormat::U32(_)
            | NumberFormat::UVar(_) => {
                js::value::from_f64(read_integer(bytes, endian, false) as f64)
            }
            NumberFormat::I8
            | NumberFormat::I16(_)
            | NumberFormat::I32(_)
            | NumberFormat::IVar(_) => {
                js::value::from_f64(read_integer(bytes, endian, true) as f64)
            }
            NumberFormat::F32(_) => {
                let array: [u8; 4] = bytes.try_into().expect("four bytes");
                let float = match endian {
                    Endian::Little => f32::from_le_bytes(array),
                    Endian::Big => f32::from_be_bytes(array),
                };
                js::value::from_f64(f64::from(float))
            }
            NumberFormat::F64(_) => {
                let array: [u8; 8] = bytes.try_into().expect("eight bytes");
                js::value::from_f64(match endian {
                    Endian::Little => f64::from_le_bytes(array),
                    Endian::Big => f64::from_be_bytes(array),
                })
            }
            NumberFormat::BigU64(_) => {
                let value = read_integer(bytes, endian, false) as u64;
                js::value::from_bigint(js::bigint::from_u64(scope, value)?)
            }
            NumberFormat::BigI64(_) => {
                let value = read_integer(bytes, endian, true) as i64;
                js::value::from_bigint(js::bigint::from_i64(scope, value)?)
            }
        })
    };
    run().map_err(|error| error.throw(scope))
}

/// `ERR_OUT_OF_RANGE` for a `value` outside `min..=max` written by a `write*` method.
fn value_out_of_range(value: f64, min: i128, max: i128, width: usize) -> Thrown {
    // Node.js's `checkInt` gives the range as powers of two for widths above 4 bytes.
    let range = match (width > 4, min) {
        (false, _) => format!(">= {min} and <= {max}"),
        (true, 0) => format!(">= 0 and < 2 ** {}", width * 8),
        (true, _) => format!(">= -(2 ** {0}) and < 2 ** {0}", width * 8 - 1),
    };
    NodeError::out_of_range(
        "value",
        &range,
        &crate::errors::out_of_range_received(value),
    )
    .into()
}

/// The body of every `write*` method, whose payload is its index in [`NUMBER_METHODS`]. Returns
/// the offset after the written bytes.
fn write_number(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let method = &NUMBER_METHODS[payload.to_int32() as usize];
    let run = || -> Result<Value, Thrown> {
        let this = Object::from_value(scope, args.this())?;
        let view = this_view(this)?;
        let width = match method.format.width() {
            Some(width) => width,
            None => byte_length_arg(scope, args.get(2))?,
        };
        let value = args.get(0);
        let offset = args.get(1);
        let offset = if offset.is_undefined() && method.format.width().is_some() {
            scope.root_value(js::value::from_f64(0.0))
        } else {
            offset
        };
        let length = view.byte_length();
        let endian = method.format.endian();
        let bytes = match method.format {
            NumberFormat::F32(_) | NumberFormat::F64(_) => {
                let float = to_number(scope, value)?;
                let offset = number_offset(scope, offset, width, length)?;
                let mut bytes = [0; 8];
                match (method.format, endian) {
                    (NumberFormat::F32(_), Endian::Little) => {
                        bytes[..4].copy_from_slice(&(float as f32).to_le_bytes())
                    }
                    (NumberFormat::F32(_), Endian::Big) => {
                        bytes[..4].copy_from_slice(&(float as f32).to_be_bytes())
                    }
                    (_, Endian::Little) => bytes = float.to_le_bytes(),
                    (_, Endian::Big) => bytes = float.to_be_bytes(),
                }
                (offset, bytes)
            }
            NumberFormat::BigU64(_) | NumberFormat::BigI64(_) => {
                let unsigned = matches!(method.format, NumberFormat::BigU64(_));
                let (min, max, range) = if unsigned {
                    (0.0, 2f64.powi(64), ">= 0n and < 2n ** 64n")
                } else {
                    (
                        -(2f64.powi(63)),
                        2f64.powi(63),
                        ">= -(2n ** 63n) and < 2n ** 63n",
                    )
                };
                // An object is compared through its primitive value.
                let value = match Object::from_value(scope, *value) {
                    Ok(object) => object.to_primitive_number(scope)?,
                    Err(_) => value,
                };
                if !value.is_bigint() {
                    // A number outside the range is an error, and any other value then throws the
                    // TypeError of combining a bigint with another type.
                    let number = value.is_number().then(|| value.to_number());
                    if let Some(number) = number.filter(|n| *n < min || *n >= max) {
                        return Err(NodeError::out_of_range(
                            "value",
                            range,
                            &crate::errors::out_of_range_received(number),
                        )
                        .into());
                    }
                    number_offset(scope, offset, width, length)?;
                    js::error::throw_type_error(
                        scope,
                        c"Cannot mix BigInt and other types, use explicit conversions",
                    );
                    return Err(Thrown::Pending);
                }
                let bigint = js::bigint::to_bigint(scope, value)?;
                let digits = js::JSString::from_value(scope, *value)?.to_utf8(scope)?;
                let magnitude = digits.trim_start_matches('-');
                let beyond_2_32 = magnitude.len() > 10
                    || (magnitude.len() == 10
                        && magnitude.parse::<u64>().is_ok_and(|m| m > 4294967296));
                let received = if beyond_2_32 {
                    format!("{}n", crate::errors::add_numerical_separator(&digits))
                } else {
                    format!("{digits}n")
                };
                let out_of_range = || NodeError::out_of_range("value", range, &received);
                let integer = if unsigned {
                    i128::from(js::bigint::fits_u64(bigint).ok_or_else(out_of_range)?)
                } else {
                    i128::from(js::bigint::fits_i64(bigint).ok_or_else(out_of_range)?)
                };
                let offset = number_offset(scope, offset, width, length)?;
                (offset, integer_bytes(integer, 8, endian))
            }
            _ => {
                let number = to_number(scope, value)?;
                let signed = matches!(
                    method.format,
                    NumberFormat::I8
                        | NumberFormat::I16(_)
                        | NumberFormat::I32(_)
                        | NumberFormat::IVar(_)
                );
                let bits = (width * 8) as u32;
                let (min, max) = if signed {
                    (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
                } else {
                    (0, (1i128 << bits) - 1)
                };
                // An 8-bit write checks the offset's type before the value's range.
                if width == 1 && !offset.is_number() {
                    return Err(bounds_error(scope, offset, 0, None));
                }
                // `NaN` passes the range check, and is stored as 0.
                if number < min as f64 || number > max as f64 {
                    return Err(value_out_of_range(number, min, max, width));
                }
                let offset = number_offset(scope, offset, width, length)?;
                let integer = if number.is_nan() {
                    0
                } else {
                    number.trunc() as i128
                };
                (offset, integer_bytes(integer, width, endian))
            }
        };
        let (offset, bytes) = bytes;
        // Converting the value can run JS that shrinks or detaches the buffer.
        if offset + width > view.byte_length() {
            return Err(out_of_bounds(None));
        }
        view.with_bytes_mut(|target| {
            target[offset..offset + width].copy_from_slice(&bytes[..width])
        });
        Ok(js::value::from_f64((offset + width) as f64))
    };
    run().map_err(|error| error.throw(scope))
}

// ============================================================================
// Buffer
// ============================================================================

/// Node.js's `fromArrayBuffer`: a `Buffer` viewing `buffer` from `byte_offset`, for `length`
/// bytes or to its end. Both are converted to numbers, a `NaN` offset is 0, and a length that isn't
/// positive is 0.
fn from_array_buffer<'s>(
    scope: &'s Scope<'_>,
    buffer: js::ArrayBuffer<'_>,
    byte_offset: Option<HandleValue<'_>>,
    length: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let byte_length = buffer.byte_length() as f64;
    let offset = match byte_offset.filter(|offset| !offset.is_undefined()) {
        None => 0.0,
        Some(offset) => {
            let offset = to_number(scope, offset)?;
            if offset.is_nan() {
                0.0
            } else {
                offset
            }
        }
    };
    let max_length = byte_length - offset;
    if max_length < 0.0 {
        return Err(out_of_bounds(Some("offset")));
    }
    let length = match length.filter(|length| !length.is_undefined()) {
        None => None,
        Some(length) => {
            let length = to_number(scope, length)?;
            if length > max_length {
                return Err(out_of_bounds(Some("length")));
            }
            Some(if length > 0.0 { length.trunc() } else { 0.0 })
        }
    };
    // The typed array constructor converts the offset to an index.
    let offset = offset.trunc();
    if offset < 0.0 {
        js::error::throw_range_error(scope, c"Start offset is outside the bounds of the buffer");
        return Err(Thrown::Pending);
    }
    let Some(length) = length else {
        // Without a length, a view of a resizable buffer follows the buffer's length, as in Node.js.
        let array = js::Uint8Array::with_buffer_to_end(scope, buffer, offset as usize)?;
        let array: Object<'s> = *array.as_array_buffer_view();
        array.set_prototype(scope, buffer_prototype(scope)?.handle())?;
        return Ok(array);
    };
    Ok(buffer_view(
        scope,
        buffer,
        offset as usize,
        length as usize,
    )?)
}

/// Node.js's `fromArrayLike`: a `Buffer` of the elements of `array_like` up to its `length`, each
/// converted to a number and stored as a `Uint8Array` stores it.
fn from_array_like<'s>(scope: &'s Scope<'_>, array_like: Object<'_>) -> Result<Object<'s>, Thrown> {
    if let Some(view) = ArrayBufferView::from_object(array_like) {
        use js::typedarray::ViewKind;
        if matches!(
            view.view_kind(),
            ViewKind::Uint8 | ViewKind::Uint8Clamped | ViewKind::Int8
        ) {
            return Ok(new_buffer(scope, &view.copy_bytes(scope)?)?);
        }
    }
    let count = to_number(scope, array_like.get_property(scope, c"length")?)?;
    let count = if count > 0.0 { count.trunc() } else { 0.0 };
    if count > MAX_LENGTH {
        js::error::throw_range_error(scope, c"Array buffer allocation failed");
        return Err(Thrown::Pending);
    }
    let count = count as u32;
    let buffer = new_zeroed_buffer(scope, count as usize)?;
    let view = ArrayBufferView::from_object(buffer).ok_or(ExnThrown)?;
    for index in 0..count {
        let element = array_like.get_element(scope, index)?;
        let byte = to_uint8(to_number(scope, element)?);
        // Content code has no reference to the new buffer, so it keeps its length.
        view.with_bytes_mut(|target| target[index as usize] = byte);
    }
    Ok(buffer)
}

/// Node.js's `Buffer.from(value, encodingOrOffset, length)`.
fn buffer_from<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    encoding_or_offset: Option<HandleValue<'_>>,
    length: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    if value.is_string() {
        let encoding = match encoding_or_offset.filter(|encoding| encoding.is_string()) {
            Some(encoding) => encoding_or_utf8(scope, Some(encoding))?,
            None => Encoding::Utf8,
        };
        let string = js::JSString::from_value(scope, value)?;
        if encoding == Encoding::Utf8 {
            return Ok(utf8_buffer(scope, string)?);
        }
        return Ok(new_buffer(scope, &encode_string(scope, string, encoding)?)?);
    }
    let invalid = || {
        NodeError::invalid_arg_type(
            scope,
            "first argument",
            "of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object",
            value,
        )
    };
    let object = Object::from_value(scope, *value).map_err(|_| invalid())?;
    if let Ok(buffer) = object.cast::<js::ArrayBuffer>() {
        return from_array_buffer(scope, buffer, encoding_or_offset, length);
    }
    // An object whose `valueOf` returns a string or another object, such as a `String` object.
    let value_of = object.get_property(scope, c"valueOf")?;
    if Object::from_value(scope, value_of).is_ok_and(|f| f.is_callable()) {
        let primitive = Function::call(scope, object, value_of, &[] as &[Value])?;
        let other_object = primitive.is_object() && primitive.to_object() != object.as_raw();
        if other_object || primitive.is_string() {
            return buffer_from(scope, primitive, encoding_or_offset, length);
        }
    }
    // An array-like object, or `{ type: 'Buffer', data: [...] }` as `toJSON` makes.
    let length_value = object.get_property(scope, c"length")?;
    let has_array_buffer = Object::from_value(scope, object.get_property(scope, c"buffer")?)
        .is_ok_and(|buffer| buffer.cast::<js::ArrayBuffer>().is_ok());
    if !length_value.is_undefined() || has_array_buffer {
        if !length_value.is_number() {
            return Ok(new_buffer(scope, &[])?);
        }
        return from_array_like(scope, object);
    }
    let data = object.get_property(scope, c"data")?;
    let is_buffer_json = object.get_property(scope, c"type")?.is_string()
        && String::from_jsval(scope, object.get_property(scope, c"type")?, ())
            .is_ok_and(|kind| kind == "Buffer");
    if is_buffer_json && js::Array::is_array_value(scope, data)? {
        let data = Object::from_value(scope, data).map_err(|_| invalid())?;
        return from_array_like(scope, data);
    }
    let key = js::symbol::get_well_known_key(scope, js::native::SymbolCode::toPrimitive);
    let to_primitive = object.get_property_by_id(scope, scope.root_id(key))?;
    if Object::from_value(scope, to_primitive).is_ok_and(|f| f.is_callable()) {
        let hint = scope.root_value(js::JSString::from_str(scope, "string")?.as_value());
        let primitive = Function::call(scope, object, to_primitive, &[hint])?;
        if primitive.is_string() {
            return buffer_from(scope, primitive, encoding_or_offset, None);
        }
    }
    Err(invalid().into())
}

/// A `Buffer.alloc` size argument: an integer from 0 to [`MAX_LENGTH`].
fn size_arg(scope: &Scope<'_>, size: HandleValue<'_>) -> Result<usize, Thrown> {
    if !size.is_number() {
        return Err(NodeError::invalid_arg_type(scope, "size", "of type number", size).into());
    }
    let size = f64::from_jsval(scope, size, ()).unwrap_or(f64::NAN);
    if !(0.0..=MAX_LENGTH).contains(&size) {
        return Err(NodeError::out_of_range(
            "size",
            &format!(">= 0 && <= {MAX_LENGTH}"),
            &crate::errors::out_of_range_received(size),
        )
        .into());
    }
    Ok(size as usize)
}

/// `ERR_INVALID_ARG_TYPE` unless `value` is a string, with the message of Node.js's C++
/// `THROW_AND_RETURN_IF_NOT_STRING`.
fn require_string(value: HandleValue<'_>) -> Result<(), Thrown> {
    if value.is_string() {
        return Ok(());
    }
    let message = "argument must be a string";
    Err(NodeError::new(ErrorKind::TypeError, "ERR_INVALID_ARG_TYPE", message).into())
}

/// Node.js's `StringBytes::Write`: write the bytes the string `string` encodes to in `encoding`
/// into `view` from `offset`, at most `max_length` of them and only whole characters. Returns how
/// many bytes were written.
fn write_string(
    scope: &Scope<'_>,
    view: ArrayBufferView<'_>,
    string: HandleValue<'_>,
    offset: usize,
    max_length: usize,
    encoding: Encoding,
) -> Result<usize, Thrown> {
    require_string(string)?;
    let string = js::JSString::from_value(scope, *string)?;
    if encoding == Encoding::Utf8 {
        // Flattening a rope allocates, so it happens before the bytes are borrowed.
        string.ensure_linear(scope)?;
        // Converting the arguments can run JS that shrinks or detaches the buffer.
        let available = view.byte_length().checked_sub(offset);
        let Some(available) = available else {
            return Err(out_of_bounds(Some("offset")));
        };
        let end = offset + max_length.min(available);
        return Ok(string.write_utf8(scope, view, offset..end)?);
    }
    let mut bytes = encode_string(scope, string, encoding)?;
    if bytes.len() > max_length {
        bytes.truncate(match encoding {
            Encoding::Utf16le => max_length & !1,
            _ => max_length,
        });
    }
    // Converting the arguments can run JS that shrinks or detaches the buffer.
    if offset + bytes.len() > view.byte_length() {
        return Err(out_of_bounds(Some("offset")));
    }
    view.with_bytes_mut(|target| target[offset..offset + bytes.len()].copy_from_slice(&bytes));
    Ok(bytes.len())
}

/// The `parent` getter: `this.buffer` for a `Buffer`, and `undefined` otherwise.
fn parent(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Ok(this) = Object::from_value(scope, args.this()) else {
        return Ok(js::value::undefined());
    };
    if !is_buffer(scope, this)? {
        return Ok(js::value::undefined());
    }
    Ok(this.get_property(scope, c"buffer")?.get())
}

/// The `offset` getter: `this.byteOffset` for a `Buffer`, and `undefined` otherwise.
fn offset(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Ok(this) = Object::from_value(scope, args.this()) else {
        return Ok(js::value::undefined());
    };
    if !is_buffer(scope, this)? {
        return Ok(js::value::undefined());
    }
    Ok(this.get_property(scope, c"byteOffset")?.get())
}

/// Node.js's `ParseArrayIndex`: an index argument converted to an integer, `default` if it is
/// `undefined`, and `ERR_OUT_OF_RANGE` if it is negative.
fn array_index(
    scope: &Scope<'_>,
    value: Option<HandleValue<'_>>,
    default: usize,
) -> Result<usize, Thrown> {
    match defined(value) {
        Some(value) => integer_index(to_number(scope, value)?),
        None => Ok(default),
    }
}

/// [`array_index`] of the number `number`: truncated, with `NaN` as 0, and `ERR_OUT_OF_RANGE` if
/// it is negative.
fn integer_index(number: f64) -> Result<usize, Thrown> {
    let number = number.trunc();
    let number = if number.is_nan() { 0.0 } else { number };
    if number < 0.0 {
        return Err(NodeError::new(
            ErrorKind::RangeError,
            "ERR_OUT_OF_RANGE",
            "Index out of range",
        )
        .into());
    }
    Ok(number.min(usize::MAX as f64) as usize)
}

/// Node.js's `StringSlice` for the `*Slice` methods: the bytes from `start` to `end` of `this`
/// decoded as `encoding`.
fn slice_as<'s>(
    scope: &'s Scope<'_>,
    this: Object<'_>,
    start: Option<HandleValue<'_>>,
    end: Option<HandleValue<'_>>,
    encoding: Encoding,
) -> Result<js::JSString<'s>, Thrown> {
    let view = this_view(this)?;
    if view.byte_length() == 0 {
        return Ok(js::JSString::empty(scope));
    }
    let start = array_index(scope, start, 0)?;
    let end = array_index(scope, end, view.byte_length())?;
    if start >= view.byte_length() || end <= start {
        return Ok(js::JSString::empty(scope));
    }
    if end > view.byte_length() {
        return Err(NodeError::new(
            ErrorKind::RangeError,
            "ERR_OUT_OF_RANGE",
            "Index out of range",
        )
        .into());
    }
    let bytes = view.with_bytes(|bytes| bytes[start..end].to_vec());
    Ok(decode_bytes(scope, &bytes, encoding)?)
}

/// The `*Write` methods: write `string` in `encoding` into `this` from `offset`, at most `length`
/// bytes, and return the count written.
///
/// For UTF-8, Latin-1 and ASCII, Node.js's JS wrappers check the numbers `offset` and `length`
/// before they are truncated: `offset` defaults to 0, `length` to the bytes after `offset`, and
/// `ERR_BUFFER_OUT_OF_BOUNDS` is thrown unless `0 <= offset <= byteLength` and
/// `0 <= length <= byteLength - offset`. `NaN` passes these checks.
///
/// Then, as Node.js's C++ `StringWrite` for every encoding, `string` must be a string, and `offset`
/// and `length` are integers as [`array_index`] converts them: an `offset` past the end throws
/// `ERR_BUFFER_OUT_OF_BOUNDS`, and `length` is clamped to the bytes after `offset`.
fn write_as(
    scope: &Scope<'_>,
    this: Object<'_>,
    string: HandleValue<'_>,
    offset: Option<HandleValue<'_>>,
    length: Option<HandleValue<'_>>,
    encoding: Encoding,
) -> Result<f64, Thrown> {
    let view = this_view(this)?;
    let capacity = view.byte_length();
    let checked = if matches!(
        encoding,
        Encoding::Utf8 | Encoding::Latin1 | Encoding::Ascii
    ) {
        let available = capacity as f64;
        let offset = match defined(offset) {
            None => 0.0,
            Some(offset) => to_number(scope, offset)?,
        };
        if offset < 0.0 || offset > available {
            return Err(out_of_bounds(Some("offset")));
        }
        let length = match defined(length) {
            None => available - offset,
            Some(length) => to_number(scope, length)?,
        };
        if length < 0.0 || length > available - offset {
            return Err(out_of_bounds(Some("length")));
        }
        Some((offset, length))
    } else {
        None
    };
    require_string(string)?;
    let offset = match checked {
        Some((offset, _)) => integer_index(offset)?,
        None => array_index(scope, offset, 0)?,
    };
    if offset > capacity {
        return Err(out_of_bounds(Some("offset")));
    }
    let max_length = match checked {
        Some((_, length)) => integer_index(length)?,
        None => array_index(scope, length, capacity - offset)?,
    };
    let max_length = max_length.min(capacity - offset);
    if max_length == 0 {
        return Ok(0.0);
    }
    Ok(write_string(scope, view, string, offset, max_length, encoding)? as f64)
}

/// Node.js's `normalizeEncoding` for a `fill` encoding argument: `undefined`, `null` and `""` are
/// UTF-8, and other values are an error unless they are a string naming an encoding.
fn fill_encoding(scope: &Scope<'_>, value: Option<HandleValue<'_>>) -> Result<Encoding, Thrown> {
    let Some(value) = value.filter(|value| !value.is_undefined() && !value.is_null()) else {
        return Ok(Encoding::Utf8);
    };
    let name = crate::errors::validate_string(scope, value, "encoding")?;
    if name.is_empty() {
        return Ok(Encoding::Utf8);
    }
    Encoding::parse(&name).ok_or_else(|| unknown_encoding(scope, value))
}

/// Node.js's `_fill(buf, value, offset, end, encoding)` for a buffer of `length` bytes: the range
/// from `offset` to `end` and the bytes that repeat over it, or `None` if the range is empty.
/// `value` is one of:
///
/// - a number, stored as a `Uint8Array` stores it;
/// - a string in `encoding`, or a string of one character that fits in a byte, stored as a number;
/// - the bytes of an `ArrayBufferView`;
/// - another value, converted to a number.
///
/// For a string, `offset` or `end` may be the encoding instead. The range is validated after the
/// encoding and before `value` is converted.
fn fill_plan(
    scope: &Scope<'_>,
    length: usize,
    value: HandleValue<'_>,
    offset: Option<HandleValue<'_>>,
    end: Option<HandleValue<'_>>,
    encoding: Option<HandleValue<'_>>,
) -> Result<Option<FillPlan>, Thrown> {
    let (mut offset, mut end, mut encoding) = (defined(offset), defined(end), encoding);
    let mut string_encoding = Encoding::Utf8;
    if value.is_string() {
        if offset.is_none_or(|offset| offset.is_string()) {
            (encoding, offset, end) = (offset, None, None);
        } else if end.is_some_and(|end| end.is_string()) {
            (encoding, end) = (end, None);
        }
        string_encoding = fill_encoding(scope, encoding)?;
    }
    let (start, stop) = match offset {
        None => (0, length),
        Some(offset) => {
            let start = crate::errors::validate_integer(scope, offset, "offset", 0.0, MAX_OFFSET)?;
            let stop = match end {
                None => length as f64,
                Some(end) => {
                    crate::errors::validate_integer(scope, end, "end", 0.0, length as f64)?
                }
            };
            if start >= stop {
                return Ok(None);
            }
            (start as usize, stop as usize)
        }
    };
    let pattern = if value.is_string() {
        let string = js::JSString::from_value(scope, value)?;
        let first = if string.len() == 1 {
            Some(string.char_at(scope, 0)?)
        } else {
            None
        };
        let byte = match (string.len(), first, string_encoding) {
            (0, _, _) => Some(0),
            (_, Some(unit), Encoding::Utf8 | Encoding::Ascii) if unit < 128 => Some(unit as u8),
            (_, Some(unit), Encoding::Latin1) => Some(unit as u8),
            _ => None,
        };
        match byte {
            Some(byte) => vec![byte],
            None => {
                let mut pattern = encode_string(scope, string, string_encoding)?;
                if !matches!(string_encoding, Encoding::Utf8 | Encoding::Utf16le) {
                    pattern.truncate(stop - start);
                }
                pattern
            }
        }
    } else if value.is_number() {
        vec![to_uint8(value.to_number())]
    } else if let Some(view) = as_view(scope, value) {
        view.copy_bytes(scope)?
    } else {
        vec![to_uint8(to_number(scope, value)?)]
    };
    if start == stop {
        return Ok(None);
    }
    if pattern.is_empty() {
        return Err(NodeError::invalid_arg_value(scope, "value", value, "is invalid").into());
    }
    Ok(Some(FillPlan {
        start,
        stop,
        pattern,
    }))
}

/// The range a `fill` writes, and the bytes that repeat over it.
struct FillPlan {
    start: usize,
    stop: usize,
    pattern: Vec<u8>,
}

impl FillPlan {
    /// Write the pattern over the range of `bytes`.
    fn apply(&self, bytes: &mut [u8]) {
        for (index, byte) in bytes[self.start..self.stop].iter_mut().enumerate() {
            *byte = self.pattern[index % self.pattern.len()];
        }
    }
}

/// `Buffer(value, encodingOrOffset, length)` without the deprecation warning: `Buffer.alloc(value)`
/// for a number, and `Buffer.from` otherwise.
fn construct<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    encoding_or_offset: Option<HandleValue<'_>>,
    length: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    if value.is_number() {
        if encoding_or_offset.is_some_and(|encoding| encoding.is_string()) {
            return Err(
                NodeError::invalid_arg_type(scope, "string", "of type string", value).into(),
            );
        }
        let size = size_arg(scope, value)?;
        return Ok(new_zeroed_buffer(scope, size)?);
    }
    buffer_from(scope, value, encoding_or_offset, length)
}

js::instance_local! {
    /// Set once DEP0005 was emitted.
    static CONSTRUCTOR_WARNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// How many calls of the `Buffer` constructor checked whether to emit DEP0005.
    static CONSTRUCTOR_CHECKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The number of `Buffer` constructor calls after which DEP0005 is no longer emitted, Node.js's
/// limit of `nodeModulesCheckCounter`.
const MAX_CONSTRUCTOR_CHECKS: u32 = 10000;

/// Emit DEP0005 for a call of the `Buffer` constructor, once per instance, and not after
/// [`MAX_CONSTRUCTOR_CHECKS`] calls. Without `--pending-deprecation`, a call from a module in
/// `node_modules` emits nothing.
fn warn_buffer_constructor(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    if CONSTRUCTOR_WARNED.get() {
        return Ok(());
    }
    let checks = CONSTRUCTOR_CHECKS.get().saturating_add(1);
    CONSTRUCTOR_CHECKS.set(checks);
    if checks > MAX_CONSTRUCTOR_CHECKS
        || (!crate::runtime_flags::get().pending_deprecation
            && crate::process::called_from_node_modules(scope)?)
    {
        return Ok(());
    }
    CONSTRUCTOR_WARNED.set(true);
    crate::process::deprecation_warning(
        scope,
        "Buffer() is deprecated due to security and usability issues. Please use the \
         Buffer.alloc(), Buffer.allocUnsafe(), or Buffer.from() methods instead.",
        "DEP0005",
    )
}

/// The constructor `Buffer[Symbol.species]` returns, which `Uint8Array` methods such as `map` and
/// `filter` create their results with: the `Buffer` constructor without the deprecation warning.
fn species_constructor(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let optional = |index| (args.len() > index).then(|| args.get(index));
    let buffer =
        construct(scope, args.get(0), optional(1), optional(2)).map_err(|e| e.throw(scope))?;
    buffer.to_jsval_raw_throwing(scope)
}

/// The getter of `Buffer[Symbol.species]`: the constructor in the payload.
fn get_species(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(payload.get())
}

/// Define `Buffer[Symbol.species]` on `ctor`, the `Buffer` constructor: a getter of a constructor
/// named `FastBuffer`, as in Node.js, that shares `Buffer.prototype`.
pub(crate) fn define_species(scope: &Scope<'_>, ctor: Object<'_>) -> Result<(), ExnThrown> {
    let species =
        Function::new_constructor_callback(scope, c"FastBuffer", 3, species_constructor, ())?;
    species.define_property(
        scope,
        c"prototype",
        ctor.get_property(scope, c"prototype")?,
        0,
    )?;
    let getter = Function::new_callback(scope, c"get [Symbol.species]", 0, get_species, *species)?;
    let key = scope.root_id(js::symbol::get_well_known_key(
        scope,
        js::native::SymbolCode::species,
    ));
    ctor.define_accessor_by_id(scope, key, Some(*getter), None, 0)
}

#[core_runtime::jsfunction_class(name = "Buffer")]
pub mod buffer {
    use super::*;

    /// <https://nodejs.org/api/buffer.html#new-bufferarray>: the deprecated `Buffer(value)` and
    /// `new Buffer(value)`, which are `Buffer.alloc(value)` for a number and `Buffer.from` otherwise.
    #[constructor(length = 3)]
    pub fn create<'s>(
        scope: &'s Scope<'_>,
        value: HandleValue<'_>,
        encoding_or_offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        warn_buffer_constructor(scope)?;
        construct(scope, value, encoding_or_offset, length)
    }

    /// <https://nodejs.org/api/buffer.html#static-method-bufferfromarray>
    #[static_method(length = 3)]
    pub fn from<'s>(
        scope: &'s Scope<'_>,
        value: HandleValue<'_>,
        encoding_or_offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        buffer_from(scope, value, encoding_or_offset, length)
    }

    /// <https://nodejs.org/api/buffer.html#static-method-bufferallocsize-fill-encoding>
    #[static_method(length = 3)]
    pub fn alloc<'s>(
        scope: &'s Scope<'_>,
        size: HandleValue<'_>,
        fill: Option<HandleValue<'_>>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let size = size_arg(scope, size)?;
        let fill = fill.filter(|fill| !fill.is_undefined());
        let mut plan = None;
        if let Some(fill) = fill.filter(|fill| !(fill.is_number() && fill.to_number() == 0.0)) {
            if size > 0 {
                let start = scope.root_value(js::value::from_f64(0.0));
                let end = scope.root_value(js::value::from_f64(size as f64));
                plan = fill_plan(scope, size, fill, Some(start), Some(end), encoding)?;
            }
        }
        Ok(new_buffer_with(scope, size, |bytes| {
            if let Some(plan) = plan {
                plan.apply(bytes);
            }
            Ok(())
        })?)
    }

    /// <https://nodejs.org/api/buffer.html#static-method-bufferallocunsafesize>: zero-filled.
    #[static_method]
    pub fn alloc_unsafe<'s>(
        scope: &'s Scope<'_>,
        size: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let size = size_arg(scope, size)?;
        Ok(new_zeroed_buffer(scope, size)?)
    }

    /// <https://nodejs.org/api/buffer.html#static-method-bufferallocunsafeslowsize>: zero-filled.
    #[static_method]
    pub fn alloc_unsafe_slow<'s>(
        scope: &'s Scope<'_>,
        size: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let size = size_arg(scope, size)?;
        Ok(new_zeroed_buffer(scope, size)?)
    }

    /// <https://nodejs.org/api/buffer.html#static-method-bufferbytelengthstring-encoding>: for a
    /// string, the length its encoding computes without decoding it, and the UTF-8 length for an
    /// unknown encoding.
    #[static_method(length = 2)]
    pub fn byte_length(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        if value.is_string() {
            let string = js::JSString::from_value(scope, value)?;
            let length = string.len();
            if length == 0 {
                return Ok(0.0);
            }
            let encoding = match encoding.filter(|encoding| js::conversion::to_boolean(*encoding)) {
                None => Encoding::Utf8,
                Some(encoding) => lookup_encoding(scope, encoding)?.unwrap_or(Encoding::Utf8),
            };
            return Ok(match encoding {
                Encoding::Utf8 => string.utf8_length(scope)?,
                Encoding::Utf16le => length * 2,
                Encoding::Latin1 | Encoding::Ascii => length,
                Encoding::Hex => length / 2,
                Encoding::Base64 | Encoding::Base64Url => {
                    let mut length = length;
                    if string.char_at(scope, length - 1)? == u16::from(b'=') {
                        length -= 1;
                    }
                    if length > 1 && string.char_at(scope, length - 1)? == u16::from(b'=') {
                        length -= 1;
                    }
                    (length * 3) >> 2
                }
            } as f64);
        }
        if let Some(view) = as_view(scope, value) {
            return Ok(view.byte_length() as f64);
        }
        if let Ok(buffer) = Object::from_value(scope, *value)
            .map_err(|_| ())
            .and_then(|object| object.cast::<js::ArrayBuffer>().map_err(|_| ()))
        {
            return Ok(buffer.byte_length() as f64);
        }
        Err(NodeError::invalid_arg_type(
            scope,
            "string",
            "of type string or an instance of Buffer or ArrayBuffer",
            value,
        )
        .into())
    }

    /// <https://nodejs.org/api/buffer.html#static-method-buffercomparebuf1-buf2>
    #[static_method(name = "compare")]
    pub fn compare_buffers(
        scope: &Scope<'_>,
        a: HandleValue<'_>,
        b: HandleValue<'_>,
    ) -> Result<i32, Thrown> {
        let a = uint8_array_arg(scope, a, "buf1")?;
        let b = uint8_array_arg(scope, b, "buf2")?;
        Ok(a.with_bytes_pair(b, |a, b| a.cmp(b)) as i32)
    }

    /// <https://nodejs.org/api/buffer.html#static-method-bufferconcatlist-totallength>
    #[static_method(length = 2)]
    pub fn concat<'s>(
        scope: &'s Scope<'_>,
        list: HandleValue<'_>,
        total_length: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let array = Object::from_value(scope, *list)
            .ok()
            .filter(|array| js::Array::is_array(scope, array.handle()).unwrap_or(false))
            .ok_or_else(|| {
                NodeError::invalid_arg_type(scope, "list", "an instance of Array", list)
            })?;
        let count = to_number(scope, array.get_property(scope, c"length")?)? as u32;
        if count == 0 {
            return Ok(new_buffer(scope, &[])?);
        }
        let total = match total_length.filter(|total| !total.is_undefined()) {
            None => None,
            Some(total) => Some(crate::errors::validate_integer(
                scope, total, "length", 0.0, MAX_LENGTH,
            )? as usize),
        };
        let mut views = Vec::new();
        for index in 0..count {
            let item = array.get_element(scope, index)?;
            let view = as_view(scope, item)
                .filter(|view| view.view_kind() == js::typedarray::ViewKind::Uint8)
                .ok_or_else(|| {
                    let name = format!("list[{index}]");
                    let expected = "an instance of Buffer or Uint8Array";
                    NodeError::invalid_arg_type(scope, &name, expected, item)
                })?;
            views.push(view);
        }
        let length =
            total.unwrap_or_else(|| views.iter().map(|view| view.byte_length()).sum::<usize>());
        let buffer = new_zeroed_buffer(scope, length)?;
        let target = ArrayBufferView::from_object(buffer).ok_or(ExnThrown)?;
        let mut position = 0;
        for view in views {
            if position == length {
                break;
            }
            let count = view.byte_length().min(length - position);
            target.copy_region_from(position, view, 0, count);
            position += count;
        }
        Ok(buffer)
    }

    /// <https://nodejs.org/api/buffer.html#static-method-bufferisbufferobj>
    #[static_method]
    pub fn is_buffer(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, Thrown> {
        match Object::from_value(scope, *value) {
            Ok(object) => Ok(super::is_buffer(scope, object)?),
            Err(_) => Ok(false),
        }
    }

    /// <https://nodejs.org/api/buffer.html#static-method-bufferisencodingencoding>
    #[static_method]
    pub fn is_encoding(scope: &Scope<'_>, encoding: HandleValue<'_>) -> bool {
        encoding.is_string()
            && String::from_jsval(scope, encoding, ())
                .is_ok_and(|name| Encoding::parse(&name).is_some())
    }

    /// <https://nodejs.org/api/buffer.html#buftostringencoding-start-end>: `start` and `end` are
    /// converted to numbers and clamped to the buffer, and a range that ends before it starts is
    /// empty.
    #[method(length = 3)]
    pub fn to_string<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        encoding: Option<HandleValue<'_>>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        let view = this_view(this)?;
        let length = view.byte_length() as f64;
        let start = match start {
            None => 0.0,
            Some(start) => to_number(scope, start)?,
        };
        let start = if start <= 0.0 || start.is_nan() {
            0.0
        } else if start >= length {
            return Ok(js::JSString::empty(scope));
        } else {
            start.trunc()
        };
        let end = match end.filter(|end| !end.is_undefined()) {
            None => length,
            Some(end) => to_number(scope, end)?,
        };
        let end = if end > length {
            length
        } else if end.is_nan() {
            0.0
        } else {
            end.trunc()
        };
        if end <= start {
            return Ok(js::JSString::empty(scope));
        }
        let encoding = match encoding.filter(|encoding| !encoding.is_undefined()) {
            None => Encoding::Utf8,
            Some(encoding) => lookup_encoding(scope, encoding)?
                .ok_or_else(|| unknown_encoding(scope, encoding))?,
        };
        // The conversions can run JS that shrinks the buffer.
        let end = (end as usize).min(view.byte_length());
        let start = (start as usize).min(end);
        let bytes = view.with_bytes(|bytes| bytes[start..end].to_vec());
        Ok(decode_bytes(scope, &bytes, encoding)?)
    }

    /// <https://nodejs.org/api/buffer.html#bufwritestring-offset-length-encoding>: `offset` or
    /// `length` may be the encoding instead. Only whole characters are written.
    #[method(length = 4)]
    pub fn write(
        scope: &Scope<'_>,
        this: Object<'_>,
        string: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        let view = this_view(this)?;
        let capacity = view.byte_length();
        let (offset, length, encoding) = match (defined(offset), defined(length)) {
            (None, _) => (0, capacity, None),
            (Some(offset), None) if offset.is_string() => (0, capacity, Some(offset)),
            (Some(offset), length) => {
                let offset =
                    crate::errors::validate_integer(scope, offset, "offset", 0.0, capacity as f64)?
                        as usize;
                let remaining = capacity - offset;
                match length {
                    None => (offset, remaining, encoding),
                    Some(length) if length.is_string() => (offset, remaining, Some(length)),
                    Some(length) => {
                        let length = crate::errors::validate_integer(
                            scope,
                            length,
                            "length",
                            0.0,
                            capacity as f64,
                        )?;
                        (offset, (length as usize).min(remaining), encoding)
                    }
                }
            }
        };
        let encoding = encoding_or_utf8(scope, encoding)?;
        Ok(write_string(scope, view, string, offset, length, encoding)? as f64)
    }

    /// <https://nodejs.org/api/buffer.html#static-method-buffercopybytesfromview-offset-length>:
    /// a copy of the elements of the typed array `view` from `offset`, `length` of them or to its
    /// end.
    #[static_method(length = 3)]
    pub fn copy_bytes_from<'s>(
        scope: &'s Scope<'_>,
        view: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let typed = as_view(scope, view)
            .filter(|typed| typed.view_kind() != js::typedarray::ViewKind::DataView)
            .ok_or_else(|| {
                NodeError::invalid_arg_type(scope, "view", "an instance of TypedArray", view)
            })?;
        let element_size = typed.view_kind().element_size();
        let count = typed.byte_length() / element_size;
        if count == 0 {
            return Ok(new_buffer(scope, &[])?);
        }
        let mut start = 0;
        if let Some(offset) = defined(offset) {
            let offset = crate::errors::validate_integer(scope, offset, "offset", 0.0, MAX_OFFSET)?;
            if offset >= count as f64 {
                return Ok(new_buffer(scope, &[])?);
            }
            start = offset as usize;
        }
        let mut end = count;
        if let Some(length) = defined(length) {
            let length = crate::errors::validate_integer(scope, length, "length", 0.0, MAX_OFFSET)?;
            end = (start as f64 + length).min(count as f64) as usize;
        }
        if end <= start {
            return Ok(new_buffer(scope, &[])?);
        }
        let bytes =
            typed.with_bytes(|bytes| bytes[start * element_size..end * element_size].to_vec());
        Ok(new_buffer(scope, &bytes)?)
    }

    /// Node.js's `utf8Slice(start, end)`.
    #[method(length = 2)]
    pub fn utf8_slice<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        slice_as(scope, this, start, end, Encoding::Utf8)
    }

    /// Node.js's `ucs2Slice(start, end)`.
    #[method(length = 2)]
    pub fn ucs2_slice<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        slice_as(scope, this, start, end, Encoding::Utf16le)
    }

    /// Node.js's `latin1Slice(start, end)`.
    #[method(length = 2)]
    pub fn latin1_slice<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        slice_as(scope, this, start, end, Encoding::Latin1)
    }

    /// Node.js's `asciiSlice(start, end)`.
    #[method(length = 2)]
    pub fn ascii_slice<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        slice_as(scope, this, start, end, Encoding::Ascii)
    }

    /// Node.js's `base64Slice(start, end)`.
    #[method(length = 2)]
    pub fn base64_slice<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        slice_as(scope, this, start, end, Encoding::Base64)
    }

    /// Node.js's `base64urlSlice(start, end)`.
    #[method(length = 2)]
    pub fn base64url_slice<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        slice_as(scope, this, start, end, Encoding::Base64Url)
    }

    /// Node.js's `hexSlice(start, end)`.
    #[method(length = 2)]
    pub fn hex_slice<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        slice_as(scope, this, start, end, Encoding::Hex)
    }

    /// Node.js's `utf8Write(string, offset, length)`.
    #[method(length = 3)]
    pub fn utf8_write(
        scope: &Scope<'_>,
        this: Object<'_>,
        string: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        write_as(scope, this, string, offset, length, Encoding::Utf8)
    }

    /// Node.js's `ucs2Write(string, offset, length)`.
    #[method(length = 3)]
    pub fn ucs2_write(
        scope: &Scope<'_>,
        this: Object<'_>,
        string: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        write_as(scope, this, string, offset, length, Encoding::Utf16le)
    }

    /// Node.js's `latin1Write(string, offset, length)`.
    #[method(length = 3)]
    pub fn latin1_write(
        scope: &Scope<'_>,
        this: Object<'_>,
        string: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        write_as(scope, this, string, offset, length, Encoding::Latin1)
    }

    /// Node.js's `asciiWrite(string, offset, length)`.
    #[method(length = 3)]
    pub fn ascii_write(
        scope: &Scope<'_>,
        this: Object<'_>,
        string: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        write_as(scope, this, string, offset, length, Encoding::Ascii)
    }

    /// Node.js's `base64Write(string, offset, length)`.
    #[method(length = 3)]
    pub fn base64_write(
        scope: &Scope<'_>,
        this: Object<'_>,
        string: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        write_as(scope, this, string, offset, length, Encoding::Base64)
    }

    /// Node.js's `base64urlWrite(string, offset, length)`.
    #[method(length = 3)]
    pub fn base64url_write(
        scope: &Scope<'_>,
        this: Object<'_>,
        string: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        write_as(scope, this, string, offset, length, Encoding::Base64Url)
    }

    /// Node.js's `hexWrite(string, offset, length)`.
    #[method(length = 3)]
    pub fn hex_write(
        scope: &Scope<'_>,
        this: Object<'_>,
        string: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        length: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        write_as(scope, this, string, offset, length, Encoding::Hex)
    }

    /// <https://nodejs.org/api/buffer.html#buftojson>
    #[method(name = "toJSON")]
    pub fn to_json<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<Object<'s>, Thrown> {
        let bytes = this_view(this)?.copy_bytes(scope)?;
        let json = Object::new_plain(scope)?;
        json.set_property(scope, c"type", "Buffer")?;
        let data: Vec<f64> = bytes.iter().map(|byte| f64::from(*byte)).collect();
        json.set_property(scope, c"data", js::Array::with_contents(scope, &data)?)?;
        Ok(json)
    }

    /// <https://nodejs.org/api/buffer.html#bufequalsotherbuffer>
    pub fn equals(
        scope: &Scope<'_>,
        this: Object<'_>,
        other: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let other = uint8_array_arg(scope, other, "otherBuffer")?;
        let mine = this_view(this)?;
        Ok(mine.with_bytes_pair(other, |mine, other| mine == other))
    }

    /// <https://nodejs.org/api/buffer.html#bufcomparetarget-targetstart-targetend-sourcestart-sourceend>
    #[method(length = 5)]
    pub fn compare(
        scope: &Scope<'_>,
        this: Object<'_>,
        target: HandleValue<'_>,
        target_start: Option<HandleValue<'_>>,
        target_end: Option<HandleValue<'_>>,
        source_start: Option<HandleValue<'_>>,
        source_end: Option<HandleValue<'_>>,
    ) -> Result<i32, Thrown> {
        let target_view = uint8_array_arg(scope, target, "target")?;
        let source_view = this_view(this)?;
        let offset = |value: Option<HandleValue<'_>>, name, max: usize, default: usize| match value
            .filter(|value| !value.is_undefined())
        {
            None => Ok(default),
            Some(value) => crate::errors::validate_integer(scope, value, name, 0.0, max as f64)
                .map(|offset| offset as usize),
        };
        let max = MAX_OFFSET as usize;
        let target_start = offset(target_start, "targetStart", max, 0)?;
        let (target_length, source_length) = (target_view.byte_length(), source_view.byte_length());
        let target_end = offset(target_end, "targetEnd", target_length, target_length)?;
        let source_start = offset(source_start, "sourceStart", max, 0)?;
        let source_end = offset(source_end, "sourceEnd", source_length, source_length)?;
        if source_start >= source_end {
            return Ok(if target_start >= target_end { 0 } else { -1 });
        }
        if target_start >= target_end {
            return Ok(1);
        }
        let order = source_view.with_bytes_pair(target_view, |source, target| {
            source[source_start..source_end].cmp(&target[target_start..target_end])
        });
        Ok(order as i32)
    }

    /// <https://nodejs.org/api/buffer.html#bufcopytarget-targetstart-sourcestart-sourceend>: the
    /// positions are converted to integers, and the copy stops at the end of either buffer.
    #[method(length = 4)]
    pub fn copy(
        scope: &Scope<'_>,
        this: Object<'_>,
        target: HandleValue<'_>,
        target_start: Option<HandleValue<'_>>,
        source_start: Option<HandleValue<'_>>,
        source_end: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        let source = this_view(this)?.copy_bytes(scope)?;
        let target = as_view(scope, target).ok_or_else(|| {
            NodeError::invalid_arg_type(
                scope,
                "target",
                "an instance of Buffer or Uint8Array",
                target,
            )
        })?;
        let integer = |value: Option<HandleValue<'_>>, default: f64| match value
            .filter(|value| !value.is_undefined())
        {
            None => Ok((default, false)),
            Some(value) => to_integer(scope, value, 0.0).map(|number| (number, true)),
        };
        let (target_start, _) = integer(target_start, 0.0)?;
        if target_start < 0.0 {
            return Err(NodeError::out_of_range(
                "targetStart",
                ">= 0",
                &crate::errors::out_of_range_received(target_start),
            )
            .into());
        }
        let (source_start, _) = integer(source_start, 0.0)?;
        if source_start < 0.0 || source_start > source.len() as f64 {
            let range = format!(">= 0 && <= {}", source.len());
            return Err(NodeError::out_of_range(
                "sourceStart",
                &range,
                &crate::errors::out_of_range_received(source_start),
            )
            .into());
        }
        let (source_end, _) = integer(source_end, source.len() as f64)?;
        if source_end < 0.0 {
            return Err(NodeError::out_of_range(
                "sourceEnd",
                ">= 0",
                &crate::errors::out_of_range_received(source_end),
            )
            .into());
        }
        let target_length = target.byte_length() as f64;
        if target_start >= target_length || source_start >= source_end {
            return Ok(0.0);
        }
        let count = (source_end - source_start)
            .min(target_length - target_start)
            .min(source.len() as f64 - source_start);
        if count <= 0.0 {
            return Ok(0.0);
        }
        let (target_start, source_start, count) =
            (target_start as usize, source_start as usize, count as usize);
        target.with_bytes_mut(|target| {
            target[target_start..target_start + count]
                .copy_from_slice(&source[source_start..source_start + count])
        });
        Ok(count as f64)
    }

    /// <https://nodejs.org/api/buffer.html#bufsubarraystart-end>
    #[method(length = 2)]
    pub fn subarray<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let view = this_view(this)?;
        let length = view.byte_length();
        let undefined = scope.root_value(js::value::undefined());
        let start = adjust_offset(scope, start.unwrap_or(undefined), length)?;
        let end = match defined(end) {
            Some(end) => adjust_offset(scope, end, length)?,
            None => length,
        }
        .max(start);
        let buffer = view.viewed_buffer(scope)?;
        Ok(buffer_view(
            scope,
            buffer,
            view.byte_offset() + start,
            end - start,
        )?)
    }

    /// <https://nodejs.org/api/buffer.html#bufslicestart-end>: the same as `subarray`, a view of
    /// the same memory.
    #[method(length = 2)]
    pub fn slice<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        subarray(scope, this, start, end)
    }

    /// <https://nodejs.org/api/buffer.html#buffillvalue-offset-end-encoding>
    #[method(length = 4)]
    pub fn fill<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        value: HandleValue<'_>,
        offset: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let view = this_view(this)?;
        let plan = fill_plan(scope, view.byte_length(), value, offset, end, encoding)?;
        if let Some(plan) = plan {
            // The conversions can run JS that shrinks or detaches the buffer, which leaves
            // nothing to fill.
            if plan.stop > view.byte_length() {
                return Ok(this);
            }
            view.with_bytes_mut(|bytes| plan.apply(bytes));
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/buffer.html#bufindexofvalue-byteoffset-encoding>
    #[method(length = 4)]
    pub fn index_of(
        scope: &Scope<'_>,
        this: Object<'_>,
        value: HandleValue<'_>,
        byte_offset: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        Ok(super::index_of(scope, this, value, byte_offset, end, encoding, true)? as f64)
    }

    /// <https://nodejs.org/api/buffer.html#buflastindexofvalue-byteoffset-encoding>
    #[method(length = 4)]
    pub fn last_index_of(
        scope: &Scope<'_>,
        this: Object<'_>,
        value: HandleValue<'_>,
        byte_offset: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<f64, Thrown> {
        Ok(super::index_of(scope, this, value, byte_offset, end, encoding, false)? as f64)
    }

    /// <https://nodejs.org/api/buffer.html#bufincludesvalue-byteoffset-encoding>
    #[method(length = 4)]
    pub fn includes(
        scope: &Scope<'_>,
        this: Object<'_>,
        value: HandleValue<'_>,
        byte_offset: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        Ok(super::index_of(scope, this, value, byte_offset, end, encoding, true)? != -1)
    }

    /// <https://nodejs.org/api/buffer.html#bufswap16>
    pub fn swap16(this: Object<'_>) -> Result<Object<'_>, Thrown> {
        swap(this, 2)
    }

    /// <https://nodejs.org/api/buffer.html#bufswap32>
    pub fn swap32(this: Object<'_>) -> Result<Object<'_>, Thrown> {
        swap(this, 4)
    }

    /// <https://nodejs.org/api/buffer.html#bufswap64>
    pub fn swap64(this: Object<'_>) -> Result<Object<'_>, Thrown> {
        swap(this, 8)
    }

    /// Define the `read*` and `write*` methods, `toLocaleString`, and `poolSize`, and make
    /// `Buffer` inherit from `Uint8Array`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let uint8_array =
            js::class::get_class_object(scope, js::class_spec::JSProtoKey::JSProto_Uint8Array)?;
        let uint8_array = Object::from_handle(uint8_array).ok_or(ExnThrown)?;
        let uint8_proto = uint8_array.get_property(scope, c"prototype")?;
        let uint8_proto = Object::from_value(scope, uint8_proto).map_err(|e| e.throw(scope))?;
        ctor.set_prototype(scope, uint8_array.handle())?;
        proto.set_prototype(scope, uint8_proto.handle())?;
        // Node.js assigns these members, so they are enumerable like the macro-defined methods.
        let enumerable = js::class_spec::JSPROP_ENUMERATE as u32;
        for (index, method) in NUMBER_METHODS.iter().enumerate() {
            // The `Uint` spellings are the same functions as the `UInt` ones, which come first.
            let name = method.name.to_str().unwrap_or_default();
            if name.contains("Uint") {
                let canonical =
                    std::ffi::CString::new(name.replace("Uint", "UInt")).unwrap_or_default();
                let function = proto.get_property_by_name(scope, &canonical)?;
                proto.define_property(scope, method.name, function, enumerable)?;
                continue;
            }
            let body = if method.write {
                write_number
            } else {
                read_number
            };
            let function =
                Function::new_callback(scope, method.name, method.length(), body, index as i32)?;
            proto.define_property(scope, method.name, *function, enumerable)?;
        }
        for (name, body) in [
            (c"parent", parent as js::function::Callback),
            (c"offset", offset),
        ] {
            let getter = Function::new_callback(scope, name, 0, body, ())?;
            proto.define_accessor(scope, name, Some(*getter), None, enumerable)?;
        }
        let to_string = proto.get_property(scope, c"toString")?;
        proto.define_property(scope, c"toLocaleString", to_string, enumerable)?;
        let inspect = Function::new_callback(scope, c"inspect", 2, inspect_buffer, ())?;
        proto.define_property(scope, c"inspect", *inspect, enumerable)?;
        let custom = crate::inspect::INSPECT_CUSTOM.id(scope)?;
        proto.set_property_by_id(scope, custom, *inspect)?;
        let of = Function::new_callback(scope, c"of", 0, buffer_of, ())?;
        ctor.define_property(scope, c"of", *of, enumerable)?;
        ctor.set_property(scope, c"poolSize", 8192)?;
        Ok(())
    }
}

/// <https://nodejs.org/api/buffer.html#static-method-bufferofitems>: a `Buffer` of `items`, each
/// converted as a `Uint8Array` element.
fn buffer_of(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let buffer = new_buffer(scope, &vec![0; args.len() as usize])?;
    for index in 0..args.len() {
        buffer.set_element(scope, index, args.get(index))?;
    }
    buffer.to_jsval_raw_throwing(scope)
}

/// `buf.inspect()`: the text `util.inspect` shows for the buffer.
fn inspect_buffer(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let text = match Object::from_value(scope, *this) {
        Ok(object) if ArrayBufferView::from_object(object).is_some() => {
            crate::inspect::inspect_buffer(scope, object)?
        }
        _ => crate::inspect::inspect(scope, this, Default::default())?,
    };
    text.as_str().to_jsval_raw_throwing(scope)
}

/// Swap the byte order of each `width`-byte group of `this`, which must be a multiple of `width`
/// bytes long.
fn swap(this: Object<'_>, width: usize) -> Result<Object<'_>, Thrown> {
    let view = this_view(this)?;
    if view.byte_length() % width != 0 {
        return Err(NodeError::new(
            ErrorKind::RangeError,
            "ERR_INVALID_BUFFER_SIZE",
            format!("Buffer size must be a multiple of {}-bits", width * 8),
        )
        .into());
    }
    view.with_bytes_mut(|bytes| {
        for group in bytes.chunks_exact_mut(width) {
            group.reverse();
        }
    });
    Ok(this)
}

/// `buffer.isUtf8(input)` and `buffer.isAscii(input)`.
fn check_bytes(
    scope: &Scope<'_>,
    input: HandleValue<'_>,
    check: fn(&[u8]) -> bool,
) -> Result<bool, Thrown> {
    let object = Object::from_value(scope, *input).ok();
    if let Some(view) = object.and_then(ArrayBufferView::from_object) {
        return Ok(view.with_bytes(check));
    }
    if let Some(buffer) = object.and_then(|object| object.cast::<js::ArrayBuffer>().ok()) {
        return Ok(buffer.with_bytes(check));
    }
    Err(NodeError::invalid_arg_type(
        scope,
        "input",
        "an instance of ArrayBuffer, Buffer, or TypedArray",
        input,
    )
    .into())
}

#[core_runtime::jsmodule(node_args, name = "node:buffer", default_namespace)]
pub mod module {
    use super::*;

    pub use super::buffer as Buffer;

    /// <https://nodejs.org/api/buffer.html#bufferkmaxlength>
    #[allow(non_upper_case_globals)]
    pub const kMaxLength: f64 = MAX_LENGTH;

    /// <https://nodejs.org/api/buffer.html#bufferkstringmaxlength>
    #[allow(non_upper_case_globals)]
    pub const kStringMaxLength: f64 = MAX_STRING_LENGTH;

    /// <https://nodejs.org/api/buffer.html#bufferinspect_max_bytes>
    pub const INSPECT_MAX_BYTES: f64 = 50.0;

    /// <https://nodejs.org/api/buffer.html#buffertranscodesource-fromenc-toenc>: between UTF-8,
    /// UTF-16LE, Latin-1 and ASCII, with Node.js's conversions:
    ///
    /// - Latin-1 or ASCII to UTF-16LE widens each byte.
    /// - UTF-8 to UTF-16LE, and UTF-16LE to UTF-8, fail with `U_INVALID_CHAR_FOUND` on invalid input,
    ///   including a lone surrogate or a UTF-16LE source shorter than two bytes.
    /// - The other conversions decode invalid input, such as an ASCII byte above 127, as U+FFFD,
    ///   and encode characters the target encoding can't represent as `?`. A UTF-16LE source's
    ///   trailing odd byte is U+FFFD in UTF-16LE output and dropped otherwise.
    pub fn transcode<'s>(
        scope: &'s Scope<'_>,
        source: HandleValue<'_>,
        from_encoding: HandleValue<'_>,
        to_encoding: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let source = uint8_array_arg(scope, source, "source")?.copy_bytes(scope)?;
        if source.is_empty() {
            return Ok(new_buffer(scope, &[])?);
        }
        // `undefined` and `null` select UTF-8, as Node.js's `normalizeEncoding` maps them.
        let encoding = |value: HandleValue<'_>| -> Result<Option<Encoding>, Thrown> {
            if value.is_undefined() || value.is_null() {
                return Ok(Some(Encoding::Utf8));
            }
            Ok(lookup_encoding(scope, value)?.filter(|encoding| {
                matches!(
                    encoding,
                    Encoding::Utf8 | Encoding::Utf16le | Encoding::Latin1 | Encoding::Ascii
                )
            }))
        };
        // ICU's error number is the `errno`: 1 for `U_ILLEGAL_ARGUMENT_ERROR`, and 10 for
        // `U_INVALID_CHAR_FOUND`.
        let error = |code: &'static str| -> Thrown {
            let message = format!("Unable to transcode Buffer [{code}]");
            let errno = if code == "U_ILLEGAL_ARGUMENT_ERROR" {
                1
            } else {
                10
            };
            let thrown = NodeError::new(ErrorKind::Error, code, message).throw(scope);
            if let Some(exception) = js::exception::take_pending(scope) {
                let exception = scope.root_value(exception.get());
                if let Ok(object) = Object::from_value(scope, exception) {
                    let _ = object.set_property(scope, c"errno", errno);
                }
                let capture = js::native::ExceptionStackBehavior::DoNotCapture;
                js::exception::set_pending(scope, exception, capture);
            }
            let ExnThrown = thrown;
            Thrown::Pending
        };
        let (Some(from), Some(to)) = (encoding(from_encoding)?, encoding(to_encoding)?) else {
            return Err(error("U_ILLEGAL_ARGUMENT_ERROR"));
        };
        let utf16_bytes = |units: &[u16]| -> Vec<u8> {
            units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
        };
        let bytes = match (from, to) {
            (Encoding::Latin1 | Encoding::Ascii, Encoding::Utf16le) => {
                source.iter().flat_map(|byte| [*byte, 0]).collect()
            }
            (Encoding::Utf8, Encoding::Utf16le) => {
                let text =
                    std::str::from_utf8(&source).map_err(|_| error("U_INVALID_CHAR_FOUND"))?;
                utf16_bytes(&text.encode_utf16().collect::<Vec<_>>())
            }
            (Encoding::Utf16le, Encoding::Utf8) => {
                let units = utf16_units(&source);
                if units.is_empty() {
                    return Err(error("U_INVALID_CHAR_FOUND"));
                }
                String::from_utf16(&units)
                    .map_err(|_| error("U_INVALID_CHAR_FOUND"))?
                    .into_bytes()
            }
            _ => {
                let mut units: Vec<u16> = match from {
                    Encoding::Utf8 => String::from_utf8_lossy(&source).encode_utf16().collect(),
                    Encoding::Utf16le => utf16_units(&source),
                    Encoding::Ascii => source
                        .iter()
                        .map(|byte| {
                            if byte.is_ascii() {
                                u16::from(*byte)
                            } else {
                                0xfffd
                            }
                        })
                        .collect(),
                    _ => source.iter().map(|byte| u16::from(*byte)).collect(),
                };
                match to {
                    Encoding::Utf16le => {
                        // Lone surrogates and a trailing odd byte decode as U+FFFD.
                        units = char::decode_utf16(units.iter().copied())
                            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                            .flat_map(|c| {
                                let mut buffer = [0; 2];
                                c.encode_utf16(&mut buffer).to_vec()
                            })
                            .collect();
                        if source.len() % 2 == 1 {
                            units.push(0xfffd);
                        }
                        utf16_bytes(&units)
                    }
                    Encoding::Utf8 => String::from_utf16_lossy(&units).into_bytes(),
                    // Each character the encoding can't represent, including a surrogate pair,
                    // becomes one `?`, except that default-ignorable characters are dropped.
                    _ => char::decode_utf16(units.iter().copied())
                        .filter_map(|c| match c.map(u32::from) {
                            Ok(code) if code < 0x80 || (to == Encoding::Latin1 && code < 0x100) => {
                                Some(code as u8)
                            }
                            Ok(code) if is_default_ignorable(code) => None,
                            _ => Some(b'?'),
                        })
                        .collect(),
                }
            }
        };
        Ok(new_buffer(scope, &bytes)?)
    }

    /// <https://nodejs.org/api/buffer.html#bufferisutf8input>
    pub fn is_utf8(scope: &Scope<'_>, input: HandleValue<'_>) -> Result<bool, Thrown> {
        check_bytes(scope, input, |bytes| std::str::from_utf8(bytes).is_ok())
    }

    /// <https://nodejs.org/api/buffer.html#bufferisasciiinput>
    pub fn is_ascii(scope: &Scope<'_>, input: HandleValue<'_>) -> Result<bool, Thrown> {
        check_bytes(scope, input, <[u8]>::is_ascii)
    }
}

/// Register `node:buffer`, whose `Blob`, `File`, `atob`, `btoa` and `constants` are the global's
/// `Blob`, `File`, `atob` and `btoa` and an object with `MAX_LENGTH` and `MAX_STRING_LENGTH`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let overrides = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let constants = Object::new_plain(scope)?;
        constants.set_property(scope, c"MAX_LENGTH", MAX_LENGTH)?;
        constants.set_property(scope, c"MAX_STRING_LENGTH", MAX_STRING_LENGTH)?;
        Ok(vec![
            (
                "Blob",
                js::class::class_constructor::<web_files::blob::BlobImpl>(scope)?
                    .to_jsval_throwing(scope)?,
            ),
            (
                "File",
                js::class::class_constructor::<web_files::file::FileImpl>(scope)?
                    .to_jsval_throwing(scope)?,
            ),
            ("SlowBuffer", slow_buffer_function(scope)?),
            (
                "atob",
                web_globals::base64::atob_function(scope)
                    .ok_or(ExnThrown)?
                    .to_jsval_throwing(scope)?,
            ),
            (
                "btoa",
                web_globals::base64::btoa_function(scope)
                    .ok_or(ExnThrown)?
                    .to_jsval_throwing(scope)?,
            ),
            ("constants", constants.to_jsval_throwing(scope)?),
        ])
    };
    let Ok(overrides) = overrides() else {
        return false;
    };
    if !crate::stub::register_implemented_with::<module::js_module>(scope, name, &overrides) {
        return false;
    }
    define_inspect_max_bytes(scope, name).is_ok()
}

/// The body of `buffer.SlowBuffer(size)`: a new zero-filled `Buffer` of `size` bytes.
fn slow_buffer(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let size = if !args.is_empty() {
        args.get(0)
    } else {
        HandleValue::undefined()
    };
    let size = size_arg(scope, size).map_err(|error| error.throw(scope))?;
    new_zeroed_buffer(scope, size)?.to_jsval_raw_throwing(scope)
}

/// `buffer.SlowBuffer`: [`slow_buffer`] deprecated with DEP0030, as `util.deprecate` wraps it.
fn slow_buffer_function<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let function = Function::new_constructor_callback(scope, c"SlowBuffer", 1, slow_buffer, ())?;
    let util = crate::internal_modules::module_default(scope, "node:util")?;
    let wrapper = Function::call_by_name(
        scope,
        util.handle(),
        c"deprecate",
        &[
            function.to_jsval_throwing(scope)?,
            js::class::get_or_init_atom(
                scope,
                c"SlowBuffer() is deprecated. Please use Buffer.allocUnsafeSlow()",
            )?,
            js::class::get_or_init_atom(scope, c"DEP0030")?,
        ],
    )?;
    Ok(scope.root_value(wrapper.get()))
}

js::instance_local! {
    /// `buffer.INSPECT_MAX_BYTES`.
    static INSPECT_MAX: std::cell::Cell<f64> = const { std::cell::Cell::new(50.0) };
}

/// The number of bytes `util.inspect` shows of a buffer, `buffer.INSPECT_MAX_BYTES`.
pub(crate) fn inspect_max_bytes() -> f64 {
    INSPECT_MAX.get()
}

/// Make `INSPECT_MAX_BYTES` of builtin module `name`'s `default` export an accessor, whose setter
/// validates the value as Node.js's `validateNumber(value, 'INSPECT_MAX_BYTES', 0)` does.
fn define_inspect_max_bytes(scope: &Scope<'_>, name: &str) -> Result<(), ExnThrown> {
    let namespace = core_runtime::module::builtin_namespace(scope, name)?.ok_or(ExnThrown)?;
    let default = namespace.get_property(scope, c"default")?;
    let default = Object::from_value(scope, default).map_err(|e| e.throw(scope))?;
    let getter = Function::new_callback(scope, c"get", 0, get_inspect_max_bytes, ())?;
    let setter = Function::new_callback(scope, c"set", 1, set_inspect_max_bytes, ())?;
    let attrs = js::class_spec::JSPROP_ENUMERATE as u32;
    default.define_accessor(
        scope,
        c"INSPECT_MAX_BYTES",
        Some(*getter),
        Some(*setter),
        attrs,
    )
}

fn get_inspect_max_bytes(
    _: &Scope<'_>,
    _: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::from_f64(INSPECT_MAX.get()))
}

fn set_inspect_max_bytes(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let number = crate::errors::validate_number(scope, args.get(0), "INSPECT_MAX_BYTES", 0.0)
        .map_err(|error| error.throw(scope))?;
    INSPECT_MAX.set(number);
    Ok(js::value::undefined())
}
