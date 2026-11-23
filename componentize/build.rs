// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Copy the runtime `starling-componentize` embeds as its default `--runtime` to
//! `$OUT_DIR/runtime.wasm`.
//!
//! The embedded runtime is the file `STARLING_EMBED_RUNTIME` names, and a
//! relative path resolves against `componentize/`. A file it names that can't
//! be read fails the build. Without it, or with it empty, the copy is empty,
//! and the binary embeds none.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=STARLING_EMBED_RUNTIME");
    let bytes = match env::var_os("STARLING_EMBED_RUNTIME").filter(|path| !path.is_empty()) {
        Some(runtime) => {
            let runtime = PathBuf::from(runtime);
            println!("cargo:rerun-if-changed={}", runtime.display());
            std::fs::read(&runtime).unwrap_or_else(|e| {
                panic!("reading STARLING_EMBED_RUNTIME={}: {e}", runtime.display())
            })
        }
        None => Vec::new(),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("set by cargo")).join("runtime.wasm");
    // The file is rewritten only when its contents changed, since the binary
    // embeds it and is rebuilt whenever the file is newer.
    if std::fs::read(&out).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::write(&out, bytes).expect("writing the embedded runtime");
    }
}
