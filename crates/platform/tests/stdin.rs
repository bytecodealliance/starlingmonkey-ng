// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Standard input, read by a child process of the test that runs this binary again.

#![cfg(all(unix, not(target_arch = "wasm32")))]

use platform::child::{run_sync, Options, Stdio, SyncOptions};
use platform::process::Stdin;
use std::io::{Read, Write};

const INPUT_LENGTH: usize = 300_000;

#[test]
fn a_child_process_receives_the_input_after_the_chunk_read() {
    if std::env::var_os("PLATFORM_STDIN_TEST_CHILD").is_some() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let first = runtime.block_on(Stdin::open().read()).unwrap().unwrap();
        let options = Options {
            file: "wc".into(),
            args: vec!["wc".into(), "-c".into()],
            stdio: vec![Stdio::Fd(0), Stdio::Pipe, Stdio::Ignore],
            ..Options::default()
        };
        let output = run_sync(&options, SyncOptions::default());
        let counted = String::from_utf8(output.output[1].clone().unwrap()).unwrap();
        println!("read {} counted {}", first.len(), counted.trim());
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_child_process_receives_the_input_after_the_chunk_read",
            "--nocapture",
        ])
        .env("PLATFORM_STDIN_TEST_CHILD", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&[b'x'; INPUT_LENGTH]);
    });
    let mut output = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert!(child.wait().unwrap().success(), "{output}");
    writer.join().unwrap();
    let line = output
        .lines()
        .find_map(|line| line.strip_prefix("read "))
        .unwrap_or_else(|| panic!("no result in {output:?}"));
    let (read, counted) = line.split_once(" counted ").unwrap();
    let read: usize = read.parse().unwrap();
    let counted: usize = counted.parse().unwrap();
    assert_eq!(read + counted, INPUT_LENGTH);
}
