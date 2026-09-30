// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The surface-world guest module.

import { log } from "external";

export function run(n) {
  log(`run ${n}`);
  return n + 1;
}

export function random() {
  return Math.random();
}

export function now() {
  return Date.now();
}
