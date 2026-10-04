// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for typed variadic arguments via `RestArgs<T>`.
//!
//! `RestArgs<T>` collects every JS argument from the position where it appears
//! (after the fixed parameters) into a `Vec<T>`.
//! It is valid wherever a callable takes arguments: `#[method]`,
//! `#[static_method]`, `#[constructor]`, and the free functions exposed by
//! `#[jsmodule]`, `#[jsglobals]`, `#[jsnamespace]`, and `#[webidl_namespace]`
//! (see the [`constructor_tests`] and [`free_fn_tests`] modules).
//!
//! `T` must implement [`js::conversion::FromJSVal`] and, for GC types, be rooted.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::jsclass;
use core_runtime::jsmethods;
use core_runtime::test_util::{eval_with_setup, throws_with_setup};

#[jsclass]
struct Variadic {}

#[jsmethods]
impl Variadic {
    #[constructor]
    fn construct() -> Self {
        Self {}
    }

    /// Typed `f64` variadic. Exercises `.iter()` and the lenient `ToNumber`
    /// coercion that `f64::from_jsval` applies to each argument.
    #[static_method]
    fn sum(rest: RestArgs<f64>) -> f64 {
        rest.iter().sum()
    }

    /// A fixed leading parameter followed by variadic strings. Exercises
    /// collection starting after the fixed args and `Deref`-to-slice (`join`).
    #[method]
    fn join(&self, sep: String, rest: RestArgs<String>) -> String {
        rest.join(sep.as_str())
    }

    /// Exercises `RestArgs::len`.
    #[method]
    fn count(&self, rest: RestArgs<String>) -> u32 {
        rest.len() as u32
    }

    /// Exercises `RestArgs::is_empty`.
    #[method]
    fn is_empty_rest(&self, rest: RestArgs<String>) -> bool {
        rest.is_empty()
    }

    /// Exercises the by-value `IntoIterator` impl.
    #[static_method]
    fn any_true(rest: RestArgs<bool>) -> bool {
        rest.into_iter().any(|b| b)
    }

    /// Variadic of class newtypes. A wrong-typed argument drives the
    /// `ConversionError::Failure` arm of the collection loop (a thrown
    /// `TypeError` from the failed `cast`), distinct from the `ExnPending`
    /// arm that a `String` conversion of a `Symbol` takes.
    #[static_method]
    fn count_selves(rest: RestArgs<Variadic>) -> u32 {
        rest.len() as u32
    }

    /// Typed `i32` variadic. Integer element types convert with
    /// `ConversionBehavior::Default` (`ToInt32`: truncate toward zero, then wrap
    /// modulo 2^32), not the `()` config that `f64`/`String` use.
    #[static_method]
    fn sum_ints(rest: RestArgs<i32>) -> i32 {
        rest.iter().sum()
    }

    /// A wide integer element type, confirming the fix spans `i8`..`u64` rather
    /// than `i32` alone. Returned as `f64` so JS sees the exact small sums.
    #[static_method]
    fn sum_longs(rest: RestArgs<i64>) -> f64 {
        rest.iter().sum::<i64>() as f64
    }
}

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        Variadic::add_to_global(scope, global);
    });
}

fn eval(code: &str) -> String {
    eval_with_setup(setup, code)
}

fn throws(code: &str) -> bool {
    throws_with_setup(setup, code)
}

// ============================================================================
// Typed numeric collection
// ============================================================================

#[test]
fn sum_collects_numbers() {
    assert_eq!(eval("Variadic.sum(1, 2, 3)"), "6");
}

#[test]
fn sum_handles_floats() {
    assert_eq!(eval("Variadic.sum(1.5, 2.5)"), "4");
}

#[test]
fn sum_with_no_args_is_empty() {
    assert_eq!(eval("Variadic.sum()"), "0");
}

#[test]
fn sum_with_single_arg() {
    assert_eq!(eval("Variadic.sum(42)"), "42");
}

/// Each element is converted with the lenient `ToNumber`, so a numeric string
/// and a boolean coerce rather than throwing.
#[test]
fn sum_coerces_each_element() {
    assert_eq!(eval("Variadic.sum(1, '2', true)"), "4");
}

// ============================================================================
// Fixed parameter + variadic strings
// ============================================================================

#[test]
fn join_collects_after_fixed_param() {
    assert_eq!(eval("new Variadic().join('-', 'a', 'b', 'c')"), "a-b-c");
}

#[test]
fn join_with_empty_rest_is_empty_string() {
    assert_eq!(eval("new Variadic().join('-')"), "");
}

#[test]
fn join_with_single_rest_element() {
    assert_eq!(eval("new Variadic().join('-', 'solo')"), "solo");
}

/// The fixed `sep` argument is not collected into the rest; only arguments at
/// or beyond the rest position are.
#[test]
fn join_does_not_swallow_the_separator() {
    assert_eq!(eval("new Variadic().join(',', '1', '2')"), "1,2");
}

// ============================================================================
// len() / is_empty()
// ============================================================================

#[test]
fn count_reports_rest_length() {
    assert_eq!(eval("new Variadic().count('a', 'b', 'c', 'd')"), "4");
    assert_eq!(eval("new Variadic().count()"), "0");
}

#[test]
fn is_empty_rest_reflects_argument_count() {
    assert_eq!(eval("new Variadic().isEmptyRest()"), "true");
    assert_eq!(eval("new Variadic().isEmptyRest('x')"), "false");
}

// ============================================================================
// Boolean variadic (IntoIterator by value)
// ============================================================================

#[test]
fn any_true_finds_a_truthy_argument() {
    assert_eq!(eval("Variadic.anyTrue(false, false, true)"), "true");
}

#[test]
fn any_true_is_false_when_all_falsy() {
    assert_eq!(eval("Variadic.anyTrue(false, false)"), "false");
    assert_eq!(eval("Variadic.anyTrue()"), "false");
}

/// `ToBoolean` coerces each argument; truthy values count.
#[test]
fn any_true_coerces_arguments() {
    assert_eq!(eval("Variadic.anyTrue(0, '', 'non-empty')"), "true");
}

// ============================================================================
// Variadic of class newtypes
// ============================================================================

#[test]
fn count_selves_collects_instances() {
    assert_eq!(
        eval("Variadic.countSelves(new Variadic(), new Variadic())"),
        "2"
    );
    assert_eq!(eval("Variadic.countSelves()"), "0");
}

// ============================================================================
// Integer variadics (ConversionBehavior config)
// ============================================================================

#[test]
fn sum_ints_collects_integers() {
    assert_eq!(eval("Variadic.sumInts(1, 2, 3)"), "6");
    assert_eq!(eval("Variadic.sumInts()"), "0");
}

/// Each element runs through `ToInt32`: a non-integer number truncates toward
/// zero and a numeric string coerces.
#[test]
fn sum_ints_applies_to_int32() {
    assert_eq!(eval("Variadic.sumInts(1, 2.7, '3')"), "6");
    assert_eq!(eval("Variadic.sumInts(-1.9)"), "-1");
}

/// `ToInt32` wraps modulo 2^32, so 2^32 maps to 0.
#[test]
fn sum_ints_wraps_modulo_2_32() {
    assert_eq!(eval("Variadic.sumInts(4294967296)"), "0");
    assert_eq!(eval("Variadic.sumInts(4294967297)"), "1");
}

#[test]
fn sum_longs_handles_wide_integers() {
    assert_eq!(eval("Variadic.sumLongs(1000, 2000, 3000)"), "6000");
    assert_eq!(eval("Variadic.sumLongs()"), "0");
}

// ============================================================================
// .length excludes the rest parameter
// ============================================================================

/// A function's `.length` counts required (non-`Option`, non-`RestArgs`)
/// parameters, so the variadic tail never contributes.
#[test]
fn length_ignores_rest_parameter() {
    // Only the rest param: length 0.
    assert_eq!(eval("Variadic.sum.length"), "0");
    assert_eq!(eval("Variadic.anyTrue.length"), "0");
    assert_eq!(eval("Variadic.prototype.count.length"), "0");
    // One fixed param ahead of the rest: length 1.
    assert_eq!(eval("Variadic.prototype.join.length"), "1");
}

// ============================================================================
// Conversion-error propagation
// ============================================================================

/// `String::from_jsval` runs `ToString`, which throws on a `Symbol`. The
/// pending exception propagates out of the collection loop (the `ExnPending`
/// arm), so the call throws.
#[test]
fn string_rest_throws_on_symbol() {
    assert!(throws("new Variadic().count(Symbol('x'))"));
}

/// A wrong-typed argument to a class-newtype rest fails the `cast`, producing
/// a `ConversionError::Failure` that is thrown as a `TypeError` (the
/// `throw_type_error` arm).
#[test]
fn class_rest_throws_on_wrong_type() {
    assert!(throws("Variadic.countSelves({})"));
    assert!(throws("Variadic.countSelves(new Variadic(), 42)"));
}

// ============================================================================
// RestArgs<HandleValue> — raw rooted values
// ============================================================================
//
// `HandleValue` collects each argument as a handle that aliases the
// engine-rooted argv slot it was read from, so a `Vec<HandleValue>` held across
// a moving GC stays valid: the collector traces the argv on the VM stack and
// updates the slots in place. (An owned `RestArgs<Value>` would copy the JSVals
// out into untraced storage and is rejected by the macro for that reason.)

mod handle_value_tests {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use core_runtime::test_util::eval_with_setup;
    use js::prelude::{FromJSVal, HandleValue, Scope};

    #[jsclass]
    struct ValueVariadic {}

    #[jsmethods]
    impl ValueVariadic {
        #[constructor]
        fn construct() -> Self {
            Self {}
        }

        /// Collect raw `any` values into a `Vec<HandleValue>` and report the
        /// count — confirms the element type is accepted at all.
        #[static_method]
        fn count_values(rest: RestArgs<HandleValue<'_>>) -> u32 {
            rest.len() as u32
        }

        /// Hold the collected handles across a forced compacting collection,
        /// then read property `x` from each object. If the handles didn't alias
        /// the rooted argv, the relocated objects would be read through stale
        /// pointers and this would crash or return garbage.
        #[static_method]
        fn sum_x_after_gc(scope: &Scope<'_>, rest: RestArgs<HandleValue<'_>>) -> i32 {
            // Force a full, compacting collection with the values still held.
            js::gc::prepare_for_full_gc(scope);
            js::gc::non_incremental_gc(scope, js::gc::GCOptions::Shrink, js::gc::GCReason::API);

            let mut total = 0;
            for v in rest.iter() {
                let obj =
                    js::Object::from_jsval(scope, *v, ()).expect("argument must be an object");
                let xv = obj.get_property(scope, c"x").expect("missing property x");
                total += if xv.is_int32() {
                    xv.to_int32()
                } else {
                    xv.to_double() as i32
                };
            }
            total
        }
    }

    fn setup() {
        core_runtime::runtime::register_global_initializer(|scope, global| {
            ValueVariadic::add_to_global(scope, global);
        });
    }

    fn eval(code: &str) -> String {
        eval_with_setup(setup, code)
    }

    #[test]
    fn collects_raw_values() {
        assert_eq!(eval("ValueVariadic.countValues(1, 'two', {}, null)"), "4");
        assert_eq!(eval("ValueVariadic.countValues()"), "0");
    }

    /// Calling `sumXAfterGc` runs a real compacting GC mid-method while the
    /// handles are live; the objects are freshly allocated (movable) and read
    /// back afterwards.
    #[test]
    fn handles_survive_compacting_gc() {
        assert_eq!(
            eval("ValueVariadic.sumXAfterGc({x: 1}, {x: 2}, {x: 3})"),
            "6"
        );
        assert_eq!(eval("ValueVariadic.sumXAfterGc()"), "0");
    }

    /// The same path under GC zeal mode 14 (compact-on-every-GC), which forces
    /// relocation deterministically rather than relying on the shrinking GC to
    /// move objects on this build.
    #[cfg(feature = "debugmozjs")]
    #[test]
    fn handles_survive_under_gc_zeal() {
        use core_runtime::config::RuntimeConfig;
        use core_runtime::runtime::Runtime;
        use js::gc::SetGCZeal;

        setup();
        let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        let scope = rt.default_global();

        // Mode 14 (Compact): every GC compacts, moving heap objects.
        unsafe { SetGCZeal(scope.raw_cx_no_gc(), 14, 1) };

        let val = js::compile::evaluate(
            &scope,
            "ValueVariadic.sumXAfterGc({x: 10}, {x: 20}, {x: 30})",
        )
        .expect("evaluation threw");
        assert_eq!(val.to_int32(), 60);

        unsafe { SetGCZeal(scope.raw_cx_no_gc(), 0, 0) };
    }
}

// ============================================================================
// `RestArgs` on constructors
//
// `parse_method_info` has always collected the rest parameter for every method
// kind, but `gen_constructor_body` dropped it on the floor, so a constructor
// taking `RestArgs` failed to compile. Constructors now go through the same
// collection as methods.
// ============================================================================

mod constructor_tests {
    use core_runtime::test_util::eval_with_setup;
    use core_runtime::{jsclass, jsmethods};

    // No `use js::class::RestArgs` here on purpose: the macro rewrites the
    // parameter type to its fully-qualified path, so the unqualified name in
    // the signatures below resolves without an import. This module failing to
    // compile means that rewrite regressed.

    #[jsclass]
    struct Tally {
        total: f64,
        label: String,
    }

    #[jsmethods]
    impl Tally {
        /// Variadic-only constructor: collection starts at argument 0.
        #[constructor]
        fn construct(rest: RestArgs<f64>) -> Self {
            Self {
                total: rest.iter().sum(),
                label: String::new(),
            }
        }

        #[getter]
        fn total(&self) -> f64 {
            self.data().total
        }

        #[getter]
        fn label(&self) -> String {
            self.data().label.clone()
        }
    }

    #[jsclass]
    struct Labelled {
        label: String,
        count: u32,
    }

    #[jsmethods]
    impl Labelled {
        /// A fixed leading parameter followed by a variadic tail, so collection
        /// must start at index 1 rather than 0.
        #[constructor]
        fn construct(label: String, rest: RestArgs<String>) -> Self {
            Self {
                label,
                count: rest.len() as u32,
            }
        }

        #[getter]
        fn label(&self) -> String {
            self.data().label.clone()
        }

        #[getter]
        fn count(&self) -> u32 {
            self.data().count
        }
    }

    fn setup() {
        core_runtime::runtime::register_global_initializer(|scope, global| {
            Tally::add_to_global(scope, global);
            Labelled::add_to_global(scope, global);
        });
    }

    fn eval(code: &str) -> String {
        eval_with_setup(setup, code)
    }

    #[test]
    fn constructor_collects_variadic_args() {
        assert_eq!(eval("new Tally(1, 2, 3).total"), "6");
    }

    #[test]
    fn constructor_with_no_variadic_args() {
        assert_eq!(eval("new Tally().total"), "0");
    }

    #[test]
    fn constructor_collects_after_fixed_params() {
        assert_eq!(eval("new Labelled('a', 'x', 'y', 'z').count"), "3");
        assert_eq!(eval("new Labelled('a', 'x', 'y', 'z').label"), "a");
    }

    #[test]
    fn constructor_with_fixed_param_and_empty_tail() {
        assert_eq!(eval("new Labelled('a').count"), "0");
    }
}

// ============================================================================
// `RestArgs` on free functions
//
// `#[jsmodule]`, `#[jsglobals]`, `#[jsnamespace]`, and `#[webidl_namespace]`
// share `parse_free_fn_export`/`gen_free_fn_native`, which previously ignored
// `RestArgs` entirely — the parameter was treated as an ordinary one and
// failed `FromJSVal`. All four now use the same collection as methods.
// ============================================================================

mod free_fn_tests {
    use core_runtime::config::RuntimeConfig;
    use core_runtime::module::evaluate_module;
    use core_runtime::runtime::Runtime;
    use core_runtime::test_util::eval_with_setup;
    use core_runtime::{jsglobals, jsmodule, jsnamespace, webidl_namespace};
    use js::class::RestArgs;
    use js::conversion::FromJSVal;

    #[jsmodule]
    mod rest_module {
        pub fn sum_all(rest: RestArgs<f64>) -> f64 {
            rest.iter().sum()
        }

        pub fn join_all(sep: String, rest: RestArgs<String>) -> String {
            rest.join(sep.as_str())
        }
    }

    #[jsglobals]
    mod rest_globals {
        pub fn global_sum(rest: RestArgs<f64>) -> f64 {
            rest.iter().sum()
        }

        pub fn global_join(sep: String, rest: RestArgs<String>) -> String {
            rest.join(sep.as_str())
        }
    }

    #[jsnamespace(name = "restNs")]
    mod rest_ns {
        use js::gc::scope::Scope;

        pub fn ns_sum(rest: RestArgs<f64>) -> f64 {
            rest.iter().sum()
        }

        /// Combines the `scope` passthrough with a variadic tail, confirming
        /// the two parameter filters compose.
        pub fn ns_count(scope: &Scope<'_>, rest: RestArgs<String>) -> u32 {
            let _ = scope;
            rest.len() as u32
        }
    }

    #[webidl_namespace(name = "RestWebIDL")]
    mod rest_webidl_ns {
        pub fn widl_sum(rest: RestArgs<i32>) -> i32 {
            rest.iter().sum()
        }
    }

    fn setup() {
        core_runtime::runtime::register_global_initializer(|scope, global| {
            rest_globals::add_to_global(scope, global);
            rest_ns::add_to_global(scope, global);
            rest_webidl_ns::add_to_global(scope, global);
            rest_module::register(scope);
        });
    }

    fn eval(code: &str) -> String {
        eval_with_setup(setup, code)
    }

    fn eval_module(body: &str) -> String {
        setup();
        let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        let scope = rt.default_global();
        let source = format!("import * as m from \"restModule\";\nglobalThis._result = {body};");
        // SAFETY: `scope` outlives the evaluation, and the module registry was
        // populated by `setup` before any JS ran.
        unsafe { evaluate_module(&scope, &source, "rest_test.mjs") }.expect("module eval failed");
        let val = js::compile::evaluate_with_filename(&scope, "globalThis._result", "read.js", 1)
            .expect("readback failed");
        String::from_jsval(&scope, val, ()).expect("null string")
    }

    // --- #[jsmodule] ---

    #[test]
    fn module_fn_collects_variadic_args() {
        assert_eq!(eval_module("m.sumAll(1, 2, 3)"), "6");
        assert_eq!(eval_module("m.sumAll()"), "0");
    }

    #[test]
    fn module_fn_collects_after_fixed_param() {
        assert_eq!(eval_module("m.joinAll('-', 'a', 'b', 'c')"), "a-b-c");
    }

    // --- #[jsglobals] ---

    #[test]
    fn global_fn_collects_variadic_args() {
        assert_eq!(eval("globalSum(1, 2, 3, 4)"), "10");
        assert_eq!(eval("globalSum()"), "0");
    }

    #[test]
    fn global_fn_collects_after_fixed_param() {
        assert_eq!(eval("globalJoin('+', 'x', 'y')"), "x+y");
    }

    // --- #[jsnamespace] / #[webidl_namespace] ---

    #[test]
    fn namespace_fn_collects_variadic_args() {
        assert_eq!(eval("restNs.nsSum(2, 4)"), "6");
        assert_eq!(eval("restNs.nsSum()"), "0");
    }

    #[test]
    fn namespace_fn_with_scope_and_variadic() {
        assert_eq!(eval("restNs.nsCount('a', 'b', 'c')"), "3");
    }

    #[test]
    fn webidl_namespace_fn_collects_variadic_args() {
        assert_eq!(eval("RestWebIDL.widlSum(1, 2, 3, 4, 5)"), "15");
    }

    /// A variadic tail is not a declared parameter, so it must not inflate
    /// `fn.length` — the same rule `length_ignores_rest_parameter` pins for
    /// methods, checked here because free functions compute `nargs` on a
    /// different path.
    #[test]
    fn length_ignores_rest_parameter_on_free_fns() {
        assert_eq!(eval("globalSum.length"), "0");
        assert_eq!(eval("globalJoin.length"), "1");
        assert_eq!(eval("restNs.nsSum.length"), "0");
        assert_eq!(eval("restNs.nsCount.length"), "0");
        assert_eq!(eval("RestWebIDL.widlSum.length"), "0");
    }

    /// The unqualified `RestArgs` in each mod block above is rewritten to the
    /// fully-qualified path by the macro, exactly as it is for methods — this
    /// compiles only if that rewrite reaches the free-function paths.
    #[test]
    fn rest_args_type_is_rewritten_in_signatures() {
        assert_eq!(rest_module::sum_all(RestArgs::new(vec![1.0, 2.0])), 3.0);
        assert_eq!(rest_globals::global_sum(RestArgs::new(vec![4.0])), 4.0);
    }
}

// ============================================================================
// `RestArgs` on a promise-returning operation
//
// For `Result<Promise<'_>, ExnThrown>` returns, WebIDL §3.7.7 says a failed
// argument conversion must *reject* the returned promise rather than throw
// synchronously. `emit_native_fn` achieves that for fixed parameters by moving
// their extraction inside a rejecting closure; the variadic tail is collected
// in the same place, so it behaves the same way.
// ============================================================================

mod promise_return_tests {
    use core_runtime::runtime::{clear_global_initializers, register_global_initializer};
    use core_runtime::test_util::eval_out_with_setup;
    use core_runtime::{jsclass, jsmethods};
    use js::error::ExnThrown;
    use js::gc::scope::Scope;
    use js::Promise;

    #[jsclass]
    struct Waiter {}

    #[jsmethods]
    impl Waiter {
        #[constructor]
        fn construct() -> Self {
            Self {}
        }

        /// A promise-returning operation whose arguments are all variadic.
        #[method]
        fn join_async<'r>(
            &self,
            scope: &'r Scope<'_>,
            rest: RestArgs<String>,
        ) -> Result<Promise<'r>, ExnThrown> {
            Promise::new_resolved_with_value(scope, rest.join(","))
        }

        /// The same, behind a fixed leading parameter, so the tail is collected
        /// from index 1 inside the closure.
        #[method]
        fn join_async_after<'r>(
            &self,
            scope: &'r Scope<'_>,
            sep: String,
            rest: RestArgs<String>,
        ) -> Result<Promise<'r>, ExnThrown> {
            Promise::new_resolved_with_value(scope, rest.join(sep.as_str()))
        }
    }

    /// Evaluate `code`, drain microtasks, and return `String(globalThis.__out)`.
    fn run(code: &str) -> String {
        eval_out_with_setup(
            || {
                clear_global_initializers();
                register_global_initializer(|scope, global| {
                    Waiter::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    #[test]
    fn variadic_promise_op_resolves() {
        assert_eq!(
            run(r#"
                globalThis.__out = "pending";
                new Waiter().joinAsync("a", "b").then(v => { globalThis.__out = v; });
            "#),
            "a,b"
        );
    }

    /// A `Symbol` can't convert to `String`, so collecting the tail fails. That
    /// must surface as a rejection, not a synchronous throw — matching how a bad
    /// *fixed* argument to the same operation behaves.
    #[test]
    fn bad_variadic_element_rejects_rather_than_throwing() {
        assert_eq!(
            run(r#"
                globalThis.__out = "pending";
                let threw = false;
                let p;
                try { p = new Waiter().joinAsync(Symbol()); } catch (e) { threw = true; }
                Promise.resolve(p).then(
                    () => { globalThis.__out = `threw=${threw},resolved`; },
                    e => { globalThis.__out = `threw=${threw},rejected:${e.constructor.name}`; },
                );
            "#),
            "threw=false,rejected:TypeError"
        );
    }

    /// The same, with the tail behind a fixed parameter.
    #[test]
    fn bad_variadic_element_after_fixed_param_rejects() {
        assert_eq!(
            run(r#"
                globalThis.__out = "pending";
                let threw = false;
                let p;
                try { p = new Waiter().joinAsyncAfter("-", Symbol()); } catch (e) { threw = true; }
                Promise.resolve(p).then(
                    () => { globalThis.__out = `threw=${threw},resolved`; },
                    e => { globalThis.__out = `threw=${threw},rejected:${e.constructor.name}`; },
                );
            "#),
            "threw=false,rejected:TypeError"
        );
    }

    /// The pre-existing behaviour for a bad *fixed* argument, as the baseline
    /// the variadic tail has to match.
    #[test]
    fn bad_fixed_arg_rejects_rather_than_throwing() {
        assert_eq!(
            run(r#"
                globalThis.__out = "pending";
                let threw = false;
                let p;
                try { p = new Waiter().joinAsyncAfter(Symbol()); } catch (e) { threw = true; }
                Promise.resolve(p).then(
                    () => { globalThis.__out = `threw=${threw},resolved`; },
                    e => { globalThis.__out = `threw=${threw},rejected:${e.constructor.name}`; },
                );
            "#),
            "threw=false,rejected:TypeError"
        );
    }
}
