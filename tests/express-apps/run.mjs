#!/usr/bin/env node
// Runs each Express app under Node.js and under StarlingMonkey, sends both the requests of the
// app's `scenario.mjs`, and compares the responses.
//
// Usage:
//   node tests/express-apps/run.mjs [app...] [--runtime=path] [--mode=native|serve|wasm|spin]
//                                   [--wasm=path] [--spin=path] [--verbose]
//
// `--mode=native` (the default) runs `starlingmonkey --node-compat --node-http-listen <entry>`,
// `--mode=serve` runs it with `--serve <port>` instead, and `--mode=wasm` runs `starling.wasm`
// under `wasmtime run` with `--node-http-listen`. `--mode=spin` runs the Spin application
// `scripts/build-spin-app.sh` builds with `spin up`, once for each app, and sends the requests to
// the app's route. Node.js then runs the app's `spin.mjs`, which mounts the app at the same
// route, and the Koa portal's entry, which Spin serves at `/`. Apps without `node_modules` are
// installed with `npm ci` first.
//
// A scenario module exports `entry`, the app's main file, and a default async function that gets
// a client `t`. It can also export `setup(ctx)`, which returns `{ env, teardown }`, `masks`,
// `serveFlags`, the runtime flags `--mode=serve` adds, and `modes`, the modes it runs in (all by
// default). `t.request(method, path, options)` and its shorthands `t.get` and `t.post` record
// each response's status, the headers in `options.headers_recorded` and `RECORDED_HEADERS`, and
// its body, after replacing the matches of `options.mask` (an array of `[RegExp, replacement]`),
// the scenario's `masks` and `DEFAULT_MASKS`. `options.slot` records the response at the position
// of the request rather than of the response, for requests that complete after later ones.
// `options.ignoreIn` maps modes to the reason a response isn't compared in them, which is recorded
// instead. `t.note(name, value)` records a value the scenario computed itself. A scenario can
// also export `spinMasks`, which `--mode=spin` adds to `masks`.

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync, readdirSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, resolve, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';
import { freePort, waitForListener } from './servers.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '../..');
// The directory Cargo builds into: `CARGO_TARGET_DIR`, or the repository's `target`.
const TARGET_DIR = process.env.CARGO_TARGET_DIR ? resolve(process.env.CARGO_TARGET_DIR) : join(ROOT, 'target');

const args = process.argv.slice(2);
const opt = (name, fallback) => {
  const a = args.find((x) => x.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : fallback;
};
const verbose = args.includes('--verbose');
const mode = opt('mode', 'native');
const runtime = resolve(opt('runtime', join(TARGET_DIR, 'release/starlingmonkey')));
const wasm = resolve(opt('wasm', join(TARGET_DIR, 'wasm32-wasip3/release/starling.wasm')));
const spinManifest = resolve(opt('spin', join(ROOT, 'target/spin-app/spin.toml')));
const spinBin = join(ROOT, 'deps/spin-bin/spin');
const nodeBin = process.execPath;
const npmBin = join(dirname(nodeBin), 'npm');

const RECORDED_HEADERS = [
  'content-type',
  'content-length',
  'content-encoding',
  'content-disposition',
  'content-range',
  'accept-ranges',
  'cache-control',
  'etag',
  'last-modified',
  'location',
  'vary',
  'allow',
  'set-cookie',
  'x-powered-by',
  'transfer-encoding',
];

const DEFAULT_MASKS = [
  // HTTP dates, as in `Last-Modified` and cookie expiry.
  [/\b(Mon|Tue|Wed|Thu|Fri|Sat|Sun), \d\d \w{3} \d{4} \d\d:\d\d:\d\d GMT/g, '<http-date>'],
  // ISO timestamps.
  [/\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z/g, '<iso-date>'],
  // Ports of the app under test.
  [/127\.0\.0\.1:\d+/g, '127.0.0.1:<port>'],
  [/localhost:\d+/g, 'localhost:<port>'],
];

/** The real path of `appDir`'s `node_modules`, in a list, if it is a symbolic link. */
function linkedNodeModules(appDir) {
  const nodeModules = join(appDir, 'node_modules');
  if (!existsSync(nodeModules)) return [];
  const real = realpathSync(nodeModules);
  return real === join(realpathSync(appDir), 'node_modules') ? [] : [real];
}
function isText(type) {
  return /^(text\/|application\/(json|javascript|xml|x-ndjson|x-www-form-urlencoded))|\+json|\+xml/.test(
    type || '',
  );
}

class Client {
  constructor(base, label) {
    this.base = base;
    this.label = label;
    this.records = [];
    this.cookies = new Map();
    this.masks = [];
  }
  applyMasks(s, masks) {
    for (const [re, rep] of [...masks, ...this.masks, ...DEFAULT_MASKS]) s = s.replace(re, rep);
    return s;
  }
  async request(method, path, options = {}) {
    const headers = { ...(options.headers || {}) };
    let body = options.body;
    if (options.json !== undefined) {
      body = JSON.stringify(options.json);
      headers['content-type'] ??= 'application/json';
    } else if (options.form !== undefined) {
      body = new URLSearchParams(options.form).toString();
      headers['content-type'] ??= 'application/x-www-form-urlencoded';
    }
    if (options.jar !== false && this.cookies.size && !headers.cookie) {
      headers.cookie = [...this.cookies].map(([k, v]) => `${k}=${v}`).join('; ');
    }
    const init = { method, headers, body, redirect: 'manual' };
    if (body && typeof body.pipe === 'function') init.duplex = 'half';
    init.signal = options.signal ?? AbortSignal.timeout(options.timeout ?? 10000);
    const slot = options.slot ? this.records.push(null) - 1 : -1;
    const record = (rec) => (slot >= 0 ? (this.records[slot] = rec) : this.records.push(rec));
    let res;
    try {
      res = await fetch(this.base + path, init);
    } catch (e) {
      const rec = { req: `${method} ${path}`, error: String(e.cause?.code || e.message) };
      record(rec);
      return { status: 0, error: e, headers: new Headers(), text: '', json: undefined };
    }
    for (const c of res.headers.getSetCookie()) {
      const [pair] = c.split(';');
      const eq = pair.indexOf('=');
      const name = pair.slice(0, eq).trim();
      const value = pair.slice(eq + 1).trim();
      if (/expires=Thu, 01 Jan 1970/i.test(c) || value === '') this.cookies.delete(name);
      else this.cookies.set(name, value);
    }
    let text;
    let bodyRecord;
    const type = res.headers.get('content-type');
    let buf;
    try {
      if (options.stream) text = await options.stream(res);
      else buf = Buffer.from(await res.arrayBuffer());
    } catch (e) {
      text = `<body error: ${e.cause?.code ?? e.name}>`;
    }
    if (!buf) {
      bodyRecord = this.applyMasks(text, options.mask || []);
    } else {
      if (isText(type) || options.text) {
        text = buf.toString('utf8');
        bodyRecord = this.applyMasks(text, options.mask || []);
      } else {
        text = buf.toString('latin1');
        bodyRecord = `<${buf.length} bytes sha256=${createHash('sha256').update(buf).digest('hex').slice(0, 16)}>`;
      }
    }
    const recorded = {};
    for (const h of [...RECORDED_HEADERS, ...(options.headers_recorded || [])]) {
      if (options.ignoreHeaders?.includes(h)) continue;
      const v = h === 'set-cookie' ? res.headers.getSetCookie().join('\n') : res.headers.get(h);
      // Hosts leave `Content-Length` out of responses without content, which Node.js sends.
      if (h === 'content-length' && (res.status === 204 || res.status === 304)) continue;
      if (v !== null && v !== '') recorded[h] = this.applyMasks(v, options.mask || []);
    }
    const req = options.name ? `${method} ${path} (${options.name})` : `${method} ${path}`;
    const ignored = options.ignoreIn?.[mode];
    const rec = ignored
      ? { req, ignored }
      : { req, status: res.status, headers: recorded, body: options.ignoreBody ? '<ignored>' : bodyRecord };
    record(rec);
    let json;
    try {
      json = JSON.parse(text);
    } catch {}
    return { status: res.status, headers: res.headers, text, json };
  }
  get(path, options) {
    return this.request('GET', path, options);
  }
  post(path, options) {
    return this.request('POST', path, options);
  }
  // Records a value the scenario computed itself.
  note(name, value) {
    this.records.push({ note: name, value });
  }
}

// The route the Spin application serves the app `name` at.
function spinRoute(name) {
  return name === 'koa-portal' ? '' : `/${name}`;
}

function startApp(kind, appDir, scenario, port, env) {
  const { entry } = scenario;
  let cmd;
  let argv;
  if (kind === 'node') {
    cmd = nodeBin;
    argv = [mode === 'spin' && existsSync(join(appDir, 'spin.mjs')) ? 'spin.mjs' : entry];
  } else if (mode === 'spin') {
    // One instance serves every request, so the app keeps its state between them.
    cmd = spinBin;
    argv = [
      'up',
      '--from',
      spinManifest,
      '--listen',
      `127.0.0.1:${port}`,
      '--allow-transient-write',
      '--log-dir',
      join(env.DATA_DIR, 'spin-logs'),
      '--max-instance-reuse-count',
      '1000000',
      '--idle-instance-timeout',
      '3600s',
    ];
  } else if (mode === 'native') {
    cmd = runtime;
    argv = ['--node-compat', '--node-http-listen', entry];
  } else if (mode === 'serve') {
    cmd = runtime;
    argv = ['--node-compat', ...(scenario.serveFlags ?? []), '--serve', String(port), entry];
  } else if (mode === 'wasm') {
    cmd = 'wasmtime';
    argv = [
      'run',
      `--dir=${appDir}::/app`,
      // WASI doesn't follow a symbolic link out of a preopened directory, so a linked
      // `node_modules` is preopened at its guest path.
      ...linkedNodeModules(appDir).map((host) => `--dir=${host}::/app/node_modules`),
      ...(env.DATA_DIR ? [`--dir=${env.DATA_DIR}::${env.DATA_DIR}`] : []),
      '-Sinherit-network=y,tcp=y,udp=y,http=y,p3=y,inherit-env=y',
      '-Wcomponent-model-async=y',
      wasm,
      '--node-compat',
      '--node-http-listen',
      `/app/${entry}`,
    ];
  } else {
    throw new Error(`unknown mode ${mode}`);
  }
  const child = spawn(cmd, argv, {
    cwd: appDir,
    env: { ...process.env, ...env, PORT: String(port), NODE_ENV: 'production' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.output = '';
  child.stdout.on('data', (d) => (child.output += d));
  child.stderr.on('data', (d) => (child.output += d));
  return child;
}

async function runUnder(kind, appDir, scenario) {
  const port = await freePort();
  const ctx = { kind, mode, name: basename(appDir), appDir, dataDir: mkdtempSync(join(tmpdir(), 'express-app-')) };
  const extra = scenario.setup ? await scenario.setup(ctx) : {};
  const env = { DATA_DIR: ctx.dataDir, ...(extra.env || {}) };
  const child = startApp(kind, appDir, scenario, port, env);
  const route = mode === 'spin' ? spinRoute(ctx.name) : '';
  const client = new Client(`http://127.0.0.1:${port}${route}`, kind);
  client.ctx = ctx;
  client.masks = [...(scenario.masks || []), ...(mode === 'spin' ? scenario.spinMasks || [] : [])];
  let error;
  const closed = new Promise((res) => child.on('close', res));
  try {
    // Spin compiles the components before it listens.
    await waitForListener(child, port, { timeoutMs: mode === 'spin' ? 300000 : 30000 });
    await scenario.default(client);
    // A server that exited, for example after a crash, fails the run even if every response
    // arrived before it did.
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`the server exited during the scenario (${child.exitCode ?? child.signalCode})`);
    }
  } catch (e) {
    error = e;
  } finally {
    child.kill('SIGKILL');
    await closed;
    await extra.teardown?.();
    rmSync(ctx.dataDir, { recursive: true, force: true });
  }
  return { records: client.records, error, output: child.output };
}

function show(v) {
  return JSON.stringify(v, null, 2);
}

async function runApp(name) {
  const appDir = join(HERE, name);
  if (!existsSync(join(appDir, 'node_modules'))) {
    execFileSync(npmBin, ['ci', '--no-audit', '--no-fund'], { cwd: appDir, stdio: 'inherit' });
  }
  const scenario = await import(pathToFileURL(join(appDir, 'scenario.mjs')));
  const spinless = mode === 'spin' && name !== 'koa-portal' && !existsSync(join(appDir, 'spin.mjs'));
  if (spinless || (scenario.modes && !scenario.modes.includes(mode))) {
    console.log(`SKIP ${name} (not run with --mode=${mode})`);
    return true;
  }
  const expected = await runUnder('node', appDir, scenario);
  if (expected.error) {
    console.log(`${name}: ERROR under Node.js: ${expected.error.stack}\n${expected.output}`);
    return false;
  }
  const actual = await runUnder('starling', appDir, scenario);
  let diffs = 0;
  const n = Math.max(expected.records.length, actual.records.length);
  for (let i = 0; i < n; i++) {
    let e = expected.records[i];
    let a = actual.records[i];
    // With `--serve` and under Spin, the host frames a body it has in memory by its length where
    // Node.js sends it chunked. Under Spin, the host's HTTP client also leaves `Transfer-Encoding`
    // out of the upstream responses a proxy copies. So in those modes, a chunked body under
    // Node.js matches a body of either framing.
    const hostFramed = mode === 'serve' || mode === 'spin';
    if (hostFramed && e?.headers?.['transfer-encoding'] === 'chunked' && a?.headers) {
      const { 'transfer-encoding': _te, ...expectedHeaders } = e.headers;
      const { 'transfer-encoding': _ate, 'content-length': _cl, ...actualHeaders } = a.headers;
      e = { ...e, headers: expectedHeaders };
      a = { ...a, headers: actualHeaders };
    }
    if (isDeepStrictEqual(e, a)) continue;
    diffs++;
    if (diffs <= 10) {
      console.log(`--- ${name} #${i} ${e?.req ?? e?.note ?? a?.req}`);
      console.log(`node:     ${show(e)}`);
      console.log(`starling: ${show(a)}`);
    }
  }
  if (actual.error) {
    console.log(`${name}: scenario error under StarlingMonkey: ${actual.error.stack}`);
  }
  const ok = diffs === 0 && !actual.error;
  if (!ok || verbose) console.log(`${name} output under StarlingMonkey:\n${actual.output}`);
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name} (${expected.records.length} requests, ${diffs} differ)`);
  return ok;
}

const apps = args.filter((a) => !a.startsWith('--'));
const all = readdirSync(HERE, { withFileTypes: true })
  .filter((d) => d.isDirectory() && existsSync(join(HERE, d.name, 'scenario.mjs')))
  .map((d) => d.name)
  .sort();
let failed = 0;
for (const app of apps.length ? apps : all) {
  if (!(await runApp(app))) failed++;
}
process.exit(failed ? 1 : 0);
