// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Runs the JS test scripts in `tests/js`: each is the entry module of a runtime started with
//! `--node-compat`, and fails by throwing or rejecting.

use std::path::Path;

use libstarling::config::RuntimeConfig;

/// Run `tests/js/<name>` to completion, and panic with its error if it threw.
pub fn run_js(name: &str) {
    run_js_with_flags(name, &[]);
}

/// Run `tests/js/<name>` to completion with the runtime flags `flags` besides `--node-compat`,
/// and panic with its error if it threw.
pub fn run_js_with_flags(name: &str, flags: &[&str]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/js")
        .join(name);
    run_path(name, &path, flags);
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
    run_path(name, &wrapper, &[]);
}

/// The environment variable that makes [`run_path`] run the module in this process.
const CHILD_VAR: &str = "NODE_BUILTINS_TEST_CHILD";

/// Run the module at `path` to completion with the runtime flags `flags` besides `--node-compat`,
/// and panic with its error, naming `name`, if it threw.
///
/// The module runs in a child process that runs the calling test again with [`CHILD_VAR`] set,
/// so `process.exit()`, a non-zero `process.exitCode` and an uncaught exception end only that
/// process. The test fails unless the child reports it passed.
fn run_path(name: &str, path: &Path, flags: &[&str]) {
    if std::env::var_os(CHILD_VAR).is_some() {
        let args = ["starling", "--node-compat"]
            .into_iter()
            .chain(flags.iter().copied())
            .chain([path.to_str().expect("UTF-8 path")])
            .map(String::from);
        let config = RuntimeConfig::from_args(args).expect("valid arguments");
        if let Err(error) = libstarling::run(config) {
            panic!("{name} failed: {error}");
        }
        return;
    }
    let thread = std::thread::current();
    let test = thread.name().expect("a test thread named after its test");
    let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD_VAR, "1")
        .output()
        .expect("running the test in a child process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let passed = output.status.success() && stdout.contains("test result: ok. 1 passed");
    assert!(
        passed,
        "{name} failed with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}
