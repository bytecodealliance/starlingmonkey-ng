// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Array creation and element access.
//!
//! SpiderMonkey arrays are ordinary objects with a special `length` property.
//! The [`Array`] marker type implements [`JSType`](crate::gc::handle::JSType),
//! enabling [`Stack<'s, Array>`](crate::gc::handle::Stack) as the scope-rooted
//! handle type. It implements `Deref` to [`Stack<Object>`](crate::Object),
//! so all property and prototype methods are available directly.

use crate::builtins::JSType;
use crate::conversion::ToJSVal;
use crate::gc::handle::Stack;
use crate::gc::scope::Scope;
use crate::value::ValueArrayRooter;
use mozjs::rust::wrappers2;
use mozjs::rust::{HandleObject, HandleValue};

use super::error::ExnThrown;

/// Marker type for JavaScript `Array` objects.
///
/// Use the `js::Array` alias for [`Stack<'s, Array>`](crate::gc::handle::Stack)
/// as the scope-rooted handle type:
///
/// ```ignore
/// let arr = js::Array::new(&scope, 5)?;
/// let len = arr.length(&scope)?;
/// ```
pub struct Array;

impl JSType for Array {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "Array";

    fn js_class() -> *const mozjs::jsapi::JSClass {
        crate::class::proto_key_to_class(mozjs::jsapi::JSProtoKey::JSProto_Array)
    }
}

impl<'s> Stack<'s, Array> {
    /// Create a new empty array with the given initial length.
    pub fn new(scope: &'s Scope<'s>, length: usize) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::NewArrayObject1(scope.cx_mut(), length) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new array pre-populated with the given values.
    pub fn with_contents(
        scope: &'s Scope<'s>,
        contents: &[impl ToJSVal<'s>],
    ) -> Result<Self, ExnThrown> {
        let mut contents_root = ValueArrayRooter::new(contents.len());
        let contents = contents_root.root(scope, contents)?;
        let obj = unsafe { wrappers2::NewArrayObject(scope.cx_mut(), &contents.handles()) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Get the `length` of this array.
    pub fn length(&self, scope: &Scope<'_>) -> Result<u32, ExnThrown> {
        let mut len: u32 = 0;
        let ok = unsafe { wrappers2::GetArrayLength(scope.cx_mut(), self.handle(), &mut len) };
        ExnThrown::check(ok)?;
        Ok(len)
    }

    /// Set the `length` of this array.
    pub fn set_length(&self, scope: &Scope<'_>, length: u32) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::SetArrayLength(scope.cx_mut(), self.handle(), length) };
        ExnThrown::check(ok)
    }

    /// Check whether an object is an `Array`.
    pub fn is_array(scope: &Scope<'_>, obj: HandleObject) -> Result<bool, ExnThrown> {
        let mut result = false;
        let ok = unsafe { wrappers2::IsArrayObject1(scope.cx_mut(), obj, &mut result) };
        ExnThrown::check(ok)?;
        Ok(result)
    }

    /// ECMAScript's `IsArray`, which `Array.isArray` applies: whether `obj` is an `Array` or a
    /// `Proxy` whose target, through any number of proxies, is one. A revoked proxy throws a
    /// `TypeError`.
    pub fn is_array_or_array_proxy(
        scope: &Scope<'_>,
        obj: HandleObject,
    ) -> Result<bool, ExnThrown> {
        let mut result = false;
        let ok = unsafe { wrappers2::IsArray(scope.cx_mut(), obj, &mut result) };
        ExnThrown::check(ok)?;
        Ok(result)
    }

    /// Check whether a value is an `Array`.
    pub fn is_array_value(scope: &Scope<'_>, value: HandleValue) -> Result<bool, ExnThrown> {
        let mut result = false;
        let ok = unsafe { wrappers2::IsArrayObject(scope.cx_mut(), value, &mut result) };
        ExnThrown::check(ok)?;
        Ok(result)
    }
}

crate::gc::handle::deref_to_object!(Array);

crate::gc::handle::from_jsval_via_cast!(Array, c"Value isn't an Array");
