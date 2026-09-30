// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Imports the main module `entry.js` back.

import * as entry from "../entry.js";

export const entryType = () => typeof entry.marker;
