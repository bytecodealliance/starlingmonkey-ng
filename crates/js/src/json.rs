// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! JSON parsing and serialization.
//!
//! This module wraps SpiderMonkey's JSON API, providing safe access to
//! `JSON.parse` and `JSON.stringify` operations.

use super::error::ExnThrown;
use super::string::Str;
use crate::gc::scope::Scope;
use js::Object;
use mozjs::gc::{HandleObject, HandleString, HandleValue};
use mozjs::jsval::UndefinedValue;
use mozjs::rust::wrappers2;

/// Parse a JSON string into a JS value.
///
/// Accepts a Rust `&str` and parses it using SpiderMonkey's JSON parser.
pub fn parse<'r>(scope: &'r Scope<'_>, json: &str) -> Result<HandleValue<'r>, ExnThrown> {
    // A JS string keeps Latin-1 text in one byte per character, and the parser reads it as such.
    let string = Str::from_str(scope, json)?;
    parse_js_string(scope, string.handle())
}

/// Parse a JSON string (represented as a `JSString`) into a JS value.
pub fn parse_js_string<'r>(
    scope: &'r Scope<'_>,
    json_str: HandleString,
) -> Result<HandleValue<'r>, ExnThrown> {
    let mut rval = scope.root_value_mut(UndefinedValue());
    let ok = unsafe { wrappers2::JS_ParseJSON1(scope.cx_mut(), json_str, rval.reborrow()) };
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// Parse a JSON string from UTF-16 chars into a JS value.
///
/// # Safety
///
/// `chars` must point to a valid UTF-16 buffer of at least `len` code units.
pub unsafe fn parse_utf16<'r>(
    scope: &'r Scope<'_>,
    chars: *const u16,
    len: u32,
) -> Result<HandleValue<'r>, ExnThrown> {
    let mut rval = scope.root_value_mut(UndefinedValue());
    let ok = wrappers2::JS_ParseJSON(scope.cx_mut(), chars, len, rval.reborrow());
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// Parse a JSON string with a reviver function.
///
/// Accepts a Rust `&str` and parses it with a JS reviver function.
pub fn parse_with_reviver<'r>(
    scope: &'r Scope<'_>,
    json: &str,
    reviver: HandleValue,
) -> Result<HandleValue<'r>, ExnThrown> {
    let string = Str::from_str(scope, json)?;
    parse_js_string_with_reviver(scope, string.handle(), reviver)
}

/// Parse JSON with a reviver function (JS string input).
pub fn parse_js_string_with_reviver<'r>(
    scope: &'r Scope<'_>,
    json_str: HandleString,
    reviver: HandleValue,
) -> Result<HandleValue<'r>, ExnThrown> {
    let mut rval = scope.root_value_mut(UndefinedValue());
    let ok = unsafe {
        wrappers2::JS_ParseJSONWithReviver1(scope.cx_mut(), json_str, reviver, rval.reborrow())
    };
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// Performs the [`JSON.stringify`][stringify] operation, as specified by ECMAScript.
///
/// [stringify]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/JSON/stringify
///
/// Returns `None` if `JSON.stringify` returns `undefined`.
// TODO: consider turning `replacer` and `space` into enums.
pub fn stringify(
    scope: &Scope<'_>,
    value: HandleValue,
    replacer: Option<Object<'_>>,
    space: Option<HandleValue>,
) -> Result<Option<String>, ExnThrown> {
    let mut output = StringifyOutput { text: None };
    let replacer = if let Some(obj) = replacer {
        obj.handle()
    } else {
        HandleObject::null()
    };

    struct StringifyOutput {
        /// Populated by [`callback`], stays `None` if stringification doesn't yield a string.
        text: Option<String>,
    }

    // SAFETY: `data` must only be called by SpiderMonkey's `ToJSON`, which happens below.
    unsafe extern "C" fn callback(
        buf: *const u16,
        len: u32,
        data: *mut std::os::raw::c_void,
    ) -> bool {
        let output = &mut *(data as *mut StringifyOutput);
        let units = std::slice::from_raw_parts(buf, len as usize);
        output.text = Some(String::from_utf16_lossy(units));
        true
    }

    // SAFETY: `callback` matches `JSONWriteCallback`, and `output` outlives the call.
    let ok = unsafe {
        wrappers2::ToJSON(
            scope.cx_mut(),
            value,
            replacer,
            space.unwrap_or(HandleValue::undefined()),
            Some(callback),
            &raw mut output as *mut std::os::raw::c_void,
        )
    };

    ExnThrown::check(ok)?;
    Ok(output.text)
}
