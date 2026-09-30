// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Copy the runtime `starling-componentize` embeds as its default `--runtime` to
//! `$OUT_DIR/runtime.wasm`.
//!
//! The embedded runtime is the file `STARLING_EMBED_RUNTIME` names, or else the
//! runtime component `just build-runtime` writes for wasm32-wasip3, under
//! `CARGO_TARGET_DIR` or the repository's `target/`. A relative path in either
//! variable resolves against `componentize/`. An empty `STARLING_EMBED_RUNTIME`
//! counts as unset. A file it names that can't be read fails the build. Without
//! a runtime at the default path, the copy is empty, and the binary embeds none.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=STARLING_EMBED_RUNTIME");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let explicit = env::var_os("STARLING_EMBED_RUNTIME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let runtime = explicit.clone().unwrap_or_else(|| {
        env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| manifest_dir.join("../target"))
            .join("wasm32-wasip3/release/starling.wasm")
    });
    println!("cargo:rerun-if-changed={}", runtime.display());
    let bytes = match std::fs::read(&runtime) {
        Ok(bytes) => bytes,
        Err(e) if explicit.is_some() => {
            panic!("reading STARLING_EMBED_RUNTIME={}: {e}", runtime.display())
        }
        Err(_) => Vec::new(),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("set by cargo")).join("runtime.wasm");
    // Rewritten only when it changed, since the binary embeds it and is rebuilt
    // whenever the file is newer.
    if std::fs::read(&out).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::write(&out, bytes).expect("writing the embedded runtime");
    }
}
