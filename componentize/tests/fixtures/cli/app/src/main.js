// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The guest of the merged worlds of `cli/a.wit` and `cli/b.wit`. Its import resolves only with
// `cli/app` as the base directory.

import { base } from "../lib.js";

console.log(`initializing at ${location.href}`);

export const one = () => base + 1;
export const two = () => base + 2;
