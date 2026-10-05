// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:punycode`, a port of the Punycode.js 2.1.0 that Node.js bundles. See
//! <https://nodejs.org/api/punycode.html>.
//!
//! `toASCII` and `toUnicode` convert each label of a domain, or of the part of an email address
//! between its first and second `@`. Only labels starting with a lowercase `xn--` are decoded.
//! Strings are handled as UTF-16 code units, so lone surrogates are encoded as they are.

use std::ffi::CString;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

const MAX_INT: u32 = 2147483647;
const BASE: u32 = 36;
const T_MIN: u32 = 1;
const T_MAX: u32 = 26;
const SKEW: u32 = 38;
const DAMP: u32 = 700;
const INITIAL_BIAS: u32 = 72;
const INITIAL_N: u32 = 128;

/// The errors Punycode.js throws as `RangeError`s without a `code`.
enum PunycodeError {
    Overflow,
    NotBasic,
    InvalidInput,
    /// `String.fromCodePoint`'s error for a value that isn't a code point.
    InvalidCodePoint(f64),
}

impl ThrowException for PunycodeError {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        let message = match self {
            PunycodeError::Overflow => "Overflow: input needs wider integers to process".into(),
            PunycodeError::NotBasic => "Illegal input >= 0x80 (not a basic code point)".into(),
            PunycodeError::InvalidInput => "Invalid input".into(),
            PunycodeError::InvalidCodePoint(value) => {
                format!(
                    "Invalid code point {}",
                    crate::errors::number_to_string(value)
                )
            }
        };
        let message = CString::new(message).expect("the messages hold no NUL");
        js::error::throw_range_error(scope, &message)
    }
}

/// The UTF-16 code units of the string `value`, or a `TypeError` naming `method` as Punycode.js
/// fails for a value that isn't a string.
fn string_units(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    method: &str,
) -> Result<Vec<u16>, ExnThrown> {
    if !value.is_string() {
        return Err(js::error::TypeError(format!("input.{method} is not a function")).throw(scope));
    }
    js::JSString::from_value(scope, value)?.to_utf16_vec(scope)
}

/// Punycode.js's `ucs2decode` of `units`: the code points, with each surrogate pair combined.
fn ucs2_decode_units(units: &[u16]) -> Vec<u32> {
    let mut output = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let value = u32::from(units[index]);
        index += 1;
        if (0xd800..=0xdbff).contains(&value) && index < units.len() {
            let extra = u32::from(units[index]);
            if extra & 0xfc00 == 0xdc00 {
                index += 1;
                output.push(((value & 0x3ff) << 10) + (extra & 0x3ff) + 0x10000);
                continue;
            }
        }
        output.push(value);
    }
    output
}

/// The UTF-16 code units of `points`, as `String.fromCodePoint` writes them.
fn from_code_points(points: &[u32]) -> Result<Vec<u16>, PunycodeError> {
    let mut units = Vec::with_capacity(points.len());
    for &point in points {
        if point > 0x10ffff {
            return Err(PunycodeError::InvalidCodePoint(f64::from(point)));
        }
        if point > 0xffff {
            let point = point - 0x10000;
            units.push(0xd800 | (point >> 10) as u16);
            units.push(0xdc00 | (point & 0x3ff) as u16);
        } else {
            units.push(point as u16);
        }
    }
    Ok(units)
}

fn basic_to_digit(code_point: u32) -> u32 {
    match code_point {
        0x30..=0x39 => 26 + (code_point - 0x30),
        0x41..=0x5a => code_point - 0x41,
        0x61..=0x7a => code_point - 0x61,
        _ => BASE,
    }
}

fn digit_to_basic(digit: u32) -> u16 {
    (digit + 22 + if digit < 26 { 75 } else { 0 }) as u16
}

fn adapt(delta: u32, num_points: u32, first_time: bool) -> u32 {
    let mut k = 0;
    let mut delta = if first_time { delta / DAMP } else { delta >> 1 };
    delta += delta / num_points;
    while delta > ((BASE - T_MIN) * T_MAX) >> 1 {
        delta /= BASE - T_MIN;
        k += BASE;
    }
    k + (BASE - T_MIN + 1) * delta / (delta + SKEW)
}

fn threshold(k: u32, bias: u32) -> u32 {
    if k <= bias {
        T_MIN
    } else if k >= bias + T_MAX {
        T_MAX
    } else {
        k - bias
    }
}

/// Punycode.js's `decode`.
fn decode_units(input: &[u16]) -> Result<Vec<u16>, PunycodeError> {
    let mut output: Vec<u32> = Vec::new();
    let mut i: u32 = 0;
    let mut n = INITIAL_N;
    let mut bias = INITIAL_BIAS;
    let basic = input
        .iter()
        .rposition(|unit| *unit == u16::from(b'-'))
        .unwrap_or(0);
    for &unit in &input[..basic] {
        if unit >= 0x80 {
            return Err(PunycodeError::NotBasic);
        }
        output.push(u32::from(unit));
    }
    let mut index = if basic > 0 { basic + 1 } else { 0 };
    while index < input.len() {
        let old_i = i;
        let mut w: u32 = 1;
        let mut k = BASE;
        loop {
            if index >= input.len() {
                return Err(PunycodeError::InvalidInput);
            }
            let digit = basic_to_digit(u32::from(input[index]));
            index += 1;
            if digit >= BASE {
                return Err(PunycodeError::InvalidInput);
            }
            if digit > (MAX_INT - i) / w {
                return Err(PunycodeError::Overflow);
            }
            i += digit * w;
            let t = threshold(k, bias);
            if digit < t {
                break;
            }
            if w > MAX_INT / (BASE - t) {
                return Err(PunycodeError::Overflow);
            }
            w *= BASE - t;
            k += BASE;
        }
        let out = output.len() as u32 + 1;
        bias = adapt(i - old_i, out, old_i == 0);
        if i / out > MAX_INT - n {
            return Err(PunycodeError::Overflow);
        }
        n += i / out;
        i %= out;
        output.insert(i as usize, n);
        i += 1;
    }
    from_code_points(&output)
}

/// Punycode.js's `encode` of the code points `input`.
fn encode_points(input: &[u32]) -> Result<Vec<u16>, PunycodeError> {
    let mut output: Vec<u16> = input
        .iter()
        .filter(|point| **point < 0x80)
        .map(|point| *point as u16)
        .collect();
    let basic_length = output.len() as u32;
    let mut handled = basic_length;
    if basic_length > 0 {
        output.push(u16::from(b'-'));
    }
    let mut n = INITIAL_N;
    let mut delta: u32 = 0;
    let mut bias = INITIAL_BIAS;
    while (handled as usize) < input.len() {
        let m = input
            .iter()
            .copied()
            .filter(|point| *point >= n && *point < MAX_INT)
            .min()
            .unwrap_or(MAX_INT);
        let handled_plus_one = handled + 1;
        if m - n > (MAX_INT - delta) / handled_plus_one {
            return Err(PunycodeError::Overflow);
        }
        delta += (m - n) * handled_plus_one;
        n = m;
        for &point in input {
            if point < n {
                delta += 1;
                if delta > MAX_INT {
                    return Err(PunycodeError::Overflow);
                }
            }
            if point == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = threshold(k, bias);
                    if q < t {
                        break;
                    }
                    output.push(digit_to_basic(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                output.push(digit_to_basic(q));
                bias = adapt(delta, handled_plus_one, handled == basic_length);
                delta = 0;
                handled += 1;
            }
        }
        delta += 1;
        n += 1;
    }
    Ok(output)
}

/// Punycode.js's `mapDomain`: convert each label of `domain`, or of the part of an email address
/// between its first and second `@`, with `convert`. Labels are separated by `.` or an
/// ideographic, fullwidth or halfwidth full stop, and joined with `.`.
fn map_domain(
    domain: &[u16],
    convert: impl Fn(&[u16]) -> Result<Vec<u16>, PunycodeError>,
) -> Result<Vec<u16>, PunycodeError> {
    let at = u16::from(b'@');
    let mut parts = domain.split(|unit| *unit == at);
    let first = parts.next().unwrap_or_default();
    let (mut result, domain) = match parts.next() {
        Some(second) => {
            let mut result = first.to_vec();
            result.push(at);
            (result, second)
        }
        None => (Vec::new(), first),
    };
    let separator = |unit: &u16| matches!(*unit, 0x2e | 0x3002 | 0xff0e | 0xff61);
    for (index, label) in domain.split(separator).enumerate() {
        if index > 0 {
            result.push(u16::from(b'.'));
        }
        result.extend(convert(label)?);
    }
    Ok(result)
}

/// `units` lowercased, as `String.prototype.toLowerCase` does it for each code point.
fn to_lower_case(units: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(units.len());
    for unit in char::decode_utf16(units.iter().copied()) {
        match unit {
            Ok(c) => {
                let mut buffer = [0u16; 2];
                for lower in c.to_lowercase() {
                    out.extend_from_slice(lower.encode_utf16(&mut buffer));
                }
            }
            Err(error) => out.push(error.unpaired_surrogate()),
        }
    }
    out
}

/// A JS string of `units`.
fn string<'s>(scope: &'s Scope<'_>, units: &[u16]) -> Result<js::JSString<'s>, ExnThrown> {
    js::JSString::from_utf16(scope, units)
}

#[core_runtime::jsmodule(node_args, name = "node:punycode", default_namespace)]
pub mod module {
    use super::*;

    /// <https://nodejs.org/api/punycode.html#punycodedecodestring>
    pub fn decode<'s>(
        scope: &'s Scope<'_>,
        input: HandleValue<'_>,
    ) -> Result<js::JSString<'s>, ExnThrown> {
        let input = string_units(scope, input, "lastIndexOf")?;
        let output = decode_units(&input).map_err(|error| error.throw(scope))?;
        string(scope, &output)
    }

    /// <https://nodejs.org/api/punycode.html#punycodeencodestring>
    pub fn encode<'s>(
        scope: &'s Scope<'_>,
        input: HandleValue<'_>,
    ) -> Result<js::JSString<'s>, ExnThrown> {
        let points = ucs2_decode_value(scope, input)?;
        let output = encode_points(&points).map_err(|error| error.throw(scope))?;
        string(scope, &output)
    }

    /// <https://nodejs.org/api/punycode.html#punycodetoasciidomain>: labels with non-ASCII
    /// characters become `xn--` labels.
    #[function(name = "toASCII")]
    pub fn to_ascii<'s>(
        scope: &'s Scope<'_>,
        domain: HandleValue<'_>,
    ) -> Result<js::JSString<'s>, ExnThrown> {
        let domain = string_units(scope, domain, "split")?;
        let output = map_domain(&domain, |label| {
            if label.iter().all(|unit| *unit < 0x80) {
                return Ok(label.to_vec());
            }
            let mut encoded: Vec<u16> = "xn--".encode_utf16().collect();
            encoded.extend(encode_points(&ucs2_decode_units(label))?);
            Ok(encoded)
        })
        .map_err(|error| error.throw(scope))?;
        string(scope, &output)
    }

    /// <https://nodejs.org/api/punycode.html#punycodetounicodedomain>: labels starting with
    /// `xn--` are decoded.
    #[function(name = "toUnicode")]
    pub fn to_unicode<'s>(
        scope: &'s Scope<'_>,
        domain: HandleValue<'_>,
    ) -> Result<js::JSString<'s>, ExnThrown> {
        let domain = string_units(scope, domain, "split")?;
        let prefix: Vec<u16> = "xn--".encode_utf16().collect();
        let output = map_domain(&domain, |label| {
            match label.strip_prefix(prefix.as_slice()) {
                Some(rest) => decode_units(&to_lower_case(rest)),
                None => Ok(label.to_vec()),
            }
        })
        .map_err(|error| error.throw(scope))?;
        string(scope, &output)
    }
}

/// Punycode.js's `ucs2decode` of any value: its `length` code units read with `charCodeAt`, so a
/// value without a `length` decodes to nothing.
fn ucs2_decode_value(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Vec<u32>, ExnThrown> {
    if value.is_string() {
        return Ok(ucs2_decode_units(&string_units(
            scope,
            value,
            "charCodeAt",
        )?));
    }
    if value.is_null() || value.is_undefined() {
        return Err(js::error::TypeError(
            "can't access property \"length\" of null or undefined".into(),
        )
        .throw(scope));
    }
    let object = Object::from_value_coerce(scope, value)?;
    let length = f64::from_jsval(scope, object.get_property(scope, c"length")?, ())
        .map_err(|_| ExnThrown)?;
    let mut units = Vec::new();
    let mut index = 0.0;
    while index < length {
        let index_value = scope.root_value(js::value::from_f64(index));
        let unit = Function::call_by_name(scope, object.handle(), c"charCodeAt", &[index_value])?;
        let unit = f64::from_jsval(scope, unit, ()).map_err(|_| ExnThrown)?;
        units.push(unit as u16);
        index += 1.0;
    }
    Ok(ucs2_decode_units(&units))
}

/// `punycode.ucs2.decode(string)`: the code points of `string`, with each surrogate pair combined.
fn ucs2_decode(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let points: Vec<f64> = ucs2_decode_value(scope, args.get(0))?
        .into_iter()
        .map(f64::from)
        .collect();
    js::Array::with_contents(scope, &points)?.to_jsval_raw_throwing(scope)
}

/// `punycode.ucs2.encode(codePoints)`, `String.fromCodePoint(...codePoints)`: a `RangeError` for
/// an element that isn't an integer from 0 to 0x10FFFF.
fn ucs2_encode(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let array = Object::from_value(scope, *args.get(0))
        .map_err(|_| js::error::TypeError("codePoints is not iterable".into()).throw(scope))?;
    let length =
        f64::from_jsval(scope, array.get_property(scope, c"length")?, ()).map_err(|_| ExnThrown)?;
    let mut points = Vec::new();
    let mut index = 0u32;
    while f64::from(index) < length {
        let element = array.get_element(scope, index)?;
        let point = f64::from_jsval(scope, element, ()).map_err(|_| ExnThrown)?;
        if point.fract() != 0.0 || !(0.0..=1114111.0).contains(&point) {
            return Err(PunycodeError::InvalidCodePoint(point).throw(scope));
        }
        points.push(point as u32);
        index += 1;
    }
    let units = from_code_points(&points).map_err(|error| error.throw(scope))?;
    Ok(js::JSString::from_utf16(scope, &units)?.as_value())
}

/// Emit DEP0040 for loading the module. Without `--pending-deprecation`, a load from a module in
/// `node_modules` emits nothing.
fn warn_deprecated(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    if !crate::runtime_flags::get().pending_deprecation
        && crate::process::called_from_node_modules(scope)?
    {
        return Ok(());
    }
    crate::process::deprecation_warning(
        scope,
        "The `punycode` module is deprecated. Please use a userland alternative instead.",
        "DEP0040",
    )
}

/// Register `node:punycode` with `ucs2`, and emit DEP0040.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let ucs2 = || -> Result<HandleValue<'_>, ExnThrown> {
        let ucs2 = Object::new_plain(scope)?;
        let decode = Function::new_callback(scope, c"decode", 1, ucs2_decode, ())?;
        let encode = Function::new_callback(scope, c"encode", 1, ucs2_encode, ())?;
        ucs2.set_property(scope, c"decode", *decode)?;
        ucs2.set_property(scope, c"encode", *encode)?;
        ucs2.to_jsval_throwing(scope)
    };
    let Ok(ucs2) = ucs2() else {
        return false;
    };
    if warn_deprecated(scope).is_err() {
        return false;
    }
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &[("ucs2", ucs2)])
}
