// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Shared test helpers for `core-runtime` in-crate tests.

use js::{conversion::FromJSVal, error::ExnThrown, gc::scope::Scope};

use crate::{config::RuntimeConfig, event_loop::run_microtasks, runtime::Runtime};

/// Create a temp directory that works on both native and wasm targets.
///
/// On WASI, `std::env::temp_dir()` panics because there is no temp filesystem.
/// The wasmtime runner mounts the CWD, so we use `tempdir_in("/tmp")` instead.
/// The component runtime needs to be invoked with a `--dir=/tmp` option for
/// this to work.
pub fn test_tempdir() -> tempfile::TempDir {
    #[cfg(target_arch = "wasm32")]
    {
        tempfile::Builder::new()
            .tempdir_in("/tmp")
            .expect("failed to create temp dir")
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        tempfile::tempdir().expect("failed to create temp dir")
    }
}

/// Run setup, create a runtime, evaluate JS code, and convert the result to a string.
pub fn eval_with_setup(setup: impl FnOnce(), code: &str) -> String {
    setup();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    match js::compile::evaluate_with_filename(&scope, code, "test.js", 1) {
        Ok(val) => String::from_jsval(&scope, val, ()).unwrap(),
        Err(_) => panic!(
            "JS evaluation threw an exception: {:?}",
            ExnThrown::capture(&scope)
        ),
    }
}

/// Run setup, create a runtime, and check whether JS code throws.
pub fn throws_with_setup(setup: impl FnOnce(), code: &str) -> bool {
    setup();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    js::compile::evaluate_with_filename(&scope, code, "test.js", 1).is_err()
}

/// Evaluate `code` on `scope`, drain microtasks, and return `String(globalThis.__out)`.
///
/// Panics if `code` throws.
pub fn eval_and_read_out(scope: &Scope<'_>, code: &str) -> String {
    if js::compile::evaluate_with_filename(scope, code, "test.js", 1).is_err() {
        panic!("evaluation threw: {:?}", ExnThrown::capture(scope));
    }
    read_out(scope)
}

/// Drain microtasks and return `String(globalThis.__out)`.
fn read_out(scope: &Scope<'_>) -> String {
    run_microtasks(scope);
    let out = js::compile::evaluate_with_filename(scope, "globalThis.__out", "out.js", 1)
        .expect("reading __out threw");
    String::from_jsval(scope, out, ()).unwrap()
}

/// Run setup, create a runtime, and return [`eval_and_read_out`] for `code`.
pub fn eval_out_with_setup(setup: impl FnOnce(), code: &str) -> String {
    setup();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    eval_and_read_out(&scope, code)
}

/// Run setup, create a runtime, evaluate `source` as an ES module, drain microtasks, and return
/// `String(globalThis.__out)`.
///
/// Panics if the module throws.
pub fn eval_module_out_with_setup(setup: impl FnOnce(), source: &str) -> String {
    setup();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    // SAFETY: `Runtime::init` initialized the module loader.
    if unsafe { crate::module::evaluate_module(&scope, source, "test.mjs") }.is_err() {
        panic!("module evaluation threw: {:?}", ExnThrown::capture(&scope));
    }
    read_out(&scope)
}
