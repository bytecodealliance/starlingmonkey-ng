// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! JavaScript ↔ WIT value conversion.
//!
//! Conversion is split into two directions, each a family of functions matching
//! `wit_dylib_ffi`'s `Call` trait one-to-one:
//!
//! - Lift (`push_*`): a WIT value becomes a JavaScript value pushed onto the
//!   [`CallStack`]. Scalars take their Rust value directly, and composites
//!   assemble themselves from field and element values already pushed by prior
//!   `push_*` calls.
//! - Lower (`pop_*`): a JavaScript value popped off the stack becomes a WIT
//!   value. Scalars return their Rust value, and composites destructure the JS
//!   value and push their field and element values back for subsequent `pop_*`
//!   calls.
//!
//! The push and pop ordering of composites follows the calling convention that
//! `wit-dylib`'s generated code drives. Records and tuples share one contract,
//! since the generated code routes both through the same codegen, which lifts
//! fields forward and lowers them forward. On lift the field and element values
//! arrive with the first one deepest and the last on top, and on lower they are
//! pushed back with the first one on top (see [`push_record`], [`push_tuple`],
//! [`pop_record`] and [`pop_tuple`]).
//!
//! The mapping is driven by a crate-local [`TypeShape`] rather than the
//! `wit-dylib-ffi` types, which keeps every function natively testable against a
//! real runtime. Field names in [`TypeShape::Record`] are already mangled at
//! construction, so the conversion functions use them verbatim.

use std::ffi::{CStr, CString};

use heck::{ToLowerCamelCase, ToUpperCamelCase};

use js::conversion::FromJSVal;
use js::error::{throw_type_error, ExnThrown};
use js::gc::scope::Scope;
use js::prelude::HandleValue;

use crate::resources::{self, ResourceDesc};
use crate::stack::CallStack;

/// A crate-local description of a WIT type's structure, covering the subset of
/// the Component Model type system the interpreter converts.
///
/// `Record` field names and the names in `Variant`, `Enum` and `Flags` are stored
/// already mangled, via [`mangle_name`], and the conversion functions never
/// re-mangle them.
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
    /// Field names are pre-mangled.
    Record(Vec<(String, TypeShape)>),
    Tuple(Vec<TypeShape>),
    /// Case names are pre-mangled. A `None` payload means the case has no
    /// value.
    Variant(Vec<(String, Option<TypeShape>)>),
    /// Enum case names, pre-mangled, in declaration order. A case lifts and
    /// lowers as its index. An `err` payload given as an `Error` names its case
    /// by its `message` (see [`crate::component_error`]).
    Enum(Vec<String>),
    Option(Box<TypeShape>),
    /// `ok` and `err` payload types, each present iff that arm has a value.
    Result(Option<Box<TypeShape>>, Option<Box<TypeShape>>),
    /// Flag names, in declaration order. Only the count reaches the conversion: a
    /// flags set lifts and lowers as its bit set.
    Flags(Vec<String>),
    /// An owned handle to the resource type `index`. For a resource the world
    /// imports, the value must be a wrapper of that type, unless an own adapter
    /// converts it (see [`crate::resources::register_own_adapter`]).
    OwnResource {
        index: u32,
        imported: bool,
    },
    /// A borrowed handle to the resource type `index`. For a resource the world
    /// imports, the value must be a wrapper of that type.
    BorrowResource {
        index: u32,
        imported: bool,
    },
    /// A Component Model `stream<T>` of the given stream type index. The
    /// dedicated `pop_stream` and `push_stream` path (see [`crate::streams`])
    /// handles the element conversion rather than the generic `pop_*` and
    /// `push_*` family, so this arm holds only the index, which is enough to
    /// classify a stream-typed argument or result and tell it apart from a
    /// `result<_, _>`.
    Stream(u32),
    /// A Component Model `future<T>` of the given future type index. See
    /// [`TypeShape::Stream`].
    Future(u32),
    /// A type the interpreter does not convert, named for the error a
    /// conversion of it reports: `map`, a fixed-length list or `error-context`.
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

    /// The canonical-ABI element size and the typed-array kind of a numeric
    /// scalar, or `None` for every other shape.
    ///
    /// The shapes with a layout are exactly the ones a list lifts and lowers
    /// through a typed array. Alignment equals size for all of them.
    pub fn numeric_layout(&self) -> Option<(usize, js::typedarray::ViewKind)> {
        use js::typedarray::ViewKind;
        Some(match self {
            TypeShape::U8 => (1, ViewKind::Uint8),
            TypeShape::S8 => (1, ViewKind::Int8),
            TypeShape::U16 => (2, ViewKind::Uint16),
            TypeShape::S16 => (2, ViewKind::Int16),
            TypeShape::U32 => (4, ViewKind::Uint32),
            TypeShape::S32 => (4, ViewKind::Int32),
            TypeShape::U64 => (8, ViewKind::BigUint64),
            TypeShape::S64 => (8, ViewKind::BigInt64),
            TypeShape::F32 => (4, ViewKind::Float32),
            TypeShape::F64 => (8, ViewKind::Float64),
            _ => return None,
        })
    }

    /// The canonical-ABI memory layout of a `count`-element list of this numeric
    /// scalar, or `None` for every other shape.
    pub fn numeric_list_layout(&self, count: usize) -> Option<std::alloc::Layout> {
        let (size, _) = self.numeric_layout()?;
        Some(std::alloc::Layout::from_size_align(size * count, size).expect("list layout overflow"))
    }
}

/// Mangle a WIT identifier into its JavaScript form.
///
/// Punctuation that is illegal in a JS identifier is replaced with `_`, then the
/// result is lower-camel-cased. This matches the componentizer's codegen, so
/// resource classes, which are upper-camel-cased, and members agree on names.
pub fn mangle_name(s: &str) -> String {
    s.replace(['@', ':', '/', '-', '[', ']', '.'], "_")
        .to_lower_camel_case()
}

/// Mangle a WIT resource-type name into its JavaScript class name.
///
/// Resource classes are UpperCamelCase (`resource-type` becomes `ResourceType`),
/// unlike functions and members, which [`mangle_name`] lower-camel-cases.
/// `[constructor]`, `[method]` and `[static]` resolution upper-camel-cases the
/// resource type before looking up the class.
pub fn mangle_resource_name(s: &str) -> String {
    s.replace(['@', ':', '/', '-', '[', ']', '.'], "_")
        .to_upper_camel_case()
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

/// Lift an `f32` as a JS double. NaNs are canonicalized by `value::from_f64`,
/// since the engine reserves non-canonical NaN bit patterns for value tagging.
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

/// Lift a record. The field values are already on the stack in forward
/// declaration order, so they are drained back-to-front into the object.
/// `field_names` are already mangled and used verbatim.
pub fn push_record(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    field_names: &[&'static CStr],
) -> Result<(), ExnThrown> {
    let obj = js::Object::new_plain(scope)?;
    for name in field_names.iter().rev() {
        let field = scope.root_value(stack.pop_value());
        define_field(&obj, scope, name, field)?;
    }
    stack.push_object(&obj);
    Ok(())
}

/// Lift a tuple as a JS array. The element values are already on the stack in
/// forward declaration order, with the first element at the bottom of the pushed
/// range.
pub fn push_tuple(stack: &mut CallStack, scope: &Scope<'_>, len: usize) -> Result<(), ExnThrown> {
    let arr = js::Array::new(scope, len)?;
    // The first element sits deepest, so the array is filled back-to-front and
    // element `i` lands at index `i`.
    for index in (0..len).rev() {
        let elem = scope.root_value(stack.pop_value());
        arr.define_element(
            scope,
            index as u32,
            elem,
            js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint,
        )?;
    }
    stack.push_object(&arr);
    Ok(())
}

/// Lift a variant case as `{tag: name}` plus `{val}` iff the case has a payload.
/// The payload value (if any) is already on the stack.
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
        define_field(&obj, scope, c"val", val)?;
    }
    stack.push_object(&obj);
    Ok(())
}

/// Lift an enum as its case index, which is the value the `const enum` in the
/// TypeScript declarations gives that case.
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

/// Lift an option. `none` is `undefined`. `some` leaves the payload as-is,
/// except when the inner type is itself an option, where the payload is wrapped
/// in `{val}` so `some(none)` (`{val: undefined}`) is distinguishable from
/// `none` (`undefined`).
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
            define_field(&obj, scope, c"val", val)?;
            stack.push_object(&obj);
        }
        // Otherwise leave the payload value on the stack as-is.
    } else {
        stack.push_value(js::value::undefined());
    }
    Ok(())
}

/// Lift a result as `{tag: "ok"|"err"}` plus `{val}` iff that arm has a payload.
/// Any payload value is already on the stack.
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
        define_field(&obj, scope, c"val", val)?;
    }
    stack.push_object(&obj);
    Ok(())
}

/// Lift flags as the bit set, one bit per declared flag in declaration order,
/// as the int32 a bitwise-or of the `const enum` members the TypeScript
/// declarations give the flags produces. A set with bit 31 is negative.
pub fn push_flags(stack: &mut CallStack, bits: u32) {
    stack.push_value(js::value::from_i32(bits as i32));
}

// ===========================================================================
// Lower: JS value popped off the stack -> WIT value
// ===========================================================================

pub fn pop_bool(stack: &mut CallStack, scope: &Scope<'_>) -> Result<bool, ExnThrown> {
    let v = scope.root_value(stack.pop_value());
    if v.is_boolean() {
        Ok(v.to_boolean())
    } else {
        Err(throw_type_error(scope, c"expected a boolean"))
    }
}

pub fn pop_u8(stack: &mut CallStack, scope: &Scope<'_>) -> Result<u8, ExnThrown> {
    pop_int_in_range(stack, scope, c"expected a u8")
}

pub fn pop_u16(stack: &mut CallStack, scope: &Scope<'_>) -> Result<u16, ExnThrown> {
    pop_int_in_range(stack, scope, c"expected a u16")
}

pub fn pop_u32(stack: &mut CallStack, scope: &Scope<'_>) -> Result<u32, ExnThrown> {
    pop_int_in_range(stack, scope, c"expected a u32")
}

pub fn pop_s8(stack: &mut CallStack, scope: &Scope<'_>) -> Result<i8, ExnThrown> {
    pop_int_in_range(stack, scope, c"expected an s8")
}

pub fn pop_s16(stack: &mut CallStack, scope: &Scope<'_>) -> Result<i16, ExnThrown> {
    pop_int_in_range(stack, scope, c"expected an s16")
}

pub fn pop_s32(stack: &mut CallStack, scope: &Scope<'_>) -> Result<i32, ExnThrown> {
    pop_int_in_range(stack, scope, c"expected an s32")
}

/// Lower a `u64`, which must be a BigInt in `[0, 2^64)`. A number is rejected,
/// as assigning one to a `BigUint64Array` element is.
pub fn pop_u64(stack: &mut CallStack, scope: &Scope<'_>) -> Result<u64, ExnThrown> {
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
    let v = scope.root_value(stack.pop_value());
    if !v.is_bigint() {
        return Err(throw_type_error(scope, c"expected an s64 (BigInt)"));
    }
    let bi = js::bigint::to_bigint(scope, v)?;
    // `to_int64` is modulo 2^64, so `2n**63n` would lower as `i64::MIN`.
    js::bigint::fits_i64(bi).ok_or_else(|| throw_type_error(scope, c"s64 BigInt is out of range"))
}

pub fn pop_f32(stack: &mut CallStack, scope: &Scope<'_>) -> Result<f32, ExnThrown> {
    let v = scope.root_value(stack.pop_value());
    Ok(pop_number(v, scope, c"expected an f32")? as f32)
}

pub fn pop_f64(stack: &mut CallStack, scope: &Scope<'_>) -> Result<f64, ExnThrown> {
    let v = scope.root_value(stack.pop_value());
    pop_number(v, scope, c"expected an f64")
}

/// Lower a `char`: the JS value must be a string holding exactly one Unicode
/// scalar value.
///
/// The scalar may be a single UTF-16 code unit or a well-formed surrogate pair.
/// A lone surrogate is not a scalar value and the canonical ABI has no `char`
/// for it, so it is rejected rather than silently replaced. The code units are
/// read off the `JSString` directly, so no `String` is allocated per element of
/// a `list<char>`.
pub fn pop_char(stack: &mut CallStack, scope: &Scope<'_>) -> Result<char, ExnThrown> {
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
    pop_jsstring(stack, scope, c"expected a string")
}

/// Lower a record: read each field by its verbatim mangled name and push the
/// field values back so the first field ends up on top, ready for the per-field
/// `pop_*` calls that follow. The generated code emits those in forward
/// declaration order, each popping the top. Iterating the fields back-to-front
/// pushes the last field deepest and the first field on top. The generated code
/// drives this and [`pop_tuple`] the same way.
pub fn pop_record(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    field_names: &[&'static CStr],
) -> Result<(), ExnThrown> {
    let obj = pop_object(stack, scope, c"expected a record (object)")?;
    for name in field_names.iter().rev() {
        let field = get_field(&obj, scope, name)?;
        stack.push_value(field.get());
    }
    Ok(())
}

/// Lower a tuple: read each element and push them back so element 0 ends up on
/// top, ready for the per-element `pop_*` calls the generated code emits in
/// forward order. Reading back-to-front pushes the last element deepest and the
/// first element on top.
pub fn pop_tuple(stack: &mut CallStack, scope: &Scope<'_>, count: usize) -> Result<(), ExnThrown> {
    let obj = pop_object(stack, scope, c"expected a tuple (array)")?;
    for index in 0..count {
        let elem = obj.get_element(scope, (count - index - 1) as u32)?;
        stack.push_value(elem.get());
    }
    Ok(())
}

/// Lower a variant `{tag, val?}`. Returns the case discriminant, pushing the
/// payload value back for a following `pop_*` iff the matched case has one.
pub fn pop_variant(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    cases: &[(&'static CStr, bool)],
) -> Result<u32, ExnThrown> {
    let obj = pop_object(stack, scope, c"expected a variant (object)")?;
    let discriminant = tag_index(&obj, scope, cases.iter().map(|(name, _)| *name))?
        .ok_or_else(|| throw_type_error(scope, c"variant tag does not match any case"))?;
    if cases[discriminant].1 {
        let val = get_field(&obj, scope, c"val")?;
        stack.push_value(val.get());
    }
    Ok(discriminant as u32)
}

/// Lower an enum value, given as its declaration-order index, checking it against
/// `case_count`.
pub fn pop_enum(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    case_count: u32,
) -> Result<u32, ExnThrown> {
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

/// Lower an option. `undefined` and `null` are both `none` (returns `false`).
/// Otherwise it is `some` (returns `true`): the payload is pushed back for a
/// following `pop_*`, unwrapping the `{val}` layer iff the inner type is itself
/// an option.
pub fn pop_option(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    inner_is_option: bool,
) -> Result<bool, ExnThrown> {
    let top = stack.last();
    if top.is_undefined() || top.is_null() {
        stack.pop_value();
        Ok(false)
    } else if inner_is_option {
        let obj = pop_object(stack, scope, c"expected a nested option wrapper (object)")?;
        let val = get_field(&obj, scope, c"val")?;
        stack.push_value(val.get());
        Ok(true)
    } else {
        // Leave the payload value on the stack as-is.
        Ok(true)
    }
}

/// Lower a result `{tag, val?}`. Returns the discriminant, 0 for ok and 1 for
/// err, and pushes the payload value back for a following `pop_*` iff the matched
/// arm has one.
pub fn pop_result(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    ok_has_payload: bool,
    err_has_payload: bool,
) -> Result<u32, ExnThrown> {
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
        let val = get_field(&obj, scope, c"val")?;
        stack.push_value(val.get());
    }
    Ok(discriminant)
}

/// Lower a flags value, given as its bit set, checking that no bit at or above
/// `flag_count` is set. The set is an int32, as a bitwise-or of flags produces,
/// or a number in the `u32` range, and a negative int32 has bit 31 set.
pub fn pop_flags(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    flag_count: u32,
) -> Result<u32, ExnThrown> {
    let bits = if stack.last().is_int32() {
        stack.pop_value().to_int32() as u32
    } else {
        pop_u32(stack, scope)
            .map_err(|_| throw_type_error(scope, c"expected a flags value (a number)"))?
    };
    // A set bit at or above the flag count names no flag, and the canonical ABI
    // has no room for it.
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
// Resources
// ===========================================================================
//
// The resource-handle conversions delegate to `src/resources.rs`. The
// imported-versus-exported distinction and the exported `new` and `rep`
// operations are wit-dylib metadata the interpreter resolves per call, so each
// arm takes a resolved [`ResourceDesc`] rather than a bare type index. A lift
// arm additionally takes the JS drop callback to store on a freshly created
// imported wrapper, and a lower arm returns the handle.
//
// All four are fallible, because reading the handle field or building a wrapper
// can throw.

/// Lift an owned resource handle to its JavaScript wrapper (or slab object).
pub fn push_own(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
    handle: u32,
    dispose_fn: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    resources::push_own(stack, scope, desc, handle, dispose_fn)
}

/// Lift a borrowed resource handle to its JavaScript wrapper (or slab object).
pub fn push_borrow(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
    handle: u32,
    dispose_fn: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    resources::push_borrow(stack, scope, desc, handle, dispose_fn)
}

/// Lower a JavaScript resource wrapper to an owned canonical handle.
pub fn pop_own(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
) -> Result<u32, ExnThrown> {
    resources::pop_own(stack, scope, desc)
}

/// Lower a JavaScript resource wrapper to a borrowed canonical handle.
pub fn pop_borrow(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
) -> Result<u32, ExnThrown> {
    resources::pop_borrow(stack, scope, desc)
}

// ===========================================================================
// Lists
// ===========================================================================

/// Whether a list element of this shape lifts/lowers through a typed array.
///
/// Numeric scalars map to the matching typed array (`u8` → `Uint8Array`, …,
/// `u64` → `BigUint64Array`, `s64` → `BigInt64Array`).
pub fn list_uses_typed_array(elem: &TypeShape) -> bool {
    elem.numeric_layout().is_some()
}

/// Lift a numeric list from a raw element buffer into the matching typed array.
///
/// `data` points to `count` contiguous little-endian elements of the element
/// type, and the bytes are copied into a freshly allocated typed array.
///
/// # Safety
///
/// `data` must point to `count` valid, contiguous elements of `elem`'s native
/// type, and remain valid for the duration of the call.
pub unsafe fn push_numeric_list(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    elem: &TypeShape,
    data: *const u8,
    count: usize,
) -> Result<(), ExnThrown> {
    // Build the matching typed array from a correctly-typed view of the buffer,
    // then read back its object pointer. The pointer is consumed immediately by
    // `push_value`, so it needs no separate rooting. An empty list yields a
    // properly aligned empty slice rather than reinterpreting a possibly
    // unaligned/dangling zero-length buffer.
    //
    // `as_typed_slice` casts `data` to `*const $elem` and forms a `&[$elem]`,
    // which is sound given the documented precondition on `data` and `count`.
    macro_rules! as_typed_slice {
        ($elem:ty) => {{
            let typed = data.cast::<$elem>();
            if count == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(typed, count)
            }
        }};
    }
    stack.push_value(match elem {
        TypeShape::U8 => js::Uint8Array::with_data(scope, as_typed_slice!(u8))?.as_value(),
        TypeShape::S8 => js::Int8Array::with_data(scope, as_typed_slice!(i8))?.as_value(),
        TypeShape::U16 => js::Uint16Array::with_data(scope, as_typed_slice!(u16))?.as_value(),
        TypeShape::S16 => js::Int16Array::with_data(scope, as_typed_slice!(i16))?.as_value(),
        TypeShape::U32 => js::Uint32Array::with_data(scope, as_typed_slice!(u32))?.as_value(),
        TypeShape::S32 => js::Int32Array::with_data(scope, as_typed_slice!(i32))?.as_value(),
        TypeShape::U64 => js::BigUint64Array::with_data(scope, as_typed_slice!(u64))?.as_value(),
        TypeShape::S64 => js::BigInt64Array::with_data(scope, as_typed_slice!(i64))?.as_value(),
        TypeShape::F32 => js::Float32Array::with_data(scope, as_typed_slice!(f32))?.as_value(),
        TypeShape::F64 => js::Float64Array::with_data(scope, as_typed_slice!(f64))?.as_value(),
        _ => unreachable!("push_numeric_list called for a non-typed-array element"),
    });
    Ok(())
}

/// Begin lifting a generic (non-typed-array) list by pushing a fresh, empty JS
/// array onto the stack.
///
/// This backs `wit_dylib_ffi`'s `Call::push_list`: the generated code calls it
/// once per list, then calls [`list_append`] once per element to fold the element
/// values, pushed in between by the element lift, into this array.
///
/// `capacity` is the list's exact element count, so the array is created at that
/// length and [`list_append`] writes element `i` at index `i` from a cursor the
/// [`CallStack`] keeps, without reading the array's `length` back per element.
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

/// Append the top-of-stack value to the JS array directly beneath it, leaving
/// that array on top.
///
/// This backs `wit_dylib_ffi`'s `Call::list_append`: the element to append is on
/// top of the stack and the partially built array is the value beneath it. The
/// index comes from the cursor [`push_list_begin`] opened, so appends land
/// contiguously from index 0.
pub fn list_append(stack: &mut CallStack, scope: &Scope<'_>) -> Result<(), ExnThrown> {
    // Root the element before any further allocation: the array lookup below can
    // GC.
    let elem = scope.root_value(stack.pop_value());
    let index = stack.lift_list_next();
    let arr = js::Object::from_value(scope, stack.last())
        .map_err(|_| throw_type_error(scope, c"list value is not an object"))?;
    arr.define_element(
        scope,
        index as u32,
        elem,
        js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint,
    )?;
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

/// Lift a numeric list into the matching typed array, adopting its buffer
/// instead of copying it.
///
/// # Safety
///
/// `data` must be a non-null allocation of the global allocator made with
/// `layout`, holding `count` valid, contiguous elements of `elem`'s native type.
/// Ownership of the allocation transfers to this function.
pub unsafe fn push_owned_numeric_list(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    elem: &TypeShape,
    data: std::ptr::NonNull<u8>,
    layout: std::alloc::Layout,
    count: usize,
) -> Result<(), ExnThrown> {
    let (_, kind) = elem
        .numeric_layout()
        .expect("push_owned_numeric_list called for a non-typed-array element");
    let buffer = js::ArrayBuffer::from_external(scope, OwnedBuffer { ptr: data, layout })?;
    let array = js::typedarray::construct_view(scope, kind, buffer, 0, count)?;
    stack.push_value(array.as_value());
    Ok(())
}

/// Lower a numeric typed-array list into a freshly allocated canonical-ABI
/// buffer.
///
/// Returns the buffer, its layout and the element count. Ownership of the buffer
/// transfers to the caller, which must free it with exactly that layout. The
/// bytes are copied straight out of the typed array's backing store, so the
/// buffer outlives the JS array.
///
/// An empty list allocates nothing and returns a dangling but aligned pointer,
/// which a zero-sized `Layout` admits.
pub fn pop_numeric_list(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    elem: &TypeShape,
) -> Result<(*mut u8, std::alloc::Layout, usize), ExnThrown> {
    use js::typedarray::ViewKind;
    let (elem_size, expected) = elem
        .numeric_layout()
        .expect("pop_numeric_list called for a non-typed-array element");
    let obj = pop_object(stack, scope, c"expected a typed-array list")?;
    let view = js::ArrayBufferView::from_object(obj)
        .ok_or_else(|| throw_type_error(scope, c"expected a typed array"))?;
    // The guest must pass the typed array whose element kind matches the WIT
    // element type. Reinterpreting a mismatched view's raw bytes would silently
    // ship wrong canonical-ABI data to the host (and truncate a non-multiple
    // length), so reject the mismatch with a TypeError. `Uint8ClampedArray` is
    // byte-identical to `Uint8Array`, so accept it for `list<u8>`.
    let kind = view.view_kind();
    let matches =
        kind == expected || (expected == ViewKind::Uint8 && kind == ViewKind::Uint8Clamped);
    if !matches {
        return Err(throw_type_error(
            scope,
            c"typed-array element kind does not match the list element type",
        ));
    }

    // `kind` matches `elem`, so the byte length is an exact multiple of the
    // element size.
    let count = view.byte_length() / elem_size;
    let layout = elem
        .numeric_list_layout(count)
        .expect("element shape has a numeric layout");
    if count == 0 {
        return Ok((layout.align() as *mut u8, layout, 0));
    }
    // SAFETY: `layout` has a non-zero size, since `count > 0`.
    let dst = unsafe { std::alloc::alloc(layout) };
    if dst.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: allocating cannot run JS or collect, so the view's backing store is
    // still the one measured above. It holds exactly `layout.size()` bytes, `dst`
    // is freshly allocated for that size, and the two regions do not overlap.
    unsafe {
        let src = view.bytes();
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst, layout.size());
    }
    Ok((dst, layout, count))
}

// ===========================================================================
// Helpers
// ===========================================================================

/// The property key for `name`, cached on the current global under `name`'s address, so the name
/// is atomized once per global. A `'static` name keeps its address and contents, so no other name
/// can share its key.
fn property_id<'s>(
    scope: &'s Scope<'_>,
    name: &'static CStr,
) -> Result<js::native::HandleId<'s>, ExnThrown> {
    js::class::get_or_init_property_id(scope, name)
}

/// Define `name` as an enumerable data property of the fresh object `obj`.
fn define_field<'v>(
    obj: &js::Object<'_>,
    scope: &'v Scope<'_>,
    name: &'static CStr,
    value: impl js::conversion::ToJSVal<'v>,
) -> Result<(), ExnThrown> {
    let id = property_id(scope, name)?;
    obj.define_value_by_id(
        scope,
        id,
        value,
        js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint,
    )
}

/// Read the property `name` of `obj`.
pub(crate) fn get_field<'s>(
    obj: &js::Object<'_>,
    scope: &'s Scope<'_>,
    name: &'static CStr,
) -> Result<js::prelude::HandleValue<'s>, ExnThrown> {
    let id = property_id(scope, name)?;
    obj.get_property_by_id(scope, id)
}

/// A mangled identifier as the NUL-terminated string the property accessors take.
pub fn to_cstring(s: &str) -> CString {
    CString::new(s).expect("mangled identifiers contain no interior NUL")
}

fn set_tag(obj: &js::Object<'_>, scope: &Scope<'_>, tag: &'static CStr) -> Result<(), ExnThrown> {
    // The tag's value is the atom of its cached property key, so every push of the same tag
    // shares one string.
    let tag_id = property_id(scope, tag)?;
    let tag_value = js::id::id_to_value(scope, *tag_id)?;
    define_field(obj, scope, c"tag", tag_value)
}

/// The index of the `tag` property's value among `names`, or `None` if it is
/// not a string or matches none of them.
///
/// The comparison runs against the JS string directly, so no `String` is
/// allocated per variant or result lowered.
pub(crate) fn tag_index<'a>(
    obj: &js::Object<'_>,
    scope: &Scope<'_>,
    names: impl Iterator<Item = &'a CStr>,
) -> Result<Option<usize>, ExnThrown> {
    let tag = get_field(obj, scope, c"tag")?;
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

/// Pop a value expected to be an object, rooting it.
fn pop_object<'s>(
    stack: &mut CallStack,
    scope: &'s Scope<'_>,
    err: &std::ffi::CStr,
) -> Result<js::Object<'s>, ExnThrown> {
    // The popped value is consumed inline by `from_value` (which roots the
    // object into `scope`), so it is never bound to an unrooted local.
    js::Object::from_value(scope, stack.pop_value()).map_err(|_| throw_type_error(scope, err))
}

/// Pop a value expected to be a string, returning its UTF-8 contents.
fn pop_jsstring(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    err: &std::ffi::CStr,
) -> Result<String, ExnThrown> {
    let v = scope.root_value(stack.pop_value());
    if !v.is_string() {
        return Err(throw_type_error(scope, err));
    }
    String::from_jsval_throwing(scope, v, ())
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
        // since `3.7 as i64 == 3`. The range narrowing below catches magnitude.
        integral_in_range(v.to_number(), -9223372036854775808.0, 9223372036854775808.0)
            .ok_or_else(|| throw_type_error(scope, err))? as i64
    } else {
        return Err(throw_type_error(scope, err));
    };
    T::try_from(n).map_err(|_| throw_type_error(scope, err))
}

/// Accept a JS number only if it is a finite, exact integer in `[lo, hi)`,
/// returning it unchanged. Rejects NaN, infinities, fractional values, and
/// out-of-range magnitudes so the integer lowering paths throw a `TypeError`
/// instead of silently saturating (`1e30 as u64 == u64::MAX`) or truncating.
fn integral_in_range(n: f64, lo: f64, hi: f64) -> Option<f64> {
    (n.is_finite() && n.fract() == 0.0 && n >= lo && n < hi).then_some(n)
}
