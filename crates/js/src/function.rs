// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Function creation, calling, and callbacks.
//!
//! The [`Function`] marker type implements
//! [`JSType`](crate::builtins::JSType), enabling
//! [`Function<'s>`](crate::Function) as the scope-rooted function handle
//! type. It implements `Deref` to [`Object<'s>`](crate::Object), so all
//! property access methods are available directly.
//!
//! # Callbacks
//!
//! Use [`Function::new_callback`](Stack::new_callback) to create a JS
//! function backed by a Rust function pointer. The callback receives a
//! [`Scope`] for interacting with the JS engine, a [`CallbackArgs`] struct
//! for safe argument access, and a rooted payload value.
//!
//! ```ignore
//! # use core_runtime::js::gc::scope::Scope;
//! # fn example(scope: &Scope<'_>) {
//! use core_runtime::js;
//!
//! let add = js::Function::new_callback(&scope, c"add", 2, |_scope, args, _payload| {
//!     let a = args.get_i32(0).unwrap_or(0);
//!     let b = args.get_i32(1).unwrap_or(0);
//!     Ok(js::value::from_i32(a + b))
//! }, js::value::undefined()).unwrap();
//! # }
//! ```
//!
//! # Calling Functions
//!
//! Use the [`call`](Stack::call), [`call_value`](Stack::call_value), or
//! [`call_by_name`](Stack::call_by_name) methods to invoke JS functions
//! from Rust.

use std::ffi::CStr;
use std::os::raw::c_uint;
use std::ptr::NonNull;

use super::error::{report_error_ascii, ExnThrown, ThrowException};
use crate::builtins::JSType;
use crate::conversion::ToJSVal;
use crate::gc::handle::Stack;
use crate::gc::scope::Scope;
use crate::value::ValueArrayRooter;
use crate::Object;
use mozjs::gc::{HandleObject, HandleValue};
use mozjs::jsapi::{
    GetFunctionNativeReserved, JSClass, JSFunction, JSNative, SetFunctionNativeReserved, Value,
};
use mozjs::jsval::UndefinedValue;
use mozjs::rust::wrappers2;

// ---------------------------------------------------------------------------
// Function marker type
// ---------------------------------------------------------------------------

/// Marker type for JavaScript `Function` objects.
///
/// [`Function<'s>`](crate::Function) is the scope-rooted handle type:
///
/// ```ignore
/// let fun = js::Function::define(&scope, global.handle(), c"greet", Some(my_native), 1, 0)?;
/// ```
///
/// `Function<'s>` derefs to [`Object<'s>`](crate::Object), so all property
/// access methods are available directly.
pub struct Function;

/// Empty argument list for zero-arg calls, avoiding failed type inference for `&[]`.
#[allow(non_upper_case_globals)]
pub const EmptyArgs: &[()] = &[];

impl JSType for Function {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "Function";

    fn js_class() -> *const JSClass {
        crate::class::proto_key_to_class(mozjs::jsapi::JSProtoKey::JSProto_Function)
    }

    /// Checks if `obj` is a `JSFunction`.
    ///
    /// Note that this is not the same as `IsCallable`: some callable objects (bound functions, proxies) are not `JSFunction`s. Callsites that need to accept any callable should use `Object::is_callable` instead of this method, and represent the function
    /// as an `Object` rather than a `Function`.
    unsafe fn is_instance(obj: *mut mozjs::jsapi::JSObject) -> bool {
        unsafe { mozjs::jsapi::JS_ObjectIsFunction(obj) }
    }
}

impl<'s> Stack<'s, Function> {
    // ---------------------------------------------------------------------------
    // Function creation
    // ---------------------------------------------------------------------------

    /// Define a native function on an object.
    ///
    /// `name` is the JS-visible function name. `nargs` is the declared number of
    /// arguments (used for `Function.length`). `attrs` are property attributes.
    pub fn define(
        scope: &'s Scope<'_>,
        obj: HandleObject,
        name: &CStr,
        call: JSNative,
        nargs: c_uint,
        attrs: c_uint,
    ) -> Result<Self, ExnThrown> {
        let fun = unsafe {
            wrappers2::JS_DefineFunction(scope.cx_mut(), obj, name.as_ptr(), call, nargs, attrs)
        };
        let fun = NonNull::new(fun).ok_or(ExnThrown)?;
        let obj = unsafe { mozjs::jsapi::JS_GetFunctionObject(fun.as_ptr()) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new standalone function (not attached to an object).
    pub fn new(
        scope: &'s Scope<'_>,
        call: JSNative,
        nargs: c_uint,
        flags: c_uint,
        name: &CStr,
    ) -> Result<Self, ExnThrown> {
        let fun =
            unsafe { wrappers2::JS_NewFunction(scope.cx_mut(), call, nargs, flags, name.as_ptr()) };
        let fun = NonNull::new(fun).ok_or(ExnThrown)?;
        let obj = unsafe { mozjs::jsapi::JS_GetFunctionObject(fun.as_ptr()) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new function with reserved slots for storing closure data.
    ///
    /// Reserved slots can be accessed via `GetFunctionNativeExtra` /
    /// `SetFunctionNativeExtra`.
    pub fn new_with_reserved(
        scope: &'s Scope<'_>,
        call: JSNative,
        nargs: c_uint,
        flags: c_uint,
        name: &CStr,
    ) -> Result<Self, ExnThrown> {
        let fun = unsafe {
            wrappers2::NewFunctionWithReserved(scope.cx_mut(), call, nargs, flags, name.as_ptr())
        };
        let fun = NonNull::new(fun).ok_or(ExnThrown)?;
        let obj = unsafe { mozjs::jsapi::JS_GetFunctionObject(fun.as_ptr()) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Get the underlying `JSFunction` pointer.
    pub fn as_function_ptr(&self) -> *mut JSFunction {
        unsafe { mozjs::jsapi::JS_GetObjectFunction(self.handle().get()) }
    }

    /// Get a reserved slot value on this function.
    ///
    /// # Safety
    ///
    /// `self` must have been created with `new_with_reserved`, or otherwise be guaranteed
    /// to be a function object with reserved slots.
    pub unsafe fn get_reserved<'r>(
        &self,
        scope: &'r Scope<'_>,
        slot: ReservedSlot,
    ) -> HandleValue<'r> {
        scope.root_value(*GetFunctionNativeReserved(self.as_raw(), slot.into()))
    }

    /// Set a reserved slot value on this function.
    ///
    /// # Safety
    ///
    /// `self` must have been created with `new_with_reserved`, or otherwise be guaranteed
    /// to be a function object with reserved slots.
    pub unsafe fn set_reserved(&self, slot: ReservedSlot, val: impl Into<Value>) {
        SetFunctionNativeReserved(self.as_raw(), slot.into(), &val.into());
    }

    // ---------------------------------------------------------------------------
    // Function calling
    // ---------------------------------------------------------------------------

    /// Call a function value with the given `this` object and arguments. A `fval` that isn't
    /// callable throws the `TypeError` of [`throw_not_callable`].
    pub fn call_value<'a>(
        scope: &'a Scope<'_>,
        this: HandleObject,
        fval: HandleValue,
        args: &[impl ToJSVal<'a>],
    ) -> Result<HandleValue<'a>, ExnThrown> {
        if !is_callable(fval) {
            return Err(throw_not_callable(scope, fval));
        }
        let mut args_root = ValueArrayRooter::new(args.len());
        let args = args_root.root(scope, args)?;
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe {
            wrappers2::JS_CallFunctionValue(
                scope.cx_mut(),
                this,
                fval,
                &args.handles(),
                rval.reborrow(),
            )
        };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Call a named method on an object. The method is looked up through the global's interned
    /// key for `name` (see [`get_or_init_property_id`](crate::class::get_or_init_property_id)). A
    /// property that isn't callable throws a `TypeError` that `name` is not a function. Panics if
    /// `name` is an array index.
    pub fn call_by_name<'a>(
        scope: &'a Scope<'_>,
        obj: HandleObject,
        name: &'static CStr,
        args: &[impl ToJSVal<'a>],
    ) -> Result<HandleValue<'a>, ExnThrown> {
        let mut args_root = ValueArrayRooter::new(args.len());
        let args = args_root.root(scope, args)?;
        let id = crate::class::get_or_init_property_id(scope, name)?;
        let mut fval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe { wrappers2::JS_GetPropertyById(scope.cx_mut(), obj, id, fval.reborrow()) };
        ExnThrown::check(ok)?;
        if !is_callable(fval.handle()) {
            let message = format!("{} is not a function", name.to_string_lossy());
            return Err(crate::error::TypeError(message).throw(scope));
        }
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok = unsafe {
            wrappers2::JS_CallFunctionValue(
                scope.cx_mut(),
                obj,
                fval.handle(),
                &args.handles(),
                rval.reborrow(),
            )
        };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Call a function object with a given `this` value. A `fun` that isn't callable throws the
    /// `TypeError` of [`throw_not_callable`].
    pub fn call<'a>(
        scope: &'a Scope<'_>,
        this: impl ToJSVal<'a>,
        fun: impl ToJSVal<'a>,
        args: &[impl ToJSVal<'a>],
    ) -> Result<HandleValue<'a>, ExnThrown> {
        let this = this.to_jsval_throwing(scope)?;
        let fun = fun.to_jsval_throwing(scope)?;
        if !is_callable(fun) {
            return Err(throw_not_callable(scope, fun));
        }
        let mut args_root = ValueArrayRooter::new(args.len());
        let args = args_root.root(scope, args)?;
        let mut rval = scope.root_value_mut(UndefinedValue());
        let ok =
            unsafe { wrappers2::Call(scope.cx_mut(), this, fun, &args.handles(), rval.reborrow()) };
        ExnThrown::check(ok)?;
        Ok(rval.handle())
    }

    /// Invoke the `new` operator on a constructor function.
    pub fn construct(
        scope: &'s Scope<'_>,
        fun: impl ToJSVal<'s>,
        args: &[impl ToJSVal<'s>],
    ) -> Result<Object<'s>, ExnThrown> {
        let fun = fun.to_jsval_throwing(scope)?;
        let mut args_root = ValueArrayRooter::new(args.len());
        let args = args_root.root(scope, args)?;
        let mut result = scope.root_object_mut(std::ptr::null_mut());
        let ok = unsafe {
            wrappers2::Construct1(scope.cx_mut(), fun, &args.handles(), result.reborrow())
        };
        ExnThrown::check(ok)?;
        Object::from_handle(result.handle()).ok_or(ExnThrown)
    }

    /// Invoke the `new` operator on a constructor with an explicit `new.target`.
    pub fn construct_with_new_target(
        scope: &'s Scope<'_>,
        fun: impl ToJSVal<'s>,
        new_target: HandleObject,
        args: &[impl ToJSVal<'s>],
    ) -> Result<Object<'s>, ExnThrown> {
        let fun = fun.to_jsval_throwing(scope)?;
        let mut args_root = ValueArrayRooter::new(args.len());
        let args = args_root.root(scope, args)?;
        let mut result = scope.root_object_mut(std::ptr::null_mut());
        let ok = unsafe {
            wrappers2::Construct(
                scope.cx_mut(),
                fun,
                new_target,
                &args.handles(),
                result.reborrow(),
            )
        };
        ExnThrown::check(ok)?;
        Object::from_handle(result.handle()).ok_or(ExnThrown)
    }

    // ---------------------------------------------------------------------------
    // Function introspection
    // ---------------------------------------------------------------------------

    /// Get the `.length` property of this function.
    pub fn length(&self, scope: &Scope<'_>) -> Result<u16, ExnThrown> {
        let fun_ptr = self.as_function_ptr();
        let fun_nn = NonNull::new(fun_ptr).expect("function object has no JSFunction");
        let fun_handle = scope.root_function(fun_nn);
        let mut length: u16 = 0;
        let ok =
            unsafe { wrappers2::JS_GetFunctionLength(scope.cx_mut(), fun_handle, &mut length) };
        ExnThrown::check(ok)?;
        Ok(length)
    }

    // ---------------------------------------------------------------------------
    // Rust-native callbacks
    // ---------------------------------------------------------------------------

    /// Create a new JS function backed by a Rust function pointer plus a
    /// JS-value payload.
    ///
    /// The callback receives a [`Scope`], a [`CallbackArgs`], and the provided `payload` value.
    /// The returned `Ok(JSVal)` is set as the function's return value.
    ///
    /// # Example
    ///
    /// ```ignore
    /// # use core_runtime::js::gc::scope::Scope;
    /// # fn example(scope: &Scope<'_>) {
    /// use core_runtime::js;
    ///
    /// let greet = js::Function::new_callback(
    ///     scope,
    ///     c"greet",
    ///     1,
    ///     |_scope, _args, _payload| Ok(js::value::from_i32(42)),
    ///     (),
    /// ).unwrap();
    /// # }
    /// ```
    pub fn new_callback(
        scope: &'s Scope<'_>,
        name: &CStr,
        nargs: c_uint,
        cb: Callback,
        payload: impl ToJSVal<'s>,
    ) -> Result<Self, ExnThrown> {
        Self::new_callback_with_flags(scope, name, nargs, 0, cb, payload)
    }

    /// Create a callback-backed function that can be invoked with `new`.
    ///
    /// Identical to [`new_callback`](Self::new_callback) but the resulting
    /// function carries `JSFUN_CONSTRUCTOR`, so `new f(...)` runs the callback as
    /// a constructor. A constructor callback whose return value is an object has
    /// that object adopted as the constructed instance; a non-object return yields a freshly
    /// allocated `this`. The `wit-bridge` interpreter uses this to synthesize
    /// imported-resource constructors.
    pub fn new_constructor_callback(
        scope: &'s Scope<'_>,
        name: &CStr,
        nargs: c_uint,
        cb: Callback,
        payload: impl ToJSVal<'s>,
    ) -> Result<Self, ExnThrown> {
        Self::new_callback_with_flags(
            scope,
            name,
            nargs,
            mozjs::jsapi::JSFUN_CONSTRUCTOR,
            cb,
            payload,
        )
    }

    /// Shared body of [`new_callback`](Self::new_callback) and
    /// [`new_constructor_callback`](Self::new_constructor_callback): create the
    /// reserved-slot function with the given creation `flags` and stash the
    /// callback pointer and payload.
    fn new_callback_with_flags(
        scope: &'s Scope<'_>,
        name: &CStr,
        nargs: c_uint,
        flags: c_uint,
        cb: Callback,
        payload: impl ToJSVal<'s>,
    ) -> Result<Self, ExnThrown> {
        let payload = payload.to_jsval_throwing(scope)?;
        let fun = Self::new_with_reserved(scope, Some(callback_trampoline), nargs, flags, name)?;

        unsafe {
            fun.set_reserved(
                ReservedSlot::Slot0,
                mozjs::jsval::PrivateValue(cb as *const std::ffi::c_void),
            );
            fun.set_reserved(ReservedSlot::Slot1, payload.get());
        }

        Ok(fun)
    }
}

crate::gc::handle::deref_to_object!(Function);

crate::gc::handle::from_jsval_via_cast!(Function, c"Value isn't a Function");

/// Marker type for any callable object: a function, a bound function, a
/// callable proxy, or any other object SpiderMonkey's `IsCallable` accepts.
///
/// Used for WebIDL callback function types: the conversion throws a
/// `TypeError` for a value that is not callable. [`Function`] accepts only
/// `JSFunction` objects, which excludes bound functions and callable proxies.
pub struct Callable;

impl JSType for Callable {
    const JS_NAME: &'static str = "callable object";
    type Rooted<'s> = Stack<'s, Self>;

    fn js_class() -> *const JSClass {
        Function::js_class()
    }

    #[inline]
    unsafe fn is_instance(obj: *mut mozjs::jsapi::JSObject) -> bool {
        unsafe { mozjs::jsapi::JS::IsCallable(obj) }
    }
}

crate::gc::handle::deref_to_object!(Callable);

crate::gc::handle::from_jsval_via_cast!(Callable, c"Value is not callable");

impl<'s> Stack<'s, Function> {
    /// This function as a [`Callable`](crate::Callable).
    pub fn as_callable(self) -> crate::Callable<'s> {
        // SAFETY: every function is callable.
        unsafe { Stack::from_handle_unchecked(self.handle()) }
    }
}

pub enum ReservedSlot {
    Slot0 = 0,
    Slot1 = 1,
    Slot2 = 2,
}

impl From<ReservedSlot> for usize {
    fn from(slot: ReservedSlot) -> Self {
        slot as usize
    }
}

// ---------------------------------------------------------------------------
// Closure-based callbacks
// ---------------------------------------------------------------------------

/// The arguments of a [`Callback`] invocation: either a JS call's
/// [`CallArgs`](mozjs::jsapi::CallArgs), or rooted values passed from Rust by a
/// direct call (see [`from_values`](Self::from_values)).
///
/// Provides indexed access to arguments, the `this` value, and the argument
/// count.
pub struct CallbackArgs<'a> {
    source: ArgsSource<'a>,
}

enum ArgsSource<'a> {
    Call(&'a mozjs::jsapi::CallArgs),
    Values(&'a [HandleValue<'a>]),
}

impl<'a> CallbackArgs<'a> {
    /// Arguments for calling a [`Callback`] directly from Rust. `this` is `undefined`, and the
    /// call is not a constructor call.
    pub fn from_values(values: &'a [HandleValue<'a>]) -> Self {
        Self {
            source: ArgsSource::Values(values),
        }
    }

    /// Number of arguments passed by the caller.
    #[inline]
    pub fn len(&self) -> u32 {
        match self.source {
            ArgsSource::Call(args) => args.argc_,
            ArgsSource::Values(values) => values.len() as u32,
        }
    }

    /// Whether no arguments were passed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get argument `i`.
    ///
    /// Returns `undefined` if `i` is out of range.
    #[inline]
    pub fn get(&'a self, i: u32) -> HandleValue<'a> {
        match self.source {
            ArgsSource::Call(args) => unsafe { HandleValue::from_raw(args.get(i)) },
            ArgsSource::Values(values) => values
                .get(i as usize)
                .copied()
                .unwrap_or(HandleValue::undefined()),
        }
    }

    /// Argument `i` without rooting it, for an immediate type check.
    #[inline]
    fn raw(&self, i: u32) -> Value {
        match self.source {
            ArgsSource::Call(args) => args.get(i).get(),
            ArgsSource::Values(values) => values
                .get(i as usize)
                .map_or_else(crate::value::undefined, |v| v.get()),
        }
    }

    /// Get argument `i` as an `i32`, or `None` if it isn't an int32.
    #[inline]
    pub fn get_i32(&self, i: u32) -> Option<i32> {
        let v = self.raw(i);
        if v.is_int32() {
            Some(v.to_int32())
        } else {
            None
        }
    }

    /// Get argument `i` as an `f64`, or `None` if it isn't a number.
    #[inline]
    pub fn get_f64(&self, i: u32) -> Option<f64> {
        let v = self.raw(i);
        if v.is_double() {
            Some(v.to_double())
        } else if v.is_int32() {
            Some(v.to_int32() as f64)
        } else {
            None
        }
    }

    /// Get argument `i` as a `bool`, or `None` if it isn't a boolean.
    #[inline]
    pub fn get_bool(&self, i: u32) -> Option<bool> {
        let v = self.raw(i);
        if v.is_boolean() {
            Some(v.to_boolean())
        } else {
            None
        }
    }

    /// Get the `this` value.
    #[inline]
    pub fn this(&self) -> Value {
        match self.source {
            ArgsSource::Call(args) => args.thisv().get(),
            ArgsSource::Values(_) => crate::value::undefined(),
        }
    }

    /// Whether this is a constructor call (`new`).
    #[inline]
    pub fn is_constructing(&self) -> bool {
        match self.source {
            ArgsSource::Call(args) => args.is_constructing(),
            ArgsSource::Values(_) => false,
        }
    }
}

/// Whether `value` is a callable object, as ECMAScript's `IsCallable` tests.
#[inline]
pub fn is_callable(value: HandleValue<'_>) -> bool {
    // SAFETY: `value` is rooted, so an object it holds is live.
    value.is_object() && unsafe { mozjs::jsapi::JS::IsCallable(value.to_object()) }
}

/// Throw the `TypeError` for a call of `value`, which isn't callable: `undefined`, or `value`'s
/// source as `JS_ValueToSource` gives it, followed by `is not a function`.
pub fn throw_not_callable(scope: &Scope<'_>, value: HandleValue<'_>) -> ExnThrown {
    let source = if value.is_undefined() {
        "undefined".to_string()
    } else {
        // SAFETY: `value` is rooted.
        let source = unsafe { wrappers2::JS_ValueToSource(scope.cx_mut(), value) };
        let Some(source) = NonNull::new(source) else {
            return ExnThrown;
        };
        crate::conversion::jsstr_to_string(scope, source)
    };
    crate::error::TypeError(format!("{source} is not a function")).throw(scope)
}

/// Type-erased callback stored in a helper function's reserved slot.
pub type Callback = fn(&Scope<'_>, CallbackArgs<'_>, HandleValue) -> Result<Value, ExnThrown>;

/// Recover the class instance `T` a [`Callback`]'s `payload` was created with.
///
/// # Panics
///
/// Panics if `payload` is not an instance of `T`. The code that creates a
/// callback also sets its payload, so a mismatch indicates a bug in that code.
pub fn cast_payload<'r, T>(scope: &'r Scope<'_>, payload: HandleValue<'_>) -> T
where
    T: crate::builtins::CastTarget<'r, Output = T>,
{
    Object::from_raw_value(scope, payload.get())
        .expect("callback payload is an object")
        .cast::<T>()
        .unwrap_or_else(|e| panic!("callback payload: {e}"))
}

/// The extern "C" trampoline that bridges JSNative to the stored callback.
///
/// # Safety
///
/// This function is called by SpiderMonkey's function dispatch and expects:
/// - `vp` to point to a valid `CallArgs` frame
/// - The callee's reserved slot 0 to contain a function with the `Callback` signature
/// - The callee's reserved slot 1 to contain the closure payload as a `Value`
unsafe extern "C" fn callback_trampoline(
    cx: *mut mozjs::jsapi::JSContext,
    argc: u32,
    vp: *mut Value,
) -> bool {
    let args = mozjs::jsapi::CallArgs::from_vp(vp, argc);

    // Create a scope for the callback. We're inside a native call so a
    // realm is always entered.
    // SAFETY: SpiderMonkey guarantees cx is valid and a realm is entered
    // when calling a native function.
    let scope = crate::gc::scope::RootScope::from_current_realm(cx);

    // Get the callback and payload from the function's reserved slots.
    let callee = args.callee();
    let cb: Callback =
        std::mem::transmute((*mozjs::jsapi::GetFunctionNativeReserved(callee, 0)).to_private());
    let payload = scope.root_value(*mozjs::jsapi::GetFunctionNativeReserved(callee, 1));

    let cb_args = CallbackArgs {
        source: ArgsSource::Call(&args),
    };

    // Call the function pointer.
    match cb(&scope, cb_args, payload) {
        Ok(val) => {
            args.rval().set(val);
            true
        }
        Err(_) => {
            // If no exception is already pending, throw a generic one.
            if !crate::exception::is_pending(&scope) {
                report_error_ascii(&scope, c"Native callback returned an error");
            }
            false
        }
    }
}
