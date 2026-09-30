// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Conversion between JavaScript values and WIT values.
//!
//! The functions correspond to the methods of `wit_dylib_ffi`'s `Call` trait,
//! and exchange values through a [`CallStack`]:
//!
//! - Lifting functions (`push_*`) convert a WIT value into a JavaScript value
//!   and push it onto the stack. A record, tuple, variant, option, result or
//!   list is built from field and element values that earlier `push_*` calls
//!   pushed.
//! - Lowering functions (`pop_*`) pop a JavaScript value off the stack and
//!   convert it into a WIT value. For a record, tuple, variant, option or
//!   result, they push the field and element values for the `pop_*` calls that
//!   follow. While the stack holds [`TypeShapeValue`]s, they pop and push those
//!   instead (see [`CallStack::push_shaped`]).

use std::ffi::{CStr, CString};

use js::conversion::FromJSVal;
use js::error::{throw_type_error, ExnThrown};
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;

use crate::stack::CallStack;

/// The structure of a WIT type, as used by the conversion functions.
///
/// The names of record fields and of variant, enum and flags cases are the JS
/// names created by [`mangle_name`](wit_name_mapping::mangle_name).
#[derive(Clone, Debug, PartialEq)]
pub enum TypeShape {
    Bool,
    U8,
    U16,
    U32,
    U64,
    S8,
    S16,
    S32,
    S64,
    F32,
    F64,
    Char,
    String,
    List(Box<TypeShape>),
    Record(Vec<(String, TypeShape)>),
    Tuple(Vec<TypeShape>),
    /// A `None` payload means the case has no value.
    Variant(Vec<(String, Option<TypeShape>)>),
    /// Case names in declaration order. A case is lifted and lowered as its
    /// index.
    Enum(Vec<String>),
    Option(Box<TypeShape>),
    /// The types of the `ok` and `err` values, or `None` for a case without a
    /// value.
    Result(Option<Box<TypeShape>>, Option<Box<TypeShape>>),
    /// Flag names in declaration order. A flags value is lifted and lowered as a
    /// bit set.
    Flags(Vec<String>),
    /// An owned handle to the resource type `index`. `imported` is set if the
    /// world imports the resource. A lowered handle to an imported resource must
    /// be a wrapper of that type, or a value converted by an own adapter (see
    /// [`crate::resources::register_own_adapter`]).
    OwnResource {
        index: u32,
        imported: bool,
    },
    /// A borrowed handle to the resource type `index`. `imported` is set if the
    /// world imports the resource. A lowered handle to an imported resource must
    /// be a wrapper of that type.
    BorrowResource {
        index: u32,
        imported: bool,
    },
    /// A `stream` of the stream type with this index. Streams aren't converted by
    /// the functions in this module.
    Stream(u32),
    /// A `future` of the future type with this index. Futures aren't converted
    /// by the functions in this module.
    Future(u32),
    /// A type that can't be converted: `map`, a fixed-length list or
    /// `error-context`. The string names the type in error messages.
    Unsupported(&'static str),
}

impl TypeShape {
    /// Whether a value of this shape contains an owned resource handle. The
    /// elements of a stream or future it contains are not part of the value.
    pub fn contains_own_resource(&self) -> bool {
        match self {
            TypeShape::OwnResource { .. } => true,
            TypeShape::List(elem) | TypeShape::Option(elem) => elem.contains_own_resource(),
            TypeShape::Record(fields) => fields.iter().any(|(_, f)| f.contains_own_resource()),
            TypeShape::Tuple(elems) => elems.iter().any(TypeShape::contains_own_resource),
            TypeShape::Variant(cases) => cases
                .iter()
                .any(|(_, p)| p.as_ref().is_some_and(TypeShape::contains_own_resource)),
            TypeShape::Result(ok, err) => [ok, err]
                .into_iter()
                .any(|arm| arm.as_deref().is_some_and(TypeShape::contains_own_resource)),
            _ => false,
        }
    }

    /// The typed-array kind of a numeric scalar, or `None` for every other
    /// shape.
    ///
    /// A list of a shape with a kind is lifted and lowered as a typed array of
    /// that kind. The kind's element size equals the scalar's canonical-ABI size
    /// and alignment.
    pub fn view_kind(&self) -> Option<js::typedarray::ViewKind> {
        use js::typedarray::ViewKind;
        Some(match self {
            TypeShape::U8 => ViewKind::Uint8,
            TypeShape::S8 => ViewKind::Int8,
            TypeShape::U16 => ViewKind::Uint16,
            TypeShape::S16 => ViewKind::Int16,
            TypeShape::U32 => ViewKind::Uint32,
            TypeShape::S32 => ViewKind::Int32,
            TypeShape::U64 => ViewKind::BigUint64,
            TypeShape::S64 => ViewKind::BigInt64,
            TypeShape::F32 => ViewKind::Float32,
            TypeShape::F64 => ViewKind::Float64,
            _ => return None,
        })
    }

    /// Whether a typed array of `kind` can be lowered as a list of this shape:
    /// a typed array of the shape's [`view_kind`](Self::view_kind), or a
    /// `Uint8ClampedArray` for `u8`.
    pub fn accepts_view_kind(&self, kind: js::typedarray::ViewKind) -> bool {
        use js::typedarray::ViewKind;
        self.view_kind().is_some_and(|expected| {
            kind == expected || (expected == ViewKind::Uint8 && kind == ViewKind::Uint8Clamped)
        })
    }

    /// The canonical-ABI memory layout of a `count`-element list of this numeric
    /// scalar, or `None` for every other shape and for a list whose size in
    /// bytes exceeds `isize::MAX`.
    pub fn numeric_list_layout(&self, count: usize) -> Option<std::alloc::Layout> {
        let size = self.view_kind()?.element_size();
        std::alloc::Layout::from_size_align(size.checked_mul(count)?, size).ok()
    }
}

/// A value of a [`TypeShape`], read from JavaScript by
/// [`crate::validate::snapshot`] and converted for lowering. Lowering it runs no
/// JavaScript, except for the parts held as [`TypeShapeValue::Js`].
///
/// Each variant holds a value of the [`TypeShape`] variant with the same name,
/// except for [`TypeShapeValue::NumericList`] and [`TypeShapeValue::Js`].
pub enum TypeShapeValue {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    S8(i8),
    S16(i16),
    S32(i32),
    S64(i64),
    F32(f32),
    F64(f64),
    Char(char),
    String(String),
    /// A list of numbers given as a typed array.
    NumericList(NumericList),
    List(Vec<TypeShapeValue>),
    /// The fields in declaration order.
    Record(Vec<TypeShapeValue>),
    Tuple(Vec<TypeShapeValue>),
    /// The index of the case, and its value if the case has one.
    Variant(u32, Option<Box<TypeShapeValue>>),
    /// The index of the case.
    Enum(u32),
    /// The bit set.
    Flags(u32),
    Option(Option<Box<TypeShapeValue>>),
    /// The `ok` or `err` case, and its value if the case has one.
    Result(Result<Option<Box<TypeShapeValue>>, Option<Box<TypeShapeValue>>>),
    /// A resource, a stream, a future, or a value of a [`TypeShape::Unsupported`]
    /// type, which is lowered from the JavaScript value.
    Js(RootedHeap<Value>),
}

impl TypeShapeValue {
    /// Returns a [`TypeShapeValue::Js`] holding `value`.
    pub fn js(value: HandleValue<'_>) -> TypeShapeValue {
        TypeShapeValue::Js(RootedHeap::from(value.get()))
    }
}

/// The elements of a numeric list in the canonical ABI's layout, with the
/// element count.
pub struct NumericList {
    /// `None` for an empty list.
    buffer: Option<OwnedBuffer>,
    count: usize,
}

impl NumericList {
    /// Copy the elements of `view`, a typed array of a kind that `elem` accepts
    /// (see [`TypeShape::accepts_view_kind`]).
    pub fn copy(
        scope: &Scope<'_>,
        view: js::ArrayBufferView<'_>,
        elem: &TypeShape,
    ) -> Result<NumericList, ExnThrown> {
        let elem_size = elem
            .view_kind()
            .expect("NumericList::copy called for a non-typed-array element")
            .element_size();
        // The view's kind matches `elem`, so the byte length is an exact multiple
        // of the element size.
        let count = view.byte_length() / elem_size;
        let layout = elem
            .numeric_list_layout(count)
            .expect("element shape has a numeric layout");
        if count == 0 {
            return Ok(NumericList {
                buffer: None,
                count,
            });
        }
        // SAFETY: `layout` has a non-zero size, since `count > 0`.
        let Some(ptr) = std::ptr::NonNull::new(unsafe { std::alloc::alloc(layout) }) else {
            js::exception::report_out_of_memory(scope);
            return Err(ExnThrown);
        };
        // SAFETY: `ptr` is freshly allocated for `layout.size()` bytes, and nothing
        // else references it.
        let target = unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), layout.size()) };
        // Allocating cannot run JS or collect, so the view still holds exactly
        // `layout.size()` bytes.
        view.with_bytes(|src| target.copy_from_slice(src));
        Ok(NumericList {
            buffer: Some(OwnedBuffer { ptr, layout }),
            count,
        })
    }

    /// The elements' bytes.
    pub fn bytes(&self) -> &[u8] {
        match &self.buffer {
            // SAFETY: `ptr` points to `layout.size()` initialized bytes owned by
            // the buffer.
            Some(buffer) => unsafe {
                std::slice::from_raw_parts(buffer.ptr.as_ptr(), buffer.layout.size())
            },
            None => &[],
        }
    }

    /// Returns the buffer, its layout and the element count, and gives up
    /// ownership of the buffer. The caller must free it with that layout. For
    /// an empty list, nothing is allocated and the pointer is dangling but
    /// aligned for `elem`.
    fn into_raw(self, elem: &TypeShape) -> (*mut u8, std::alloc::Layout, usize) {
        match self.buffer {
            Some(buffer) => {
                let buffer = std::mem::ManuallyDrop::new(buffer);
                (buffer.ptr.as_ptr(), buffer.layout, self.count)
            }
            None => {
                let layout = elem
                    .numeric_list_layout(0)
                    .expect("element shape has a numeric layout");
                (layout.align() as *mut u8, layout, 0)
            }
        }
    }
}

// ===========================================================================
// Lift: WIT value -> JS value pushed onto the stack
// ===========================================================================

pub fn push_bool(stack: &mut CallStack, val: bool) {
    stack.push_value(js::value::from_bool(val));
}

pub fn push_u8(stack: &mut CallStack, val: u8) {
    stack.push_value(js::value::from_u32(val as u32));
}

pub fn push_u16(stack: &mut CallStack, val: u16) {
    stack.push_value(js::value::from_u32(val as u32));
}

pub fn push_u32(stack: &mut CallStack, val: u32) {
    stack.push_value(js::value::from_u32(val));
}

pub fn push_s8(stack: &mut CallStack, val: i8) {
    stack.push_value(js::value::from_i32(val as i32));
}

pub fn push_s16(stack: &mut CallStack, val: i16) {
    stack.push_value(js::value::from_i32(val as i32));
}

pub fn push_s32(stack: &mut CallStack, val: i32) {
    stack.push_value(js::value::from_i32(val));
}

/// Lift a `u64` as a BigInt.
pub fn push_u64(stack: &mut CallStack, scope: &Scope<'_>, val: u64) -> Result<(), ExnThrown> {
    let bi = js::bigint::from_u64(scope, val)?;
    stack.push_value(js::value::from_bigint(bi));
    Ok(())
}

/// Lift an `s64` as a BigInt.
pub fn push_s64(stack: &mut CallStack, scope: &Scope<'_>, val: i64) -> Result<(), ExnThrown> {
    let bi = js::bigint::from_i64(scope, val)?;
    stack.push_value(js::value::from_bigint(bi));
    Ok(())
}

/// Lift an `f32` as a JS double. NaNs are canonicalized by `value::from_f64`.
pub fn push_f32(stack: &mut CallStack, val: f32) {
    stack.push_value(js::value::from_f64(val as f64));
}

/// Lift an `f64` as a JS double. NaNs are canonicalized.
pub fn push_f64(stack: &mut CallStack, val: f64) {
    stack.push_value(js::value::from_f64(val));
}

/// Lift a `char` as a one-code-point JS string.
pub fn push_char(stack: &mut CallStack, scope: &Scope<'_>, val: char) -> Result<(), ExnThrown> {
    let mut buf = [0u8; 4];
    let s = js::JSString::from_str(scope, val.encode_utf8(&mut buf))?;
    stack.push_value(s.as_value());
    Ok(())
}

/// Lift a string.
pub fn push_string(stack: &mut CallStack, scope: &Scope<'_>, val: &str) -> Result<(), ExnThrown> {
    let s = js::JSString::from_str(scope, val)?;
    stack.push_value(s.as_value());
    Ok(())
}

/// Lift a record. The field values must be on the stack in declaration order,
/// with the last field on top.
pub fn push_record(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    field_names: &[&'static CStr],
) -> Result<(), ExnThrown> {
    let obj = js::Object::new_plain(scope)?;
    for name in field_names.iter().rev() {
        let field = scope.root_value(stack.pop_value());
        obj.define_property(scope, name, field, ENUMERATE)?;
    }
    stack.push_object(&obj);
    Ok(())
}

/// Lift a tuple as a JS array. The element values must be on the stack in
/// order, with the last element on top.
pub fn push_tuple(stack: &mut CallStack, scope: &Scope<'_>, len: usize) -> Result<(), ExnThrown> {
    let arr = js::Array::new(scope, len)?;
    for index in (0..len).rev() {
        let elem = scope.root_value(stack.pop_value());
        arr.define_element(scope, index as u32, elem, ENUMERATE)?;
    }
    stack.push_object(&arr);
    Ok(())
}

/// Lift a variant case as `{tag: name}`, or as `{tag: name, val}` if the case
/// has a value. The value must be on top of the stack.
pub fn push_variant(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    cases: &[(&'static CStr, bool)],
    discriminant: u32,
) -> Result<(), ExnThrown> {
    let Some((tag, has_payload)) = cases.get(discriminant as usize) else {
        return Err(throw_type_error(
            scope,
            c"variant discriminant out of range",
        ));
    };
    let obj = js::Object::new_plain(scope)?;
    set_tag(&obj, scope, tag)?;
    if *has_payload {
        let val = scope.root_value(stack.pop_value());
        obj.define_property(scope, c"val", val, ENUMERATE)?;
    }
    stack.push_object(&obj);
    Ok(())
}

/// Lift an enum as its case index, which is the value that the `const enum` in
/// the TypeScript declarations gives that case.
pub fn push_enum(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    case_count: u32,
    discriminant: u32,
) -> Result<(), ExnThrown> {
    if discriminant >= case_count {
        return Err(throw_type_error(scope, c"enum discriminant out of range"));
    }
    stack.push_value(js::value::from_u32(discriminant));
    Ok(())
}

/// Lift an option. `none` is lifted as `undefined`, and `some(x)` as `x`. If
/// the inner type is also an option, `some(x)` is lifted as `{val: x}` instead,
/// so that `some(none)` differs from `none`. For `some`, `x` must be on top of
/// the stack.
pub fn push_option(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    inner_is_option: bool,
    is_some: bool,
) -> Result<(), ExnThrown> {
    if is_some {
        if inner_is_option {
            let obj = js::Object::new_plain(scope)?;
            let val = scope.root_value(stack.pop_value());
            obj.define_property(scope, c"val", val, ENUMERATE)?;
            stack.push_object(&obj);
        }
        // Otherwise leave the payload value on the stack as-is.
    } else {
        stack.push_value(js::value::undefined());
    }
    Ok(())
}

/// Lift a result as `{tag: "ok"}` or `{tag: "err"}`, with a `val` property if
/// the case has a value. The value must be on top of the stack.
pub fn push_result(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    ok_has_payload: bool,
    err_has_payload: bool,
    is_err: bool,
) -> Result<(), ExnThrown> {
    let obj = js::Object::new_plain(scope)?;
    let tag = if is_err { c"err" } else { c"ok" };
    set_tag(&obj, scope, tag)?;
    let has_payload = if is_err {
        err_has_payload
    } else {
        ok_has_payload
    };
    if has_payload {
        let val = scope.root_value(stack.pop_value());
        obj.define_property(scope, c"val", val, ENUMERATE)?;
    }
    stack.push_object(&obj);
    Ok(())
}

/// Lift flags as an int32 bit set, with bit `i` for the `i`th flag in
/// declaration order. The value equals the bitwise or of the members of the
/// TypeScript `const enum` for the set flags. A set with bit 31 is negative.
pub fn push_flags(stack: &mut CallStack, bits: u32) {
    stack.push_value(js::value::from_i32(bits as i32));
}

// ===========================================================================
// Lower: JS value popped off the stack -> WIT value
// ===========================================================================

/// If a [`TypeShapeValue`] other than [`TypeShapeValue::Js`] is on top of
/// `stack`, pop it and return its value from the enclosing function. Panics if
/// it isn't the variant `$variant`.
macro_rules! return_shaped {
    ($stack:expr, $variant:ident) => {
        if let Some(shaped) = $stack.pop_shaped() {
            return match shaped {
                TypeShapeValue::$variant(value) => Ok(value),
                _ => mismatched_shape(stringify!($variant)),
            };
        }
    };
}

/// Panic for a [`TypeShapeValue`] that isn't of the type being lowered, which
/// indicates a bug in the calling convention rather than untrusted input.
fn mismatched_shape(expected: &str) -> ! {
    panic!("lowering a {expected}, but the shaped value on the stack is of another type")
}

pub fn pop_bool(stack: &mut CallStack, scope: &Scope<'_>) -> Result<bool, ExnThrown> {
    return_shaped!(stack, Bool);
    let v = scope.root_value(stack.pop_value());
    if v.is_boolean() {
        Ok(v.to_boolean())
    } else {
        Err(throw_type_error(scope, c"expected a boolean"))
    }
}

pub fn pop_u8(stack: &mut CallStack, scope: &Scope<'_>) -> Result<u8, ExnThrown> {
    return_shaped!(stack, U8);
    pop_int_in_range(stack, scope, c"expected a u8")
}

pub fn pop_u16(stack: &mut CallStack, scope: &Scope<'_>) -> Result<u16, ExnThrown> {
    return_shaped!(stack, U16);
    pop_int_in_range(stack, scope, c"expected a u16")
}

pub fn pop_u32(stack: &mut CallStack, scope: &Scope<'_>) -> Result<u32, ExnThrown> {
    return_shaped!(stack, U32);
    pop_int_in_range(stack, scope, c"expected a u32")
}

pub fn pop_s8(stack: &mut CallStack, scope: &Scope<'_>) -> Result<i8, ExnThrown> {
    return_shaped!(stack, S8);
    pop_int_in_range(stack, scope, c"expected an s8")
}

pub fn pop_s16(stack: &mut CallStack, scope: &Scope<'_>) -> Result<i16, ExnThrown> {
    return_shaped!(stack, S16);
    pop_int_in_range(stack, scope, c"expected an s16")
}

pub fn pop_s32(stack: &mut CallStack, scope: &Scope<'_>) -> Result<i32, ExnThrown> {
    return_shaped!(stack, S32);
    pop_int_in_range(stack, scope, c"expected an s32")
}

/// Lower a `u64`, which must be a BigInt in `[0, 2^64)`. A number is rejected,
/// as assigning one to a `BigUint64Array` element is.
pub fn pop_u64(stack: &mut CallStack, scope: &Scope<'_>) -> Result<u64, ExnThrown> {
    return_shaped!(stack, U64);
    let v = scope.root_value(stack.pop_value());
    if !v.is_bigint() {
        return Err(throw_type_error(scope, c"expected a u64 (BigInt)"));
    }
    let bi = js::bigint::to_bigint(scope, v)?;
    // `to_uint64` is modulo 2^64, so `-1n` would lower as `u64::MAX` and `2n**64n`
    // as `0`.
    js::bigint::fits_u64(bi).ok_or_else(|| throw_type_error(scope, c"u64 BigInt is out of range"))
}

/// Lower an `s64`, which must be a BigInt in `[-2^63, 2^63)`. A number is
/// rejected, as assigning one to a `BigInt64Array` element is.
pub fn pop_s64(stack: &mut CallStack, scope: &Scope<'_>) -> Result<i64, ExnThrown> {
    return_shaped!(stack, S64);
    let v = scope.root_value(stack.pop_value());
    if !v.is_bigint() {
        return Err(throw_type_error(scope, c"expected an s64 (BigInt)"));
    }
    let bi = js::bigint::to_bigint(scope, v)?;
    // `to_int64` is modulo 2^64, so `2n**63n` would lower as `i64::MIN`.
    js::bigint::fits_i64(bi).ok_or_else(|| throw_type_error(scope, c"s64 BigInt is out of range"))
}

pub fn pop_f32(stack: &mut CallStack, scope: &Scope<'_>) -> Result<f32, ExnThrown> {
    return_shaped!(stack, F32);
    let v = scope.root_value(stack.pop_value());
    Ok(pop_number(v, scope, c"expected an f32")? as f32)
}

pub fn pop_f64(stack: &mut CallStack, scope: &Scope<'_>) -> Result<f64, ExnThrown> {
    return_shaped!(stack, F64);
    let v = scope.root_value(stack.pop_value());
    pop_number(v, scope, c"expected an f64")
}

/// Lower a `char`: the JS value must be a string holding exactly one Unicode
/// scalar value.
///
/// The scalar value may be a single UTF-16 code unit or a surrogate pair. A
/// lone surrogate is rejected.
pub fn pop_char(stack: &mut CallStack, scope: &Scope<'_>) -> Result<char, ExnThrown> {
    return_shaped!(stack, Char);
    const NOT_A_CHAR: &std::ffi::CStr = c"expected a char (a string of one Unicode scalar value)";
    let v = scope.root_value(stack.pop_value());
    if !v.is_string() {
        return Err(throw_type_error(scope, NOT_A_CHAR));
    }
    let s = js::JSString::from_value(scope, v)?;
    let units = match s.len() {
        1 => [s.char_at(scope, 0)?, 0],
        2 => [s.char_at(scope, 0)?, s.char_at(scope, 1)?],
        _ => return Err(throw_type_error(scope, NOT_A_CHAR)),
    };
    let len = if s.len() == 1 { 1 } else { 2 };
    let mut decoded = char::decode_utf16(units[..len].iter().copied());
    match (decoded.next(), decoded.next()) {
        (Some(Ok(c)), None) => Ok(c),
        // A lone surrogate, or two code units that are two characters.
        _ => Err(throw_type_error(scope, NOT_A_CHAR)),
    }
}

/// Lower a string into an owned UTF-8 `String`.
pub fn pop_string(stack: &mut CallStack, scope: &Scope<'_>) -> Result<String, ExnThrown> {
    return_shaped!(stack, String);
    let v = scope.root_value(stack.pop_value());
    if !v.is_string() {
        return Err(throw_type_error(scope, c"expected a string"));
    }
    String::from_jsval_throwing(scope, v, ())
}

/// Lower a record. The fields are read by name and pushed onto the stack so
/// that the first field is on top.
pub fn pop_record(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    field_names: &[&'static CStr],
) -> Result<(), ExnThrown> {
    if let Some(shaped) = stack.pop_shaped() {
        let TypeShapeValue::Record(fields) = shaped else {
            mismatched_shape("record")
        };
        push_shaped_reversed(stack, fields);
        return Ok(());
    }
    let obj = pop_object(stack, scope, c"expected a record (object)")?;
    for name in field_names.iter().rev() {
        let field = obj.get_property(scope, name)?;
        stack.push_value(field.get());
    }
    Ok(())
}

/// Lower a tuple. The elements are read and pushed onto the stack so that the
/// first element is on top.
pub fn pop_tuple(stack: &mut CallStack, scope: &Scope<'_>, count: usize) -> Result<(), ExnThrown> {
    if let Some(shaped) = stack.pop_shaped() {
        let TypeShapeValue::Tuple(elements) = shaped else {
            mismatched_shape("tuple")
        };
        push_shaped_reversed(stack, elements);
        return Ok(());
    }
    let obj = pop_object(stack, scope, c"expected a tuple (array)")?;
    for index in 0..count {
        let elem = obj.get_element(scope, (count - index - 1) as u32)?;
        stack.push_value(elem.get());
    }
    Ok(())
}

/// Lower a variant `{tag, val}`. Returns the index of the case named by `tag`.
/// If the case has a value, `val` is pushed onto the stack.
pub fn pop_variant(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    cases: &[(&'static CStr, bool)],
) -> Result<u32, ExnThrown> {
    if let Some(shaped) = stack.pop_shaped() {
        let TypeShapeValue::Variant(case, payload) = shaped else {
            mismatched_shape("variant")
        };
        if let Some(payload) = payload {
            stack.push_shaped(*payload);
        }
        return Ok(case);
    }
    let obj = pop_object(stack, scope, c"expected a variant (object)")?;
    let discriminant = tag_index(&obj, scope, cases.iter().map(|(name, _)| *name))?
        .ok_or_else(|| throw_type_error(scope, c"variant tag does not match any case"))?;
    if cases[discriminant].1 {
        let val = obj.get_property(scope, c"val")?;
        stack.push_value(val.get());
    }
    Ok(discriminant as u32)
}

/// Lower an enum value, given as the index of its case. The index must be less
/// than `case_count`.
pub fn pop_enum(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    case_count: u32,
) -> Result<u32, ExnThrown> {
    return_shaped!(stack, Enum);
    let index = pop_u32(stack, scope)?;
    if index < case_count {
        Ok(index)
    } else {
        Err(throw_type_error(
            scope,
            c"enum value is not one of the declared cases",
        ))
    }
}

/// Lower an option. Returns `false` for `none`, given as `undefined` or `null`.
/// Returns `true` for `some`, and leaves the value on the stack. If the inner
/// type is also an option, the value is given as `{val}`, and `val` is pushed
/// onto the stack instead.
pub fn pop_option(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    inner_is_option: bool,
) -> Result<bool, ExnThrown> {
    if let Some(shaped) = stack.pop_shaped() {
        let TypeShapeValue::Option(payload) = shaped else {
            mismatched_shape("option")
        };
        return Ok(match payload {
            Some(payload) => {
                stack.push_shaped(*payload);
                true
            }
            None => false,
        });
    }
    if stack.last().is_null_or_undefined() {
        stack.pop_value();
        Ok(false)
    } else if inner_is_option {
        let obj = pop_object(stack, scope, c"expected a nested option wrapper (object)")?;
        let val = obj.get_property(scope, c"val")?;
        stack.push_value(val.get());
        Ok(true)
    } else {
        // Leave the payload value on the stack as-is.
        Ok(true)
    }
}

/// Lower a result `{tag, val}`. Returns 0 for `ok` and 1 for `err`. If the case
/// has a value, `val` is pushed onto the stack.
pub fn pop_result(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    ok_has_payload: bool,
    err_has_payload: bool,
) -> Result<u32, ExnThrown> {
    if let Some(shaped) = stack.pop_shaped() {
        let TypeShapeValue::Result(result) = shaped else {
            mismatched_shape("result")
        };
        let (discriminant, payload) = match result {
            Ok(payload) => (0, payload),
            Err(payload) => (1, payload),
        };
        if let Some(payload) = payload {
            stack.push_shaped(*payload);
        }
        return Ok(discriminant);
    }
    let obj = pop_object(stack, scope, c"expected a result (object)")?;
    let (discriminant, has_payload) = match tag_index(&obj, scope, [c"ok", c"err"].into_iter())? {
        Some(0) => (0u32, ok_has_payload),
        Some(_) => (1u32, err_has_payload),
        None => {
            return Err(throw_type_error(
                scope,
                c"result tag must be \"ok\" or \"err\"",
            ))
        }
    };
    if has_payload {
        let val = obj.get_property(scope, c"val")?;
        stack.push_value(val.get());
    }
    Ok(discriminant)
}

/// Lower a flags value, given as its bit set, checking that no bit at or above
/// `flag_count` is set. The set is an int32, such as the result of a bitwise-or
/// of flags, or a number in the `u32` range. A negative int32 has bit 31 set.
pub fn pop_flags(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    flag_count: u32,
) -> Result<u32, ExnThrown> {
    return_shaped!(stack, Flags);
    const NOT_FLAGS: &std::ffi::CStr = c"expected a flags value (a number)";
    let bits = if stack.last().is_int32() {
        stack.pop_value().to_int32() as u32
    } else if stack.last().is_number() {
        pop_int_in_range(stack, scope, NOT_FLAGS)?
    } else {
        stack.pop_value();
        return Err(throw_type_error(scope, NOT_FLAGS));
    };
    let declared = if flag_count >= u32::BITS {
        u32::MAX
    } else {
        (1u32 << flag_count) - 1
    };
    if bits & !declared == 0 {
        Ok(bits)
    } else {
        Err(throw_type_error(
            scope,
            c"flags value has a bit that names no declared flag",
        ))
    }
}

// ===========================================================================
// Lists
// ===========================================================================

/// Begin lifting a list that isn't lifted as a typed array, by pushing an array
/// of length `capacity` onto the stack. `capacity` must be the number of
/// elements. Each element is lifted and then added with [`list_append`].
pub fn push_list_begin(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    capacity: usize,
) -> Result<(), ExnThrown> {
    let arr = js::Array::new(scope, capacity)?;
    stack.push_object(&arr);
    stack.lift_list_begin(capacity);
    Ok(())
}

/// Pop the value on top of the stack and add it to the array beneath it, which
/// was pushed by [`push_list_begin`]. The values are stored at consecutive
/// indices, starting at 0.
pub fn list_append(stack: &mut CallStack, scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let elem = scope.root_value(stack.pop_value());
    let index = stack.lift_list_next();
    let arr = js::Object::from_raw_value(scope, stack.last())
        .map_err(|_| throw_type_error(scope, c"list value is not an object"))?;
    arr.define_element(scope, index as u32, elem, ENUMERATE)?;
    Ok(())
}

/// A non-empty buffer from the global allocator, freed with `layout` when
/// dropped.
struct OwnedBuffer {
    ptr: std::ptr::NonNull<u8>,
    layout: std::alloc::Layout,
}

// SAFETY: `OwnedBuffer` exclusively owns its heap allocation.
unsafe impl js::typedarray::ExternalBytes for OwnedBuffer {
    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` points to `layout.size()` initialized bytes this value owns.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for OwnedBuffer {
    fn drop(&mut self) {
        // SAFETY: `ptr` was allocated by the global allocator with `layout`.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/// Lift a `count`-element numeric list into the matching typed array, adopting
/// its buffer instead of copying it.
///
/// # Safety
///
/// With `count > 0`, `data` must be a non-null allocation of the global
/// allocator made with `layout`, holding `count` valid, contiguous elements of
/// `elem`'s native type, and ownership of the allocation transfers to this
/// function. With `count == 0`, `data` and `layout` are not used.
pub unsafe fn push_numeric_list(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    elem: &TypeShape,
    data: *mut u8,
    layout: std::alloc::Layout,
    count: usize,
) -> Result<(), ExnThrown> {
    let kind = elem
        .view_kind()
        .expect("push_numeric_list called for a non-typed-array element");
    let buffer = match std::ptr::NonNull::new(data).filter(|_| count > 0) {
        Some(ptr) => js::ArrayBuffer::from_external(scope, OwnedBuffer { ptr, layout })?,
        None => js::ArrayBuffer::new(scope, 0)?,
    };
    let array = js::typedarray::construct_view(scope, kind, buffer, 0, count)?;
    stack.push_value(array.as_value());
    Ok(())
}

/// Lower a numeric typed-array list into a freshly allocated canonical-ABI
/// buffer.
///
/// The typed array's element type must match `elem`, except that a
/// `Uint8ClampedArray` is also accepted for `list<u8>`.
///
/// Returns the buffer, its layout and the element count. The caller owns the
/// buffer and must free it with that layout. For an empty list, nothing is
/// allocated and the pointer is dangling but aligned.
pub fn pop_numeric_list(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    elem: &TypeShape,
) -> Result<(*mut u8, std::alloc::Layout, usize), ExnThrown> {
    if let Some(shaped) = stack.pop_shaped() {
        let TypeShapeValue::NumericList(list) = shaped else {
            mismatched_shape("numeric list")
        };
        return Ok(list.into_raw(elem));
    }
    let obj = pop_object(stack, scope, c"expected a typed-array list")?;
    let view = js::ArrayBufferView::from_object(obj)
        .ok_or_else(|| throw_type_error(scope, c"expected a typed array"))?;
    if !elem.accepts_view_kind(view.view_kind()) {
        return Err(throw_type_error(
            scope,
            c"typed-array element kind does not match the list element type",
        ));
    }
    Ok(NumericList::copy(scope, view, elem)?.into_raw(elem))
}

// ===========================================================================
// Helpers
// ===========================================================================

/// The attributes of a property created by an object literal.
pub(crate) const ENUMERATE: std::ffi::c_uint = js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint;

/// Convert a mangled identifier into a `CString` for use as a property name.
pub fn to_cstring(s: &str) -> CString {
    CString::new(s).expect("mangled identifiers contain no interior NUL")
}

fn set_tag(obj: &js::Object<'_>, scope: &Scope<'_>, tag: &'static CStr) -> Result<(), ExnThrown> {
    // The tag is the string of a cached property key, so all lifted values with
    // the same tag share one string.
    let tag_id = js::class::get_or_init_property_id(scope, tag)?;
    let tag_value = js::id::id_to_value(scope, *tag_id)?;
    obj.define_property(scope, c"tag", tag_value, ENUMERATE)
}

/// The index of the `tag` property's value among `names`, or `None` if it is
/// not a string or matches none of them.
pub(crate) fn tag_index<'a>(
    obj: &js::Object<'_>,
    scope: &Scope<'_>,
    names: impl Iterator<Item = &'a CStr>,
) -> Result<Option<usize>, ExnThrown> {
    let tag = obj.get_property(scope, c"tag")?;
    if !tag.is_string() {
        return Ok(None);
    }
    let s = js::JSString::from_value(scope, tag)?;
    for (index, name) in names.enumerate() {
        if s.equals_ascii(scope, name)? {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// Push `values` onto `stack` so that the first one is on top.
fn push_shaped_reversed(stack: &mut CallStack, values: Vec<TypeShapeValue>) {
    for value in values.into_iter().rev() {
        stack.push_shaped(value);
    }
}

/// Pop a value expected to be an object, rooting it.
fn pop_object<'s>(
    stack: &mut CallStack,
    scope: &'s Scope<'_>,
    err: &std::ffi::CStr,
) -> Result<js::Object<'s>, ExnThrown> {
    // The popped value is passed directly to `from_raw_value`, which roots it,
    // so it is never held in an unrooted local.
    js::Object::from_raw_value(scope, stack.pop_value()).map_err(|_| throw_type_error(scope, err))
}

/// Extract a number from a value, accepting int32 and double, including NaN and
/// the infinities.
fn pop_number(
    v: HandleValue<'_>,
    scope: &Scope<'_>,
    err: &std::ffi::CStr,
) -> Result<f64, ExnThrown> {
    if v.is_number() {
        Ok(v.to_number())
    } else {
        Err(throw_type_error(scope, err))
    }
}

/// Pop a small-integer value and narrow it into `T`, rejecting out-of-range and
/// non-integer inputs.
fn pop_int_in_range<T: TryFrom<i64>>(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    err: &std::ffi::CStr,
) -> Result<T, ExnThrown> {
    let v = scope.root_value(stack.pop_value());
    let n = if v.is_int32() {
        v.to_int32() as i64
    } else if v.is_number() {
        // Reject NaN, infinities and fractional values rather than truncating,
        // since `3.7 as i64 == 3`, and values outside the `i64` range rather
        // than saturating. `T::try_from` below rejects values outside `T`.
        let n = v.to_number();
        let in_range = (-9223372036854775808.0..9223372036854775808.0).contains(&n);
        if !(n.is_finite() && n.fract() == 0.0 && in_range) {
            return Err(throw_type_error(scope, err));
        }
        n as i64
    } else {
        return Err(throw_type_error(scope, err));
    };
    T::try_from(n).map_err(|_| throw_type_error(scope, err))
}
