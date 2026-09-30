#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

'use strict';

// Run this platform's `starling-componentize` with the arguments given.
const { spawnSync } = require('node:child_process');
const { binaryPath } = require('..');

const result = spawnSync(binaryPath(), process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  throw result.error;
}
process.exitCode = result.status ?? 1;
