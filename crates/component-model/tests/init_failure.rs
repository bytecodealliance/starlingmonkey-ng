// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A main module that fails at its top level must fail the bootstrap.
//!
//! `JS::ModuleEvaluate` reports a top-level throw by rejecting the promise it
//! returns and clearing the pending exception, so a bootstrap that only checks
//! for a pending exception reports success and, on the dylib path, snapshots a
//! broken instance.
//!
//! [`initialize_runtime`] may be attempted at most once per process, so this
//! suite is its own test binary with a single case.

use component_model::initialize_runtime;

const MAIN: &str = r#"
    export function unreachable() {}
    throw new Error("top level went wrong");
"#;

#[test]
fn main_module_that_throws_fails_the_bootstrap() {
    let result = initialize_runtime(
        &core_runtime::config::RuntimeConfig::default(),
        MAIN,
        "main.js",
        None,
        |_scope| Ok(()),
    );

    let message = result
        .err()
        .expect("a throwing main module must not report success");
    assert!(
        message.contains("top level went wrong"),
        "the failure must name the thrown error, got: {message}"
    );

    // A second attempt fails rather than re-entering the once-per-process
    // `JSEngine::init`.
    let again = initialize_runtime(
        &core_runtime::config::RuntimeConfig::default(),
        "export function ok() {}",
        "main.js",
        None,
        |_scope| Ok(()),
    );
    assert!(again.is_err(), "a second bootstrap attempt must fail");
}
