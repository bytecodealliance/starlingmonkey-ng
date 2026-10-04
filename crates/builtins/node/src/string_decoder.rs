// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:string_decoder`. See <https://nodejs.org/api/string_decoder.html>.
//!
//! A `StringDecoder` decodes a stream of byte chunks. Bytes at the end of a chunk that leave a
//! character (UTF-8 and UTF-16) or a base64 group incomplete are buffered until the next chunk
//! completes it. The decoder follows Node.js's `src/string_decoder.cc`, including where that
//! differs from decoding the chunks' concatenation: a UTF-16 high surrogate completed from an odd
//! byte is emitted before its low surrogate arrives, and a UTF-8 sequence that the next chunk ends
//! early decodes as replacement characters.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{ArrayBufferView, Object};

use crate::buffer::{encode_base64, Encoding};
use crate::errors::{ErrorKind, NodeError, Thrown};

/// The size of the buffer for an incomplete character or base64 group.
const INCOMPLETE_LENGTH: usize = 4;

/// `bytes` decoded as `encoding`, as UTF-16 code units. Invalid UTF-8 decodes as replacement
/// characters.
fn decode_units(bytes: &[u8], encoding: Encoding) -> Vec<u16> {
    match encoding {
        Encoding::Utf8 => String::from_utf8_lossy(bytes).encode_utf16().collect(),
        Encoding::Utf16le => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect(),
        Encoding::Latin1 => bytes.iter().map(|byte| u16::from(*byte)).collect(),
        Encoding::Ascii => bytes.iter().map(|byte| u16::from(byte & 0x7f)).collect(),
        Encoding::Base64 | Encoding::Base64Url => {
            encode_base64(bytes, encoding == Encoding::Base64Url)
                .encode_utf16()
                .collect()
        }
        Encoding::Hex => bytes
            .iter()
            .flat_map(|byte| format!("{byte:02x}").encode_utf16().collect::<Vec<_>>())
            .collect(),
    }
}

/// The canonical name of `encoding`, as `normalizeEncoding` returns it.
fn encoding_name(encoding: Encoding) -> &'static str {
    match encoding {
        Encoding::Utf8 => "utf8",
        Encoding::Utf16le => "utf16le",
        Encoding::Latin1 => "latin1",
        Encoding::Ascii => "ascii",
        Encoding::Base64 => "base64",
        Encoding::Base64Url => "base64url",
        Encoding::Hex => "hex",
    }
}

/// The bytes of a `write` or `end` argument that is a `Buffer`, a typed array or a `DataView`.
fn chunk_bytes(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Vec<u8>, Thrown> {
    Object::from_value(scope, *value)
        .ok()
        .and_then(ArrayBufferView::from_object)
        .map(|view| view.copy_bytes(scope))
        .transpose()?
        .ok_or_else(|| {
            NodeError::invalid_arg_type(
                scope,
                "buf",
                "an instance of Buffer, TypedArray, or DataView",
                value,
            )
            .into()
        })
}

/// A decoder's state, which a `StringDecoder` holds in its `_starlingNativeDecoder` property.
#[core_runtime::jsclass(hidden)]
pub struct NativeDecoder {
    #[no_trace]
    encoding: Encoding,
    /// The bytes of an incomplete character or base64 group, `buffered` of them.
    #[no_trace]
    incomplete: [u8; INCOMPLETE_LENGTH],
    /// How many more bytes complete the character or group in `incomplete`.
    #[no_trace]
    missing: usize,
    #[no_trace]
    buffered: usize,
}

impl NativeDecoderImpl {
    /// Node.js's `StringDecoder::DecodeData`: decode `data`, after the bytes held back from the
    /// previous chunk, and buffer the bytes of an incomplete character or group at its end.
    fn decode(&mut self, mut data: &[u8]) -> Vec<u16> {
        let encoding = self.encoding;
        if !matches!(
            encoding,
            Encoding::Utf8 | Encoding::Utf16le | Encoding::Base64 | Encoding::Base64Url
        ) {
            return decode_units(data, encoding);
        }
        let mut prepend = None;
        if self.missing > 0 {
            if encoding == Encoding::Utf8 {
                // A byte that isn't a continuation byte ends the incomplete character early, and
                // starts a new one.
                let end = data.len().min(self.missing);
                if let Some(index) = data[..end].iter().position(|byte| byte & 0xc0 != 0x80) {
                    self.missing = 0;
                    self.incomplete[self.buffered..self.buffered + index]
                        .copy_from_slice(&data[..index]);
                    self.buffered += index;
                    data = &data[index..];
                }
            }
            let found = data.len().min(self.missing);
            self.incomplete[self.buffered..self.buffered + found].copy_from_slice(&data[..found]);
            data = &data[found..];
            self.missing -= found;
            self.buffered += found;
            if self.missing == 0 {
                prepend = Some(decode_units(&self.incomplete[..self.buffered], encoding));
                self.buffered = 0;
            }
        }
        if data.is_empty() {
            return prepend.unwrap_or_default();
        }
        let length = data.len();
        match encoding {
            Encoding::Utf8 if data[length - 1] & 0x80 != 0 => {
                // Find where the character the last byte belongs to starts.
                for index in (0..length).rev() {
                    self.buffered += 1;
                    if data[index] & 0xc0 == 0x80 {
                        // Four continuation bytes, or no lead byte, are invalid anyway.
                        if self.buffered >= INCOMPLETE_LENGTH || index == 0 {
                            self.buffered = 0;
                            break;
                        }
                        continue;
                    }
                    let lead = data[index];
                    self.missing = if lead & 0xe0 == 0xc0 {
                        2
                    } else if lead & 0xf0 == 0xe0 {
                        3
                    } else if lead & 0xf8 == 0xf0 {
                        4
                    } else {
                        self.buffered = 0;
                        break;
                    };
                    if self.buffered >= self.missing {
                        self.missing = 0;
                        self.buffered = 0;
                    }
                    self.missing -= self.buffered;
                    break;
                }
            }
            Encoding::Utf16le if length % 2 == 1 => {
                self.buffered = 1;
                self.missing = 1;
            }
            Encoding::Utf16le if data[length - 1] & 0xfc == 0xd8 => {
                self.buffered = 2;
                self.missing = 2;
            }
            Encoding::Base64 | Encoding::Base64Url => {
                self.buffered = length % 3;
                if self.buffered > 0 {
                    self.missing = 3 - self.buffered;
                }
            }
            _ => {}
        }
        let complete = length - self.buffered;
        self.incomplete[..self.buffered].copy_from_slice(&data[complete..]);
        let body = decode_units(&data[..complete], encoding);
        match prepend {
            Some(mut units) => {
                units.extend(body);
                units
            }
            None => body,
        }
    }

    /// Node.js's `StringDecoder::FlushData`: decode the buffered bytes, and discard them. A single
    /// byte of UTF-16 is dropped.
    fn flush(&mut self) -> Vec<u16> {
        if self.encoding == Encoding::Utf16le && self.buffered % 2 == 1 {
            self.missing = self.missing.saturating_sub(1);
            self.buffered -= 1;
        }
        let units = decode_units(&self.incomplete[..self.buffered], self.encoding);
        self.missing = 0;
        self.buffered = 0;
        units
    }
}

#[core_runtime::jsmethods]
impl NativeDecoder {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// The encoding a `StringDecoder` constructor argument selects: `undefined`, `null` and `""`
/// select UTF-8.
fn parse_encoding(
    scope: &Scope<'_>,
    encoding: Option<HandleValue<'_>>,
) -> Result<Encoding, Thrown> {
    let Some(name) = encoding.filter(|encoding| !encoding.is_undefined() && !encoding.is_null())
    else {
        return Ok(Encoding::Utf8);
    };
    let is_string = name.is_string();
    let name = js::JSString::from_value(scope, name)?.to_utf8(scope)?;
    let unknown = || {
        NodeError::new(
            ErrorKind::TypeError,
            "ERR_UNKNOWN_ENCODING",
            format!("Unknown encoding: {name}"),
        )
    };
    if !is_string {
        return Err(unknown().into());
    }
    if name.is_empty() {
        return Ok(Encoding::Utf8);
    }
    Ok(Encoding::parse(&name).ok_or_else(unknown)?)
}

/// The decoder state `this` holds, or `ERR_INVALID_THIS` if it holds none.
fn native_decoder<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<NativeDecoder<'s>, Thrown> {
    let native = this.get_property(scope, c"_starlingNativeDecoder")?;
    Object::from_value(scope, *native)
        .ok()
        .and_then(|native| native.cast::<NativeDecoder>().ok())
        .ok_or_else(|| {
            NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_THIS",
                "Value of \"this\" must be of type StringDecoder",
            )
            .into()
        })
}

#[core_runtime::jsfunction_class(name = "StringDecoder")]
pub mod string_decoder_class {
    use super::*;

    /// <https://nodejs.org/api/string_decoder.html#new-stringdecoderencoding>: initialize `this`
    /// with the canonical name of the encoding in `encoding` and an empty decoder state.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let encoding = parse_encoding(scope, encoding)?;
        this.set_property(scope, c"encoding", encoding_name(encoding))?;
        let native =
            js::class::create_instance_with::<NativeDecoderImpl>(scope, |_| NativeDecoderImpl {
                encoding,
                incomplete: [0; INCOMPLETE_LENGTH],
                missing: 0,
                buffered: 0,
            })?;
        this.define_property(scope, c"_starlingNativeDecoder", native, 0)?;
        Ok(())
    }

    /// <https://nodejs.org/api/string_decoder.html#stringdecoderwritebuffer>: a string is returned
    /// as it is.
    pub fn write<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        buffer: HandleValue<'_>,
    ) -> Result<js::JSString<'s>, Thrown> {
        if buffer.is_string() {
            return Ok(js::JSString::from_value(scope, *buffer)?);
        }
        let bytes = chunk_bytes(scope, buffer)?;
        let native = native_decoder(scope, this)?;
        let units = native.data_mut().decode(&bytes);
        Ok(js::JSString::from_utf16(scope, &units)?)
    }

    /// <https://nodejs.org/api/string_decoder.html#stringdecoderendbuffer>: decodes `buffer` with
    /// `this.write`, then the buffered bytes.
    pub fn end<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        buffer: Option<HandleValue<'s>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        let text = match buffer.filter(|buffer| !buffer.is_undefined()) {
            Some(buffer) => {
                let text = js::Function::call_by_name(scope, this.handle(), c"write", &[buffer])?;
                js::JSString::from_value(scope, text)?
            }
            None => js::JSString::empty(scope),
        };
        let native = decoder_state(scope, this)?;
        if native.data().buffered == 0 {
            return Ok(text);
        }
        let units = native.data_mut().flush();
        Ok(text.concat(scope, js::JSString::from_utf16(scope, &units)?)?)
    }

    /// The legacy `text(buffer, offset)`: discard the buffered bytes, then decode `buffer` from
    /// `offset`, as its `slice` method returns it, with `this.write`.
    pub fn text<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        buffer: HandleValue<'s>,
        offset: HandleValue<'s>,
    ) -> Result<HandleValue<'s>, Thrown> {
        {
            let native = decoder_state(scope, this)?;
            let mut data = native.data_mut();
            data.missing = 0;
            data.buffered = 0;
        }
        let object = Object::from_value(scope, *buffer)?;
        let slice = object.get_property(scope, c"slice")?;
        let rest = js::Function::call(scope, object, slice, &[offset])?;
        Ok(js::Function::call_by_name(
            scope,
            this.handle(),
            c"write",
            &[rest],
        )?)
    }

    /// Define the `lastChar`, `lastNeed` and `lastTotal` getters, enumerable as Node.js's are.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        for (name, body) in [
            (c"lastChar", last_char as js::function::Callback),
            (c"lastNeed", last_need),
            (c"lastTotal", last_total),
        ] {
            let getter = js::Function::new_callback(scope, name, 0, body, ())?;
            let attrs = js::class_spec::JSPROP_ENUMERATE as u32;
            proto.define_accessor(scope, name, Some(*getter), None, attrs)?;
        }
        Ok(())
    }
}

/// The decoder state `this` holds, or a `TypeError` without a code if it holds none, as Node.js's
/// `end`, `text` and getters throw when they read the state of `undefined`.
fn decoder_state<'s>(
    scope: &'s Scope<'_>,
    this: Object<'_>,
) -> Result<NativeDecoder<'s>, ExnThrown> {
    let native = this.get_property(scope, c"_starlingNativeDecoder")?;
    Object::from_value(scope, *native)
        .ok()
        .and_then(|native| native.cast::<NativeDecoder>().ok())
        .ok_or_else(|| {
            js::error::TypeError("Cannot read properties of undefined".into()).throw(scope)
        })
}

/// The decoder state of the `this` of `args`, as [`decoder_state`] finds it.
fn this_decoder<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
) -> Result<NativeDecoder<'s>, ExnThrown> {
    let this = scope.root_value(args.this());
    let this = Object::from_value(scope, *this).map_err(|_| {
        js::error::TypeError("Cannot read properties of undefined".into()).throw(scope)
    })?;
    decoder_state(scope, this)
}

/// The `lastChar` getter: the buffered bytes, in a `Buffer` of four bytes.
fn last_char(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let incomplete = this_decoder(scope, &args)?.data().incomplete;
    crate::buffer::new_buffer(scope, &incomplete)?.to_jsval_raw_throwing(scope)
}

/// The `lastNeed` getter: how many more bytes complete the buffered character or group.
fn last_need(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let missing = this_decoder(scope, &args)?.data().missing;
    Ok(js::value::from_f64(missing as f64))
}

/// The `lastTotal` getter: the length of the buffered character or group once complete.
fn last_total(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let decoder = this_decoder(scope, &args)?;
    let data = decoder.data();
    Ok(js::value::from_f64((data.buffered + data.missing) as f64))
}

#[core_runtime::jsmodule(node_args, name = "node:string_decoder", default_namespace)]
pub mod module {
    pub use super::string_decoder_class as StringDecoder;
}
