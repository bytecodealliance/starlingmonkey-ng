// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Native integration tests for runtime bootstrap and export dispatch.
//!
//! These run against the process-lifetime component-model runtime that
//! [`initialize_runtime`] sets up. Because that bootstrap is once-per-process
//! (it leaks the realm scope and stashes the context in a thread-local, on top
//! of the engine's once-per-process `JSEngine::init`), the whole suite lives in
//! a single `#[test]` function that bootstraps once and then exercises every
//! case against that runtime, kept in its own test binary, separate from
//! `sync_native`, so the two don't fight over the engine.

use component_model::exports::with_export_table;
use component_model::naming::{InterfaceNames, Shape};
use component_model::value::mangle_name;
use component_model::{
    call_export, initialize_runtime, install_table, resolve_export, with_main_scope, ExportSpec,
    ExportTable, Outcome, Resolved, ResultShape, TypeShape,
};
use core_runtime::runtime::register_global_initializer;
use js::gc::scope::Scope;

mod shared;
use shared::{as_number, as_string, force_gc, num};

/// The application's main module. It exports plain functions, an interface-style
/// namespace object (an exported object whose members are the interface's
/// functions, matching the shape of interface exports), and a class
/// with an instance method and a static method (matching the user-authored
/// resource classes export resolution looks up).
const MAIN: &str = r#"
    export function add(a, b) { return a + b; }

    export function boom() { throw new Error("boom from add-side"); }

    // A result-typed export that throws a ComponentError: its `.payload`
    // becomes the `err` value. (ComponentError is installed on the global by
    // `component_model::add_to_global`, registered before bootstrap below.)
    export function boomComponent() { throw new ComponentError({ reason: "wit-err" }); }

    // Result-typed exports throwing values that stand for an `err` payload
    // without being a ComponentError: an Error named after an enum case, and a
    // bare payload value.
    export function boomCase() { throw new Error("notFound"); }
    export function boomBare() { throw "bare payload"; }

    export const myIface = {
        ping: function() { return "pong"; },
        doubleIt: function(v) { return v * 2; },
        failing: function() { throw new Error("iface failure"); },
    };

    export class Counter {
        constructor(start) { this.value = start; }
        bump(by) { this.value += by; return this.value; }
        static origin() { return new Counter(0); }
        static label() { return "Counter"; }
    }

    // Items in the package layer, the interface layer and the versioned layer,
    // and one provided in two shapes.
    export const demoApp = { api: { greet() { return "package layer"; } } };
    export const twice = { hello() { return "interface layer"; } };
    export function hello() { return "bare"; }
    const v2 = { greet() { return "versioned layer"; } };
    export { v2 as "demo:app/api@2.0.0" };
"#;

#[test]
fn exports_native() {
    // `ComponentError` must be on the global before the main module evaluates,
    // exactly as the dylib registers it.
    register_global_initializer(component_model::add_to_global);

    // This application imports nothing, so synthesis is a no-op.
    initialize_runtime(
        &core_runtime::config::RuntimeConfig::default(),
        MAIN,
        "main.js",
        None,
        |_scope| Ok(()),
    )
    .expect("runtime bootstrap failed");

    // A GC right after bootstrap, before anything resolves, would expose a
    // missing root on the stashed main-module record as a crash.
    with_main_scope(|scope, _ns| force_gc(scope));

    bootstrap_namespace_is_reachable();
    plain_function_resolves_and_calls();
    interface_member_resolves_and_calls();
    resource_constructor_method_and_statics();
    component_error_becomes_err_when_result_typed();
    plain_error_traps_for_an_err_type_without_a_message_form();
    thrown_values_coerce_to_err_payloads();
    exception_traps_when_not_result_typed();
    export_table_resolves_every_kind();
    export_table_finds_the_providing_shape();
    missing_export_message_names_the_js_export();
}

/// Read a JS number out of a returned [`Outcome`], accepting int32 and double.
fn returned_number(outcome: &Outcome) -> f64 {
    let v = match outcome {
        Outcome::Returned(v) => v,
        Outcome::Threw(_) => panic!("expected a returned value, got a thrown one"),
    };
    as_number(v.get())
}

/// Read a UTF-8 string out of a returned [`Outcome`].
fn returned_string(scope: &Scope<'_>, outcome: &Outcome) -> String {
    let v = match outcome {
        // The carried value is already rooted on the call's scope.
        Outcome::Returned(v) => *v,
        Outcome::Threw(_) => panic!("expected a returned value, got a thrown one"),
    };
    as_string(scope, v)
}

/// The simplest end-to-end check: the main module's namespace is reachable and
/// its plain exported function is present and callable through `with_main_scope`.
fn bootstrap_namespace_is_reachable() {
    with_main_scope(|scope, ns| {
        let add = ns
            .get_property(scope, c"add")
            .expect("reading `add` from the namespace");
        assert!(
            add.get().is_object(),
            "`add` should be an exported function object"
        );
        let add_obj = js::Object::from_value(scope, add).unwrap();
        assert!(add_obj.is_callable(), "`add` should be callable");
    });
}

/// A plain (non-interface, non-resource) export resolves to `Plain` and calls
/// through `call_export` with the lowered args.
fn plain_function_resolves_and_calls() {
    with_main_scope(|scope, ns| {
        let resolved = resolve_export(scope, ns, "add").expect("`add` resolves");
        assert!(
            matches!(resolved, component_model::Resolved::Plain { .. }),
            "`add` should resolve to a plain function"
        );
        // GC between resolution and the call: a missing root on the resolved
        // handle would surface here under debugmozjs.
        force_gc(scope);
        let args = [num(scope, 2), num(scope, 3)];
        let outcome =
            call_export(scope, resolved, &args, &ResultShape::Plain).expect("`add` does not trap");
        assert_eq!(returned_number(&outcome), 5.0, "add(2, 3) == 5");
    });
}

/// An interface-qualified export resolves on the interface member object whose
/// key is the mangled interface name, then dispatches within it.
fn interface_member_resolves_and_calls() {
    with_main_scope(|scope, ns| {
        // `my-iface` mangles to `myIface`, the exported member object's key.
        let iface = ns.get_property(scope, c"myIface").unwrap();
        let iface = js::Object::from_value(scope, iface).unwrap();
        let ping = resolve_export(scope, &iface, "ping").expect("`ping` resolves");
        assert!(
            matches!(ping, component_model::Resolved::Plain { .. }),
            "interface member resolves to a plain function"
        );
        force_gc(scope);
        let outcome =
            call_export(scope, ping, &[], &ResultShape::Plain).expect("`ping` does not trap");
        assert_eq!(returned_string(scope, &outcome), "pong");

        let double_it = resolve_export(scope, &iface, "double-it").expect("`double-it` resolves");
        let args = [num(scope, 21)];
        let outcome =
            call_export(scope, double_it, &args, &ResultShape::Plain).expect("does not trap");
        assert_eq!(returned_number(&outcome), 42.0, "doubleIt(21) == 42");
    });
}

/// A resource class resolves its `[constructor]`, `[method]`, and `[static]`
/// forms, and a method receives the receiver as `args[0]`.
fn resource_constructor_method_and_statics() {
    with_main_scope(|scope, ns| {
        // [constructor]counter → construct with [10] → an object.
        let ctor = resolve_export(scope, ns, "[constructor]counter")
            .expect("`[constructor]counter` resolves");
        assert!(
            matches!(ctor, component_model::Resolved::Constructor { .. }),
            "resolves to a constructor"
        );
        force_gc(scope);
        let outcome = call_export(scope, ctor, &[num(scope, 10)], &ResultShape::Plain)
            .expect("constructor does not trap");
        let counter = match outcome {
            // The constructed object is already rooted on the call's scope.
            Outcome::Returned(v) => v,
            Outcome::Threw(_) => panic!("constructor threw"),
        };
        assert!(counter.is_object(), "construction yields an object");
        let counter_this = counter;

        // [method]counter.bump → Counter.prototype.bump, receiver as args[0].
        let bump = resolve_export(scope, ns, "[method]counter.bump")
            .expect("`[method]counter.bump` resolves");
        assert!(
            matches!(bump, component_model::Resolved::Method { .. }),
            "resolves to a method"
        );
        force_gc(scope);
        let args = [counter_this, num(scope, 5)];
        let outcome =
            call_export(scope, bump, &args, &ResultShape::Plain).expect("method does not trap");
        assert_eq!(returned_number(&outcome), 15.0, "bump(5) on start=10 == 15");

        // [static]counter.origin → a Counter object.
        let origin = resolve_export(scope, ns, "[static]counter.origin")
            .expect("`[static]counter.origin` resolves");
        assert!(
            matches!(origin, component_model::Resolved::Static { .. }),
            "resolves to a static method"
        );
        let outcome =
            call_export(scope, origin, &[], &ResultShape::Plain).expect("static does not trap");
        match outcome {
            Outcome::Returned(v) => assert!(v.is_object(), "origin() returns a Counter object"),
            Outcome::Threw(_) => panic!("origin() threw"),
        }

        // [static]counter.label → "Counter".
        let label = resolve_export(scope, ns, "[static]counter.label")
            .expect("`[static]counter.label` resolves");
        let outcome =
            call_export(scope, label, &[], &ResultShape::Plain).expect("static does not trap");
        assert_eq!(returned_string(scope, &outcome), "Counter");
    });
}

/// A result-typed export that throws a `ComponentError` yields `Outcome::Threw`
/// carrying the error's `.payload` (not the error object itself), with no Rust
/// `Err`.
fn component_error_becomes_err_when_result_typed() {
    with_main_scope(|scope, ns| {
        let boom = resolve_export(scope, ns, "boom-component").expect("`boom-component` resolves");
        force_gc(scope);
        let outcome = call_export(scope, boom, &[], &result_with_err(TypeShape::String))
            .expect("a result-typed export absorbs a ComponentError throw rather than trapping");
        match outcome {
            Outcome::Threw(v) => {
                // The carried value is the `.payload` (already rooted), here
                // `{ reason: "wit-err" }`.
                let obj = js::Object::from_value(scope, v).expect("payload is an object");
                let reason = obj
                    .get_property(scope, c"reason")
                    .expect("payload has reason");
                assert_eq!(as_string(scope, reason), "wit-err");
            }
            Outcome::Returned(_) => panic!("boomComponent() should have thrown"),
        }
        assert!(!js::exception::is_pending(scope));
    });
}

/// A result-typed export that throws a plain `Error` its `err` type cannot hold,
/// here a `u32`, is a trap rather than an `err` payload, surfaced as a Rust
/// `Err`.
fn plain_error_traps_for_an_err_type_without_a_message_form() {
    with_main_scope(|scope, ns| {
        let boom = resolve_export(scope, ns, "boom").expect("`boom` resolves");
        force_gc(scope);
        let Err(err) = call_export(scope, boom, &[], &result_with_err(TypeShape::U32)) else {
            panic!("a non-ComponentError throw traps even for a result-typed export");
        };
        let message = err.message.expect("the trap carries a message");
        assert!(
            message.contains("boom from add-side"),
            "the trap message includes the thrown text, got: {message}"
        );
        assert!(!js::exception::is_pending(scope));
    });
}

/// A `result` with no `ok` payload and an `err` payload of type `err`.
fn result_with_err(err: TypeShape) -> ResultShape {
    ResultShape::Result {
        ok: false,
        err: Some(std::rc::Rc::new(err)),
    }
}

/// A result-typed export's throw becomes the `err` payload it lowers as: an
/// `Error`'s message for a `string` `E`, the index of the case its message names
/// for an `enum` `E`, a bare value for itself, and anything at all for an `E`
/// without a payload.
fn thrown_values_coerce_to_err_payloads() {
    with_main_scope(|scope, ns| {
        let threw = |name: &str, result: &ResultShape| {
            let resolved = resolve_export(scope, ns, name).expect("the export resolves");
            match call_export(scope, resolved, &[], result).expect("the throw is absorbed") {
                Outcome::Threw(payload) => payload,
                Outcome::Returned(_) => panic!("{name}() should have thrown"),
            }
        };

        let payload = threw("boom", &result_with_err(TypeShape::String));
        assert_eq!(as_string(scope, payload), "boom from add-side");

        let cases = TypeShape::Enum(vec!["notFound".to_string(), "denied".to_string()]);
        let payload = threw("boom-case", &result_with_err(cases));
        assert_eq!(payload.to_int32(), 0, "`notFound` is the enum's first case");

        let payload = threw("boom-bare", &result_with_err(TypeShape::String));
        assert_eq!(as_string(scope, payload), "bare payload");

        let no_payload = ResultShape::Result {
            ok: false,
            err: None,
        };
        let payload = threw("boom", &no_payload);
        assert!(
            payload.is_undefined(),
            "a payload-less `err` arm carries nothing"
        );

        // An `Error` whose message names no case of an `enum` `E` is not absorbed.
        let cases = TypeShape::Enum(vec!["denied".to_string()]);
        let boom = resolve_export(scope, ns, "boom-case").expect("the export resolves");
        assert!(
            call_export(scope, boom, &[], &result_with_err(cases)).is_err(),
            "an Error naming no enum case traps"
        );
        assert!(!js::exception::is_pending(scope));
    });
}

/// A throwing export whose declared result is NOT a `result` traps: the call
/// returns a Rust `Err` carrying the thrown message.
fn exception_traps_when_not_result_typed() {
    with_main_scope(|scope, ns| {
        let boom = resolve_export(scope, ns, "boom").expect("`boom` resolves");
        force_gc(scope);
        let Err(err) = call_export(scope, boom, &[], &ResultShape::Plain) else {
            panic!("a non-result export propagates the throw as a trap");
        };
        let message = err.message.expect("the trap carries a message");
        assert!(
            message.contains("boom from add-side"),
            "the trap message includes the thrown text, got: {message}"
        );
    });
}

/// An export spec for `name` on `interface`, with a plain result, provided in
/// the interface layer, or bare for a world-level export.
fn spec(interface: Option<&str>, name: &str) -> ExportSpec {
    let shapes = match interface {
        Some(interface) => vec![Shape::Interface(mangle_name(interface))],
        None => vec![Shape::Bare],
    };
    shaped_spec(interface, name, shapes)
}

/// An export spec for `name` on `interface`, with a plain result, provided in
/// one of `shapes`.
fn shaped_spec(interface: Option<&str>, name: &str, shapes: Vec<Shape>) -> ExportSpec {
    ExportSpec {
        interface: interface.map(str::to_string),
        name: name.to_string(),
        shapes,
        result: ResultShape::Plain,
        optional: false,
    }
}

/// Every shape `naming::plan` allows for `name` on `interface`, for a world
/// exporting just that.
fn all_shapes(interface: &str, name: &str) -> Vec<Shape> {
    let item = component_model::exports::export_item(name);
    let items = [(Some(InterfaceNames::of(interface)), item)];
    component_model::naming::plan(&items, &[])
        .unwrap()
        .remove(0)
}

/// `ExportTable::build` finds each export in the one shape that provides it,
/// and fails for an export two shapes provide.
fn export_table_finds_the_providing_shape() {
    with_main_scope(|scope, ns| {
        let specs = [
            shaped_spec(
                Some("demo:app/api"),
                "greet",
                all_shapes("demo:app/api", "greet"),
            ),
            shaped_spec(
                Some("demo:app/api@2.0.0"),
                "greet",
                vec![Shape::Versioned("demo:app/api@2.0.0".to_string())],
            ),
        ];
        let table = ExportTable::build(scope, ns, specs).expect("every export resolves");
        for (index, expected) in [(0, "package layer"), (1, "versioned layer")] {
            let outcome = call_export(
                scope,
                table.resolved(scope, index),
                &[],
                &ResultShape::Plain,
            )
            .expect("`greet` does not trap");
            assert_eq!(returned_string(scope, &outcome), expected);
        }

        let err = ExportTable::build(
            scope,
            ns,
            [shaped_spec(
                Some("demo:app/twice"),
                "hello",
                all_shapes("demo:app/twice", "hello"),
            )],
        )
        .err()
        .expect("an export provided twice fails the build");
        assert!(
            err.contains(
                "provides the WIT export `demo:app/twice#hello` twice, as `twice.hello` and as \
                 `hello`"
            ),
            "{err}"
        );
        assert!(!js::exception::is_pending(scope));
    });
}

/// `ExportTable::build` resolves a plain export, an interface member, and a
/// resource's constructor, method and static in order, and `install_table`
/// makes the same table reachable through `with_export_table`, whose entries
/// are callable.
fn export_table_resolves_every_kind() {
    with_main_scope(|scope, ns| {
        let specs = [
            spec(None, "add"),
            spec(Some("my-iface"), "ping"),
            spec(None, "[constructor]counter"),
            spec(None, "[method]counter.bump"),
            spec(None, "[static]counter.label"),
        ];
        let table = ExportTable::build(scope, ns, specs).expect("every export resolves");
        assert_eq!(table.len(), 5);
        assert!(matches!(table.resolved(scope, 0), Resolved::Plain { .. }));
        assert!(matches!(table.resolved(scope, 1), Resolved::Plain { .. }));
        assert!(matches!(
            table.resolved(scope, 2),
            Resolved::Constructor { .. }
        ));
        assert!(matches!(table.resolved(scope, 3), Resolved::Method { .. }));
        assert!(matches!(table.resolved(scope, 4), Resolved::Static { .. }));
        assert_eq!(table.result(0), ResultShape::Plain);

        let specs = [
            spec(None, "add"),
            spec(Some("my-iface"), "double-it"),
            spec(None, "[static]counter.label"),
        ];
        install_table(scope, ns, specs).expect("the table installs");
        // A GC between installation and use: the table's entries are traced
        // through the crate tracer, so a missing root would crash here.
        force_gc(scope);
        with_export_table(|table| {
            assert_eq!(table.len(), 3);
            let outcome = call_export(
                scope,
                table.resolved(scope, 0),
                &[num(scope, 20), num(scope, 22)],
                &ResultShape::Plain,
            )
            .expect("`add` does not trap");
            assert_eq!(returned_number(&outcome), 42.0);
            let outcome = call_export(
                scope,
                table.resolved(scope, 1),
                &[num(scope, 4)],
                &ResultShape::Plain,
            )
            .expect("`doubleIt` does not trap");
            assert_eq!(returned_number(&outcome), 8.0);
            let outcome = call_export(scope, table.resolved(scope, 2), &[], &ResultShape::Plain)
                .expect("`label` does not trap");
            assert_eq!(returned_string(scope, &outcome), "Counter");
        });
    });
}

/// A spec whose member is missing makes `ExportTable::build` and
/// `install_table` fail with a message naming the JS export the main module
/// must provide and the statement that would define it, and leaves no
/// exception pending.
fn missing_export_message_names_the_js_export() {
    with_main_scope(|scope, ns| {
        let err = ExportTable::build(scope, ns, [spec(Some("my-iface"), "nope")])
            .err()
            .expect("a missing interface member fails the build");
        assert!(
            err.contains(
                "does not provide a function `nope` for the WIT export `my-iface#nope`. Export it \
                 as `myIface.nope`, for example: export const myIface = { nope(...) { ... } };"
            ),
            "{err}"
        );
        let shapes = all_shapes("demo:app/api", "nope");
        let err = ExportTable::build(
            scope,
            ns,
            [shaped_spec(Some("demo:app/api"), "nope", shapes)],
        )
        .err()
        .expect("a missing export fails the build");
        assert!(
            err.contains(
                "Export it as `demoApp.api.nope` or `api.nope` or `nope`, for example: export \
                 function nope(...) { ... }"
            ),
            "{err}"
        );
        assert!(!js::exception::is_pending(scope));

        let err = install_table(
            scope,
            ns,
            [spec(None, "add"), spec(None, "[method]counter.gone")],
        )
        .expect_err("a missing method fails the install");
        assert!(
            err.contains(
                "does not provide a class `Counter` with a method `gone` for the WIT export \
                 `[method]counter.gone`. Export it as `Counter`, for example: export class \
                 Counter { gone(...) { ... } }"
            ),
            "{err}"
        );
        assert!(!js::exception::is_pending(scope));

        let err = ExportTable::build(scope, ns, [spec(None, "[constructor]missing")])
            .err()
            .expect("a missing class fails the build");
        assert!(
            err.contains("a class `Missing` with a constructor"),
            "{err}"
        );
        assert!(!js::exception::is_pending(scope));
    });
}
