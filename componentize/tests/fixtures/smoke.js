// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The smoke-world guest module.
//
// `log` is the world-level WIT import, synthesized into the `external` module.
// `run` is the world-level WIT export, a named export of this main module that
// the interpreter resolves against the module namespace.

import { log } from "external";

export function run(n) {
  log(`run was called with ${n}`);
  return n + 1;
}
