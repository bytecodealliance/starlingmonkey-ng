// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Transports for WASIp3 targets, and the interrupt request a debugger outside the instance
//! writes.

pub(crate) mod interrupt;
pub(crate) mod pump;
pub(crate) mod stdio;
pub(crate) mod tcp;
