// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Native integration tests for the JavaScript→host import path.
//!
//! A main module `import`s from a synthesized interface module, calls the
//! imported functions and checks the lifted results. A recording
//! [`ImportInvoker`] replaces the host call, and calls the `value::pop_*` and
//! `push_*` functions itself.
//!
//! The cases are grouped in a single test because `JSEngine` can only be
//! initialized once per process.

use std::cell::RefCell;
use std::rc::Rc;

use core_runtime::runtime::register_global_initializer;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::Object;
use wit_bridge::resources;
use wit_bridge::value::{self, TypeShape};
use wit_bridge::{
    initialize_runtime, with_main_scope, CallStack, EnumDesc, ErrorClass, ImportFuncDesc,
    ImportInvoker, InterfaceDesc, ResourceTypeDesc, WorldDesc,
};

/// The WIT name of the interface, which is the specifier of its module.
const IFACE: &str = "test:iface/x";

/// The application's main module. It imports the synthesized function `f` and the
/// synthesized resource class `R` from the interface module, and exports
/// functions that call them, which the test calls through `with_main_scope`.
///
/// The resource instance is kept in a module-level variable, so that a method
/// call doesn't need to receive it as an argument.
const MAIN: &str = r#"
    import { f, fResult, fEnumErr, fAsync, R, ZeroError, AccessError, Color, Perms }
        from "test:iface/x";

    // The enum and flags objects, their reverse mappings, and whether they are
    // frozen.
    export function enumFacts() {
        return JSON.stringify([
            Color.red, Color.green, Color[1], Object.isFrozen(Color),
            Perms.read | Perms.write, Perms.sticky, Perms[-2147483648],
            Object.isFrozen(Perms),
        ]);
    }

    export function callF() { return f(2, "hi"); }

    // Calling an async import with a sync-only invoker throws a TypeError
    // instead of panicking. Return the caught error's name.
    export function callFAsync() {
        try {
            fAsync(1);
            return "no throw";
        } catch (e) {
            return "threw:" + e.name;
        }
    }

    export function callFResultOk() { return fResult(10); }
    export function callFResultErr() {
        try {
            fResult(0);
            return "no throw";
        } catch (e) {
            // The err arm throws an instance of the `err` type's class, which
            // extends `ComponentError`, so return its payload.
            if (!(e instanceof ZeroError) || !(e instanceof ComponentError)) {
                return "not a ZeroError";
            }
            if (e.name !== "ZeroError") {
                return `named ${e.name}`;
            }
            return e.payload;
        }
    }

    // An enum `err` payload throws an instance of its type's class, whose message
    // is the case name.
    export function callFEnumErr() {
        try {
            fEnumErr(0);
            return "no throw";
        } catch (e) {
            return `${e instanceof AccessError} ${e.name} ${e.message} ${e.payload}`;
        }
    }

    let inst;
    export function ctorR(seed) { inst = new R(seed); return inst; }
    // The wrapper's own and inherited enumerable keys.
    export function instKeys() {
        const keys = [];
        for (const key in inst) keys.push(key);
        return JSON.stringify([Object.keys(inst), keys, Object.keys(R)]);
    }
    export function callM(x) { return inst.m(x); }
    export function instHasM() { return typeof inst.m === "function"; }
    export function callS() { return R.s(); }
"#;

mod shared;
use shared::{as_number, as_string, force_gc, num};

/// The lowered arguments of all import calls, and the index of the last call.
#[derive(Debug, Default)]
struct Recording {
    /// The import index of the most recent call.
    last_index: Option<u32>,
    /// The lowered `u32` arguments seen, in pop (declaration) order.
    u32_args: Vec<u32>,
    /// The lowered `string` arguments seen, in pop (declaration) order.
    string_args: Vec<String>,
    /// Resource self-handles lowered off a method receiver (`borrow`).
    self_handles: Vec<u32>,
}

/// An [`ImportInvoker`] that lowers the arguments with the `value::pop_*`
/// functions, records them, and lifts a result with the `value::push_*`
/// functions. It calls the functions for the import's parameter and result
/// types in the same order as the generated code.
struct RecordingInvoker {
    log: Rc<RefCell<Recording>>,
    /// The handle returned by the resource constructor. A method checks that
    /// its receiver has this handle.
    ctor_handle: u32,
    /// The resource type index of the created wrappers.
    resource_type_idx: u32,
}

impl ImportInvoker for RecordingInvoker {
    fn invoke(
        &self,
        scope: &Scope<'_>,
        stack: &mut CallStack,
        func: &ImportFuncDesc,
    ) -> Result<(), ExnThrown> {
        self.log.borrow_mut().last_index = Some(func.index);

        // The first argument is on top of the stack, so the arguments are popped
        // in declaration order. The last lowered `u32` is kept, so that the
        // result of `f-result` can depend on it.
        let mut last_u32 = 0u32;
        for shape in &func.params {
            match shape {
                TypeShape::U32 => {
                    let n = value::pop_u32(stack, scope)?;
                    last_u32 = n;
                    self.log.borrow_mut().u32_args.push(n);
                }
                TypeShape::String => {
                    let s = value::pop_string(stack, scope)?;
                    self.log.borrow_mut().string_args.push(s);
                }
                TypeShape::BorrowResource { .. } => {
                    // A method's leading `self`: lower it as a borrow to read the
                    // receiver's canonical handle.
                    let desc = wit_bridge::ResourceDesc::Imported {
                        type_idx: self.resource_type_idx,
                    };
                    let handle = resources::pop_borrow(stack, scope, &desc)?;
                    self.log.borrow_mut().self_handles.push(handle);
                }
                other => panic!("recording invoker: unexpected param shape {other:?}"),
            }
        }

        // Lift the result the host "returned", per the descriptor's result shape.
        match func.result.as_ref() {
            // `f(a, b) -> u32` and `[method]r.m(self, x) -> u32` and
            // `[static]r.s() -> u32`: return a marker so the JS side can assert
            // the lifted value.
            Some(TypeShape::U32) => {
                value::push_u32(stack, 4242);
                Ok(())
            }
            // `f-enum-err(a) -> result<u32, access-error>`: `err(denied)`.
            Some(TypeShape::Result(ok, Some(err))) if matches!(**err, TypeShape::Enum(_)) => {
                value::push_enum(stack, scope, 2, 1)?;
                value::push_result(stack, scope, ok.is_some(), true, true)
            }
            // `f-result(a) -> result<u32, string>`: `ok(a*10)` when `a != 0`, an
            // `err("zero")` when `a == 0`, branching on the lowered argument.
            Some(TypeShape::Result(ok, err)) => {
                if last_u32 == 0 {
                    value::push_string(stack, scope, "zero")?;
                    value::push_result(stack, scope, ok.is_some(), err.is_some(), true)
                } else {
                    value::push_u32(stack, last_u32.wrapping_mul(10));
                    value::push_result(stack, scope, ok.is_some(), err.is_some(), false)
                }
            }
            // `[constructor]r -> own<r>`: create an imported wrapper holding the
            // handle, which becomes the result of `new R(...)`.
            Some(TypeShape::OwnResource { .. }) => {
                let desc = wit_bridge::ResourceDesc::Imported {
                    type_idx: self.resource_type_idx,
                };
                resources::push_own(stack, scope, &desc, self.ctor_handle)
            }
            other => panic!("recording invoker: unexpected result shape {other:?}"),
        }
    }
}

/// The indices of the imports.
const IDX_F: u32 = 0;
const IDX_F_RESULT: u32 = 1;
const IDX_CTOR: u32 = 2;
const IDX_METHOD: u32 = 3;
const IDX_STATIC: u32 = 4;
const IDX_F_ASYNC: u32 = 5;
const IDX_F_ENUM_ERR: u32 = 6;
const RESOURCE_TYPE_IDX: u32 = 7;
/// The handle returned by the constructor, which a method checks on its
/// receiver.
const CTOR_HANDLE: u32 = 0x9000_0000;

/// The class of `f-result`'s `err` arm.
fn zero_error() -> ErrorClass {
    ErrorClass {
        interface: Some(IFACE.to_string()),
        name: "zero-error".to_string(),
        enum_cases: Vec::new(),
    }
}

/// The class of `f-enum-err`'s `err` arm, an enum.
fn access_error() -> ErrorClass {
    ErrorClass {
        interface: Some(IFACE.to_string()),
        name: "access-error".to_string(),
        enum_cases: vec!["notFound".to_string(), "denied".to_string()],
    }
}

fn build_world() -> WorldDesc {
    WorldDesc {
        interfaces: vec![InterfaceDesc {
            wit_name: Some(IFACE.to_string()),
            functions: vec![
                ImportFuncDesc {
                    index: IDX_F,
                    name: "f".to_string(),
                    params: vec![TypeShape::U32, TypeShape::String],
                    result: Some(TypeShape::U32),
                    is_async: false,
                    err_class: None,
                },
                ImportFuncDesc {
                    index: IDX_F_RESULT,
                    name: "f-result".to_string(),
                    params: vec![TypeShape::U32],
                    result: Some(TypeShape::Result(
                        Some(Box::new(TypeShape::U32)),
                        Some(Box::new(TypeShape::String)),
                    )),
                    is_async: false,
                    err_class: Some(zero_error()),
                },
                ImportFuncDesc {
                    index: IDX_F_ENUM_ERR,
                    name: "f-enum-err".to_string(),
                    params: vec![TypeShape::U32],
                    result: Some(TypeShape::Result(
                        Some(Box::new(TypeShape::U32)),
                        Some(Box::new(TypeShape::Enum(vec![
                            "notFound".to_string(),
                            "denied".to_string(),
                        ]))),
                    )),
                    is_async: false,
                    err_class: Some(access_error()),
                },
                // Paired with the sync recording invoker, to exercise the
                // `call_import_from_js_async` guard.
                ImportFuncDesc {
                    index: IDX_F_ASYNC,
                    name: "f-async".to_string(),
                    params: vec![TypeShape::U32],
                    result: Some(TypeShape::U32),
                    is_async: true,
                    err_class: None,
                },
            ],
            error_classes: vec![zero_error(), access_error()],
            enums: vec![
                EnumDesc {
                    js_name: "Color".to_string(),
                    members: vec![("red".to_string(), 0), ("green".to_string(), 1)],
                },
                EnumDesc {
                    js_name: "Perms".to_string(),
                    members: vec![
                        ("read".to_string(), 1),
                        ("write".to_string(), 2),
                        ("sticky".to_string(), i32::MIN),
                    ],
                },
            ],
            resources: vec![ResourceTypeDesc {
                type_idx: RESOURCE_TYPE_IDX,
                name: "r".to_string(),
                constructor: Some(ImportFuncDesc {
                    index: IDX_CTOR,
                    name: "[constructor]r".to_string(),
                    params: vec![TypeShape::U32],
                    result: Some(TypeShape::OwnResource {
                        index: RESOURCE_TYPE_IDX,
                        imported: true,
                    }),
                    is_async: false,
                    err_class: None,
                }),
                methods: vec![(
                    "m".to_string(),
                    ImportFuncDesc {
                        index: IDX_METHOD,
                        name: "[method]r.m".to_string(),
                        // The leading self is a borrow, followed by the explicit arg.
                        params: vec![
                            TypeShape::BorrowResource {
                                index: RESOURCE_TYPE_IDX,
                                imported: true,
                            },
                            TypeShape::U32,
                        ],
                        result: Some(TypeShape::U32),
                        is_async: false,
                        err_class: None,
                    },
                )],
                statics: vec![(
                    "s".to_string(),
                    ImportFuncDesc {
                        index: IDX_STATIC,
                        name: "[static]r.s".to_string(),
                        params: vec![],
                        result: Some(TypeShape::U32),
                        is_async: false,
                        err_class: None,
                    },
                )],
            }],
        }],
        unexported_error_classes: Vec::new(),
    }
}

#[test]
fn imports_native() {
    register_global_initializer(wit_bridge::add_to_global);

    let log = Rc::new(RefCell::new(Recording::default()));
    let invoker: Rc<dyn ImportInvoker> = Rc::new(RecordingInvoker {
        log: Rc::clone(&log),
        ctor_handle: CTOR_HANDLE,
        resource_type_idx: RESOURCE_TYPE_IDX,
    });

    let world = build_world();
    initialize_runtime(
        &core_runtime::config::RuntimeConfig::default(),
        MAIN,
        "main.js",
        None,
        |scope| {
            // The synthesized resource prototype is allocated under compacting GC
            // zeal so it lives in a relocatable arena, which
            // `resource_prototype_registry_survives_relocation` below depends on.
            #[cfg(feature = "debugmozjs")]
            // SAFETY: the bootstrap scope wraps a valid context.
            unsafe {
                js::gc::SetGCZeal(scope.raw_cx_no_gc(), 14, 1)
            };
            let result = wit_bridge::synthesize_import_modules(scope, &world, Rc::clone(&invoker));
            // Reset before `initialize_runtime` goes on to compile and evaluate
            // the main module.
            #[cfg(feature = "debugmozjs")]
            // SAFETY: same context as above.
            unsafe {
                js::gc::SetGCZeal(scope.raw_cx_no_gc(), 0, 0)
            };
            result
        },
    )
    .expect("runtime bootstrap failed");

    with_main_scope(|scope, _ns| force_gc(scope));

    freestanding_function_lowers_args_and_lifts_result(&log);
    result_typed_import_ok_unwraps_to_bare_value();
    result_typed_import_err_throws_component_error();
    enum_err_throws_its_case_name();
    async_import_with_sync_invoker_throws();
    method_without_receiver_throws();
    resource_constructor_method_and_static(&log);
    enums_and_flags_are_frozen_objects();
    #[cfg(feature = "debugmozjs")]
    resource_prototype_registry_survives_relocation();
    wit_bridge::shutdown_runtime();
}

/// The module exports each [`EnumDesc`] as a frozen object mapping case names
/// to values and values to case names. A flags member for bit 31 is negative.
fn enums_and_flags_are_frozen_objects() {
    with_main_scope(|scope, ns| {
        force_gc(scope);
        let result = call_named(scope, ns, c"enumFacts", &[]);
        assert_eq!(
            as_string(scope, result),
            r#"[0,1,"green",true,3,-2147483648,"sticky",true]"#
        );
    });
}

/// Calling an async import with an invoker whose `supports_async()` is `false`
/// throws a `TypeError` instead of panicking in `invoke_async`.
fn async_import_with_sync_invoker_throws() {
    with_main_scope(|scope, ns| {
        force_gc(scope);
        let result = call_named(scope, ns, c"callFAsync", &[]);
        assert_eq!(
            as_string(scope, result),
            "threw:TypeError",
            "an async import on a sync-only invoker throws a TypeError, not a panic"
        );
        assert!(!js::exception::is_pending(scope));
    });
}

/// A method import called with no arguments, so without its receiver, throws a
/// `TypeError` counting the arguments without the receiver.
fn method_without_receiver_throws() {
    with_main_scope(|scope, _ns| {
        assert!(wit_bridge::call_import_from_js(scope, IDX_METHOD, &[]).is_err());
        let message = js::error::ExnThrown::capture(scope).message;
        assert!(
            message
                .as_deref()
                .is_some_and(|m| m.ends_with("takes 1 argument, got 0")),
            "{message:?}"
        );
    });
}

/// `f-enum-err(0)` throws an `AccessError` whose message is the name of the
/// `err` case and whose payload is the case's index.
fn enum_err_throws_its_case_name() {
    with_main_scope(|scope, ns| {
        let result = call_named(scope, ns, c"callFEnumErr", &[]);
        assert_eq!(as_string(scope, result), "true AccessError denied 1");
    });
}

/// Call a named export off the main namespace with the given args, returning the
/// (unrooted) result value.
fn call_named<'s>(
    scope: &'s Scope<'_>,
    ns: &Object<'s>,
    name: &std::ffi::CStr,
    args: &[js::native::Handle<'s, js::native::Value>],
) -> js::native::Handle<'s, js::native::Value> {
    let f = ns
        .get_property_by_name(scope, name)
        .expect("export resolves");
    match js::Function::call(scope, ns, f, args) {
        Ok(v) => v,
        Err(_) => {
            let captured = js::error::ExnThrown::capture(scope);
            panic!(
                "export {name:?} call trapped: {:?}",
                captured.message.unwrap_or_default()
            );
        }
    }
}

/// `f(2, "hi") -> u32`. The import lowers its `u32` and `string` arguments in
/// declaration order, and JS receives the lifted `u32` result. The recorded
/// index is `IDX_F`.
fn freestanding_function_lowers_args_and_lifts_result(log: &Rc<RefCell<Recording>>) {
    with_main_scope(|scope, ns| {
        force_gc(scope);
        let result = call_named(scope, ns, c"callF", &[]);
        assert_eq!(
            as_number(result.get()),
            4242.0,
            "f's lifted u32 result reaches JS"
        );

        let rec = log.borrow();
        assert_eq!(rec.last_index, Some(IDX_F), "callback threaded f's index");
        assert_eq!(rec.u32_args, vec![2], "f lowered its u32 arg 2");
        assert_eq!(
            rec.string_args,
            vec!["hi".to_string()],
            "f lowered its string arg \"hi\""
        );
    });
}

/// `f-result(10) -> result<u32, string>` lifted as `ok(100)`.
/// JS receives `100`, not the `{tag, val}` object.
fn result_typed_import_ok_unwraps_to_bare_value() {
    with_main_scope(|scope, ns| {
        force_gc(scope);
        let result = call_named(scope, ns, c"callFResultOk", &[]);
        assert_eq!(
            as_number(result.get()),
            100.0,
            "ok arm unwraps {{tag, val}} to the bare value (10 * 10)"
        );
    });
}

/// `f-result(0)` lifted as `err("zero")`.
/// The call throws an instance of the `err` type's class, `ZeroError`, which
/// the module exports and which extends `ComponentError`, with the `err` string
/// as its `payload`. `callFResultErr` returns `e.payload`.
fn result_typed_import_err_throws_component_error() {
    with_main_scope(|scope, ns| {
        force_gc(scope);
        let result = call_named(scope, ns, c"callFResultErr", &[]);
        assert_eq!(
            as_string(scope, result),
            "zero",
            "err arm throws a ZeroError whose .payload is the err string"
        );
        // The exception was caught in JS, so nothing is pending.
        assert!(!js::exception::is_pending(scope));
    });
}

/// The synthesized resource class `R`.
///
/// - `new R(seed)` calls the constructor import, and returns the wrapper
///   created by the invoker with `push_own`.
/// - `R.s()` calls the static import, which is a property of the class.
/// - `r.m(x)` calls the method import with the receiver as the first argument,
///   which the invoker lowers as a `borrow`. The receiver's handle is the
///   handle returned by the constructor.
fn resource_constructor_method_and_static(log: &Rc<RefCell<Recording>>) {
    with_main_scope(|scope, ns| {
        force_gc(scope);

        // new R(99) calls the constructor import and returns its wrapper.
        let inst = call_named(scope, ns, c"ctorR", &[num(scope, 99)]);
        assert!(inst.is_object(), "new R(99) yields an object");
        {
            let rec = log.borrow();
            assert_eq!(rec.last_index, Some(IDX_CTOR), "constructor index threaded");
            // The constructor's only WIT param is its `u32` seed.
            assert!(
                rec.u32_args.contains(&99),
                "constructor lowered its u32 seed 99, saw {:?}",
                rec.u32_args
            );
        }
        // The wrapper holds the canonical handle the invoker returned.
        assert_eq!(
            wit_bridge::resources::imported_handle(scope, inst),
            Some(CTOR_HANDLE),
            "minted wrapper carries the constructor's canonical handle"
        );

        force_gc(scope);

        // R.s(): a static function, which is a property of the class.
        let s = call_named(scope, ns, c"callS", &[]);
        assert_eq!(
            as_number(s.get()),
            4242.0,
            "static R.s() lifts its u32 result"
        );
        assert_eq!(
            log.borrow().last_index,
            Some(IDX_STATIC),
            "static index threaded"
        );

        // The instance exposes `m` through `R.prototype`.
        let has_m = call_named(scope, ns, c"instHasM", &[]);
        assert!(
            has_m.is_boolean() && has_m.to_boolean(),
            "the constructed instance exposes its method `m` via R.prototype"
        );
        // Methods and statics are not enumerable, as those of a JS `class` are.
        let keys = call_named(scope, ns, c"instKeys", &[]);
        assert_eq!(as_string(scope, keys), "[[],[],[]]");

        force_gc(scope);

        // r.m(5): the receiver is lowered as a `borrow`, followed by the `u32`
        // argument. The receiver's handle is CTOR_HANDLE.
        let before = log.borrow().self_handles.len();
        let m = call_named(scope, ns, c"callM", &[num(scope, 5)]);
        assert_eq!(
            as_number(m.get()),
            4242.0,
            "method r.m(5) lifts its u32 result"
        );
        let rec = log.borrow();
        assert_eq!(rec.last_index, Some(IDX_METHOD), "method index threaded");
        assert_eq!(
            rec.self_handles.get(before).copied(),
            Some(CTOR_HANDLE),
            "the method forwarded the receiver's handle as its first lowered arg"
        );
        assert!(
            rec.u32_args.contains(&5),
            "the method lowered its explicit u32 arg 5, saw {:?}",
            rec.u32_args
        );
    });
}

/// The imported-resource prototype registry survives a relocating GC: a wrapper
/// created after a compacting GC has the same prototype as one created before
/// it.
#[cfg(feature = "debugmozjs")]
fn resource_prototype_registry_survives_relocation() {
    with_main_scope(|scope, ns| {
        // The instance stays rooted across the relocating GC so its prototype is
        // the live object that moves.
        let first = call_named(scope, ns, c"ctorR", &[num(scope, 11)]);
        let first_obj = Object::from_value(scope, first).expect("first instance is an object");

        // SAFETY: the main scope wraps a valid context.
        unsafe { js::gc::SetGCZeal(scope.raw_cx_no_gc(), 14, 1) };
        js::gc::prepare_for_full_gc(scope);
        js::gc::non_incremental_gc(scope, js::gc::GCOptions::Shrink, js::gc::GCReason::API);

        let second = call_named(scope, ns, c"ctorR", &[num(scope, 22)]);
        let second_obj = Object::from_value(scope, second).expect("second instance is an object");

        // SAFETY: same context.
        unsafe { js::gc::SetGCZeal(scope.raw_cx_no_gc(), 0, 0) };

        // The second wrapper's method resolves through the (relocated) prototype.
        let has_m = call_named(scope, ns, c"instHasM", &[]);
        assert!(
            has_m.is_boolean() && has_m.to_boolean(),
            "the second wrapper exposes `m` via the relocated R.prototype"
        );
        let m = call_named(scope, ns, c"callM", &[num(scope, 7)]);
        assert_eq!(
            as_number(m.get()),
            4242.0,
            "calling a method on the second wrapper works after relocation"
        );

        // No GC runs between the two reads, so a raw-pointer compare is a valid
        // identity check.
        let proto_first = first_obj
            .get_prototype(scope)
            .expect("first prototype readable")
            .expect("first wrapper has a prototype");
        let proto_second = second_obj
            .get_prototype(scope)
            .expect("second prototype readable")
            .expect("second wrapper has a prototype");
        assert_eq!(
            proto_first.as_raw(),
            proto_second.as_raw(),
            "both wrappers share R.prototype from the registry"
        );
    });
}
