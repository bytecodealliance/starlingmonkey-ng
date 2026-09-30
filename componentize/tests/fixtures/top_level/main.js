// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The top-level-world guest module.
//
// It imports a module by relative path and a JSON module, both resolved against
// the base directory at componentize time, awaits a dynamic import and a timer
// at top level, and writes to stdout. The export is a `const` declared after the
// `await`s, so it is only initialized once the top level has finished
// evaluating.

import { name } from "./lib/name.js";
import config from "./lib/config.json" with { type: "json" };

const { late } = await import("./lib/late.js");
const suffix = await new Promise((resolve) => setTimeout(() => resolve("after a timer"), 20));
console.log("top level finished");

export const greeting = () => `hello ${name} ${late} ${suffix}${config.punctuation}`;
