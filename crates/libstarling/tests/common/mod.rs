// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Shared plumbing for the serve-mode integration tests: standing a server up, speaking HTTP/1.1
//! to it, and the upstream servers a handler forwards to.

#![cfg(not(target_arch = "wasm32"))]
// Each test binary uses a different subset of these.
#![allow(dead_code)]

// The transport helpers and upstream servers are shared with the wasm end-to-end suite through
// `serve-test-support`. Only the serve starters, which link the engine, remain here.
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
use std::net::TcpStream;
use std::time::Duration;

/// A running serve loop; [`ServeHandle::stop`] shuts it down and joins the thread so the runtime
/// drops cleanly (a leaked runtime aborts at process teardown with outstanding engine handles).
pub struct ServeHandle {
    shutdown: futures_channel::oneshot::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl ServeHandle {
    pub fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.thread.join();
    }
}

/// Start a serve loop with `handler` on `port` in a background thread, and wait until it accepts
/// connections.
pub fn start_serve(handler: &str, port: u16) -> ServeHandle {
    start_serve_with(handler, port, false)
}

/// Like [`start_serve`], but chooses whether each request gets its own global.
pub fn start_serve_with(handler: &str, port: u16, isolated: bool) -> ServeHandle {
    start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(port),
            serve_isolated: isolated,
            ..Default::default()
        },
        port,
    )
}

/// Start a serve loop from a fully-specified config — for the cases that need more than an inline
/// script, such as a module entry point on disk.
pub fn start_serve_config(config: RuntimeConfig, port: u16) -> ServeHandle {
    let (shutdown_tx, shutdown_rx) = futures_channel::oneshot::channel();
    let thread = std::thread::spawn(move || {
        // `run` does this before serving; the test calls `serve` directly, so register here.
        libstarling::apply_pre_init_config(&config).expect("valid config");
        libstarling::register_builtins();
        let _ = libstarling::serve_native::serve_with_shutdown(config, port, async {
            let _ = shutdown_rx.await;
        });
    });
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return ServeHandle {
                shutdown: shutdown_tx,
                thread,
            };
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("serve did not start on port {port}");
}
