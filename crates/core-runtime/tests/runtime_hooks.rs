// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Before-content hooks run between runtime initialization and the content script, and shutdown
//! hooks run while the runtime's context is still alive.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use std::cell::Cell;

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::{
    clear_runtime_hooks, register_before_content_hook, register_shutdown_hook, Runtime,
};

fn define_marker(runtime: &Runtime, _: &RuntimeConfig) -> Result<(), String> {
    let scope = runtime.default_global();
    js::compile::evaluate(&scope, "globalThis.fromHook = 1")
        .map(|_| ())
        .map_err(|_| "defining the marker failed".to_string())
}

fn refuse(_: &Runtime, _: &RuntimeConfig) -> Result<(), String> {
    Err("refused by hook".to_string())
}

#[test]
fn before_content_hook_runs_before_the_content_script() {
    clear_runtime_hooks();
    register_before_content_hook(define_marker);
    let config = RuntimeConfig::from_arg_string(
        "-e 'if (globalThis.fromHook !== 1) throw new Error(\"hook did not run first\")'",
    )
    .unwrap();
    core_runtime::run(config, |_, _, _| Ok(())).expect("the content script saw the marker");
}

#[test]
fn before_content_hook_error_aborts_the_run() {
    clear_runtime_hooks();
    register_before_content_hook(refuse);
    let config = RuntimeConfig::from_arg_string("-e 'globalThis.ran = true'").unwrap();
    let err = core_runtime::run(config, |_, _, _| Ok(())).unwrap_err();
    assert_eq!(err, "refused by hook");
}

thread_local! {
    static SHUTDOWN_SAW_GLOBAL: Cell<bool> = const { Cell::new(false) };
}

fn read_global_at_shutdown(runtime: &Runtime) {
    let scope = runtime.default_global();
    let ok = js::compile::evaluate(&scope, "typeof globalThis")
        .map(|value| !value.is_undefined())
        .unwrap_or(false);
    SHUTDOWN_SAW_GLOBAL.set(ok);
}

#[test]
fn shutdown_hook_runs_while_the_context_is_alive() {
    clear_runtime_hooks();
    register_shutdown_hook(read_global_at_shutdown);
    let runtime = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    drop(runtime);
    assert!(SHUTDOWN_SAW_GLOBAL.get());
}
