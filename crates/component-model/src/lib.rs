// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Component Model interpreter for StarlingMonkey.
//!
//! This crate implements the JavaScript side of a WebAssembly component built
//! by `starling-componentize`: it lifts WIT values into JavaScript values and
//! lowers JavaScript values back into WIT values, driving exported and imported
//! component functions through a SpiderMonkey runtime.
//!
//! The conversion core in [`value`] is written against a crate-local
//! [`TypeShape`](value::TypeShape) description rather than the `wit-dylib-ffi`
//! types, so the whole JS↔WIT mapping can be tested natively against a real
//! runtime.

pub mod component_error;
pub mod exports;
pub mod imports;
pub mod init;
#[cfg(target_arch = "wasm32")]
pub mod interpreter;
pub mod naming;
#[cfg(target_arch = "wasm32")]
pub mod promises;
#[cfg(target_arch = "wasm32")]
pub mod readable;
pub mod resources;
#[cfg(target_arch = "wasm32")]
pub mod run;
pub mod stack;
#[cfg(target_arch = "wasm32")]
pub mod streams;
pub mod tracing;
pub mod validate;
pub mod value;

pub use component_error::{component_error_payload, error_class, ComponentError, ErrorClass};
pub use exports::{
    call_export, install_table, resolve_export, ExportSpec, ExportTable, Outcome, Resolved,
    ResultShape,
};
pub use imports::{
    call_import_from_js, call_import_from_js_async, synthesize_import_modules, EnumDesc,
    ImportFuncDesc, ImportInvoker, InterfaceDesc, ResourceTypeDesc, WorldDesc,
};
pub use init::{initialize_runtime, with_main_scope, with_scope, Initialized};
pub use resources::{ExportedResources, ResourceDesc};
pub use stack::CallStack;
pub use tracing::install_tracer;
pub use value::{mangle_name, TypeShape};

/// Install the component-model globals on `global`.
///
/// [`ComponentError`] holds the `err` arm of a WIT `result<_, err>` on the JS
/// side. On wasm, this also registers the internal classes backing
/// `stream<T>` and `future<T>` values (see [`readable`] and [`promises`]).
pub fn add_to_global(scope: &js::gc::scope::Scope<'_>, global: js::Object<'_>) {
    ComponentError::add_to_global(scope, global);
    component_error::name_prototype(scope);
    #[cfg(target_arch = "wasm32")]
    {
        readable::IncomingStream::add_to_global(scope, global);
        readable::StreamPump::add_to_global(scope, global);
        promises::FutureWrite::add_to_global(scope, global);
        promises::FutureRead::add_to_global(scope, global);
    }
}
