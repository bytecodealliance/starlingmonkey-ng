// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! WPT (Web Platform Tests) support globals.
//!
//! Provides `evalScript` — a global function that evaluates a script in a
//! non-syntactic scope, so that top-level `let` and `const` bindings are
//! shared across calls. This emulates the behavior of HTML `<script>` tags
//! and is required by the WPT test harness.

use js::conversion::ConversionError;
use js::error::throw_error;
use js::gc::scope::Scope;
use js::native::{Handle, RawJSContext};
use js::prelude::FromJSVal;
use js::Object;

/// Install WPT-specific globals on the given global object.
///
/// Currently installs:
/// - `evalScript(source)` — evaluate a script in non-syntactic scope
/// - `__setLocation(url)` — configure the URL backing `globalThis.location`
///
/// # Safety
///
/// Must be called with a valid scope and global object.
pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    js::Function::define(
        scope,
        global.handle(),
        c"evalScript",
        Some(eval_script_native),
        2,
        0,
    )
    .expect("failed to define evalScript");
    js::Function::define(
        scope,
        global.handle(),
        c"__setLocation",
        Some(set_location_native),
        1,
        0,
    )
    .expect("failed to define __setLocation");
    js::Function::define(
        scope,
        global.handle(),
        c"__wptDone",
        Some(wpt_done_native),
        0,
        0,
    )
    .expect("failed to define __wptDone");
}

/// JSNative implementation of `evalScript(source, url)`.
///
/// Evaluates the given string as a script in a non-syntactic scope, making
/// top-level `let`/`const` bindings visible to subsequent calls. This is
/// how the WPT harness loads `META: script=...` dependencies.
///
/// `url` names the script in stack traces and error messages; it is what tells
/// a failure in a META dependency apart from one in the test itself.
unsafe extern "C" fn eval_script_native(
    raw_cx: *mut RawJSContext,
    argc: u32,
    vp: *mut js::native::Value,
) -> bool {
    use js::prelude::RootScope;

    let scope = RootScope::from_current_realm(raw_cx);
    let args = js::native::CallArgs::from_vp(vp, argc);
    let source =
        match String::from_jsval(&scope, Handle::from_raw(args.get(0)), ()).inspect_err(|e| {
            if let ConversionError::Failure(_) = e {
                throw_error(&scope, "evalScript: argument must be a string");
            }
        }) {
            Ok(source) => source,
            Err(_) => return false,
        };

    // The harness passes the script's URL; fall back to the function's own name when it is absent
    // (a hand-written `evalScript(source)` call). The argument count is what says "absent": the
    // conversion is WebIDL's DOMString one, which turns a missing argument into the *string*
    // "undefined" rather than failing, and traces attributed to a script called "undefined" read
    // as corrupted rather than as an eval'd script.
    let filename = if argc >= 2 {
        match String::from_jsval(&scope, Handle::from_raw(args.get(1)), ()) {
            Ok(filename) => filename,
            Err(_) => return false,
        }
    } else {
        "evalScript".to_string()
    };

    // Evaluate in non-syntactic scope.
    match js::compile::evaluate_non_syntactic(&scope, &source, &filename, 1) {
        Ok(rval) => {
            args.rval().set(rval.get());
            true
        }
        Err(_) => {
            eprintln!("Eval failed");
            false
        }
    }
}

/// JSNative implementation of `__setLocation(url)`.
///
/// Parses `url` and installs it as the URL backing `globalThis.location`. The
/// WPT harness calls this once per test with the test's canonical
/// `http://web-platform.test:8000/...` URL so that tests querying
/// `location.origin`, `location.href`, etc. observe the same values they
/// would when loaded from a real WPT HTTP server.
unsafe extern "C" fn set_location_native(
    raw_cx: *mut RawJSContext,
    argc: u32,
    vp: *mut js::native::Value,
) -> bool {
    use js::prelude::RootScope;

    let scope = RootScope::from_current_realm(raw_cx);
    let args = js::native::CallArgs::from_vp(vp, argc);
    let raw = match String::from_jsval(&scope, Handle::from_raw(args.get(0)), ()) {
        Ok(s) => s,
        Err(_) => {
            throw_error(&scope, "__setLocation: argument must be a string");
            return false;
        }
    };
    let url = match url::Url::parse(&raw) {
        Ok(u) => u,
        Err(e) => {
            throw_error(&scope, &format!("__setLocation: invalid URL {raw:?}: {e}"));
            return false;
        }
    };
    web_globals::worker_location::set_init_location(url);
    args.rval().set(js::value::undefined());
    true
}

/// JSNative implementation of `__wptDone()`.
///
/// The WPT post-harness calls this from the test's completion callback, after results have been
/// emitted. It requests that the event loop stop, so a finished test that left a live `setInterval`
/// (or other pending timer) running does not keep the process alive until the harness timeout.
unsafe extern "C" fn wpt_done_native(
    _raw_cx: *mut RawJSContext,
    argc: u32,
    vp: *mut js::native::Value,
) -> bool {
    let args = js::native::CallArgs::from_vp(vp, argc);
    core_runtime::event_loop::request_stop();
    args.rval().set(js::value::undefined());
    true
}
