// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A top level that rejects after a timer, which fails the run.
await new Promise((resolve) => setTimeout(resolve, 10));
throw new Error("late top-level failure");
