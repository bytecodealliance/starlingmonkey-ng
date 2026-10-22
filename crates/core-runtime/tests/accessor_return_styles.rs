// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Accessor and method return styles: a fallible setter whose body returns `Result<T, E>` with a
//! non-unit `T` (classified `ResultValue`) throws the `Err` and ignores the `Ok` value, and a
//! `Result<Promise, E>` operation or attribute rejects with the `Err`.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::jsclass;
use core_runtime::jsmethods;
use core_runtime::test_util::{eval_out_with_setup, eval_with_setup};
use js::error::TypeError;
use js::gc::scope::Scope;
use js::Promise;

#[jsclass]
struct Cell {
    value: i32,
}

#[jsmethods]
impl Cell {
    #[constructor]
    fn construct() -> Self {
        Self { value: 0 }
    }

    #[getter]
    fn value(&self) -> i32 {
        self.data().value
    }

    #[setter]
    fn set_value(&mut self, v: i32) -> Result<i32, TypeError> {
        if v < 0 {
            return Err(TypeError("value must be non-negative".into()));
        }
        self.data_mut().value = v;
        Ok(v)
    }

    #[method]
    fn fail_async<'r>(&self, _scope: &'r Scope<'_>) -> Result<Promise<'r>, TypeError> {
        Err(TypeError("method failure".into()))
    }

    #[getter]
    fn failing_promise<'r>(&self, _scope: &'r Scope<'_>) -> Result<Promise<'r>, TypeError> {
        Err(TypeError("getter failure".into()))
    }
}

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        Cell::add_to_global(scope, global);
    });
}

#[test]
fn fallible_setter_assigns_ok_and_throws_err() {
    assert_eq!(
        eval_with_setup(
            setup,
            "const c = new Cell(); c.value = 9; let threw = false; \
             try { c.value = -1; } catch (e) { threw = e instanceof TypeError; } \
             `${c.value} ${threw}`"
        ),
        "9 true"
    );
}

/// A `Result<Promise, E>` operation or attribute turns an `Err` of any
/// `ThrowException` type into a promise rejected with that error.
#[test]
fn result_promise_rejects_with_the_thrown_error() {
    assert_eq!(
        eval_out_with_setup(
            setup,
            "const c = new Cell();
             Promise.all([
                 c.failAsync().catch(e => `${e.constructor.name}:${e.message}`),
                 c.failingPromise.catch(e => `${e.constructor.name}:${e.message}`),
             ]).then(v => { globalThis.__out = v.join(','); });"
        ),
        "TypeError:method failure,TypeError:getter failure"
    );
}
