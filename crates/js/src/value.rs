// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! JS value creation and inspection.
//!
//! This module provides ergonomic constructors for SpiderMonkey's [`JSVal`]
//! type. It does **not** define a new wrapper type — it works directly with the
//! existing [`JSVal`] and [`HandleValue`] / [`MutableHandleValue`] types.
//!
//! For type conversions between Rust and JS values, see [`crate::conversion`].
//!
//! # Creating values
//!
//! ```ignore
//! use crate::value;
//!
//! let v = value::undefined();
//! let v = value::null();
//! let v = value::from_bool(true);
//! let v = value::from_i32(42);
//! let v = value::from_f64(3.14);
//! ```
//!
//! # Inspecting values
//!
//! The type-checking methods live directly on [`JSVal`] (e.g., `val.is_int32()`,
//! `val.to_int32()`).

use std::hint::cold_path;

use mozjs::gc::{Handle, HandleFunction};
use mozjs::jsapi::{BigInt, JSObject, JSString, Symbol};
use mozjs::jsval::{
    BigIntValue, BooleanValue, DoubleValue, Int32Value, JSVal, NullValue, ObjectOrNullValue,
    ObjectValue, PrivateValue, StringValue, SymbolValue, UInt32Value, UndefinedValue,
};

use crate::conversion::ToJSVal;
use crate::error::ExnThrown;
pub use mozjs::jsval::JSVal as Value;
use mozjs_sys::jsapi::JSFunction;

/// Create an `undefined` value.
#[inline]
pub fn undefined() -> JSVal {
    UndefinedValue()
}

/// Create a `null` value.
#[inline]
pub fn null() -> JSVal {
    NullValue()
}

/// Create a boolean value.
#[inline]
pub fn from_bool(b: bool) -> JSVal {
    BooleanValue(b)
}

/// Create an `int32` value.
#[inline]
pub fn from_i32(i: i32) -> JSVal {
    Int32Value(i)
}

/// Create a numeric value from a `u32`.
///
/// If the value fits in an `int32`, an `int32` value is produced; otherwise a
/// `double`.
#[inline]
pub fn from_u32(u: u32) -> JSVal {
    UInt32Value(u)
}

/// Create a `double` value.
///
/// NaNs are canonicalized first (matching `JS::CanonicalizedDoubleValue`):
/// the engine reserves non-canonical NaN bit patterns for value tagging, so
/// a payload-carrying NaN, e.g. read from raw `Float64Array` bits, would
/// otherwise fail `DoubleValue`'s bit-pattern assertion and abort.
#[inline]
pub fn from_f64(f: f64) -> JSVal {
    let f = if f.is_nan() {
        cold_path();
        // SpiderMonkey's canonical NaN: positive quiet NaN with zero payload.
        const CANONICAL_NAN: f64 = f64::from_bits(0x7FF8_0000_0000_0000);
        CANONICAL_NAN
    } else {
        f
    };
    DoubleValue(f)
}

/// Create an object value, or `null` if the pointer is null.
///
/// # Safety
///
/// `obj` must be either null or a valid, rooted `JSObject` pointer.
#[inline]
pub unsafe fn from_object_or_null(obj: *mut JSObject) -> JSVal {
    ObjectOrNullValue(obj)
}

/// Create an object value.
///
/// # Safety
///
/// `obj` must be a valid, rooted, non-null `JSObject` pointer.
#[inline]
pub unsafe fn from_object(obj: *mut JSObject) -> JSVal {
    ObjectValue(obj)
}

/// Create a value from a rooted `BigInt` handle.
#[inline]
pub fn from_bigint(bi: Handle<*mut BigInt>) -> JSVal {
    // SAFETY: the `BigInt` is rooted via the handle, so the reference is valid
    // for the duration of this call.
    unsafe { BigIntValue(&*bi.get()) }
}

/// Create a value from a rooted symbol handle.
#[inline]
pub fn from_symbol(symbol: Handle<*mut Symbol>) -> JSVal {
    // SAFETY: the symbol is rooted via the handle, so the reference is valid for the duration of
    // this call.
    unsafe { SymbolValue(&*symbol.get()) }
}

/// Create a value from a rooted function handle.
///
/// This is safe because the function is already rooted via the handle.
#[inline]
pub fn from_function(fun: HandleFunction) -> JSVal {
    // SAFETY: The function is rooted via the handle. JS_GetFunctionObject
    // returns a non-null pointer for any valid JSFunction.
    unsafe { from_raw_function(fun.get()) }
}

/// Create a value from a raw function pointer.
///
/// # Safety
///
/// `fun` must be a valid, non-null, rooted `JSFunction` pointer.
#[inline]
pub unsafe fn from_raw_function(fun: *mut JSFunction) -> JSVal {
    ObjectValue(mozjs::jsapi::JS_GetFunctionObject(fun))
}

/// Create a private value from a pointer.
///
/// Private values store opaque pointers in JS values. The pointer is not
/// traced by the GC — this is for storing Rust data, not JS objects.
///
/// # Safety
///
/// `ptr` can be any pointer; it will be stored opaquely. The caller must
/// manage the pointer's lifetime independently of the GC.
#[inline]
pub unsafe fn from_private(ptr: *const std::ffi::c_void) -> JSVal {
    PrivateValue(ptr)
}

/// Create a JS string value from a raw `JSString` pointer.
///
/// # Safety
///
/// `s` must be a valid, non-null, rooted `JSString` pointer.
#[inline]
pub unsafe fn from_string_raw(s: *mut JSString) -> JSVal {
    StringValue(&*s)
}

// ---------------------------------------------------------------------------
// Rooted HandleValueArray backing
// ---------------------------------------------------------------------------

/// Rooted backing buffer for a [`HandleValueArray`].
///
/// `Function::call*` and `Array::with_contents` take `&[impl ToJSVal]` and
/// convert the values in here. The buffer is rooted before the first
/// conversion, so a conversion that allocates cannot invalidate the values
/// converted before it. The values are traced for as long as the rooted guard
/// lives.
///
/// ```ignore
/// let mut args_root = ValueArrayRooter::new(args.len());
/// let args = args_root.root(scope, args)?;
/// some_jsapi_call(scope.cx_mut(), &args.handles());
/// ```
pub(crate) struct ValueArrayRooter<A: smallvec::Array<Item = JSVal> = [JSVal; INLINE_VALUES]>(
    mozjs::gc::CustomAutoRooter<ValueBuffer<A>>,
);

/// The values a [`ValueArrayRooter`] holds. Up to the length of `A` live inline, on the stack.
pub(crate) struct ValueBuffer<A: smallvec::Array<Item = JSVal>>(smallvec::SmallVec<A>);

/// How many values a [`ValueArrayRooter`] of the default type holds without allocating.
const INLINE_VALUES: usize = 8;

// SAFETY: every element is a `JSVal`, and `trace` is only invoked with an engine provided tracer.
unsafe impl<A: smallvec::Array<Item = JSVal>> mozjs::gc::CustomTrace for ValueBuffer<A> {
    fn trace(&self, trc: *mut mozjs::jsapi::JSTracer) {
        for value in &self.0 {
            value.trace(trc);
        }
    }
}

impl ValueArrayRooter {
    pub(crate) fn new(capacity: usize) -> Self {
        Self::with_capacity(capacity)
    }
}

impl<A: smallvec::Array<Item = JSVal>> ValueArrayRooter<A> {
    /// A rooter for `capacity` values, which holds up to the length of `A` inline.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self(mozjs::gc::CustomAutoRooter::new(ValueBuffer(
            smallvec::SmallVec::with_capacity(capacity),
        )))
    }

    pub(crate) fn root<'a, 's>(
        &'a mut self,
        scope: &'s crate::gc::scope::Scope<'_>,
        values: &[impl ToJSVal<'s>],
    ) -> Result<RootedValueArray<'a, A>, ExnThrown> {
        // SAFETY: adding the rooter to the root stack performs no GC.
        let mut rooted = RootedValueArray(self.0.root(unsafe { scope.raw_cx_no_gc() }));
        for v in values {
            rooted.0 .0.push(v.to_jsval_raw_throwing(scope)?);
        }
        Ok(rooted)
    }
}

/// A rooted view over a [`ValueArrayRooter`]'s values; see there.
pub(crate) struct RootedValueArray<'a, A: smallvec::Array<Item = JSVal> = [JSVal; INLINE_VALUES]>(
    mozjs::gc::CustomAutoRooterGuard<'a, ValueBuffer<A>>,
);

impl<A: smallvec::Array<Item = JSVal>> RootedValueArray<'_, A> {
    pub(crate) fn handles(&self) -> mozjs::jsapi::HandleValueArray {
        mozjs::jsapi::HandleValueArray {
            length_: self.0 .0.len(),
            elements_: self.0 .0.as_ptr(),
        }
    }
}
