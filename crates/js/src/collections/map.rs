// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! JavaScript `Map` collection type.

use std::ptr::NonNull;

use crate::builtins::JSType;
use crate::gc::handle::Stack;
use crate::gc::scope::Scope;
use mozjs::jsval::UndefinedValue;
use mozjs::rust::wrappers2;
use mozjs::rust::{HandleObject, HandleValue};

use crate::error::ExnThrown;

/// Marker type for JavaScript `Map` objects.
///
/// Use the `js::Map` alias for [`Stack<'s, Map>`](crate::gc::handle::Stack)
/// as the scope-rooted handle type:
///
/// ```ignore
/// let map = js::Map::new(&scope)?;
/// map.insert(&scope, key.handle(), val.handle())?;
/// ```
pub struct Map;

impl JSType for Map {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "Map";

    fn js_class() -> *const mozjs::jsapi::JSClass {
        crate::class::proto_key_to_class(mozjs::jsapi::JSProtoKey::JSProto_Map)
    }
}

impl<'s> Stack<'s, Map> {
    /// Create a new empty `Map` object.
    pub fn new(scope: &'s Scope<'_>) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::NewMapObject(scope.cx_mut()) };
        NonNull::new(obj)
            .map(|nn| unsafe { Self::from_handle_unchecked(scope.root_object(nn)) })
            .ok_or(ExnThrown)
    }

    /// Get the number of entries.
    pub fn size(&self, scope: &Scope<'_>) -> u32 {
        unsafe { wrappers2::MapSize(scope.cx(), self.handle()) }
    }

    /// Look up a value by key.
    ///
    /// This is named `lookup` rather than `get` to avoid confusion with
    /// `Handle::get`.
    pub fn lookup<'r>(
        &self,
        scope: &'r Scope<'_>,
        key: HandleValue<'r>,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe { wrappers2::MapGet(scope.cx_mut(), self.handle(), key, rval.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Check whether the map contains a key.
    pub fn has(&self, scope: &Scope<'_>, key: HandleValue) -> Result<bool, ExnThrown> {
        let mut result = false;
        let ok = unsafe { wrappers2::MapHas(scope.cx_mut(), self.handle(), key, &mut result) };
        ExnThrown::check(ok)?;
        Ok(result)
    }

    /// Insert a key-value pair.
    ///
    /// This is named `insert` rather than `set` to avoid confusion with
    /// `Handle::set`.
    pub fn insert(
        &self,
        scope: &Scope<'_>,
        key: HandleValue,
        val: HandleValue,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::MapSet(scope.cx_mut(), self.handle(), key, val) };
        ExnThrown::check(ok)
    }

    /// Delete a key. Returns whether the key was present.
    pub fn delete(&self, scope: &Scope<'_>, key: HandleValue) -> Result<bool, ExnThrown> {
        let mut deleted = false;
        let ok = unsafe { wrappers2::MapDelete(scope.cx_mut(), self.handle(), key, &mut deleted) };
        ExnThrown::check(ok)?;
        Ok(deleted)
    }

    /// Remove all entries from the map.
    pub fn clear(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::MapClear(scope.cx(), self.handle()) };
        ExnThrown::check(ok)
    }

    /// Get an iterator over the map's keys.
    pub fn keys<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe { wrappers2::MapKeys(scope.cx_mut(), self.handle(), rval.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Get an iterator over the map's values.
    pub fn values<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe { wrappers2::MapValues(scope.cx_mut(), self.handle(), rval.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Get an iterator over the map's entries (key-value pairs).
    pub fn entries<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe { wrappers2::MapEntries(scope.cx_mut(), self.handle(), rval.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Call a callback for each entry in the map.
    pub fn for_each(
        &self,
        scope: &Scope<'_>,
        callback_fn: HandleValue,
        this_val: HandleValue,
    ) -> Result<(), ExnThrown> {
        let ok =
            unsafe { wrappers2::MapForEach(scope.cx_mut(), self.handle(), callback_fn, this_val) };
        ExnThrown::check(ok)
    }

    /// The map's entries as `(key, value)` pairs, in insertion order. Runs no code the script
    /// can change.
    pub fn entries_vec<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Result<Vec<(HandleValue<'r>, HandleValue<'r>)>, ExnThrown> {
        let values = super::collect_for_each(scope, self.handle(), wrappers2::MapForEach, 2)?;
        Ok(values
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[key, value]| (*key, *value))
            .collect())
    }

    /// Check whether an object is a `Map`.
    pub fn is_map(scope: &Scope<'_>, obj: HandleObject) -> Result<bool, ExnThrown> {
        let mut result = false;
        // SAFETY: cx and obj are valid; IsMapObject writes to result.
        let ok = unsafe { wrappers2::IsMapObject(scope.cx_mut(), obj, &mut result) };
        ExnThrown::check(ok)?;
        Ok(result)
    }
}

crate::gc::handle::deref_to_object!(Map);

crate::gc::handle::from_jsval_via_cast!(Map, c"Value isn't a Map");
