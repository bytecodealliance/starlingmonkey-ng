// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A main module that a module it imports imports back. The importer gets this
// module itself, so its top level runs once.

import { entryType } from "./lib/back.js";

globalThis.entryRuns = (globalThis.entryRuns ?? 0) + 1;
export const marker = {};
export const greeting = () => `${globalThis.entryRuns} ${entryType()}`;
