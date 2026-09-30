// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

use std::path::PathBuf;

/// Whether this target's wasi-libc exports `__wasi_init_tp`, which `static_init` in `src/lib.rs`
/// calls. wasi-sdk 33 exports it and wasi-sdk 34 does not, and a link that references the
/// missing symbol fails. The WASI SDK is the one `WASI_SDK_PATH` names, or `/opt/wasi-sdk`.
fn has_wasi_init_tp(target: &str) -> bool {
    let sdk = std::env::var_os("WASI_SDK_PATH").unwrap_or_else(|| "/opt/wasi-sdk".into());
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
    // Routes `__wasm_call_ctors` through `__wrap___wasm_call_ctors` in `lib.rs`.
    println!("cargo::rustc-link-arg-cdylib=--wrap=__wasm_call_ctors");
    // The wit-dylib bindings that `starling-componentize` links against the core module import the
    // function table from it, which is growable so the bindings' element segment fits, and on
    // wasm32-wasip2 the shadow stack pointer global. On wasm32-wasip3 the stack pointer lives in
    // the task context, and no such global exists to export. Memory is exported by default.
    println!("cargo::rustc-link-arg-cdylib=--export-table");
    println!("cargo::rustc-link-arg-cdylib=--growable-table");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("p2") {
        println!("cargo::rustc-link-arg-cdylib=--export=__stack_pointer");
    }
}
