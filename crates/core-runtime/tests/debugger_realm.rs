// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A realm created with `debugger_realm_options` is invisible to every `Debugger`.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;
use core_runtime::test_util::eval_and_read_out;
use js::engine::RealmOptions;
use js::gc::handle::RootedHeap;
use js::gc::scope::{OnNewGlobalHookOption, RootScope};

/// Create a second global with `options`, then count the globals a `Debugger` in the default
/// global can debug. The default global itself is never counted, since a `Debugger` cannot
/// debug its own compartment.
fn debuggees_seen_from_content(mut options: RealmOptions) -> String {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let content = rt.default_global();
    js::class::install_global_private_trace(&mut options);
    let other = {
        let scope = RootScope::new_global(
            content.cx_mut(),
            &js::class::STARLING_GLOBAL_CLASS,
            OnNewGlobalHookOption::DontFireOnNewGlobalHook,
            options,
        );
        RootedHeap::new(scope.global())
    };
    js::debug::define_debugger_object(&content, content.global().handle())
        .expect("defining Debugger");
    let seen = eval_and_read_out(
        &content,
        "const dbg = new Debugger(); dbg.addAllGlobalsAsDebuggees(); \
         globalThis.__out = String(dbg.getDebuggees().length)",
    );
    drop(other);
    seen
}

#[test]
fn plain_realms_are_visible_to_debuggers() {
    assert_eq!(debuggees_seen_from_content(RealmOptions::default()), "1");
}

#[test]
fn debugger_realms_are_invisible_to_debuggers() {
    assert_eq!(
        debuggees_seen_from_content(js::debug::debugger_realm_options()),
        "0"
    );
}
