// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:querystring`. See <https://nodejs.org/api/querystring.html>.
//!
//! The functions follow Node.js's `lib/querystring.js` and work on UTF-16 code units, so lone
//! surrogates pass through `parse` and `unescape` as they are.

use js::conversion::FromJSVal;
use js::error::{ExnThrown, ThrowException};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::buffer::MAX_STRING_LENGTH;
use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::support::{decode_uri_component, hex_digit, DecodeUriError};

/// Whether `escape` leaves `byte` as is: `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte)
}

/// The uppercase hex digits.
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// `querystring.escape` for a string without lone surrogates, which leaves `:` as is if `colon`,
/// or `None` if allocating the result fails.
pub fn escape_str(text: &str, colon: bool) -> Option<String> {
    let kept = |byte: u8| is_unreserved(byte) || (colon && byte == b':');
    let escaped = text.bytes().filter(|byte| !kept(*byte)).count();
    let mut out = String::new();
    out.try_reserve_exact(text.len().checked_add(escaped.checked_mul(2)?)?)
        .ok()?;
    for byte in text.bytes() {
        if kept(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0xf)] as char);
        }
    }
    Some(out)
}

/// Reserve room for `additional` more code units of a string in `out`. Throws V8's
/// `RangeError: Invalid string length` if the string would exceed [`MAX_STRING_LENGTH`], and
/// reports out of memory if the allocation fails.
fn reserve(scope: &Scope<'_>, out: &mut Vec<u16>, additional: usize) -> Result<(), ExnThrown> {
    if out.len().saturating_add(additional) as f64 > MAX_STRING_LENGTH {
        return Err(js::error::throw_range_error(
            scope,
            c"Invalid string length",
        ));
    }
    out.try_reserve(additional).map_err(|_| {
        js::exception::report_out_of_memory(scope);
        ExnThrown
    })
}

/// Append `units` to `out`, reserving room with [`reserve`].
fn append(scope: &Scope<'_>, out: &mut Vec<u16>, units: &[u16]) -> Result<(), ExnThrown> {
    reserve(scope, out, units.len())?;
    out.extend_from_slice(units);
    Ok(())
}

/// Report out of memory, for a failed allocation that returned `None`.
fn out_of_memory(scope: &Scope<'_>) -> ExnThrown {
    js::exception::report_out_of_memory(scope);
    ExnThrown
}

/// The UTF-16 code units of `value` converted to a string.
fn units(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Vec<u16>, ExnThrown> {
    js::JSString::from_value(scope, value)?.to_utf16_vec(scope)
}

/// A JS string of `units`, as a value.
fn string_value<'s>(scope: &'s Scope<'_>, units: &[u16]) -> Result<HandleValue<'s>, ExnThrown> {
    Ok(scope.root_value(js::JSString::from_utf16(scope, units)?.as_value()))
}

/// `ERR_INVALID_URI`, the `URIError` `escape` throws for a surrogate that ends its input.
fn invalid_uri() -> Thrown {
    NodeError::new(ErrorKind::UriError, "ERR_INVALID_URI", "URI malformed").into()
}

/// Node.js's `encodeStr` with the `noEscape` table: the UTF-8 bytes of `units` percent-encoded,
/// except the unreserved ASCII characters. A surrogate code unit is combined with the code unit
/// after it, whatever that is, and is an error at the end of `units`. The output is limited as
/// [`reserve`] limits it.
fn encode_units(scope: &Scope<'_>, units: &[u16]) -> Result<Vec<u16>, Thrown> {
    let mut out = Vec::new();
    reserve(scope, &mut out, units.len())?;
    let hex = |out: &mut Vec<u16>, byte: u32| {
        out.push(u16::from(b'%'));
        out.push(u16::from(HEX[(byte >> 4) as usize & 0xf]));
        out.push(u16::from(HEX[byte as usize & 0xf]));
    };
    let mut index = 0;
    while index < units.len() {
        // One step writes at most four escapes of three code units.
        reserve(scope, &mut out, 12)?;
        let unit = u32::from(units[index]);
        if unit < 0x80 {
            if is_unreserved(unit as u8) {
                out.push(unit as u16);
            } else {
                hex(&mut out, unit);
            }
        } else if unit < 0x800 {
            hex(&mut out, 0xc0 | (unit >> 6));
            hex(&mut out, 0x80 | (unit & 0x3f));
        } else if !(0xd800..0xe000).contains(&unit) {
            hex(&mut out, 0xe0 | (unit >> 12));
            hex(&mut out, 0x80 | ((unit >> 6) & 0x3f));
            hex(&mut out, 0x80 | (unit & 0x3f));
        } else {
            index += 1;
            let Some(next) = units.get(index) else {
                return Err(invalid_uri());
            };
            let code = 0x10000 + (((unit & 0x3ff) << 10) | (u32::from(*next) & 0x3ff));
            hex(&mut out, 0xf0 | (code >> 18));
            hex(&mut out, 0x80 | ((code >> 12) & 0x3f));
            hex(&mut out, 0x80 | ((code >> 6) & 0x3f));
            hex(&mut out, 0x80 | (code & 0x3f));
        }
        index += 1;
    }
    Ok(out)
}

/// Node.js's `unescapeBuffer`: `%XX` escapes decoded to bytes, and `+` to a space if
/// `decode_spaces`. Malformed escapes are kept, and other code units are stored as their low byte.
/// `None` if allocating the bytes fails.
fn unescape_buffer(units: &[u16], decode_spaces: bool) -> Option<Vec<u8>> {
    // There is at most one byte per code unit, so nothing below reallocates.
    let mut out = Vec::new();
    out.try_reserve_exact(units.len()).ok()?;
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if unit == u16::from(b'+') && decode_spaces {
            out.push(b' ');
            index += 1;
            continue;
        }
        if unit == u16::from(b'%') && index + 2 < units.len() {
            match (hex_digit(units[index + 1]), hex_digit(units[index + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    index += 3;
                }
                _ => {
                    out.push(b'%');
                    index += 1;
                }
            }
            continue;
        }
        out.push(unit as u8);
        index += 1;
    }
    Some(out)
}

/// Node.js's `qsUnescape`: `units` decoded with `decodeURIComponent`, or if that throws, with
/// [`unescape_buffer`] as UTF-8.
fn unescape_units<'s>(
    scope: &'s Scope<'_>,
    units: &[u16],
    decode_spaces: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    match decode_uri_component(units) {
        Ok(decoded) => return string_value(scope, &decoded),
        Err(DecodeUriError::OutOfMemory) => return Err(out_of_memory(scope)),
        Err(DecodeUriError::Malformed) => {}
    }
    let bytes = unescape_buffer(units, decode_spaces).ok_or_else(|| out_of_memory(scope))?;
    let decoded = js::JSString::from_utf8_lossy(scope, &bytes)?;
    Ok(scope.root_value(decoded.as_value()))
}

/// A `sep` or `eq` argument: the code units of its string value, or `default` if it is falsy.
fn separator(
    scope: &Scope<'_>,
    value: Option<HandleValue<'_>>,
    default: &str,
) -> Result<Vec<u16>, ExnThrown> {
    match value.filter(|value| bool::from_jsval(scope, *value, ()).unwrap_or(false)) {
        Some(value) => units(scope, value),
        None => Ok(default.encode_utf16().collect()),
    }
}

/// The function `options[name]` names, if it is a function.
fn option_function<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'_>>,
    name: &'static std::ffi::CStr,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let Some(options) = options.and_then(|options| Object::from_value(scope, *options).ok()) else {
        return Ok(None);
    };
    let function = options.get_property(scope, name)?;
    Ok(Object::from_value(scope, function)
        .is_ok_and(|function| function.is_callable())
        .then_some(function))
}

/// The module's default export and the functions it exports as `escape` and `unescape`, read from
/// the module's namespace once per global.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ModuleExports {
    exports: Heap<js::object::Object>,
    escape: Heap<Value>,
    unescape: Heap<Value>,
}

#[core_runtime::jsmethods]
impl ModuleExports {}

/// The key of the global's [`ModuleExports`].
static EXPORTS_KEY: u8 = 0;

/// The value of `querystring[name]`, for `escape` or `unescape`, if an application replaced the
/// function the module exports under `name`, as `parse` and `stringify` then call the
/// replacement.
fn replaced_function<'s>(
    scope: &'s Scope<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let shared =
        js::class::get_or_init_shared_object(scope, &EXPORTS_KEY as *const u8 as usize, |scope| {
            let namespace = core_runtime::module::builtin_namespace(scope, "node:querystring")?
                .ok_or_else(|| js::error::TypeError("no node:querystring".into()).throw(scope))?;
            let exports = Object::from_value(scope, *namespace.get_property(scope, c"default")?)
                .map_err(|error| error.throw(scope))?;
            let escape = namespace.get_property(scope, c"escape")?;
            let unescape = namespace.get_property(scope, c"unescape")?;
            let shared = js::class::create_instance_with::<ModuleExportsImpl>(scope, |_| {
                ModuleExportsImpl {
                    exports: Heap::from(exports),
                    escape: Heap::from(escape.get()),
                    unescape: Heap::from(unescape.get()),
                }
            })?;
            Ok(shared.as_object())
        })?;
    let shared = shared
        .cast::<ModuleExports>()
        .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))?;
    let (exports, original) = {
        let data = shared.data();
        let original = if name == c"escape" {
            &data.escape
        } else {
            &data.unescape
        };
        (data.exports.get(scope), original.get(scope))
    };
    let current = exports.get_property(scope, name)?;
    if js::comparison::strictly_equal(scope, original, current)? {
        return Ok(None);
    }
    Ok(Some(current))
}

/// Node.js's `addKeyVal`: decode `key` and `value` if they hold escapes, and add them to `object`,
/// collecting the values of a repeated key in an array. A custom `decode` that throws is retried
/// with `unescape` and `decodeSpaces`: the application's replacement `unescape` if there is one,
/// and the module's own otherwise.
#[allow(clippy::too_many_arguments)]
fn add_key_value(
    scope: &Scope<'_>,
    object: Object<'_>,
    key: &[u16],
    value: &[u16],
    key_encoded: bool,
    value_encoded: bool,
    decode: Option<HandleValue<'_>>,
    unescape: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let decode_part = |part: &[u16], encoded: bool| -> Result<HandleValue<'_>, ExnThrown> {
        if part.is_empty() || !encoded {
            return string_value(scope, part);
        }
        match decode {
            None => unescape_units(scope, part, false),
            Some(decode) => {
                let text = string_value(scope, part)?;
                match Function::call(scope, js::value::undefined(), decode, &[text]) {
                    Ok(decoded) => Ok(decoded),
                    Err(ExnThrown) => {
                        js::exception::clear(scope);
                        match unescape {
                            Some(unescape) => {
                                let text = string_value(scope, part)?;
                                let spaces = scope.root_value(js::value::from_bool(true));
                                Function::call(
                                    scope,
                                    js::value::undefined(),
                                    unescape,
                                    &[text, spaces],
                                )
                            }
                            None => unescape_units(scope, part, true),
                        }
                    }
                }
            }
        }
    };
    let key = decode_part(key, key_encoded)?;
    let value = decode_part(value, value_encoded)?;
    let id = scope.root_id(js::id::value_to_id(scope, key)?);
    let existing = object.get_property_by_id(scope, id)?;
    if existing.is_undefined() {
        return object.set_property_by_id(scope, id, value);
    }
    let array = match Object::from_value(scope, *existing) {
        Ok(existing) => {
            let pop = existing.get_property(scope, c"pop")?;
            bool::from_jsval(scope, pop, ())
                .unwrap_or(false)
                .then_some(existing)
        }
        Err(_) => None,
    };
    match array {
        Some(array) => {
            let length = f64::from_jsval(scope, array.get_property(scope, c"length")?, ())
                .map_err(|_| ExnThrown)?;
            array.set_element(scope, length as u32, value)
        }
        None => {
            let pair = js::Array::with_contents(scope, &[existing, value])?;
            object.set_property_by_id(scope, id, *pair)
        }
    }
}

/// `value` as `stringifyPrimitive` renders it: a string as is, a finite number, a bigint or a
/// boolean as its string form, and anything else as the empty string.
fn stringify_primitive(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Vec<u16>, ExnThrown> {
    let finite_number = value.is_number() && value.to_number().is_finite();
    if value.is_string() || finite_number || value.is_bigint() || value.is_boolean() {
        return units(scope, value);
    }
    Ok(Vec::new())
}

/// A key or value as `stringify` writes it: encoded with the custom `encode` function, or as
/// Node.js's `encodeStringified` writes it with `escape`.
fn stringify_part(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    encode: Option<HandleValue<'_>>,
) -> Result<Vec<u16>, Thrown> {
    let text = stringify_primitive(scope, value)?;
    if let Some(encode) = encode {
        let text = string_value(scope, &text)?;
        let encoded = Function::call(scope, js::value::undefined(), encode, &[text])?;
        return Ok(units(scope, encoded)?);
    }
    // Strings, and numbers whose string form has an exponent, are escaped.
    if value.is_string() || (value.is_number() && value.to_number().abs() >= 1e21) {
        return encode_units(scope, &text);
    }
    Ok(text)
}

#[core_runtime::jsmodule(node_args, name = "node:querystring", default_namespace)]
pub mod module {
    use super::*;

    /// <https://nodejs.org/api/querystring.html#querystringescapestr>
    pub fn escape<'s>(
        scope: &'s Scope<'_>,
        text: HandleValue<'_>,
    ) -> Result<js::JSString<'s>, Thrown> {
        let encoded = encode_units(scope, &units(scope, text)?)?;
        Ok(js::JSString::from_utf16(scope, &encoded)?)
    }

    /// <https://nodejs.org/api/querystring.html#querystringunescapestr>: `decodeURIComponent`, or
    /// if that throws, [`unescape_buffer`]'s bytes as UTF-8.
    #[function(length = 2)]
    pub fn unescape<'s>(
        scope: &'s Scope<'_>,
        text: HandleValue<'_>,
        decode_spaces: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let decode_spaces =
            decode_spaces.is_some_and(|value| bool::from_jsval(scope, value, ()).unwrap_or(false));
        Ok(unescape_units(scope, &units(scope, text)?, decode_spaces)?)
    }

    /// <https://nodejs.org/api/querystring.html#querystringunescapebufferstr-decodespaces>: a
    /// `Buffer` of the bytes `%XX` escapes encode, with `+` as a space if `decode_spaces`. For a
    /// `text` other than a string, the code units are the first `text.length` of `String(text)`,
    /// with zeros past its end, and `Buffer.allocUnsafe`'s error for a `length` that isn't a valid
    /// size. `null` and `undefined` fail reading their `length`.
    #[function(length = 2)]
    pub fn unescape_buffer<'s>(
        scope: &'s Scope<'_>,
        text: HandleValue<'_>,
        decode_spaces: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let decode_spaces =
            decode_spaces.is_some_and(|value| bool::from_jsval(scope, value, ()).unwrap_or(false));
        let text_units = if text.is_string() {
            units(scope, text)?
        } else {
            if text.is_null() || text.is_undefined() {
                let kind = if text.is_null() { "null" } else { "undefined" };
                let message = format!("Cannot read properties of {kind} (reading 'length')");
                return Err(js::error::TypeError(message).throw(scope).into());
            }
            let object = Object::from_value_coerce(scope, text)?;
            let size = crate::buffer::size_arg(scope, object.get_property(scope, c"length")?)?;
            let size = crate::buffer::unsafe_buffer_length(scope, size)?;
            let mut text_units = units(scope, text)?;
            if size > text_units.len() {
                text_units
                    .try_reserve_exact(size - text_units.len())
                    .map_err(|_| out_of_memory(scope))?;
            }
            text_units.resize(size, 0);
            text_units
        };
        let bytes = super::unescape_buffer(&text_units, decode_spaces)
            .ok_or_else(|| out_of_memory(scope))?;
        Ok(crate::buffer::new_buffer(scope, &bytes)?)
    }

    /// <https://nodejs.org/api/querystring.html#querystringparsestr-sep-eq-options>: into an
    /// object without a prototype, with the values of repeated keys in an array. At most
    /// `options.maxKeys` (1000 by default, any number but a positive one for no limit) pairs are
    /// read. Keys and values are decoded only if they hold an escape, or with a custom
    /// `options.decodeURIComponent`.
    #[function(length = 4)]
    pub fn parse<'s>(
        scope: &'s Scope<'_>,
        text: HandleValue<'_>,
        sep: Option<HandleValue<'_>>,
        eq: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let object = Object::new_plain(scope)?;
        object.set_prototype(scope, js::native::HandleObject::null())?;
        if !text.is_string() {
            return Ok(object);
        }
        let text = units(scope, text)?;
        if text.is_empty() {
            return Ok(object);
        }
        let sep = separator(scope, sep, "&")?;
        let eq = separator(scope, eq, "=")?;
        let mut pairs = 1000.0;
        if let Some(options) = options.and_then(|options| Object::from_value(scope, *options).ok())
        {
            let max_keys = options.get_property(scope, c"maxKeys")?;
            if max_keys.is_number() {
                let max_keys = max_keys.to_number();
                pairs = if max_keys > 0.0 { max_keys } else { -1.0 };
            }
        }
        let unescape = replaced_function(scope, c"unescape")?;
        let decode = match option_function(scope, options, c"decodeURIComponent")? {
            Some(decode) => Some(decode),
            None => unescape,
        };
        let custom = decode.is_some();
        let plus: &[u16] = if custom { &[0x25, 0x32, 0x30] } else { &[0x20] };
        let is_hex = |unit: u16| hex_digit(unit).is_some();

        let (mut last, mut sep_index, mut eq_index) = (0, 0, 0);
        let (mut key, mut value) = (Vec::new(), Vec::new());
        let (mut key_encoded, mut value_encoded) = (custom, custom);
        let mut encode_check = 0;
        let mut index = 0;
        while index < text.len() {
            let code = text[index];
            if sep.get(sep_index) == Some(&code) {
                sep_index += 1;
                if sep_index == sep.len() {
                    let end = index + 1 - sep_index;
                    if eq_index < eq.len() {
                        if last < end {
                            append(scope, &mut key, &text[last..end])?;
                        } else if key.is_empty() {
                            pairs -= 1.0;
                            if pairs == 0.0 {
                                return Ok(object);
                            }
                            last = index + 1;
                            sep_index = 0;
                            eq_index = 0;
                            index += 1;
                            continue;
                        }
                    } else if last < end {
                        append(scope, &mut value, &text[last..end])?;
                    }
                    add_key_value(
                        scope,
                        object,
                        &key,
                        &value,
                        key_encoded,
                        value_encoded,
                        decode,
                        unescape,
                    )?;
                    pairs -= 1.0;
                    if pairs == 0.0 {
                        return Ok(object);
                    }
                    key_encoded = custom;
                    value_encoded = custom;
                    key.clear();
                    value.clear();
                    encode_check = 0;
                    last = index + 1;
                    sep_index = 0;
                    eq_index = 0;
                }
                index += 1;
                continue;
            }
            sep_index = 0;
            if eq_index < eq.len() {
                if code == eq[eq_index] {
                    eq_index += 1;
                    if eq_index == eq.len() {
                        let end = index + 1 - eq_index;
                        if last < end {
                            append(scope, &mut key, &text[last..end])?;
                        }
                        encode_check = 0;
                        last = index + 1;
                    }
                    index += 1;
                    continue;
                }
                eq_index = 0;
                if !key_encoded {
                    if code == u16::from(b'%') {
                        encode_check = 1;
                        index += 1;
                        continue;
                    } else if encode_check > 0 {
                        if is_hex(code) {
                            encode_check += 1;
                            if encode_check == 3 {
                                key_encoded = true;
                            }
                            index += 1;
                            continue;
                        }
                        encode_check = 0;
                    }
                }
                if code == u16::from(b'+') {
                    append(scope, &mut key, &text[last..index])?;
                    append(scope, &mut key, plus)?;
                    last = index + 1;
                    index += 1;
                    continue;
                }
            }
            if code == u16::from(b'+') {
                append(scope, &mut value, &text[last..index])?;
                append(scope, &mut value, plus)?;
                last = index + 1;
            } else if !value_encoded {
                if code == u16::from(b'%') {
                    encode_check = 1;
                } else if encode_check > 0 {
                    if is_hex(code) {
                        encode_check += 1;
                        if encode_check == 3 {
                            value_encoded = true;
                        }
                    } else {
                        encode_check = 0;
                    }
                }
            }
            index += 1;
        }
        if last < text.len() {
            if eq_index < eq.len() {
                append(scope, &mut key, &text[last..])?;
            } else if sep_index < sep.len() {
                append(scope, &mut value, &text[last..])?;
            }
        } else if eq_index == 0 && key.is_empty() {
            return Ok(object);
        }
        add_key_value(
            scope,
            object,
            &key,
            &value,
            key_encoded,
            value_encoded,
            decode,
            unescape,
        )?;
        Ok(object)
    }

    /// <https://nodejs.org/api/querystring.html#querystringdecodestr-sep-eq-options>
    #[function(length = 4)]
    pub fn decode<'s>(
        scope: &'s Scope<'_>,
        text: HandleValue<'_>,
        sep: Option<HandleValue<'_>>,
        eq: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        parse(scope, text, sep, eq, options)
    }

    /// <https://nodejs.org/api/querystring.html#querystringstringifyobj-sep-eq-options>: the
    /// object's own enumerable string-keyed properties, an array value as one pair per element and
    /// an empty array as none.
    #[function(length = 4)]
    pub fn stringify<'s>(
        scope: &'s Scope<'_>,
        object: HandleValue<'_>,
        sep: Option<HandleValue<'_>>,
        eq: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        let Some(object) = Object::from_value(scope, *object)
            .ok()
            .filter(|o| !o.is_callable())
        else {
            return Ok(js::JSString::empty(scope));
        };
        let sep = separator(scope, sep, "&")?;
        let eq = separator(scope, eq, "=")?;
        let encode = match option_function(scope, options, c"encodeURIComponent")? {
            Some(encode) => Some(encode),
            None => replaced_function(scope, c"escape")?,
        };
        // As Node.js does, the keys are `Object.keys(object)`, and each value is read once after.
        // Each element of an array value is read as its pair is written. The text is limited as
        // [`reserve`] limits it.
        let mut fields: Vec<u16> = Vec::new();
        for key in object.own_enumerable_keys(scope)? {
            let id = scope.root_id(js::id::value_to_id(scope, key)?);
            let value = object.get_property_by_id(scope, id)?;
            let key = scope.root_value(js::JSString::from_value(scope, key)?.as_value());
            let mut prefix = stringify_part(scope, key, encode)?;
            append(scope, &mut prefix, &eq)?;
            let array = match Object::from_value(scope, value) {
                Ok(value) if js::Array::is_array_or_array_proxy(scope, value.handle())? => {
                    Some(value)
                }
                _ => None,
            };
            let add = |fields: &mut Vec<u16>, item| -> Result<(), Thrown> {
                if !fields.is_empty() {
                    append(scope, fields, &sep)?;
                }
                append(scope, fields, &prefix)?;
                append(scope, fields, &stringify_part(scope, item, encode)?)?;
                Ok(())
            };
            match array {
                Some(array) => {
                    let length =
                        f64::from_jsval(scope, array.get_property(scope, c"length")?, ())? as u32;
                    for element in 0..length {
                        add(&mut fields, array.get_element(scope, element)?)?;
                    }
                }
                None => add(&mut fields, value)?,
            }
        }
        Ok(js::JSString::from_utf16(scope, &fields)?)
    }

    /// <https://nodejs.org/api/querystring.html#querystringencodeobj-sep-eq-options>
    #[function(length = 4)]
    pub fn encode<'s>(
        scope: &'s Scope<'_>,
        object: HandleValue<'_>,
        sep: Option<HandleValue<'_>>,
        eq: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<js::JSString<'s>, Thrown> {
        stringify(scope, object, sep, eq, options)
    }
}

/// Register `node:querystring`, whose `default` export has Node.js's property order.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    if !crate::stub::register_implemented::<module::js_module>(scope, name) {
        return false;
    }
    let order = [
        c"unescapeBuffer",
        c"unescape",
        c"escape",
        c"stringify",
        c"encode",
        c"parse",
        c"decode",
    ];
    crate::stub::order_default_export(scope, name, &order).is_ok()
}
