// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Child processes.

#![cfg(all(unix, not(target_arch = "wasm32")))]

use platform::child::{run_sync, spawn, Options, Stdio, SyncOptions};
use std::time::Duration;

#[test]
fn a_timeout_without_pipes_kills_the_child() {
    let options = Options {
        file: "sleep".into(),
        args: vec!["sleep".into(), "5".into()],
        stdio: vec![Stdio::Ignore, Stdio::Ignore, Stdio::Ignore],
        ..Options::default()
    };
    let sync = SyncOptions {
        timeout: Some(Duration::from_millis(100)),
        kill_signal: libc::SIGTERM,
        ..SyncOptions::default()
    };
    let output = run_sync(&options, sync);
    assert_eq!(
        output.error.and_then(|error| error.raw_os_error()),
        Some(libc::ETIMEDOUT)
    );
    let status = output.status.expect("the child was spawned");
    assert_eq!(status.signal, Some(libc::SIGTERM));
}

#[test]
fn a_pipe_keeps_its_descriptor_after_a_read_waited() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut echo = spawn(&Options {
            file: "sh".into(),
            args: vec!["sh".into(), "-c".into(), "sleep 0.2; echo hello".into()],
            stdio: vec![Stdio::Ignore, Stdio::Pipe, Stdio::Ignore],
            ..Options::default()
        });
        let pipe = echo.pipes[1].take().unwrap();
        let fd = pipe.as_raw_fd();
        let (mut reader, _writer) = pipe.split().unwrap();
        // The child writes nothing yet, so the read registers the reader with tokio and waits.
        assert!(futures_lite::future::poll_once(reader.read())
            .await
            .is_none());

        let mut cat = spawn(&Options {
            file: "cat".into(),
            args: vec!["cat".into()],
            stdio: vec![Stdio::Fd(fd), Stdio::Pipe, Stdio::Ignore],
            ..Options::default()
        });
        cat.child.expect("`cat` was spawned");
        let (mut output, _writer) = cat.pipes[1].take().unwrap().split().unwrap();
        let mut received = Vec::new();
        let read_all = async {
            while let Some(bytes) = output.read().await.unwrap() {
                received.extend(bytes);
            }
        };
        tokio::time::timeout(Duration::from_secs(10), read_all)
            .await
            .expect("`cat` ended its output");
        assert_eq!(received, b"hello\n");
        drop(reader);
    });
}
