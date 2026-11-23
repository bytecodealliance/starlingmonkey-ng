#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

'use strict';

// Run this platform's `starling-componentize` with the arguments given. Unless
// `--runtime` or `$STARLING_RUNTIME` names a runtime, it uses the release
// runtime of this package, or with `STARLING_RUNTIME_BUILD=debug` the debug
// runtime, which is downloaded on first use, but not just to print help.
const { spawnSync } = require('node:child_process');
const { binaryPath, runtimePath, debugRuntimePath } = require('..');

// The arguments before `--`, after which an argument is never an option.
function options(args) {
  const end = args.indexOf('--');
  return end === -1 ? args : args.slice(0, end);
}

// Whether `args` name a runtime with `--runtime`, which the binary prefers to
// `$STARLING_RUNTIME`.
function hasRuntimeArg(args) {
  return options(args).some(arg => arg === '--runtime' || arg.startsWith('--runtime='));
}

// Whether `args` only ask for help, which needs no runtime.
function isHelp(args) {
  const opts = options(args);
  return opts.some(arg => arg === '-h' || arg === '--help') || opts.find(arg => !arg.startsWith('-')) === 'help';
}

async function main() {
  const args = process.argv.slice(2);
  const env = { ...process.env };
  if (!env.STARLING_RUNTIME && !hasRuntimeArg(args) && !isHelp(args)) {
    const build = env.STARLING_RUNTIME_BUILD || 'release';
    if (build === 'release') {
      env.STARLING_RUNTIME = runtimePath();
    } else if (build === 'debug') {
      env.STARLING_RUNTIME = await debugRuntimePath();
    } else {
      throw new Error(`STARLING_RUNTIME_BUILD is \`${build}\`, not \`release\` or \`debug\``);
    }
  }
  const result = spawnSync(binaryPath(), args, { stdio: 'inherit', env });
  if (result.error) {
    throw result.error;
  }
  process.exitCode = result.status ?? 1;
}

main().catch(error => {
  console.error(`starling-componentize: ${error.message}`);
  process.exitCode = 1;
});
