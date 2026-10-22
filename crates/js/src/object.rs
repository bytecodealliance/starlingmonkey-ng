// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Object creation, property access, prototype chain, and object utilities.
//!
//! The [`Object`] marker type implements [`JSType`](crate::gc::handle::JSType),
//! enabling [`Object<'s>`](crate::Object) as the scope-rooted object handle
//! type. All other builtins (`Array<'s>`, `Promise<'s>`, etc.) deref to
//! `Object<'s>`, so property access methods are available on every builtin
//! type.
//!
//! # Property Access
//!
//! Properties can be accessed by name (`&CStr`) or by index (`u32`):
//!
//! ```ignore
//! let obj = Object::new_plain(&scope)?;
//! obj.set_property(&scope, c"foo", val)?;
//! let val = obj.get_property(&scope, c"foo")?;
//! ```

use std::borrow::Cow;
use std::ffi::CStr;
use std::os::raw::c_uint;
use std::ptr::NonNull;

use crate::builtins::JSType;
use crate::conversion::ConversionError;
use crate::exception;
use crate::gc::handle::Stack;
use crate::gc::scope::Scope;
use crate::prelude::{FromJSVal, ToJSVal};

use mozjs::gc::{Handle, HandleId, HandleObject, HandleValue, MutableHandle};
use mozjs::jsapi::{
    JSClass, JSFunctionSpec, JSObject, JSPropertySpec, ObjectOpResult, PropertyDescriptor, Value,
};
use mozjs::jsval::UndefinedValue;
use mozjs::rust::wrappers2;

use super::error::ExnThrown;

/// Marker type for JavaScript `Object` — the base type for all JS objects.
///
/// [`Object<'s>`](crate::Object) is the scope-rooted handle type:
///
/// ```ignore
/// let obj = Object::new_plain(&scope)?;
/// obj.set_property(&scope, c"x", val)?;
/// ```
///
/// All builtin types ([`Array<'s>`](crate::Array),
/// [`Promise<'s>`](crate::Promise), etc.) deref to `Object<'s>`, so all
/// property access methods are available on every builtin.
pub struct Object;

impl JSType for Object {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "Object";

    fn js_class() -> *const JSClass {
        crate::class::proto_key_to_class(mozjs::jsapi::JSProtoKey::JSProto_Object)
    }
}

impl<'s> Stack<'s, Object> {
    // ---------------------------------------------------------------------------
    // Object creation
    // ---------------------------------------------------------------------------

    /// Create a new object with the given class, or a plain object if `clasp` is
    /// `None`. The object is rooted in the scope's pool.
    pub fn new(scope: &'s Scope<'_>, clasp: Option<&'static JSClass>) -> Result<Self, ExnThrown> {
        let raw = scope.cx_mut();
        let obj = unsafe {
            match clasp {
                Some(c) => wrappers2::JS_NewObject(raw, c),
                None => wrappers2::JS_NewPlainObject(raw),
            }
        };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new plain object (`{}`), rooted in the scope's pool.
    pub fn new_plain(scope: &'s Scope<'_>) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::JS_NewPlainObject(scope.cx_mut()) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new object with a specific prototype.
    pub fn new_with_proto(
        scope: &'s Scope<'_>,
        clasp: &'static JSClass,
        proto: Stack<'s, Object>,
    ) -> Result<Self, ExnThrown> {
        let obj =
            unsafe { wrappers2::JS_NewObjectWithGivenProto(scope.cx_mut(), clasp, proto.handle()) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new object for use inside a constructor (`new.target`).
    pub fn new_for_constructor(
        scope: &'s Scope<'_>,
        clasp: &'static JSClass,
        args: &mozjs::jsapi::CallArgs,
    ) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::JS_NewObjectForConstructor(scope.cx_mut(), clasp, args) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    // ---------------------------------------------------------------------------
    // Pointer access
    // ---------------------------------------------------------------------------

    /// Get a raw `NonNull` pointer to the underlying `JSObject`.
    pub fn as_non_null(self) -> Option<NonNull<JSObject>> {
        NonNull::new(self.handle().get())
    }

    /// Create from a JS value. Returns an error if the value is not an object.
    pub fn from_value(
        scope: &'s Scope<'_>,
        val: impl ToJSVal<'s>,
    ) -> Result<Self, ConversionError> {
        Self::from_raw_value(scope, val.to_jsval_raw(scope)?)
    }

    /// Root the object `val` holds, or fail if it holds something else.
    ///
    /// `val` must be read from a rooted location, such as a handle, right before the call, with
    /// nothing that can GC in between, or hold no GC thing.
    pub fn from_raw_value(scope: &'s Scope<'_>, val: Value) -> Result<Self, ConversionError> {
        if val.is_object() {
            // SAFETY: the value holds an object, so the pointer is a non-null JSObject.
            Ok(unsafe { Self::from_raw(scope, val.to_object()) }.unwrap())
        } else {
            Err(ConversionError::Failure(Cow::Borrowed(
                c"Value isn't an object",
            )))
        }
    }

    /// Wrap an existing rooted handle.
    ///
    /// Useful for interop with code that already has rooted handles.
    ///
    /// Returns `None` if the handle is null.
    pub fn from_handle(handle: Handle<'s, *mut JSObject>) -> Option<Self> {
        // SAFETY: Handle is already rooted and valid.
        NonNull::new(handle.get()).map(|_| unsafe { Self::from_handle_unchecked(handle) })
    }

    // -----------------------------------------------------------------------
    // Property access
    // -----------------------------------------------------------------------

    /// Get a property by name, through the global's interned key for `name` (see
    /// [`get_or_init_property_id`](crate::class::get_or_init_property_id)). Panics if `name` is an
    /// array index.
    #[inline]
    pub fn get_property<'a>(
        &self,
        scope: &'a Scope<'_>,
        name: &'static CStr,
    ) -> Result<HandleValue<'a>, ExnThrown> {
        let id = crate::class::get_or_init_property_id(scope, name)?;
        self.get_property_by_id(scope, id)
    }

    /// Get a property by a name that isn't `'static`. The name is atomized on each call.
    #[inline]
    pub fn get_property_by_name<'a>(
        &self,
        scope: &'a Scope<'_>,
        name: &CStr,
    ) -> Result<HandleValue<'a>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe {
            wrappers2::JS_GetProperty(
                scope.cx_mut(),
                self.handle(),
                name.as_ptr(),
                rval.reborrow(),
            )
        };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Get a property by property key (jsid).
    #[inline]
    pub fn get_property_by_id<'a>(
        &self,
        scope: &'a Scope<'_>,
        id: HandleId,
    ) -> Result<HandleValue<'a>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe {
            wrappers2::JS_GetPropertyById(scope.cx_mut(), self.handle(), id, rval.reborrow())
        };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Get an array element by index.
    #[inline]
    pub fn get_element<'a>(
        &self,
        scope: &'a Scope<'_>,
        index: u32,
    ) -> Result<HandleValue<'a>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe {
            wrappers2::JS_GetElement(scope.cx_mut(), self.handle(), index, rval.reborrow())
        };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Set a property by name, through the global's interned key for `name` (see
    /// [`get_or_init_property_id`](crate::class::get_or_init_property_id)). Panics if `name` is an
    /// array index.
    #[inline]
    pub fn set_property<'v>(
        &self,
        scope: &'v Scope<'_>,
        name: &'static CStr,
        value: impl ToJSVal<'v>,
    ) -> Result<(), ExnThrown> {
        let id = crate::class::get_or_init_property_id(scope, name)?;
        self.set_property_by_id(scope, id, value)
    }

    /// Set a property by a name that isn't `'static`. The name is atomized on each call.
    #[inline]
    pub fn set_property_by_name<'v>(
        &self,
        scope: &'v Scope<'_>,
        name: &CStr,
        value: impl ToJSVal<'v>,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::JS_SetProperty(
                scope.cx_mut(),
                self.handle(),
                name.as_ptr(),
                value.to_jsval_throwing(scope)?,
            )
        };
        ExnThrown::check(ok)
    }

    /// Set a property by property key (jsid).
    #[inline]
    pub fn set_property_by_id<'v>(
        &self,
        scope: &'v Scope<'_>,
        id: HandleId,
        value: impl ToJSVal<'v>,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::JS_SetPropertyById(
                scope.cx_mut(),
                self.handle(),
                id,
                value.to_jsval_throwing(scope)?,
            )
        };
        ExnThrown::check(ok)
    }

    /// Set an array element by index.
    #[inline]
    pub fn set_element<'v>(
        &self,
        scope: &'v Scope<'_>,
        index: u32,
        value: impl ToJSVal<'v>,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::JS_SetElement(
                scope.cx_mut(),
                self.handle(),
                index,
                value.to_jsval_throwing(scope)?,
            )
        };
        ExnThrown::check(ok)
    }

    /// Check whether this object has a property with the given name, through the global's
    /// interned key for `name` (see [`get_or_init_property_id`](crate::class::get_or_init_property_id)).
    /// Panics if `name` is an array index.
    #[inline]
    pub fn has_property(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<bool, ExnThrown> {
        let id = crate::class::get_or_init_property_id(scope, name)?;
        self.has_property_by_id(scope, id)
    }

    /// Check whether this object has a property with a name that isn't `'static`. The name is
    /// atomized on each call.
    #[inline]
    pub fn has_property_by_name(&self, scope: &Scope<'_>, name: &CStr) -> Result<bool, ExnThrown> {
        let mut found = false;
        let ok = unsafe {
            wrappers2::JS_HasProperty(scope.cx_mut(), self.handle(), name.as_ptr(), &mut found)
        };
        ExnThrown::check(ok)?;
        Ok(found)
    }

    /// Check whether this object has a property with the given id.
    #[inline]
    pub fn has_property_by_id(&self, scope: &Scope<'_>, id: HandleId) -> Result<bool, ExnThrown> {
        let mut found = false;
        let ok =
            unsafe { wrappers2::JS_HasPropertyById(scope.cx_mut(), self.handle(), id, &mut found) };
        ExnThrown::check(ok)?;
        Ok(found)
    }

    /// Check whether this object has an element at the given index.
    #[inline]
    pub fn has_element(&self, scope: &Scope<'_>, index: u32) -> Result<bool, ExnThrown> {
        let mut found = false;
        let ok =
            unsafe { wrappers2::JS_HasElement(scope.cx_mut(), self.handle(), index, &mut found) };
        ExnThrown::check(ok)?;
        Ok(found)
    }

    /// Check whether this object has an own property with the given name, through the global's
    /// interned key for `name` (see [`get_or_init_property_id`](crate::class::get_or_init_property_id)).
    /// Panics if `name` is an array index.
    #[inline]
    pub fn has_own_property(
        &self,
        scope: &Scope<'_>,
        name: &'static CStr,
    ) -> Result<bool, ExnThrown> {
        let id = crate::class::get_or_init_property_id(scope, name)?;
        self.has_own_property_by_id(scope, id)
    }

    /// Check whether this object has an own property with a name that isn't `'static`. The name
    /// is atomized on each call.
    #[inline]
    pub fn has_own_property_by_name(
        &self,
        scope: &Scope<'_>,
        name: &CStr,
    ) -> Result<bool, ExnThrown> {
        let mut found = false;
        let ok = unsafe {
            wrappers2::JS_HasOwnProperty(scope.cx_mut(), self.handle(), name.as_ptr(), &mut found)
        };
        ExnThrown::check(ok)?;
        Ok(found)
    }

    /// Check whether this object has an own property with the given id.
    #[inline]
    pub fn has_own_property_by_id(
        &self,
        scope: &Scope<'_>,
        id: HandleId,
    ) -> Result<bool, ExnThrown> {
        let mut found = false;
        let ok = unsafe {
            wrappers2::JS_HasOwnPropertyById(scope.cx_mut(), self.handle(), id, &mut found)
        };
        ExnThrown::check(ok)?;
        Ok(found)
    }

    /// Delete a property by key.
    pub fn delete_property_by_id(&self, scope: &Scope<'_>, id: HandleId) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::JS_DeletePropertyById1(scope.cx_mut(), self.handle(), id) };
        ExnThrown::check(ok)
    }

    /// Delete a property by name, through the global's interned key for `name`. Panics if `name`
    /// is an array index.
    #[inline]
    pub fn delete_property(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<(), ExnThrown> {
        let id = crate::class::get_or_init_property_id(scope, name)?;
        self.delete_property_by_id(scope, id)
    }

    /// Delete a property by a name that isn't `'static`. The name is atomized on each call.
    #[inline]
    pub fn delete_property_by_name(&self, scope: &Scope<'_>, name: &CStr) -> Result<(), ExnThrown> {
        let ok =
            unsafe { wrappers2::JS_DeleteProperty1(scope.cx_mut(), self.handle(), name.as_ptr()) };
        ExnThrown::check(ok)
    }

    /// Delete a property by name, returning the operation result.
    #[inline]
    pub fn delete_property_with_result(
        &self,
        scope: &Scope<'_>,
        name: &CStr,
        result: &mut ObjectOpResult,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::JS_DeleteProperty(scope.cx_mut(), self.handle(), name.as_ptr(), result)
        };
        ExnThrown::check(ok)
    }

    /// Delete an element by index.
    #[inline]
    pub fn delete_element(&self, scope: &Scope<'_>, index: u32) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::JS_DeleteElement1(scope.cx_mut(), self.handle(), index) };
        ExnThrown::check(ok)
    }

    // -----------------------------------------------------------------------
    // Property definition
    // -----------------------------------------------------------------------

    /// Define a property with a JS value and attribute flags, through the global's interned key
    /// for `name`. Panics if `name` is an array index.
    #[inline]
    pub fn define_property<'v>(
        &self,
        scope: &'v Scope<'_>,
        name: &'static CStr,
        value: impl ToJSVal<'v>,
        attrs: c_uint,
    ) -> Result<(), ExnThrown> {
        let id = crate::class::get_or_init_property_id(scope, name)?;
        self.define_value_by_id(scope, id, value, attrs)
    }

    /// Like [`define_property`](Self::define_property), for a name that isn't
    /// `'static`. The name is atomized on each call.
    #[inline]
    pub fn define_property_by_name<'v>(
        &self,
        scope: &'v Scope<'_>,
        name: &CStr,
        value: impl ToJSVal<'v>,
        attrs: c_uint,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::JS_DefineProperty(
                scope.cx_mut(),
                self.handle(),
                name.as_ptr(),
                value.to_jsval_throwing(scope)?,
                attrs,
            )
        };
        ExnThrown::check(ok)
    }

    /// Define a property by key with a JS value and attribute flags.
    #[inline]
    pub fn define_value_by_id<'v>(
        &self,
        scope: &'v Scope<'_>,
        id: HandleId,
        value: impl ToJSVal<'v>,
        attrs: c_uint,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::JS_DefinePropertyById2(
                scope.cx_mut(),
                self.handle(),
                id,
                value.to_jsval_throwing(scope)?,
                attrs,
            )
        };
        ExnThrown::check(ok)
    }

    /// Define an element by index with attribute flags.
    ///
    /// Installs an own data property without consulting the prototype chain or invoking setters.
    /// Use this to build objects whose indexed slots must be immune to `Object.prototype`
    /// numeric accessors.
    /// Use [`set_element`](Self::set_element) for normal element assignment, which would invoke
    /// (inherited) setters for the index.
    #[inline]
    pub fn define_element<'v>(
        &self,
        scope: &'v Scope<'_>,
        index: u32,
        value: impl ToJSVal<'v>,
        attrs: c_uint,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::JS_DefineElement(
                scope.cx_mut(),
                self.handle(),
                index,
                value.to_jsval_throwing(scope)?,
                attrs,
            )
        };
        ExnThrown::check(ok)
    }

    /// Define a property using a property descriptor.
    #[inline]
    pub fn define_property_by_id(
        &self,
        scope: &Scope<'_>,
        id: HandleId,
        desc: Handle<'_, PropertyDescriptor>,
    ) -> Result<(), ExnThrown> {
        let ok =
            unsafe { wrappers2::JS_DefinePropertyById1(scope.cx_mut(), self.handle(), id, desc) };
        ExnThrown::check(ok)
    }

    /// Define multiple properties from a static property spec array.
    ///
    /// # Safety
    ///
    /// `ps` must point to a null-terminated array of [`JSPropertySpec`].
    #[inline]
    pub unsafe fn define_properties(
        &self,
        scope: &Scope<'_>,
        ps: *const JSPropertySpec,
    ) -> Result<(), ExnThrown> {
        let ok = wrappers2::JS_DefineProperties(scope.cx_mut(), self.handle(), ps);
        ExnThrown::check(ok)
    }

    /// Define multiple functions from a static function spec array.
    ///
    /// # Safety
    ///
    /// `fs` must point to a null-terminated array of [`JSFunctionSpec`].
    #[inline]
    pub unsafe fn define_functions(
        &self,
        scope: &Scope<'_>,
        fs: *const JSFunctionSpec,
    ) -> Result<(), ExnThrown> {
        let ok = wrappers2::JS_DefineFunctions(scope.cx_mut(), self.handle(), fs);
        ExnThrown::check(ok)
    }

    // -----------------------------------------------------------------------
    // Prototype chain
    // -----------------------------------------------------------------------

    /// Get the prototype of this object.
    ///
    /// Returns `Ok(None)` for a null `[[Prototype]]` (e.g.
    /// `Object.create(null)` or `Object.prototype`).
    #[inline]
    pub fn get_prototype(
        &self,
        scope: &'s Scope<'_>,
    ) -> Result<Option<Stack<'s, Object>>, ExnThrown> {
        let mut result = scope.root_object_mut(std::ptr::null_mut());
        let ok =
            unsafe { wrappers2::JS_GetPrototype(scope.cx_mut(), self.handle(), result.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(Self::from_handle(result.handle()))
    }

    /// Set the prototype of this object.
    #[inline]
    pub fn set_prototype(&self, scope: &Scope<'_>, proto: HandleObject) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::JS_SetPrototype(scope.cx_mut(), self.handle(), proto) };
        ExnThrown::check(ok)
    }

    /// Attempt to make this object's prototype immutable.
    ///
    /// The `Ok(bool)` result indicates whether the attempt was successful.
    ///
    /// This is a nonstandard internal method mainly used to ensure the integrity of the global
    /// object's prototype chain.
    pub fn make_proto_immutable(&self, scope: &Scope<'_>) -> Result<bool, ExnThrown> {
        let mut success = false;
        let ok = unsafe {
            wrappers2::JS_SetImmutablePrototype(scope.cx_mut(), self.handle(), &mut success)
        };
        if ok {
            Ok(success)
        } else {
            debug_assert!(
                exception::is_pending(scope),
                "We don't handle uncatchable exceptions in this method"
            );
            Err(ExnThrown)
        }
    }

    /// Whether this object is callable, which is true for functions, bound
    /// functions, and callable proxies.
    ///
    /// This is the WebIDL `IsCallable` test: it accepts any callable, unlike
    /// the [`Function`](crate::function::Function) brand check, which is
    /// restricted to objects with a backing `JSFunction`. Use this to validate
    /// values for callback function types, then invoke them by value with
    /// [`Function::call`](crate::function::Function::call).
    #[inline]
    pub fn is_callable(&self) -> bool {
        // SAFETY: `self` is a live, rooted object.
        unsafe { mozjs::jsapi::JS::IsCallable(self.handle().get()) }
    }

    /// Check whether this object is extensible.
    #[inline]
    pub fn is_extensible(&self, scope: &Scope<'_>) -> Result<bool, ExnThrown> {
        let mut extensible = false;
        let ok =
            unsafe { wrappers2::JS_IsExtensible(scope.cx_mut(), self.handle(), &mut extensible) };
        ExnThrown::check(ok)?;
        Ok(extensible)
    }

    /// Prevent extensions on this object.
    #[inline]
    pub fn prevent_extensions(
        &self,
        scope: &Scope<'_>,
        result: &mut ObjectOpResult,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::JS_PreventExtensions(scope.cx_mut(), self.handle(), result) };
        ExnThrown::check(ok)
    }

    /// Freeze this object (make all properties non-configurable and
    /// non-writable).
    #[inline]
    pub fn freeze(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::JS_FreezeObject(scope.cx_mut(), self.handle()) };
        ExnThrown::check(ok)
    }

    /// Deep-freeze this object and all objects reachable from it.
    #[inline]
    pub fn deep_freeze(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::JS_DeepFreezeObject(scope.cx_mut(), self.handle()) };
        ExnThrown::check(ok)
    }

    /// Define an object property that is itself a new object.
    pub fn define_object(
        scope: &'s Scope<'_>,
        obj: Stack<'_, Object>,
        name: &CStr,
        clasp: &'static JSClass,
        attrs: c_uint,
    ) -> Result<Self, ExnThrown> {
        let child = unsafe {
            wrappers2::JS_DefineObject(scope.cx_mut(), obj.handle(), name.as_ptr(), clasp, attrs)
        };
        unsafe { Self::from_raw(scope, child).ok_or(ExnThrown) }
    }

    /// Get a constructor from a prototype object.
    pub fn get_constructor(
        scope: &'s Scope<'_>,
        proto: Stack<'_, Object>,
    ) -> Result<Self, ExnThrown> {
        let ctor = unsafe { wrappers2::JS_GetConstructor(scope.cx_mut(), proto.handle()) };
        unsafe { Self::from_raw(scope, ctor).ok_or(ExnThrown) }
    }

    /// Get a property descriptor by id.
    ///
    /// `is_none` is set to `true` if the property was not found.
    // TODO: this should return a descriptor instead of taking a mutable handle outparam.
    pub fn get_own_property_descriptor(
        &self,
        scope: &Scope<'_>,
        id: HandleId,
        desc: MutableHandle<PropertyDescriptor>,
        is_none: &mut bool,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::JS_GetOwnPropertyDescriptorById(
                scope.cx_mut(),
                self.handle(),
                id,
                desc,
                is_none,
            )
        };
        ExnThrown::check(ok)
    }

    /// Check whether the object already has an own property with the given name.
    pub fn already_has_own_property(
        &self,
        scope: &Scope<'_>,
        name: &CStr,
    ) -> Result<bool, ExnThrown> {
        let mut found = false;
        let ok = unsafe {
            wrappers2::JS_AlreadyHasOwnProperty(
                scope.cx_mut(),
                self.handle(),
                name.as_ptr(),
                &mut found,
            )
        };
        ExnThrown::check(ok)?;
        Ok(found)
    }

    /// Convert a value to an object.
    pub fn from_value_coerce(scope: &'s Scope<'_>, val: HandleValue) -> Result<Self, ExnThrown> {
        if val.is_null_or_undefined() {
            return Err(crate::error::throw_type_error(
                scope,
                c"can't convert null or undefined to object",
            ));
        }

        let mut objp = scope.root_object_mut(std::ptr::null_mut());
        let ok = unsafe { wrappers2::JS_ValueToObject(scope.cx_mut(), val, objp.reborrow()) };
        ExnThrown::check(ok)?;
        Self::from_handle(objp.handle()).ok_or(ExnThrown)
    }

    /// Assign all enumerable own properties from `src` to this object.
    pub fn assign(&self, scope: &Scope<'_>, src: Stack<'_, Object>) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::JS_AssignObject(scope.cx_mut(), self.handle(), src.handle()) };
        ExnThrown::check(ok)
    }

    /// Wrap this object for use in the current compartment.
    pub fn wrap_object(&self, scope: &'s Scope<'_>) -> Result<crate::Object<'s>, ExnThrown> {
        let mut wrapped = scope.root_object_mut(self.handle().get());
        let ok = unsafe { wrappers2::JS_WrapObject(scope.cx_mut(), wrapped.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(crate::Object::from_handle(wrapped.handle()).unwrap())
    }

    /// Transplant this object to a new target.
    pub fn transplant(
        &self,
        scope: &'s Scope<'_>,
        target: Stack<'_, Object>,
    ) -> Result<Self, ExnThrown> {
        let result = unsafe {
            wrappers2::JS_TransplantObject(scope.cx_mut(), self.handle(), target.handle())
        };
        unsafe { Self::from_raw(scope, result).ok_or(ExnThrown) }
    }
}

impl<'s, 'v> FromJSVal<'s, 'v> for Stack<'s, Object> {
    type Config = ();

    fn from_jsval(
        scope: &'s Scope<'_>,
        val: HandleValue<'v>,
        _option: Self::Config,
    ) -> Result<Self, ConversionError> {
        Self::from_raw_value(scope, val.get())
    }
}

// ---------------------------------------------------------------------------
// Reserved slots
// ---------------------------------------------------------------------------

/// Get the `JSClass` pointer for a JS object.
///
/// # Safety
///
/// - `obj` must be a valid, non-null JS object pointer.
#[inline]
pub unsafe fn get_object_class(obj: *mut JSObject) -> *const JSClass {
    mozjs::rust::get_object_class(obj)
}

/// Return the number of reserved slots for a JS object's class.
///
/// # Safety
///
/// - `obj` must be a valid, non-null JS object pointer.
pub unsafe fn reserved_slot_count(obj: *mut JSObject) -> u32 {
    let clasp = get_object_class(obj);
    let flags = (*clasp).flags;
    (flags >> crate::class_spec::JSCLASS_RESERVED_SLOTS_SHIFT)
        & crate::class_spec::JSCLASS_RESERVED_SLOTS_MASK
}

/// Get the value stored in a reserved slot of a JS object.
///
/// # Safety
///
/// - `obj` must be a valid, non-null JS object pointer.
/// - `slot` must be less than the number of reserved slots for the object's class.
pub unsafe fn get_reserved_slot(obj: *mut JSObject, slot: u32) -> Value {
    let mut val = UndefinedValue();
    mozjs::glue::JS_GetReservedSlot(obj, slot, &mut val);
    val
}

/// Get the value stored in a reserved slot of a native (non-proxy) JS object.
///
/// Use this instead of [`get_reserved_slot`] for objects known to be native: it is inlined.
///
/// # Safety
///
/// - `obj` must be a valid, non-null pointer to a native JS object.
/// - `slot` must be less than the number of reserved slots for the object's class, and less than
///   16.
#[inline]
pub unsafe fn get_native_reserved_slot(obj: *mut JSObject, slot: u32) -> Value {
    use mozjs::jsapi::JS::shadow;
    debug_assert!(slot < reserved_slot_count(obj));
    debug_assert!((slot as usize) < shadow::Object_MAX_FIXED_SLOTS);
    // Native shape kinds have the low kind bit set.
    debug_assert!(
        (*(*(obj as *const shadow::Object)).shape).immutableFlags >> shadow::Shape_KIND_SHIFT & 1
            == 1
    );
    // The first 16 reserved slots of a native object are fixed slots, which directly follow the
    // object header.
    let fixed_slots = (obj as *const u8).add(size_of::<shadow::NativeObject>()) as *const Value;
    *fixed_slots.add(slot as usize)
}

/// Set a value in a reserved slot of a JS object.
///
/// # Safety
///
/// - `obj` must be a valid, non-null JS object pointer.
/// - `slot` must be less than the number of reserved slots for the object's class.
pub unsafe fn set_reserved_slot(obj: *mut JSObject, slot: u32, val: &Value) {
    mozjs::jsapi::JS_SetReservedSlot(obj, slot, val);
}

// Object<'s> is the base type; no further Deref needed.
// Other builtins (Array, Promise, etc.) Deref to Object<'s>.
