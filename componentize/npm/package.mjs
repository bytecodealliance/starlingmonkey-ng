// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

// Assemble the npm packages of `starling-componentize`: the package in
// `starling-componentize/`, which holds the release runtime and resolves the
// binary, and one package per platform holding it, named
// `<scope>/starling-componentize-<platform>`.
//
// Usage:
//
//   node package.mjs --out <dir> [--version 0.3.0] [--scope @bytecodealliance] \
//     --runtime <release starling.wasm> --debug-runtime <debug starling.wasm> \
//     [--pack <tarballs dir>] [--all-platforms] <platform>=<binary> [<platform>=<binary> ...]
//
// `<platform>` is a key of `platforms.json`, such as `linux-x64`, and
// `<binary>` the `starling-componentize` built for it. The debug runtime is not
// packaged: the resolver package records its SHA-256 digest, and downloads it
// from the GitHub release on first use. Each package is written
// to `<dir>/<package directory name>`, ready for `npm pack` or `npm publish`.
// The resolver package lists every platform of `platforms.json` as an optional
// dependency, whether or not a binary for it was given, and `--all-platforms`
// fails unless one was given for each. The version defaults to the one in
// `componentize/Cargo.toml`. With `--pack`, `npm pack` writes each package's
// tarball to `<tarballs dir>`.

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, copyFileSync, cpSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const NAME = 'starling-componentize';
const here = path.dirname(fileURLToPath(import.meta.url));
const platforms = JSON.parse(readFileSync(path.join(here, 'platforms.json'), 'utf8'));
const cargoToml = readFileSync(path.join(here, '..', 'Cargo.toml'), 'utf8');
// The GitHub repository whose release of each version has the debug runtime, and which the
// packages name as theirs. It is not read from `Cargo.toml`, whose `repository` is the upstream's.
const repository = 'https://github.com/bytecodealliance/starlingmonkey-ng';

const { values, positionals } = parseArgs({
  options: {
    version: { type: 'string', default: cargoToml.match(/^version = "(.*)"$/m)[1] },
    out: { type: 'string' },
    scope: { type: 'string', default: '@bytecodealliance' },
    runtime: { type: 'string' },
    'debug-runtime': { type: 'string' },
    pack: { type: 'string' },
    'all-platforms': { type: 'boolean', default: false },
  },
  allowPositionals: true,
});
if (!values.out) {
  throw new Error('--out is required');
}
if (!values.runtime || !values['debug-runtime']) {
  throw new Error('--runtime and --debug-runtime are required');
}
const { version, out, scope, pack } = values;
const given = new Set(positionals.map(arg => arg.split('=')[0]));
const missing = Object.keys(platforms).filter(platform => !given.has(platform));
if (values['all-platforms'] && missing.length > 0) {
  throw new Error(`no binary for the platforms ${missing.join(', ')} of platforms.json`);
}

const common = {
  version,
  license: 'Apache-2.0 WITH LLVM-exception',
  repository: {
    type: 'git',
    url: `git+${repository}.git`,
    directory: 'componentize',
  },
  publishConfig: { access: 'public', provenance: true },
};

function writeJson(file, value) {
  writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

for (const arg of positionals) {
  const [platform, binary] = arg.split('=');
  const target = platforms[platform];
  if (!target || !binary) {
    throw new Error(`\`${arg}\` is not <platform>=<binary> with a platform of platforms.json`);
  }
  const dir = path.join(out, `${NAME}-${platform}`);
  mkdirSync(path.join(dir, 'bin'), { recursive: true });
  const exe = target.os === 'win32' ? `${NAME}.exe` : NAME;
  copyFileSync(binary, path.join(dir, 'bin', exe));
  chmodSync(path.join(dir, 'bin', exe), 0o755);
  writeJson(path.join(dir, 'package.json'), {
    name: `${scope}/${NAME}-${platform}`,
    description: `The ${platform} binary of ${scope}/${NAME}`,
    ...common,
    os: [target.os],
    cpu: [target.cpu],
    files: ['bin'],
  });
  writeFileSync(
    path.join(dir, 'README.md'),
    `# ${scope}/${NAME}-${platform}\n\nThe \`${NAME}\` binary for ${platform}. Install \`${scope}/${NAME}\`, which depends on it.\n`,
  );
}

const main = path.join(out, NAME);
cpSync(path.join(here, NAME), main, { recursive: true });
chmodSync(path.join(main, 'bin', `${NAME}.js`), 0o755);
// npm packs only what is in the package's directory, so the release runtime is
// copied in, for `runtimePath()` in `index.js`. The debug runtime is not: only
// its digest is recorded below, and `index.js` downloads it on first use.
copyFileSync(values.runtime, path.join(main, 'starling.wasm'));
writeJson(path.join(main, 'package.json'), {
  name: `${scope}/${NAME}`,
  description: 'Build WebAssembly components from JavaScript on the StarlingMonkey runtime',
  ...common,
  main: 'index.js',
  bin: { [NAME]: `bin/${NAME}.js` },
  files: ['index.js', 'bin', 'starling.wasm'],
  engines: { node: '>=22' },
  optionalDependencies: Object.fromEntries(
    Object.keys(platforms).map(platform => [`${scope}/${NAME}-${platform}`, version]),
  ),
  // Read by `index.js` to name the platform packages.
  starlingComponentize: {
    scope,
    name: NAME,
    // Read by `index.js` to download and verify the debug runtime, which the
    // release workflow attaches to the GitHub release.
    debugRuntime: {
      url: `${repository}/releases/download/v${version}/starling-debug.wasm`,
      sha256: createHash('sha256').update(readFileSync(values['debug-runtime'])).digest('hex'),
    },
  },
});

if (pack) {
  mkdirSync(pack, { recursive: true });
  for (const dir of [...[...given].map(platform => `${NAME}-${platform}`), NAME]) {
    execFileSync('npm', ['pack', '--silent', '--pack-destination', path.resolve(pack)], {
      cwd: path.join(out, dir),
      stdio: ['ignore', 'ignore', 'inherit'],
    });
  }
}
