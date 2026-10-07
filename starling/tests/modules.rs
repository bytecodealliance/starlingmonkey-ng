// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end checks of module loading in the `starling` binary.

#![cfg(not(target_arch = "wasm32"))]

use std::process::Command;

/// A module importing the entry module by path gets the entry module itself, so
/// the entry's top level runs once and the importer sees its bindings. That holds
/// for an absolute entry path and for a bare filename in the working directory.
#[test]
fn importing_the_entry_module_gets_the_entry_module() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.mjs"),
        "import { markerType } from './dep.mjs';\n\
         export const marker = {};\n\
         globalThis.runs = (globalThis.runs ?? 0) + 1;\n\
         console.log(`runs ${globalThis.runs} ${markerType()}`);",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dep.mjs"),
        "import * as main from './main.mjs';\n\
         export const markerType = () => typeof main.marker;",
    )
    .unwrap();
    for entry in [dir.path().join("main.mjs"), "main.mjs".into()] {
        let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
            .current_dir(dir.path())
            .arg(&entry)
            .output()
            .expect("failed to run starling");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "runs 1 object\n",
            "entry {}: stderr: {}",
            entry.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `crates/builtins/node/tests/js/child_process.mjs` passes when the binary runs it. The binary is
/// `process.execPath` then, so the script's `fork` and IPC checks always run, which the in-process
/// run of `node-builtins` skips without a built binary.
#[test]
fn child_process_script_forks_the_binary() {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/builtins/node/tests/js/child_process.mjs"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"))
        .args(["--node-compat", script])
        .output()
        .expect("failed to run starling");
    assert!(out.status.success(), "{out:?}");
}
