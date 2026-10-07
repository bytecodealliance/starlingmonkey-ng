// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Typed array, `ArrayBuffer`, and `ArrayBufferView` creation and access.
//!
//! Every JS buffer type — `ArrayBuffer`, `SharedArrayBuffer`, and each
//! concrete typed-array view (`Uint8Array`, `Int32Array`, …) — has a marker
//! struct in this module that implements [`JSType`]. The scope-rooted handle
//! type is [`Stack<'s, Marker>`](crate::gc::handle::Stack), exposed at the
//! crate root as the [`js::Uint8Array<'s>`](crate::Uint8Array) family of
//! aliases.
//!
//! [`ArrayBufferView`] is the umbrella marker for the union of all view
//! types. Use [`ArrayBufferView::from_object`] to test whether an object is
//! any view (typed array or `DataView`) and obtain a typed handle exposing
//! byte-level access.
//!
//! # Example
//!
//! ```ignore
//! use js::{Uint8Array, ArrayBuffer};
//!
//! let bytes = Uint8Array::with_data(&scope, b"hello")?;
//! let buf = ArrayBuffer::new(&scope, 1024)?;
//! ```
//!
//! # Safety
//!
//! Methods that hand out raw slices into a buffer's backing store
//! (`data`, `data_mut`) are `unsafe`: callers must ensure that no GC runs
//! and the buffer is not detached or transferred while the slice is live.
//! Use [`copy_bytes`](Stack::copy_bytes) when you want an owned copy.
//!
//! The underlying SpiderMonkey element-type tags
//! (`mozjs::typedarray::{Uint8, Int32, …}`) and the generic
//! `TypedArray<T, S>` wrapper are an internal implementation detail of this
//! module and are not part of the public surface.

use std::os::raw::c_void;
use std::ptr::NonNull;

use crate::builtins::JSType;
use crate::conversion::{ConversionError, FromJSVal};
use crate::gc::handle::Stack;
use crate::gc::scope::Scope;
use crate::Object;
use mozjs::gc::{HandleObject, HandleValue};
use mozjs::jsapi::{JSClass, JSObject, JSProtoKey, JS};
use mozjs::rust::wrappers2;
use mozjs::typedarray::{
    ClampedU8, Float32, Float64, Int16, Int32, Int8, TypedArrayElement, TypedArrayElementCreator,
    Uint16, Uint32, Uint8,
};

use super::error::ExnThrown;

// ---------------------------------------------------------------------------
// ArrayBuffer
// ---------------------------------------------------------------------------

/// Byte storage that can back an external (engine-owned, JS-writable)
/// `ArrayBuffer` — see [`ArrayBuffer::from_external`](crate::ArrayBuffer).
///
/// # Safety
///
/// Implementors must guarantee, for as long as the value is owned by the
/// buffer:
///
/// - **Exclusive ownership** of the bytes `as_mut_slice` exposes: no other
///   handle (a `Bytes`/`Arc` clone, a cached copy) can read or write them. JS
///   gets full write access to the buffer, so shared storage would let script
///   mutate memory other owners see as immutable.
/// - **Heap-backed, address-stable** bytes: the slice must live behind the
///   (boxed, so itself immovable) value, not inline in it, so the pointer
///   handed to the engine stays valid.
pub unsafe trait ExternalBytes: 'static {
    /// Exclusive access to the backing bytes.
    fn as_mut_slice(&mut self) -> &mut [u8];
}

// SAFETY: a `Vec` exclusively owns its heap allocation.
unsafe impl ExternalBytes for Vec<u8> {
    fn as_mut_slice(&mut self) -> &mut [u8] {
        self
    }
}

// SAFETY: a `Box<[u8]>` exclusively owns its heap allocation.
unsafe impl ExternalBytes for Box<[u8]> {
    fn as_mut_slice(&mut self) -> &mut [u8] {
        self
    }
}

/// Marker type for JavaScript `ArrayBuffer` objects.
///
/// Scope-rooted handle type: [`js::ArrayBuffer<'s>`](crate::ArrayBuffer).
pub struct ArrayBuffer;

impl JSType for ArrayBuffer {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "ArrayBuffer";

    fn js_class() -> *const JSClass {
        crate::class::proto_key_to_class(JSProtoKey::JSProto_ArrayBuffer)
    }

    /// Fixed-length and resizable `ArrayBuffer`s have different classes.
    #[inline]
    unsafe fn is_instance(obj: *mut JSObject) -> bool {
        unsafe { JS::IsArrayBufferObject(obj) }
    }
}

impl<'s> Stack<'s, ArrayBuffer> {
    /// Create a new `ArrayBuffer` with the given byte length.
    pub fn new(scope: &'s Scope<'_>, byte_length: usize) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::NewArrayBuffer(scope.cx_mut(), byte_length) };
        root_or_throw(scope, obj)
    }

    /// Create a new `ArrayBuffer` with the given bytes copied into it.
    pub fn with_data(scope: &'s Scope<'_>, data: &[u8]) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::NewArrayBuffer(scope.cx_mut(), data.len()) };
        let nn = NonNull::new(obj).ok_or(ExnThrown)?;
        if !data.is_empty() {
            // SAFETY: nn was just created with `data.len()` bytes; we have a
            // unique reference to it before exposing it to JS.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    buffer_data_ptr(nn.as_ptr()),
                    data.len(),
                );
            }
        }
        Ok(unsafe { Self::from_handle_unchecked(scope.root_object(nn)) })
    }

    /// Create a new `ArrayBuffer` whose contents are borrowed from the caller.
    ///
    /// The returned buffer references `data` without copying. The caller
    /// must ensure `data` outlives the buffer and is not mutated through
    /// any other reference while in use.
    ///
    /// # Safety
    ///
    /// `data` must remain valid for as long as JS can reach the buffer.
    pub unsafe fn with_user_owned_contents(
        scope: &'s Scope<'_>,
        data: &[u8],
    ) -> Result<Self, ExnThrown> {
        let obj = wrappers2::NewArrayBufferWithUserOwnedContents(
            scope.cx_mut(),
            data.len(),
            data.as_ptr() as *mut c_void,
        );
        root_or_throw(scope, obj)
    }

    /// Create a new `ArrayBuffer` from `data`, without copying where possible.
    ///
    /// `data` is used as-is if its alignment meets the engine's ArrayBuffer alignment
    /// requirements (8). Otherwise, the contents are copied into a new buffer, and
    /// `data` is dropped immediately.
    ///
    /// In either case, when `data` is dropped (either immediately or once the resulting
    /// `ArrayBuffer`'s destructor runs), `data`'s free callback is run to drop `data`.
    pub fn from_external<D>(scope: &'s Scope<'_>, data: D) -> Result<Self, ExnThrown>
    where
        D: ExternalBytes,
    {
        // Drops the boxed `D` behind `user_data`.
        unsafe extern "C" fn free_external<D>(_contents: *mut c_void, user_data: *mut c_void) {
            drop(Box::from_raw(user_data as *mut D));
        }

        // External ArrayBuffer contents must be aligned to the engine's `ARRAY_BUFFER_ALIGNMENT`.
        const ARRAY_BUFFER_ALIGNMENT: usize = 8;
        let mut boxed = Box::new(data);
        // The contents pointer is read off the *boxed* value — the address the free
        // callback's box will own — never the pre-move one.
        let slice = boxed.as_mut_slice();
        let len = slice.len();
        // An external buffer needs non-null contents; an empty body is a plain empty buffer.
        if len == 0 {
            return Self::new(scope, 0);
        }
        // Unaligned data cannot back an external buffer; copy it into a normal one.
        if !(slice.as_ptr() as usize).is_multiple_of(ARRAY_BUFFER_ALIGNMENT) {
            return Self::with_data(scope, slice);
        }
        let ptr = slice.as_mut_ptr() as *mut c_void;
        let user_data = Box::into_raw(boxed) as *mut c_void;
        let obj = unsafe {
            wrappers2::NewExternalArrayBuffer(
                scope.cx_mut(),
                len,
                ptr,
                Some(free_external::<D>),
                user_data,
            )
        };
        match NonNull::new(obj) {
            // SAFETY: `obj` is a freshly created, rooted ArrayBuffer.
            Some(nn) => Ok(unsafe { Self::from_handle_unchecked(scope.root_object(nn)) }),
            None => {
                // The engine did not take ownership; reclaim and drop the data ourselves.
                unsafe { drop(Box::from_raw(user_data as *mut D)) };
                Err(ExnThrown)
            }
        }
    }

    /// Copy an existing `ArrayBuffer`, returning a new independent buffer.
    pub fn copy_from(scope: &'s Scope<'_>, src: HandleObject) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::CopyArrayBuffer(scope.cx_mut(), src) };
        root_or_throw(scope, obj)
    }

    /// Detach this `ArrayBuffer`, making it zero-length.
    pub fn detach(self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::DetachArrayBuffer(scope.cx_mut(), self.handle()) };
        ExnThrown::check(ok)
    }

    /// Get the byte length of this `ArrayBuffer`.
    pub fn byte_length(self) -> usize {
        unsafe { JS::GetArrayBufferByteLength(self.as_raw()) }
    }

    /// Whether this buffer has been detached.
    pub fn is_detached(self) -> bool {
        unsafe { JS::IsDetachedArrayBufferObject(self.as_raw()) }
    }

    /// Borrow the buffer's backing bytes.
    ///
    /// # Safety
    ///
    /// No GC may run and the buffer must not be detached or transferred
    /// while the returned slice is live.
    pub unsafe fn bytes(self) -> &'s [u8] {
        let (ptr, len) = array_buffer_length_and_data(self.as_raw());
        if ptr.is_null() || len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(ptr, len)
        }
    }

    /// Mutably borrow the buffer's backing bytes.
    ///
    /// # Safety
    ///
    /// No GC may run and the buffer must not be detached or transferred
    /// while the returned slice is live. Concurrent access via another
    /// reference is undefined behaviour.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn bytes_mut(self) -> &'s mut [u8] {
        let (ptr, len) = array_buffer_length_and_data(self.as_raw());
        if ptr.is_null() || len == 0 {
            &mut []
        } else {
            std::slice::from_raw_parts_mut(ptr, len)
        }
    }

    /// Copy the buffer's contents into an owned `Vec`.
    pub fn copy_bytes(self) -> Vec<u8> {
        unsafe { self.bytes() }.to_vec()
    }

    /// Transfer this buffer's contents into a new `ArrayBuffer`, detaching the
    /// receiver.
    ///
    /// This is the WHATWG abstract operation `TransferArrayBuffer`: the returned
    /// buffer holds the receiver's bytes and the receiver is left detached
    /// (zero-length). The receiver must not already be detached. The contents are
    /// moved, not copied.
    pub fn transfer(self, scope: &'s Scope<'_>) -> Result<Self, ExnThrown> {
        let length = self.byte_length();
        // A zero-length buffer may have no contents to steal. Hand back a fresh empty buffer
        // instead. The receiver is still detached, matching `TransferArrayBuffer`'s observable
        // effect.
        if length == 0 {
            self.detach(scope)?;
            return Self::new(scope, 0);
        }
        // SAFETY: `self` is a rooted `ArrayBuffer`. Stealing detaches it and returns its
        // contents, or null with an exception pending.
        let contents =
            unsafe { wrappers2::StealArrayBufferContents(scope.cx_mut(), self.handle()) };
        if contents.is_null() {
            return Err(ExnThrown);
        }
        // SAFETY: `contents` holds `length` bytes allocated by the JS allocator, as
        // `StealArrayBufferContents` returns them. The call takes ownership of `contents`,
        // including freeing it if creating the buffer fails.
        let obj =
            unsafe { wrappers2::NewArrayBufferWithContents(scope.cx_mut(), length, contents) };
        root_or_throw(scope, obj)
    }

    /// Clone the region `[byte_offset, byte_offset + length)` of this buffer into
    /// a new `ArrayBuffer`.
    ///
    /// This is the streams spec's `CloneArrayBuffer(buffer, byteOffset, length,
    /// %ArrayBuffer%)`. The region must lie within this buffer, which must not be
    /// detached.
    pub fn clone_region(
        self,
        scope: &'s Scope<'_>,
        byte_offset: usize,
        length: usize,
    ) -> Result<Self, ExnThrown> {
        let out = Self::new(scope, length)?;
        if length != 0 {
            // SAFETY: both buffers are live and non-detached, the source region
            // is within bounds (caller-validated), and no GC runs between the
            // two borrows because no allocation happens here.
            unsafe {
                let src = self.bytes();
                let dst = out.bytes_mut();
                dst.copy_from_slice(&src[byte_offset..byte_offset + length]);
            }
        }
        Ok(out)
    }
}

crate::gc::handle::deref_to_object!(ArrayBuffer);

crate::gc::handle::from_jsval_via_cast!(ArrayBuffer, c"Value isn't an ArrayBuffer");

// ---------------------------------------------------------------------------
// SharedArrayBuffer
// ---------------------------------------------------------------------------

/// Marker type for JavaScript `SharedArrayBuffer` objects.
pub struct SharedArrayBuffer;

impl JSType for SharedArrayBuffer {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "SharedArrayBuffer";

    fn js_class() -> *const JSClass {
        crate::class::proto_key_to_class(JSProtoKey::JSProto_SharedArrayBuffer)
    }
}

impl<'s> Stack<'s, SharedArrayBuffer> {
    /// Create a new `SharedArrayBuffer` with the given byte length.
    pub fn new(scope: &'s Scope<'_>, byte_length: usize) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::NewSharedArrayBuffer(scope.cx_mut(), byte_length) };
        root_or_throw(scope, obj)
    }

    /// Borrow the buffer's backing bytes.
    ///
    /// # Safety
    ///
    /// No GC may run while the returned slice is live. Other threads sharing the buffer can
    /// write to it concurrently.
    pub unsafe fn bytes(self) -> &'s [u8] {
        let mut len = 0;
        let mut is_shared = false;
        let mut ptr = std::ptr::null_mut();
        JS::GetSharedArrayBufferLengthAndData(self.as_raw(), &mut len, &mut is_shared, &mut ptr);
        if ptr.is_null() || len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(ptr, len)
        }
    }
}

crate::gc::handle::deref_to_object!(SharedArrayBuffer);

crate::gc::handle::from_jsval_via_cast!(SharedArrayBuffer, c"Value isn't a SharedArrayBuffer");

// ---------------------------------------------------------------------------
// ArrayBufferView — umbrella for typed arrays and DataView
// ---------------------------------------------------------------------------

/// Marker type for any JavaScript `ArrayBufferView` (typed array or
/// `DataView`).
///
/// This is a Web IDL union, not a single JS class, so the [`JSType`] impl
/// overrides [`is_instance`](JSType::is_instance) to delegate to
/// `JS_IsArrayBufferViewObject`. [`js_class`](JSType::js_class) returns the
/// abstract `%TypedArray%` prototype's class as a stable identity for
/// purposes that need a class pointer.
pub struct ArrayBufferView;

impl JSType for ArrayBufferView {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "ArrayBufferView";

    fn js_class() -> *const JSClass {
        crate::class::proto_key_to_class(JSProtoKey::JSProto_TypedArray)
    }

    #[inline]
    unsafe fn is_instance(obj: *mut JSObject) -> bool {
        unsafe { mozjs::jsapi::JS_IsArrayBufferViewObject(obj) }
    }
}

/// The element kind of an [`ArrayBufferView`]: one of the typed-array element
/// types, or `DataView`.
///
/// This captures the "typed array constructors table" distinctions the streams
/// spec needs (`element size` and `view constructor`) without exposing the raw
/// SpiderMonkey `Scalar::Type` tags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewKind {
    Int8,
    Uint8,
    Uint8Clamped,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float16,
    Float32,
    Float64,
    BigInt64,
    BigUint64,
    DataView,
}

impl ViewKind {
    /// Whether this is an integer typed array kind: `Int8Array`, `Uint8Array`,
    /// `Uint8ClampedArray`, `Int16Array`, `Uint16Array`, `Int32Array`, `Uint32Array`,
    /// `BigInt64Array` or `BigUint64Array`.
    pub fn is_integer_array(self) -> bool {
        matches!(
            self,
            ViewKind::Int8
                | ViewKind::Uint8
                | ViewKind::Uint8Clamped
                | ViewKind::Int16
                | ViewKind::Uint16
                | ViewKind::Int32
                | ViewKind::Uint32
                | ViewKind::BigInt64
                | ViewKind::BigUint64
        )
    }

    /// The name of the view kind's constructor, such as `Uint8Array`.
    pub fn name(self) -> &'static str {
        match self {
            ViewKind::Int8 => "Int8Array",
            ViewKind::Uint8 => "Uint8Array",
            ViewKind::Uint8Clamped => "Uint8ClampedArray",
            ViewKind::Int16 => "Int16Array",
            ViewKind::Uint16 => "Uint16Array",
            ViewKind::Int32 => "Int32Array",
            ViewKind::Uint32 => "Uint32Array",
            ViewKind::Float16 => "Float16Array",
            ViewKind::Float32 => "Float32Array",
            ViewKind::Float64 => "Float64Array",
            ViewKind::BigInt64 => "BigInt64Array",
            ViewKind::BigUint64 => "BigUint64Array",
            ViewKind::DataView => "DataView",
        }
    }

    /// The size in bytes of one element of this view kind. `DataView` has an
    /// element size of 1, matching the streams spec.
    pub fn element_size(self) -> usize {
        match self {
            ViewKind::Int8 | ViewKind::Uint8 | ViewKind::Uint8Clamped | ViewKind::DataView => 1,
            ViewKind::Int16 | ViewKind::Uint16 | ViewKind::Float16 => 2,
            ViewKind::Int32 | ViewKind::Uint32 | ViewKind::Float32 => 4,
            ViewKind::Float64 | ViewKind::BigInt64 | ViewKind::BigUint64 => 8,
        }
    }

    /// Whether this kind is a typed array (i.e. not `DataView`).
    pub fn is_typed_array(self) -> bool {
        self != ViewKind::DataView
    }
}

impl<'s> Stack<'s, ArrayBufferView> {
    /// Wrap `obj` as an `ArrayBufferView` handle if it is a typed array or
    /// `DataView`, otherwise return `None`.
    pub fn from_object(obj: Object<'s>) -> Option<Self> {
        obj.cast::<Self>().ok()
    }

    /// `value` as an `ArrayBufferView` handle if it is a typed array or `DataView`, otherwise
    /// `None`.
    pub fn from_value(scope: &'s Scope<'_>, value: HandleValue<'_>) -> Option<Self> {
        Object::from_value(scope, value.get())
            .ok()
            .and_then(Self::from_object)
    }

    /// Get the byte length of this view.
    pub fn byte_length(self) -> usize {
        unsafe { mozjs::jsapi::JS_GetArrayBufferViewByteLength(self.as_raw()) }
    }

    /// Get the byte offset of this view into its underlying buffer.
    pub fn byte_offset(self) -> usize {
        unsafe { mozjs::jsapi::JS_GetArrayBufferViewByteOffset(self.as_raw()) }
    }

    /// Whether this view's buffer is a `SharedArrayBuffer`.
    pub fn is_shared(self) -> bool {
        // SAFETY: `self` is a live, rooted array buffer view.
        unsafe { JS::IsArrayBufferViewShared(self.as_raw()) }
    }

    /// Get this view's underlying `ArrayBuffer` (its `[[ViewedArrayBuffer]]`).
    ///
    /// Materialising the buffer can allocate (a view created over inline data
    /// gets a buffer object on demand), so a context is required and the result
    /// is rooted.
    pub fn viewed_buffer(self, scope: &'s Scope<'_>) -> Result<Stack<'s, ArrayBuffer>, ExnThrown> {
        let mut is_shared = false;
        // SAFETY: `self` is a live, rooted array buffer view.
        let obj = unsafe {
            wrappers2::JS_GetArrayBufferViewBuffer(scope.cx_mut(), self.handle(), &mut is_shared)
        };
        root_or_throw(scope, obj)
    }

    /// The element kind of this view ([`ViewKind`]).
    pub fn view_kind(self) -> ViewKind {
        use mozjs::jsapi::JS::Scalar::Type;
        // SAFETY: `self` is a live, rooted array buffer view.
        if !unsafe { mozjs::jsapi::JS_IsTypedArrayObject(self.as_raw()) } {
            return ViewKind::DataView;
        }
        // SAFETY: `self` is a typed array (checked above), so its element type is
        // one of the typed-array `Scalar::Type` tags.
        match unsafe { mozjs::jsapi::JS_GetArrayBufferViewType(self.as_raw()) } {
            Type::Int8 => ViewKind::Int8,
            Type::Uint8 => ViewKind::Uint8,
            Type::Uint8Clamped => ViewKind::Uint8Clamped,
            Type::Int16 => ViewKind::Int16,
            Type::Uint16 => ViewKind::Uint16,
            Type::Int32 => ViewKind::Int32,
            Type::Uint32 => ViewKind::Uint32,
            Type::Float16 => ViewKind::Float16,
            Type::Float32 => ViewKind::Float32,
            Type::Float64 => ViewKind::Float64,
            Type::BigInt64 => ViewKind::BigInt64,
            Type::BigUint64 => ViewKind::BigUint64,
            _ => unreachable!(
                "JS_GetArrayBufferViewType returned non-typed-array type for a typed array view"
            ),
        }
    }

    /// The number of elements in this view: its `[[ArrayLength]]` for a typed
    /// array, or its byte length for a `DataView` (which has element size 1).
    pub fn array_length(self) -> usize {
        self.byte_length() / self.view_kind().element_size()
    }

    /// Borrow the view's bytes.
    ///
    /// # Safety
    ///
    /// No GC may run and the buffer must not be detached while the
    /// returned slice is live.
    pub unsafe fn bytes(self) -> &'s [u8] {
        let length = self.byte_length();
        if length == 0 {
            return &[];
        }
        let mut is_shared = false;
        let nogc = JS::AutoRequireNoGC { _address: 0 };
        let data = mozjs::jsapi::JS_GetArrayBufferViewData(self.as_raw(), &mut is_shared, &nogc);
        if data.is_null() {
            &[]
        } else {
            std::slice::from_raw_parts(data as *const u8, length)
        }
    }

    /// Mutably borrow the view's bytes.
    ///
    /// # Safety
    ///
    /// No GC may run and the buffer must not be detached while the
    /// returned slice is live. Concurrent access via another reference is
    /// undefined behaviour.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn bytes_mut(self) -> &'s mut [u8] {
        let length = self.byte_length();
        if length == 0 {
            return &mut [];
        }
        let mut is_shared = false;
        let nogc = JS::AutoRequireNoGC { _address: 0 };
        let data = mozjs::jsapi::JS_GetArrayBufferViewData(self.as_raw(), &mut is_shared, &nogc);
        if data.is_null() {
            &mut []
        } else {
            std::slice::from_raw_parts_mut(data as *mut u8, length)
        }
    }

    /// Copy the view's bytes into an owned `Vec`.
    pub fn copy_bytes(self) -> Vec<u8> {
        unsafe { self.bytes() }.to_vec()
    }
}

/// Construct an [`ArrayBufferView`] of the given [`ViewKind`] over the region of
/// `buffer` starting at `byte_offset`.
///
/// For a typed array, `length` is the element count; for a `DataView`, it is the
/// byte length. This is the streams spec's `Construct(view constructor, «buffer,
/// byteOffset, length»)`.
pub fn construct_view<'s>(
    scope: &'s Scope<'_>,
    kind: ViewKind,
    buffer: Stack<'_, ArrayBuffer>,
    byte_offset: usize,
    length: usize,
) -> Result<Object<'s>, ExnThrown> {
    let cx = scope.cx_mut();
    let buf = buffer.handle();
    let len = length as i64;
    // SAFETY: `buffer` is a live, rooted, non-detached `ArrayBuffer`; the region
    // is validated by the caller (the streams pull-into machinery).
    let obj = unsafe {
        match kind {
            ViewKind::Int8 => wrappers2::JS_NewInt8ArrayWithBuffer(cx, buf, byte_offset, len),
            ViewKind::Uint8 => wrappers2::JS_NewUint8ArrayWithBuffer(cx, buf, byte_offset, len),
            ViewKind::Uint8Clamped => {
                wrappers2::JS_NewUint8ClampedArrayWithBuffer(cx, buf, byte_offset, len)
            }
            ViewKind::Int16 => wrappers2::JS_NewInt16ArrayWithBuffer(cx, buf, byte_offset, len),
            ViewKind::Uint16 => wrappers2::JS_NewUint16ArrayWithBuffer(cx, buf, byte_offset, len),
            ViewKind::Float16 => wrappers2::JS_NewFloat16ArrayWithBuffer(cx, buf, byte_offset, len),
            ViewKind::Int32 => wrappers2::JS_NewInt32ArrayWithBuffer(cx, buf, byte_offset, len),
            ViewKind::Uint32 => wrappers2::JS_NewUint32ArrayWithBuffer(cx, buf, byte_offset, len),
            ViewKind::Float32 => wrappers2::JS_NewFloat32ArrayWithBuffer(cx, buf, byte_offset, len),
            ViewKind::Float64 => wrappers2::JS_NewFloat64ArrayWithBuffer(cx, buf, byte_offset, len),
            ViewKind::BigInt64 => {
                wrappers2::JS_NewBigInt64ArrayWithBuffer(cx, buf, byte_offset, len)
            }
            ViewKind::BigUint64 => {
                wrappers2::JS_NewBigUint64ArrayWithBuffer(cx, buf, byte_offset, len)
            }
            ViewKind::DataView => wrappers2::JS_NewDataView(cx, buf, byte_offset, length),
        }
    };
    root_or_throw(scope, obj)
}

crate::gc::handle::deref_to_object!(ArrayBufferView);

crate::gc::handle::from_jsval_via_cast!(ArrayBufferView, c"Value isn't a typed array or DataView");

/// Copy the bytes held by any `BufferSource` (an `ArrayBuffer`,
/// `SharedArrayBuffer`, or `ArrayBufferView`) into an owned `Vec`.
///
/// Returns `None` if `obj` is none of the above.
pub fn copy_bytes_if_buffer_source(obj: Object<'_>) -> Option<Vec<u8>> {
    // SAFETY: All functions called following JSAPI's contract.
    unsafe {
        if JS::IsArrayBufferObject(obj.as_raw()) {
            let (ptr, len) = array_buffer_length_and_data(obj.as_raw());
            if ptr.is_null() || len == 0 {
                return Some(Vec::new());
            }
            return Some(std::slice::from_raw_parts(ptr, len).to_vec());
        }
        if JS::IsSharedArrayBufferObject(obj.as_raw()) {
            let mut len = 0;
            let mut is_shared = false;
            let mut ptr = std::ptr::null_mut();
            JS::GetSharedArrayBufferLengthAndData(obj.as_raw(), &mut len, &mut is_shared, &mut ptr);
            if ptr.is_null() || len == 0 {
                return Some(Vec::new());
            }
            return Some(std::slice::from_raw_parts(ptr, len).to_vec());
        }
        if mozjs::jsapi::JS_IsArrayBufferViewObject(obj.as_raw()) {
            let view: Stack<'_, ArrayBufferView> = Stack::from_handle_unchecked(obj.handle());
            return Some(view.copy_bytes());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Typed array specializations
// ---------------------------------------------------------------------------

/// Trait implemented by every concrete typed-array marker (`Uint8Array`,
/// `Int32Array`, …). It associates the marker with its element type and the
/// SpiderMonkey routines for creating and inspecting arrays of that type.
///
/// This trait is `pub` so callers can write generic code, but the only
/// implementors are the markers defined in this module.
pub trait TypedArrayKind: JSType {
    /// The Rust primitive type of one element.
    type Element: Copy;

    /// Create a new typed array of this kind with `length` elements.
    ///
    /// The context must have a realm entered.
    fn create_new(cx: &mut mozjs::context::JSContext, length: usize) -> *mut JSObject;

    /// Get the data pointer and element count of an existing typed array.
    ///
    /// A detached array yields a null pointer and a length of zero.
    ///
    /// # Safety
    ///
    /// `obj` must be a non-null typed array of this kind.
    unsafe fn length_and_data(obj: *mut JSObject) -> (*mut Self::Element, usize);
}

impl<'s, T: TypedArrayKind> Stack<'s, T> {
    /// Create a new typed array with `length` elements (zero-initialized).
    pub fn new(scope: &'s Scope<'_>, length: usize) -> Result<Self, ExnThrown> {
        let obj = T::create_new(scope.cx_mut(), length);
        root_or_throw(scope, obj)
    }

    /// Create a new typed array pre-populated with `data`.
    pub fn with_data(scope: &'s Scope<'_>, data: &[T::Element]) -> Result<Self, ExnThrown> {
        let obj = T::create_new(scope.cx_mut(), data.len());
        let nn = NonNull::new(obj).ok_or(ExnThrown)?;
        if !data.is_empty() {
            // SAFETY: just-created array of `data.len()` elements; we have
            // unique access before exposing it to JS.
            unsafe {
                let (buf, _) = T::length_and_data(obj);
                std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
            }
        }
        Ok(unsafe { Self::from_handle_unchecked(scope.root_object(nn)) })
    }

    /// Number of elements in this typed array.
    pub fn len(self) -> usize {
        // SAFETY: self is a rooted handle to a typed array of kind T.
        unsafe { T::length_and_data(self.as_raw()) }.1
    }

    /// Whether this typed array has zero elements.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Byte length of the underlying buffer view.
    pub fn byte_length(self) -> usize {
        // SAFETY: self is a rooted handle to an ArrayBufferView.
        unsafe { mozjs::jsapi::JS_GetArrayBufferViewByteLength(self.as_raw()) }
    }

    /// Borrow the typed array's elements.
    ///
    /// # Safety
    ///
    /// No GC may run and the underlying buffer must not be detached while
    /// the returned slice is live.
    pub unsafe fn as_slice(self) -> &'s [T::Element] {
        let (ptr, len) = T::length_and_data(self.as_raw());
        if ptr.is_null() || len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(ptr, len)
        }
    }

    /// Mutably borrow the typed array's elements.
    ///
    /// # Safety
    ///
    /// No GC may run and the underlying buffer must not be detached while
    /// the returned slice is live. Concurrent access via another reference
    /// is undefined behaviour.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn as_mut_slice(self) -> &'s mut [T::Element] {
        let (ptr, len) = T::length_and_data(self.as_raw());
        if ptr.is_null() || len == 0 {
            &mut []
        } else {
            std::slice::from_raw_parts_mut(ptr, len)
        }
    }

    /// View this typed array through the abstract `ArrayBufferView` handle.
    ///
    /// Every typed array is an `ArrayBufferView`, so this is infallible.
    pub fn as_array_buffer_view(self) -> Stack<'s, ArrayBufferView> {
        unsafe { Stack::<ArrayBufferView>::from_handle_unchecked(self.handle()) }
    }
}

impl<'s> Stack<'s, Uint8Array> {
    /// Construct a `Uint8Array` viewing the region `[byte_offset,
    /// byte_offset + length)` of `buffer`, without copying.
    ///
    /// This is the streams spec's `Construct(%Uint8Array%, « buffer, byteOffset,
    /// length »)`. The region must lie within `buffer`'s byte length.
    pub fn with_buffer(
        scope: &'s Scope<'_>,
        buffer: Stack<'_, ArrayBuffer>,
        byte_offset: usize,
        length: usize,
    ) -> Result<Self, ExnThrown> {
        // SAFETY: `buffer` is a live, rooted, non-detached `ArrayBuffer`.
        let obj = unsafe {
            wrappers2::JS_NewUint8ArrayWithBuffer(
                scope.cx_mut(),
                buffer.handle(),
                byte_offset,
                length as i64,
            )
        };
        root_or_throw(scope, obj)
    }

    /// Construct a `Uint8Array` viewing `buffer` from `byte_offset` to its end, without copying.
    ///
    /// This is `new Uint8Array(buffer, byteOffset)`: for a resizable `buffer`, the view's length
    /// follows the buffer's. `byte_offset` must be at most `buffer`'s byte length.
    pub fn with_buffer_to_end(
        scope: &'s Scope<'_>,
        buffer: Stack<'_, ArrayBuffer>,
        byte_offset: usize,
    ) -> Result<Self, ExnThrown> {
        // SAFETY: `buffer` is a live, rooted, non-detached `ArrayBuffer`. A length of -1 views the
        // buffer to its end.
        let obj = unsafe {
            wrappers2::JS_NewUint8ArrayWithBuffer(scope.cx_mut(), buffer.handle(), byte_offset, -1)
        };
        root_or_throw(scope, obj)
    }
}

macro_rules! typed_array_marker {
    ($Marker:ident, $name:literal, $proto:ident, $tag:ty) => {
        #[doc = concat!("Marker type for JavaScript `", $name, "` objects.")]
        pub struct $Marker;

        impl JSType for $Marker {
            type Rooted<'s> = Stack<'s, Self>;
            const JS_NAME: &'static str = $name;

            fn js_class() -> *const JSClass {
                crate::class::proto_key_to_class(JSProtoKey::$proto)
            }

            /// Views of fixed-length and of resizable buffers have different classes. Both are
            /// instances.
            unsafe fn is_instance(obj: *mut JSObject) -> bool {
                // SAFETY: `obj` is a valid object pointer, per the trait's contract.
                !unsafe { <$tag as TypedArrayElement>::unwrap_array(obj) }.is_null()
            }
        }

        impl TypedArrayKind for $Marker {
            type Element = <$tag as TypedArrayElement>::Element;

            fn create_new(cx: &mut mozjs::context::JSContext, length: usize) -> *mut JSObject {
                <$tag as TypedArrayElementCreator>::create_new(cx, length)
            }

            unsafe fn length_and_data(obj: *mut JSObject) -> (*mut Self::Element, usize) {
                match <$tag as TypedArrayElement>::length_and_data(obj) {
                    Some(data) => (data.as_ptr().cast(), data.len()),
                    None => (std::ptr::null_mut(), 0),
                }
            }
        }

        impl<'s> std::ops::Deref for Stack<'s, $Marker> {
            type Target = Object<'s>;

            fn deref(&self) -> &Object<'s> {
                // SAFETY: both wrappers are repr(transparent) over the same handle.
                unsafe { &*(self as *const Stack<'s, $Marker> as *const Object<'s>) }
            }
        }

        impl<'s, 'v> FromJSVal<'s, 'v> for Stack<'s, $Marker> {
            type Config = ();

            fn from_jsval(
                scope: &'s Scope<'_>,
                val: HandleValue<'v>,
                _option: Self::Config,
            ) -> Result<Self, ConversionError> {
                Object::from_value(scope, *val)?
                    .cast::<Self>()
                    .map_err(|_| {
                        const MSG: &std::ffi::CStr = unsafe {
                            std::ffi::CStr::from_bytes_with_nul_unchecked(
                                concat!("Value isn't a ", $name, "\0").as_bytes(),
                            )
                        };
                        ConversionError::Failure(std::borrow::Cow::Borrowed(MSG))
                    })
            }
        }
    };
}

typed_array_marker!(Int8Array, "Int8Array", JSProto_Int8Array, Int8);
typed_array_marker!(Uint8Array, "Uint8Array", JSProto_Uint8Array, Uint8);
typed_array_marker!(
    Uint8ClampedArray,
    "Uint8ClampedArray",
    JSProto_Uint8ClampedArray,
    ClampedU8
);
typed_array_marker!(Int16Array, "Int16Array", JSProto_Int16Array, Int16);
typed_array_marker!(Uint16Array, "Uint16Array", JSProto_Uint16Array, Uint16);
typed_array_marker!(Int32Array, "Int32Array", JSProto_Int32Array, Int32);
typed_array_marker!(Uint32Array, "Uint32Array", JSProto_Uint32Array, Uint32);
typed_array_marker!(Float32Array, "Float32Array", JSProto_Float32Array, Float32);
typed_array_marker!(Float64Array, "Float64Array", JSProto_Float64Array, Float64);

/// The 64-bit integer kinds, which `mozjs::typedarray` has no element tags for,
/// so the marker is written out against the JSAPI entry points directly.
// TODO: add element tags to mozjs and remove the need for this.
macro_rules! bigint_typed_array_marker {
    ($Marker:ident, $name:literal, $proto:ident, $elem:ty, $new:ident, $data:ident, $unwrap:ident) => {
        #[doc = concat!("Marker type for JavaScript `", $name, "` objects.")]
        pub struct $Marker;

        impl JSType for $Marker {
            type Rooted<'s> = Stack<'s, Self>;
            const JS_NAME: &'static str = $name;

            fn js_class() -> *const JSClass {
                crate::class::proto_key_to_class(JSProtoKey::$proto)
            }

            /// Views of fixed-length and of resizable buffers have different classes. Both are
            /// instances.
            unsafe fn is_instance(obj: *mut JSObject) -> bool {
                // SAFETY: `obj` is a valid object pointer, per the trait's contract.
                !unsafe { mozjs::jsapi::js::$unwrap(obj) }.is_null()
            }
        }

        impl TypedArrayKind for $Marker {
            type Element = $elem;

            fn create_new(cx: &mut mozjs::context::JSContext, length: usize) -> *mut JSObject {
                // SAFETY: `cx` has a realm entered, per the trait's contract.
                unsafe { mozjs::jsapi::$new(cx.raw_cx(), length) }
            }

            unsafe fn length_and_data(obj: *mut JSObject) -> (*mut Self::Element, usize) {
                let mut is_shared = false;
                let nogc = JS::AutoRequireNoGC { _address: 0 };
                let data = mozjs::jsapi::$data(obj, &mut is_shared, &nogc);
                debug_assert!(!is_shared, "expected an unshared typed array");
                if data.is_null() {
                    (std::ptr::null_mut(), 0)
                } else {
                    (data, mozjs::jsapi::JS_GetTypedArrayLength(obj))
                }
            }
        }

        impl<'s> std::ops::Deref for Stack<'s, $Marker> {
            type Target = Object<'s>;

            fn deref(&self) -> &Object<'s> {
                // SAFETY: both wrappers are repr(transparent) over the same handle.
                unsafe { &*(self as *const Stack<'s, $Marker> as *const Object<'s>) }
            }
        }

        impl<'s, 'v> FromJSVal<'s, 'v> for Stack<'s, $Marker> {
            type Config = ();

            fn from_jsval(
                scope: &'s Scope<'_>,
                val: HandleValue<'v>,
                _option: Self::Config,
            ) -> Result<Self, ConversionError> {
                Object::from_value(scope, *val)?
                    .cast::<Self>()
                    .map_err(|_| {
                        const MSG: &std::ffi::CStr = unsafe {
                            std::ffi::CStr::from_bytes_with_nul_unchecked(
                                concat!("Value isn't a ", $name, "\0").as_bytes(),
                            )
                        };
                        ConversionError::Failure(std::borrow::Cow::Borrowed(MSG))
                    })
            }
        }
    };
}

bigint_typed_array_marker!(
    BigInt64Array,
    "BigInt64Array",
    JSProto_BigInt64Array,
    i64,
    JS_NewBigInt64Array,
    JS_GetBigInt64ArrayData,
    UnwrapBigInt64Array
);
bigint_typed_array_marker!(
    BigUint64Array,
    "BigUint64Array",
    JSProto_BigUint64Array,
    u64,
    JS_NewBigUint64Array,
    JS_GetBigUint64ArrayData,
    UnwrapBigUint64Array
);

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn root_or_throw<'s, T: JSType>(
    scope: &'s Scope<'_>,
    obj: *mut JSObject,
) -> Result<Stack<'s, T>, ExnThrown> {
    // SAFETY: callers pass pointers fresh from the JSAPI constructor for `T`.
    unsafe { Stack::from_mozjs_rval(scope, obj) }
}

/// Read the data pointer and length of an `ArrayBuffer`, asserting it is
/// not shared.
///
/// # Safety
///
/// `obj` must be a live `ArrayBuffer` object. The returned pointer is valid
/// only until the next GC or detach.
unsafe fn array_buffer_length_and_data(obj: *mut JSObject) -> (*mut u8, usize) {
    let mut length = 0usize;
    let mut is_shared = false;
    let mut data: *mut u8 = std::ptr::null_mut();
    JS::GetArrayBufferLengthAndData(obj, &mut length, &mut is_shared, &mut data);
    debug_assert!(!is_shared, "expected an unshared ArrayBuffer");
    (data, length)
}

/// Get the data pointer of a freshly created (and therefore non-detached,
/// non-shared) `ArrayBuffer`.
///
/// # Safety
///
/// `obj` must be a live `ArrayBuffer` and not detached.
unsafe fn buffer_data_ptr(obj: *mut JSObject) -> *mut u8 {
    let mut is_shared = false;
    let nogc = JS::AutoRequireNoGC { _address: 0 };
    JS::GetArrayBufferData(obj, &mut is_shared, &nogc)
}
