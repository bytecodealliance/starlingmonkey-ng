// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! JavaScript `Set` collection type.

use std::ptr::NonNull;

use crate::builtins::JSType;
use crate::gc::handle::Stack;
use crate::gc::scope::Scope;
use mozjs::jsval::UndefinedValue;
use mozjs::rust::wrappers2;
use mozjs::rust::{HandleObject, HandleValue};

use crate::error::ExnThrown;

/// Marker type for JavaScript `Set` objects.
///
/// Use the `js::Set` alias for [`Stack<'s, Set>`](crate::gc::handle::Stack)
/// as the scope-rooted handle type:
///
/// ```ignore
/// let set = js::Set::new(&scope)?;
/// set.add(&scope, val.handle())?;
/// ```
pub struct Set;

impl JSType for Set {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "Set";

    fn js_class() -> *const mozjs::jsapi::JSClass {
        crate::class::proto_key_to_class(mozjs::jsapi::JSProtoKey::JSProto_Set)
    }
}

impl<'s> Stack<'s, Set> {
    /// Create a new empty `Set` object.
    pub fn new(scope: &'s Scope<'_>) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::NewSetObject(scope.cx_mut()) };
        NonNull::new(obj)
            .map(|nn| unsafe { Self::from_handle_unchecked(scope.root_object(nn)) })
            .ok_or(ExnThrown)
    }

    /// Get the number of entries.
    pub fn size(&self, scope: &Scope<'_>) -> u32 {
        unsafe { wrappers2::SetSize(scope.cx(), self.handle()) }
    }

    /// Check whether the set contains a value.
    pub fn has(&self, scope: &Scope<'_>, key: HandleValue) -> Result<bool, ExnThrown> {
        let mut result = false;
        let ok = unsafe { wrappers2::SetHas(scope.cx_mut(), self.handle(), key, &mut result) };
        ExnThrown::check(ok)?;
        Ok(result)
    }

    /// Add a value to the set.
    pub fn add(&self, scope: &Scope<'_>, key: HandleValue) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::SetAdd(scope.cx_mut(), self.handle(), key) };
        ExnThrown::check(ok)
    }

    /// Delete a value. Returns whether the value was present.
    pub fn delete(&self, scope: &Scope<'_>, key: HandleValue) -> Result<bool, ExnThrown> {
        let mut deleted = false;
        let ok = unsafe { wrappers2::SetDelete(scope.cx_mut(), self.handle(), key, &mut deleted) };
        ExnThrown::check(ok)?;
        Ok(deleted)
    }

    /// Remove all entries from the set.
    pub fn clear(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::SetClear(scope.cx(), self.handle()) };
        ExnThrown::check(ok)
    }

    /// Get an iterator over the set's keys (same as values for sets).
    pub fn keys<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe { wrappers2::SetKeys(scope.cx_mut(), self.handle(), rval.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Get an iterator over the set's values.
    pub fn values<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe { wrappers2::SetValues(scope.cx_mut(), self.handle(), rval.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Get an iterator over the set's entries.
    pub fn entries<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe { wrappers2::SetEntries(scope.cx_mut(), self.handle(), rval.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Call a callback for each value in the set.
    pub fn for_each(
        &self,
        scope: &Scope<'_>,
        callback_fn: HandleValue,
        this_val: HandleValue,
    ) -> Result<(), ExnThrown> {
        let ok =
            unsafe { wrappers2::SetForEach(scope.cx_mut(), self.handle(), callback_fn, this_val) };
        ExnThrown::check(ok)
    }

    /// The set's values, in insertion order. Runs no code the script can change.
    pub fn values_vec<'r>(&self, scope: &'r Scope<'_>) -> Result<Vec<HandleValue<'r>>, ExnThrown> {
        super::collect_for_each(scope, self.handle(), wrappers2::SetForEach, 1)
    }

    /// Check whether an object is a `Set`.
    pub fn is_set(scope: &Scope<'_>, obj: HandleObject) -> Result<bool, ExnThrown> {
        let mut result = false;
        // SAFETY: cx and obj are valid; IsSetObject writes to result.
        let ok = unsafe { wrappers2::IsSetObject(scope.cx_mut(), obj, &mut result) };
        ExnThrown::check(ok)?;
        Ok(result)
    }
}

crate::gc::handle::deref_to_object!(Set);

crate::gc::handle::from_jsval_via_cast!(Set, c"Value isn't a Set");
