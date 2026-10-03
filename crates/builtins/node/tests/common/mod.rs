// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Runs the JS test scripts in `tests/js`: each is the entry module of a runtime started with
//! `--node-compat`, and fails by throwing or rejecting.

use std::path::Path;

use libstarling::config::RuntimeConfig;

/// Run `tests/js/<name>` to completion, and panic with its error if it threw.
pub fn run_js(name: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/js")
        .join(name);
    run_path(name, &path);
}

/// Run `tests/js/<name>` to completion after `prelude`, a script that sets up globals for it, and
/// panic with its error if it threw.
pub fn run_js_after(name: &str, prelude: &str) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/js")
        .join(name);
    let dir = core_runtime::test_util::test_tempdir();
    let wrapper = dir.path().join("wrapper.mjs");
    let source = format!("{prelude}\nawait import({:?});\n", script.to_string_lossy());
    std::fs::write(&wrapper, source).expect("writing the wrapper");
    run_path(name, &wrapper);
}

/// Run the module at `path` to completion, and panic with its error, naming `name`, if it threw.
fn run_path(name: &str, path: &Path) {
    let config = RuntimeConfig::from_args(
        [
            "starling",
            "--node-compat",
            path.to_str().expect("UTF-8 path"),
        ]
        .into_iter()
        .map(String::from),
    )
    .expect("valid arguments");
    if let Err(error) = libstarling::run(config) {
        panic!("{name} failed: {error}");
    }
}
