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
use js::object::ESClass;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::support::try_extend;

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
    /// A failed allocation, which is reported as out of memory.
    OutOfMemory,
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
            PunycodeError::OutOfMemory => {
                js::exception::report_out_of_memory(scope);
                return ExnThrown;
            }
        };
        let message = CString::new(message).expect("the messages hold no NUL");
        js::error::throw_range_error(scope, &message)
    }
}

/// Append `item` to `vec`, or fail with [`PunycodeError::OutOfMemory`] if allocating room fails.
fn push<T>(vec: &mut Vec<T>, item: T) -> Result<(), PunycodeError> {
    try_extend(vec, [item]).ok_or(PunycodeError::OutOfMemory)
}

/// A new `Vec` with room for `capacity` items, or [`PunycodeError::OutOfMemory`].
fn with_capacity<T>(capacity: usize) -> Result<Vec<T>, PunycodeError> {
    let mut vec = Vec::new();
    vec.try_reserve(capacity)
        .map_err(|_| PunycodeError::OutOfMemory)?;
    Ok(vec)
}

/// The `TypeError` JavaScript throws for `receiver.method(...)` where `value` is the receiver:
/// reading a property of `null` or `undefined`, or calling a property that isn't a function.
fn not_a_function(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    receiver: &str,
    method: &str,
) -> ExnThrown {
    let message = if value.is_null() || value.is_undefined() {
        let kind = if value.is_null() { "null" } else { "undefined" };
        format!("Cannot read properties of {kind} (reading '{method}')")
    } else {
        format!("{receiver}.{method} is not a function")
    };
    js::error::TypeError(message).throw(scope)
}

/// The UTF-16 code units of the domain `value`, a string or a `String` object, or the
/// `TypeError` Punycode.js's `mapDomain` fails with for another value.
fn domain_units(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Vec<u16>, ExnThrown> {
    if value.is_string() {
        return js::JSString::from_value(scope, value)?.to_utf16_vec(scope);
    }
    if let Ok(object) = Object::from_value(scope, *value) {
        if object.builtin_class(scope)? == ESClass::String {
            return js::JSString::from_value(scope, value)?.to_utf16_vec(scope);
        }
    }
    Err(not_a_function(scope, value, "domain", "split"))
}

/// The code units of `value` as Punycode.js reads them from a parameter named `receiver`: those
/// of a string, and for another value `value.charCodeAt(index)` for each index below its
/// `length`. Each of `required`, a key and the method's name, must be a function of `value`, or
/// the read fails with the `TypeError` of calling it. A `null` or `undefined` `value` fails
/// reading its `length`.
fn code_units(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    receiver: &str,
    required: &[(&'static std::ffi::CStr, &str)],
) -> Result<Vec<u16>, ExnThrown> {
    if value.is_string() {
        return js::JSString::from_value(scope, value)?.to_utf16_vec(scope);
    }
    if value.is_null() || value.is_undefined() {
        return Err(not_a_function(scope, value, receiver, "length"));
    }
    let object = Object::from_value_coerce(scope, value)?;
    let length = f64::from_jsval(scope, object.get_property(scope, c"length")?, ())
        .map_err(|_| ExnThrown)?;
    for &(name, method) in required {
        let function = object.get_property(scope, name)?;
        if !Object::from_value(scope, *function).is_ok_and(|function| function.is_callable()) {
            return Err(not_a_function(scope, value, receiver, method));
        }
    }
    let mut units = Vec::new();
    let mut index = 0.0;
    while index < length {
        let char_code_at = object.get_property(scope, c"charCodeAt")?;
        if !Object::from_value(scope, *char_code_at).is_ok_and(|function| function.is_callable()) {
            return Err(not_a_function(scope, value, receiver, "charCodeAt"));
        }
        let index_value = scope.root_value(js::value::from_f64(index));
        let unit = Function::call(scope, object, char_code_at, &[index_value])?;
        let unit = f64::from_jsval(scope, unit, ()).map_err(|_| ExnThrown)?;
        push(&mut units, unit as u16).map_err(|error| error.throw(scope))?;
        index += 1.0;
    }
    Ok(units)
}

/// Punycode.js's `ucs2decode` of `units`: the code points, with each surrogate pair combined.
fn ucs2_decode_units(units: &[u16]) -> Result<Vec<u32>, PunycodeError> {
    let mut output = with_capacity(units.len())?;
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
    Ok(output)
}

/// The UTF-16 code units of `points`, as `String.fromCodePoint` writes them.
fn from_code_points(points: &[u32]) -> Result<Vec<u16>, PunycodeError> {
    let mut units = with_capacity(points.len().saturating_mul(2))?;
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
    let mut i: u32 = 0;
    let mut n = INITIAL_N;
    let mut bias = INITIAL_BIAS;
    let basic = input
        .iter()
        .rposition(|unit| *unit == u16::from(b'-'))
        .unwrap_or(0);
    let mut output: Vec<u32> = with_capacity(input.len())?;
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
        // `output` has room for one code point per unit of `input`.
        output.insert(i as usize, n);
        i += 1;
    }
    from_code_points(&output)
}

/// Punycode.js's `encode` of the code points `input`.
fn encode_points(input: &[u32]) -> Result<Vec<u16>, PunycodeError> {
    let mut output: Vec<u16> = Vec::new();
    let basic = input
        .iter()
        .filter(|point| **point < 0x80)
        .map(|point| *point as u16);
    try_extend(&mut output, basic).ok_or(PunycodeError::OutOfMemory)?;
    let basic_length = output.len() as u32;
    let mut handled = basic_length;
    if basic_length > 0 {
        push(&mut output, u16::from(b'-'))?;
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
                    push(&mut output, digit_to_basic(t + (q - t) % (BASE - t)))?;
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                push(&mut output, digit_to_basic(q))?;
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
    let extend = |result: &mut Vec<u16>, units: &[u16]| {
        try_extend(result, units.iter().copied()).ok_or(PunycodeError::OutOfMemory)
    };
    let mut result = Vec::new();
    let domain = match parts.next() {
        Some(second) => {
            extend(&mut result, first)?;
            push(&mut result, at)?;
            second
        }
        None => first,
    };
    let separator = |unit: &u16| matches!(*unit, 0x2e | 0x3002 | 0xff0e | 0xff61);
    for (index, label) in domain.split(separator).enumerate() {
        if index > 0 {
            push(&mut result, u16::from(b'.'))?;
        }
        extend(&mut result, &convert(label)?)?;
    }
    Ok(result)
}

/// `units` lowercased, as `String.prototype.toLowerCase` does it for each code point.
fn to_lower_case(units: &[u16]) -> Result<Vec<u16>, PunycodeError> {
    let mut out = with_capacity(units.len())?;
    for unit in char::decode_utf16(units.iter().copied()) {
        match unit {
            Ok(c) => {
                let mut buffer = [0u16; 2];
                for lower in c.to_lowercase() {
                    let lower = lower.encode_utf16(&mut buffer);
                    try_extend(&mut out, lower.iter().copied())
                        .ok_or(PunycodeError::OutOfMemory)?;
                }
            }
            Err(error) => push(&mut out, error.unpaired_surrogate())?,
        }
    }
    Ok(out)
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
        let input = code_units(scope, input, "input", &[(c"lastIndexOf", "lastIndexOf")])?;
        let output = decode_units(&input).map_err(|error| error.throw(scope))?;
        string(scope, &output)
    }

    /// <https://nodejs.org/api/punycode.html#punycodeencodestring>
    pub fn encode<'s>(
        scope: &'s Scope<'_>,
        input: HandleValue<'_>,
    ) -> Result<js::JSString<'s>, ExnThrown> {
        let units = code_units(scope, input, "string", &[])?;
        let output = ucs2_decode_units(&units)
            .and_then(|points| encode_points(&points))
            .map_err(|error| error.throw(scope))?;
        string(scope, &output)
    }

    /// <https://nodejs.org/api/punycode.html#punycodetoasciidomain>: labels with non-ASCII
    /// characters become `xn--` labels.
    #[function(name = "toASCII")]
    pub fn to_ascii<'s>(
        scope: &'s Scope<'_>,
        domain: HandleValue<'_>,
    ) -> Result<js::JSString<'s>, ExnThrown> {
        let domain = domain_units(scope, domain)?;
        let output = map_domain(&domain, |label| {
            let mut encoded = Vec::new();
            if label.iter().all(|unit| *unit < 0x80) {
                try_extend(&mut encoded, label.iter().copied())
                    .ok_or(PunycodeError::OutOfMemory)?;
                return Ok(encoded);
            }
            let points = encode_points(&ucs2_decode_units(label)?)?;
            try_extend(&mut encoded, "xn--".encode_utf16().chain(points))
                .ok_or(PunycodeError::OutOfMemory)?;
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
        let domain = domain_units(scope, domain)?;
        let prefix: Vec<u16> = "xn--".encode_utf16().collect();
        let output = map_domain(&domain, |label| {
            match label.strip_prefix(prefix.as_slice()) {
                Some(rest) => decode_units(&to_lower_case(rest)?),
                None => {
                    let mut copy = Vec::new();
                    try_extend(&mut copy, label.iter().copied())
                        .ok_or(PunycodeError::OutOfMemory)?;
                    Ok(copy)
                }
            }
        })
        .map_err(|error| error.throw(scope))?;
        string(scope, &output)
    }
}

/// `punycode.ucs2.decode(string)`: the code points of `string`, with each surrogate pair combined.
/// A value other than a string is read with [`code_units`].
fn ucs2_decode(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let units = code_units(scope, args.get(0), "string", &[])?;
    let points = ucs2_decode_units(&units).map_err(|error| error.throw(scope))?;
    let mut values = Vec::new();
    try_extend(&mut values, points.into_iter().map(f64::from))
        .ok_or_else(|| PunycodeError::OutOfMemory.throw(scope))?;
    js::Array::with_contents(scope, &values)?.to_jsval_raw_throwing(scope)
}

/// `punycode.ucs2.encode(codePoints)`, `String.fromCodePoint(...codePoints)`: the elements the
/// iterator of `codePoints` produces, converted to numbers, as code points. A `RangeError` for an
/// element that isn't an integer from 0 to 0x10FFFF.
fn ucs2_encode(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let mut elements = Vec::new();
    let iterable = js::conversion::for_of(scope, args.get(0), |element| {
        push(&mut elements, scope.root_value(element.get())).map_err(|error| error.throw(scope))?;
        Ok::<_, js::conversion::ConversionError>(())
    })
    .map_err(|error| error.throw(scope))?;
    if !iterable {
        return Err(js::error::TypeError("codePoints is not iterable".into()).throw(scope));
    }
    let mut points = with_capacity(elements.len()).map_err(|error| error.throw(scope))?;
    for element in elements {
        let point = f64::from_jsval(scope, element, ()).map_err(|_| ExnThrown)?;
        if point.fract() != 0.0 || !(0.0..=1114111.0).contains(&point) {
            return Err(PunycodeError::InvalidCodePoint(point).throw(scope));
        }
        points.push(point as u32);
    }
    let units = from_code_points(&points).map_err(|error| error.throw(scope))?;
    Ok(js::JSString::from_utf16(scope, &units)?.as_value())
}

/// Emit DEP0040 for loading the module, unless the load comes from a module in `node_modules`.
fn warn_deprecated(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    if crate::process::called_from_node_modules(scope)? {
        return Ok(());
    }
    crate::process::deprecation_warning(
        scope,
        "The `punycode` module is deprecated. Please use a userland alternative instead.",
        "DEP0040",
    )
}

/// Register `node:punycode` with `ucs2`, in Node.js's property order, and emit DEP0040.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let ucs2 = || -> Result<HandleValue<'_>, ExnThrown> {
        let ucs2 = Object::new_plain(scope)?;
        let decode = Function::new_callback(scope, c"ucs2decode", 1, ucs2_decode, ())?;
        let encode = Function::new_callback(scope, c"ucs2encode", 1, ucs2_encode, ())?;
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
    if !crate::stub::register_implemented_with::<module::js_module>(scope, name, &[("ucs2", ucs2)])
    {
        return false;
    }
    let order = [
        c"version",
        c"ucs2",
        c"decode",
        c"encode",
        c"toASCII",
        c"toUnicode",
    ];
    crate::stub::order_default_export(scope, name, &order).is_ok()
}
