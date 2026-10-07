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

pub use mozjs::jsapi::ESClass;
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

    /// Create a new plain object whose prototype is `null`, as `Object.create(null)` does.
    pub fn new_with_null_prototype(scope: &'s Scope<'_>) -> Result<Self, ExnThrown> {
        // A null class makes a plain object.
        let obj = unsafe {
            wrappers2::JS_NewObjectWithGivenProto(
                scope.cx_mut(),
                std::ptr::null(),
                mozjs::rust::HandleObject::null(),
            )
        };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new plain object whose prototype is `proto`, as `Object.create(proto)` does.
    pub fn new_plain_with_proto(
        scope: &'s Scope<'_>,
        proto: mozjs::rust::HandleObject,
    ) -> Result<Self, ExnThrown> {
        // A null class makes a plain object.
        let obj = unsafe {
            wrappers2::JS_NewObjectWithGivenProto(scope.cx_mut(), std::ptr::null(), proto)
        };
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

    /// Create a new object of class `clasp` whose prototype is `proto`, with fixed slots for the
    /// class's reserved slots plus `property_count` properties, up to SpiderMonkey's maximum number
    /// of fixed slots. A `clasp` of `None` creates a plain object, and a `proto` of `None` an
    /// object with a null prototype. [`add_new_data_properties`](Self::add_new_data_properties)
    /// adds the properties.
    ///
    /// `clasp` must be a native class that isn't a function, array or global class and whose
    /// instances don't store data inline.
    pub fn new_with_property_capacity(
        scope: &'s Scope<'_>,
        clasp: Option<&'static JSClass>,
        proto: Option<Stack<'_, Object>>,
        property_count: usize,
    ) -> Result<Self, ExnThrown> {
        let proto = proto.map_or(HandleObject::null(), |proto| proto.handle());
        let clasp = clasp.map_or(std::ptr::null(), std::ptr::from_ref);
        // SAFETY: `proto` is rooted or null, and `clasp` is null or a static class.
        let obj = unsafe {
            wrappers2::NewObjectWithPropertyCapacity(scope.cx_mut(), clasp, proto, property_count)
        };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new plain object whose prototype is `Object.prototype`, with an enumerable,
    /// writable and configurable data property for each of `keys`, in order, with the value at the
    /// same index of `values`. SpiderMonkey reuses the shape of a recent object with the same
    /// keys.
    ///
    /// # Safety
    ///
    /// The keys must be distinct names or symbols, not array indices, that stay alive during the
    /// call, as the keys that [`crate::class::get_or_init_property_id`] interns and cached symbols
    /// do. `keys` and `values` have the same length.
    pub unsafe fn new_plain_with_unique_names(
        scope: &'s Scope<'_>,
        keys: &[mozjs::jsapi::PropertyKey],
        values: &[impl ToJSVal<'s>],
    ) -> Result<Self, ExnThrown> {
        assert_eq!(keys.len(), values.len());
        let mut rooter = PropertyValueRooter::with_capacity(values.len());
        let values = rooter.root(scope, values)?;
        // SAFETY: the caller keeps the keys alive. Atoms and symbols are never moved by the GC.
        let obj = unsafe {
            wrappers2::NewPlainObjectWithUniqueNames(
                scope.cx_mut(),
                keys.as_ptr(),
                &values.handles(),
            )
        };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new plain object whose prototype is `Object.prototype`, with an enumerable,
    /// writable and configurable data property for each key of `template`, in order, with the
    /// value at the same index of `values`, as
    /// [`new_plain_with_unique_names`](Self::new_plain_with_unique_names) creates it. `values` has
    /// a value for every key. Debug builds check that the global registered the template.
    pub fn new_plain_with_template(
        scope: &'s Scope<'_>,
        template: &'static PropertyTemplate,
        values: &[impl ToJSVal<'s>],
    ) -> Result<Self, ExnThrown> {
        assert_eq!(template.len(), values.len());
        let (cells, registered) = crate::class::get_or_init_template_keys(scope, template)?;
        debug_assert!(
            registered,
            "property templates are registered at global setup"
        );
        // SAFETY: the cells are traced, and live as long as the global.
        let keys: smallvec::SmallVec<[mozjs::jsapi::jsid; INLINE_PROPERTIES]> =
            unsafe { (*cells).iter().map(|cell| **cell).collect() };
        // SAFETY: a template's keys are distinct names, which aren't array indices, and symbols,
        // which the global keeps alive.
        unsafe { Self::new_plain_with_unique_names(scope, &keys, values) }
    }

    /// Add an enumerable, writable and configurable data property for each of `keys` to this
    /// object, in order, with the value at the same index of `values`. No class hooks run and the
    /// prototype chain is not consulted.
    ///
    /// # Safety
    ///
    /// The object is a native, extensible object whose class has no `addProperty` or `resolve`
    /// hook and no custom property operations, such as an object
    /// [`new_with_property_capacity`](Self::new_with_property_capacity) created, and has none of
    /// the keys. The keys are distinct and stay alive during the call, as the keys that
    /// [`crate::class::get_or_init_property_id`] interns and cached symbols do. `keys` and
    /// `values` have the same length. Debug builds of SpiderMonkey assert the conditions on the
    /// object and the keys.
    pub unsafe fn add_new_data_properties<'v>(
        &self,
        scope: &'v Scope<'_>,
        keys: &[mozjs::jsapi::PropertyKey],
        values: &[impl ToJSVal<'v>],
    ) -> Result<(), ExnThrown> {
        assert_eq!(keys.len(), values.len());
        let mut rooter = PropertyValueRooter::with_capacity(values.len());
        let values = rooter.root(scope, values)?;
        // SAFETY: the object is rooted, and the caller keeps the keys alive. Atoms and symbols
        // are never moved by the GC.
        let ok = unsafe {
            wrappers2::AddDataPropertiesToNewObject(
                scope.cx_mut(),
                self.handle(),
                keys.as_ptr(),
                &values.handles(),
            )
        };
        ExnThrown::check(ok)
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

    /// Create a new plain object for a constructor call, as
    /// [`new_with_capacity_for_constructor`](Self::new_with_capacity_for_constructor) does with a
    /// `clasp` of `None`.
    pub fn new_plain_for_constructor(
        scope: &'s Scope<'_>,
        args: &mozjs::jsapi::CallArgs,
        property_count: usize,
    ) -> Result<Self, ExnThrown> {
        Self::new_with_capacity_for_constructor(scope, None, args, property_count)
    }

    /// Create a new object of class `clasp`, or a plain object for `None`, for a constructor
    /// call, whose prototype is `new.target`'s `prototype` property, or else the
    /// `Object.prototype` of `new.target`'s realm, as `OrdinaryCreateFromConstructor` creates it.
    /// A `property_count` other than 0 gives it fixed slots for that many properties, as
    /// [`new_with_property_capacity`](Self::new_with_property_capacity) does, whose requirements
    /// on `clasp` apply.
    pub fn new_with_capacity_for_constructor(
        scope: &'s Scope<'_>,
        clasp: Option<&'static JSClass>,
        args: &mozjs::jsapi::CallArgs,
        property_count: usize,
    ) -> Result<Self, ExnThrown> {
        let create = |proto: Stack<'_, Object>| {
            if clasp.is_none() && property_count == 0 {
                Self::new_plain_with_proto(scope, proto.handle())
            } else {
                Self::new_with_property_capacity(scope, clasp, Some(proto), property_count)
            }
        };
        let new_target =
            Self::from_value(scope, args.new_target().get()).map_err(|e| e.throw(scope))?;
        let proto = new_target.get_property(scope, c"prototype")?;
        if let Ok(proto) = Self::from_value(scope, proto) {
            return create(proto);
        }
        // SAFETY: `new_target` is a rooted object.
        let realm = unsafe { wrappers2::GetFunctionRealm(scope.cx_mut(), new_target.handle()) };
        if realm.is_null() {
            return Err(ExnThrown);
        }
        // SAFETY: `GetFunctionRealm` returned a live realm, whose global is alive. The prototype is
        // rooted before anything can GC.
        let proto = unsafe {
            let global = mozjs::jsapi::JS::GetRealmGlobalOrNull(realm);
            let previous = mozjs::jsapi::JS::EnterRealm(scope.raw_cx_no_gc(), global);
            let proto = wrappers2::GetRealmObjectPrototype(scope.cx());
            mozjs::jsapi::JS::LeaveRealm(scope.raw_cx_no_gc(), previous);
            Self::from_mozjs_rval(scope, proto)?
        };
        let mut wrapped = scope.root_object_mut(proto.handle().get());
        // SAFETY: `wrapped` is a rooted location.
        ExnThrown::check(unsafe { wrappers2::JS_WrapObject(scope.cx_mut(), wrapped.reborrow()) })?;
        // SAFETY: `wrapped` holds a live object.
        let proto = unsafe { Self::from_raw(scope, wrapped.handle().get()) }.ok_or(ExnThrown)?;
        create(proto)
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
        let val = val.to_jsval_raw(scope)?;
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
    /// [`get_or_init_property_id`](crate::class::get_or_init_property_id)).
    #[inline]
    pub fn get_property<'a>(
        &self,
        scope: &'a Scope<'_>,
        name: &'static CStr,
    ) -> Result<HandleValue<'a>, ExnThrown> {
        let id = crate::class::get_or_init_property_id(scope, name)?;
        self.get_property_by_id(scope, id)
    }

    /// The result of JS's `ToPrimitive(this, number)`: calls `Symbol.toPrimitive` with `"number"`,
    /// or else `valueOf` and then `toString`.
    pub fn to_primitive_number<'a>(
        &self,
        scope: &'a Scope<'_>,
    ) -> Result<HandleValue<'a>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe {
            wrappers2::ToPrimitive(
                scope.cx_mut(),
                self.handle(),
                mozjs::jsapi::JSType::JSTYPE_NUMBER,
                rval.reborrow(),
            )
        };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
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
    /// [`get_or_init_property_id`](crate::class::get_or_init_property_id)).
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

    /// Define the accessor property `name` with `getter` and `setter`, either of which can be
    /// absent, and the property attributes `attrs`, through the global's interned key for `name`.
    pub fn define_accessor(
        &self,
        scope: &Scope<'_>,
        name: &'static CStr,
        getter: Option<Stack<'_, Object>>,
        setter: Option<Stack<'_, Object>>,
        attrs: c_uint,
    ) -> Result<(), ExnThrown> {
        let id = crate::class::get_or_init_property_id(scope, name)?;
        self.define_accessor_by_id(scope, id, getter, setter, attrs)
    }

    /// Define the accessor property with key `id`, as [`define_accessor`](Self::define_accessor)
    /// defines one by name.
    pub fn define_accessor_by_id(
        &self,
        scope: &Scope<'_>,
        id: HandleId,
        getter: Option<Stack<'_, Object>>,
        setter: Option<Stack<'_, Object>>,
        attrs: c_uint,
    ) -> Result<(), ExnThrown> {
        let getter = getter.map_or(HandleObject::null(), |getter| getter.handle());
        let setter = setter.map_or(HandleObject::null(), |setter| setter.handle());
        let ok = unsafe {
            wrappers2::JS_DefinePropertyById4(
                scope.cx_mut(),
                self.handle(),
                id,
                getter,
                setter,
                attrs,
            )
        };
        ExnThrown::check(ok)
    }

    /// Define the accessor property `name`, a name that isn't `'static`, as
    /// [`define_accessor`](Self::define_accessor) defines one. The name is atomized on each call.
    pub fn define_accessor_by_name(
        &self,
        scope: &Scope<'_>,
        name: &CStr,
        getter: Option<Stack<'_, Object>>,
        setter: Option<Stack<'_, Object>>,
        attrs: c_uint,
    ) -> Result<(), ExnThrown> {
        let getter = getter.map_or(HandleObject::null(), |getter| getter.handle());
        let setter = setter.map_or(HandleObject::null(), |setter| setter.handle());
        let ok = unsafe {
            wrappers2::JS_DefineProperty2(
                scope.cx_mut(),
                self.handle(),
                name.as_ptr(),
                getter,
                setter,
                attrs,
            )
        };
        ExnThrown::check(ok)
    }

    /// Delete a property by key.
    pub fn delete_property_by_id(&self, scope: &Scope<'_>, id: HandleId) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::JS_DeletePropertyById1(scope.cx_mut(), self.handle(), id) };
        ExnThrown::check(ok)
    }

    /// The object's own enumerable string keys, as `Object.keys` returns them.
    pub fn own_enumerable_keys<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Result<Vec<HandleValue<'r>>, ExnThrown> {
        // SAFETY: as in `own_keys`.
        unsafe {
            let mut ids = mozjs::rust::IdVector::new(scope.cx_mut());
            ExnThrown::check(wrappers2::GetPropertyKeys(
                scope.cx_mut(),
                self.handle(),
                mozjs::jsapi::JSITER_OWNONLY,
                ids.handle_mut(),
            ))?;
            (0..ids.len())
                .map(|i| crate::id::id_to_value(scope, ids[i]))
                .collect()
        }
    }

    /// This object's own property keys, strings and symbols, in `Reflect.ownKeys` order.
    pub fn own_keys<'r>(&self, scope: &'r Scope<'_>) -> Result<Vec<HandleValue<'r>>, ExnThrown> {
        // SAFETY: the vector is rooted for its lifetime, and each id is converted to a rooted value
        // before the next call that can GC.
        unsafe {
            let mut ids = mozjs::rust::IdVector::new(scope.cx_mut());
            let flags = mozjs::jsapi::JSITER_OWNONLY
                | mozjs::jsapi::JSITER_HIDDEN
                | mozjs::jsapi::JSITER_SYMBOLS;
            ExnThrown::check(wrappers2::GetPropertyKeys(
                scope.cx_mut(),
                self.handle(),
                flags,
                ids.handle_mut(),
            ))?;
            (0..ids.len())
                .map(|i| crate::id::id_to_value(scope, ids[i]))
                .collect()
        }
    }

    /// [`own_keys`](Self::own_keys) without the array indices, the canonical numeric strings
    /// below `2**32 - 1`. The indices are skipped without creating their strings.
    pub fn own_non_index_keys<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Result<Vec<HandleValue<'r>>, ExnThrown> {
        // SAFETY: as in `own_keys`. The string of a string id is an atom, which is linear.
        unsafe {
            let mut ids = mozjs::rust::IdVector::new(scope.cx_mut());
            let flags = mozjs::jsapi::JSITER_OWNONLY
                | mozjs::jsapi::JSITER_HIDDEN
                | mozjs::jsapi::JSITER_SYMBOLS;
            ExnThrown::check(wrappers2::GetPropertyKeys(
                scope.cx_mut(),
                self.handle(),
                flags,
                ids.handle_mut(),
            ))?;
            let mut keys = Vec::new();
            for i in 0..ids.len() {
                let id = ids[i];
                if id.is_int() {
                    continue;
                }
                if id.is_string() {
                    let mut index = 0;
                    let linear = id.to_string().cast::<mozjs::jsapi::JSLinearString>();
                    if mozjs::jsapi::js::StringIsArrayIndex(linear, &mut index) {
                        continue;
                    }
                }
                keys.push(crate::id::id_to_value(scope, id)?);
            }
            Ok(keys)
        }
    }

    /// Delete a property by name, through the global's interned key for `name`.
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
    /// for `name`.
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

    /// Define a property with a name that isn't `'static`, as
    /// [`define_property`](Self::define_property) defines one. The name is atomized on each call.
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

    /// Start creating the properties of `template` on this object, as `init` creates them. The
    /// template's keys are looked up once, here. Debug builds check that the global registered
    /// the template.
    ///
    /// # Safety
    ///
    /// With [`PropertyInit::Define`], the object is native and extensible, and its class has no
    /// `addProperty` or `resolve` hook and no custom property operations, as for plain objects
    /// and instances of `#[jsclass]` classes. It has none of the keys the writer creates, and gets
    /// none of them in another way before [`PropertyWriter::flush`] or
    /// [`PropertyWriter::finish`]. So no JavaScript that can reach the object runs before then,
    /// apart from what [`PropertyWriter::get`] runs. With [`PropertyInit::Assign`], there are no
    /// requirements.
    pub unsafe fn property_writer<'w>(
        self,
        scope: &'w Scope<'_>,
        template: &'static PropertyTemplate,
        init: PropertyInit,
    ) -> Result<PropertyWriter<'w>, ExnThrown>
    where
        's: 'w,
    {
        let (keys, registered) = crate::class::get_or_init_template_keys(scope, template)?;
        debug_assert!(
            registered,
            "property templates are registered at global setup"
        );
        Ok(PropertyWriter {
            object: self,
            template,
            keys,
            next: 0,
            mode: match init {
                PropertyInit::Define => WriteMode::Collect,
                PropertyInit::Assign => WriteMode::Assign,
            },
            collected_keys: smallvec::SmallVec::new(),
            collected_values: smallvec::SmallVec::new(),
        })
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

    /// This object's ECMAScript class, such as `ESClass::Date` for a `Date`, as `JS::GetBuiltinClass`
    /// determines it. A wrapper that can be unwrapped has the class of its target.
    pub fn builtin_class(&self, scope: &Scope<'_>) -> Result<ESClass, ExnThrown> {
        let mut class = ESClass::Other;
        // SAFETY: `self` is rooted, and `GetBuiltinClass` only writes `class`.
        let ok = unsafe { wrappers2::GetBuiltinClass(scope.cx_mut(), self.handle(), &mut class) };
        ExnThrown::check(ok)?;
        Ok(class)
    }

    /// The primitive value of a `Number`, `String`, `Boolean`, `BigInt` or `Symbol` wrapper
    /// object, as `js::Unbox` reads it. Returns `undefined` for other objects, proxies included.
    pub fn unbox<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        if self.is_proxy() {
            return Ok(rval.handle());
        }
        // SAFETY: the handles are rooted.
        let ok = unsafe {
            mozjs::jsapi::js::Unbox(
                scope.raw_cx_no_gc(),
                self.handle().into(),
                rval.reborrow().into(),
            )
        };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Whether this object is an `arguments` object.
    pub fn is_arguments(&self) -> bool {
        // SAFETY: `self` is rooted.
        unsafe { wrappers2::IsArgumentsObject(self.handle()) }
    }

    /// `value instanceof this`: the result of this object's `Symbol.hasInstance` method if it has
    /// one, and `OrdinaryHasInstance` otherwise. Throws if this object isn't callable and has no
    /// `Symbol.hasInstance` method.
    pub fn instance_of(&self, scope: &Scope<'_>, value: HandleValue) -> Result<bool, ExnThrown> {
        let mut result = false;
        // SAFETY: the handles are rooted.
        let ok =
            unsafe { wrappers2::JS_HasInstance(scope.cx_mut(), self.handle(), value, &mut result) };
        ExnThrown::check(ok)?;
        Ok(result)
    }

    /// `OrdinaryHasInstance(this, value)`: whether `value` has this constructor's `prototype` in
    /// its prototype chain, without consulting `Symbol.hasInstance`. Throws if this object isn't
    /// callable or its `prototype` isn't an object.
    pub fn ordinary_has_instance(
        &self,
        scope: &Scope<'_>,
        value: HandleValue,
    ) -> Result<bool, ExnThrown> {
        let mut result = false;
        // SAFETY: the handles are rooted.
        let ok = unsafe {
            wrappers2::OrdinaryHasInstance(scope.cx_mut(), self.handle(), value, &mut result)
        };
        ExnThrown::check(ok)?;
        Ok(result)
    }

    /// Whether this object is a `Proxy`. Cross-compartment wrappers, which are proxies to the
    /// engine, aren't.
    pub fn is_proxy(&self) -> bool {
        // SAFETY: `self` is a live, rooted object.
        unsafe {
            let class = get_object_class(self.handle().get());
            (*class).flags & mozjs::jsapi::JSCLASS_IS_PROXY != 0
                && !mozjs::glue::IsWrapper(self.handle().get())
        }
    }

    /// The target of this `Proxy`, or `None` for a revoked proxy. Must only be called for an
    /// object [`is_proxy`](Self::is_proxy) is `true` for.
    pub fn proxy_target<'r>(&self, scope: &'r Scope<'_>) -> Option<Stack<'r, Object>> {
        debug_assert!(self.is_proxy());
        let mut target = UndefinedValue();
        // SAFETY: `self` is a proxy, whose private slot holds its target or null. The target is
        // rooted before anything can GC.
        unsafe {
            mozjs::glue::GetProxyPrivate(self.handle().get(), &mut target);
            if target.is_object() {
                Stack::<Object>::from_raw(scope, target.to_object())
            } else {
                None
            }
        }
    }

    /// The handler of this scripted `Proxy`, as `new Proxy` and `Proxy.revocable` create them, or
    /// `None` for a revoked proxy. Must only be called for an object
    /// [`is_proxy`](Self::is_proxy) is `true` for.
    pub fn proxy_handler<'r>(&self, scope: &'r Scope<'_>) -> Option<Stack<'r, Object>> {
        debug_assert!(self.is_proxy());
        let mut handler = UndefinedValue();
        // SAFETY: `self` is a proxy. A scripted proxy holds its handler or null in reserved slot
        // 0. The handler is rooted before anything can GC.
        unsafe {
            mozjs::glue::GetProxyReservedSlot(self.handle().get(), 0, &mut handler);
            if handler.is_object() {
                Stack::<Object>::from_raw(scope, handler.to_object())
            } else {
                None
            }
        }
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

    /// The own property of this object with key `id`, as `Object.getOwnPropertyDescriptor`
    /// returns it, or `None` if there is none.
    pub fn own_property_descriptor<'r>(
        &self,
        scope: &'r Scope<'_>,
        id: HandleId,
    ) -> Result<Option<OwnProperty<'r>>, ExnThrown> {
        mozjs::rooted!(in(unsafe { scope.raw_cx_no_gc() }) let mut desc = PropertyDescriptor::default());
        let mut is_none = true;
        self.get_own_property_descriptor(scope, id, desc.handle_mut(), &mut is_none)?;
        if is_none {
            return Ok(None);
        }
        let accessor = desc.hasGetter_() || desc.hasSetter_();
        // SAFETY: the descriptor holds live objects, which are rooted here before anything can GC.
        let object = |raw: *mut JSObject| unsafe { Stack::<Object>::from_raw(scope, raw) };
        Ok(Some(OwnProperty {
            value: (!accessor).then(|| scope.root_value(desc.value_)),
            getter: object(desc.getter_),
            setter: object(desc.setter_),
            enumerable: desc.enumerable_(),
            configurable: desc.configurable_(),
            writable: desc.writable_(),
        }))
    }

    /// The own property of this object with key `id` as an object, as
    /// `Object.getOwnPropertyDescriptor` returns it, or `undefined` if there is none.
    pub fn own_property_descriptor_value<'r>(
        &self,
        scope: &'r Scope<'_>,
        id: HandleId,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        mozjs::rooted!(in(unsafe { scope.raw_cx_no_gc() }) let mut desc = PropertyDescriptor::default());
        let mut is_none = true;
        self.get_own_property_descriptor(scope, id, desc.handle_mut(), &mut is_none)?;
        let mut rval = scope.root_value_mut(UndefinedValue());
        if is_none {
            return Ok(rval.handle());
        }
        // SAFETY: the descriptor and the result are rooted.
        let ok = unsafe {
            wrappers2::FromPropertyDescriptor(scope.cx_mut(), desc.handle(), rval.reborrow())
        };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Define the own property with key `id` from the property descriptor object `descriptor`,
    /// completed with the defaults of `ToCompletePropertyDescriptor`. Returns whether the property
    /// was defined, as `Reflect.defineProperty` does.
    pub fn define_property_from_descriptor(
        &self,
        scope: &Scope<'_>,
        id: HandleId,
        descriptor: HandleValue,
    ) -> Result<bool, ExnThrown> {
        mozjs::rooted!(in(unsafe { scope.raw_cx_no_gc() }) let mut desc = PropertyDescriptor::default());
        // SAFETY: the descriptor value and the descriptor are rooted.
        let ok = unsafe {
            wrappers2::ToCompletePropertyDescriptor(scope.cx_mut(), descriptor, desc.handle_mut())
        };
        ExnThrown::check(ok)?;
        // The `Uninitialized` code has all bits set.
        let mut result = ObjectOpResult { code_: usize::MAX };
        // SAFETY: the handles are rooted, and `result` is a valid location.
        let ok = unsafe {
            wrappers2::JS_DefinePropertyById(
                scope.cx_mut(),
                self.handle(),
                id,
                desc.handle(),
                &mut result,
            )
        };
        ExnThrown::check(ok)?;
        Ok(result.code_ == mozjs::jsapi::ObjectOpResult_SpecialCodes::OkCode as usize)
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

    /// Get the property keys of this object.
    ///
    /// `flags` controls which properties to include (e.g.,
    /// `JSITER_OWNONLY`, `JSITER_HIDDEN`).
    // TODO: this should return a vector of ids instead of taking a MutableHandle outparam.
    pub fn get_property_keys(
        &self,
        scope: &Scope<'_>,
        flags: c_uint,
        props: mozjs::jsapi::MutableHandleIdVector,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::GetPropertyKeys(scope.cx_mut(), self.handle(), flags, props) };
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

/// A key of a [`PropertyTemplate`].
#[derive(Clone, Copy, Debug)]
pub enum TemplateKey {
    /// A property name, interned as [`crate::class::get_or_init_property_id`] interns it. It must
    /// not be an array index.
    Name(&'static CStr),
    /// A symbol.
    Symbol(&'static crate::symbol::CachedSymbol),
}

impl From<&'static CStr> for TemplateKey {
    fn from(name: &'static CStr) -> Self {
        TemplateKey::Name(name)
    }
}

impl From<&'static crate::symbol::CachedSymbol> for TemplateKey {
    fn from(symbol: &'static crate::symbol::CachedSymbol) -> Self {
        TemplateKey::Symbol(symbol)
    }
}

impl TemplateKey {
    /// Whether `self` and `other` are the same key.
    fn same(self, other: TemplateKey) -> bool {
        match (self, other) {
            (TemplateKey::Name(a), TemplateKey::Name(b)) => a == b,
            (TemplateKey::Symbol(a), TemplateKey::Symbol(b)) => std::ptr::eq(a, b),
            _ => false,
        }
    }
}

/// The keys of the own data properties that objects of one kind are created with, in the order
/// they are created. Templates are `static`s.
///
/// [`Object::property_writer`] creates the properties on an object, and looks the keys up once per
/// object rather than once per property. Every object that gets its properties from one template
/// gets them in the same order. A global registers each template with
/// [`register`](Self::register) when it is set up.
pub struct PropertyTemplate {
    keys: &'static [TemplateKey],
}

impl PropertyTemplate {
    /// A template of `keys`, in that order. A key appears at most once.
    pub const fn new(keys: &'static [TemplateKey]) -> Self {
        PropertyTemplate { keys }
    }

    /// The keys, in order.
    pub fn keys(&self) -> &'static [TemplateKey] {
        self.keys
    }

    /// The number of keys.
    pub const fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the template has no keys.
    pub const fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Intern the template's keys in the current global.
    pub fn register(&'static self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        crate::class::get_or_init_template_keys(scope, self).map(|_| ())
    }
}

/// How a [`PropertyWriter`] creates properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyInit {
    /// Create each property as `Object.defineProperty` does, enumerable, writable and
    /// configurable, without consulting the prototype chain. Setters on the prototype chain are
    /// not called. The writer collects the properties and adds them to the object in one call.
    Define,
    /// Assign each property as `object[key] = value` does, which calls a setter that the
    /// prototype chain has for the key.
    Assign,
}

/// How many properties [`PropertyWriter`] collects, and [`Object::add_new_data_properties`] and
/// [`Object::new_plain_with_unique_names`] create, without allocating.
const INLINE_PROPERTIES: usize = 36;

/// The rooter of the values of properties that are created in one call.
type PropertyValueRooter = crate::value::ValueArrayRooter<[mozjs::jsval::JSVal; INLINE_PROPERTIES]>;

/// How a [`PropertyWriter`] creates the properties written to it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    /// Collect them, and add them in one call.
    Collect,
    /// Define each one as it is written.
    Define,
    /// Assign each one as it is written.
    Assign,
}

/// Creates the properties of [`PropertyTemplate`]s on an object, in the templates' order, from
/// [`Object::property_writer`].
///
/// Each call names the key it creates, which must be the template's next key. Debug builds check
/// it. A writer can stop before the template's last key, and [`skip`](Self::skip) leaves out a
/// key, so a template can hold keys that only some objects get.
/// [`continue_with`](Self::continue_with) continues with the keys of another template, and the
/// keys of all of a writer's templates are distinct.
///
/// With [`PropertyInit::Define`], the properties exist only after [`finish`](Self::finish) or
/// [`flush`](Self::flush). Every writer ends with `finish`.
pub struct PropertyWriter<'s> {
    object: Stack<'s, Object>,
    template: &'static PropertyTemplate,
    /// The addresses of the global's traced cells that hold the template's keys.
    keys: *const [*const mozjs::jsapi::jsid],
    next: usize,
    mode: WriteMode,
    /// The keys of the collected properties. The global's cells keep them alive.
    collected_keys: smallvec::SmallVec<[mozjs::jsapi::jsid; INLINE_PROPERTIES]>,
    /// The values of the collected properties, rooted in the writer's scope.
    collected_values: smallvec::SmallVec<[HandleValue<'s>; INLINE_PROPERTIES]>,
}

impl<'s> PropertyWriter<'s> {
    /// Create the property `key`, the template's next key, with `value`.
    pub fn write(
        &mut self,
        scope: &'s Scope<'_>,
        key: impl Into<TemplateKey>,
        value: impl ToJSVal<'s>,
    ) -> Result<(), ExnThrown> {
        let id = self.take(key.into());
        match self.mode {
            WriteMode::Collect => {
                let value = value.to_jsval_throwing(scope)?;
                self.collected_keys.push(id.get());
                self.collected_values.push(value);
                Ok(())
            }
            WriteMode::Define => self.object.define_value_by_id(
                scope,
                id,
                value,
                crate::class_spec::JSPROP_ENUMERATE as c_uint,
            ),
            WriteMode::Assign => self.object.set_property_by_id(scope, id, value),
        }
    }

    /// Leave out the property `key`, the template's next key.
    pub fn skip(&mut self, key: impl Into<TemplateKey>) {
        self.take(key.into());
    }

    /// Continue with the keys of `template`, from its first key. The keys of the previous
    /// template that weren't written or skipped are left out. Debug builds check that the global
    /// registered the template.
    pub fn continue_with(
        &mut self,
        scope: &Scope<'_>,
        template: &'static PropertyTemplate,
    ) -> Result<(), ExnThrown> {
        let (keys, registered) = crate::class::get_or_init_template_keys(scope, template)?;
        debug_assert!(
            registered,
            "property templates are registered at global setup"
        );
        self.template = template;
        self.keys = keys;
        self.next = 0;
        Ok(())
    }

    /// The property `name` of the object, as `object[name]` reads it, including a property the
    /// writer collected. Before a read that runs JavaScript, through an accessor or a `Proxy` on
    /// the prototype chain, it adds the collected properties as [`flush`](Self::flush) does.
    pub fn get(
        &mut self,
        scope: &'s Scope<'_>,
        name: &'static CStr,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let id = crate::class::get_or_init_property_id(scope, name)?;
        if self.mode == WriteMode::Collect {
            let bits = id.get().asBits_;
            if let Some(index) = self
                .collected_keys
                .iter()
                .position(|key| key.asBits_ == bits)
            {
                return Ok(self.collected_values[index]);
            }
            let mut holder = Some(self.object);
            while let Some(object) = holder {
                if object.is_proxy() {
                    break;
                }
                match object.own_property_descriptor(scope, id)? {
                    Some(OwnProperty {
                        value: Some(value), ..
                    }) => return Ok(value),
                    Some(_) => break,
                    None => holder = object.get_prototype(scope)?,
                }
            }
            if holder.is_none() {
                return Ok(HandleValue::undefined());
            }
            self.flush(scope)?;
        }
        self.object.get_property_by_id(scope, id)
    }

    /// Add the properties the writer collected to the object, and define each later property as
    /// it is written. The object can be passed to JavaScript after this call.
    pub fn flush(&mut self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        if self.mode == WriteMode::Collect {
            self.mode = WriteMode::Define;
        }
        self.add_collected(scope)
    }

    /// Add the properties the writer collected to the object.
    pub fn finish(mut self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        self.add_collected(scope)
    }

    /// Add the properties the writer collected to the object in one call.
    fn add_collected(&mut self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        if self.collected_keys.is_empty() {
            return Ok(());
        }
        // SAFETY: the creator of the writer guarantees that the object can get the properties
        // added in one call, and the template's keys are distinct, interned keys that the
        // global keeps alive.
        let result = unsafe {
            self.object
                .add_new_data_properties(scope, &self.collected_keys, &self.collected_values)
        };
        self.collected_keys.clear();
        self.collected_values.clear();
        result
    }

    /// Return the template's next key, which must be `key`, and advance past it.
    fn take(&mut self, key: TemplateKey) -> HandleId<'s> {
        let index = self.next;
        debug_assert!(
            self.template
                .keys
                .get(index)
                .is_some_and(|expected| expected.same(key)),
            "{key:?} isn't key {index} of its template"
        );
        self.next += 1;
        // SAFETY: the cells are traced, and live as long as the global, which outlives the writer.
        unsafe { HandleId::from_marked_location((&*self.keys)[index]) }
    }
}

/// An own property of an object, as [`Object::own_property_descriptor`] returns it.
pub struct OwnProperty<'r> {
    /// The value of a data property, or `None` for an accessor property.
    pub value: Option<HandleValue<'r>>,
    /// The getter of an accessor property, if it has one.
    pub getter: Option<Stack<'r, Object>>,
    /// The setter of an accessor property, if it has one.
    pub setter: Option<Stack<'r, Object>>,
    pub enumerable: bool,
    pub configurable: bool,
    /// Whether a data property is writable. `false` for an accessor property.
    pub writable: bool,
}

impl<'s, 'v> FromJSVal<'s, 'v> for Stack<'s, Object> {
    type Config = ();

    fn from_jsval(
        scope: &'s Scope<'_>,
        val: HandleValue<'v>,
        _option: Self::Config,
    ) -> Result<Self, ConversionError> {
        Self::from_value(scope, *val)
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
