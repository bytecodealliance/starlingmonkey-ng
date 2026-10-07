// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Map, Set, and WeakMap collection types.
//!
//! Each collection is represented by a scope-rooted newtype wrapping `Handle<'s, *mut JSObject>`.
//! Create instances with the `new()` constructor and use methods for operations.
//!
//! # Example
//!
//! ```ignore
//! use crate::collections::map::Map;
//!
//! let map = Map::new(scope)?;
//! map.set(scope, key, val)?;
//! assert!(map.has(scope, key)?);
//! ```

use mozjs::rust::{HandleObject, HandleValue};

use crate::error::ExnThrown;
use crate::gc::scope::Scope;

pub mod map;
pub mod set;
pub mod weak_map;
pub mod weak_set;

/// The signature of `JS::MapForEach` and `JS::SetForEach`.
type ForEach =
    unsafe fn(&mut mozjs::context::JSContext, HandleObject, HandleValue, HandleValue) -> bool;

/// The values `for_each` passes to its callback for `collection`, each call's first `count`
/// arguments in reverse order: a `Map`'s key and then value for 2, and a `Set`'s value for 1.
fn collect_for_each<'r>(
    scope: &'r Scope<'_>,
    collection: HandleObject,
    for_each: ForEach,
    count: u32,
) -> Result<Vec<HandleValue<'r>>, ExnThrown> {
    let values = crate::Array::new(scope, 0)?;
    let callback = crate::Function::new_callback(scope, c"", count, collect_arguments, values)?;
    let this = crate::value::from_i32(count as i32);
    // SAFETY: the handles are rooted, and the callback is a function.
    let ok = unsafe {
        for_each(
            scope.cx_mut(),
            collection,
            scope.root_value(crate::conversion::ToJSVal::to_jsval_raw_throwing(
                &callback, scope,
            )?),
            scope.root_value(this),
        )
    };
    ExnThrown::check(ok)?;
    (0..values.length(scope)?)
        .map(|index| values.get_element(scope, index))
        .collect()
}

/// The callback of [`collect_for_each`], whose payload is the array it appends to and whose
/// `this` is the number of arguments to append, in reverse order.
fn collect_arguments(
    scope: &Scope<'_>,
    args: crate::function::CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<crate::native::Value, ExnThrown> {
    let values = crate::function::cast_payload::<crate::Array>(scope, payload);
    let length = values.length(scope)?;
    let attrs = crate::class_spec::JSPROP_ENUMERATE as u32;
    let count = args.this().to_int32() as u32;
    for (offset, index) in (0..count).rev().enumerate() {
        values.define_element(scope, length + offset as u32, args.get(index), attrs)?;
    }
    Ok(crate::value::undefined())
}
