// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

/// The runtime `build.rs` embeds, which is empty when it found none.
#[cfg(not(target_arch = "wasm32"))]
static EMBEDDED_RUNTIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.wasm"));

#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    let embedded = (!EMBEDDED_RUNTIME.is_empty()).then_some(EMBEDDED_RUNTIME);
    componentize::command::run(std::env::args_os(), embedded)
}

#[cfg(target_arch = "wasm32")]
fn main() {}
