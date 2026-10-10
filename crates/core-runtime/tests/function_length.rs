// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `.length` of functions that the macros create: the count of parameters that aren't
//! `Option<T>`, or the value of `length = N` in a module function's `#[function]`, a
//! `#[jsfunction_class]` member's or a `#[jsmethods]` constructor's attribute.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::module::evaluate_module;
use core_runtime::runtime::Runtime;
use core_runtime::test_util::eval_with_setup;
use core_runtime::{jsclass, jsfunction_class, jsglobals, jsmethods, jsmodule, jsnamespace};
use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::Object;

#[jsmodule]
mod length_module {
    pub fn derived(a: f64, b: Option<f64>) -> f64 {
        a + b.unwrap_or(0.0)
    }

    #[function(length = 3)]
    pub fn overridden(a: f64, b: Option<f64>) -> f64 {
        a + b.unwrap_or(0.0)
    }
}

#[jsglobals]
mod length_globals {
    #[function(length = 2)]
    pub fn global_overridden(a: Option<f64>) -> f64 {
        a.unwrap_or(1.0)
    }
}

#[jsnamespace(name = "lengthNs")]
mod length_ns {
    #[function(length = 4)]
    pub fn ns_overridden(a: Option<f64>) -> f64 {
        a.unwrap_or(1.0)
    }
}

#[jsfunction_class(name = "Sized")]
pub mod sized {
    use super::*;

    #[constructor(length = 1)]
    pub fn init(this: Object<'_>, _options: Option<f64>) -> Result<(), ExnThrown> {
        let _ = this;
        Ok(())
    }

    #[method(length = 3)]
    pub fn write(this: Object<'_>, chunk: f64, _encoding: Option<String>) -> f64 {
        let _ = this;
        chunk
    }

    pub fn read(this: Object<'_>, size: Option<f64>) -> f64 {
        let _ = this;
        size.unwrap_or(0.0)
    }

    #[static_method(length = 2)]
    pub fn from(value: Option<f64>) -> f64 {
        value.unwrap_or(0.0)
    }
}

#[jsclass]
struct Counted {
    start: f64,
}

#[jsmethods]
impl Counted {
    #[constructor(length = 2)]
    fn new(&self, start: Option<f64>) -> Result<(), ExnThrown> {
        self.data_mut().start = start.unwrap_or(1.0);
        Ok(())
    }

    #[getter]
    fn start(&self) -> f64 {
        self.data().start
    }
}

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope: &Scope<'_>, global| {
        Counted::add_to_global(scope, global);
        length_globals::add_to_global(scope, global);
        length_ns::add_to_global(scope, global);
        length_module::register(scope);
        global
            .set_property(scope, c"Sized", sized::register(scope).expect("registers"))
            .expect("defines Sized");
    });
}

fn eval_module(body: &str) -> String {
    setup();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let source = format!("import * as m from \"lengthModule\";\nglobalThis._result = {body};");
    // SAFETY: `scope` outlives the evaluation, and the module registry was
    // populated by `setup` before any JS ran.
    unsafe { evaluate_module(&scope, &source, "length_test.mjs") }.expect("module eval failed");
    let val = js::compile::evaluate_with_filename(&scope, "globalThis._result", "read.js", 1)
        .expect("readback failed");
    String::from_jsval(&scope, val, ()).expect("null string")
}

#[test]
fn module_functions_take_their_length_from_the_attribute() {
    assert_eq!(
        eval_module("[m.derived.length, m.overridden.length, m.overridden(1)].join()"),
        "1,3,1"
    );
}

#[test]
fn global_and_namespace_functions_take_their_length_from_the_attribute() {
    let result = eval_with_setup(
        setup,
        "[globalOverridden.length, lengthNs.nsOverridden.length, globalOverridden()].join()",
    );
    assert_eq!(result, "2,4,1");
}

#[test]
fn function_class_members_take_their_length_from_the_marker() {
    let result = eval_with_setup(
        setup,
        "[Sized.length, Sized.prototype.write.length, Sized.prototype.read.length, \
          Sized.from.length, new Sized().write(5)].join()",
    );
    assert_eq!(result, "1,3,0,2,5");
}

#[test]
fn class_constructors_take_their_length_from_the_marker() {
    let result = eval_with_setup(
        setup,
        "[Counted.length, new Counted().start, new Counted(3).start].join()",
    );
    assert_eq!(result, "2,1,3");
}
