// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The WebIDL [`ByteString`] type.
//!
//! [`ByteString`]: https://webidl.spec.whatwg.org/#idl-ByteString
//!
//! A `ByteString` is a string whose code units are all in the range 0x00–0xFF.
//! Conversion first stringifies the value (like `DOMString`), then throws a
//! `TypeError` if any code unit exceeds 255. Header names, header values, and
//! request methods are all `ByteString`s in the Fetch IDL.

use std::borrow::Cow;

use js::conversion::{ConversionError, FromJSVal, ToJSVal};
use js::gc::scope::Scope;
use js::prelude::HandleValue;

/// A WebIDL `ByteString`. The inner `String`'s `char`s are guaranteed to all be
/// ≤ U+00FF, so each maps one-to-one to a byte.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ByteString(String);

impl ByteString {
    /// Borrow the contents as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner `String`.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for ByteString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'s, 'v> FromJSVal<'s, 'v> for ByteString {
    type Config = ();

    fn from_jsval(
        scope: &'s Scope<'s>,
        val: HandleValue<'v>,
        _: (),
    ) -> Result<Self, ConversionError> {
        // WebIDL ByteString conversion: stringify, then reject any code unit > 255.
        let s = String::from_jsval(scope, val, ())?;
        if s.chars().any(|c| c as u32 > 0xFF) {
            return Err(ConversionError::Failure(Cow::Borrowed(
                c"Cannot convert value to a ByteString because a code unit is greater than 255",
            )));
        }
        Ok(ByteString(s))
    }
}

impl<'s> ToJSVal<'s> for ByteString {
    fn to_jsval_raw(&self, scope: &'s Scope<'s>) -> Result<js::value::Value, ConversionError> {
        self.0.to_jsval_raw(scope)
    }
}
