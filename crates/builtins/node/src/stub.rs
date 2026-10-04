// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Stubs of the Node.js builtin modules that aren't implemented: modules with Node's export names
//! that can be imported, whose functions and classes throw `ERR_METHOD_NOT_IMPLEMENTED` when called
//! or constructed.

use std::ffi::CString;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use crate::errors::NodeError;
use crate::stream::Props;

/// The kind of value a builtin module exports under a name, as `exports_table` records it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ExportKind {
    /// A class. The stub is a constructor that throws, with a `prototype` object to extend.
    Class,
    /// A function. The stub throws when called.
    Function,
    /// A string constant, exported as is.
    Str(&'static str),
    /// A number constant, exported as is.
    Num(f64),
    /// A boolean constant, exported as is.
    Bool(bool),
    /// `undefined`.
    Undefined,
    /// `null`.
    Null,
    /// A symbol with this description. The stub is a new symbol. Only the modules that exist with
    /// a runtime flag record symbols as such.
    Symbol(&'static str),
    /// Any other object. The stub is an empty object.
    Object,
}

/// A builtin module's exports and their kinds.
pub(crate) type ModuleExports = &'static [(&'static str, ExportKind)];

/// A builtin module's name, the other specifiers that import it, and its exports.
pub(crate) type BuiltinEntry = (&'static str, &'static [&'static str], ModuleExports);

/// A builtin module that only exists with a runtime flag: its name, the other specifiers that
/// import it, the kind of its value, and its exports.
pub(crate) type FlaggedEntry = (
    &'static str,
    &'static [&'static str],
    ExportKind,
    ModuleExports,
);

/// The exports of builtin module `name`, including the modules that only exist with a runtime
/// flag, or `None` for a name that isn't one.
pub(crate) fn exports_of(name: &str) -> Option<ModuleExports> {
    let builtin = crate::exports_table::BUILTIN_EXPORTS
        .binary_search_by(|(module, _, _)| (*module).cmp(name))
        .ok()
        .map(|index| crate::exports_table::BUILTIN_EXPORTS[index].2);
    builtin.or_else(|| flagged_entry(name).map(|entry| entry.3))
}

/// The entry of `name` among the builtin modules that only exist with a runtime flag.
pub(crate) fn flagged_entry(name: &str) -> Option<&'static FlaggedEntry> {
    crate::exports_table::FLAGGED_EXPORTS
        .binary_search_by(|(module, _, _, _)| (*module).cmp(name))
        .ok()
        .map(|index| &crate::exports_table::FLAGGED_EXPORTS[index])
}

/// Register the stub of builtin module `name` under that name: its named exports, and a
/// `default` export holding them all. An export takes the value of the global of the same name
/// where one is defined, such as `stream/web`'s classes and `perf_hooks.performance`. `console`'s
/// exports are the global `console`'s properties, and its default export is `console` itself.
pub(crate) fn register_stub(scope: &Scope<'_>, name: &'static str) -> bool {
    create_stub(scope, name).is_ok()
}

fn create_stub(scope: &Scope<'_>, name: &'static str) -> Result<(), ExnThrown> {
    let exports = exports_of(name).unwrap_or_default();
    let global = scope.global();
    let console = match name {
        "node:console" => global.obj(scope, c"console")?,
        _ => None,
    };
    let namespace = console.map_or_else(|| Object::new_plain(scope), Ok)?;
    let mut values = Vec::with_capacity(exports.len() + 1);
    for &(export, kind) in exports {
        let c_export = CString::new(export).map_err(|_| ExnThrown)?;
        let existing = match console {
            Some(console) => console.get_property_by_name(scope, &c_export)?,
            None => global.get_property_by_name(scope, &c_export)?,
        };
        let value = if existing.is_undefined() {
            stub_value(scope, name, export, kind)?
        } else {
            existing
        };
        namespace.set_property_by_name(scope, &c_export, value)?;
        values.push((export, value));
    }
    let default = namespace.to_jsval_throwing(scope)?;
    values.push(("default", default));
    core_runtime::module::register_synthetic_module(scope, name, &values)
}

/// The stub of export `export` of builtin module `module`, which has `kind`.
pub(crate) fn stub_value<'s>(
    scope: &'s Scope<'_>,
    module: &str,
    export: &str,
    kind: ExportKind,
) -> Result<HandleValue<'s>, ExnThrown> {
    Ok(match kind {
        ExportKind::Class | ExportKind::Function => {
            let module = module.strip_prefix("node:").unwrap_or(module);
            let qualified = format!("{module}.{export}");
            let c_export = CString::new(export).map_err(|_| ExnThrown)?;
            let function = if matches!(kind, ExportKind::Class) {
                let ctor = js::Function::new_constructor_callback(
                    scope,
                    &c_export,
                    0,
                    not_implemented,
                    qualified.as_str(),
                )?;
                let prototype = Object::new_plain(scope)?;
                prototype.define_property(scope, c"constructor", *ctor, 0)?;
                ctor.define_property(scope, c"prototype", prototype, 0)?;
                ctor
            } else {
                js::Function::new_callback(
                    scope,
                    &c_export,
                    0,
                    not_implemented,
                    qualified.as_str(),
                )?
            };
            mark_unimplemented(scope, *function, &qualified)?;
            (*function).to_jsval_throwing(scope)?
        }
        ExportKind::Str(string) => string.to_jsval_throwing(scope)?,
        ExportKind::Num(number) => scope.root_value(js::value::from_f64(number)),
        ExportKind::Bool(boolean) => scope.root_value(js::value::from_bool(boolean)),
        ExportKind::Undefined => scope.root_value(js::value::undefined()),
        ExportKind::Null => scope.root_value(js::value::null()),
        ExportKind::Symbol(description) => crate::symbol(scope, description, false)?,
        ExportKind::Object => Object::new_plain(scope)?.to_jsval_throwing(scope)?,
    })
}

/// Set `stub[Symbol.for("starling.unimplemented")]` to `qualified`, the module and export name the
/// stub stands for, for tests to recognize stubs by.
fn mark_unimplemented(
    scope: &Scope<'_>,
    stub: Object<'_>,
    qualified: &str,
) -> Result<(), ExnThrown> {
    let key = crate::symbol(scope, "starling.unimplemented", true)?;
    let key = scope.root_id(js::id::value_to_id(scope, key)?);
    stub.set_property_by_id(scope, key, qualified)
}

/// Register the native module `T` as builtin module `name`, with stubs for the exports Node.js has
/// and `T` lacks.
pub(crate) fn register_implemented<T: core_runtime::module::NativeModule>(
    scope: &Scope<'_>,
    name: &'static str,
) -> bool {
    register_implemented_with::<T>(scope, name, &[])
}

/// [`register_implemented`], with `overrides` as further exports with the given values, in place
/// of the stubs for those names.
pub(crate) fn register_implemented_with<T: core_runtime::module::NativeModule>(
    scope: &Scope<'_>,
    name: &'static str,
    overrides: &[(&'static str, HandleValue<'_>)],
) -> bool {
    use core_runtime::module::{DefaultExport, ModuleExport};
    let declared: Vec<&str> = T::declarations()
        .iter()
        .map(|decl| match decl {
            ModuleExport::Function { js_name, .. } | ModuleExport::Value { js_name } => *js_name,
        })
        .chain(T::MIRRORED_EXPORTS.iter().copied())
        .chain(match T::DEFAULT_EXPORT {
            DefaultExport::Namespace {
                self_export: Some(name),
            } => Some(name),
            _ => None,
        })
        .collect();
    let mut extra: Vec<(&str, HandleValue<'_>)> = overrides.to_vec();
    for &(export, kind) in exports_of(name).unwrap_or_default() {
        if declared.contains(&export) || overrides.iter().any(|(name, _)| *name == export) {
            continue;
        }
        let Ok(value) = stub_value(scope, name, export, kind) else {
            return false;
        };
        extra.push((export, value));
    }
    core_runtime::module::register_module_with::<T>(scope, &extra)
}

/// The body of every stub function: throw `ERR_METHOD_NOT_IMPLEMENTED` naming the export, which
/// is the callback's payload.
fn not_implemented(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let what = String::from_jsval(scope, payload, ()).unwrap_or_default();
    Err(NodeError::not_implemented(&what).throw(scope))
}
