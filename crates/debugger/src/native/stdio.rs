// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The stdio transport. Messages arrive on stdin and leave on the process's original stdout.
//! File descriptors 1 and 2 are redirected into pipes, and what is written to them becomes
//! `output` events in the `stdout` and `stderr` categories.

use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;

use super::check;
use super::threads::{StdioCapture, ThreadConnection};

/// Redirect fds 1 and 2, and connect over stdin and the original stdout. Child processes do not
/// inherit the original stdout.
pub(crate) fn connect() -> io::Result<ThreadConnection> {
    // SAFETY: `F_DUPFD_CLOEXEC` returns a new descriptor this process owns, which `File` takes
    // over.
    let protocol_out =
        unsafe { File::from_raw_fd(check(libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 0))?) };
    let capture = StdioCapture::start()?;
    ThreadConnection::new(io::stdin(), protocol_out, Some(capture))
}
