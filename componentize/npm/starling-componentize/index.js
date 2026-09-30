// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

'use strict';

const path = require('node:path');

const { scope, name } = require('./package.json').starlingComponentize;

/**
 * The path of the `starling-componentize` binary for this platform, from the
 * platform package npm installed as an optional dependency.
 *
 * Throws if this platform has no package, or if its package is not installed,
 * such as after an install with `--no-optional`.
 */
function binaryPath() {
  const platform = `${process.platform}-${process.arch}`;
  const packageName = `${scope}/${name}-${platform}`;
  const exe = process.platform === 'win32' ? `${name}.exe` : name;
  let packageJson;
  try {
    packageJson = require.resolve(`${packageName}/package.json`);
  } catch {
    throw new Error(
      `${packageName} is not installed. It provides \`${name}\` for ${platform}. ` +
        'Reinstall without --no-optional or --omit=optional, or, if there is no ' +
        'package for this platform, build `starling-componentize` from source.',
    );
  }
  return path.join(path.dirname(packageJson), 'bin', exe);
}

module.exports = { binaryPath };
