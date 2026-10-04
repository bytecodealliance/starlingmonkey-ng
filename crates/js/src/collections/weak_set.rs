// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! JavaScript `WeakSet` collection type.

use crate::builtins::JSType;
use crate::gc::handle::Stack;

/// Marker type for JavaScript `WeakSet` objects, for brand checks with `cast::<js::WeakSet>()`.
pub struct WeakSet;

impl JSType for WeakSet {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "WeakSet";

    fn js_class() -> *const mozjs::jsapi::JSClass {
        crate::class::proto_key_to_class(mozjs::jsapi::JSProtoKey::JSProto_WeakSet)
    }
}

crate::gc::handle::deref_to_object!(WeakSet);
