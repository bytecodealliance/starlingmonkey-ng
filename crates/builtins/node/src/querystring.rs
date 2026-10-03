// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:querystring`. See <https://nodejs.org/api/querystring.html>.
//!
//! The functions follow Node.js's `lib/querystring.js` and work on UTF-16 code units, so lone
//! surrogates pass through `parse` and `unescape` as they are.

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::string::StrChars as Chars;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};

/// Whether `escape` leaves `byte` as is: `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte)
}

/// The uppercase hex digits.
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// `querystring.escape` for a string without lone surrogates.
pub fn escape_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if is_unreserved(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0xf)] as char);
        }
    }
    out
}

/// The UTF-16 code units of `value` converted to a string.
fn units(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Vec<u16>, ExnThrown> {
    let string = js::JSString::from_value(scope, value)?;
    // SAFETY: the closure makes no call into SpiderMonkey.
    unsafe {
        string.with_chars(scope, |chars| match chars {
            Chars::Latin1(bytes) => bytes.iter().map(|byte| u16::from(*byte)).collect(),
            Chars::TwoByte(units) => units.to_vec(),
        })
    }
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
/// after it, whatever that is, and is an error at the end of `units`.
fn encode_units(units: &[u16]) -> Result<Vec<u16>, Thrown> {
    let mut out = Vec::with_capacity(units.len());
    let hex = |out: &mut Vec<u16>, byte: u32| {
        out.push(u16::from(b'%'));
        out.push(u16::from(HEX[(byte >> 4) as usize & 0xf]));
        out.push(u16::from(HEX[byte as usize & 0xf]));
    };
    let mut index = 0;
    while index < units.len() {
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

/// The value of hex digit `unit`, if it is one.
fn hex_digit(unit: u16) -> Option<u8> {
    char::from_u32(u32::from(unit))
        .and_then(|c| c.to_digit(16))
        .map(|digit| digit as u8)
}

/// Node.js's `unescapeBuffer`: `%XX` escapes decoded to bytes, and `+` to a space if
/// `decode_spaces`. Malformed escapes are kept, and other code units are stored as their low byte.
fn unescape_buffer(units: &[u16], decode_spaces: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
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
    out
}

/// JS's `decodeURIComponent` of `units`, or `None` where it throws a `URIError`: each `%XX`
/// escape sequence of a UTF-8 encoded code point replaced by the code point.
fn decode_uri_component(units: &[u16]) -> Option<Vec<u16>> {
    let byte_at = |index: usize| -> Option<u8> {
        if units.get(index) != Some(&u16::from(b'%')) {
            return None;
        }
        Some(hex_digit(*units.get(index + 1)?)? * 16 + hex_digit(*units.get(index + 2)?)?)
    };
    let mut out = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        if units[index] != u16::from(b'%') {
            out.push(units[index]);
            index += 1;
            continue;
        }
        let lead = byte_at(index)?;
        index += 3;
        if lead < 0x80 {
            out.push(u16::from(lead));
            continue;
        }
        let length = match lead {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => return None,
        };
        let mut bytes = vec![lead];
        for _ in 1..length {
            let byte = byte_at(index)?;
            if byte & 0xc0 != 0x80 {
                return None;
            }
            bytes.push(byte);
            index += 3;
        }
        let c = std::str::from_utf8(&bytes).ok()?.chars().next()?;
        let mut buffer = [0u16; 2];
        out.extend_from_slice(c.encode_utf16(&mut buffer));
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
    if let Some(decoded) = decode_uri_component(units) {
        return string_value(scope, &decoded);
    }
    let bytes = unescape_buffer(units, decode_spaces);
    let decoded: Vec<u16> = String::from_utf8_lossy(&bytes).encode_utf16().collect();
    string_value(scope, &decoded)
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

/// The value of `querystring[name]` if an application replaced the function the module exports
/// under `name`, as `parse` and `stringify` then call the replacement.
fn replaced_function<'s>(
    scope: &'s Scope<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let Some(namespace) = core_runtime::module::builtin_namespace(scope, "node:querystring")?
    else {
        return Ok(None);
    };
    let original = namespace.get_property(scope, name)?;
    let Ok(exports) = Object::from_value(scope, *namespace.get_property(scope, c"default")?) else {
        return Ok(None);
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
        return encode_units(&text);
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
        let encoded = encode_units(&units(scope, text)?)?;
        Ok(js::JSString::from_utf16(scope, &encoded)?)
    }

    /// <https://nodejs.org/api/querystring.html#querystringunescapestr>: `decodeURIComponent`, or
    /// if that throws, [`unescape_buffer`]'s bytes as UTF-8.
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
    /// `Buffer` of the bytes `%XX` escapes encode, with `+` as a space if `decode_spaces`.
    pub fn unescape_buffer<'s>(
        scope: &'s Scope<'_>,
        text: HandleValue<'_>,
        decode_spaces: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let decode_spaces =
            decode_spaces.is_some_and(|value| bool::from_jsval(scope, value, ()).unwrap_or(false));
        let bytes = super::unescape_buffer(&units(scope, text)?, decode_spaces);
        Ok(crate::buffer::new_buffer(scope, &bytes)?)
    }

    /// <https://nodejs.org/api/querystring.html#querystringparsestr-sep-eq-options>: into an
    /// object without a prototype, with the values of repeated keys in an array. At most
    /// `options.maxKeys` (1000 by default, any number but a positive one for no limit) pairs are
    /// read. Keys and values are decoded only if they hold an escape, or with a custom
    /// `options.decodeURIComponent`.
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
                            key.extend_from_slice(&text[last..end]);
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
                        value.extend_from_slice(&text[last..end]);
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
                            key.extend_from_slice(&text[last..end]);
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
                    key.extend_from_slice(&text[last..index]);
                    key.extend_from_slice(plus);
                    last = index + 1;
                    index += 1;
                    continue;
                }
            }
            if code == u16::from(b'+') {
                value.extend_from_slice(&text[last..index]);
                value.extend_from_slice(plus);
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
                key.extend_from_slice(&text[last..]);
            } else if sep_index < sep.len() {
                value.extend_from_slice(&text[last..]);
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
        // Copying `object` onto an object without a prototype reads each of its own enumerable
        // properties once, in `Object.keys` order.
        let copy = Object::new_plain(scope)?;
        copy.set_prototype(scope, js::native::HandleObject::null())?;
        copy.assign(scope, object)?;
        let mut fields: Vec<u16> = Vec::new();
        for key in copy.own_keys(scope)? {
            if key.is_symbol() {
                continue;
            }
            let id = scope.root_id(js::id::value_to_id(scope, key)?);
            let value = copy.get_property_by_id(scope, id)?;
            let key = scope.root_value(js::JSString::from_value(scope, key)?.as_value());
            let mut prefix = stringify_part(scope, key, encode)?;
            prefix.extend_from_slice(&eq);
            let array = Object::from_value(scope, value)
                .ok()
                .filter(|value| js::Array::is_array(scope, value.handle()).unwrap_or(false));
            let items = match array {
                Some(array) => {
                    let length =
                        f64::from_jsval(scope, array.get_property(scope, c"length")?, ())? as u32;
                    (0..length)
                        .map(|element| array.get_element(scope, element))
                        .collect::<Result<Vec<_>, _>>()?
                }
                None => vec![value],
            };
            for item in items {
                if !fields.is_empty() {
                    fields.extend_from_slice(&sep);
                }
                fields.extend_from_slice(&prefix);
                fields.extend(stringify_part(scope, item, encode)?);
            }
        }
        Ok(js::JSString::from_utf16(scope, &fields)?)
    }

    /// <https://nodejs.org/api/querystring.html#querystringencodeobj-sep-eq-options>
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
