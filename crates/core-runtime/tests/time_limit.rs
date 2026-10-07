// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `js::watchdog::with_time_limit` reports a limit only when reaching it terminated JavaScript.

// Wasm has no threads, so no limit is ever reached there.
#![cfg(not(target_arch = "wasm32"))]

use std::time::Duration;

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;

#[test]
fn a_limit_reached_while_javascript_runs_terminates_it() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let (result, reached) =
        js::watchdog::with_time_limit(&scope, Duration::from_millis(10), || {
            js::compile::evaluate(&scope, "for (;;) {}").is_ok()
        });
    assert_eq!((result, reached), (false, true));
}

#[test]
fn a_limit_reached_while_no_javascript_runs_is_not_reported() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let (_, reached) = js::watchdog::with_time_limit(&scope, Duration::from_millis(10), || {
        std::thread::sleep(Duration::from_millis(100));
    });
    assert!(!reached);
}
