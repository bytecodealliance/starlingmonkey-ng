// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Transports for WASIp3 targets.

pub(crate) mod pump;
pub(crate) mod stdio;
pub(crate) mod tcp;

/// Exit the instance with `code`.
pub(crate) fn exit(code: u8) -> ! {
    wasip3::cli::exit::exit_with_code(code);
    unreachable!("`exit-with-code` returned")
}
