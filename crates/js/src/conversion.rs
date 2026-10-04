/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Conversions of Rust values to and from `JSVal`, adapted from
//! mozjs.
//!
//! | IDL type                | Type                             |
//! |-------------------------|----------------------------------|
//! | any                     | `JSVal`                          |
//! | boolean                 | `bool`                           |
//! | byte                    | `i8`                             |
//! | octet                   | `u8`                             |
//! | short                   | `i16`                            |
//! | unsigned short          | `u16`                            |
//! | long                    | `i32`                            |
//! | unsigned long           | `u32`                            |
//! | long long               | `i64`                            |
//! | unsigned long long      | `u64`                            |
//! | unrestricted float      | `f32`                            |
//! | float                   | `Finite<f32>`                    |
//! | unrestricted double     | `f64`                            |
//! | double                  | `Finite<f64>`                    |
//! | USVString               | `String`                         |
//! | object                  | `*mut JSObject`                  |
//! | symbol                  | `*mut Symbol`                    |
//! | nullable types          | `Option<T>`                      |
//! | sequences               | `Vec<T>`                         |

#![deny(missing_docs)]

use mozjs::jsapi::JS;
use mozjs::jsapi::JSPROP_ENUMERATE;
use mozjs::jsapi::{ForOfIterator, ForOfIterator_NonIterableBehavior};
use mozjs::jsapi::{JSString, PropertyDescriptor, RootedObject, RootedValue};
use mozjs::jsval::{
    BooleanValue, DoubleValue, Int32Value, JSVal, StringValue, UInt32Value, UndefinedValue,
};
use mozjs::rooted;
use mozjs::rust::{
    HandleValue, ToBoolean, ToInt32, ToInt64, ToNumber, ToString, ToUint16, ToUint32, ToUint64,
};
use mozjs_sys::jsgc::Rooted;
use num_traits::PrimInt;
use std::borrow::Cow;
use std::ffi::CStr;
use std::ptr::NonNull;
use std::rc::Rc;
use std::{mem, ptr};

use crate::error::{throw_type_error, ExnThrown, ThrowException};
use crate::gc::handle::Heap;
use crate::heap::{MozHeap, Trace};
use crate::prelude::Scope;
use crate::value;

pub use indexmap::IndexMap;

trait As<O>: Copy {
    fn cast(self) -> O;
}

macro_rules! impl_as {
    ($I:ty, $O:ty) => {
        impl As<$O> for $I {
            fn cast(self) -> $O {
                self as $O
            }
        }
    };
}

impl_as!(f64, u8);
impl_as!(f64, u16);
impl_as!(f64, u32);
impl_as!(f64, u64);
impl_as!(f64, i8);
impl_as!(f64, i16);
impl_as!(f64, i32);
impl_as!(f64, i64);

impl_as!(u8, f64);
impl_as!(u16, f64);
impl_as!(u32, f64);
impl_as!(u64, f64);
impl_as!(i8, f64);
impl_as!(i16, f64);
impl_as!(i32, f64);
impl_as!(i64, f64);

impl_as!(i32, i8);
impl_as!(i32, u8);
impl_as!(i32, i16);
impl_as!(u16, u16);
impl_as!(i32, i32);
impl_as!(u32, u32);
impl_as!(i64, i64);
impl_as!(u64, u64);

/// Similar to num_traits, but we need to be able to customize values
pub trait Number {
    /// Zero value of this type
    const ZERO: Self;
    /// Smallest finite number this type can represent
    const MIN: Self;
    /// Largest finite number this type can represent
    const MAX: Self;
}

macro_rules! impl_num {
    ($N:ty, $zero:expr, $min:expr, $max:expr) => {
        impl Number for $N {
            const ZERO: $N = $zero;
            const MIN: $N = $min;
            const MAX: $N = $max;
        }
    };
}

// lower upper bound per: https://webidl.spec.whatwg.org/#abstract-opdef-converttoint
impl_num!(u8, 0, u8::MIN, u8::MAX);
impl_num!(u16, 0, u16::MIN, u16::MAX);
impl_num!(u32, 0, u32::MIN, u32::MAX);
impl_num!(u64, 0, 0, (1 << 53) - 1);

impl_num!(i8, 0, i8::MIN, i8::MAX);
impl_num!(i16, 0, i16::MIN, i16::MAX);
impl_num!(i32, 0, i32::MIN, i32::MAX);
impl_num!(i64, 0, -(1 << 53) + 1, (1 << 53) - 1);

impl_num!(f32, 0.0, f32::MIN, f32::MAX);
impl_num!(f64, 0.0, f64::MIN, f64::MAX);

/// A trait to convert Rust types to `JSVal`s.
pub trait ToJSVal<'s> {
    /// Convert `self` to a rooted `HandleValue`.
    ///
    /// Conversion failure results in `Err(ConversionError)`: either a pending JS exception, or a
    /// type error without a pending exception.
    #[inline]
    fn to_jsval(&self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        Ok(scope.root_value(self.to_jsval_raw(scope)?))
    }

    /// Convert `self` to a rooted `HandleValue`.
    ///
    /// Conversion failure results in a pending exception.
    #[inline]
    fn to_jsval_throwing(&self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
        self.to_jsval(scope).map_err(|e| e.throw(scope))
    }

    /// Convert `self` to a rooted `HandleValue`.
    ///
    /// Conversion failure results in `Err(ConversionError)`: either a pending JS exception, or a
    /// type error without a pending exception.
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError>;

    /// Convert `self` to a rooted `HandleValue`.
    ///
    /// Conversion failure results in a pending exception.
    fn to_jsval_raw_throwing(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ExnThrown> {
        self.to_jsval_raw(scope).map_err(|e| e.throw(scope))
    }
}

/// Error type for conversions of Rust types to `JSVal`s.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum ConversionError {
    /// Conversion failed, resulting in a pending JS exception.
    ExnPending,
    /// Conversion failed, without a pending JS exception.
    Failure(Cow<'static, CStr>),
}

impl ConversionError {
    /// Throw ConversionError::Failure as a JavaScript `TypeError` exception.
    pub fn throw(&self, scope: &Scope<'_>) -> ExnThrown {
        if let ConversionError::Failure(msg) = self {
            return throw_type_error(scope, msg.as_ref());
        }
        ExnThrown
    }
}

impl From<ExnThrown> for ConversionError {
    fn from(_: ExnThrown) -> Self {
        ConversionError::ExnPending
    }
}

impl ThrowException for ConversionError {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        ConversionError::throw(&self, scope)
    }
}

/// A trait to convert `JSVal`s to Rust types.
///
/// The lifetime `'s` ties the scope to the returned value, allowing
/// implementations for scope-rooted types like `Stack<'s, Object>`. The
/// lifetime `'v` is the incoming value handle's lifetime; most implementations
/// root into the scope and are blanket over it (`impl<'s, 'v> FromJSVal<'s,
/// 'v>`). `HandleValue` is the exception: it returns the handle unchanged, so
/// it only implements `FromJSVal<'s, 's>` and callers holding a shorter-lived
/// handle must re-root into the scope first. The `Vec` and `Record` impls do
/// exactly that — they root each element into the scope and convert it at
/// `'v = 's`, so scope-tied element types (`HandleValue`, stack newtypes)
/// work as element types.
///
/// For types that don't borrow from the scope (primitives, `String`, etc.),
/// implement as `impl FromJSVal<'_, '_> for T`.
pub trait FromJSVal<'s, 'v>: Sized {
    /// Optional configurable behaviour switch; use () for no configuration.
    type Config;
    /// Convert `val` to type `Self`.
    /// Optional configuration of type `T` can be passed as the `option`
    /// argument.
    fn from_jsval(
        scope: &'s Scope<'s>,
        val: HandleValue<'v>,
        option: Self::Config,
    ) -> Result<Self, ConversionError>;

    /// Convert `val` to type `Self`.
    /// Optional configuration of type `T` can be passed as the `option`
    /// argument.
    /// Always throws a JS exception on failure.
    fn from_jsval_throwing(
        scope: &'s Scope<'_>,
        val: HandleValue<'v>,
        option: Self::Config,
    ) -> Result<Self, ExnThrown> {
        Self::from_jsval(scope, val, option).map_err(|e| e.throw(scope))
    }
}

/// Behavior for converting out-of-range integers.
#[derive(PartialEq, Eq, Clone)]
pub enum ConversionBehavior {
    /// Wrap into the integer's range.
    Default,
    /// Throw an exception.
    EnforceRange,
    /// Clamp into the integer's range.
    Clamp,
}

/// A WebIDL `[EnforceRange]` integer.
///
/// Converting a value that is not finite, or whose truncation lies outside `T`'s
/// WebIDL range, throws a `TypeError` instead of wrapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EnforceRange<T>(pub T);

impl<'s, 'v, T> FromJSVal<'s, 'v> for EnforceRange<T>
where
    T: FromJSVal<'s, 'v, Config = ConversionBehavior>,
{
    type Config = ();

    fn from_jsval(
        scope: &'s Scope<'s>,
        val: HandleValue<'v>,
        _: (),
    ) -> Result<Self, ConversionError> {
        T::from_jsval(scope, val, ConversionBehavior::EnforceRange).map(EnforceRange)
    }
}

/// A WebIDL `[Clamp]` integer.
///
/// Converting a value outside `T`'s range clamps it into the range, and `NaN` converts to 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clamp<T>(pub T);

impl<'s, 'v, T> FromJSVal<'s, 'v> for Clamp<T>
where
    T: FromJSVal<'s, 'v, Config = ConversionBehavior>,
{
    type Config = ();

    fn from_jsval(
        scope: &'s Scope<'s>,
        val: HandleValue<'v>,
        _: (),
    ) -> Result<Self, ConversionError> {
        T::from_jsval(scope, val, ConversionBehavior::Clamp).map(Clamp)
    }
}

/// Try to cast the number to a smaller type, but
/// if it doesn't fit, it will return an error.
// https://searchfox.org/mozilla-esr128/rev/1aa97f9d67f7a7231e62af283eaa02a6b31380e1/dom/bindings/PrimitiveConversions.h#166
fn enforce_range<D>(scope: &Scope<'_>, d: f64) -> Result<D, ()>
where
    D: Number + As<f64>,
    f64: As<D>,
{
    if d.is_infinite() {
        throw_type_error(scope, c"value out of range in an EnforceRange argument");
        return Err(());
    }

    let rounded = d.signum() * d.abs().floor();
    if D::MIN.cast() <= rounded && rounded <= D::MAX.cast() {
        Ok(rounded.cast())
    } else {
        throw_type_error(scope, c"value out of range in an EnforceRange argument");
        Err(())
    }
}

/// WebIDL ConvertToInt (Clamp) conversion.
/// Spec: <https://webidl.spec.whatwg.org/#abstract-opdef-converttoint>
///
/// This function is ported from Gecko’s
/// [`PrimitiveConversionTraits_Clamp`](https://searchfox.org/firefox-main/rev/aee7c0f24f488cd7f5a835803b48dd0c0cb2fd5f/dom/bindings/PrimitiveConversions.h#226).
///
/// # Warning
/// This function must only be used when the target type `D` represents an
/// integer WebIDL type. Using it with non-integer types would be incorrect.
fn clamp_to<D>(d: f64) -> D
where
    D: Number + PrimInt + As<f64>,
    f64: As<D>,
{
    // NaN maps to zero.
    if d.is_nan() {
        return D::ZERO;
    }

    if d >= D::MAX.cast() {
        return D::MAX;
    }
    if d <= D::MIN.cast() {
        return D::MIN;
    }

    debug_assert!(d.is_finite());

    // Banker's rounding (round ties towards even).
    // We move away from 0 by 0.5 and then truncate. That gets us the right
    // answer for any starting value except plus or minus N.5. With a starting
    // value of that form, we now have plus or minus N+1. If N is odd, this is
    // the correct result. If N is even, plus or minus N is the correct result.
    let to_truncate = if d < 0.0 { d - 0.5 } else { d + 0.5 };

    let mut truncated: D = to_truncate.cast();

    if truncated.cast() == to_truncate {
        // It was a tie (since moving away from 0 by 0.5 gave us the exact integer
        // we want). Since we rounded away from 0, we either already have an even
        // number or we have an odd number but the number we want is one closer to
        // 0. So just unconditionally masking out the ones bit should do the trick
        // to get us the value we want.
        truncated = truncated & !D::one();
    }

    truncated
}

// https://heycam.github.io/webidl/#es-void
impl<'s> ToJSVal<'s> for () {
    #[inline]
    fn to_jsval(&self, _scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        Ok(HandleValue::undefined())
    }
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(value::undefined())
    }
}

impl FromJSVal<'_, '_> for JSVal {
    type Config = ();
    fn from_jsval(
        _scope: &Scope<'_>,
        value: HandleValue,
        _option: (),
    ) -> Result<JSVal, ConversionError> {
        Ok(value.get())
    }
}

impl<'s> ToJSVal<'s> for JSVal {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(*self)
    }
}

impl<'s> FromJSVal<'s, 's> for HandleValue<'s> {
    type Config = ();
    fn from_jsval(
        _scope: &'s Scope<'_>,
        value: HandleValue<'s>,
        _option: (),
    ) -> Result<HandleValue<'s>, ConversionError> {
        Ok(value)
    }
}

impl<'s> ToJSVal<'s> for HandleValue<'s> {
    #[inline]
    fn to_jsval(&self, _scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        Ok(*self)
    }

    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(self.get())
    }
}

impl<'s> ToJSVal<'s> for MozHeap<JSVal> {
    #[inline]
    fn to_jsval(&self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        Ok(scope.root_value(self.get()))
    }

    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(self.get())
    }
}

#[inline]
fn convert_int_from_jsval<T, M>(
    scope: &Scope<'_>,
    value: HandleValue,
    option: ConversionBehavior,
    convert_fn: unsafe fn(&mut mozjs::context::JSContext, HandleValue) -> Result<M, ()>,
) -> Result<T, ConversionError>
where
    T: Number + As<f64> + PrimInt,
    M: Number + As<T>,
    f64: As<T>,
{
    let result = match option {
        ConversionBehavior::Default => {
            unsafe { convert_fn(scope.cx_mut(), value) }.map(|v| v.cast())
        }
        _ => match unsafe { ToNumber(scope.cx_mut(), value) } {
            Ok(num) => {
                if matches!(option, ConversionBehavior::EnforceRange) {
                    enforce_range(scope, num)
                } else {
                    Ok(clamp_to(num))
                }
            }
            Err(()) => Err(()),
        },
    };
    result.map_err(|_| ConversionError::ExnPending)
}

// https://heycam.github.io/webidl/#es-boolean
impl<'s> ToJSVal<'s> for bool {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(BooleanValue(*self))
    }
}

// https://heycam.github.io/webidl/#es-boolean
impl FromJSVal<'_, '_> for bool {
    type Config = ();
    fn from_jsval(
        _scope: &Scope<'_>,
        val: HandleValue,
        _option: (),
    ) -> Result<bool, ConversionError> {
        Ok(to_boolean(val))
    }
}

/// ECMAScript's `ToBoolean`: whether `value` is truthy.
#[inline]
pub fn to_boolean(value: HandleValue) -> bool {
    // SAFETY: `value` is rooted. `ToBoolean` reads it and cannot GC.
    unsafe { ToBoolean(value) }
}

// https://heycam.github.io/webidl/#es-byte
impl<'s> ToJSVal<'s> for i8 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(Int32Value(*self as i32))
    }
}

// https://heycam.github.io/webidl/#es-byte
impl FromJSVal<'_, '_> for i8 {
    type Config = ConversionBehavior;
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        option: ConversionBehavior,
    ) -> Result<i8, ConversionError> {
        convert_int_from_jsval(scope, val, option, ToInt32)
    }
}

// https://heycam.github.io/webidl/#es-octet
impl<'s> ToJSVal<'s> for u8 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(Int32Value(*self as i32))
    }
}

// https://heycam.github.io/webidl/#es-octet
impl FromJSVal<'_, '_> for u8 {
    type Config = ConversionBehavior;
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        option: ConversionBehavior,
    ) -> Result<u8, ConversionError> {
        convert_int_from_jsval(scope, val, option, ToInt32)
    }
}

// https://heycam.github.io/webidl/#es-short
impl<'s> ToJSVal<'s> for i16 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(Int32Value(*self as i32))
    }
}

// https://heycam.github.io/webidl/#es-short
impl FromJSVal<'_, '_> for i16 {
    type Config = ConversionBehavior;
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        option: ConversionBehavior,
    ) -> Result<i16, ConversionError> {
        convert_int_from_jsval(scope, val, option, ToInt32)
    }
}

// https://heycam.github.io/webidl/#es-unsigned-short
impl<'s> ToJSVal<'s> for u16 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(Int32Value(*self as i32))
    }
}

// https://heycam.github.io/webidl/#es-unsigned-short
impl FromJSVal<'_, '_> for u16 {
    type Config = ConversionBehavior;
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        option: ConversionBehavior,
    ) -> Result<u16, ConversionError> {
        convert_int_from_jsval(scope, val, option, ToUint16)
    }
}

// https://heycam.github.io/webidl/#es-long
impl<'s> ToJSVal<'s> for i32 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(Int32Value(*self))
    }
}

// https://heycam.github.io/webidl/#es-long
impl FromJSVal<'_, '_> for i32 {
    type Config = ConversionBehavior;
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        option: ConversionBehavior,
    ) -> Result<i32, ConversionError> {
        convert_int_from_jsval(scope, val, option, ToInt32)
    }
}

// https://heycam.github.io/webidl/#es-unsigned-long
impl<'s> ToJSVal<'s> for u32 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(UInt32Value(*self))
    }
}

// https://heycam.github.io/webidl/#es-unsigned-long
impl FromJSVal<'_, '_> for u32 {
    type Config = ConversionBehavior;
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        option: ConversionBehavior,
    ) -> Result<u32, ConversionError> {
        convert_int_from_jsval(scope, val, option, ToUint32)
    }
}

// https://heycam.github.io/webidl/#es-long-long
impl<'s> ToJSVal<'s> for i64 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(DoubleValue(*self as f64))
    }
}

// https://heycam.github.io/webidl/#es-long-long
impl FromJSVal<'_, '_> for i64 {
    type Config = ConversionBehavior;
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        option: ConversionBehavior,
    ) -> Result<i64, ConversionError> {
        convert_int_from_jsval(scope, val, option, ToInt64)
    }
}

// https://heycam.github.io/webidl/#es-unsigned-long-long
impl<'s> ToJSVal<'s> for u64 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        Ok(DoubleValue(*self as f64))
    }
}

// https://heycam.github.io/webidl/#es-unsigned-long-long
impl FromJSVal<'_, '_> for u64 {
    type Config = ConversionBehavior;
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        option: ConversionBehavior,
    ) -> Result<u64, ConversionError> {
        convert_int_from_jsval(scope, val, option, ToUint64)
    }
}

// https://heycam.github.io/webidl/#es-float
impl<'s> ToJSVal<'s> for f32 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        // from_f64 canonicalizes NaNs; a payload-carrying NaN would trip
        // DoubleValue's bit-pattern assertion and abort.
        Ok(crate::value::from_f64(*self as f64))
    }
}

// https://heycam.github.io/webidl/#es-float
impl FromJSVal<'_, '_> for f32 {
    type Config = ();
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        _option: (),
    ) -> Result<f32, ConversionError> {
        match unsafe { ToNumber(scope.cx_mut(), val) } {
            Ok(result) => Ok(result as f32),
            Err(_) => Err(ConversionError::ExnPending),
        }
    }
}

// https://heycam.github.io/webidl/#es-double
impl<'s> ToJSVal<'s> for f64 {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'_ Scope<'_>) -> Result<JS::Value, ConversionError> {
        // from_f64 canonicalizes NaNs; a payload-carrying NaN would trip
        // DoubleValue's bit-pattern assertion and abort.
        Ok(crate::value::from_f64(*self))
    }
}

// https://heycam.github.io/webidl/#es-double
impl FromJSVal<'_, '_> for f64 {
    type Config = ();
    fn from_jsval(
        scope: &Scope<'_>,
        val: HandleValue,
        _option: (),
    ) -> Result<f64, ConversionError> {
        match unsafe { ToNumber(scope.cx_mut(), val) } {
            Ok(result) => Ok(result),
            Err(_) => Err(ConversionError::ExnPending),
        }
    }
}

/// A finite floating-point number: the *restricted* WebIDL `float`/`double`
/// types, which reject NaN and ±Infinity during conversion (WebIDL
/// [§3.2.13](https://webidl.spec.whatwg.org/#es-float) and
/// [§3.2.15](https://webidl.spec.whatwg.org/#es-double)). Use plain
/// `f32`/`f64` for the unrestricted variants.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Finite<T>(T);

impl<T: num_traits::Float> Finite<T> {
    /// Wrap `value`, returning `None` if it is NaN or infinite.
    pub fn new(value: T) -> Option<Self> {
        value.is_finite().then_some(Finite(value))
    }
}

impl<T: Copy> Finite<T> {
    /// The wrapped value.
    pub fn get(self) -> T {
        self.0
    }
}

impl<T> std::ops::Deref for Finite<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

// https://webidl.spec.whatwg.org/#es-float
impl FromJSVal<'_, '_> for Finite<f32> {
    type Config = ();
    fn from_jsval(scope: &Scope<'_>, val: HandleValue, _: ()) -> Result<Self, ConversionError> {
        // Step 1-2: Let x be ? ToNumber(V); reject non-finite x.
        let x = f64::from_jsval(scope, val, ())?;
        if !x.is_finite() {
            throw_type_error(scope, c"value is not a finite floating-point value");
            return Err(ConversionError::ExnPending);
        }
        // Step 3-4: Round to IEEE single precision; reject overflow to
        // infinity.
        let y = x as f32;
        Finite::new(y).ok_or_else(|| {
            throw_type_error(scope, c"value is out of range for a float");
            ConversionError::ExnPending
        })
    }
}

// https://webidl.spec.whatwg.org/#es-double
impl FromJSVal<'_, '_> for Finite<f64> {
    type Config = ();
    fn from_jsval(scope: &Scope<'_>, val: HandleValue, _: ()) -> Result<Self, ConversionError> {
        // Step 1-2: Let x be ? ToNumber(V); reject non-finite x.
        let x = f64::from_jsval(scope, val, ())?;
        Finite::new(x).ok_or_else(|| {
            throw_type_error(scope, c"value is not a finite floating-point value");
            ConversionError::ExnPending
        })
    }
}

// https://webidl.spec.whatwg.org/#es-float
impl<'s> ToJSVal<'s> for Finite<f32> {
    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        self.0.to_jsval_raw(scope)
    }
}

// https://webidl.spec.whatwg.org/#es-double
impl<'s> ToJSVal<'s> for Finite<f64> {
    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        self.0.to_jsval_raw(scope)
    }
}

/// Converts a `JSString` into a `String`, regardless of used encoding.
pub fn jsstr_to_string(scope: &Scope<'_>, jsstr: NonNull<JSString>) -> String {
    let jsstr = jsstr.as_ptr();
    let mut length = 0;
    // SAFETY: `jsstr` is a live string. The accessors flatten a rope and return its characters,
    // which stay in place until the next call into SpiderMonkey, and only `encoding_rs` reads them
    // before then. Each conversion writes valid UTF-8 into the prefix it reports.
    unsafe {
        if mozjs::jsapi::JS_DeprecatedStringHasLatin1Chars(jsstr) {
            let chars = mozjs::rust::wrappers2::JS_GetLatin1StringCharsAndLength(
                scope.cx(),
                jsstr,
                &mut length,
            );
            assert!(!chars.is_null());
            let chars = std::slice::from_raw_parts(chars, length);
            // An ASCII prefix is copied as is, and each character after it takes at most two
            // bytes, the size `convert_latin1_to_utf8` requires.
            let ascii = encoding_rs::Encoding::ascii_valid_up_to(chars);
            if ascii == length {
                return String::from_utf8_unchecked(chars.to_vec());
            }
            let mut utf8 = Vec::with_capacity(ascii + (length - ascii) * 2);
            utf8.extend_from_slice(&chars[..ascii]);
            utf8.resize(utf8.capacity(), 0);
            let written =
                encoding_rs::mem::convert_latin1_to_utf8(&chars[ascii..], &mut utf8[ascii..]);
            utf8.truncate(ascii + written);
            String::from_utf8_unchecked(utf8)
        } else {
            let chars = mozjs::rust::wrappers2::JS_GetTwoByteStringCharsAndLength(
                scope.cx(),
                jsstr,
                &mut length,
            );
            assert!(!chars.is_null());
            let chars = std::slice::from_raw_parts(chars, length);
            // `convert_utf16_to_utf8` requires three bytes per code unit, and replaces unpaired
            // surrogates with U+FFFD.
            let mut utf8 = vec![0; length * 3];
            let written = encoding_rs::mem::convert_utf16_to_utf8(chars, &mut utf8);
            utf8.truncate(written);
            String::from_utf8_unchecked(utf8)
        }
    }
}

// https://heycam.github.io/webidl/#es-USVString
impl<'s> ToJSVal<'s> for str {
    #[inline]
    #[deny(unsafe_op_in_unsafe_fn)]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        let jsstr = crate::string::new_js_string(scope, self);
        if jsstr.is_null() {
            return Err(ConversionError::ExnPending);
        }
        Ok(StringValue(unsafe { &*jsstr }))
    }
}

// https://heycam.github.io/webidl/#es-USVString
impl<'s> ToJSVal<'s> for String {
    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        self.as_str().to_jsval_raw(scope)
    }
}

// https://heycam.github.io/webidl/#es-USVString
impl FromJSVal<'_, '_> for String {
    type Config = ();
    fn from_jsval(scope: &Scope<'_>, val: HandleValue, _: ()) -> Result<String, ConversionError> {
        let jsstr = unsafe { ToString(scope.cx_mut(), val) };
        let Some(jsstr) = NonNull::new(jsstr) else {
            return Err(ConversionError::ExnPending);
        };
        Ok(jsstr_to_string(scope, jsstr))
    }
}

impl<'s, T: ToJSVal<'s>> ToJSVal<'s> for Option<T> {
    #[inline]
    fn to_jsval(&self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        match self {
            Some(value) => value.to_jsval(scope),
            None => Ok(HandleValue::null()),
        }
    }

    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        match self {
            Some(value) => value.to_jsval_raw(scope),
            None => Ok(value::null()),
        }
    }
}

impl<'s, 'v, T: FromJSVal<'s, 'v>> FromJSVal<'s, 'v> for Option<T> {
    type Config = T::Config;
    fn from_jsval(
        scope: &'s Scope<'_>,
        val: HandleValue<'v>,
        option: T::Config,
    ) -> Result<Option<T>, ConversionError> {
        if val.get().is_null_or_undefined() {
            Ok(None)
        } else {
            FromJSVal::from_jsval(scope, val, option).map(Some)
        }
    }
}

impl<'s, T: ToJSVal<'s> + ?Sized> ToJSVal<'s> for &'_ T {
    #[inline]
    fn to_jsval(&self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        (**self).to_jsval(scope)
    }

    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        (**self).to_jsval_raw(scope)
    }
}

impl<'s, T: ToJSVal<'s>> ToJSVal<'s> for Box<T> {
    #[inline]
    fn to_jsval(&self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        (**self).to_jsval(scope)
    }

    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        (**self).to_jsval_raw(scope)
    }
}

impl<'s, T: ToJSVal<'s>> ToJSVal<'s> for Rc<T> {
    #[inline]
    fn to_jsval(&self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        (**self).to_jsval(scope)
    }

    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        (**self).to_jsval_raw(scope)
    }
}

// https://heycam.github.io/webidl/#es-sequence
impl<'s, T: ToJSVal<'s>> ToJSVal<'s> for [T] {
    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        let array =
            crate::Array::new(scope, self.len()).map_err(|_| ConversionError::ExnPending)?;

        for (index, obj) in self.iter().enumerate() {
            // TODO: this would be much better to do with a reused rooted value,
            //       which we don't currently have an API for.
            let val = obj.to_jsval(scope)?;

            array.define_element(scope, index as u32, val, JSPROP_ENUMERATE as u32)?;
        }

        Ok(array.as_value())
    }

    #[inline]
    fn to_jsval(&self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        Ok(scope.root_value(self.to_jsval_raw(scope)?))
    }
}

// https://heycam.github.io/webidl/#es-sequence
impl<'s, T: ToJSVal<'s>> ToJSVal<'s> for Vec<T> {
    #[inline]
    fn to_jsval(&self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ConversionError> {
        self.as_slice().to_jsval(scope)
    }
    #[inline]
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        self.as_slice().to_jsval_raw(scope)
    }
}

/// Returns `true` if `val` is an object with a non-null `@@iterator` method.
///
/// This is the WebIDL §3.2.25 distinguishability check used when a union type
/// includes a sequence type: when the input has `@@iterator`, sequence
/// conversion is selected.
///
/// Returns `Ok(false)` for non-objects. Returns `Err(ExnPending)` if reading
/// the `@@iterator` property throws.
pub fn is_iterable_value(scope: &Scope<'_>, val: HandleValue<'_>) -> Result<bool, ConversionError> {
    let Ok(obj) = crate::Object::from_value(scope, val) else {
        return Ok(false);
    };
    let iter_key = crate::symbol::get_well_known_key(scope, crate::native::SymbolCode::iterator);
    let iter_id = scope.root_id(iter_key);
    let method = obj
        .get_property_by_id(scope, iter_id)
        .map_err(|_| ConversionError::ExnPending)?;
    Ok(!method.is_null_or_undefined())
}

/// Rooting guard for a [`ForOfIterator`].
///
/// Behaves like `RootedGuard` (roots on creation, unroots on drop), but borrows
/// the whole `ForOfIterator` so its methods remain usable through `root`. Both
/// of the struct's `Rooted` fields are registered, and the borrow keeps the
/// struct pinned for as long as the guard lives — a registered `Rooted` records
/// its own address on the context root stack and must not move.
///
/// This type is deliberately private: the only sound way to obtain a rooted,
/// initialized iterator is [`for_of`], which owns the backing storage.
struct ForOfIteratorGuard<'s> {
    root: &'s mut ForOfIterator,
}

impl<'s> ForOfIteratorGuard<'s> {
    fn new(scope: &'s Scope<'_>, root: &'s mut ForOfIterator) -> Self {
        let cx = unsafe { scope.cx_mut().raw_cx() };
        // SpiderMonkey's `ForOfIterator` declares both `iterator` and
        // `nextMethod` as `Rooted` members. We build the struct by hand, so
        // both fields must be registered on the context's root stack;
        // otherwise a moving GC between `init` and `next` leaves `nextMethod`
        // pointing at a forwarded cell.
        unsafe {
            Rooted::add_to_root_stack(&raw mut root.iterator, cx);
            Rooted::add_to_root_stack(&raw mut root.nextMethod, cx);
        }
        ForOfIteratorGuard { root }
    }
}

impl<'s> Drop for ForOfIteratorGuard<'s> {
    fn drop(&mut self) {
        unsafe {
            self.root.nextMethod.remove_from_root_stack();
            self.root.iterator.remove_from_root_stack();
        }
    }
}

/// Build the backing [`ForOfIterator`] in an inert, unrooted state.
///
/// The result must be pinned (bound to a stack slot and rooted via
/// [`ForOfIteratorGuard`]) before any of its methods are called.
fn for_of_iterator_slot(scope: &Scope<'_>) -> ForOfIterator {
    // Depending on the LLVM version, bindgen may add a trailing padding field
    // to `ForOfIterator`. Start from a zeroed instance and assign the named
    // fields, so any such padding stays zero without being named explicitly.
    let mut it: ForOfIterator = unsafe { mem::zeroed() };
    it.cx_ = unsafe { scope.cx_mut().raw_cx() };
    it.iterator = RootedObject::new_unrooted(ptr::null_mut());
    it.nextMethod = RootedValue::new_unrooted(JSVal { asBits_: 0 });
    it.index = u32::MAX; // NOT_ARRAY
    it
}

/// Drive the ES `for...of` protocol over `value`, calling `f` for each element.
///
/// This owns the backing [`ForOfIterator`], so it pins and roots it correctly
/// for the whole iteration; callers never touch the raw rooting machinery and
/// cannot observe a half-rooted iterator. SpiderMonkey's array fast path is
/// preserved, since the engine's own iterator is used.
///
/// Returns `Ok(true)` once iteration completes, or `Ok(false)` if `value` is
/// not iterable. `f` receives a handle to a value slot that is reused across
/// iterations, so it must consume each element before returning. The handle is
/// passed at a fresh lifetime, so the compiler rejects any attempt to retain
/// it past the callback; to collect elements, re-root each one via
/// [`Scope::root_value`] first, as [`Vec`]'s `FromJSVal` impl does:
///
/// ```compile_fail
/// use js::conversion::{for_of, ConversionError};
/// use js::prelude::{HandleValue, Scope};
///
/// fn collect<'s>(scope: &'s Scope<'s>, val: HandleValue<'s>) {
///     let mut kept: Vec<HandleValue<'s>> = vec![];
///     let _ = for_of(scope, val, |elem| {
///         kept.push(elem); // ERROR: `elem` escapes the callback
///         Ok::<_, ConversionError>(())
///     });
/// }
/// ```
pub fn for_of<E: From<ConversionError>>(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    mut f: impl FnMut(HandleValue<'_>) -> Result<(), E>,
) -> Result<bool, E> {
    let mut slot = for_of_iterator_slot(scope);
    let guard = ForOfIteratorGuard::new(scope, &mut slot);

    if !unsafe {
        guard.root.init(
            value.into(),
            ForOfIterator_NonIterableBehavior::AllowNonIterable,
        )
    } {
        return Err(ConversionError::ExnPending.into());
    }

    if guard.root.iterator.data.is_null() {
        return Ok(false);
    }

    let mut out = scope.root_value_mut(UndefinedValue());
    loop {
        let mut done = false;
        if !unsafe { guard.root.next(out.reborrow().into(), &mut done) } {
            return Err(ConversionError::ExnPending.into());
        }
        if done {
            return Ok(true);
        }
        f(out.handle())?;
    }
}

impl<'s, 'v, C: Clone, T: FromJSVal<'s, 's, Config = C>> FromJSVal<'s, 'v> for Vec<T> {
    type Config = C;

    fn from_jsval(
        scope: &'s Scope<'s>,
        val: HandleValue<'v>,
        option: C,
    ) -> Result<Vec<T>, ConversionError> {
        if !val.is_object() {
            return Err(ConversionError::Failure(c"Value is not an object".into()));
        }

        let mut ret = vec![];
        let iterable = for_of(scope, val, |elem| {
            // `elem` aliases `for_of`'s reused slot. Root each element into
            // its own scope slot so scope-tied element types (`HandleValue`,
            // stack newtypes) stay valid after the iteration moves on.
            let elem = scope.root_value(elem.get());
            ret.push(T::from_jsval(scope, elem, option.clone())?);
            Ok::<_, ConversionError>(())
        })?;

        if !iterable {
            return Err(ConversionError::Failure(c"Value is not iterable".into()));
        }

        Ok(ret)
    }
}

// ---------------------------------------------------------------------------
// WebIDL record<DOMString, V>
// https://webidl.spec.whatwg.org/#es-record
// ---------------------------------------------------------------------------

/// WebIDL `record<K, V>` — an ordered map. `K` is a WebIDL string type (`ByteString`, `USVString`,
/// or `DOMString`), kept generic so keys are validated by the same conversion as the source IDL.
///
/// Wraps `IndexMap<K, V>` to preserve insertion order as required by the spec. `Deref` and
/// `IntoIterator` delegate to the inner map.
#[derive(Debug, Clone)]
pub struct Record<K, V>(pub IndexMap<K, V>);

// SAFETY: Traces each key and value in the map; both trace via their own `Trace` impls.
unsafe impl<K: Trace, V: Trace> Trace for Record<K, V> {
    #[inline]
    unsafe fn trace(&self, trc: *mut mozjs::jsapi::JSTracer) {
        for (k, v) in &self.0 {
            k.trace(trc);
            v.trace(trc);
        }
    }
}

impl<K, V> Record<K, V> {
    /// Create an empty record.
    pub fn new() -> Self {
        Record(IndexMap::new())
    }
}

impl<K, V> Default for Record<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> std::ops::Deref for Record<K, V> {
    type Target = IndexMap<K, V>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<K, V> std::ops::DerefMut for Record<K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<K, V> IntoIterator for Record<K, V> {
    type Item = (K, V);
    type IntoIter = indexmap::map::IntoIter<K, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, K, V> IntoIterator for &'a Record<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = indexmap::map::Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

// https://webidl.spec.whatwg.org/#es-record
impl<'s, 'v, C, K, V> FromJSVal<'s, 'v> for Record<K, V>
where
    C: Clone,
    K: FromJSVal<'s, 's, Config = ()> + std::hash::Hash + Eq,
    V: FromJSVal<'s, 's, Config = C>,
{
    type Config = C;

    fn from_jsval(
        scope: &'s Scope<'s>,
        val: HandleValue<'v>,
        option: C,
    ) -> Result<Record<K, V>, ConversionError> {
        // Step 1: If Type(V) is not Object, throw a TypeError.
        if !val.is_object() {
            return Err(ConversionError::Failure(
                c"Value is not an object (expected record)".into(),
            ));
        }

        let obj = crate::Object::from_value(scope, val)?;

        // Step 2: Let result be a new empty instance of record<K, V>.
        let mut result = IndexMap::new();

        // Step 3: Let keys be ? O.[[OwnPropertyKeys]]().
        // Use JSITER_OWNONLY | JSITER_HIDDEN | JSITER_SYMBOLS to get the full
        // [[OwnPropertyKeys]] set, then manually filter out symbols and
        // non-enumerable properties (per spec step 4).
        unsafe {
            let mut ids = mozjs::rust::IdVector::new(scope.cx_mut());

            // Use JSITER_OWNONLY | JSITER_HIDDEN | JSITER_SYMBOLS to match
            // [[OwnPropertyKeys]] ordering.
            let flags = mozjs::jsapi::JSITER_OWNONLY
                | mozjs::jsapi::JSITER_HIDDEN
                | mozjs::jsapi::JSITER_SYMBOLS;

            if !mozjs::rust::wrappers2::GetPropertyKeys(
                scope.cx_mut(),
                obj.handle(),
                flags,
                ids.handle_mut(),
            ) {
                return Err(ConversionError::ExnPending);
            }

            for i in 0..ids.len() {
                rooted!(in(scope.raw_cx_no_gc()) let id_rooted = ids[i]);

                // Step 4.2: Let desc be ? O.GetOwnPropertyDescriptor(key).
                // The spec runs [[GetOwnProperty]] for every key, including symbols, before any
                // filtering, so do this first.
                // Step 4.3: If desc is not undefined and desc.[[Enumerable]] is true...
                let mut is_none = true;
                rooted!(in(scope.raw_cx_no_gc()) let mut desc = PropertyDescriptor {
                    _bitfield_align_1: [0; 0],
                    _bitfield_1: PropertyDescriptor::new_bitfield_1(
                        false, false, false, false, false, false,
                        false, false, false, false,
                    ),
                    getter_: ptr::null_mut(),
                    setter_: ptr::null_mut(),
                    value_: UndefinedValue(),
                });
                if !mozjs::rust::wrappers2::JS_GetOwnPropertyDescriptorById(
                    scope.cx_mut(),
                    obj.handle(),
                    id_rooted.handle(),
                    desc.handle_mut(),
                    &mut is_none,
                ) {
                    return Err(ConversionError::ExnPending);
                }

                if is_none || !desc.get().enumerable_() {
                    continue;
                }

                // Step 4.4: Let typedKey be key converted to an IDL value of type K. Turn the
                // property key (a string, integer-index, or symbol id) into a value, then run the
                // key type's own conversion. This throws for a symbol key (ToString of a Symbol is a
                // TypeError) and for an out-of-range key (e.g. a `ByteString` code unit > 255) —
                // before the value is read.
                let key_val = crate::id::id_to_value(scope, id_rooted.get())
                    .map_err(|_| ConversionError::ExnPending)?;
                let key = K::from_jsval(scope, key_val, ())?;

                // Step 4.5: Let value be ? Get(O, key).
                let mut val_rooted = scope.root_value_mut(UndefinedValue());
                if !mozjs::rust::wrappers2::JS_GetPropertyById(
                    scope.cx_mut(),
                    obj.handle(),
                    id_rooted.handle(),
                    val_rooted.reborrow(),
                ) {
                    return Err(ConversionError::ExnPending);
                }

                // Step 4.6: Let typedValue be the result of converting value
                // to an IDL value of type V.
                let typed_value = V::from_jsval(scope, val_rooted.handle(), option.clone())?;

                // Step 4.7: Set result[typedKey] to typedValue.
                result.insert(key, typed_value);
            }
        }

        Ok(Record(result))
    }
}

// https://webidl.spec.whatwg.org/#es-record
impl<'s, K: AsRef<str>, V: ToJSVal<'s>> ToJSVal<'s> for Record<K, V> {
    fn to_jsval_raw(&self, scope: &'s Scope<'_>) -> Result<JS::Value, ConversionError> {
        let obj = crate::Object::new(scope, None).map_err(|_| ConversionError::ExnPending)?;

        for (key, value) in &self.0 {
            let val = value.to_jsval(scope)?;
            // Set by the two-byte name: the C-string entry point would read
            // the key as Latin-1 instead of Rust's UTF-8 and cannot represent
            // keys with interior NULs, which are legal property names.
            let key_utf16: Vec<u16> = key.as_ref().encode_utf16().collect();
            let ok = unsafe {
                mozjs::rust::wrappers2::JS_SetUCProperty(
                    scope.cx_mut(),
                    obj.handle(),
                    key_utf16.as_ptr(),
                    key_utf16.len(),
                    val,
                )
            };
            if !ok {
                return Err(ConversionError::ExnPending);
            }
        }

        Ok(obj.as_value())
    }
}

// ============================================================================
// Async Sequence — WebIDL §3.2.22
// ============================================================================

/// An async iterable reference captured from a JS value.
///
/// Per WebIDL §3.2.22, an async sequence captures a reference to a JS
/// iterable (either async or sync) for lazy iteration. The type parameter
/// is irrelevant at capture time — values are converted during iteration.
///
/// Created via `FromJSVal` when used as a method parameter:
/// ```rust,ignore
/// #[method]
/// fn process(&self, scope: &Scope<'_>, items: AsyncSequence) -> Promise { ... }
/// ```
#[crate::allow_unrooted_interior]
pub struct AsyncSequence {
    /// The iterable object.
    object: Heap<crate::native::Value>,
    /// The iterator factory method (Symbol.asyncIterator or Symbol.iterator).
    method: Heap<crate::native::Value>,
    /// `true` if the method came from `Symbol.asyncIterator`; `false` for
    /// `Symbol.iterator` (sync, needs wrapping via `CreateAsyncFromSyncIterator`).
    is_async: bool,
}

// SAFETY: Both MozHeap<JSVal> fields maintain GC write barriers.
unsafe impl Trace for AsyncSequence {
    unsafe fn trace(&self, trc: *mut crate::native::JSTracer) {
        self.object.trace(trc);
        self.method.trace(trc);
    }
}

impl AsyncSequence {
    /// Whether the captured iterator factory is an async iterator.
    pub fn is_async(&self) -> bool {
        self.is_async
    }
}

// https://webidl.spec.whatwg.org/#es-async-iterable
impl FromJSVal<'_, '_> for AsyncSequence {
    type Config = ();
    #[crate::allow_unrooted]
    fn from_jsval(scope: &Scope<'_>, val: HandleValue, _: ()) -> Result<Self, ConversionError> {
        if !val.is_object() {
            return Err(ConversionError::Failure(
                c"Value is not an object (expected async iterable)".into(),
            ));
        }
        let obj = crate::Object::from_value(scope, val)?;

        // GetMethod(V, P): Get the property — no [[HasProperty]] pre-check,
        // and a Get error propagates — treat null/undefined as absent, and
        // require anything else to be callable.
        let get_method =
            |code: crate::native::SymbolCode| -> Result<Option<JSVal>, ConversionError> {
                let key = crate::symbol::get_well_known_key(scope, code);
                let id = scope.root_id(key);
                let method_val = obj.get_property_by_id(scope, id)?;
                Ok(crate::iteration::ensure_is_method(scope, method_val)?.map(|m| m.get()))
            };

        // Let method be ? GetMethod(V, %Symbol.asyncIterator%).
        if let Some(method) = get_method(crate::native::SymbolCode::asyncIterator)? {
            return Ok(AsyncSequence {
                object: Heap::from(val.get()),
                method: Heap::from(method),
                is_async: true,
            });
        }

        // If method is undefined, let it be ? GetMethod(V, %Symbol.iterator%).
        if let Some(method) = get_method(crate::native::SymbolCode::iterator)? {
            return Ok(AsyncSequence {
                object: Heap::from(val.get()),
                method: Heap::from(method),
                is_async: false,
            });
        }

        Err(ConversionError::Failure(
            c"Object is not iterable (no Symbol.asyncIterator or Symbol.iterator)".into(),
        ))
    }
}

pub use mozjs::conversions::Utf8Chars;
