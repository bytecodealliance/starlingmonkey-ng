// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Shared plumbing for the serve-mode integration tests: standing a server up, speaking HTTP/1.1
//! to it, and the upstream servers a handler forwards to.

#![cfg(not(target_arch = "wasm32"))]
// Each test binary uses a different subset of these.
#![allow(dead_code)]

// The transport helpers and upstream servers are shared with the wasm end-to-end suite through
// `serve-test-support`. Only the serve starters and their config and script helpers remain here.
pub use serve_test_support::*;

// `serve-test-support` duplicates `IDLE_TRACE` rather than depend on `core-runtime`. This fails
// the build if the copies drift.
const _: () = {
    let ours = core_runtime::event_loop::IDLE_TRACE.as_bytes();
    let theirs = serve_test_support::wasm_serve::IDLE_TRACE.as_bytes();
    assert!(ours.len() == theirs.len());
    let mut i = 0;
    while i < ours.len() {
        assert!(ours[i] == theirs[i]);
        i += 1;
    }
};

use libstarling::config::RuntimeConfig;
use std::net::TcpListener;

/// A running serve loop; [`ServeHandle::stop`] shuts it down and joins the thread so the runtime
/// drops cleanly (a leaked runtime aborts at process teardown with outstanding engine handles).
pub struct ServeHandle {
    port: u16,
    shutdown: futures_channel::oneshot::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl ServeHandle {
    /// The port the server listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.thread.join();
    }
}

/// Start a serve loop with `handler` on a port the system picks, in a background thread.
pub fn start_serve(handler: &str) -> ServeHandle {
    start_serve_with(handler, false)
}

/// Like [`start_serve`], but chooses whether each request gets its own global.
pub fn start_serve_with(handler: &str, isolated: bool) -> ServeHandle {
    start_serve_config(RuntimeConfig {
        serve_isolated: isolated,
        ..legacy_script(handler)
    })
}

/// A config that runs `handler` as an inline classic script.
pub fn legacy_script(handler: &str) -> RuntimeConfig {
    RuntimeConfig {
        eval_script: Some(handler.to_string()),
        legacy_script: true,
        ..Default::default()
    }
}

/// Start a serve loop from `config` on a port the system picks, for the cases that need more than
/// an inline script, such as a module entry point on disk.
pub fn start_serve_config(config: RuntimeConfig) -> ServeHandle {
    start_serve_on(bind_local().0, config)
}

/// Start a serve loop from `config` on `listener`, for a handler that needs to know its own port
/// before the server starts.
///
/// The listener is bound before this returns, so a request made right away waits in its backlog
/// until the script has finished evaluating.
pub fn start_serve_on(listener: TcpListener, config: RuntimeConfig) -> ServeHandle {
    let port = listener.local_addr().unwrap().port();
    let (shutdown_tx, shutdown_rx) = futures_channel::oneshot::channel();
    let thread = std::thread::spawn(move || {
        // `run` does this before serving; the test calls `serve` directly, so register here.
        libstarling::apply_pre_init_config(&config).expect("valid config");
        libstarling::register_builtins();
        let _ = libstarling::serve_native::serve_with_shutdown(config, listener, async {
            let _ = shutdown_rx.await;
        });
    });
    ServeHandle {
        port,
        shutdown: shutdown_tx,
        thread,
    }
}

/// A temporary directory of content-script files, removed on drop.
pub struct ScriptDir(tempfile::TempDir);

impl ScriptDir {
    /// Create a directory holding `files`, given as `(relative path, contents)`.
    pub fn new(files: &[(&str, &str)]) -> Self {
        let dir = libstarling::test_util::test_tempdir();
        for (path, contents) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        ScriptDir(dir)
    }

    /// The path of `file` in this directory.
    pub fn path(&self, file: &str) -> String {
        self.0.path().join(file).to_string_lossy().into_owned()
    }
}
