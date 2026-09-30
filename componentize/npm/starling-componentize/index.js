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
  const platform = `${process.platform}-${process.arch}${libcSuffix()}`;
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

/**
 * `-musl` on a Linux system whose C library is not glibc, such as Alpine's musl,
 * and otherwise empty. The Linux packages hold glibc binaries, so no package
 * provides a platform with this suffix. Node.js reports a glibc version only
 * when it runs against glibc. A Node.js built without diagnostic reports counts
 * as glibc.
 */
function libcSuffix() {
  if (process.platform !== 'linux' || typeof process.report?.getReport !== 'function') {
    return '';
  }
  return process.report.getReport().header.glibcVersionRuntime ? '' : '-musl';
}

module.exports = { binaryPath };
