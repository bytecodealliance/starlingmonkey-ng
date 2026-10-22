// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Convenient re-exports of the most commonly used types and traits.
//!
//! ```ignore
//! use crate::prelude::*;
//! ```

pub use super::error::{CapturedError, ExnThrown};

// Re-export the conversion traits.
pub use super::conversion::{ConversionError, FromJSVal, ToJSVal};

// Re-export the scope-based rooting types.
pub use crate::gc::handle::OptionHeapExt;
pub use crate::gc::scope::{InnerScope, RootScope, Scope};

// Re-export builtin type-checking and conversion traits.
pub use super::builtins::IsPrimitive;

// Re-export the rooting types since they are essential.
pub use mozjs::gc::{
    Handle, HandleFunction, HandleObject, HandleScript, HandleString, HandleSymbol, HandleValue,
    MutableHandle, RootedGuard,
};
pub use mozjs::jsval::JSVal;
pub use mozjs::rooted;

// Re-export the exception handling scope.
pub use super::try_catch::TryCatch;

// Re-export the callback args type for closure-based callbacks.
pub use super::function::CallbackArgs;
