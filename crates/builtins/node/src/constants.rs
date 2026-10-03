// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:constants`, the deprecated module of the `os.constants` `dlopen`, `errno`, `priority` and
//! `signals` values and the `fs.constants` values, as Node.js's `lib/constants.js` merges them.

use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

#[core_runtime::jsmodule(node_args, name = "node:constants", default_namespace)]
pub mod module {}

/// The own enumerable properties of `object`, in order.
fn entries<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
) -> Result<Vec<(String, HandleValue<'s>)>, ExnThrown> {
    let mut entries = Vec::new();
    for key in object.own_keys(scope)? {
        if !key.is_string() {
            continue;
        }
        let name = js::JSString::from_value(scope, *key)?.to_utf8(scope)?;
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        entries.push((name, object.get_property_by_id(scope, id)?));
    }
    Ok(entries)
}

/// Register `node:constants`.
pub(crate) fn register_module(scope: &Scope<'_>, _name: &'static str) -> bool {
    let values = || -> Result<Vec<(String, HandleValue<'_>)>, ExnThrown> {
        let os = crate::os::constants(scope)?;
        let mut values = Vec::new();
        for group in [c"dlopen", c"errno", c"priority", c"signals"] {
            if let Ok(group) = Object::from_value(scope, os.get_property(scope, group)?) {
                values.extend(entries(scope, group)?);
            }
        }
        if let Ok(fs) = Object::from_value(scope, crate::fs::constants(scope)?) {
            values.extend(entries(scope, fs)?);
        }
        Ok(values)
    };
    let Ok(values) = values() else {
        return false;
    };
    // A later group's value replaces an earlier one's, as `Object.assign` does.
    let mut merged: Vec<(&str, HandleValue<'_>)> = Vec::new();
    for (name, value) in &values {
        merged.retain(|(existing, _)| existing != name);
        merged.push((name.as_str(), *value));
    }
    core_runtime::module::register_module_with::<module::js_module>(scope, &merged)
}
