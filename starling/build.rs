// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

use std::path::PathBuf;

/// Whether this target's wasi-libc still exports `__wasi_init_tp`.
///
/// wasi-sdk 33 ships it, and `static_init` in `src/lib.rs` calls it to do the
/// main-thread setup a crt-less link leaves out. wasi-sdk 34 dropped it: its crt
/// sets up TLS through a static `init_tls` with no exported entry point, and a
/// wasm32-wasip3 link that references the old name fails. So `static_init` is
/// only compiled when this target's `libc.a` exports the symbol.
fn has_wasi_init_tp(target: &str) -> bool {
    let Ok(sdk) = std::env::var("WASI_SDK_PATH") else {
        return false;
    };
    let libc = PathBuf::from(sdk)
        .join("share/wasi-sysroot/lib")
        .join(target)
        .join("libc.a");
    println!("cargo::rerun-if-changed={}", libc.display());
    std::fs::read(&libc)
        .map(|bytes| {
            bytes
                .windows(b"__wasi_init_tp".len())
                .any(|w| w == b"__wasi_init_tp")
        })
        .unwrap_or(false)
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=WASI_SDK_PATH");
    println!("cargo::rustc-check-cfg=cfg(has_wasi_init_tp)");

    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.starts_with("wasm32-") {
        return;
    }
    if has_wasi_init_tp(&target) {
        println!("cargo::rustc-cfg=has_wasi_init_tp");
    }
    // Routes `__wasm_call_ctors` through `__wrap___wasm_call_ctors` in `lib.rs`, so every path
    // that runs the static constructors shares one guard. Without it they run twice and
    // SpiderMonkey's `InitializeUptime` aborts.
    println!("cargo::rustc-link-arg-cdylib=--wrap=__wasm_call_ctors");
    // What the wit-dylib bindings `starling-componentize` links against the core module import
    // from it: the function table, growable so the bindings' element segment fits, and on
    // wasm32-wasip2 the shadow stack pointer global. On wasm32-wasip3 the stack pointer lives in
    // the task context, and no such global exists to export. Memory is exported by default.
    println!("cargo::rustc-link-arg-cdylib=--export-table");
    println!("cargo::rustc-link-arg-cdylib=--growable-table");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("p2") {
        println!("cargo::rustc-link-arg-cdylib=--export=__stack_pointer");
    }
}
