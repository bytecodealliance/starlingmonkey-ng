// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! JS string creation, encoding, comparison, and atom operations.
//!
//! SpiderMonkey strings are GC-managed and encoded internally as either Latin1
//! or two-byte (UTF-16). Unlike `Object`, `Array`, or `Function`, JS strings
//! are a separate GC-managed type (`*mut JSString`), not JS objects.
//!
//! The [`Str<'s>`] newtype wraps a scope-rooted `Handle<'s, *mut JSString>`
//! and exposes all string operations as methods. The public type alias
//! [`JSString<'s>`](crate::JSString) is the preferred name.
//!
//! # Creating Strings
//!
//! ```ignore
//! let s = js::JSString::from_str(&scope, "hello")?;
//! let c = js::JSString::from_cstr(&scope, c"world")?;
//! let e = js::JSString::empty(&scope);
//! ```
//!
//! # Extracting Content
//!
//! ```ignore
//! let rust_str: String = s.to_utf8(&scope)?;
//! let ch: u16 = s.char_at(&scope, 0)?;
//! let len: usize = s.len();
//! ```

use std::ffi::CStr;
use std::os::raw::c_char;
use std::ptr::NonNull;

use super::error::ExnThrown;
use crate::conversion::{ConversionError, ToJSVal, Utf8Chars};
use crate::gc::scope::Scope;
use mozjs::gc::Handle;
use mozjs::jsapi::{JSLinearString, JSString};
use mozjs::jsval::StringValue;
use mozjs::rust::wrappers2;
use smallvec::SmallVec;

/// Create an unrooted JS string holding `s`, in the smallest encoding that represents it. Returns
/// null with an exception pending if allocation fails.
///
/// ASCII text is copied as Latin-1 directly. Other text that fits in Latin-1 is converted with
/// `encoding_rs`' vectorized routine before SpiderMonkey copies it, since SpiderMonkey's own
/// UTF-8 to Latin-1 conversion (`JS_NewStringCopyUTF8N`) converts one code point at a time. Text
/// that needs UTF-16 goes through `JS_NewStringCopyUTF8N`, which converts it directly into the
/// string's buffer.
pub(crate) fn new_js_string(scope: &Scope<'_>, s: &str) -> *mut JSString {
    let bytes = s.as_bytes();
    let ascii = encoding_rs::Encoding::ascii_valid_up_to(bytes);
    if ascii == bytes.len() {
        // SAFETY: ASCII bytes are valid Latin-1 characters.
        return unsafe {
            wrappers2::JS_NewStringCopyN(scope.cx_mut(), bytes.as_ptr().cast(), bytes.len())
        };
    }
    if encoding_rs::mem::is_str_latin1(&s[ascii..]) {
        // The Latin-1 text has fewer characters than `s` has bytes.
        let mut latin1: SmallVec<[u8; 256]> = SmallVec::from_elem(0, bytes.len());
        let written = encoding_rs::mem::convert_utf8_to_latin1_lossy(bytes, &mut latin1);
        // SAFETY: `latin1[..written]` holds Latin-1 characters.
        return unsafe {
            wrappers2::JS_NewStringCopyN(scope.cx_mut(), latin1.as_ptr().cast(), written)
        };
    }
    let utf8 = Utf8Chars::from(s);
    // SAFETY: `utf8` borrows `s`, which outlives the call.
    unsafe { wrappers2::JS_NewStringCopyUTF8N(scope.cx_mut(), &*utf8 as *const _) }
}

/// The characters of a JS string, in the representation the string stores them in.
pub enum StrChars<'a> {
    /// One byte per character, U+0000 to U+00FF.
    Latin1(&'a [u8]),
    /// UTF-16 code units, which may include unpaired surrogates.
    TwoByte(&'a [u16]),
}

// ---------------------------------------------------------------------------
// Str — scope-rooted JS string handle
// ---------------------------------------------------------------------------

/// A scope-rooted handle to a SpiderMonkey string.
///
/// Unlike [`Object`](crate::Object) and other builtin handle types, JS
/// strings are not JSObjects — they are a separate GC-managed type. `Str<'s>`
/// wraps a `Handle<'s, *mut JSString>` and provides all string operations as
/// methods.
///
/// The public type alias [`JSString<'s>`](crate::JSString) is the preferred
/// name for this type.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct Str<'s> {
    handle: Handle<'s, *mut JSString>,
}

impl<'s> Str<'s> {
    // ---------------------------------------------------------------------------
    // Construction from a raw handle
    // ---------------------------------------------------------------------------

    /// Root a raw string pointer, mapping null to `ExnThrown`.
    ///
    /// JSAPI string functions return null with an exception pending, so
    /// this is the standard wrapper for their results.
    fn from_mozjs_rval(scope: &'s Scope<'_>, ptr: *mut JSString) -> Result<Self, ExnThrown> {
        NonNull::new(ptr)
            .map(|p| Str::from_non_null(scope, p))
            .ok_or(ExnThrown)
    }

    /// Root `ptr` in `scope`.
    pub fn from_non_null(scope: &'s Scope<'_>, ptr: NonNull<JSString>) -> Self {
        Str {
            handle: scope.root_string(ptr),
        }
    }

    /// Wrap a rooted string handle, or `None` if it is null.
    pub fn from_handle(handle: Handle<'s, *mut JSString>) -> Option<Self> {
        (!handle.get().is_null()).then_some(Str { handle })
    }

    /// Get the underlying rooted handle.
    pub fn handle(self) -> Handle<'s, *mut JSString> {
        self.handle
    }

    /// Get the raw `*mut JSString` pointer.
    pub fn as_raw(self) -> *mut JSString {
        self.handle.get()
    }

    /// Convert to a JS `Value` containing this string.
    pub fn as_value(self) -> mozjs::jsapi::Value {
        // SAFETY: The string is rooted via the handle.
        unsafe { StringValue(&*self.handle.get()) }
    }

    // ---------------------------------------------------------------------------
    // String creation
    // ---------------------------------------------------------------------------

    /// Create a new JS string from a Rust `&str`.
    pub fn from_str(scope: &'s Scope<'_>, s: &str) -> Result<Self, ExnThrown> {
        Self::from_mozjs_rval(scope, new_js_string(scope, s))
    }

    /// Create a new JS string from UTF-8 bytes, with each maximal invalid subsequence replaced by
    /// U+FFFD. The replaced text is allocated fallibly, and a failed allocation reports out of
    /// memory.
    pub fn from_utf8_lossy(scope: &'s Scope<'_>, bytes: &[u8]) -> Result<Self, ExnThrown> {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Self::from_str(scope, text);
        }
        const REPLACEMENT: &str = "\u{FFFD}";
        let length = bytes.utf8_chunks().fold(0usize, |length, chunk| {
            let replacement = if chunk.invalid().is_empty() {
                0
            } else {
                REPLACEMENT.len()
            };
            length
                .saturating_add(chunk.valid().len())
                .saturating_add(replacement)
        });
        let mut text = String::new();
        if text.try_reserve_exact(length).is_err() {
            crate::exception::report_out_of_memory(scope);
            return Err(ExnThrown);
        }
        for chunk in bytes.utf8_chunks() {
            text.push_str(chunk.valid());
            if !chunk.invalid().is_empty() {
                text.push_str(REPLACEMENT);
            }
        }
        Self::from_str(scope, &text)
    }

    /// Create a new JS string from Latin-1 characters, one byte each.
    // TODO: explore whether we can use the non-copying `JS_NewStrin*` variants here and below.
    pub fn from_latin1(scope: &'s Scope<'_>, chars: &[u8]) -> Result<Self, ExnThrown> {
        // SAFETY: every byte is a Latin-1 character.
        let js_str = unsafe {
            wrappers2::JS_NewStringCopyN(scope.cx_mut(), chars.as_ptr().cast(), chars.len())
        };
        Self::from_mozjs_rval(scope, js_str)
    }

    /// Create a new JS string from a null-terminated C string.
    pub fn from_cstr(scope: &'s Scope<'_>, s: &CStr) -> Result<Self, ExnThrown> {
        let js_str = unsafe { wrappers2::JS_NewStringCopyZ(scope.cx_mut(), s.as_ptr()) };
        Self::from_mozjs_rval(scope, js_str)
    }

    /// Create a new JS string from a UTF-16 slice.
    pub fn from_utf16(scope: &'s Scope<'_>, s: &[u16]) -> Result<Self, ExnThrown> {
        let js_str = unsafe { wrappers2::JS_NewUCStringCopyN(scope.cx_mut(), s.as_ptr(), s.len()) };
        Self::from_mozjs_rval(scope, js_str)
    }

    /// Get the empty string for this context.
    ///
    /// The empty string is permanently rooted by the runtime, so this never
    /// fails.
    pub fn empty(scope: &'s Scope<'_>) -> Self {
        let ptr = unsafe { wrappers2::JS_GetEmptyString(scope.cx()) };
        // SAFETY: The empty string is always present in a valid runtime.
        let nn = unsafe { NonNull::new_unchecked(ptr) };
        Str::from_non_null(scope, nn)
    }

    /// Convert a JS value to a string via the `ToString` abstract operation.
    ///
    /// Throws a `TypeError` for symbols — unlike `String(value)`, which
    /// special-cases them to their description.
    pub fn from_value(scope: &'s Scope<'_>, val: impl ToJSVal<'s>) -> Result<Self, ExnThrown> {
        let val = val.to_jsval_throwing(scope)?;
        let js_str = unsafe { mozjs::rust::ToString(scope.cx_mut(), val) };
        Self::from_mozjs_rval(scope, js_str)
    }

    // ---------------------------------------------------------------------------
    // String encoding / extraction
    // ---------------------------------------------------------------------------

    /// Encode this string to UTF-8, returning an owned Rust [`String`].
    pub fn to_utf8(&self, scope: &Scope<'_>) -> Result<String, ExnThrown> {
        let nn = NonNull::new(self.as_raw()).expect("a Str is never null");
        Ok(crate::conversion::jsstr_to_string(scope, nn))
    }

    /// The length of this string in UTF-8, with each lone surrogate counted as the three bytes of
    /// U+FFFD.
    pub fn utf8_length(&self, scope: &Scope<'_>) -> Result<usize, ExnThrown> {
        // SAFETY: counting makes no call into SpiderMonkey.
        unsafe {
            self.with_chars(scope, |chars| match chars {
                StrChars::Latin1(bytes) => {
                    bytes.len() + bytes.iter().filter(|byte| **byte >= 0x80).count()
                }
                StrChars::TwoByte(units) => char::decode_utf16(units.iter().copied())
                    .map(|c| c.map_or(3, char::len_utf8))
                    .sum(),
            })
        }
    }

    /// Write this string as UTF-8 into the bytes `range` of `view`, with each lone surrogate as
    /// U+FFFD, up to the last whole character that fits. Returns the number of bytes written.
    ///
    /// # Panics
    ///
    /// Panics if `range` doesn't lie within `view`.
    pub fn write_utf8(
        &self,
        scope: &Scope<'_>,
        view: crate::ArrayBufferView<'_>,
        range: std::ops::Range<usize>,
    ) -> Result<usize, ExnThrown> {
        // SAFETY: the closure makes no call into SpiderMonkey.
        unsafe {
            self.with_chars(scope, |chars| {
                view.with_bytes_mut(|bytes| {
                    let buffer = &mut bytes[range];
                    match chars {
                        StrChars::Latin1(bytes) => {
                            encoding_rs::mem::convert_latin1_to_utf8_partial(bytes, buffer).1
                        }
                        StrChars::TwoByte(units) => {
                            encoding_rs::mem::convert_utf16_to_utf8_partial(units, buffer).1
                        }
                    }
                })
            })
        }
    }

    /// Get a single character (code unit) at the given index.
    pub fn char_at(&self, scope: &Scope<'_>, index: usize) -> Result<u16, ExnThrown> {
        let mut ch: u16 = 0;
        let ok =
            unsafe { wrappers2::JS_GetStringCharAt(scope.cx(), self.as_raw(), index, &mut ch) };
        ExnThrown::check(ok)?;
        Ok(ch)
    }

    // ---------------------------------------------------------------------------
    // Atom operations
    // ---------------------------------------------------------------------------

    /// Turns a Rust string into an atomized JS string.
    ///
    /// Atomized strings are canonicalized and deduplicated by the engine, so atomizing the same
    /// string multiple times yields the same pointer, which can then be used for identity
    /// comparison.
    pub fn atomize(scope: &'s Scope<'_>, s: &str) -> Result<Self, ExnThrown> {
        let js_str = if s.is_ascii() {
            unsafe {
                wrappers2::JS_AtomizeStringN(scope.cx(), s.as_ptr() as *const c_char, s.len())
            }
        } else {
            let units: Vec<u16> = s.encode_utf16().collect();
            unsafe { wrappers2::JS_AtomizeUCStringN(scope.cx(), units.as_ptr(), units.len()) }
        };
        Self::from_mozjs_rval(scope, js_str)
    }

    /// Turns a Rust string into an atomized JS string that's kept alive for the lifetime of the
    /// runtime.
    ///
    /// SpiderMonkey supports pinning only for Latin-1, so strings containing non-latin-1 characters are rejected with a TypeError.
    ///
    /// Note: use [`atomize`](Self::atomize) if pinning isn't needed.
    pub fn atomize_and_pin(scope: &'s Scope<'_>, s: &str) -> Result<Self, ExnThrown> {
        let js_str = if s.is_ascii() {
            unsafe {
                wrappers2::JS_AtomizeAndPinStringN(scope.cx(), s.as_ptr() as *const c_char, s.len())
            }
        } else if s.chars().all(|c| (c as u32) <= 0xFF) {
            let latin1: Vec<u8> = s.chars().map(|c| c as u8).collect();
            unsafe {
                wrappers2::JS_AtomizeAndPinStringN(
                    scope.cx(),
                    latin1.as_ptr() as *const c_char,
                    latin1.len(),
                )
            }
        } else {
            return Err(crate::error::throw_type_error(
                scope,
                c"cannot pin an atom containing code points above U+00FF",
            ));
        };
        Self::from_mozjs_rval(scope, js_str)
    }

    /// Check whether this atomized string has been pinned.
    pub fn has_been_pinned(&self, scope: &Scope<'_>) -> bool {
        unsafe { wrappers2::JS_StringHasBeenPinned(scope.cx(), self.as_raw()) }
    }

    // ---------------------------------------------------------------------------
    // String utilities
    // ---------------------------------------------------------------------------

    /// Concatenate this string with another, returning a new string.
    pub fn concat(&self, scope: &'s Scope<'_>, other: Str<'_>) -> Result<Self, ExnThrown> {
        let result =
            unsafe { wrappers2::JS_ConcatStrings(scope.cx_mut(), self.handle, other.handle) };
        Self::from_mozjs_rval(scope, result)
    }

    /// Compare this string with another, returning a result like `strcmp`.
    ///
    /// Returns `< 0` if `self < other`, `0` if equal, `> 0` if `self > other`.
    pub fn compare(&self, scope: &Scope<'_>, other: Str<'_>) -> Result<i32, ExnThrown> {
        let mut result: i32 = 0;
        let ok = unsafe {
            wrappers2::JS_CompareStrings(scope.cx(), self.as_raw(), other.as_raw(), &mut result)
        };
        ExnThrown::check(ok)?;
        Ok(result)
    }

    /// Check whether this string equals an ASCII string literal.
    pub fn equals_ascii(&self, scope: &Scope<'_>, ascii: &CStr) -> Result<bool, ExnThrown> {
        let mut matched = false;
        let ok = unsafe {
            wrappers2::JS_StringEqualsAscii(scope.cx(), self.as_raw(), ascii.as_ptr(), &mut matched)
        };
        ExnThrown::check(ok)?;
        Ok(matched)
    }

    /// Get the length in code units.
    pub fn len(&self) -> usize {
        unsafe { mozjs::jsapi::JS_GetStringLength(self.as_raw()) }
    }

    /// Check whether this string is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Check whether this string has a linear (flat) representation.
    pub fn is_linear(&self) -> bool {
        unsafe { mozjs::jsapi::JS_StringIsLinear(self.as_raw()) }
    }

    /// Call `f` with this string's characters, flattening the string first if it is a rope.
    ///
    /// # Safety
    ///
    /// A garbage collection can move the characters, so `f` must not call into SpiderMonkey.
    pub unsafe fn with_chars<R>(
        &self,
        scope: &Scope<'_>,
        f: impl FnOnce(StrChars<'_>) -> R,
    ) -> Result<R, ExnThrown> {
        let linear = self.ensure_linear(scope)?.as_ptr().cast::<JSString>();
        let mut length = 0;
        // SAFETY: `linear` is a live linear string, whose characters stay in place until the next
        // call into SpiderMonkey, which `f` does not make.
        unsafe {
            if mozjs::jsapi::JS_DeprecatedStringHasLatin1Chars(linear) {
                let chars =
                    wrappers2::JS_GetLatin1StringCharsAndLength(scope.cx(), linear, &mut length);
                Ok(f(StrChars::Latin1(std::slice::from_raw_parts(
                    chars, length,
                ))))
            } else {
                let chars =
                    wrappers2::JS_GetTwoByteStringCharsAndLength(scope.cx(), linear, &mut length);
                Ok(f(StrChars::TwoByte(std::slice::from_raw_parts(
                    chars, length,
                ))))
            }
        }
    }

    /// This string's UTF-16 code units. Reports out of memory if the vector can't be allocated.
    pub fn to_utf16_vec(&self, scope: &Scope<'_>) -> Result<Vec<u16>, ExnThrown> {
        self.to_utf16_prefix(scope, self.len())
    }

    /// The first `count` UTF-16 code units of this string, or all of them if it is shorter.
    /// Reports out of memory if the vector can't be allocated.
    pub fn to_utf16_prefix(&self, scope: &Scope<'_>, count: usize) -> Result<Vec<u16>, ExnThrown> {
        let mut units = Vec::new();
        // SAFETY: the closure doesn't call into SpiderMonkey.
        let copied = unsafe {
            self.with_chars(scope, |chars| {
                match chars {
                    StrChars::Latin1(chars) => {
                        let chars = &chars[..count.min(chars.len())];
                        units.try_reserve_exact(chars.len()).ok()?;
                        units.extend(chars.iter().map(|&c| u16::from(c)));
                    }
                    StrChars::TwoByte(chars) => {
                        let chars = &chars[..count.min(chars.len())];
                        units.try_reserve_exact(chars.len()).ok()?;
                        units.extend_from_slice(chars);
                    }
                }
                Some(())
            })
        }?;
        if copied.is_none() {
            crate::exception::report_out_of_memory(scope);
            return Err(ExnThrown);
        }
        Ok(units)
    }

    /// Ensure this string has a linear (flat) representation.
    ///
    /// Returns the linear string pointer, or an error if allocation fails.
    pub fn ensure_linear(&self, scope: &Scope<'_>) -> Result<NonNull<JSLinearString>, ExnThrown> {
        let result = unsafe { wrappers2::JS_EnsureLinearString(scope.cx(), self.as_raw()) };
        NonNull::new(result).ok_or(ExnThrown)
    }

    /// Create a dependent (substring) string.
    pub fn substring(
        &self,
        scope: &'s Scope<'_>,
        start: usize,
        length: usize,
    ) -> Result<Self, ExnThrown> {
        let result =
            unsafe { wrappers2::JS_NewDependentString(scope.cx(), self.handle, start, length) };
        Self::from_mozjs_rval(scope, result)
    }

    /// Get the encoding (byte) length when encoded to Latin-1.
    pub fn encoding_length(&self, scope: &Scope<'_>) -> usize {
        unsafe { wrappers2::JS_GetStringEncodingLength(scope.cx(), self.as_raw()) }
    }

    /// Create an external Latin1 string backed by caller-owned memory.
    ///
    /// # Safety
    ///
    /// - `chars` must remain valid for the lifetime of the string.
    /// - `callbacks` must handle deallocation correctly.
    pub unsafe fn new_external_latin1(
        scope: &'s Scope<'_>,
        chars: *const u8,
        length: usize,
        callbacks: *const mozjs::jsapi::JSExternalStringCallbacks,
    ) -> Result<Self, ExnThrown> {
        let result =
            wrappers2::JS_NewExternalStringLatin1(scope.cx_mut(), chars, length, callbacks);
        Self::from_mozjs_rval(scope, result)
    }

    /// Create an external two-byte string backed by caller-owned memory.
    ///
    /// # Safety
    ///
    /// - `chars` must remain valid for the lifetime of the string.
    /// - `callbacks` must handle deallocation correctly.
    pub unsafe fn new_external_uc(
        scope: &'s Scope<'_>,
        chars: *const u16,
        length: usize,
        callbacks: *const mozjs::jsapi::JSExternalStringCallbacks,
    ) -> Result<Self, ExnThrown> {
        let result = wrappers2::JS_NewExternalUCString(scope.cx_mut(), chars, length, callbacks);
        Self::from_mozjs_rval(scope, result)
    }
}

/// The value converted with `ToString`.
///
/// Used for WebIDL `DOMString` types.
impl<'s, 'v> crate::conversion::FromJSVal<'s, 'v> for Str<'s> {
    type Config = ();
    fn from_jsval(
        scope: &'s Scope<'s>,
        val: mozjs::gc::HandleValue<'v>,
        _: (),
    ) -> Result<Self, ConversionError> {
        // SAFETY: `val` is rooted.
        let js_str = unsafe { mozjs::rust::ToString(scope.cx_mut(), val) };
        Str::from_mozjs_rval(scope, js_str).map_err(|_| ConversionError::ExnPending)
    }
}

impl ToJSVal<'_> for Str<'_> {
    #[inline]
    fn to_jsval_raw(&self, _scope: &Scope<'_>) -> Result<crate::value::Value, ConversionError> {
        Ok(self.as_value())
    }
}

impl<'s> std::fmt::Debug for Str<'s> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JSString")
            .field("ptr", &self.as_raw())
            .finish()
    }
}
