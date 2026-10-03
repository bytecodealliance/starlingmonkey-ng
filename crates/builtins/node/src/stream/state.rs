// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! High water marks: Node's `lib/internal/streams/state.js`.

use std::cell::Cell;
use std::ffi::CStr;

use js::gc::scope::Scope;
use js::Object;

use super::util::Props;
use crate::errors::{NodeError, Thrown};

js::instance_local! {
    /// The default high water mark of byte streams, which `setDefaultHighWaterMark` changes.
    static DEFAULT_BYTES: Cell<f64> = const { Cell::new(if cfg!(windows) { 16384.0 } else { 65536.0 }) };

    /// The default high water mark of object-mode streams.
    static DEFAULT_OBJECTS: Cell<f64> = const { Cell::new(16.0) };
}

/// The default high water mark for object-mode streams if `object_mode`, and byte streams
/// otherwise.
pub(crate) fn default_high_water_mark(object_mode: bool) -> f64 {
    if object_mode {
        DEFAULT_OBJECTS.with(Cell::get)
    } else {
        DEFAULT_BYTES.with(Cell::get)
    }
}

/// Set the default high water mark for object-mode streams if `object_mode`, and byte streams
/// otherwise.
pub(crate) fn set_default_high_water_mark(object_mode: bool, value: f64) {
    if object_mode {
        DEFAULT_OBJECTS.with(|cell| cell.set(value));
    } else {
        DEFAULT_BYTES.with(|cell| cell.set(value));
    }
}

/// Node's `getHighWaterMark`: `options.highWaterMark`, else `options[duplex_key]` for a duplex
/// stream, else the default for `object_mode`. A value that isn't a non-negative integer is
/// `ERR_INVALID_ARG_VALUE` naming `options[duplex_key]` for a duplex stream and
/// `options.highWaterMark` otherwise.
pub(crate) fn high_water_mark(
    scope: &Scope<'_>,
    options: Object<'_>,
    duplex_key: &'static CStr,
    is_duplex: bool,
    object_mode: bool,
) -> Result<f64, Thrown> {
    let mut value = options.val(scope, c"highWaterMark")?;
    if (value.is_null() || value.is_undefined()) && is_duplex {
        value = options.val(scope, duplex_key)?;
    }
    let name = if is_duplex {
        format!("options.{}", duplex_key.to_str().unwrap_or_default())
    } else {
        "options.highWaterMark".to_string()
    };
    if value.is_null() || value.is_undefined() {
        return Ok(default_high_water_mark(object_mode));
    }
    let number = value.is_number().then(|| value.to_number());
    match number {
        Some(number) if number >= 0.0 && number.fract() == 0.0 && number.is_finite() => Ok(number),
        _ => Err(NodeError::invalid_arg_value(scope, &name, value, "is invalid").into()),
    }
}

/// The high water mark passed to `setDefaultHighWaterMark`, validated as Node's
/// `validateInteger(value, 'value', 0)` does.
pub(crate) fn validate_default(
    scope: &Scope<'_>,
    value: js::prelude::HandleValue<'_>,
) -> Result<f64, Thrown> {
    crate::errors::validate_integer(scope, value, "value", 0.0, 9007199254740991.0)
}
