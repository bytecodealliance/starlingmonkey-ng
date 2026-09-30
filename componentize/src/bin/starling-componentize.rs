// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

/// The runtime `build.rs` embeds, which is empty when it found none.
static EMBEDDED_RUNTIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.wasm"));

#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    let embedded = (!EMBEDDED_RUNTIME.is_empty()).then_some(EMBEDDED_RUNTIME);
    componentize::command::run(std::env::args_os(), embedded)
}
