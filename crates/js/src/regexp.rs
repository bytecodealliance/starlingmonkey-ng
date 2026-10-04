// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Regular expression creation, execution, and inspection.
//!
//! The [`RegExp`] marker type implements [`JSType`](crate::gc::handle::JSType),
//! enabling [`RegExp<'s>`](crate::RegExp) as the scope-rooted
//! handle type. It provides methods for creating and testing regular
//! expressions.

use std::ffi::CStr;
use std::ptr::NonNull;

use crate::builtins::JSType;
use crate::gc::handle::Stack;
use crate::gc::scope::Scope;
use mozjs::gc::HandleValue;
use mozjs::jsapi::JSString;
pub use mozjs::jsapi::RegExpFlags;
use mozjs::jsval::UndefinedValue;
use mozjs::rust::wrappers2;
use mozjs::rust::HandleObject;

use super::error::ExnThrown;

/// Marker type for JavaScript `RegExp` objects.
///
/// [`RegExp<'s>`](crate::RegExp) is the scope-rooted handle type:
///
/// ```ignore
/// let re = js::RegExp::new(&scope, c"\\d+", flags)?;
/// let src = re.source(&scope)?;
/// ```
pub struct RegExp;

impl JSType for RegExp {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "RegExp";

    fn js_class() -> *const mozjs::jsapi::JSClass {
        crate::class::proto_key_to_class(mozjs::jsapi::JSProtoKey::JSProto_RegExp)
    }
}

impl<'s> Stack<'s, RegExp> {
    /// Create a new `RegExp` object from a Latin-1 (byte) pattern and flags.
    pub fn new(
        scope: &'s Scope<'_>,
        pattern: &CStr,
        flags: RegExpFlags,
    ) -> Result<Self, ExnThrown> {
        let bytes = pattern.as_ptr();
        let len = pattern.to_bytes().len();
        let obj = unsafe { wrappers2::NewRegExpObject(scope.cx_mut(), bytes, len, flags) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new `RegExp` object from a UTF-16 pattern and flags.
    pub fn from_utf16(
        scope: &'s Scope<'_>,
        chars: &[u16],
        flags: RegExpFlags,
    ) -> Result<Self, ExnThrown> {
        let obj = unsafe {
            wrappers2::NewUCRegExpObject(scope.cx_mut(), chars.as_ptr(), chars.len(), flags)
        };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Check whether an object is a `RegExp`.
    pub fn is_regexp(scope: &Scope<'_>, obj: HandleObject) -> Result<bool, ExnThrown> {
        let mut result = false;
        // SAFETY: cx and obj are valid; ObjectIsRegExp writes to result.
        let ok = unsafe { wrappers2::ObjectIsRegExp(scope.cx_mut(), obj, &mut result) };
        ExnThrown::check(ok)?;
        Ok(result)
    }

    /// Get the source pattern string of this `RegExp`.
    pub fn source<'a>(
        &self,
        scope: &'a Scope<'_>,
    ) -> Result<mozjs::gc::Handle<'a, *mut JSString>, ExnThrown> {
        let s = unsafe { wrappers2::GetRegExpSource(scope.cx_mut(), self.handle()) };
        NonNull::new(s)
            .map(|p| scope.root_string(p))
            .ok_or(ExnThrown)
    }

    /// Execute this `RegExp` against a UTF-16 string without modifying statics.
    ///
    /// If `test` is true, only tests for a match (the result value is a boolean).
    /// If `test` is false, returns the match result array.
    ///
    /// `indexp` is the byte index to start searching from (updated on return).
    ///
    /// # Safety
    ///
    /// The `chars` slice must be a valid UTF-16 string and must remain valid for
    /// the duration of this call.
    pub unsafe fn execute_no_statics<'r>(
        &self,
        scope: &'r Scope<'_>,
        chars: &[u16],
        indexp: &mut usize,
        test: bool,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = wrappers2::ExecuteRegExpNoStatics(
            scope.cx_mut(),
            self.handle(),
            chars.as_ptr(),
            chars.len(),
            indexp,
            test,
            rval.reborrow(),
        );
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Check whether a regular expression pattern is syntactically valid.
    ///
    /// If the pattern is invalid, returns the error value. If valid,
    /// returns `undefined`.
    pub fn check_syntax<'r>(
        scope: &'r Scope<'_>,
        chars: &[u16],
        flags: RegExpFlags,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let mut error = scope.root_value_mut(UndefinedValue());
        let ok = unsafe {
            wrappers2::CheckRegExpSyntax(
                scope.cx_mut(),
                chars.as_ptr(),
                chars.len(),
                flags,
                error.reborrow(),
            )
        };
        ExnThrown::check(ok)?;
        Ok(error.handle())
    }
}

/// The flags the flag characters of `flags` name, such as `gi` for global and case-insensitive
/// matching. Other characters are ignored.
pub fn flags_from_str(flags: &str) -> RegExpFlags {
    use mozjs::jsapi::JS::*;
    let flags = flags.chars().fold(RegExpFlag_NoFlags, |flags, flag| {
        flags
            | match flag {
                'd' => RegExpFlag_HasIndices,
                'g' => RegExpFlag_Global,
                'i' => RegExpFlag_IgnoreCase,
                'm' => RegExpFlag_Multiline,
                's' => RegExpFlag_DotAll,
                'u' => RegExpFlag_Unicode,
                'v' => RegExpFlag_UnicodeSets,
                'y' => RegExpFlag_Sticky,
                _ => RegExpFlag_NoFlags,
            }
    });
    RegExpFlags { flags_: flags }
}

crate::gc::handle::deref_to_object!(RegExp);

crate::gc::handle::from_jsval_via_cast!(RegExp, c"Value isn't a RegExp");
