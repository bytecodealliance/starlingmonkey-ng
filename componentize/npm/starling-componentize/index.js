// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { scope, name, debugRuntime } = require('./package.json').starlingComponentize;

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

/** The path of the release runtime (`starling.wasm`) this package holds. */
function runtimePath() {
  return path.join(__dirname, 'starling.wasm');
}

/**
 * The path of the debug runtime of this package's version, which is built with
 * SpiderMonkey's debug checks. It is not part of the package: the first call
 * downloads it from the GitHub release, or from `$STARLING_DEBUG_RUNTIME_URL`,
 * checks its SHA-256 digest against the one this package records, and caches
 * it in `$STARLING_CACHE_DIR`, by default `starling-componentize` in the user's
 * cache directory.
 */
async function debugRuntimePath() {
  const cacheDir = process.env.STARLING_CACHE_DIR || path.join(userCacheDir(), name);
  const cached = path.join(cacheDir, `starling-debug-${debugRuntime.sha256}.wasm`);
  // The cache may be shared, or hold a truncated file, so a cached file is
  // used only if its digest still matches. One that does not is replaced.
  if (isVerified(cached)) {
    return cached;
  }
  const url = process.env.STARLING_DEBUG_RUNTIME_URL || debugRuntime.url;
  process.stderr.write(`Downloading the debug runtime from ${url}\n`);
  const hint =
    'A package built from an unreleased version has no debug runtime to download: set ' +
    'STARLING_DEBUG_RUNTIME_URL to where it is, or STARLING_RUNTIME to its path.';
  let response;
  try {
    response = await fetch(url);
  } catch (error) {
    throw new Error(`downloading the debug runtime from ${url}: ${error.cause?.message ?? error.message}`);
  }
  if (!response.ok) {
    throw new Error(
      `downloading the debug runtime from ${url}: ${response.status} ${response.statusText}. ${hint}`,
    );
  }
  const bytes = Buffer.from(await response.arrayBuffer());
  const sha256 = sha256Of(bytes);
  if (sha256 !== debugRuntime.sha256) {
    throw new Error(`the debug runtime from ${url} has SHA-256 ${sha256}, not ${debugRuntime.sha256}. ${hint}`);
  }
  fs.mkdirSync(cacheDir, { recursive: true });
  const tmp = `${cached}.${process.pid}.tmp`;
  try {
    fs.writeFileSync(tmp, bytes);
    fs.renameSync(tmp, cached);
  } catch (error) {
    // Another process may have cached it first: on Windows, renaming onto a
    // file it holds open fails. Its copy will do, if it is verified.
    if (!isVerified(cached)) {
      throw error;
    }
  } finally {
    fs.rmSync(tmp, { force: true });
  }
  return cached;
}

function sha256Of(bytes) {
  return crypto.createHash('sha256').update(bytes).digest('hex');
}

// Whether `file` exists and has the debug runtime's digest. It is hashed in
// chunks, since the debug runtime is too large to read into memory each time.
function isVerified(file) {
  let fd;
  try {
    fd = fs.openSync(file, 'r');
  } catch {
    return false;
  }
  try {
    const hash = crypto.createHash('sha256');
    const chunk = Buffer.allocUnsafe(1 << 20);
    for (let n; (n = fs.readSync(fd, chunk, 0, chunk.length, null)) > 0; ) {
      hash.update(chunk.subarray(0, n));
    }
    return hash.digest('hex') === debugRuntime.sha256;
  } catch {
    return false;
  } finally {
    fs.closeSync(fd);
  }
}

function userCacheDir() {
  if (process.platform === 'win32') {
    return process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
  }
  if (process.platform === 'darwin') {
    return path.join(os.homedir(), 'Library', 'Caches');
  }
  return process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache');
}

module.exports = { binaryPath, runtimePath, debugRuntimePath };
