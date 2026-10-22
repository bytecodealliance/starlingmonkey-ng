// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for the `js` API.

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;

use js::builtins::IsPrimitive;
use js::builtins::{Boolean, Double, Int32, Null, StringPrimitive, SymbolPrimitive, Undefined};
use js::conversion::{ConversionBehavior, FromJSVal, ToJSVal};
use js::error::{CapturedError, ExnThrown};
use js::value;
use js::Array;
use js::Date;
use js::Map;
use js::Object;
use js::Promise;
use js::Set;

// --- Pure tests (no engine needed) ---

#[test]
fn test_value_constructors() {
    let v = value::undefined();
    assert!(v.is_undefined());

    let v = value::null();
    assert!(v.is_null());

    let v = value::from_bool(true);
    assert!(v.is_boolean());
    assert!(v.to_boolean());

    let v = value::from_i32(42);
    assert!(v.is_int32());
    assert_eq!(v.to_int32(), 42);

    let v = value::from_u32(100);
    assert!(v.is_int32() || v.is_double());

    let v = value::from_f64(2.5);
    assert!(v.is_double());
    assert!((v.to_double() - 2.5).abs() < f64::EPSILON);
}

#[test]
fn test_jserror_check() {
    assert!(ExnThrown::check(true).is_ok());
    assert!(ExnThrown::check(false).is_err());
    match ExnThrown::check(false) {
        Err(ExnThrown) => {}
        other => panic!("expected ExnThrown, got {other:?}"),
    }
}

#[test]
fn test_jserror_display() {
    let e = ExnThrown;
    assert_eq!(format!("{e}"), "JavaScript exception pending");

    let e = CapturedError {
        message: Some("bad thing".into()),
        filename: Some("test.js".into()),
        lineno: 10,
        column: 5,
        stack: None,
    };
    assert_eq!(format!("{e}"), "bad thing at test.js:10:5");
}

// --- Engine-dependent tests (single test function) ---

#[test]
fn test_js_api_with_runtime() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    // --- String operations ---
    let s = js::JSString::from_str(&scope, "hello world").unwrap();
    let result = s.to_utf8(&scope).unwrap();
    assert_eq!(result, "hello world");

    // --- Object creation ---
    let obj = Object::new(&scope, None).unwrap();
    assert!(!obj.as_raw().is_null());

    // --- Array creation ---
    let arr = Array::new(&scope, 5).unwrap();
    let len = arr.length(&scope).unwrap();
    assert_eq!(len, 5);

    // --- Evaluate and extract ---
    let rval = js::compile::evaluate(&scope, "2 + 3").unwrap();
    let result = i32::from_jsval(&scope, rval, ConversionBehavior::Default).unwrap();
    assert_eq!(result, 5);

    // --- GC operations ---
    js::gc::maybe_gc(&scope);
    js::gc::prepare_for_full_gc(&scope);

    // --- ToJSVal ---
    assert!(Option::<i32>::None.to_jsval(&scope).unwrap().is_null());
    assert_eq!(Some(42i32).to_jsval(&scope).unwrap().to_int32(), 42);
    assert_eq!(7i8.to_jsval(&scope).unwrap().to_int32(), 7);
    assert_eq!(300u16.to_jsval(&scope).unwrap().to_int32(), 300);
    assert!((1.5f32.to_jsval(&scope).unwrap().to_double() - 1.5).abs() < 0.001);

    // --- FromJSVal for bool ---
    {
        let v = scope.root_value(value::from_bool(false));
        assert!(!bool::from_jsval(&scope, v, ()).unwrap());
    }

    // --- FromJSVal for i32 ---
    {
        let v = scope.root_value(value::from_i32(-7));
        assert_eq!(
            i32::from_jsval(&scope, v, ConversionBehavior::Default).unwrap(),
            -7
        );
    }

    // --- FromJSVal for f64 ---
    {
        let v = scope.root_value(value::from_f64(1.5));
        assert!((f64::from_jsval(&scope, v, ()).unwrap() - 1.5).abs() < f64::EPSILON);
    }

    // --- FromJSVal for u32 ---
    {
        let v = scope.root_value(value::from_u32(100));
        assert_eq!(
            u32::from_jsval(&scope, v, ConversionBehavior::Default).unwrap(),
            100
        );
    }

    // --- Finite<f32>/Finite<f64> (restricted WebIDL float/double) ---
    {
        use js::Finite;

        let v = scope.root_value(value::from_f64(1.5));
        assert_eq!(Finite::<f64>::from_jsval(&scope, v, ()).unwrap().get(), 1.5);
        assert_eq!(*Finite::<f32>::from_jsval(&scope, v, ()).unwrap(), 1.5f32);

        // NaN and infinities are rejected with a TypeError.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let v = scope.root_value(value::from_f64(bad));
            assert!(Finite::<f64>::from_jsval(&scope, v, ()).is_err());
            js::exception::clear(&scope);
            assert!(Finite::<f32>::from_jsval(&scope, v, ()).is_err());
            js::exception::clear(&scope);
        }

        // A finite double that overflows single precision is rejected too.
        let v = scope.root_value(value::from_f64(1e300));
        assert!(Finite::<f32>::from_jsval(&scope, v, ()).is_err());
        js::exception::clear(&scope);

        // Finite values round-trip through ToJSVal.
        let f = Finite::<f64>::new(2.5).unwrap();
        assert_eq!(f.to_jsval(&scope).unwrap().to_double(), 2.5);
        assert!(Finite::<f64>::new(f64::NAN).is_none());
    }

    // --- Vec / Record with scope-tied element types ---
    {
        use js::conversion::Record;
        use js::prelude::HandleValue;

        // `for_of` hands the conversion a single reused slot; the `Vec` impl
        // must re-root each element into its own slot. Identical elements
        // here would mean every handle still aliases that one slot.
        let rval = js::compile::evaluate(&scope, "[1, 2, 3]").unwrap();
        let vals = Vec::<HandleValue>::from_jsval(&scope, rval, ()).unwrap();
        let ints: Vec<i32> = vals.iter().map(|v| v.to_int32()).collect();
        assert_eq!(ints, [1, 2, 3]);

        let rval = js::compile::evaluate(&scope, "({a: 1, b: 2})").unwrap();
        let rec = Record::<String, HandleValue>::from_jsval(&scope, rval, ()).unwrap();
        let pairs: Vec<(String, i32)> = rec.into_iter().map(|(k, v)| (k, v.to_int32())).collect();
        assert_eq!(pairs, [("a".to_string(), 1), ("b".to_string(), 2)]);

        // Stack newtypes work as element types too.
        let rval = js::compile::evaluate(&scope, "[{x: 1}, {x: 2}]").unwrap();
        let objs = Vec::<Object>::from_jsval(&scope, rval, ()).unwrap();
        assert_eq!(objs.len(), 2);
    }

    // --- Comparison operations ---
    {
        let v1 = 42.to_jsval(&scope).unwrap();
        let v2 = 42.to_jsval(&scope).unwrap();
        let v3 = 42.0.to_jsval(&scope).unwrap();

        // Strict equality: 42 === 42
        let eq = js::comparison::strictly_equal(&scope, v1, v2).unwrap();
        assert!(eq, "42 === 42 should be true");

        // Loose equality: 42 == 42.0
        let eq = js::comparison::loosely_equal(&scope, v1, v3).unwrap();
        assert!(eq, "42 == 42.0 should be true");

        // SameValue: Object.is(42, 42)
        let eq = js::comparison::same_value(&scope, v1, v2).unwrap();
        assert!(eq, "Object.is(42, 42) should be true");
    }

    // --- Map operations ---
    {
        let map = Map::new(&scope).unwrap();

        assert_eq!(map.size(&scope), 0);

        let key = 1.to_jsval(&scope).unwrap();
        let val = 100.to_jsval(&scope).unwrap();
        map.insert(&scope, key, val).unwrap();
        assert_eq!(map.size(&scope), 1);

        assert!(map.has(&scope, key).unwrap());

        let result_val = map.lookup(&scope, key).unwrap();
        assert_eq!(result_val.to_int32(), 100);

        assert!(map.delete(&scope, key).unwrap());
        assert_eq!(map.size(&scope), 0);
    }

    // --- Set operations ---
    {
        let set = Set::new(&scope).unwrap();

        assert_eq!(set.size(&scope), 0);

        let key = 42.to_jsval(&scope).unwrap();
        set.add(&scope, key).unwrap();
        assert_eq!(set.size(&scope), 1);
        assert!(set.has(&scope, key).unwrap());

        assert!(set.delete(&scope, key).unwrap());
        assert_eq!(set.size(&scope), 0);
    }

    // --- Date builtins ---
    {
        let rval = js::compile::evaluate(&scope, "new Date(2024, 0, 15)").unwrap();
        let date_obj = Object::from_raw_value(&scope, rval.get()).unwrap();
        assert!(Date::is_date(&scope, date_obj.handle()).unwrap());
        let js_date = date_obj.cast::<Date>().unwrap();
        assert!(js_date.is_valid(&scope).unwrap());
    }

    // --- Promise builtins ---
    {
        let rval = js::compile::evaluate(&scope, "new Promise(function(resolve) { resolve(42) })")
            .unwrap();
        let promise_obj = Object::from_raw_value(&scope, rval.get()).unwrap();
        assert!(Promise::is_promise(promise_obj.handle()));
        let js_promise = promise_obj.cast::<Promise>().unwrap();
        let _id = js_promise.id();
        // A settled promise reports its result.
        assert_eq!(js_promise.result(&scope).unwrap().to_int32(), 42);

        // A pending promise has no result yet (the engine asserts on the
        // access in debug builds).
        let rval = js::compile::evaluate(&scope, "new Promise(function() {})").unwrap();
        let pending = Object::from_raw_value(&scope, rval.get()).unwrap();
        let pending = pending.cast::<Promise>().unwrap();
        assert!(pending.is_pending());
        assert!(pending.result(&scope).is_none());
    }

    // --- Primitive type checks ---
    {
        assert!(Undefined::is_value(value::undefined()));
        assert!(!Undefined::is_value(value::null()));

        assert!(Null::is_value(value::null()));
        assert!(!Null::is_value(value::undefined()));

        assert!(Boolean::is_value(value::from_bool(true)));
        assert!(!Boolean::is_value(value::from_i32(1)));

        assert!(Int32::is_value(value::from_i32(42)));
        assert!(!Int32::is_value(value::from_f64(42.0)));

        assert!(Double::is_value(value::from_f64(2.5)));

        // Non-canonical NaNs (e.g. raw Float64Array bits) canonicalize
        // instead of aborting on DoubleValue's bit-pattern assertion.
        {
            use js::conversion::ToJSVal;
            let sneaky = f64::from_bits(0xFFFF_FFFF_FFFF_FFFF);
            assert!(sneaky.is_nan());
            let v = value::from_f64(sneaky);
            assert!(Double::is_value(v) && v.to_double().is_nan());
            let h = sneaky.to_jsval(&scope).unwrap();
            assert!(h.get().is_double() && h.get().to_double().is_nan());
            let h = (f32::from_bits(0xFFFF_FFFF)).to_jsval(&scope).unwrap();
            assert!(h.get().is_double() && h.get().to_double().is_nan());
        }

        assert!(!StringPrimitive::is_value(value::from_i32(0)));
        assert!(!SymbolPrimitive::is_value(value::from_i32(0)));
    }

    // --- JSON operations ---
    {
        let rval = js::json::parse(&scope, r#"{"a": 1}"#).unwrap();
        assert!(rval.is_object());
    }

    // --- Compile operations ---
    {
        assert!(js::compile::is_compilable_unit(&scope, "2 + 3"));
        // Incomplete source — missing closing brace
        assert!(!js::compile::is_compilable_unit(&scope, "function f() {"));
        // Interior NUL: a raw U+0000 is a legal SourceCharacter inside a
        // string literal, so the parser must see the full buffer — a
        // truncated C-string view would stop at the NUL and misjudge the
        // incomplete tail below.
        assert!(js::compile::is_compilable_unit(&scope, "'a\0b' + 'c'"));
        assert!(!js::compile::is_compilable_unit(
            &scope,
            "'a\0b' + function f() {"
        ));
    }

    // --- String char_at ---
    {
        let s = js::JSString::from_str(&scope, "ABC").unwrap();
        let ch = s.char_at(&scope, 0).unwrap();
        assert_eq!(ch, b'A' as u16);
        let ch = s.char_at(&scope, 2).unwrap();
        assert_eq!(ch, b'C' as u16);
    }

    // --- String to_utf8 with interior NUL ---
    {
        // U+0000 is a legal JS string code unit; extraction must not stop
        // at it.
        let s = js::JSString::from_str(&scope, "a\0b").unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s.to_utf8(&scope).unwrap(), "a\0b");
    }

    // --- Prototype chain ---
    {
        let obj = Object::new_plain(&scope).unwrap();
        let proto = obj.get_prototype(&scope).unwrap();
        assert!(proto.is_some(), "plain object has Object.prototype");
        // A null [[Prototype]] is a legitimate result, not an error.
        let end = proto.unwrap().get_prototype(&scope).unwrap();
        assert!(end.is_none(), "Object.prototype's prototype is null");
        assert!(!js::exception::is_pending(&scope));
    }

    // --- Atomization ---
    {
        // Non-ASCII text must round-trip (a Latin-1 read of the UTF-8 bytes
        // would mojibake it) and intern to a single atom.
        let atom = js::JSString::atomize(&scope, "héllo🦊").unwrap();
        assert_eq!(atom.to_utf8(&scope).unwrap(), "héllo🦊");
        let atom2 = js::JSString::atomize(&scope, "héllo🦊").unwrap();
        assert_eq!(atom.as_raw(), atom2.as_raw());

        // Pinning supports the Latin-1 range...
        let pinned = js::JSString::atomize_and_pin(&scope, "café").unwrap();
        assert_eq!(pinned.to_utf8(&scope).unwrap(), "café");
        assert!(pinned.has_been_pinned(&scope));
        // ...and throws for wider text instead of corrupting it.
        assert!(js::JSString::atomize_and_pin(&scope, "🦊").is_err());
        assert!(js::exception::is_pending(&scope));
        js::exception::clear(&scope);
    }

    // --- Exception capture for non-Error throws ---
    {
        use js::error::ExnThrown;

        // Primitive throws stringify instead of a generic placeholder.
        js::compile::evaluate(&scope, "throw 'boom'").unwrap_err();
        assert_eq!(ExnThrown::capture(&scope).message.as_deref(), Some("boom"));
        js::compile::evaluate(&scope, "throw 42").unwrap_err();
        assert_eq!(ExnThrown::capture(&scope).message.as_deref(), Some("42"));
        js::compile::evaluate(&scope, "throw {a: 1}").unwrap_err();
        assert_eq!(
            ExnThrown::capture(&scope).message.as_deref(),
            Some("[object Object]")
        );

        // A thrown symbol can't stringify; the fallback must not leave the
        // secondary ToString exception pending.
        js::compile::evaluate(&scope, "throw Symbol('s')").unwrap_err();
        assert_eq!(
            ExnThrown::capture(&scope).message.as_deref(),
            Some("(non-Error exception)")
        );
        assert!(!js::exception::is_pending(&scope));
    }

    // --- TryCatch operations ---
    {
        use js::try_catch::TryCatch;

        // TryCatch with no exception
        {
            let tc = TryCatch::new(&scope);
            let result = js::compile::evaluate(tc.scope(), "2 + 3");
            assert!(result.is_ok());
            assert!(!tc.has_caught(), "no exception should be pending");
        }

        // TryCatch catches a thrown exception
        {
            let tc = TryCatch::new(&scope);
            let result = js::compile::evaluate(tc.scope(), "throw new Error('test error')");
            assert!(result.is_err());
            assert!(tc.has_caught(), "exception should have been caught");

            // Inspect the exception value without clearing
            let exc = tc.exception();
            assert!(exc.is_some(), "should have an exception value");
            assert!(exc.unwrap().is_object(), "Error is an object");

            // Capture clears the exception
            let captured = tc.capture();
            assert!(
                captured.message.as_deref().unwrap().contains("test error"),
                "captured message should contain 'test error', got: {:?}",
                captured.message,
            );
            assert!(!tc.has_caught(), "capture should clear the exception");
        }

        // TryCatch reset clears exception
        {
            let tc = TryCatch::new(&scope);
            let _ = js::compile::evaluate(tc.scope(), "throw 'reset me'");
            assert!(tc.has_caught());
            tc.reset();
            assert!(!tc.has_caught(), "reset should clear exception");
        }

        // TryCatch rethrow re-sets exception
        {
            let tc = TryCatch::new(&scope);
            let _ = js::compile::evaluate(tc.scope(), "throw 42");
            assert!(tc.has_caught());
            let exc = tc.exception().unwrap();
            tc.reset();
            assert!(!tc.has_caught());
            tc.rethrow(exc);
            assert!(tc.has_caught(), "rethrow should re-set exception");

            // Clean up for subsequent tests
            tc.reset();
        }

        // TryCatch with syntax error
        {
            let tc = TryCatch::new(&scope);
            let result = js::compile::evaluate(tc.scope(), "function(");
            assert!(result.is_err());
            // Syntax errors are also caught
            let captured = tc.capture();
            assert!(captured.message.is_some());
        }
    }

    // --- Script .run() ---
    {
        let scope = scope.inner_scope();

        let script = js::compile::compile(&scope, "1 + 2").expect("compile should succeed");
        let result = js::compile::execute_script(&scope, script).expect("run should succeed");
        assert!(result.is_int32());
        assert_eq!(result.to_int32(), 3);
    }

    // --- Callbacks ---
    {
        use js::try_catch::TryCatch;

        let scope = scope.inner_scope();

        // Simple callback returning a constant
        {
            let fun = js::Function::new_callback(
                &scope,
                c"forty_two",
                0,
                |_scope, _args, _| Ok(value::from_i32(42)),
                (),
            )
            .expect("new_callback should succeed");

            scope
                .global()
                .set_property(&scope, c"fortyTwo", fun)
                .expect("set_property");

            let result =
                js::compile::evaluate(&scope, "fortyTwo()").expect("evaluate should succeed");
            assert!(result.is_int32());
            assert_eq!(result.to_int32(), 42);
        }

        // Callback that reads arguments
        {
            let fun = js::Function::new_callback(
                &scope,
                c"add",
                2,
                |_scope, args, _| {
                    let a = args.get_i32(0).unwrap_or(0);
                    let b = args.get_i32(1).unwrap_or(0);
                    Ok(value::from_i32(a + b))
                },
                (),
            )
            .expect("new_callback should succeed");

            scope
                .global()
                .set_property(&scope, c"add", fun)
                .expect("set_property");

            let result =
                js::compile::evaluate(&scope, "add(10, 32)").expect("evaluate should succeed");
            assert!(result.is_int32());
            assert_eq!(result.to_int32(), 42);
        }

        // Callback that returns an error
        {
            let fun = js::Function::new_callback(
                &scope,
                c"fail",
                0,
                |_scope, _args, _| Err(ExnThrown),
                (),
            )
            .expect("new_callback should succeed");

            scope
                .global()
                .set_property(&scope, c"fail", fun)
                .expect("set_property");

            let tc = TryCatch::new(&scope);
            let result = js::compile::evaluate(tc.scope(), "fail()");
            assert!(result.is_err(), "calling fail() should throw");
            assert!(tc.has_caught());
            tc.reset();
        }

        // Callback with CallbackArgs metadata
        {
            let fun = js::Function::new_callback(
                &scope,
                c"argInfo",
                0,
                |_scope, args, _| Ok(value::from_i32(args.len() as i32)),
                (),
            )
            .expect("new_callback should succeed");

            scope
                .global()
                .set_property(&scope, c"argInfo", fun)
                .expect("set_property");

            let result = js::compile::evaluate(&scope, "argInfo(1, 2, 3)").expect("evaluate");
            assert_eq!(result.to_int32(), 3);
        }
    }
}

/// `js::jobs::queue_microtask` runs a Rust callback as a microtask.
///
/// The job is enqueued as a promise reaction, so it is called with the source
/// promise's `undefined` result, and a job that throws must not stop the jobs
/// queued behind it.
#[test]
fn test_queue_microtask() {
    use std::cell::Cell;

    thread_local! {
        static RAN: Cell<u32> = const { Cell::new(0) };
        static FIRST_ARG_WAS_UNDEFINED: Cell<bool> = const { Cell::new(false) };
    }

    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    let record = js::Function::new_callback(
        &scope,
        c"record",
        0,
        |_scope, args, _| {
            RAN.with(|n| n.set(n.get() + 1));
            FIRST_ARG_WAS_UNDEFINED.with(|f| f.set(args.get(0).is_undefined()));
            Ok(value::undefined())
        },
        (),
    )
    .unwrap();

    let thrower =
        js::Function::new_callback(&scope, c"thrower", 0, |_scope, _args, _| Err(ExnThrown), ())
            .unwrap();

    // Nothing runs until the queue is drained.
    js::jobs::queue_microtask(&scope, &record).unwrap();
    assert_eq!(RAN.with(Cell::get), 0);
    assert!(js::jobs::has_pending_jobs(&scope));

    js::jobs::run_jobs(&scope);
    assert_eq!(RAN.with(Cell::get), 1);
    assert!(FIRST_ARG_WAS_UNDEFINED.with(Cell::get));

    // A job that throws leaves no pending exception behind and does not stop
    // the jobs queued after it.
    js::jobs::queue_microtask(&scope, &thrower).unwrap();
    js::jobs::queue_microtask(&scope, &record).unwrap();
    js::jobs::run_jobs(&scope);
    assert_eq!(RAN.with(Cell::get), 2);
    assert!(!js::exception::is_pending(&scope));

    // Jobs run in the order they were queued.
    let order = js::compile::evaluate(&scope, "globalThis.__order = []; 0").unwrap();
    assert_eq!(order.to_int32(), 0);
    for tag in ['a', 'b', 'c'] {
        let src = format!("() => globalThis.__order.push('{tag}')");
        let f = js::compile::evaluate(&scope, &src).unwrap();
        let f = Object::from_value(&scope, f).unwrap();
        let f = f.cast::<js::Function>().unwrap();
        js::jobs::queue_microtask(&scope, &f).unwrap();
    }
    js::jobs::run_jobs(&scope);
    let joined = js::compile::evaluate(&scope, "globalThis.__order.join('')").unwrap();
    let joined = String::from_jsval(&scope, joined, ()).unwrap();
    assert_eq!(joined, "abc");
}

/// `ArrayBuffer::transfer` moves the contents into a new buffer and detaches the original, for
/// empty buffers too.
#[test]
fn array_buffer_transfer_moves_the_contents() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    let value = js::compile::evaluate(&scope, "new Uint8Array([1, 2, 3, 4]).buffer").unwrap();
    let source = js::ArrayBuffer::from_jsval(&scope, value, ()).unwrap();
    let moved = source.transfer(&scope).unwrap();
    assert!(source.is_detached());
    assert_eq!(moved.byte_length(), 4);
    assert_eq!(moved.with_bytes(<[u8]>::to_vec), [1, 2, 3, 4]);

    let empty = js::ArrayBuffer::new(&scope, 0).unwrap();
    let moved_empty = empty.transfer(&scope).unwrap();
    assert!(empty.is_detached());
    assert_eq!(moved_empty.byte_length(), 0);
}

/// Byte access to a cross-compartment wrapper of an `ArrayBuffer` or a typed array reads the
/// wrapped object's bytes.
#[test]
fn byte_access_reads_through_cross_compartment_wrappers() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let value = js::compile::evaluate(&scope, "new Uint8Array([1, 2, 3, 4])").unwrap();
    let array = Object::from_value(&scope, value).unwrap();
    let value = js::compile::evaluate(&scope, "new Uint8Array([5, 6]).buffer").unwrap();
    let buffer = Object::from_value(&scope, value).unwrap();

    let other = rt.new_global();
    let wrapped_array = array.wrap_object(&other).unwrap();
    let wrapped_buffer = buffer.wrap_object(&other).unwrap();
    assert_ne!(wrapped_array.as_raw(), array.as_raw());
    assert_ne!(wrapped_buffer.as_raw(), buffer.as_raw());
    let array = wrapped_array.cast::<js::Uint8Array>().unwrap();
    assert_eq!(array.len(), 4);
    assert_eq!(array.with_slice(<[u8]>::to_vec), [1, 2, 3, 4]);
    let buffer = wrapped_buffer.cast::<js::ArrayBuffer>().unwrap();
    assert_eq!(buffer.with_bytes(<[u8]>::to_vec), [5, 6]);
    assert_eq!(buffer.copy_bytes(&other).unwrap(), [5, 6]);
}

/// Strings created from Rust text hold the same UTF-16 code units as the text, whether it is
/// ASCII, Latin-1, other BMP characters, or characters outside the BMP, and whether the
/// non-ASCII part starts at the beginning or after an ASCII prefix longer than the conversion
/// buffers' inline capacity.
#[test]
fn strings_from_rust_text_hold_its_utf16_code_units() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    let long_prefix = "a".repeat(300);
    let samples = [
        String::new(),
        "plain ascii".to_string(),
        "héllo wörld ÿ".to_string(),
        "é".to_string(),
        "日本語 ✓".to_string(),
        "emoji 🦀 and ☃".to_string(),
        format!("{long_prefix}é"),
        format!("{long_prefix}日"),
        format!("{long_prefix}🦀"),
    ];
    for text in &samples {
        let expected: Vec<u16> = text.encode_utf16().collect();
        let from_str = js::string::Str::from_str(&scope, text).unwrap();
        let converted = js::string::Str::from_value(&scope, text.as_str()).unwrap();
        for s in [from_str, converted] {
            assert_eq!(s.len(), expected.len(), "{text:?}");
            let units: Vec<u16> = (0..s.len())
                .map(|i| s.char_at(&scope, i).unwrap())
                .collect();
            assert_eq!(units, expected, "{text:?}");
            assert_eq!(s.to_utf8(&scope).unwrap(), *text);
        }
    }
}

/// Converting a JS string to UTF-8 flattens ropes, encodes Latin-1 characters after any ASCII
/// prefix as two bytes each, and replaces unpaired surrogates with U+FFFD.
#[test]
fn strings_convert_to_utf8_from_either_representation() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    let long_prefix = "a".repeat(300);
    let cases = [
        ("''", String::new()),
        ("'ÿé'", "ÿé".to_string()),
        ("'abc ÿé'", "abc ÿé".to_string()),
        ("'\\uD800x\\uDC00'", "\u{FFFD}x\u{FFFD}".to_string()),
        ("'\\uD83E\\uDD80'", "🦀".to_string()),
        (
            "let a = 'a'.repeat(300); a + String.fromCharCode(0xe9)",
            format!("{long_prefix}é"),
        ),
        (
            "let b = 'a'.repeat(300); b + String.fromCharCode(0x65e5)",
            format!("{long_prefix}日"),
        ),
    ];
    for (source, expected) in &cases {
        let value = js::compile::evaluate(&scope, source).unwrap();
        let s = js::string::Str::from_value(&scope, value).unwrap();
        assert_eq!(s.to_utf8(&scope).unwrap(), *expected, "{source}");
    }
}

/// Calling a value that isn't callable from native code throws a `TypeError` naming the method or
/// the value's source.
#[test]
fn native_calls_of_values_that_are_not_functions_name_them() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let message = |scope: &js::gc::scope::Scope<'_>| {
        let error = js::exception::take_pending(scope).expect("a pending exception");
        let error = Object::from_value(scope, error).expect("an error object");
        let message = error.get_property(scope, c"message").unwrap();
        String::from_jsval(scope, message, ()).unwrap()
    };

    let object = Object::new_plain(&scope).unwrap();
    let result =
        js::Function::call_by_name(&scope, object.handle(), c"nope", js::function::EmptyArgs);
    assert!(result.is_err());
    assert_eq!(message(&scope), "nope is not a function");

    let five = scope.root_value(value::from_i32(5));
    let result = js::Function::call(&scope, five, five, js::function::EmptyArgs);
    assert!(result.is_err());
    assert_eq!(message(&scope), "5 is not a function");

    let undefined = scope.root_value(value::undefined());
    let result = js::Function::call(&scope, undefined, undefined, js::function::EmptyArgs);
    assert!(result.is_err());
    assert_eq!(message(&scope), "undefined is not a function");

    let value = js::compile::evaluate(&scope, "({ a: [1, 'b'] })").unwrap();
    let result = js::Function::call_value(&scope, object.handle(), value, js::function::EmptyArgs);
    assert!(result.is_err());
    assert_eq!(message(&scope), r#"({a:[1, "b"]}) is not a function"#);
}

/// Setting and defining a property under a name that isn't interned yet stores the given value
/// when the GC that interning the name runs moves it.
#[cfg(feature = "debugmozjs")]
#[test]
fn named_property_writes_store_a_value_that_interning_the_name_moves() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let target = Object::new_plain(&scope).unwrap();
    let value = Object::new_plain(&scope).unwrap();

    // Zeal mode 14 runs a compacting GC on every allocation, which moves `value`.
    unsafe { js::gc::SetGCZeal(scope.raw_cx_no_gc(), 14, 1) };
    let set = target.set_property(&scope, c"setPropertyFreshName", value);
    let defined = target.define_property(
        &scope,
        c"definePropertyFreshName",
        value,
        js::class_spec::JSPROP_ENUMERATE as u32,
    );
    unsafe { js::gc::SetGCZeal(scope.raw_cx_no_gc(), 0, 0) };
    set.unwrap();
    defined.unwrap();

    for name in [c"setPropertyFreshName", c"definePropertyFreshName"] {
        let stored = target.get_property(&scope, name).unwrap();
        assert_eq!(stored.to_object(), value.as_raw(), "{name:?}");
    }
}
