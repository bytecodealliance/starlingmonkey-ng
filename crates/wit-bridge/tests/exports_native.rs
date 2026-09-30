// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Native integration tests for runtime bootstrap and export dispatch.
//!
//! These run against the runtime [`initialize_runtime`] sets up. The cases are
//! grouped in a single test because it can be called only once per process.

use core_runtime::runtime::register_global_initializer;
use js::gc::scope::Scope;
use wit_bridge::exports::with_export_table;
use wit_bridge::{
    call_export, initialize_runtime, install_table, resolve_export, shutdown_runtime,
    with_main_scope, ExportSpec, ExportTable, Outcome, Resolved, ResultShape, TypeShape,
};
use wit_name_mapping::{mangle_name, InterfaceNames, Shape};

mod shared;
use shared::{as_number, as_string, force_gc, num};

/// The application's main module. It exports plain functions, an object with
/// the functions of an interface, and a class implementing a resource, with an
/// instance method and static methods.
const MAIN: &str = r#"
    export function add(a, b) { return a + b; }

    export function boom() { throw new Error("boom from add-side"); }

    // A result-typed export that throws a ComponentError: its `.payload`
    // becomes the `err` value.
    export function boomComponent() { throw new ComponentError({ reason: "wit-err" }); }

    // Result-typed exports that throw an `err` value without a ComponentError:
    // an Error whose message is an enum case, and a plain value.
    export function boomCase() { throw new Error("notFound"); }
    export function boomBare() { throw "bare payload"; }

    // Errors whose `message` is read by a getter that throws, and whose
    // `message` isn't a string.
    export function boomGetter() {
        const e = new Error("unused");
        Object.defineProperty(e, "message", {
            get() { throw new Error("message getter boom"); },
        });
        throw e;
    }
    export function boomNumber() {
        const e = new Error();
        e.message = 42;
        throw e;
    }

    export const myIface = {
        ping: function() { return "pong"; },
        doubleIt: function(v) { return v * 2; },
    };

    export class Counter {
        constructor(start) { this.value = start; }
        bump(by) { this.value += by; return this.value; }
        static origin() { return new Counter(0); }
        static label() { return "Counter"; }
    }

    // A class whose method is a getter that throws.
    export class Flaky {
        get bump() { throw new Error("getter boom"); }
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
    register_global_initializer(wit_bridge::add_to_global);

    // This application imports nothing, so synthesis does nothing.
    initialize_runtime(
        &core_runtime::config::RuntimeConfig::default(),
        MAIN,
        "main.js",
        None,
        |_scope| Ok(()),
    )
    .expect("runtime bootstrap failed");

    // A GC right after bootstrap, before anything resolves, would expose a
    // missing root on the stored main-module record as a crash.
    with_main_scope(|scope, _ns| force_gc(scope));

    resource_constructor_method_and_statics();
    component_error_becomes_err_when_result_typed();
    plain_error_traps_for_an_err_type_without_a_message_form();
    thrown_values_coerce_to_err_payloads();
    exception_traps_when_not_result_typed();
    export_table_resolves_every_kind();
    export_table_finds_the_providing_shape();
    missing_export_message_names_the_js_export();
    shutdown_runtime();
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
        // The value is already rooted on the call's scope.
        Outcome::Returned(v) => *v,
        Outcome::Threw(_) => panic!("expected a returned value, got a thrown one"),
    };
    as_string(scope, v)
}

/// A resource class resolves its `[constructor]`, `[method]`, and `[static]`
/// forms, and a method receives the receiver as `args[0]`.
fn resource_constructor_method_and_statics() {
    with_main_scope(|scope, ns| {
        // [constructor]counter → construct with [10] → an object.
        let ctor = resolve_export(scope, ns, "[constructor]counter")
            .expect("`[constructor]counter` resolves");
        assert!(
            matches!(ctor, wit_bridge::Resolved::Constructor { .. }),
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
            matches!(bump, wit_bridge::Resolved::Method { .. }),
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
            matches!(origin, wit_bridge::Resolved::Static { .. }),
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

/// A result-typed export that throws a `ComponentError` returns
/// `Outcome::Threw` with the error's `payload`, not the error itself.
fn component_error_becomes_err_when_result_typed() {
    with_main_scope(|scope, ns| {
        let boom = resolve_export(scope, ns, "boom-component").expect("`boom-component` resolves");
        force_gc(scope);
        let outcome = call_export(scope, boom, &[], &result_with_err(TypeShape::String))
            .expect("a ComponentError thrown by a result-typed export doesn't trap");
        match outcome {
            Outcome::Threw(v) => {
                // The value is the error's `payload`, `{ reason: "wit-err" }`,
                // which is already rooted.
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

/// A result-typed export that throws an `Error` that can't be converted into
/// its `err` type, here a `u32`, traps, which `call_export` reports as `Err`.
fn plain_error_traps_for_an_err_type_without_a_message_form() {
    with_main_scope(|scope, ns| {
        let boom = resolve_export(scope, ns, "boom").expect("`boom` resolves");
        force_gc(scope);
        let Err(err) = call_export(scope, boom, &[], &result_with_err(TypeShape::U32)) else {
            panic!("a non-ComponentError throw traps even for a result-typed export");
        };
        let message = err.message.expect("the trap carries a message");
        assert!(
            message.contains("boom from add-side")
                && message.contains("can only be converted into an `err` value of type `string`"),
            "the trap message includes the thrown text and the reason, got: {message}"
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

/// An exception thrown by a result-typed export is converted into an `err`
/// value: an `Error`'s message for a `string` `E`, the index of the case named
/// by its message for an `enum` `E`, the value itself for a value that isn't an
/// `Error`, and `undefined` for an `err` case without a value.
fn thrown_values_coerce_to_err_payloads() {
    with_main_scope(|scope, ns| {
        let threw = |name: &str, result: &ResultShape| {
            let resolved = resolve_export(scope, ns, name).expect("the export resolves");
            match call_export(scope, resolved, &[], result).expect("the exception is converted") {
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
            "the value of an `err` case without a value is `undefined`"
        );
        let payload = threw("boom-component", &no_payload);
        assert!(
            payload.is_undefined(),
            "the payload of a ComponentError is dropped for an `err` case without a value"
        );

        // A `message` that isn't a string is converted to one.
        let payload = threw("boom-number", &result_with_err(TypeShape::String));
        assert!(payload.is_string(), "the `err` value is a string");
        assert_eq!(as_string(scope, payload), "42");
        let cases = TypeShape::Variant(vec![("42".to_string(), None)]);
        let payload = threw("boom-number", &result_with_err(cases));
        let tag = js::Object::from_value(scope, payload)
            .expect("the `err` value is an object")
            .get_property(scope, c"tag")
            .expect("the `err` value has a tag");
        assert!(tag.is_string(), "the tag is a string");

        // An `Error` whose message names no case of an `enum` `E` isn't
        // converted, and the trap message gives the reason.
        let failed = |name: &str, result: &ResultShape| {
            let resolved = resolve_export(scope, ns, name).expect("the export resolves");
            let Err(err) = call_export(scope, resolved, &[], result) else {
                panic!("{name}() must trap");
            };
            assert!(!js::exception::is_pending(scope));
            err.message.expect("the trap has a message")
        };
        let cases = TypeShape::Enum(vec!["denied".to_string()]);
        let message = failed("boom-case", &result_with_err(cases));
        assert!(
            message.contains("notFound")
                && message.contains("names no case of the `err` type, which has the cases denied"),
            "{message}"
        );

        // A `message` getter that throws is reported.
        let message = failed("boom-getter", &result_with_err(TypeShape::String));
        assert!(
            message.contains("reading the error's `message` threw")
                && message.contains("message getter boom"),
            "{message}"
        );
    });
}

/// A throwing export whose result type isn't a `result` traps, which
/// `call_export` reports as `Err` with the thrown message.
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

/// Every shape returned by `possible_export_names` for `name` on `interface`,
/// for a world that exports only that function.
fn all_shapes(interface: &str, name: &str) -> Vec<Shape> {
    let item = wit_name_mapping::item_js_name(name);
    let items = [(Some(InterfaceNames::of(interface)), item)];
    wit_name_mapping::possible_export_names(&items, &[])
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
        // A table that is not installed keeps its entries alive across a
        // compacting GC.
        js::gc::prepare_for_full_gc(scope);
        js::gc::non_incremental_gc(scope, js::gc::GCOptions::Shrink, js::gc::GCReason::API);
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
        assert!(table.provided(0));

        // An optional export the main module leaves out builds as unprovided.
        let absent = ExportSpec {
            optional: true,
            ..spec(None, "not-exported")
        };
        let table =
            ExportTable::build(scope, ns, [absent]).expect("an optional export may be absent");
        assert!(!table.provided(0));

        let specs = [
            spec(None, "add"),
            spec(Some("my-iface"), "double-it"),
            spec(None, "[static]counter.label"),
        ];
        install_table(scope, ns, specs).expect("the table installs");
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

        // A getter that throws while the export is resolved is reported as the
        // exception, not as a missing export.
        let err = ExportTable::build(scope, ns, [spec(None, "[method]flaky.bump")])
            .err()
            .expect("a throwing getter fails the build");
        assert!(
            err.contains("resolving the WIT export `[method]flaky.bump` threw")
                && err.contains("getter boom"),
            "{err}"
        );
        assert!(!js::exception::is_pending(scope));
    });
}
