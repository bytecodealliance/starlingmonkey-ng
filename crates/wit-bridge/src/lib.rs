// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The bridge between WIT and JavaScript in a component built by
//! `starling-componentize`.
//!
//! The crate calls the application's JavaScript implementations of the
//! component's exports, and provides the component's imports to JavaScript as
//! ES modules. Lifting converts a WIT value into a JavaScript value, and
//! lowering converts a JavaScript value into a WIT value.
//!
//! The conversions in [`value`] work on [`TypeShape`], a description of WIT
//! types that doesn't depend on `wit-dylib-ffi`, so they can be tested
//! natively. The modules that use `wit-dylib-ffi` compile only for wasm.

pub mod component_error;
pub mod exports;
pub mod imports;
pub mod init;
#[cfg(target_arch = "wasm32")]
pub mod interpreter;
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

pub use component_error::{
    component_error_payload, create_error_class, error_class, ComponentError, ErrorClass,
};
pub use exports::{
    call_export, install_table, resolve_export, ExportSpec, ExportTable, Outcome, Resolved,
    ResultShape,
};
pub use imports::{
    call_import_from_js, call_import_from_js_async, synthesize_import_modules, EnumDesc,
    ImportFuncDesc, ImportInvoker, InterfaceDesc, ResourceTypeDesc, WorldDesc,
};
pub use init::{initialize_runtime, shutdown_runtime, with_main_scope, with_scope, Initialized};
pub use resources::{ExportedResources, ResourceDesc};
pub use stack::CallStack;
pub use tracing::install_tracer;
pub use value::TypeShape;

/// Install [`ComponentError`] on `global`, and register the internal classes
/// of the crate: the wrapper class of imported resources and, on wasm, the
/// classes used to implement `stream` and `future` values.
pub fn add_to_global(scope: &js::gc::scope::Scope<'_>, global: js::Object<'_>) {
    ComponentError::add_to_global(scope, global);
    component_error::name_prototype(scope);
    component_error::install_class_factory(scope)
        .expect("creating the error class factory can only fail due to OOM");
    resources::ImportedResource::add_to_global(scope, global);
    #[cfg(target_arch = "wasm32")]
    {
        readable::IncomingStream::add_to_global(scope, global);
        promises::FutureWrite::add_to_global(scope, global);
        promises::FutureRead::add_to_global(scope, global);
    }
}
