// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Typed handles for scope-rooted and heap-traced JS object references.
//!
//! [`Stack<'s, T>`] ties a JS object reference to a rooting scope,
//! preventing the GC from collecting it. [`Heap<T>`] stores a GC-traced
//! reference that can outlive any particular rooting scope.
//!
//! Both types are parameterized by a [`JSType`] marker that carries
//! static type information about the kind of JS object.

use crate::builtins::{CastError, CastTarget, JSType};
use crate::conversion::{ConversionError, ToJSVal};
use crate::gc::scope::Scope;
use crate::heap::{MozHeap, Trace};
use crate::native::{JSObject, JSTracer, RawHandle};
use mozjs::gc::{GCMethods, Handle, RootedTraceableBox};
use mozjs_sys::jsval::ObjectValue;
use std::marker::PhantomData;
use std::pin::Pin;
use std::ptr::NonNull;

/// A scope-rooted handle to a JavaScript object of type `T`.
///
/// `Stack<'s, T>` ties a JS object reference to a rooting scope `'s`,
/// preventing the GC from collecting it. The type parameter `T` carries
/// static type information about what kind of JS object this is.
///
/// Methods are partitioned by trait bound:
/// - `T: JSType` — handle access, `as_value()`, `as_raw()`
/// - `T: ClassDef` — `data()`, `data_mut()`, `cast()` (defined in [`class`](crate::class))
/// - Concrete `T` — type-specific methods (e.g. `Object<'s>` has property accessors)
#[repr(transparent)]
pub struct Stack<'s, T: JSType> {
    pub(crate) handle: Handle<'s, *mut JSObject>,
    pub(crate) _marker: PhantomData<T>,
}

impl<'s, T: JSType> Stack<'s, T> {
    /// Check whether `self` is an instance of the target type `U`.
    ///
    /// Accepts both builtin marker types (`Date`, `Array`, …) and
    /// proc-macro newtypes (`Dog<'s>`, etc.) as the target.
    pub fn is<U: CastTarget<'s>>(&self) -> bool {
        unsafe { U::is_instance(self.as_raw()) }
    }

    /// Type-checked downcast to any supported target type.
    ///
    /// Reads the object's actual `JSClass` pointer at runtime and checks
    /// whether it matches `U` (or a subclass). Works for both builtin
    /// types (Array, Date, Promise, …) and proc-macro newtypes
    /// (Dog, Cat, …).
    ///
    /// ```ignore
    /// let date: Stack<'s, Date> = obj.cast::<Date>()?;
    /// let dog: Dog<'s> = obj.cast::<Dog>()?;
    /// ```
    pub fn cast<U: CastTarget<'s>>(self) -> Result<U::Output, CastError> {
        if !self.is::<U>() {
            return Err(CastError {
                from: T::JS_NAME,
                to: U::TARGET_NAME,
            });
        }
        Ok(unsafe { U::construct_unchecked(self.handle()) })
    }
}

// Manual Clone/Copy impls: T lives only in PhantomData, so Stack is always
// Copy regardless of whether T is Copy.
impl<'s, T: JSType> Clone for Stack<'s, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'s, T: JSType> Copy for Stack<'s, T> {}

impl<'s, T: JSType> Stack<'s, T> {
    /// Wrap an existing rooted handle.
    ///
    /// # Safety
    ///
    /// The handle must point to a JS object of type `T` (or a subtype).
    pub unsafe fn from_handle_unchecked(handle: Handle<'s, *mut JSObject>) -> Self {
        Stack {
            handle,
            _marker: PhantomData,
        }
    }

    /// Get the underlying rooted handle.
    pub fn handle(self) -> Handle<'s, *mut JSObject> {
        self.handle
    }

    /// Get the underlying raw (lifetime-erased) rooted handle.
    pub fn raw_handle(self) -> RawHandle<*mut JSObject> {
        self.handle.into()
    }

    /// Get the raw `*mut JSObject` pointer.
    pub fn as_raw(self) -> *mut JSObject {
        self.handle.get()
    }

    /// Get the JS value representation.
    pub fn as_value(self) -> crate::native::Value {
        // SAFETY: the handle is rooted, so the object is alive.
        unsafe { crate::value::from_object(self.handle.get()) }
    }

    /// Root a raw pointer in the given scope.
    ///
    /// Returns `None` if `ptr` is null.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a JS object of type `T` (or a subtype).
    pub unsafe fn from_raw(scope: &'s Scope<'_>, ptr: *mut JSObject) -> Option<Self> {
        NonNull::new(ptr).map(|nn| {
            debug_assert!(
                Self::is_instance(ptr),
                "raw pointer {:p} is not a {}",
                ptr,
                T::JS_NAME
            );
            Stack {
                handle: scope.root_object(nn),
                _marker: PhantomData,
            }
        })
    }

    /// Root a raw pointer in the given scope, mapping null to `ExnThrown`.
    ///
    /// JSAPI creation functions return null with an exception pending, so
    /// this is the standard wrapper for their results.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or point to a JS object of type `T` (or a
    /// subtype).
    pub(crate) unsafe fn from_mozjs_rval(
        scope: &'s Scope<'_>,
        ptr: *mut JSObject,
    ) -> Result<Self, crate::error::ExnThrown> {
        Self::from_raw(scope, ptr).ok_or(crate::error::ExnThrown)
    }
}

/// Implements `Deref<Target = Object<'s>>` for a builtin marker's stack
/// handle, making all property-access methods available on it.
///
/// Sound because every `Stack<'s, T>` is `repr(transparent)` over
/// `Handle<'s, *mut JSObject>`, so the layouts are identical.
macro_rules! deref_to_object {
    ($marker:ty) => {
        impl<'s> std::ops::Deref for $crate::gc::handle::Stack<'s, $marker> {
            type Target = $crate::Object<'s>;

            fn deref(&self) -> &$crate::Object<'s> {
                // SAFETY: both are repr(transparent) over the same handle.
                unsafe { &*(self as *const Self as *const $crate::Object<'s>) }
            }
        }
    };
}
pub(crate) use deref_to_object;

/// Implements `FromJSVal` for a builtin marker's stack handle via a
/// type-checked [`cast`](Stack::cast) from `Object`, with the given
/// wrong-type message.
macro_rules! from_jsval_via_cast {
    ($marker:ty, $err:literal) => {
        impl<'s, 'v> $crate::conversion::FromJSVal<'s, 'v>
            for $crate::gc::handle::Stack<'s, $marker>
        {
            type Config = ();

            fn from_jsval(
                scope: &'s $crate::gc::scope::Scope<'_>,
                val: mozjs::gc::HandleValue<'v>,
                _option: Self::Config,
            ) -> Result<Self, $crate::conversion::ConversionError> {
                $crate::Object::from_raw_value(scope, val.get())?
                    .cast::<Self>()
                    .map_err(|_| {
                        $crate::conversion::ConversionError::Failure(std::borrow::Cow::Borrowed(
                            $err,
                        ))
                    })
            }
        }
    };
}
pub(crate) use from_jsval_via_cast;

impl<'s, T: JSType> std::fmt::Debug for Stack<'s, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stack")
            .field("type", &T::JS_NAME)
            .field("ptr", &self.handle.get())
            .finish()
    }
}

// ToJSValConvertible: allows returning Stack newtypes from methods/getters.
impl<'s, T: JSType> ToJSVal<'s> for Stack<'s, T> {
    #[inline]
    fn to_jsval_raw(&self, _scope: &'s Scope<'s>) -> Result<crate::value::Value, ConversionError> {
        Ok(self.as_value())
    }
}

/// Maps a `Heap<T>` type parameter to the mozjs GC cell it stores.
///
/// Implemented blanket for object marker types (`T: JSType`, cell =
/// `*mut JSObject`) and concretely for [`Value`](crate::native::Value). This is
/// what lets one `Heap<T>` cover both object references and arbitrary JS values
/// while the inner `MozHeap` stays boxed (and therefore move-safe).
pub trait HeapCell {
    /// The mozjs cell type backing this heap reference.
    type Cell: Copy + GCMethods;
    /// The empty/uninitialised cell value (null object / `undefined`).
    fn null_cell() -> Self::Cell;
}

impl<T: JSType> HeapCell for T {
    type Cell = *mut JSObject;
    fn null_cell() -> *mut JSObject {
        std::ptr::null_mut()
    }
}

impl HeapCell for crate::native::Value {
    type Cell = crate::native::Value;
    fn null_cell() -> crate::native::Value {
        crate::value::undefined()
    }
}

/// A GC-traced heap reference to a JavaScript object of type `T`.
///
/// `Heap<T>` stores a JS object pointer that is traced by the garbage
/// collector, allowing it to outlive any particular rooting scope. Use
/// this to store JS objects inside Rust structs that are themselves traced.
///
/// The inner `MozHeap` is boxed so that the GC write barrier address
/// remains stable when the `Heap` is moved (e.g. into a `Vec` or
/// `Option`). This makes `From<Stack<T>> for Heap<T>` safe and the overall
/// API much more ergonomic.
///
/// The downside is an extra level of indirection and heap allocation for
/// every `Heap`. In practice this cost is relatively small compared to the
/// cost of the JS objects being stored.
///
/// To access the object, root it with [`get`](Heap::get), which returns
/// the stack newtype directly (e.g. `Item<'s>` for `Heap<ItemImpl>`).
#[crate::must_root]
pub struct Heap<T: HeapCell> {
    heap: Pin<Box<MozHeap<T::Cell>>>,
    _marker: PhantomData<T>,
}

#[crate::allow_unrooted]
impl<T: JSType> Heap<T> {
    /// Create from a raw JS object pointer.
    ///
    /// # Safety
    ///
    /// `obj` must be a valid, non-null JS object of type `T`.
    pub unsafe fn from_raw(obj: *mut JSObject) -> Self {
        debug_assert!(!obj.is_null(), "Heap::from_raw called with null pointer");
        let heap = Box::pin(MozHeap::default());
        (*heap).set(obj);
        Heap {
            heap,
            _marker: PhantomData,
        }
    }

    /// Root the referenced object, returning a stack-rooted handle.
    ///
    /// Returns `T::Rooted<'s>`, the stack-rooted type for `Heap<T>`.
    ///
    /// # Panics
    ///
    /// Panics if called on a default-constructed (null) `Heap`.
    pub fn get<'s>(&self, scope: &'s Scope<'_>) -> T::Rooted<'s> {
        let obj = self.heap.get();
        let handle = scope.root_object(
            NonNull::new(obj).expect("Heap::get called on a null (default-constructed) Heap"),
        );
        T::Rooted::from(Stack {
            handle,
            _marker: PhantomData,
        })
    }

    /// Consume the heap reference, returning its referent rooted in `scope`.
    ///
    /// Use this when a `Heap` (often a field of a struct) has been moved out of a
    /// traced location into a Rust local and the next step can GC. A traced `Heap`
    /// has its stored pointer updated by a moving GC and reclaimed cleanly; an
    /// *untraced* one left to drop after a GC would run its drop write barrier on a
    /// pointer the GC has since moved or freed — a use-after-free.
    ///
    /// The subtlety is that *rooting itself* can GC (e.g. under `RootsChange`
    /// zeal), so it is not enough to `get` and then drop: that drop would still
    /// run after GC has invalidated the pointer. `take` instead reads the
    /// pointer, drops the `Heap` (running its barrier on the still-valid pointer),
    /// and only *then* roots.
    ///
    /// # Panics
    ///
    /// Panics if called on a default-constructed (null) `Heap`.
    pub fn take<'s>(self, scope: &'s Scope<'_>) -> T::Rooted<'s> {
        let obj = NonNull::new(self.heap.get())
            .expect("Heap::take called on a null (default-constructed) Heap");
        // Drop the `Heap` now, while its stored pointer is still live, so its
        // barrier never runs on a pointer a later (rooting-triggered) GC moved.
        drop(self);
        // Rooting may trigger a GC that moves `obj`; `root_object` installs it
        // into a traced slot first, so the returned handle tracks the new
        // location even though the local `obj` becomes stale.
        let handle = scope.root_object(obj);
        T::Rooted::from(Stack {
            handle,
            _marker: PhantomData,
        })
    }

    /// Whether this heap reference has been initialized (is non-null).
    ///
    /// A default-constructed `Heap` is null; one created via `From<Stack>`
    /// or `from_raw` is always initialized. Once initialized, a `Heap`
    /// remains non-null for its entire lifetime.
    pub fn is_initialized(&self) -> bool {
        !self.heap.get().is_null()
    }

    /// Store a new referent, replacing any previous one.
    pub fn set<'a>(&self, value: impl Into<Stack<'a, T>>) {
        (*self.heap).set(value.into().as_raw());
    }

    /// The current raw object pointer, for identity comparison only.
    ///
    /// `Heap` is traced, so the GC updates the stored pointer when a
    /// compacting collection moves the object; this returns that
    /// barrier-updated pointer. Comparing two live `as_ptr` results is
    /// therefore correct across collections — unlike caching a raw pointer,
    /// which would go stale. The returned pointer must not be dereferenced or
    /// stored; root it via [`get`](Heap::get) for any actual use.
    ///
    /// # Safety
    /// The returned pointer is not rooted. Use `get` wherever possible.
    pub unsafe fn as_ptr(&self) -> *mut JSObject {
        self.heap.get()
    }

    pub fn eq_stack(&self, other: &Stack<'_, T>) -> bool {
        unsafe { self.as_ptr().eq(&other.as_raw()) }
    }
}

/// Rooting helpers for a nullable heap reference.
///
/// `Option<Heap<T>>` is the standard shape for a field that may be absent, and
/// reading one is always the same two steps: match out the `Some`, then root
/// what it holds. These methods mirror [`Heap::get`] and [`Heap::take`] so the
/// call site looks the same whether or not the field is optional.
pub trait OptionHeapExt<T: JSType> {
    /// Return a rooted handle to the contained item, if there is one.
    fn get<'s>(&self, scope: &'s Scope<'_>) -> Option<T::Rooted<'s>>;

    /// Store a new referent, replacing any previous one.
    fn set<'s>(&mut self, value: impl Into<Stack<'s, T>>);

    /// Return a rooted handle to the contained item, if there is one,
    /// and clear the field.
    ///
    /// Note: The name avoids colliding with the inherent [`Option::take`].
    fn take_rooted<'s>(&mut self, scope: &'s Scope<'_>) -> Option<T::Rooted<'s>>;
}

#[crate::allow_unrooted]
impl<T: JSType> OptionHeapExt<T> for Option<Heap<T>> {
    fn get<'s>(&self, scope: &'s Scope<'_>) -> Option<T::Rooted<'s>> {
        self.as_ref().map(|heap| heap.get(scope))
    }

    fn set<'s>(&mut self, value: impl Into<Stack<'s, T>>) {
        *self = Some(Heap::from(value.into()));
    }

    fn take_rooted<'s>(&mut self, scope: &'s Scope<'_>) -> Option<T::Rooted<'s>> {
        self.take().map(|heap| heap.take(scope))
    }
}

impl<T: JSType> std::cmp::Eq for Heap<T> {}
impl<T: JSType> std::cmp::PartialEq for Heap<T> {
    fn eq(&self, other: &Self) -> bool {
        unsafe { self.as_ptr() == other.as_ptr() }
    }
}

impl<T: JSType> std::cmp::PartialEq<Stack<'_, T>> for Heap<T> {
    fn eq(&self, other: &Stack<'_, T>) -> bool {
        unsafe { self.as_ptr() == other.as_raw() }
    }
}

impl<T: JSType> std::cmp::PartialEq<Heap<T>> for Stack<'_, T> {
    fn eq(&self, other: &Heap<T>) -> bool {
        unsafe { self.as_raw() == other.as_ptr() }
    }
}

/// Default creates a null `Heap`. Only intended for use by the proc
/// macro's setup-style constructor path, which initializes all fields
/// before the object is exposed. Calling `get()` on a default `Heap`
/// panics.
#[crate::allow_unrooted]
impl<T: HeapCell> Default for Heap<T>
where
    MozHeap<T::Cell>: Default,
{
    fn default() -> Self {
        Heap {
            heap: Box::pin(MozHeap::default()),
            _marker: PhantomData,
        }
    }
}

/// Create a `Heap<T>` from a rooted `Stack<'s, T>`.
///
/// The inner `MozHeap` is boxed, so the GC write barrier address is
/// stable regardless of subsequent moves.
#[crate::allow_unrooted]
impl<'s, T: JSType> From<Stack<'s, T>> for Heap<T> {
    fn from(stack: Stack<'s, T>) -> Self {
        let heap = Box::pin(MozHeap::default());
        (*heap).set(stack.as_raw());
        Heap {
            heap,
            _marker: PhantomData,
        }
    }
}

#[crate::allow_unrooted_interior]
unsafe impl<T: HeapCell> Trace for Heap<T>
where
    MozHeap<T::Cell>: Trace,
{
    #[inline]
    unsafe fn trace(&self, trc: *mut JSTracer) {
        self.heap.trace(trc);
    }
}

#[crate::allow_unrooted]
impl Heap<crate::native::Value> {
    /// Root the stored value in `scope`, returning a rooted handle.
    ///
    /// This is the safe way to read the value: the returned [`Handle`] keeps
    /// it alive for the scope's lifetime, so it is safe to hold across
    /// allocations.
    pub fn get<'s>(&self, scope: &'s Scope<'_>) -> Handle<'s, crate::native::Value> {
        scope.root_value(self.heap.get())
    }

    /// Store `v`. Safe to call repeatedly and at any time: the inner `MozHeap`
    /// is boxed, so its write barrier is registered at a stable address that
    /// survives moving this `Heap` into a struct, `Vec`, or box.
    pub fn set(&self, v: crate::native::Value) {
        (*self.heap).set(v);
    }

    /// Whether the stored value is `undefined` (a default-constructed
    /// `Heap<Value>`). Reads only the value's tag; the value is not retained,
    /// so this needs no rooting.
    pub fn is_undefined(&self) -> bool {
        self.heap.get().is_undefined()
    }

    /// A `Heap<Value>` is always initialized: its default is `undefined`, a
    /// valid JS value, so there is no null/uninitialized state to guard
    /// against.
    ///
    /// This method satisfies the `debug_assert_fully_initialized` hook that
    /// the `#[jsclass]` / `#[webidl_interface]` proc macros emit for bare
    /// `Heap<T>` fields.
    pub fn is_initialized(&self) -> bool {
        true
    }

    /// The stored value without rooting it.
    ///
    /// # Safety
    ///
    /// The returned [`Value`](crate::native::Value) is **not** rooted. If it is
    /// a GC pointer (object/string), the caller must consume it before any
    /// allocation can trigger a GC — e.g. set it straight into `rval` or a
    /// call's argument array — or root it via [`get`](Self::get). Holding it
    /// across an allocation is undefined behavior.
    pub unsafe fn as_raw(&self) -> crate::native::Value {
        self.heap.get()
    }
}

#[crate::allow_unrooted]
impl From<crate::native::Value> for Heap<crate::native::Value> {
    fn from(v: crate::native::Value) -> Self {
        let heap = Box::pin(MozHeap::default());
        (*heap).set(v);
        Heap {
            heap,
            _marker: PhantomData,
        }
    }
}

/// Panics on a null (default-constructed) `Heap`, as [`Heap::get`] does.
impl<T: JSType> ToJSVal<'_> for Heap<T> {
    #[inline]
    fn to_jsval_raw(&self, _scope: &Scope<'_>) -> Result<crate::value::Value, ConversionError> {
        let obj = self.heap.get();
        debug_assert!(
            !obj.is_null(),
            "converted a null (default-constructed) Heap"
        );
        Ok(ObjectValue(obj))
    }
}

impl ToJSVal<'_> for Heap<crate::native::Value> {
    #[inline]
    fn to_jsval_raw(&self, _scope: &Scope<'_>) -> Result<crate::value::Value, ConversionError> {
        Ok(self.heap.get())
    }
}

/// A GC-rooted boxed handle not tied to any `Scope`'s lifetime.
///
/// Use this to store a GC reference in locations that aren't traced themselves.
#[crate::allow_unrooted_interior]
pub struct RootedHeap<T: JSType> {
    boxed: RootedTraceableBox<MozHeap<*mut JSObject>>,
    _marker: PhantomData<T>,
}

impl<T: JSType> RootedHeap<T> {
    /// Create a rooted boxed handle for the given stack-rooted object.
    pub fn new<'s>(object: impl Into<Stack<'s, T>>) -> Self {
        let boxed = RootedTraceableBox::new(MozHeap::default());
        boxed.set(object.into().handle().get());
        Self {
            boxed,
            _marker: PhantomData,
        }
    }

    /// Root the referenced object, returning a stack-rooted handle.
    ///
    /// Returns `T::Rooted<'s>`, the stack-rooted type for `Heap<T>`.
    pub fn get<'s>(&self, scope: &'s Scope<'_>) -> T::Rooted<'s> {
        let obj = self.boxed.get();
        let handle = scope.root_object(NonNull::new(obj).expect("RootedHeap holds an object"));
        T::Rooted::from(Stack {
            handle,
            _marker: PhantomData,
        })
    }

    /// The referenced `JSObject`'s current address, for identity comparison.
    ///
    /// # Safety
    ///
    /// The returned pointer is not rooted. It must not be dereferenced or held across anything
    /// that can GC. Use [`get`](Self::get) for any other use.
    pub unsafe fn as_ptr(&self) -> *mut JSObject {
        self.boxed.get()
    }
}

impl<T: JSType> std::cmp::Eq for RootedHeap<T> {}
impl<T: JSType> std::cmp::PartialEq for RootedHeap<T> {
    fn eq(&self, other: &Self) -> bool {
        unsafe { self.as_ptr() == other.as_ptr() }
    }
}

impl<T: JSType> std::cmp::PartialEq<Stack<'_, T>> for RootedHeap<T> {
    fn eq(&self, other: &Stack<'_, T>) -> bool {
        unsafe { self.as_ptr() == other.as_raw() }
    }
}

impl<T: JSType> std::cmp::PartialEq<RootedHeap<T>> for Stack<'_, T> {
    fn eq(&self, other: &RootedHeap<T>) -> bool {
        unsafe { self.as_raw() == other.as_ptr() }
    }
}
