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

    /// Typed `i64` variadic. The sum is returned as `f64` so JS sees the exact small sums.
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

/// Each element is converted with the lenient `ToNumber`, so a numeric string and a boolean
/// coerce rather than throwing.
#[test]
fn numeric_rest_args_collect_and_coerce() {
    assert_eq!(
        eval("[Variadic.sum(1.5, '2', true), Variadic.sum()].join()"),
        "4.5,0"
    );
}

/// The fixed `sep` argument is not collected into the rest.
#[test]
fn rest_args_collect_after_fixed_params() {
    assert_eq!(
        eval("const v = new Variadic(); [v.join('-', 'a', 'b'), v.join('-')].join('|')"),
        "a-b|"
    );
}

#[test]
fn rest_args_report_len_and_is_empty() {
    assert_eq!(
        eval(
            "const v = new Variadic(); \
             [v.count('a', 'b'), v.count(), v.isEmptyRest(), v.isEmptyRest('x')].join()"
        ),
        "2,0,true,false"
    );
}

/// `ToBoolean` coerces each argument.
#[test]
fn boolean_rest_args_coerce_each_element() {
    assert_eq!(
        eval(
            "[Variadic.anyTrue(0, '', 'x'), Variadic.anyTrue(false, 0), Variadic.anyTrue()].join()"
        ),
        "true,false,false"
    );
}

#[test]
fn count_selves_collects_instances() {
    assert_eq!(
        eval(
            "[Variadic.countSelves(new Variadic(), new Variadic()), Variadic.countSelves()].join()"
        ),
        "2,0"
    );
}

/// Each integer element runs through `ToInt32`: a non-integer number truncates toward zero, a
/// numeric string coerces, and 2^32 + 1 wraps to 1.
#[test]
fn integer_rest_args_convert_with_to_int32() {
    assert_eq!(
        eval(
            "[Variadic.sumInts(1, 2.7, '3'), Variadic.sumInts(-1.9), Variadic.sumInts(4294967297), \
              Variadic.sumInts(), Variadic.sumLongs(1000, 2000, 3000)].join()"
        ),
        "6,-1,1,0,6000"
    );
}

/// A function's `.length` counts required (non-`Option`, non-`RestArgs`) parameters, so the
/// variadic tail never contributes.
#[test]
fn length_ignores_rest_parameter() {
    assert_eq!(
        eval(
            "[Variadic.sum.length, Variadic.anyTrue.length, Variadic.prototype.count.length, \
              Variadic.prototype.join.length].join()"
        ),
        "0,0,0,1"
    );
}

/// `String::from_jsval` runs `ToString`, which throws on a `Symbol`. The pending exception
/// propagates out of the collection loop (the `ExnPending` arm), so the call throws.
#[test]
fn string_rest_throws_on_symbol() {
    assert!(throws("new Variadic().count(Symbol('x'))"));
}

/// A wrong-typed argument to a class-newtype rest fails the `cast`, producing a
/// `ConversionError::Failure` that is thrown as a `TypeError`.
#[test]
fn class_rest_throws_on_wrong_type() {
    assert!(throws("Variadic.countSelves(new Variadic(), 42)"));
    assert!(throws("Variadic.countSelves({})"));
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

    /// `sumXAfterGc` runs a compacting GC while the handles are live, and reads the freshly
    /// allocated objects back afterwards.
    #[test]
    fn raw_values_are_collected_and_survive_compacting_gc() {
        assert_eq!(
            eval(
                "[ValueVariadic.countValues(1, 'two', {}, null), ValueVariadic.countValues(), \
                  ValueVariadic.sumXAfterGc({x: 1}, {x: 2}, {x: 3}), ValueVariadic.sumXAfterGc()]\
                 .join()"
            ),
            "4,0,6,0"
        );
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
    }

    #[jsmethods]
    impl Tally {
        /// Variadic-only constructor: collection starts at argument 0.
        #[constructor]
        fn construct(rest: RestArgs<f64>) -> Self {
            Self {
                total: rest.iter().sum(),
            }
        }

        #[getter]
        fn total(&self) -> f64 {
            self.data().total
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
    fn constructors_collect_variadic_args() {
        assert_eq!(
            eval(
                "[new Tally(1, 2, 3).total, new Tally().total, new Labelled('a', 'x', 'y').count, \
                  new Labelled('a', 'x').label, new Labelled('a').count].join()"
            ),
            "6,0,2,a,0"
        );
    }
}

// ============================================================================
// `RestArgs` on free functions
//
// `#[jsmodule]`, `#[jsglobals]`, `#[jsnamespace]`, and `#[webidl_namespace]`
// share `parse_free_fn_export`/`gen_free_fn_native`.
// ============================================================================

mod free_fn_tests {
    use core_runtime::test_util::{eval_module_out_with_setup, eval_with_setup};
    use core_runtime::{jsglobals, jsmodule, jsnamespace, webidl_namespace};
    use js::class::RestArgs;

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

    #[test]
    fn module_fns_collect_variadic_args() {
        assert_eq!(
            eval_module_out_with_setup(
                setup,
                "import * as m from 'restModule';
                 globalThis.__out = [m.sumAll(1, 2, 3), m.sumAll(), m.joinAll('-', 'a', 'b')].join('|');"
            ),
            "6|0|a-b"
        );
    }

    #[test]
    fn global_and_namespace_fns_collect_variadic_args() {
        assert_eq!(
            eval(
                "[globalSum(1, 2), globalSum(), globalJoin('+', 'x', 'y'), restNs.nsSum(2, 4), \
                  restNs.nsSum(), restNs.nsCount('a', 'b'), RestWebIDL.widlSum(1, 2, 3)].join('|')"
            ),
            "3|0|x+y|6|0|2|6"
        );
    }

    /// Free functions compute `nargs` on a different path than methods.
    #[test]
    fn length_ignores_rest_parameter_on_free_fns() {
        assert_eq!(
            eval(
                "[globalSum.length, globalJoin.length, restNs.nsSum.length, \
                  restNs.nsCount.length, RestWebIDL.widlSum.length].join()"
            ),
            "0,1,0,0,0"
        );
    }

    /// The macro rewrites the unqualified `RestArgs` in each mod block above to the
    /// fully-qualified path, so this compiles only if that rewrite reaches the free-function
    /// paths.
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
    fn bad_arguments_reject_rather_than_throw() {
        assert_eq!(
            run(r#"
                const w = new Waiter();
                const outcome = (call) => {
                    let p;
                    try { p = call(); } catch (e) { return `threw:${e.constructor.name}`; }
                    return p.then(v => `resolved:${v}`, e => `rejected:${e.constructor.name}`);
                };
                Promise.all([
                    outcome(() => w.joinAsync("a", "b")),
                    outcome(() => w.joinAsync(Symbol())),
                    outcome(() => w.joinAsyncAfter("-", Symbol())),
                    outcome(() => w.joinAsyncAfter(Symbol())),
                ]).then(v => { globalThis.__out = v.join("|"); });
            "#),
            "resolved:a,b|rejected:TypeError|rejected:TypeError|rejected:TypeError"
        );
    }
}
