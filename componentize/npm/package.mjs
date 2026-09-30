// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

// Assemble the npm packages of `starling-componentize`: the package in
// `starling-componentize/`, which resolves the binary, and one package per
// platform holding it, named `<scope>/starling-componentize-<platform>`.
//
// Usage:
//
//   node package.mjs --version 0.3.0 --out <dir> [--scope @bytecodealliance] \
//     <platform>=<binary> [<platform>=<binary> ...]
//
// `<platform>` is a key of `platforms.json`, such as `linux-x64`, and
// `<binary>` the `starling-componentize` built for it. Each package is written
// to `<dir>/<package directory name>`, ready for `npm pack` or `npm publish`.
// The resolver package lists every platform of `platforms.json` as an optional
// dependency, whether or not a binary for it was given.

import { chmodSync, copyFileSync, cpSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const NAME = 'starling-componentize';
const here = path.dirname(fileURLToPath(import.meta.url));
const platforms = JSON.parse(readFileSync(path.join(here, 'platforms.json'), 'utf8'));
const cargoToml = readFileSync(path.join(here, '..', 'Cargo.toml'), 'utf8');

const { values, positionals } = parseArgs({
  options: {
    version: { type: 'string' },
    out: { type: 'string' },
    scope: { type: 'string', default: '@bytecodealliance' },
  },
  allowPositionals: true,
});
if (!values.version || !values.out) {
  throw new Error('--version and --out are required');
}
const { version, out, scope } = values;

const common = {
  version,
  license: 'Apache-2.0 WITH LLVM-exception',
  repository: {
    type: 'git',
    url: `git+${cargoToml.match(/^repository = "(.*)"$/m)[1]}.git`,
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
    ...(target.libc && { libc: [target.libc] }),
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
writeJson(path.join(main, 'package.json'), {
  name: `${scope}/${NAME}`,
  description: 'Build WebAssembly components from JavaScript on the StarlingMonkey runtime',
  ...common,
  main: 'index.js',
  bin: { [NAME]: `bin/${NAME}.js` },
  files: ['index.js', 'bin'],
  engines: { node: '>=18' },
  optionalDependencies: Object.fromEntries(
    Object.keys(platforms).map(platform => [`${scope}/${NAME}-${platform}`, version]),
  ),
  // Read by `index.js` to name the platform packages.
  starlingComponentize: { scope, name: NAME },
});
