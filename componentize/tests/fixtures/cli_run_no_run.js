// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The negative case of the CLI suite: an app that exports no `run` function,
// which componentizing it as a CLI tool refuses.

export function notRun() {
  return 42;
}
