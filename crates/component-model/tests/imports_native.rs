// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Native integration tests for the JavaScript→host import path.
//!
//! A main module `import`s from a synthesized interface module, calls the
//! imported functions and observes the lifted results, while a recording
//! [`ImportInvoker`] stands in for the wit-dylib host call by driving the real
//! `value::pop_*`/`push_*` conversions by hand.
//!
//! Like the other native suites this is one `#[test]` against a once-per-process
//! runtime, kept in its own test binary so it does not fight `exports_native` or
//! `sync_native` over the engine.

use std::cell::RefCell;
use std::rc::Rc;

use component_model::value::{self, TypeShape};
use component_model::{
    initialize_runtime, with_main_scope, CallStack, EnumDesc, ErrorClass, ImportFuncDesc,
    ImportInvoker, InterfaceDesc, ResourceTypeDesc, WorldDesc,
};
use core_runtime::runtime::register_global_initializer;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::Object;

/// The WIT interface specifier the synthesized module is named after, used
/// verbatim as the `import` specifier (imports.rs synthesizes the module under
/// `InterfaceDesc::wit_name`).
const IFACE: &str = "test:iface/x";

/// The application's main module. It imports the synthesized function `f` and the
/// synthesized resource class `R` from the interface module, and exposes thin
/// export wrappers the test drives through `with_main_scope`.
///
/// The resource instance is kept in a module-level variable so a method call does
/// not have to round-trip the wrapper object back through a lowered argument.
const MAIN: &str = r#"
    import { f, fResult, fAsync, R, ZeroError, Color, Perms } from "test:iface/x";

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

    // An async import dispatched to a sync-only invoker must throw cleanly
    // (a TypeError), not panic. Report the caught error's name.
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
            // extends `ComponentError`, so surface its payload.
            if (!(e instanceof ZeroError) || !(e instanceof ComponentError)) {
                return "not a ZeroError";
            }
            if (e.name !== "ZeroError") {
                return `named ${e.name}`;
            }
            return e.payload;
        }
    }

    let inst;
    export function ctorR(seed) { inst = new R(seed); return inst; }
    export function callM(x) { return inst.m(x); }
    export function instHasM() { return typeof inst.m === "function"; }
    export function callS() { return R.s(); }
"#;

mod shared;
use shared::{as_number, as_string, force_gc, num};

/// Set a GC zeal mode (14 = Compact, which relocates objects) on the scope's
/// context.
///
/// # Safety
///
/// Must only be called when a valid `JSContext` is available.
#[cfg(feature = "debugmozjs")]
unsafe fn set_zeal(scope: &Scope<'_>, mode: u8, frequency: u32) {
    js::gc::SetGCZeal(scope.raw_cx_no_gc(), mode, frequency);
}

/// Reset GC zeal back to the default (mode 0), so the rest of the run does not
/// take a full compacting GC on every allocation.
#[cfg(feature = "debugmozjs")]
unsafe fn reset_zeal(scope: &Scope<'_>) {
    js::gc::SetGCZeal(scope.raw_cx_no_gc(), 0, 0);
}

/// Force a full compacting GC, which relocates tenured objects. Only relocation
/// surfaces an un-traced `Heap` as a stale pointer read, so the
/// prototype-registry trace test below needs this rather than [`force_gc`].
#[cfg(feature = "debugmozjs")]
fn force_compacting_gc(scope: &Scope<'_>) {
    js::gc::prepare_for_full_gc(scope);
    js::gc::non_incremental_gc(scope, js::gc::GCOptions::Shrink, js::gc::GCReason::API);
}

/// One recorded import call: the lowered arguments and the function index.
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

/// A test [`ImportInvoker`] that lowers the call's arguments through the real
/// `value::pop_*` conversions, records what it saw, and lifts a chosen result back
/// through the real `value::push_*` conversions. The behavior per function is
/// keyed off the descriptor's `params` and `result` shapes, so it drives the same
/// conversions in the same order the generated code would.
struct RecordingInvoker {
    log: Rc<RefCell<Recording>>,
    /// Handle the resource constructor lifts as the new instance's canonical
    /// handle. Read back when a method lowers its receiver, to assert identity
    /// threading.
    ctor_handle: u32,
    /// The resource type index minted wrappers carry.
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

        // `call_import_from_js` pushed the arguments reversed, so a forward sweep
        // of `params` pops them in declaration order. The last lowered `u32` is
        // tracked so a result-typed import can branch on it.
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
                    let desc = component_model::ResourceDesc::Imported {
                        type_idx: self.resource_type_idx,
                    };
                    let handle = value::pop_borrow(stack, scope, &desc)?;
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
            // `[constructor]r -> own<r>`: mint an imported wrapper carrying the
            // canonical handle, which `new R(...)` adopts as the instance.
            Some(TypeShape::OwnResource { .. }) => {
                let desc = component_model::ResourceDesc::Imported {
                    type_idx: self.resource_type_idx,
                };
                let dispose = no_op_dispose(scope);
                value::push_own(stack, scope, &desc, self.ctor_handle, dispose)
            }
            other => panic!("recording invoker: unexpected result shape {other:?}"),
        }
    }
}

/// A no-op dispose callback for minted wrappers (the test does not exercise
/// drop here; `sync_native` covers dispose semantics).
fn no_op_dispose<'s>(scope: &'s Scope<'_>) -> js::prelude::HandleValue<'s> {
    let f = js::Function::new_callback(
        scope,
        c"noopDispose",
        0,
        |_scope, _args, _payload| Ok(js::value::undefined()),
        (),
    )
    .unwrap();
    scope.root_value(f.as_value())
}

/// Import indices, assigned by hand (the synthesis records every import by its
/// `index`, and the callback payload holds it).
const IDX_F: u32 = 0;
const IDX_F_RESULT: u32 = 1;
const IDX_CTOR: u32 = 2;
const IDX_METHOD: u32 = 3;
const IDX_STATIC: u32 = 4;
const IDX_F_ASYNC: u32 = 5;
const RESOURCE_TYPE_IDX: u32 = 7;
/// The canonical handle the constructor invoker returns, a distinctive value so a
/// method that lowers its receiver can assert it threaded the right handle.
///
/// Deliberately above `i32::MAX`, which `from_u32` boxes as a JS double, so this
/// also exercises the large-handle round-trip from `push_own` back through
/// `read_handle`.
const CTOR_HANDLE: u32 = 0x9000_0000;

/// The class of `f-result`'s `err` arm.
fn zero_error() -> ErrorClass {
    ErrorClass {
        interface: Some(IFACE.to_string()),
        name: "zero-error".to_string(),
        enum_cases: Vec::new(),
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
            error_classes: vec![zero_error()],
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
    }
}

#[test]
fn imports_native() {
    register_global_initializer(component_model::add_to_global);

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
            //
            #[cfg(feature = "debugmozjs")]
            // SAFETY: the bootstrap scope wraps a valid context.
            unsafe {
                set_zeal(scope, 14, 1)
            };
            // SAFETY: the module loader is initialized by `Runtime::init` (inside
            // `initialize_runtime`) before `synthesize` runs.
            let result = unsafe {
                component_model::synthesize_import_modules(scope, &world, Rc::clone(&invoker))
            };
            // Reset before `initialize_runtime` goes on to compile and evaluate
            // the main module.
            #[cfg(feature = "debugmozjs")]
            // SAFETY: same context as above.
            unsafe {
                reset_zeal(scope)
            };
            result
        },
    )
    .expect("runtime bootstrap failed");

    with_main_scope(|scope, _ns| force_gc(scope));

    freestanding_function_lowers_args_and_lifts_result(&log);
    result_typed_import_ok_unwraps_to_bare_value();
    result_typed_import_err_throws_component_error();
    async_import_with_sync_invoker_throws();
    resource_constructor_method_and_static(&log);
    enums_and_flags_are_frozen_objects();
    #[cfg(feature = "debugmozjs")]
    resource_prototype_registry_survives_relocation();
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

/// An async-declared import dispatched to a sync-only invoker (whose
/// `supports_async()` is `false`) must throw a clean `TypeError`, not reach
/// `invoke_async`'s panic on a future polled across the FFI boundary.
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

/// Call a named export off the main namespace with the given args, returning the
/// (unrooted) result value.
fn call_named<'s>(
    scope: &'s Scope<'_>,
    ns: &Object<'s>,
    name: &std::ffi::CStr,
    args: &[js::native::Handle<'s, js::native::Value>],
) -> js::native::Value {
    let f = ns.get_property(scope, name).expect("export resolves");
    match js::Function::call(scope, ns, f, args) {
        Ok(v) => v.get(),
        Err(_) => {
            let captured = js::error::ExnThrown::capture(scope);
            panic!(
                "export {name:?} call trapped: {:?}",
                captured.message.unwrap_or_default()
            );
        }
    }
}

/// Stage 1: `f(2, "hi") -> u32`. The freestanding import lowers its `u32` and
/// `string` arguments in declaration order, which a reversed push would break by
/// feeding `"hi"` to `pop_u32`, and the JS side observes the lifted `u32` result.
/// The recorded index must be `IDX_F`, threaded from the callback payload.
fn freestanding_function_lowers_args_and_lifts_result(log: &Rc<RefCell<Recording>>) {
    with_main_scope(|scope, ns| {
        force_gc(scope);
        let result = call_named(scope, ns, c"callF", &[]);
        assert_eq!(
            as_number(result),
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

/// Stage 2 (ok arm): `f-result(10) -> result<u32, string>` lifted as `ok(100)`.
/// `handle_import_result` must unwrap the `{tag, val}` wrapper down to the bare
/// `val` before it reaches JS.
fn result_typed_import_ok_unwraps_to_bare_value() {
    with_main_scope(|scope, ns| {
        force_gc(scope);
        let result = call_named(scope, ns, c"callFResultOk", &[]);
        assert_eq!(
            as_number(result),
            100.0,
            "ok arm unwraps {{tag, val}} to the bare value (10 * 10)"
        );
    });
}

/// Stage 2 (err arm): `f-result(0)` lifted as `err("zero")`.
/// `handle_import_result` must throw an instance of the `err` type's class,
/// `ZeroError`, which the module exports and which extends `ComponentError`,
/// carrying the `err` string as its `.payload`. The JS `callFResultErr` returns
/// `e.payload`, which a plain throw would not expose.
fn result_typed_import_err_throws_component_error() {
    with_main_scope(|scope, ns| {
        force_gc(scope);
        let result = call_named(scope, ns, c"callFResultErr", &[]);
        assert_eq!(
            as_string(scope, result),
            "zero",
            "err arm throws a ZeroError whose .payload is the err string"
        );
        // The throw was fully absorbed by the guest's try/catch, so nothing is
        // pending.
        assert!(!js::exception::is_pending(scope));
    });
}

/// Stage 3: the synthesized resource class `R`.
///
/// - `new R(seed)` invokes the constructor import (index threaded as a ctor) and
///   produces the wrapper the invoker minted via `push_own`, carrying the hidden
///   handle field.
/// - `R.s()` invokes the static import (statics live on the constructor object).
/// - `r.m(x)` invokes the method import. `method_callback` prepends the receiver
///   as `args[0]`, so the invoker lowers it as the leading `borrow` self, and the
///   recorded self-handle must equal the handle the constructor returned.
fn resource_constructor_method_and_static(log: &Rc<RefCell<Recording>>) {
    with_main_scope(|scope, ns| {
        force_gc(scope);

        // new R(99): invokes the constructor import and adopts its minted wrapper.
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
        // The wrapper holds the hidden canonical handle the invoker returned.
        let inst_obj = Object::from_value(scope, inst).expect("instance is an object");
        let handle = inst_obj
            .get_property(scope, component_model::resources::HANDLE_FIELD)
            .expect("handle field readable")
            .get();
        // A handle above `i32::MAX` is boxed as a double, so it is read through
        // the number path.
        assert!(
            handle.is_number() && handle.to_number() as u32 == CTOR_HANDLE,
            "minted wrapper carries the constructor's canonical handle"
        );

        force_gc(scope);

        // R.s(): a static, resolved off the constructor object.
        let s = call_named(scope, ns, c"callS", &[]);
        assert_eq!(as_number(s), 4242.0, "static R.s() lifts its u32 result");
        assert_eq!(
            log.borrow().last_index,
            Some(IDX_STATIC),
            "static index threaded"
        );

        // The instance must expose `m` via its prototype, which fails if
        // `push_own` builds the wrapper without `R.prototype`.
        let has_m = call_named(scope, ns, c"instHasM", &[]);
        assert!(
            has_m.is_boolean() && has_m.to_boolean(),
            "the constructed instance exposes its method `m` via R.prototype"
        );

        force_gc(scope);

        // r.m(5): the receiver is lowered as the leading borrow self, then the
        // explicit u32 arg. The recorded self-handle must equal CTOR_HANDLE.
        let before = log.borrow().self_handles.len();
        let m = call_named(scope, ns, c"callM", &[num(scope, 5)]);
        assert_eq!(as_number(m), 4242.0, "method r.m(5) lifts its u32 result");
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

/// Stage 4: the imported-resource prototype registry survives a relocating GC.
///
/// `push_own`'s imported arm re-reads `RESOURCE_PROTOTYPES` on every wrapper and
/// re-parents the new wrapper to the registered prototype. If that registry's
/// boxed `Heap` were not traced by `trace_resources`, a compacting GC would move
/// the prototype without updating the stored pointer and a later mint would crash
/// under `debugmozjs`.
///
/// The synthesis-time compacting zeal set in the `initialize_runtime` closure
/// above already relocates the prototype, so an un-traced `RESOURCE_PROTOTYPES`
/// holds a stale pointer by the time stage 3 builds its first wrapper. Fault
/// injection confirms it: dropping the `RESOURCE_PROTOTYPES` trace SIGSEGVs in
/// `set_prototype`.
///
/// This stage adds a positive exercise on top. It mints a wrapper, forces another
/// compacting GC, builds a second wrapper whose registry re-read happens after the
/// move, calls a prototype method on it, and asserts both wrappers share one
/// prototype identity.
#[cfg(feature = "debugmozjs")]
fn resource_prototype_registry_survives_relocation() {
    with_main_scope(|scope, ns| {
        // The instance stays rooted across the relocating GC so its prototype is
        // the live object that moves.
        let first = call_named(scope, ns, c"ctorR", &[num(scope, 11)]);
        let first_obj = Object::from_value(scope, first).expect("first instance is an object");

        // With an un-traced `RESOURCE_PROTOTYPES`, the stored prototype pointer is
        // stale after this.
        // SAFETY: the main scope wraps a valid context.
        unsafe { set_zeal(scope, 14, 1) };
        force_compacting_gc(scope);

        // The registry re-read now happens after relocation, so
        // `with_resource_prototype` reifies the moved `Heap`.
        let second = call_named(scope, ns, c"ctorR", &[num(scope, 22)]);
        let second_obj = Object::from_value(scope, second).expect("second instance is an object");

        // SAFETY: same context.
        unsafe { reset_zeal(scope) };

        // The second wrapper's method resolves through the (relocated) prototype.
        let has_m = call_named(scope, ns, c"instHasM", &[]);
        assert!(
            has_m.is_boolean() && has_m.to_boolean(),
            "the second wrapper exposes `m` via the relocated R.prototype"
        );
        let m = call_named(scope, ns, c"callM", &[num(scope, 7)]);
        assert_eq!(
            as_number(m),
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
