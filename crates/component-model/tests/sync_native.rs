// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Native integration tests for the JS↔WIT value-conversion core.
//!
//! These run against a real SpiderMonkey runtime, exercising the conversion
//! mapping and the GC-traced call stack without any wasm build. The whole suite
//! lives in a single `#[test]` function because `JSEngine` can be initialized
//! only once per process, as `crates/core-runtime/tests/js_api.rs` does.
//!
//! Most cases use the [`WitValue`] test driver, which walks a value tree and
//! drives the per-method `push_*`/`pop_*` functions in exactly the order
//! `wit-dylib`'s generated code would. That makes a round trip through the
//! driver a genuine exercise of the real conversion functions, not a mock.

use component_model::resources::{self, ResourceDesc};
use component_model::value::{self, TypeShape};
use component_model::{install_tracer, mangle_name, CallStack};
use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::{register_global_initializer, Runtime};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

mod shared;
use shared::{as_string, force_gc};

#[test]
fn sync_native() {
    // `ComponentError` must be on the global before the runtime is created.
    register_global_initializer(component_model::add_to_global);

    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    install_tracer(&scope);
    resources::init_finalization(&scope).unwrap();

    stack_survives_gc(&scope);
    deferred_deallocations_run_on_drop(&scope);
    primitive_round_trips(&scope);
    sixty_four_bit_values_are_bigints(&scope);
    validation_names_the_failing_part(&scope);
    snapshot_reads_each_part_once(&scope);
    nan_canonicalization(&scope);
    char_rejects_multichar(&scope);
    non_ascii_string(&scope);
    record_round_trip(&scope);
    record_field_mangling(&scope);
    pop_record_stack_order(&scope);
    push_record_stack_order(&scope);
    tuple_round_trip(&scope);
    variant_round_trips(&scope);
    enum_round_trips(&scope);
    option_nesting(&scope);
    result_round_trips(&scope);
    flags_round_trip(&scope);
    list_round_trips(&scope);
    numeric_list_kind_mismatch_throws(&scope);
    type_mismatch_throws(&scope);

    // Resources and `ComponentError`.
    component_error_basics(&scope);
    component_error_export_paths(&scope);
    imported_resource_round_trip(&scope);
    imported_resource_dispose(&scope);
    exported_slab(&scope);
    exported_resource_to_canon_mints_per_lowering(&scope);
    exported_resource_lift_lower(&scope);
    exported_resource_reexport_round_trip(&scope);
    exported_resource_drop_runs_hook(&scope);
    exported_resource_finalizer_ignores_dtor_hook(&scope);
    lent_wrapper_is_kept_for_the_call(&scope);
    borrow_release_ordering(&scope);
    borrow_release_drains_through_throw(&scope);
    exported_borrow_release(&scope);
    imported_dispose_strips_handle(&scope);
    abandoned_wrapper_is_dropped_by_gc(&scope);
}

// ===========================================================================
// CallStack GC smoke test
// ===========================================================================

fn stack_survives_gc(scope: &Scope<'_>) {
    let mut stack = CallStack::new();

    let s = js::JSString::from_str(scope, "hello call stack").unwrap();
    stack.push_value(s.as_value());
    stack.push_value(js::value::from_i32(7));
    stack.push_value(js::value::from_f64(3.5));
    assert_eq!(stack.len(), 3);

    force_gc(scope);

    let f = stack.pop_value();
    assert!(f.is_double() && (f.to_double() - 3.5).abs() < f64::EPSILON);
    let i = stack.pop_value();
    assert!(i.is_int32() && i.to_int32() == 7);
    let popped = stack.pop_value();
    assert!(popped.is_string());
    assert_eq!(as_string(scope, popped), "hello call stack");

    assert!(stack.is_empty());
    drop(stack);
    force_gc(scope);
}

/// Deferred deallocations are freed exactly once when the stack drops. The free
/// itself is not directly observable, so this guards against a panic or
/// double-free under `debugmozjs` rather than asserting on the freed memory.
fn deferred_deallocations_run_on_drop(_scope: &Scope<'_>) {
    use std::alloc::Layout;

    let mut stack = CallStack::new();
    for size in [1usize, 64, 4096] {
        let layout = Layout::from_size_align(size, 8).unwrap();
        // SAFETY: a non-zero-size layout, and ownership of the allocation is handed
        // to the stack, which frees it on drop.
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null());
        unsafe { stack.defer_deallocate(ptr, layout) };
    }
    // The string buffer hands back a stable borrow into stored owned strings.
    assert_eq!(stack.store_string("borrowed".to_string()), "borrowed");

    // Drop runs the deferred frees, and reaching the next line without aborting is
    // the assertion.
    drop(stack);
}

// ===========================================================================
// A test-only WIT value tree and a recursive driver over it.
// ===========================================================================

#[derive(Clone, Debug, PartialEq)]
enum WitValue {
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
    List(Vec<WitValue>),
    Record(Vec<(String, WitValue)>),
    Tuple(Vec<WitValue>),
    /// (discriminant, optional payload)
    Variant(u32, Option<Box<WitValue>>),
    Enum(u32),
    /// `None` = none; `Some` = some(payload)
    Option(Option<Box<WitValue>>),
    /// `Ok`/`Err` with an optional payload
    Result(Result<Option<Box<WitValue>>, Option<Box<WitValue>>>),
    Flags(u32),
}

/// Lift a `WitValue` onto the stack, recursing into children first so the
/// composite constructors find their payloads already pushed in the order they
/// expect.
fn lift(stack: &mut CallStack, scope: &Scope<'_>, shape: &TypeShape, v: &WitValue) {
    match (shape, v) {
        (TypeShape::Bool, WitValue::Bool(b)) => value::push_bool(stack, *b),
        (TypeShape::U8, WitValue::U8(n)) => value::push_u8(stack, *n),
        (TypeShape::U16, WitValue::U16(n)) => value::push_u16(stack, *n),
        (TypeShape::U32, WitValue::U32(n)) => value::push_u32(stack, *n),
        (TypeShape::U64, WitValue::U64(n)) => value::push_u64(stack, scope, *n).unwrap(),
        (TypeShape::S8, WitValue::S8(n)) => value::push_s8(stack, *n),
        (TypeShape::S16, WitValue::S16(n)) => value::push_s16(stack, *n),
        (TypeShape::S32, WitValue::S32(n)) => value::push_s32(stack, *n),
        (TypeShape::S64, WitValue::S64(n)) => value::push_s64(stack, scope, *n).unwrap(),
        (TypeShape::F32, WitValue::F32(n)) => value::push_f32(stack, *n),
        (TypeShape::F64, WitValue::F64(n)) => value::push_f64(stack, *n),
        (TypeShape::Char, WitValue::Char(c)) => value::push_char(stack, scope, *c).unwrap(),
        (TypeShape::String, WitValue::String(s)) => value::push_string(stack, scope, s).unwrap(),
        (TypeShape::List(elem), WitValue::List(items)) => lift_list(stack, scope, elem, items),
        (TypeShape::Record(fields), WitValue::Record(values)) => {
            // Push fields forward so field[0] sits deepest, matching the
            // generated code's forward-lift convention (same as tuples).
            for (name, ty) in fields {
                let (_, val) = values.iter().find(|(n, _)| n == name).unwrap();
                lift(stack, scope, ty, val);
            }
            value::push_record(stack, scope, &field_names(fields)).unwrap();
        }
        (TypeShape::Tuple(types), WitValue::Tuple(items)) => {
            // Push elements forward so element[0] sits deepest.
            for (ty, item) in types.iter().zip(items) {
                lift(stack, scope, ty, item);
            }
            value::push_tuple(stack, scope, types.len()).unwrap();
        }
        (TypeShape::Variant(cases), WitValue::Variant(disc, payload)) => {
            if let Some(p) = payload {
                let ty = cases[*disc as usize].1.as_ref().unwrap();
                lift(stack, scope, ty, p);
            }
            value::push_variant(stack, scope, &case_tags(cases), *disc).unwrap();
        }
        (TypeShape::Enum(names), WitValue::Enum(disc)) => {
            value::push_enum(stack, scope, names.len() as u32, *disc).unwrap();
        }
        (TypeShape::Option(inner), WitValue::Option(payload)) => {
            if let Some(p) = payload {
                lift(stack, scope, inner, p);
            }
            value::push_option(stack, scope, inner_is_option(inner), payload.is_some()).unwrap();
        }
        (TypeShape::Result(ok, err), WitValue::Result(r)) => {
            let (is_err, payload) = match r {
                Ok(p) => (false, p),
                Err(p) => (true, p),
            };
            if let Some(p) = payload {
                let ty = if is_err { err } else { ok }.as_ref().unwrap();
                lift(stack, scope, ty, p);
            }
            value::push_result(stack, scope, ok.is_some(), err.is_some(), is_err).unwrap();
        }
        (TypeShape::Flags(_), WitValue::Flags(bits)) => {
            value::push_flags(stack, *bits);
        }
        (s, v) => panic!("lift: shape {s:?} does not match value {v:?}"),
    }
}

fn lift_list(stack: &mut CallStack, scope: &Scope<'_>, elem: &TypeShape, items: &[WitValue]) {
    if value::list_uses_typed_array(elem) {
        lift_numeric_list(stack, scope, elem, items);
    } else {
        // Drive the production generic-list lift the way wit-dylib's codegen does:
        // push the empty array, then lift each element and fold it in via
        // `list_append`.
        value::push_list_begin(stack, scope, items.len()).unwrap();
        for item in items {
            lift(stack, scope, elem, item);
            value::list_append(stack, scope).unwrap();
        }
    }
}

/// Drive the typed-array lift path the way `wit-dylib`'s generated code
/// hands `push_numeric_list` a pointer to a buffer of native elements.
///
/// `push_numeric_list` casts the base pointer to `*const T` for the element
/// type `T`, so the buffer must be `T`-aligned. Each arm collects a
/// properly-typed `Vec<T>` (whose allocation is element-aligned) and passes
/// `v.as_ptr() as *const u8`. Passing the bytes of a `Vec<u8>`, which is align 1, would
/// be undefined behavior whenever `T`'s alignment exceeds 1.
fn lift_numeric_list(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    elem: &TypeShape,
    items: &[WitValue],
) {
    /// Build a typed `Vec<$ty>` from the matching `WitValue` variant and lift
    /// it through `push_numeric_list` with an element-aligned base pointer.
    macro_rules! lift_typed {
        ($ty:ty, $variant:path) => {{
            let v: Vec<$ty> = items
                .iter()
                .map(|item| match item {
                    $variant(n) => *n,
                    other => panic!("lift_numeric_list: mismatch {elem:?} / {other:?}"),
                })
                .collect();
            // SAFETY: `v` holds `v.len()` contiguous valid `$ty` elements at an
            // `$ty`-aligned address, and outlives this call.
            unsafe {
                value::push_numeric_list(stack, scope, elem, v.as_ptr() as *const u8, v.len())
                    .unwrap();
            }
        }};
    }
    match elem {
        TypeShape::U8 => lift_typed!(u8, WitValue::U8),
        TypeShape::S8 => lift_typed!(i8, WitValue::S8),
        TypeShape::U16 => lift_typed!(u16, WitValue::U16),
        TypeShape::S16 => lift_typed!(i16, WitValue::S16),
        TypeShape::U32 => lift_typed!(u32, WitValue::U32),
        TypeShape::S32 => lift_typed!(i32, WitValue::S32),
        TypeShape::U64 => lift_typed!(u64, WitValue::U64),
        TypeShape::S64 => lift_typed!(i64, WitValue::S64),
        TypeShape::F32 => lift_typed!(f32, WitValue::F32),
        TypeShape::F64 => lift_typed!(f64, WitValue::F64),
        _ => panic!("lift_numeric_list: not a typed-array element: {elem:?}"),
    }
}

/// Lower the top-of-stack JS value into a `WitValue`, recursing as the
/// destructuring `pop_*` functions push children back.
fn lower(stack: &mut CallStack, scope: &Scope<'_>, shape: &TypeShape) -> WitValue {
    match shape {
        TypeShape::Bool => WitValue::Bool(value::pop_bool(stack, scope).unwrap()),
        TypeShape::U8 => WitValue::U8(value::pop_u8(stack, scope).unwrap()),
        TypeShape::U16 => WitValue::U16(value::pop_u16(stack, scope).unwrap()),
        TypeShape::U32 => WitValue::U32(value::pop_u32(stack, scope).unwrap()),
        TypeShape::U64 => WitValue::U64(value::pop_u64(stack, scope).unwrap()),
        TypeShape::S8 => WitValue::S8(value::pop_s8(stack, scope).unwrap()),
        TypeShape::S16 => WitValue::S16(value::pop_s16(stack, scope).unwrap()),
        TypeShape::S32 => WitValue::S32(value::pop_s32(stack, scope).unwrap()),
        TypeShape::S64 => WitValue::S64(value::pop_s64(stack, scope).unwrap()),
        TypeShape::F32 => WitValue::F32(value::pop_f32(stack, scope).unwrap()),
        TypeShape::F64 => WitValue::F64(value::pop_f64(stack, scope).unwrap()),
        TypeShape::Char => WitValue::Char(value::pop_char(stack, scope).unwrap()),
        TypeShape::String => WitValue::String(value::pop_string(stack, scope).unwrap()),
        TypeShape::List(elem) => lower_list(stack, scope, elem),
        TypeShape::Record(fields) => {
            value::pop_record(stack, scope, &field_names(fields)).unwrap();
            // pop_record pushes fields so field[0] ends up on top, so lower in
            // declaration order directly (same as tuples).
            let mut values: Vec<(String, WitValue)> = Vec::with_capacity(fields.len());
            for (name, ty) in fields {
                values.push((name.clone(), lower(stack, scope, ty)));
            }
            WitValue::Record(values)
        }
        TypeShape::Tuple(types) => {
            value::pop_tuple(stack, scope, types.len()).unwrap();
            // pop_tuple pushes elements so element[0] ends up on top, so lower in
            // declaration order directly.
            let mut values: Vec<WitValue> = Vec::with_capacity(types.len());
            for ty in types {
                values.push(lower(stack, scope, ty));
            }
            WitValue::Tuple(values)
        }
        TypeShape::Variant(cases) => {
            let disc = value::pop_variant(stack, scope, &case_tags(cases)).unwrap();
            let payload = cases[disc as usize]
                .1
                .as_ref()
                .map(|ty| Box::new(lower(stack, scope, ty)));
            WitValue::Variant(disc, payload)
        }
        TypeShape::Enum(names) => {
            WitValue::Enum(value::pop_enum(stack, scope, names.len() as u32).unwrap())
        }
        TypeShape::Option(inner) => {
            if value::pop_option(stack, scope, inner_is_option(inner)).unwrap() {
                WitValue::Option(Some(Box::new(lower(stack, scope, inner))))
            } else {
                WitValue::Option(None)
            }
        }
        TypeShape::Result(ok, err) => {
            let disc = value::pop_result(stack, scope, ok.is_some(), err.is_some()).unwrap();
            if disc == 0 {
                let p = ok.as_ref().map(|ty| Box::new(lower(stack, scope, ty)));
                WitValue::Result(Ok(p))
            } else {
                let p = err.as_ref().map(|ty| Box::new(lower(stack, scope, ty)));
                WitValue::Result(Err(p))
            }
        }
        TypeShape::Flags(names) => {
            WitValue::Flags(value::pop_flags(stack, scope, names.len() as u32).unwrap())
        }
        TypeShape::OwnResource { .. } | TypeShape::BorrowResource { .. } => {
            unreachable!("resources are not converted by these tests")
        }
        TypeShape::Stream(_) | TypeShape::Future(_) => {
            // Streams/futures are wasm-only (their conversion lives in
            // `crate::streams`, gated to `wasm32`), so the native conversion tests
            // never construct one.
            unreachable!("streams/futures are wasm-only, not exercised natively")
        }
        TypeShape::Unsupported(what) => unreachable!("{what} is not converted"),
    }
}

/// `value::pop_numeric_list` into an owned `Vec<u8>`, freeing the canonical
/// buffer it hands over. The interpreter defers that free to the call stack's
/// drop instead, which these tests do not model.
fn pop_numeric_list_bytes(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    elem: &TypeShape,
) -> (Vec<u8>, usize) {
    let (ptr, layout, count) = value::pop_numeric_list(stack, scope, elem).unwrap();
    if count == 0 {
        return (Vec::new(), 0);
    }
    // SAFETY: `pop_numeric_list` transferred ownership of `layout.size()` bytes
    // allocated with the global allocator under exactly `layout`.
    let bytes = unsafe {
        let bytes = std::slice::from_raw_parts(ptr, layout.size()).to_vec();
        std::alloc::dealloc(ptr, layout);
        bytes
    };
    (bytes, count)
}

/// The narrow descriptors the value intrinsics take, derived from the test
/// driver's `TypeShape`. The interpreter builds these straight from wit-dylib
/// metadata instead, without a `TypeShape` at all.
fn field_names(fields: &[(String, TypeShape)]) -> Vec<&'static std::ffi::CStr> {
    fields
        .iter()
        .map(|(name, _)| &*Box::leak(value::to_cstring(name).into_boxed_c_str()))
        .collect()
}

fn case_tags(cases: &[(String, Option<TypeShape>)]) -> Vec<(&'static std::ffi::CStr, bool)> {
    cases
        .iter()
        .map(|(name, payload)| {
            (
                &*Box::leak(value::to_cstring(name).into_boxed_c_str()),
                payload.is_some(),
            )
        })
        .collect()
}

fn inner_is_option(inner: &TypeShape) -> bool {
    matches!(inner, TypeShape::Option(_))
}

fn lower_list(stack: &mut CallStack, scope: &Scope<'_>, elem: &TypeShape) -> WitValue {
    if value::list_uses_typed_array(elem) {
        let (bytes, count) = pop_numeric_list_bytes(stack, scope, elem);
        WitValue::List(numeric_list_from_bytes(elem, &bytes, count))
    } else {
        let len = stack.lower_list_begin(scope).unwrap();
        let mut items = Vec::with_capacity(len);
        for _ in 0..len {
            stack.lower_list_next(scope).unwrap();
            items.push(lower(stack, scope, elem));
        }
        stack.lower_list_end();
        WitValue::List(items)
    }
}

/// Pop the top value and view it as an object (it roots internally).
fn pop_object<'s>(stack: &mut CallStack, scope: &'s Scope<'_>) -> js::Object<'s> {
    js::Object::from_value(scope, stack.pop_value()).unwrap()
}

/// Round-trip a value lift→lower and assert it comes back equal.
fn round_trip(scope: &Scope<'_>, shape: &TypeShape, value: &WitValue) {
    let mut stack = CallStack::new();
    lift(&mut stack, scope, shape, value);
    assert_eq!(
        stack.len(),
        1,
        "lift of {value:?} left {} values",
        stack.len()
    );
    // A GC between lift and lower must not disturb the held value.
    force_gc(scope);
    let back = lower(&mut stack, scope, shape);
    assert!(
        stack.is_empty(),
        "lower of {value:?} left values on the stack"
    );
    assert_eq!(&back, value, "round trip of {value:?} (shape {shape:?})");
}

// ===========================================================================
// Numeric list bytes -> WitValue helper (test side)
// ===========================================================================
//
// The lift direction builds element-aligned typed buffers in `lift_numeric_list`
// (see there for why a `Vec<u8>` would be undefined behavior). The lower
// direction reads the bytes `pop_numeric_list` returns, which are accessed only
// as byte subslices, so no alignment concern arises here.

fn numeric_list_from_bytes(elem: &TypeShape, bytes: &[u8], count: usize) -> Vec<WitValue> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(match elem {
            TypeShape::U8 => WitValue::U8(bytes[i]),
            TypeShape::S8 => WitValue::S8(bytes[i] as i8),
            TypeShape::U16 => WitValue::U16(u16::from_ne_bytes(
                bytes[i * 2..i * 2 + 2].try_into().unwrap(),
            )),
            TypeShape::S16 => WitValue::S16(i16::from_ne_bytes(
                bytes[i * 2..i * 2 + 2].try_into().unwrap(),
            )),
            TypeShape::U32 => WitValue::U32(u32::from_ne_bytes(
                bytes[i * 4..i * 4 + 4].try_into().unwrap(),
            )),
            TypeShape::S32 => WitValue::S32(i32::from_ne_bytes(
                bytes[i * 4..i * 4 + 4].try_into().unwrap(),
            )),
            TypeShape::U64 => WitValue::U64(u64::from_ne_bytes(
                bytes[i * 8..i * 8 + 8].try_into().unwrap(),
            )),
            TypeShape::S64 => WitValue::S64(i64::from_ne_bytes(
                bytes[i * 8..i * 8 + 8].try_into().unwrap(),
            )),
            TypeShape::F32 => WitValue::F32(f32::from_ne_bytes(
                bytes[i * 4..i * 4 + 4].try_into().unwrap(),
            )),
            TypeShape::F64 => WitValue::F64(f64::from_ne_bytes(
                bytes[i * 8..i * 8 + 8].try_into().unwrap(),
            )),
            _ => unreachable!(),
        });
    }
    out
}

// ===========================================================================
// Cases
// ===========================================================================

fn primitive_round_trips(scope: &Scope<'_>) {
    round_trip(scope, &TypeShape::Bool, &WitValue::Bool(true));
    round_trip(scope, &TypeShape::Bool, &WitValue::Bool(false));
    round_trip(scope, &TypeShape::U8, &WitValue::U8(255));
    round_trip(scope, &TypeShape::U16, &WitValue::U16(65535));
    round_trip(scope, &TypeShape::U32, &WitValue::U32(u32::MAX));
    round_trip(scope, &TypeShape::S8, &WitValue::S8(-128));
    round_trip(scope, &TypeShape::S16, &WitValue::S16(-32768));
    round_trip(scope, &TypeShape::S32, &WitValue::S32(i32::MIN));
    round_trip(scope, &TypeShape::F32, &WitValue::F32(1.5));
    round_trip(scope, &TypeShape::F64, &WitValue::F64(-1234.5678));
    round_trip(scope, &TypeShape::Char, &WitValue::Char('A'));
    round_trip(scope, &TypeShape::Char, &WitValue::Char('🦊'));
    round_trip(scope, &TypeShape::String, &WitValue::String("hello".into()));
    round_trip(scope, &TypeShape::String, &WitValue::String(String::new()));
}

/// 64-bit integers lift as BigInts whatever their magnitude, and round-trip at
/// both ends of their ranges.
fn sixty_four_bit_values_are_bigints(scope: &Scope<'_>) {
    round_trip(scope, &TypeShape::U64, &WitValue::U64(0));
    round_trip(scope, &TypeShape::U64, &WitValue::U64(42));
    round_trip(scope, &TypeShape::S64, &WitValue::S64(-42));
    round_trip(scope, &TypeShape::U64, &WitValue::U64(u64::MAX));
    round_trip(scope, &TypeShape::U64, &WitValue::U64(1 << 40));
    round_trip(scope, &TypeShape::S64, &WitValue::S64(i64::MIN));
    round_trip(scope, &TypeShape::S64, &WitValue::S64(i64::MAX));

    let mut stack = CallStack::new();
    value::push_u64(&mut stack, scope, 42).unwrap();
    assert!(stack.last().is_bigint(), "a small u64 lifts as a BigInt");
    stack.pop_value();
    value::push_s64(&mut stack, scope, -1).unwrap();
    assert!(stack.last().is_bigint(), "a small s64 lifts as a BigInt");
    stack.pop_value();
}

/// A NaN lifted to JS must come back as a NaN (canonicalized, never trapping).
fn nan_canonicalization(scope: &Scope<'_>) {
    let mut stack = CallStack::new();
    // A non-canonical NaN bit pattern would abort DoubleValue's assertion if
    // not canonicalized.
    let sneaky = f64::from_bits(0xFFFF_FFFF_FFFF_FFFF);
    assert!(sneaky.is_nan());
    value::push_f64(&mut stack, sneaky);
    assert!(stack.last().is_double() && stack.last().to_double().is_nan());
    let back = value::pop_f64(&mut stack, scope).unwrap();
    assert!(back.is_nan());

    value::push_f32(&mut stack, f32::from_bits(0xFFFF_FFFF));
    let back = value::pop_f32(&mut stack, scope).unwrap();
    assert!(back.is_nan());
}

fn char_rejects_multichar(scope: &Scope<'_>) {
    let mut stack = CallStack::new();
    // A two-character string is not a valid char.
    let s = js::JSString::from_str(scope, "ab").unwrap();
    stack.push_value(s.as_value());
    let err = value::pop_char(&mut stack, scope);
    assert!(
        err.is_err(),
        "multi-character string must not lower to a char"
    );
    js::exception::clear(scope);

    // An empty string is not a valid char either.
    let s = js::JSString::from_str(scope, "").unwrap();
    stack.push_value(s.as_value());
    assert!(value::pop_char(&mut stack, scope).is_err());
    js::exception::clear(scope);
}

fn non_ascii_string(scope: &Scope<'_>) {
    round_trip(
        scope,
        &TypeShape::String,
        &WitValue::String("héllo 🦊 café".into()),
    );
}

fn record_round_trip(scope: &Scope<'_>) {
    let shape = TypeShape::Record(vec![
        ("x".into(), TypeShape::U32),
        ("y".into(), TypeShape::String),
        ("z".into(), TypeShape::Bool),
    ]);
    let value = WitValue::Record(vec![
        ("x".into(), WitValue::U32(7)),
        ("y".into(), WitValue::String("hi".into())),
        ("z".into(), WitValue::Bool(true)),
    ]);
    round_trip(scope, &shape, &value);

    // Cross-check the JS shape directly: field names are plain properties.
    let mut stack = CallStack::new();
    lift(&mut stack, scope, &shape, &value);
    let obj = pop_object(&mut stack, scope);
    assert_eq!(obj.get_property(scope, c"x").unwrap().get().to_int32(), 7);
}

/// A WIT field name with punctuation must be mangled to its JS identifier on
/// the produced object.
fn record_field_mangling(scope: &Scope<'_>) {
    let mangled = mangle_name("my-field");
    assert_eq!(mangled, "myField");
    let shape = TypeShape::Record(vec![(mangled, TypeShape::U32)]);
    let value = WitValue::Record(vec![("myField".into(), WitValue::U32(99))]);

    let mut stack = CallStack::new();
    lift(&mut stack, scope, &shape, &value);
    let obj = pop_object(&mut stack, scope);
    assert!(obj.has_property(scope, c"myField").unwrap());
    assert_eq!(
        obj.get_property(scope, c"myField")
            .unwrap()
            .get()
            .to_int32(),
        99
    );

    // The round trip recovers it.
    round_trip(scope, &shape, &value);
}

/// Contract test for `pop_record`'s stack arrangement, independent of `lift`.
///
/// The generated code lowers record fields in forward declaration order, each
/// popping the stack top, so `pop_record` must leave field[0] on top and the
/// last field deepest. A round-trip echo cannot catch a flipped order (it would
/// cancel against a matching `push_record` flip), so this drives the stack
/// directly: pop one value per field and assert the order is field[0] first.
fn pop_record_stack_order(scope: &Scope<'_>) {
    let fields = vec![
        ("a".to_string(), TypeShape::U32),
        ("b".to_string(), TypeShape::U32),
        ("c".to_string(), TypeShape::U32),
    ];

    let mut stack = CallStack::new();
    let obj = js::compile::evaluate(scope, "({ a: 10, b: 20, c: 30 })").unwrap();
    let obj = Object::from_value(scope, obj).unwrap();
    stack.push_object(&obj);

    value::pop_record(&mut stack, scope, &field_names(&fields)).unwrap();
    assert_eq!(stack.len(), 3, "pop_record must push one value per field");

    // Force a GC across the destructured-but-not-yet-consumed values.
    force_gc(scope);

    // field[0] = a (10) is on top, then b (20), then c (30) deepest.
    assert_eq!(
        stack.pop_value().to_int32(),
        10,
        "field[0] (a) must be on top"
    );
    assert_eq!(stack.pop_value().to_int32(), 20, "field[1] (b) next");
    assert_eq!(stack.pop_value().to_int32(), 30, "field[2] (c) deepest");
    assert!(stack.is_empty());
}

/// Contract test for `push_record`'s stack arrangement, independent of `lower`.
///
/// The generated code lifts record fields in forward declaration order, each
/// pushing onto the stack top, so when `push_record` runs field[0] is deepest
/// and the last field is on top. This drives that arrangement directly: push
/// the field values forward (a=10 deepest, then b=20, then c=30 on top), call
/// `push_record`, and assert it assembled `{a:10, b:20, c:30}`, meaning it read
/// field[0] from the deepest slot, not the top.
fn push_record_stack_order(scope: &Scope<'_>) {
    let fields = vec![
        ("a".to_string(), TypeShape::U32),
        ("b".to_string(), TypeShape::U32),
        ("c".to_string(), TypeShape::U32),
    ];

    let mut stack = CallStack::new();
    // Forward lift order: field[0] (a) first and so deepest, field[2] (c) last
    // and so on top.
    stack.push_value(js::value::from_u32(10)); // a, deepest
    stack.push_value(js::value::from_u32(20)); // b
    stack.push_value(js::value::from_u32(30)); // c, on top

    // Force a GC across the values before the record is assembled.
    force_gc(scope);

    value::push_record(&mut stack, scope, &field_names(&fields)).unwrap();
    assert_eq!(
        stack.len(),
        1,
        "push_record leaves exactly the record object"
    );

    let obj = pop_object(&mut stack, scope);
    assert_eq!(obj.get_property(scope, c"a").unwrap().get().to_int32(), 10);
    assert_eq!(obj.get_property(scope, c"b").unwrap().get().to_int32(), 20);
    assert_eq!(obj.get_property(scope, c"c").unwrap().get().to_int32(), 30);
    assert!(stack.is_empty());
}

fn tuple_round_trip(scope: &Scope<'_>) {
    let shape = TypeShape::Tuple(vec![TypeShape::U8, TypeShape::String, TypeShape::S32]);
    let value = WitValue::Tuple(vec![
        WitValue::U8(1),
        WitValue::String("two".into()),
        WitValue::S32(-3),
    ]);
    round_trip(scope, &shape, &value);

    // Cross-check: element ordering is positional in a JS array.
    let mut stack = CallStack::new();
    lift(&mut stack, scope, &shape, &value);
    let arr = pop_object(&mut stack, scope);
    assert_eq!(arr.get_element(scope, 0).unwrap().get().to_int32(), 1);
    assert_eq!(arr.get_element(scope, 2).unwrap().get().to_int32(), -3);
}

fn variant_round_trips(scope: &Scope<'_>) {
    let shape = TypeShape::Variant(vec![
        ("none".into(), None),
        ("num".into(), Some(TypeShape::U32)),
        ("text".into(), Some(TypeShape::String)),
    ]);
    // Payload-free case.
    round_trip(scope, &shape, &WitValue::Variant(0, None));
    // Cases with payloads.
    round_trip(
        scope,
        &shape,
        &WitValue::Variant(1, Some(Box::new(WitValue::U32(5)))),
    );
    round_trip(
        scope,
        &shape,
        &WitValue::Variant(2, Some(Box::new(WitValue::String("x".into())))),
    );
}

fn enum_round_trips(scope: &Scope<'_>) {
    let shape = TypeShape::Enum(vec!["red".into(), "green".into(), "blue".into()]);
    round_trip(scope, &shape, &WitValue::Enum(0));
    round_trip(scope, &shape, &WitValue::Enum(2));

    // The JS form is the case index, so a value past the last case fails to
    // lower rather than naming a case that does not exist.
    let mut stack = CallStack::new();
    stack.push_value(js::value::from_u32(3));
    assert!(value::pop_enum(&mut stack, scope, 3).is_err());
    js::exception::clear(scope);

    // So does a value of the wrong type.
    let s = js::JSString::from_str(scope, "purple").unwrap();
    stack.push_value(s.as_value());
    assert!(value::pop_enum(&mut stack, scope, 3).is_err());
    js::exception::clear(scope);
}

/// The subtle case: `option<option<T>>` distinguishes `some(none)` from `none`.
fn option_nesting(scope: &Scope<'_>) {
    let flat = TypeShape::Option(Box::new(TypeShape::U32));
    round_trip(scope, &flat, &WitValue::Option(None));
    round_trip(
        scope,
        &flat,
        &WitValue::Option(Some(Box::new(WitValue::U32(8)))),
    );

    let nested = TypeShape::Option(Box::new(TypeShape::Option(Box::new(TypeShape::U32))));
    // none, some(none), some(some(x)) must all round-trip distinctly.
    round_trip(scope, &nested, &WitValue::Option(None));
    round_trip(
        scope,
        &nested,
        &WitValue::Option(Some(Box::new(WitValue::Option(None)))),
    );
    round_trip(
        scope,
        &nested,
        &WitValue::Option(Some(Box::new(WitValue::Option(Some(Box::new(
            WitValue::U32(9),
        )))))),
    );

    // Assert the exact JS shapes the three nested cases produce.
    // none -> undefined
    let mut stack = CallStack::new();
    lift(&mut stack, scope, &nested, &WitValue::Option(None));
    assert!(stack.last().is_undefined());
    stack.pop_value();
    // some(none) -> {val: undefined}
    lift(
        &mut stack,
        scope,
        &nested,
        &WitValue::Option(Some(Box::new(WitValue::Option(None)))),
    );
    let obj = pop_object(&mut stack, scope);
    assert!(obj.has_property(scope, c"val").unwrap());
    assert!(obj
        .get_property(scope, c"val")
        .unwrap()
        .get()
        .is_undefined());
    // some(some(9)) -> {val: 9}
    lift(
        &mut stack,
        scope,
        &nested,
        &WitValue::Option(Some(Box::new(WitValue::Option(Some(Box::new(
            WitValue::U32(9),
        )))))),
    );
    let obj = pop_object(&mut stack, scope);
    assert_eq!(obj.get_property(scope, c"val").unwrap().get().to_int32(), 9);
}

fn result_round_trips(scope: &Scope<'_>) {
    // result<u32, string>
    let both = TypeShape::Result(
        Some(Box::new(TypeShape::U32)),
        Some(Box::new(TypeShape::String)),
    );
    round_trip(
        scope,
        &both,
        &WitValue::Result(Ok(Some(Box::new(WitValue::U32(1))))),
    );
    round_trip(
        scope,
        &both,
        &WitValue::Result(Err(Some(Box::new(WitValue::String("bad".into()))))),
    );

    // result (no payloads either side)
    let neither = TypeShape::Result(None, None);
    round_trip(scope, &neither, &WitValue::Result(Ok(None)));
    round_trip(scope, &neither, &WitValue::Result(Err(None)));

    // result<_, string> (err payload only)
    let err_only = TypeShape::Result(None, Some(Box::new(TypeShape::String)));
    round_trip(scope, &err_only, &WitValue::Result(Ok(None)));
    round_trip(
        scope,
        &err_only,
        &WitValue::Result(Err(Some(Box::new(WitValue::String("e".into()))))),
    );
}

fn flags_round_trip(scope: &Scope<'_>) {
    let shape = TypeShape::Flags(vec!["a".into(), "b".into(), "c".into()]);
    round_trip(scope, &shape, &WitValue::Flags(0b101));
    round_trip(scope, &shape, &WitValue::Flags(0));
    round_trip(scope, &shape, &WitValue::Flags(0b111));

    // The JS form is the bare bit set, one bit per flag in declaration order.
    let mut stack = CallStack::new();
    value::push_flags(&mut stack, 0b10);
    let v = scope.root_value(stack.pop_value());
    assert_eq!(v.to_int32(), 0b10);

    // A bit at or above the flag count names no flag, so it fails to lower.
    stack.push_value(js::value::from_u32(0b100));
    assert!(value::pop_flags(&mut stack, scope, 2).is_err());
    js::exception::clear(scope);

    // A flag count of 32 leaves no bit that names nothing.
    stack.push_value(js::value::from_u32(u32::MAX));
    assert_eq!(value::pop_flags(&mut stack, scope, 32).unwrap(), u32::MAX);

    // A set with bit 31 is the negative int32 a bitwise-or produces, both when
    // it lifts and when it lowers.
    let high = js::compile::evaluate(scope, "(1 << 31) | 1").unwrap();
    stack.push_value(high.get());
    assert_eq!(
        value::pop_flags(&mut stack, scope, 32).unwrap(),
        0x8000_0001
    );
    value::push_flags(&mut stack, 0x8000_0001);
    assert_eq!(stack.pop_value().to_int32(), (1 << 31) | 1);
}

fn list_round_trips(scope: &Scope<'_>) {
    // list<u8> -> a real Uint8Array.
    let u8_list = TypeShape::List(Box::new(TypeShape::U8));
    let value = WitValue::List(vec![WitValue::U8(1), WitValue::U8(2), WitValue::U8(255)]);
    round_trip(scope, &u8_list, &value);
    // Confirm it really is a Uint8Array.
    let mut stack = CallStack::new();
    lift(&mut stack, scope, &u8_list, &value);
    let obj = pop_object(&mut stack, scope);
    let view = js::ArrayBufferView::from_object(obj).unwrap();
    assert_eq!(view.view_kind(), js::typedarray::ViewKind::Uint8);

    // list<u64> -> a BigUint64Array, and list<s64> -> a BigInt64Array, with
    // values past the 32-bit and the safe-integer range.
    let u64_list = TypeShape::List(Box::new(TypeShape::U64));
    let value = WitValue::List(vec![
        WitValue::U64(0),
        WitValue::U64(u32::MAX as u64 + 1),
        WitValue::U64((1 << 53) + 1),
        WitValue::U64(u64::MAX),
    ]);
    round_trip(scope, &u64_list, &value);
    let mut stack = CallStack::new();
    lift(&mut stack, scope, &u64_list, &value);
    let obj = pop_object(&mut stack, scope);
    let view = js::ArrayBufferView::from_object(obj).unwrap();
    assert_eq!(view.view_kind(), js::typedarray::ViewKind::BigUint64);

    let s64_list = TypeShape::List(Box::new(TypeShape::S64));
    let value = WitValue::List(vec![
        WitValue::S64(-1),
        WitValue::S64(i64::MIN),
        WitValue::S64(-((1 << 53) + 1)),
        WitValue::S64(i64::MAX),
    ]);
    round_trip(scope, &s64_list, &value);
    let mut stack = CallStack::new();
    lift(&mut stack, scope, &s64_list, &value);
    let obj = pop_object(&mut stack, scope);
    let view = js::ArrayBufferView::from_object(obj).unwrap();
    assert_eq!(view.view_kind(), js::typedarray::ViewKind::BigInt64);

    // list<string>
    let str_list = TypeShape::List(Box::new(TypeShape::String));
    round_trip(
        scope,
        &str_list,
        &WitValue::List(vec![
            WitValue::String("a".into()),
            WitValue::String("bb".into()),
        ]),
    );

    // list<list<string>>: a nested generic list, so the lift cursor stack has to
    // nest too.
    let nested_str = TypeShape::List(Box::new(TypeShape::List(Box::new(TypeShape::String))));
    round_trip(
        scope,
        &nested_str,
        &WitValue::List(vec![
            WitValue::List(vec![WitValue::String("a".into())]),
            WitValue::List(vec![]),
            WitValue::List(vec![
                WitValue::String("b".into()),
                WitValue::String("c".into()),
            ]),
        ]),
    );

    // list<list<u32>>
    let nested = TypeShape::List(Box::new(TypeShape::List(Box::new(TypeShape::U32))));
    round_trip(
        scope,
        &nested,
        &WitValue::List(vec![
            WitValue::List(vec![WitValue::U32(1), WitValue::U32(2)]),
            WitValue::List(vec![]),
            WitValue::List(vec![WitValue::U32(3)]),
        ]),
    );

    // list<record>
    let rec = TypeShape::Record(vec![
        ("k".into(), TypeShape::U32),
        ("v".into(), TypeShape::String),
    ]);
    let rec_list = TypeShape::List(Box::new(rec));
    round_trip(
        scope,
        &rec_list,
        &WitValue::List(vec![
            WitValue::Record(vec![
                ("k".into(), WitValue::U32(1)),
                ("v".into(), WitValue::String("one".into())),
            ]),
            WitValue::Record(vec![
                ("k".into(), WitValue::U32(2)),
                ("v".into(), WitValue::String("two".into())),
            ]),
        ]),
    );
}

/// Lowering a `list<numeric>` whose typed array is the wrong element kind throws
/// a TypeError rather than silently reinterpreting the bytes.
fn numeric_list_kind_mismatch_throws(scope: &Scope<'_>) {
    // A Uint16Array where list<u8> is expected: reinterpreting its bytes would
    // ship the wrong canonical data, so it must be rejected.
    let mut stack = CallStack::new();
    let arr = js::Uint16Array::with_data(scope, &[1u16, 2, 3]).unwrap();
    stack.push_value(arr.as_value());
    assert!(
        value::pop_numeric_list(&mut stack, scope, &TypeShape::U8).is_err(),
        "a Uint16Array must not lower as list<u8>"
    );
    js::exception::clear(scope);

    // The matching kind still lowers, with an exact element count.
    let arr = js::Uint8Array::with_data(scope, &[10u8, 20, 30, 40]).unwrap();
    stack.push_value(arr.as_value());
    let (bytes, count) = pop_numeric_list_bytes(&mut stack, scope, &TypeShape::U8);
    assert_eq!(count, 4);
    assert_eq!(bytes, vec![10, 20, 30, 40]);

    // Uint8ClampedArray is byte-identical to Uint8Array, so it is accepted for
    // list<u8>.
    let arr = js::Uint8ClampedArray::with_data(scope, &[7u8, 8]).unwrap();
    stack.push_value(arr.as_value());
    let (bytes, count) = pop_numeric_list_bytes(&mut stack, scope, &TypeShape::U8);
    assert_eq!(count, 2);
    assert_eq!(bytes, vec![7, 8]);

    // The two 64-bit integer kinds are distinct: a BigInt64Array where list<u64>
    // is expected is rejected, as is a Float64Array of the same element width.
    let arr = js::BigInt64Array::with_data(scope, &[-1i64, 2]).unwrap();
    stack.push_value(arr.as_value());
    assert!(
        value::pop_numeric_list(&mut stack, scope, &TypeShape::U64).is_err(),
        "a BigInt64Array must not lower as list<u64>"
    );
    js::exception::clear(scope);
    let arr = js::Float64Array::with_data(scope, &[1.5f64]).unwrap();
    stack.push_value(arr.as_value());
    assert!(
        value::pop_numeric_list(&mut stack, scope, &TypeShape::U64).is_err(),
        "a Float64Array must not lower as list<u64>"
    );
    js::exception::clear(scope);
    let arr = js::BigUint64Array::with_data(scope, &[1u64]).unwrap();
    stack.push_value(arr.as_value());
    assert!(
        value::pop_numeric_list(&mut stack, scope, &TypeShape::S64).is_err(),
        "a BigUint64Array must not lower as list<s64>"
    );
    js::exception::clear(scope);

    // The matching kinds lower with their full 64-bit values.
    let arr = js::BigUint64Array::with_data(scope, &[u64::MAX, 1 << 40]).unwrap();
    stack.push_value(arr.as_value());
    let (bytes, count) = pop_numeric_list_bytes(&mut stack, scope, &TypeShape::U64);
    assert_eq!(count, 2);
    assert_eq!(
        numeric_list_from_bytes(&TypeShape::U64, &bytes, count),
        vec![WitValue::U64(u64::MAX), WitValue::U64(1 << 40)]
    );
    let arr = js::BigInt64Array::with_data(scope, &[i64::MIN, -3]).unwrap();
    stack.push_value(arr.as_value());
    let (bytes, count) = pop_numeric_list_bytes(&mut stack, scope, &TypeShape::S64);
    assert_eq!(count, 2);
    assert_eq!(
        numeric_list_from_bytes(&TypeShape::S64, &bytes, count),
        vec![WitValue::S64(i64::MIN), WitValue::S64(-3)]
    );
}

/// Lowering a value of the wrong JS type throws a TypeError and returns `Err`.
fn type_mismatch_throws(scope: &Scope<'_>) {
    // A string where a u32 is expected.
    let mut stack = CallStack::new();
    let s = js::JSString::from_str(scope, "not a number").unwrap();
    stack.push_value(s.as_value());
    assert!(value::pop_u32(&mut stack, scope).is_err());
    assert!(js::exception::is_pending(scope));
    js::exception::clear(scope);

    // A number larger than u32::MAX is rejected rather than silently wrapped:
    // the lowering path range-checks instead of truncating via `as u32`.
    stack.push_value(js::value::from_f64(5_000_000_000.0));
    assert!(value::pop_u32(&mut stack, scope).is_err());
    js::exception::clear(scope);

    // A negative number lowering to an unsigned width is rejected, not wrapped.
    stack.push_value(js::value::from_f64(-1.0));
    assert!(value::pop_u32(&mut stack, scope).is_err());
    js::exception::clear(scope);

    // A fractional number is rejected, not truncated (3.7 must not become 3).
    stack.push_value(js::value::from_f64(3.7));
    assert!(value::pop_u8(&mut stack, scope).is_err());
    js::exception::clear(scope);
    // NaN must not lower to 0.
    stack.push_value(js::value::from_f64(f64::NAN));
    assert!(value::pop_u8(&mut stack, scope).is_err());
    js::exception::clear(scope);

    // A 64-bit integer must be a BigInt: a number is rejected, as assigning one
    // to a `BigUint64Array` element is, whether or not it is integral.
    for n in [42.0, (1u64 << 40) as f64, 1e30, -1.0, 2.5] {
        stack.push_value(js::value::from_f64(n));
        assert!(
            value::pop_u64(&mut stack, scope).is_err(),
            "{n} lowered as a u64"
        );
        js::exception::clear(scope);
        stack.push_value(js::value::from_f64(n));
        assert!(
            value::pop_s64(&mut stack, scope).is_err(),
            "{n} lowered as an s64"
        );
        js::exception::clear(scope);
    }

    // A number where a record (object) is expected.
    stack.push_value(js::value::from_i32(5));
    let fields = vec![("x".to_string(), TypeShape::U32)];
    assert!(value::pop_record(&mut stack, scope, &field_names(&fields)).is_err());
    assert!(js::exception::is_pending(scope));
    js::exception::clear(scope);

    // BigInt inputs are range-checked rather than taken modulo 2^64.
    for (src, shape) in [
        ("-1n", TypeShape::U64),
        ("2n ** 64n", TypeShape::U64),
        ("2n ** 63n", TypeShape::S64),
        ("-(2n ** 63n) - 1n", TypeShape::S64),
    ] {
        let v = js::compile::evaluate(scope, src).unwrap();
        stack.push_value(v.get());
        let lowered = match shape {
            TypeShape::U64 => value::pop_u64(&mut stack, scope).map(|_| ()),
            _ => value::pop_s64(&mut stack, scope).map(|_| ()),
        };
        assert!(lowered.is_err(), "{src} must not lower as {shape:?}");
        js::exception::clear(scope);
    }
    // An in-range BigInt still lowers, at both ends of each range.
    for (src, expected) in [("2n ** 64n - 1n", u64::MAX), ("0n", 0)] {
        let v = js::compile::evaluate(scope, src).unwrap();
        stack.push_value(v.get());
        assert_eq!(value::pop_u64(&mut stack, scope).unwrap(), expected);
    }
    for (src, expected) in [("2n ** 63n - 1n", i64::MAX), ("-(2n ** 63n)", i64::MIN)] {
        let v = js::compile::evaluate(scope, src).unwrap();
        stack.push_value(v.get());
        assert_eq!(value::pop_s64(&mut stack, scope).unwrap(), expected);
    }

    // A lone surrogate is not a Unicode scalar value, so it is no `char`.
    let v = js::compile::evaluate(scope, "'\\uD800'").unwrap();
    stack.push_value(v.get());
    assert!(value::pop_char(&mut stack, scope).is_err());
    js::exception::clear(scope);
    // A well-formed surrogate pair is one supplementary-plane scalar value.
    let v = js::compile::evaluate(scope, "'\\u{1F600}'").unwrap();
    stack.push_value(v.get());
    assert_eq!(value::pop_char(&mut stack, scope).unwrap(), '\u{1F600}');

    // A non-array object where a list is expected throws rather than tripping an
    // assertion on `length`'s `undefined`.
    let v = js::compile::evaluate(scope, "({})").unwrap();
    stack.push_value(v.get());
    assert!(stack.lower_list_begin(scope).is_err());
    js::exception::clear(scope);
    stack.pop_value();

    // A boolean where a string is expected.
    stack.push_value(js::value::from_bool(true));
    assert!(value::pop_string(&mut stack, scope).is_err());
    js::exception::clear(scope);
}

// ===========================================================================
// ComponentError and resources
// ===========================================================================

use std::cell::RefCell;

thread_local! {
    /// Handles passed to drop callbacks during a test, in call order. The
    /// recording drop callback (built by `recording_dispose_fn`) appends the
    /// handle it reads off `this`.
    static DROPPED: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// Build a JS drop callback that records the handle it is invoked with.
///
/// This has the shape the dylib wraps around wit-dylib's `drop`: the callback
/// reads `this._componentizeJsHandle` and, if present, records it as dropped.
/// Reading the handle off `this` rather than a captured value means a stripped
/// handle makes the drop a no-op, which is the idempotency guarantee.
fn recording_dispose_fn<'s>(scope: &'s Scope<'_>) -> HandleValue<'s> {
    let f = js::Function::new_callback(
        scope,
        c"recordingDispose",
        0,
        |scope, args, _payload| {
            let this = js::Object::from_value(scope, args.this()).map_err(|_| {
                js::error::throw_type_error(scope, c"dispose this is not an object")
            })?;
            let handle = this.get_property(scope, resources::HANDLE_FIELD)?.get();
            // Like the runtime's drop callback, only a registered wrapper, or the
            // finalizer's clone of one, drops its handle.
            if handle.is_int32() && resources::unregister_resource(scope, &this)? {
                let h = handle.to_int32() as u32;
                DROPPED.with(|d| d.borrow_mut().push(h));
            }
            Ok(js::value::undefined())
        },
        (),
    )
    .unwrap();
    scope.root_value(f.as_value())
}

fn take_dropped() -> Vec<u32> {
    DROPPED.with(|d| std::mem::take(&mut *d.borrow_mut()))
}

thread_local! {
    /// Canonical handles passed to an exported resource type's `[resource-drop]`
    /// during a test, in call order.
    static DROPPED_EXPORTED: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// The `drop` operation for a test's `ResourceDesc::Exported`.
fn record_exported_drop(handle: u32) {
    DROPPED_EXPORTED.with(|d| d.borrow_mut().push(handle));
}

/// The exported-handle release callback `release_borrows` takes. The type index
/// selects the wit-dylib `drop` in the real interpreter and is unused here.
fn record_exported_borrow_drop(_type_idx: u32, handle: u32) {
    record_exported_drop(handle);
}

fn take_dropped_exported() -> Vec<u32> {
    DROPPED_EXPORTED.with(|d| std::mem::take(&mut *d.borrow_mut()))
}

/// An imported wrapper the guest abandons is dropped by the collector.
///
/// `wrap_imported` registers the wrapper with the `FinalizationRegistry` the
/// bootstrap creates. SpiderMonkey queues that registry's cleanup with the host
/// hook `core_runtime::finalization` installs, and the microtask checkpoint runs
/// it, which calls the wrapper's dispose.
fn abandoned_wrapper_is_dropped_by_gc(scope: &Scope<'_>) {
    take_dropped();

    {
        // An inner scope, so the wrapper's root slot is released when it ends.
        // The outer scope holds its slots for the whole test.
        let inner = scope.inner_scope();
        let dispose = recording_dispose_fn(&inner);
        let wrapper = resources::wrap_imported(&inner, 4242, 5, None, dispose).unwrap();
        assert_eq!(wrapper_handle(&inner, &wrapper), Some(4242));
    }

    // Two collections: the first clears the wrapper, the second lets the
    // registry observe the target as dead and queue its cleanup.
    force_gc(scope);
    force_gc(scope);
    core_runtime::event_loop::run_microtasks(scope);

    assert_eq!(
        take_dropped(),
        vec![4242],
        "an abandoned imported wrapper must have its handle dropped by the collector"
    );
}

/// An imported wrapper lent to the host stays reachable through the call's
/// stack, however many collections run during the call, and the end of the call
/// leaves it undisposed.
fn lent_wrapper_is_kept_for_the_call(scope: &Scope<'_>) {
    take_dropped();
    let mut stack = CallStack::new();

    {
        // An inner scope, so the wrapper's root slot is released when it ends.
        let inner = scope.inner_scope();
        let dispose = recording_dispose_fn(&inner);
        let wrapper = resources::wrap_imported(&inner, 4343, 6, None, dispose).unwrap();
        stack.push_object(&wrapper);
        let desc = ResourceDesc::Imported { type_idx: 6 };
        assert_eq!(value::pop_borrow(&mut stack, &inner, &desc).unwrap(), 4343);
    }

    force_gc(scope);
    force_gc(scope);
    core_runtime::event_loop::run_microtasks(scope);
    assert_eq!(
        take_dropped(),
        Vec::<u32>::new(),
        "a lent wrapper must not be collected during the call"
    );

    // `release_borrows` roots each object in the scope it is given, so it gets a
    // scope that ends with it.
    resources::release_borrows(
        &mut stack,
        &scope.inner_scope(),
        &record_exported_borrow_drop,
    )
    .unwrap();
    assert_eq!(
        take_dropped(),
        Vec::<u32>::new(),
        "ending the lend must not dispose"
    );

    // Once the call has ended, the wrapper is collectible again.
    drop(stack);
    force_gc(scope);
    force_gc(scope);
    core_runtime::event_loop::run_microtasks(scope);
    assert_eq!(take_dropped(), vec![4343]);
}

/// A dispose callback that always throws, to exercise `release_borrows`' drain
/// past a throwing drop.
fn throwing_dispose_fn<'s>(scope: &'s Scope<'_>) -> HandleValue<'s> {
    let f = js::Function::new_callback(
        scope,
        c"throwingDispose",
        0,
        |scope, _args, _payload| Err(js::error::throw_type_error(scope, c"dispose boom")),
        (),
    )
    .unwrap();
    scope.root_value(f.as_value())
}

/// Read a wrapper's `_componentizeJsHandle`, or `None` if absent/stripped.
fn wrapper_handle(scope: &Scope<'_>, obj: &Object<'_>) -> Option<u32> {
    let v = obj
        .get_property(scope, resources::HANDLE_FIELD)
        .unwrap()
        .get();
    v.is_int32().then(|| v.to_int32() as u32)
}

/// `ComponentError`: construct from JS, extract its payload, and confirm the
/// prototype chain (`instanceof Error`).
fn component_error_basics(scope: &Scope<'_>) {
    // Construct with an object payload.
    let v = js::compile::evaluate(scope, "new ComponentError({ code: 7 })").unwrap();
    let payload =
        component_model::component_error_payload(scope, v).expect("value is a ComponentError");
    let payload_obj = Object::from_value(scope, payload).unwrap();
    let code = payload_obj.get_property(scope, c"code").unwrap().get();
    assert!(code.is_int32() && code.to_int32() == 7);

    // A string payload becomes the message verbatim, while a non-string payload's
    // message mentions `error.payload` (globals.js semantics).
    assert_eq!(
        eval_string(scope, "new ComponentError('boom').message"),
        "boom"
    );
    assert_eq!(
        eval_string(scope, "new ComponentError(42).message"),
        "42 (see error.payload)"
    );
    // An `undefined` payload leaves the empty `message` of `Error.prototype`.
    assert_eq!(
        eval_string(
            scope,
            "(e => `${Object.hasOwn(e, 'message')} ${e}`)(new ComponentError(undefined))"
        ),
        "false ComponentError"
    );
    assert_eq!(
        eval_string(
            scope,
            "Object.prototype.toString.call(new ComponentError('x'))"
        ),
        "[object Error]"
    );

    // `message` is a non-enumerable own property, as a native `Error`'s is.
    assert_eq!(
        eval_string(scope, "Object.keys(new ComponentError('x')).join(',')"),
        ""
    );
    assert_eq!(
        eval_string(scope, "JSON.stringify(new ComponentError('x'))"),
        "{}"
    );

    // Prototype chain and brand.
    assert_eq!(
        eval_string(scope, "String(new ComponentError('x') instanceof Error)"),
        "true"
    );
    assert_eq!(
        eval_string(
            scope,
            "String(new ComponentError('x') instanceof ComponentError)"
        ),
        "true"
    );
    assert_eq!(eval_string(scope, "ComponentError.name"), "ComponentError");
    assert_eq!(
        eval_string(scope, "new ComponentError('x').name"),
        "ComponentError"
    );
    assert_eq!(
        eval_string(scope, "String(new ComponentError('x'))"),
        "ComponentError: x"
    );

    // A thrown ComponentError is captured with the stack it was created with.
    assert!(js::compile::evaluate(
        scope,
        "(function thrower() { throw new ComponentError('x'); })()"
    )
    .is_err());
    let captured = js::error::ExnThrown::capture(scope);
    assert!(
        captured
            .stack
            .as_deref()
            .is_some_and(|stack| stack.contains("thrower")),
        "{captured:?}"
    );

    // A plain object is not a ComponentError.
    let plain = js::compile::evaluate(scope, "({})").unwrap();
    assert!(component_model::component_error_payload(scope, plain).is_none());
    // A plain Error is not a ComponentError either.
    let err = js::compile::evaluate(scope, "new Error('nope')").unwrap();
    assert!(component_model::component_error_payload(scope, err).is_none());

    force_gc(scope);
}

/// `call_export` result-typed path, for a `result<_, u32>`: a thrown
/// `ComponentError` yields `Threw(payload)`, and a thrown plain `Error`, which a
/// `u32` cannot hold, traps.
fn component_error_export_paths(scope: &Scope<'_>) {
    use component_model::{call_export, resolve_export, Outcome, ResultShape};

    let result_with_u32_err = || ResultShape::Result {
        ok: false,
        err: Some(std::rc::Rc::new(TypeShape::U32)),
    };

    // A namespace object exposing two throwing functions.
    let ns_v = js::compile::evaluate(
        scope,
        r#"({
            throwsComponentError() { throw new ComponentError({ kind: 'wit-err' }); },
            throwsPlainError() { throw new Error('a real trap'); },
        })"#,
    )
    .unwrap();
    let ns = Object::from_value(scope, ns_v).unwrap();

    // result-typed export throwing a ComponentError -> Threw(payload).
    let resolved = resolve_export(scope, &ns, "throws-component-error").unwrap();
    let outcome = call_export(scope, resolved, &[], &result_with_u32_err()).unwrap();
    match outcome {
        Outcome::Threw(payload) => {
            let obj = Object::from_value(scope, payload).unwrap();
            let kind = obj.get_property(scope, c"kind").unwrap();
            assert_eq!(as_string(scope, kind), "wit-err");
        }
        Outcome::Returned(_) => panic!("expected Threw, got Returned"),
    }
    assert!(!js::exception::is_pending(scope));

    // result-typed export throwing a plain Error its `u32` err type cannot hold
    // -> trap (Err).
    let resolved = resolve_export(scope, &ns, "throws-plain-error").unwrap();
    let Err(err) = call_export(scope, resolved, &[], &result_with_u32_err()) else {
        panic!("a plain Error its err type cannot hold traps");
    };
    assert!(
        err.message
            .as_deref()
            .unwrap_or_default()
            .contains("a real trap"),
        "trap message should carry the thrown Error's message, got {:?}",
        err.message
    );
    assert!(!js::exception::is_pending(scope));

    force_gc(scope);
}

/// An imported owned resource: `wrap_imported` mints a wrapper with the hidden
/// fields and a drop callback. `unwrap_to_canon(owned=false)` reads the handle
/// back, `unwrap_to_canon(owned=true)` unregisters and strips it, and either
/// rejects a wrapper of another resource type.
fn imported_resource_round_trip(scope: &Scope<'_>) {
    let dispose = recording_dispose_fn(scope);
    let wrapper = resources::wrap_imported(scope, 42, 3, None, dispose).unwrap();

    // Hidden fields are present.
    assert_eq!(wrapper_handle(scope, &wrapper), Some(42));
    let ty = wrapper
        .get_property(scope, resources::TYPE_FIELD)
        .unwrap()
        .get();
    assert!(ty.is_int32() && ty.to_int32() == 3);

    // The hidden fields are non-writable: a guest cannot forge the handle by
    // assignment (`wrapper._componentizeJsHandle = 9999`), which would otherwise
    // confuse the host resource table.
    let _ = wrapper.set_property(scope, resources::HANDLE_FIELD, 9999u32);
    js::exception::clear(scope);
    assert_eq!(
        wrapper_handle(scope, &wrapper),
        Some(42),
        "a guest must not be able to forge the resource handle by assignment"
    );

    force_gc(scope);

    // Borrowed lower: handle read back, wrapper unchanged.
    let wrapper_v = scope.root_value(wrapper.as_value());
    let borrowed = resources::unwrap_to_canon(scope, wrapper_v, 3, false).unwrap();
    assert_eq!(borrowed, 42);

    // A wrapper of another resource type is rejected, and keeps its handle.
    assert!(resources::unwrap_to_canon(scope, wrapper_v, 4, false).is_err());
    js::exception::clear(scope);
    assert!(resources::unwrap_to_canon(scope, wrapper_v, 4, true).is_err());
    js::exception::clear(scope);
    assert_eq!(wrapper_handle(scope, &wrapper), Some(42));

    // Owned lower: handle read back, then stripped + unregistered.
    let owned = resources::unwrap_to_canon(scope, wrapper_v, 3, true).unwrap();
    assert_eq!(owned, 42);
    assert_eq!(wrapper_handle(scope, &wrapper), None);

    // A second owned lower now fails, because the handle is gone (idempotency: a
    // transferred handle cannot be lowered/dropped twice).
    assert!(resources::unwrap_to_canon(scope, wrapper_v, 3, true).is_err());
    js::exception::clear(scope);

    force_gc(scope);
}

/// The drop callback fires when invoked through the wrapper's dispose property,
/// and is a no-op once the handle has been stripped (idempotency).
fn imported_resource_dispose(scope: &Scope<'_>) {
    let _ = take_dropped();
    let dispose = recording_dispose_fn(scope);
    let wrapper = resources::wrap_imported(scope, 99, 1, None, dispose).unwrap();

    // Invoke the drop callback the way JS would: `wrapper[disposeField]()`.
    let dispose_v = wrapper
        .get_property(scope, resources::DISPOSE_FIELD)
        .unwrap();
    js::Function::call(scope, wrapper, dispose_v, js::function::EmptyArgs).unwrap();
    assert_eq!(take_dropped(), vec![99]);

    // After an owned lower strips the handle, the dispose callback no-ops.
    let wrapper_v = scope.root_value(wrapper.as_value());
    resources::unwrap_to_canon(scope, wrapper_v, 1, true).unwrap();
    let dispose_v = wrapper
        .get_property(scope, resources::DISPOSE_FIELD)
        .unwrap();
    js::Function::call(scope, wrapper, dispose_v, js::function::EmptyArgs).unwrap();
    assert!(
        take_dropped().is_empty(),
        "dropping a stripped wrapper must no-op"
    );

    force_gc(scope);
}

/// The exported-resource slab: insert -> rep, lookup round-trips the same
/// object, remove empties the slot.
fn exported_slab(scope: &Scope<'_>) {
    let a = js::compile::evaluate(scope, "({ tag: 'a' })").unwrap();
    let a = Object::from_value(scope, a).unwrap();
    let b = js::compile::evaluate(scope, "({ tag: 'b' })").unwrap();
    let b = Object::from_value(scope, b).unwrap();

    let (rep_a, rep_b) =
        resources::with_exported_resources(|slab| (slab.insert(&a), slab.insert(&b)));
    assert_ne!(rep_a, rep_b);

    force_gc(scope);

    // Lookup returns the same JS object identity.
    resources::with_exported_resources(|slab| {
        let looked = slab.lookup(rep_a).unwrap().get(scope);
        let tag = looked.get_property(scope, c"tag").unwrap();
        assert_eq!(as_string(scope, tag), "a");
    });

    // Remove empties the slot, and the freed rep is reused by the next insert.
    let removed = resources::with_exported_resources(|slab| slab.remove(scope, rep_a));
    assert!(removed.is_some());
    resources::with_exported_resources(|slab| assert!(slab.lookup(rep_a).is_none()));
    let c = js::compile::evaluate(scope, "({ tag: 'c' })").unwrap();
    let c = Object::from_value(scope, c).unwrap();
    let rep_c = resources::with_exported_resources(|slab| slab.insert(&c));
    assert_eq!(rep_c, rep_a, "a freed rep is reused");

    // Clean up so later tests start from an empty-ish slab.
    resources::with_exported_resources(|slab| {
        slab.remove(scope, rep_b);
        slab.remove(scope, rep_c);
    });

    force_gc(scope);
}

/// Each `exported_resource_to_canon` of the same object inserts it into the
/// slab again and mints a new handle, and leaves no handle on the object.
fn exported_resource_to_canon_mints_per_lowering(scope: &Scope<'_>) {
    use std::cell::Cell;

    let next_handle = Cell::new(1000u32);
    let new = |_rep: u32| {
        let h = next_handle.get();
        next_handle.set(h + 1);
        h
    };
    let rep = |handle: u32| handle - 1000; // inverse of `new`'s rep->handle map
    let desc = ResourceDesc::Exported {
        type_idx: 5,
        new: &new,
        rep: &rep,
        drop: &record_exported_drop,
    };

    let obj_v = js::compile::evaluate(scope, "({ kind: 'exported' })").unwrap();
    let h1 = resources::exported_resource_to_canon(scope, &desc, obj_v).unwrap();
    assert_eq!(h1, 1000);
    let h2 = resources::exported_resource_to_canon(scope, &desc, obj_v).unwrap();
    assert_eq!(h2, 1001, "a second lowering mints a new handle");
    let obj = Object::from_value(scope, obj_v).unwrap();
    assert_eq!(wrapper_handle(scope, &obj), None);

    // Both lowerings' slab entries hold the object, so clean them up.
    resources::with_exported_resources(|slab| {
        assert!(slab.remove(scope, rep(h1)).is_some());
        assert!(slab.remove(scope, rep(h2)).is_some());
    });

    force_gc(scope);
}

/// An exported resource's full lift/lower cycle through `value::push_own` /
/// `value::pop_own`: lowering inserts the object into the slab and mints a
/// handle, and the matching `push_own` resolves the handle back to its rep,
/// removes it from the slab so ownership returns to JS, and pushes the same
/// object.
fn exported_resource_lift_lower(scope: &Scope<'_>) {
    use std::cell::Cell;

    // `new`: rep -> handle (rep + 7000). `rep`: handle -> rep (handle - 7000).
    let new_calls = Cell::new(0u32);
    let new = |rep: u32| {
        new_calls.set(new_calls.get() + 1);
        rep + 7000
    };
    let rep = |handle: u32| handle - 7000;
    let desc = ResourceDesc::Exported {
        type_idx: 9,
        new: &new,
        rep: &rep,
        drop: &record_exported_drop,
    };

    let mut stack = CallStack::new();

    // Lower a fresh JS object: it enters the slab and gets a handle.
    let obj = js::compile::evaluate(scope, "({ marker: 'exp-rt' })").unwrap();
    let obj = Object::from_value(scope, obj).unwrap();
    stack.push_object(&obj);
    let handle = value::pop_own(&mut stack, scope, &desc).unwrap();
    assert_eq!(new_calls.get(), 1);

    force_gc(scope);

    // Lift the handle back as an owned resource: the slab entry is removed and
    // the original object surfaces again.
    let dispose = recording_dispose_fn(scope);
    value::push_own(&mut stack, scope, &desc, handle, dispose).unwrap();
    let back = Object::from_value(scope, stack.pop_value()).unwrap();
    let marker = back.get_property(scope, c"marker").unwrap();
    assert_eq!(as_string(scope, marker), "exp-rt");

    // The slab slot is now free (ownership is back in JS), and the handle the
    // host's lower added to the guest's table is released.
    resources::with_exported_resources(|slab| assert!(slab.lookup(rep(handle)).is_none()));
    assert_eq!(take_dropped_exported(), vec![handle]);

    drop(stack);
    force_gc(scope);
}

/// The out->in->out round trip: an exported object that crosses out, comes back
/// owned, and is re-exported re-enters the slab with a fresh rep, and a host
/// `push_borrow` on that rep surfaces the object again.
fn exported_resource_reexport_round_trip(scope: &Scope<'_>) {
    use std::cell::Cell;

    // Consistent inverses so a reused rep still maps back to the right slot:
    // `new`: rep -> handle (rep + 8000); `rep`: handle -> rep (handle - 8000).
    let new_calls = Cell::new(0u32);
    let new = |rep: u32| {
        new_calls.set(new_calls.get() + 1);
        rep + 8000
    };
    let rep = |handle: u32| handle - 8000;
    let desc = ResourceDesc::Exported {
        type_idx: 11,
        new: &new,
        rep: &rep,
        drop: &record_exported_drop,
    };

    let mut stack = CallStack::new();

    // Out (1): lower a fresh JS object, which enters the slab and gets a handle.
    let obj = js::compile::evaluate(scope, "({ marker: 'reexport' })").unwrap();
    let obj = Object::from_value(scope, obj).unwrap();
    stack.push_object(&obj);
    let handle1 = value::pop_own(&mut stack, scope, &desc).unwrap();
    assert_eq!(new_calls.get(), 1, "first crossing mints a handle");
    assert_eq!(
        wrapper_handle(scope, &obj),
        None,
        "lowering an own transfers the handle out of the guest's table"
    );

    force_gc(scope);

    // In: lift the handle back owned. The slab entry is removed and the handle
    // is released back to the guest's table.
    let dispose = recording_dispose_fn(scope);
    value::push_own(&mut stack, scope, &desc, handle1, dispose).unwrap();
    let back = Object::from_value(scope, stack.pop_value()).unwrap();
    assert_eq!(
        take_dropped_exported(),
        vec![handle1],
        "lifting an own back in must release the handle the host's lower added"
    );
    resources::with_exported_resources(|slab| assert!(slab.lookup(rep(handle1)).is_none()));

    force_gc(scope);

    // Out (2): re-export the same object. It re-enters the slab and `new` runs
    // again. The free list reuses the freed slot, so the fresh rep, and thus the
    // handle, may equal the first one.
    let back_v = scope.root_value(back.as_value());
    let handle2 = resources::exported_resource_to_canon(scope, &desc, back_v).unwrap();
    assert_eq!(new_calls.get(), 2, "re-export must mint a fresh handle");

    // The fresh rep is genuinely in the slab: a host `push_borrow` on it (the
    // rep, which push_borrow looks up directly) surfaces the same object.
    let dispose = recording_dispose_fn(scope);
    value::push_borrow(&mut stack, scope, &desc, rep(handle2), dispose).unwrap();
    let borrowed = Object::from_value(scope, stack.pop_value()).unwrap();
    let marker = borrowed.get_property(scope, c"marker").unwrap();
    assert_eq!(
        as_string(scope, marker),
        "reexport",
        "re-exported rep resolves back to the original object"
    );

    // Clean up the slab so later tests start fresh.
    resources::with_exported_resources(|slab| {
        slab.remove(scope, rep(handle2));
    });

    drop(stack);
    force_gc(scope);
}

thread_local! {
    /// Count of exported-resource drop-hook invocations during a test. The
    /// recording hook (built by `recording_exported_dtor`) increments this.
    static EXPORTED_HOOK_FIRES: RefCell<u32> = const { RefCell::new(0) };
}

/// Build a JS function that records each time it is invoked, for installing as
/// an exported object's `[Symbol.dispose]`. It ignores `this`, standing in for
/// the guest's module-level drop hook.
fn recording_exported_dtor<'s>(scope: &'s Scope<'_>) -> HandleValue<'s> {
    let f = js::Function::new_callback(
        scope,
        c"recordingExportedDtor",
        0,
        |_scope, _args, _payload| {
            EXPORTED_HOOK_FIRES.with(|c| *c.borrow_mut() += 1);
            Ok(js::value::undefined())
        },
        (),
    )
    .unwrap();
    scope.root_value(f.as_value())
}

fn take_exported_hook_fires() -> u32 {
    EXPORTED_HOOK_FIRES.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// `drop_exported` removes the slab entry and runs the object's
/// `[Symbol.dispose]` exactly once. A second `drop_exported` on
/// the now-free rep is a tolerant no-op with no second fire, so the explicit-drop
/// side of the idempotency guarantee.
fn exported_resource_drop_runs_hook(scope: &Scope<'_>) {
    let _ = take_exported_hook_fires();

    // `new`: rep -> handle (rep + 5000); `rep`: handle -> rep (handle - 5000).
    let new = |rep: u32| rep + 5000;
    let rep = |handle: u32| handle - 5000;
    let desc = ResourceDesc::Exported {
        type_idx: 13,
        new: &new,
        rep: &rep,
        drop: &record_exported_drop,
    };

    // Mint a slab-resident exported object the way the lower path does: stamp a
    // recording drop hook, then lower it to a handle (slab insert + `new`).
    let obj = js::compile::evaluate(scope, "({ marker: 'drop-hook' })").unwrap();
    let obj = Object::from_value(scope, obj).unwrap();
    let hook = recording_exported_dtor(scope);
    resources::define_dispose(scope, &obj, hook).unwrap();

    let mut stack = CallStack::new();
    stack.push_object(&obj);
    let handle = value::pop_own(&mut stack, scope, &desc).unwrap();
    let resource_rep = rep(handle);

    // The object is in the slab with its recorded type index.
    resources::with_exported_resources(|slab| assert!(slab.lookup(resource_rep).is_some()));

    force_gc(scope);

    // Drop it the way `resource_dtor` does: the slab entry goes and the hook
    // fires once.
    resources::drop_exported(scope, resource_rep, 13).unwrap();
    assert_eq!(take_exported_hook_fires(), 1, "the drop hook fires once");
    resources::with_exported_resources(|slab| {
        assert!(
            slab.lookup(resource_rep).is_none(),
            "the slab entry is removed on drop"
        )
    });

    // A second drop on the freed rep is a no-op: no slab entry, no hook fire.
    resources::drop_exported(scope, resource_rep, 13).unwrap();
    assert_eq!(
        take_exported_hook_fires(),
        0,
        "a double drop neither fires the hook nor traps"
    );

    drop(stack);
    force_gc(scope);
}

/// Collecting an exported object does not run its `[Symbol.dispose]` hook. Only
/// `drop_exported` runs it.
fn exported_resource_finalizer_ignores_dtor_hook(scope: &Scope<'_>) {
    let _ = take_exported_hook_fires();

    let new = |rep: u32| rep + 6000;
    let rep = |handle: u32| handle - 6000;
    let desc = ResourceDesc::Exported {
        type_idx: 17,
        new: &new,
        rep: &rep,
        drop: &record_exported_drop,
    };

    // Mint and lower an exported object carrying a recording hook, then remove
    // it from the slab without going through `drop_exported`, which leaves it
    // unreachable.
    let handle = {
        let obj = js::compile::evaluate(scope, "({ marker: 'finalizer' })").unwrap();
        let obj = Object::from_value(scope, obj).unwrap();
        let hook = recording_exported_dtor(scope);
        resources::define_dispose(scope, &obj, hook).unwrap();
        let mut stack = CallStack::new();
        stack.push_object(&obj);
        // `pop_own` lowers the object to a handle and consumes it off the stack
        // (slab insert + `new`), leaving the stack empty.
        let handle = value::pop_own(&mut stack, scope, &desc).unwrap();
        drop(stack);
        handle
    };
    // Drop the slab's strong reference so the object is collectible.
    resources::with_exported_resources(|slab| {
        slab.remove(scope, rep(handle));
    });

    // Finalizers run on a job, so drain the jobs after each collection.
    js::jobs::run_jobs(scope);
    force_gc(scope);
    js::jobs::run_jobs(scope);

    assert_eq!(
        take_exported_hook_fires(),
        0,
        "a collected exported object must not fire its drop hook via the finalizer"
    );
}

/// Borrows recorded during a call are released, in reverse push order, at call
/// end, each dropping exactly once.
fn borrow_release_ordering(scope: &Scope<'_>) {
    let _ = take_dropped();
    let mut stack = CallStack::new();

    // Lift three imported borrows onto the stack (handles 10, 20, 30). Each is
    // recorded as a per-call borrow with the recording drop callback.
    for handle in [10u32, 20, 30] {
        let dispose = recording_dispose_fn(scope);
        let desc = ResourceDesc::Imported { type_idx: 0 };
        value::push_borrow(&mut stack, scope, &desc, handle, dispose).unwrap();
        // Pop the wrapper back off the value stack, as the export call would
        // have consumed it as an argument. The borrow record stays on the
        // stack.
        stack.pop_value();
    }

    force_gc(scope);

    // Release at call end: borrows drop in reverse push order (LIFO), like the
    // one-at-a-time `while let Some(..) = borrows.pop()` drain.
    resources::release_borrows(&mut stack, scope, &record_exported_borrow_drop).unwrap();
    assert_eq!(take_dropped(), vec![30, 20, 10]);

    // A second release is a no-op (the list is empty).
    resources::release_borrows(&mut stack, scope, &record_exported_borrow_drop).unwrap();
    assert!(take_dropped().is_empty());

    drop(stack);
    force_gc(scope);
}

/// A guest-owned object borrowed to the host is unpinned at call end.
///
/// Lowering it as a `borrow` mints a canonical handle and pins the object in
/// the slab, and `release_borrows` drops the handle and unpins it. Each borrow
/// of the same object mints its own handle.
fn exported_borrow_release(scope: &Scope<'_>) {
    use std::cell::Cell;

    let _ = take_dropped_exported();

    let new_calls = Cell::new(0u32);
    let new = |rep: u32| {
        new_calls.set(new_calls.get() + 1);
        rep + 9000
    };
    let rep = |handle: u32| handle - 9000;
    let desc = ResourceDesc::Exported {
        type_idx: 19,
        new: &new,
        rep: &rep,
        drop: &record_exported_drop,
    };

    let mut stack = CallStack::new();

    let obj = js::compile::evaluate(scope, "({ marker: 'exp-borrow' })").unwrap();
    let obj = Object::from_value(scope, obj).unwrap();

    // Lower it as a borrow argument, as calling an import with `borrow<r>` does.
    stack.push_object(&obj);
    let handle = value::pop_borrow(&mut stack, scope, &desc).unwrap();
    assert_eq!(new_calls.get(), 1);
    resources::with_exported_resources(|slab| assert!(slab.lookup(rep(handle)).is_some()));

    force_gc(scope);

    resources::release_borrows(&mut stack, scope, &record_exported_borrow_drop).unwrap();
    assert_eq!(take_dropped_exported(), vec![handle]);
    resources::with_exported_resources(|slab| {
        assert!(
            slab.lookup(rep(handle)).is_none(),
            "the borrow must not pin the object in the slab"
        )
    });

    // A second borrow of the same object mints a fresh handle rather than
    // reusing the released one.
    stack.push_object(&obj);
    let handle2 = value::pop_borrow(&mut stack, scope, &desc).unwrap();
    assert_eq!(new_calls.get(), 2);
    resources::release_borrows(&mut stack, scope, &record_exported_borrow_drop).unwrap();
    assert_eq!(take_dropped_exported(), vec![handle2]);

    // Lent to two calls at once, as two concurrent async imports do, the object
    // gets a handle per call, and the end of the first call leaves the second
    // call's handle and slab entry in place.
    let mut other = CallStack::new();
    stack.push_object(&obj);
    let first = value::pop_borrow(&mut stack, scope, &desc).unwrap();
    other.push_object(&obj);
    let second = value::pop_borrow(&mut other, scope, &desc).unwrap();
    assert_ne!(first, second);
    resources::release_borrows(&mut stack, scope, &record_exported_borrow_drop).unwrap();
    assert_eq!(take_dropped_exported(), vec![first]);
    resources::with_exported_resources(|slab| assert!(slab.lookup(rep(second)).is_some()));
    force_gc(scope);
    resources::release_borrows(&mut other, scope, &record_exported_borrow_drop).unwrap();
    assert_eq!(take_dropped_exported(), vec![second]);
    resources::with_exported_resources(|slab| assert!(slab.lookup(rep(second)).is_none()));

    drop(other);
    drop(stack);
    force_gc(scope);
}

/// Disposing an imported wrapper strips its handle, so a second dispose is a
/// no-op and a stashed wrapper cannot pass the released handle back to the host.
///
/// The drop callback the dylib installs reads the handle off `this` and strips
/// it after dropping. This exercises the same contract with the test's recording
/// callback plus `release_borrows`' own strip.
fn imported_dispose_strips_handle(scope: &Scope<'_>) {
    let _ = take_dropped();
    let mut stack = CallStack::new();

    let desc = ResourceDesc::Imported { type_idx: 0 };
    value::push_borrow(&mut stack, scope, &desc, 55, recording_dispose_fn(scope)).unwrap();
    let wrapper = Object::from_value(scope, stack.pop_value()).unwrap();
    assert_eq!(wrapper_handle(scope, &wrapper), Some(55));

    resources::release_borrows(&mut stack, scope, &record_exported_borrow_drop).unwrap();
    assert_eq!(take_dropped(), vec![55]);
    assert_eq!(
        wrapper_handle(scope, &wrapper),
        None,
        "a released borrow must not keep the host's handle"
    );

    // A method call on the stashed wrapper now fails instead of passing the
    // released handle to the host.
    let wrapper_v = scope.root_value(wrapper.as_value());
    assert!(resources::unwrap_to_canon(scope, wrapper_v, 0, false).is_err());
    js::exception::clear(scope);

    drop(stack);
    force_gc(scope);
}

/// A throwing borrow dispose must not abort the drain: every other borrow still
/// drops (its host handle is released, finalizer disarmed), and the failure
/// surfaces as `Err` with a pending exception.
fn borrow_release_drains_through_throw(scope: &Scope<'_>) {
    let _ = take_dropped();
    let mut stack = CallStack::new();

    // Push borrows 10, 20, 30. Drain order is LIFO (30, 20, 10), so make the
    // middle-drained borrow (handle 20) throw on dispose.
    let desc = ResourceDesc::Imported { type_idx: 0 };
    value::push_borrow(&mut stack, scope, &desc, 10, recording_dispose_fn(scope)).unwrap();
    stack.pop_value();
    value::push_borrow(&mut stack, scope, &desc, 20, throwing_dispose_fn(scope)).unwrap();
    stack.pop_value();
    value::push_borrow(&mut stack, scope, &desc, 30, recording_dispose_fn(scope)).unwrap();
    stack.pop_value();

    let result = resources::release_borrows(&mut stack, scope, &record_exported_borrow_drop);
    assert!(
        result.is_err(),
        "a throwing borrow dispose must surface as Err"
    );
    assert!(
        js::exception::is_pending(scope),
        "the captured exception must be re-raised"
    );
    js::exception::clear(scope);
    // 30 drained first, then 20 threw, then 10 still drained: the throw did not
    // abort the loop.
    assert_eq!(
        take_dropped(),
        vec![30, 10],
        "every non-throwing borrow must still drop"
    );

    drop(stack);
    force_gc(scope);
}

/// Evaluate `code` and return its string value.
fn eval_string(scope: &Scope<'_>, code: &str) -> String {
    let v = js::compile::evaluate(scope, code).unwrap();
    as_string(scope, v)
}

/// `validate::check` accepts what lowering accepts, and names the path to the
/// first part that does not lower and why.
fn validation_names_the_failing_part(scope: &Scope<'_>) {
    use component_model::validate::check;

    let check_src = |src: &str, shape: &TypeShape| {
        let value = js::compile::evaluate(scope, src).unwrap();
        check(scope, value, shape).map_err(|m| (m.path, m.reason))
    };
    let string = || TypeShape::String;
    let items = TypeShape::Record(vec![(
        "items".to_string(),
        TypeShape::List(Box::new(TypeShape::Record(vec![(
            "name".to_string(),
            string(),
        )]))),
    )]);

    assert_eq!(check_src("({ items: [{ name: 'a' }] })", &items), Ok(()));
    let (path, reason) =
        check_src("({ items: [{ name: 'a' }, { name: 7 }] })", &items).unwrap_err();
    assert_eq!(path, ".items[1].name");
    assert!(
        reason.contains("expected a string") && reason.ends_with("got 7"),
        "{reason}"
    );

    let (path, reason) = check_src("300", &TypeShape::U8).unwrap_err();
    assert_eq!(path, "");
    assert!(
        reason.contains("expected a u8") && reason.ends_with("got 300"),
        "{reason}"
    );

    let (_, reason) = check_src("5", &TypeShape::U64).unwrap_err();
    assert!(
        reason.contains("BigInt") && reason.ends_with("got 5"),
        "{reason}"
    );
    assert_eq!(check_src("5n", &TypeShape::U64), Ok(()));

    let bytes = TypeShape::List(Box::new(TypeShape::U8));
    assert_eq!(check_src("new Uint8Array(2)", &bytes), Ok(()));
    assert_eq!(check_src("[1, 2]", &bytes), Ok(()));
    let (_, reason) = check_src("new Int8Array(2)", &bytes).unwrap_err();
    assert_eq!(reason, "expected Uint8Array, got Int8Array");
    let (path, _) = check_src("[1, 256]", &bytes).unwrap_err();
    assert_eq!(path, "[1]");

    let shape = TypeShape::Variant(vec![
        ("none".to_string(), None),
        ("some".to_string(), Some(TypeShape::U32)),
    ]);
    assert_eq!(check_src("({ tag: 'none' })", &shape), Ok(()));
    let (path, _) = check_src("({ tag: 'some', val: -1 })", &shape).unwrap_err();
    assert_eq!(path, ".val");
    let (_, reason) = check_src("({ tag: 'other' })", &shape).unwrap_err();
    assert!(
        reason.contains("\"other\" is none of the cases"),
        "{reason}"
    );
    let (_, reason) = check_src("({ tag: 1 })", &shape).unwrap_err();
    assert!(reason.contains("expected a string `tag`"), "{reason}");

    let option = TypeShape::Option(Box::new(TypeShape::Char));
    assert_eq!(check_src("undefined", &option), Ok(()));
    assert_eq!(check_src("null", &option), Ok(()));
    assert!(check_src("'ab'", &option).is_err());

    let result = TypeShape::Result(Some(Box::new(string())), None);
    assert_eq!(check_src("({ tag: 'ok', val: 'x' })", &result), Ok(()));
    assert_eq!(check_src("({ tag: 'err' })", &result), Ok(()));
    let (path, _) = check_src("({ tag: 'ok', val: 1 })", &result).unwrap_err();
    assert_eq!(path, ".val");

    let tuple = TypeShape::Tuple(vec![TypeShape::Bool, string()]);
    let (path, _) = check_src("[true, false]", &tuple).unwrap_err();
    assert_eq!(path, "[1]");

    // A getter that throws is reported, and its exception cleared.
    let record = TypeShape::Record(vec![("x".to_string(), TypeShape::U32)]);
    let (path, reason) =
        check_src("({ get x() { throw new Error('nope'); } })", &record).unwrap_err();
    assert_eq!(path, "");
    assert!(reason.contains("nope"), "{reason}");
    assert!(!js::exception::is_pending(scope));
}

/// `validate::snapshot` reads each part of a value once and returns a copy that
/// later changes to the value and its getters do not reach.
fn snapshot_reads_each_part_once(scope: &Scope<'_>) {
    use component_model::validate::snapshot;

    let shape = TypeShape::Record(vec![
        ("name".to_string(), TypeShape::String),
        (
            "counts".to_string(),
            TypeShape::List(Box::new(TypeShape::U32)),
        ),
        (
            "kind".to_string(),
            TypeShape::Variant(vec![
                ("none".to_string(), None),
                ("some".to_string(), Some(TypeShape::U8)),
            ]),
        ),
    ]);
    let value = js::compile::evaluate(
        scope,
        "globalThis.reads = 0;
         globalThis.original = {
           get name() { reads++; return reads === 1 ? 'first' : 7; },
           counts: new Uint32Array([1, 2]),
           kind: { tag: 'some', val: 3 },
         };
         original",
    )
    .unwrap();
    let copy = snapshot(scope, value, &shape).expect("the value lowers");
    let global = scope.global();
    global
        .set_property(scope, c"copy", copy)
        .expect("storing the copy");
    let described = eval_string(
        scope,
        "original.counts[0] = 9; original.kind.val = 'x';
         JSON.stringify([reads, copy.name, Array.from(copy.counts),
           copy.counts instanceof Uint32Array, copy.kind,
           Object.getOwnPropertyDescriptor(copy, 'name').get === undefined])",
    );
    assert_eq!(
        described,
        r#"[1,"first",[1,2],true,{"tag":"some","val":3},true]"#
    );

    let bad = js::compile::evaluate(
        scope,
        "({ name: 'a', counts: [1], kind: { tag: 'some', val: 300 } })",
    )
    .unwrap();
    let mismatch = snapshot(scope, bad, &shape).map(|_| ()).unwrap_err();
    assert_eq!(mismatch.path, ".kind.val");
    assert!(
        mismatch.reason.contains("expected a u8"),
        "{}",
        mismatch.reason
    );
}
