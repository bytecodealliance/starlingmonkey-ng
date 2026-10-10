// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Property key (`jsid`) operations.
//!
//! In SpiderMonkey, property keys (`jsid` / `PropertyKey`) can be strings,
//! integers, or symbols. This module provides utilities for creating and
//! converting between property keys and their constituent types.

use crate::gc::scope::Scope;
use mozjs::gc::{Handle, HandleId, HandleString, HandleValue};
use mozjs::jsapi::{jsid, JSProtoKey, PropertyKey};
use mozjs::jsval::UndefinedValue;
use mozjs::rooted;
use mozjs::rust::wrappers2;

use super::error::ExnThrown;

/// Convert a JS value to a property key (`jsid`).
pub fn value_to_id(scope: &Scope<'_>, v: mozjs::rust::HandleValue) -> Result<jsid, ExnThrown> {
    rooted!(in(unsafe { scope.raw_cx_no_gc() }) let mut idp = jsid::default());
    let ok = unsafe { wrappers2::JS_ValueToId(scope.cx_mut(), v, idp.handle_mut()) };
    ExnThrown::check(ok)?;
    Ok(idp.get())
}

/// Convert a JS string to a property key.
pub fn string_to_id(scope: &Scope<'_>, s: HandleString) -> Result<jsid, ExnThrown> {
    rooted!(in(unsafe { scope.raw_cx_no_gc() }) let mut idp = jsid::default());
    let ok = unsafe { wrappers2::JS_StringToId(scope.cx_mut(), s, idp.handle_mut()) };
    ExnThrown::check(ok)?;
    Ok(idp.get())
}

/// Convert a property key back to a JS value.
pub fn id_to_value<'r>(scope: &'r Scope<'_>, id: jsid) -> Result<HandleValue<'r>, ExnThrown> {
    let mut vp = scope.root_value_mut(UndefinedValue());
    let ok = unsafe { wrappers2::JS_IdToValue(scope.cx(), id, vp.reborrow()) };
    ExnThrown::check(ok)?;
    Ok(vp.handle())
}

/// The property key `id` as `Reflect.ownKeys` returns it: a string or a symbol. An integer key
/// becomes its decimal string.
pub fn id_to_key<'r>(scope: &'r Scope<'_>, id: jsid) -> Result<HandleValue<'r>, ExnThrown> {
    if id.is_int() {
        let string = crate::JSString::from_str(scope, &id.to_int().to_string())?;
        return Ok(scope.root_value(string.as_value()));
    }
    id_to_value(scope, id)
}

/// Convert a numeric index to a property key.
pub fn index_to_id(scope: &Scope<'_>, index: u32) -> Result<jsid, ExnThrown> {
    rooted!(in(unsafe { scope.raw_cx_no_gc() }) let mut idp = jsid::default());
    let ok = unsafe { wrappers2::JS_IndexToId(scope.cx(), index, idp.handle_mut()) };
    ExnThrown::check(ok)?;
    Ok(idp.get())
}

/// Get the `JSProtoKey` for a given property key identifier.
pub fn id_to_proto_key(scope: &Scope<'_>, id: HandleId) -> JSProtoKey {
    unsafe { wrappers2::JS_IdToProtoKey(scope.cx(), id) }
}

/// Mark a property key as cross-zone (needed for property keys used across zones).
pub fn mark_cross_zone_id(scope: &Scope<'_>, id: jsid) {
    unsafe { wrappers2::JS_MarkCrossZoneId(scope.cx(), id) }
}

/// Convert getter/setter property ids.
pub fn to_getter_id(scope: &Scope<'_>, id: Handle<PropertyKey>) -> Result<PropertyKey, ExnThrown> {
    rooted!(in(unsafe { scope.raw_cx_no_gc() }) let mut getter_id = PropertyKey::default());
    let ok = unsafe { wrappers2::ToGetterId(scope.cx_mut(), id, getter_id.handle_mut()) };
    ExnThrown::check(ok)?;
    Ok(getter_id.get())
}

/// Convert to setter property id.
pub fn to_setter_id(scope: &Scope<'_>, id: Handle<PropertyKey>) -> Result<PropertyKey, ExnThrown> {
    rooted!(in(unsafe { scope.raw_cx_no_gc() }) let mut setter_id = PropertyKey::default());
    let ok = unsafe { wrappers2::ToSetterId(scope.cx_mut(), id, setter_id.handle_mut()) };
    ExnThrown::check(ok)?;
    Ok(setter_id.get())
}
