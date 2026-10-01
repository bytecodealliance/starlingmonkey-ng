// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Transports for native targets.

pub(crate) mod stdio;
pub(crate) mod tcp;
pub(crate) mod threads;

/// `result` as an `io::Result`, with the OS error if it is negative.
fn check(result: libc::c_int) -> std::io::Result<libc::c_int> {
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
