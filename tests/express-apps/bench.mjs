#!/usr/bin/env node
// Benchmarks the apps of `tests/express-apps` under StarlingMonkey, Node.js, Deno and Bun, with
// `oha` as the load generator.
//
// Usage:
//   node tests/express-apps/bench.mjs [--configs=listen,serve,...] [--apps=rest-api,...]
//                                     [--routes=substring] [--duration=5] [--warmup=1]
//                                     [--concurrency=1,32] [--repeat=3] [--startup-runs=5]
//                                     [--runtime=path] [--spin-app=path] [--reuse=N]
//                                     [--client-threads=N]
//                                     [--out=target/express-bench/results.json]
//   node tests/express-apps/bench.mjs --report=target/express-bench/results.json
//
// Every config runs the same entry: an Express app's `spin.mjs`, which mounts the app at
// `/<app>`, the Koa portal's `app.mjs` at `/`, and `realtime`'s `app.js` at `/`. The configs:
//
// - `listen`: `starlingmonkey --node-compat --node-http-listen`.
// - `serve`: `starlingmonkey --node-compat --serve`, with `--allow-blocking-host-io` for `ssr-site`.
// - `wasmtime`: the app's component from `scripts/build-spin-app.sh` under `wasmtime serve`, with
//   the mounts and environment `spin.toml` gives it, and wasmtime's default instance reuse.
// - `spin`: the whole Spin application under `spin up`, with Spin's default instance reuse.
// - `node`, `deno` and `bun`.
//
// `--runtime` names the `starlingmonkey` binary (`release/starlingmonkey` in `CARGO_TARGET_DIR` or
// `target` by default), and `--spin-app` the directory `scripts/build-spin-app.sh` builds
// (`target/spin-app`). `--reuse=N` passes `--max-instance-reuse-count N` to `wasmtime` and `spin`.
// `--client-threads=N` sets the number of threads `oha` runs (the number of CPUs by default). `oha`
// runs on the same machine as the server.
//
// Each repeat starts every (app, config) pair once, in turn, so that background load affects the
// configs alike. A run measures startup, then runs each route for `--warmup` seconds at the
// highest concurrency and then for `--duration` seconds at each concurrency. The routes that add
// state to the app run last. A route's requests per second count the responses with the route's
// expected status, and its latencies are those of the responses with a 2xx status. Startup is
// measured `--startup-runs` times in separate processes: the time from spawning the server until
// it accepts a TCP connection, and the latency of the first request after that. RSS is the sum
// over the server's process tree: after the first request, and the maximum sampled during the run.
//
// `--report` prints the Markdown tables of a results file: the median of the repeats, with half
// the range between the smallest and largest repeat as `±`.

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync, cpSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir, cpus, totalmem, loadavg } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { freePort, processTree, waitForListener } from './servers.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '../..');
// The directory Cargo builds into: `CARGO_TARGET_DIR`, or the repository's `target`.
const TARGET_DIR = process.env.CARGO_TARGET_DIR ? resolve(process.env.CARGO_TARGET_DIR) : join(ROOT, 'target');

const args = process.argv.slice(2);
const opt = (name, fallback) => {
  const a = args.find((x) => x.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : fallback;
};
const list = (name, fallback) => opt(name, fallback).split(',').filter(Boolean);
const SPIN_APP = resolve(opt('spin-app', join(ROOT, 'target/spin-app')));
const clientThreads = Number(opt('client-threads', String(cpus().length)));

const BIN = {
  starling: resolve(opt('runtime', join(TARGET_DIR, 'release/starlingmonkey'))),
  wasmtime: 'wasmtime',
  spin: join(ROOT, 'deps/spin-bin/spin'),
  node: join(ROOT, 'deps/node-bin/bin/node'),
  deno: join(ROOT, 'deps/deno-bin/deno'),
  bun: join(ROOT, 'deps/bun-bin/bun'),
  oha: 'oha',
};

const ALL_CONFIGS = ['listen', 'serve', 'wasmtime', 'spin', 'node', 'deno', 'bun'];
const WASM_CONFIGS = ['wasmtime', 'spin'];

// The mounts and environment of each app's component, as `spin.toml` declares them.
const COMPONENT = {
  'koa-portal': { mounts: [['koa-portal/public', '/public']], env: {} },
  'rest-api': { mounts: [], env: {} },
  'ssr-site': {
    mounts: [
      ['ssr-site/views', '/views'],
      ['ssr-site/public', '/public'],
      ['ssr-site/data', '/data'],
    ],
    env: { DATA_DIR: '/data' },
  },
  files: { mounts: [['files/data', '/data']], env: { DATA_DIR: '/data' } },
  auth: { mounts: [], env: {} },
  data: { mounts: [['data/data', '/data']], env: { DATA_DIR: '/data', UPSTREAM_URL: 'http://127.0.0.1:9' } },
};

const json = (value) => ({ body: JSON.stringify(value), headers: { 'content-type': 'application/json' } });
const IDENTITY = { 'accept-encoding': 'identity' };
const GZIP = { 'accept-encoding': 'gzip' };

/** `fetch(url, init)`, which throws unless the response has the status `expected`. */
async function fetchExpecting(expected, url, init) {
  const res = await fetch(url, init);
  if (res.status !== expected) throw new Error(`${init?.method ?? 'GET'} ${url} got ${res.status}, not ${expected}`);
  return res;
}

// Each app's routes. `prepare(base)` runs once per process, before the routes, and returns
// headers every route sends, and throws if a response has another status than it expects.
// `mutates` routes add state to the app, and run after the others. Startup requests `probe`, or
// else the first route, with `GET`.
const APPS = {
  'koa-portal': {
    entry: 'app.mjs',
    prefix: '',
    routes: [
      { name: 'GET / (Handlebars page)', path: '/', headers: IDENTITY },
      { name: 'GET /api/apps (JSON)', path: '/api/apps', headers: IDENTITY },
      { name: 'GET /css/portal.css (static)', path: '/css/portal.css', headers: IDENTITY },
      { name: 'GET /css/portal.css (static, gzip)', path: '/css/portal.css', headers: GZIP },
      {
        name: 'POST /api/feedback (JSON, validated)',
        method: 'POST',
        path: '/api/feedback',
        ...json({ name: 'Bench', message: 'a benchmark note', app: 'rest-api', rating: 4 }),
        status: 201,
        mutates: true,
      },
    ],
  },
  'rest-api': {
    entry: 'spin.mjs',
    prefix: '/rest-api',
    routes: [
      { name: 'GET /health (text)', path: '/health', headers: IDENTITY },
      { name: 'GET /api/v1/big (JSON, 12 KB)', path: '/api/v1/big', headers: IDENTITY },
      { name: 'GET /api/v1/big (JSON, gzip)', path: '/api/v1/big', headers: GZIP },
      {
        name: 'POST /api/v1/echo (JSON)',
        method: 'POST',
        path: '/api/v1/echo?x=1',
        ...json({ hello: 'world', list: [1, 2, 3], nested: { a: true } }),
      },
      {
        name: 'POST /api/v1/todos (JSON, zod, 1 ms timer)',
        method: 'POST',
        path: '/api/v1/todos',
        ...json({ title: 'Benchmark todo', priority: 2, tags: ['bench'] }),
        status: 201,
        mutates: true,
      },
    ],
  },
  'ssr-site': {
    entry: 'spin.mjs',
    prefix: '/ssr-site',
    serveFlags: ['--allow-blocking-host-io'],
    routes: [
      { name: 'GET /static/css/site.css (static)', path: '/static/css/site.css', headers: IDENTITY },
      { name: 'GET /files/blob.bin (static, 70 KB)', path: '/files/blob.bin', headers: IDENTITY },
      // Without a cookie, each request creates a session.
      { name: 'GET / (EJS page, new session)', path: '/', headers: IDENTITY, mutates: true },
    ],
  },
  files: {
    entry: 'spin.mjs',
    prefix: '/files',
    routes: [
      { name: 'GET /stream/numbers (Readable.pipe, 1000 chunks)', path: '/stream/numbers?n=1000' },
      {
        name: 'POST /echo-gzip (request body stream)',
        method: 'POST',
        path: '/echo-gzip',
        body: 'x'.repeat(4096),
        headers: { 'content-type': 'text/plain' },
      },
    ],
  },
  auth: {
    entry: 'spin.mjs',
    prefix: '/auth',
    probe: '/welcome?email=bench%40example.com',
    async prepare(base) {
      await fetchExpecting(201, `${base}/register`, {
        method: 'POST',
        ...json({ email: 'bench@example.com', password: 'benchmark1' }),
      });
      const res = await fetchExpecting(200, `${base}/login`, {
        method: 'POST',
        ...json({ email: 'bench@example.com', password: 'benchmark1' }),
      });
      const { access } = await res.json();
      return { authorization: `Bearer ${access}` };
    },
    routes: [
      { name: 'POST /sign (JWT sign + verify)', method: 'POST', path: '/sign', ...json({ payload: { sub: 'bench', role: 'user' } }) },
      { name: 'GET /me (JWT verify)', path: '/me' },
    ],
  },
  data: {
    entry: 'spin.mjs',
    prefix: '/data',
    env: { UPSTREAM_URL: 'http://127.0.0.1:9' },
    async prepare(base) {
      for (let i = 0; i < 3; i++) {
        await fetchExpecting(201, `${base}/posts`, { method: 'POST', ...json({ title: `Post ${i}`, body: 'x'.repeat(100) }) });
      }
      return {};
    },
    routes: [
      { name: 'GET /config (JSON)', path: '/config' },
      { name: 'GET /posts (lowdb file read)', path: '/posts' },
      { name: 'GET /export.ndjson (stream, 3 × 1 ms timer)', path: '/export.ndjson' },
    ],
  },
  realtime: {
    entry: 'app.js',
    prefix: '',
    configs: ['listen', 'node', 'deno', 'bun'],
    async prepare(base) {
      const login = await fetchExpecting(200, `${base}/login`, { method: 'POST', ...json({ user: 'bench' }) });
      const cookie = login.headers
        .getSetCookie()
        .map((c) => c.split(';')[0])
        .join('; ');
      for (let i = 0; i < 20; i++) {
        await fetchExpecting(201, `${base}/rooms/lobby/messages`, {
          method: 'POST',
          body: JSON.stringify({ text: `message ${i}` }),
          headers: { 'content-type': 'application/json', cookie },
        });
      }
      return {};
    },
    routes: [{ name: 'GET /rooms/lobby/messages (JSON)', path: '/rooms/lobby/messages' }],
  },
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// The summed RSS in KiB of `pid` and its descendants.
function treeRss(pid) {
  return processTree(pid).reduce((sum, [, , rss]) => sum + rss, 0);
}

function startServer(config, appName, port, dataDir) {
  const app = APPS[appName];
  const appDir = join(HERE, appName);
  const entry = app.entry;
  const env = { ...process.env, ...(app.env ?? {}), PORT: String(port), NODE_ENV: 'production', DATA_DIR: dataDir };
  const reuse = opt('reuse');
  let cmd;
  let argv;
  let cwd = appDir;
  switch (config) {
    case 'listen':
      [cmd, argv] = [BIN.starling, ['--node-compat', '--node-http-listen', entry]];
      break;
    case 'serve':
      [cmd, argv] = [BIN.starling, ['--node-compat', ...(app.serveFlags ?? []), '--serve', String(port), entry]];
      break;
    case 'wasmtime': {
      const component = COMPONENT[appName];
      const mounts = component.mounts.map(([src, dest]) => {
        // The data directories are copied, since the apps write to them.
        let from = join(SPIN_APP, src);
        if (dest === '/data') {
          from = join(dataDir, 'mount');
          cpSync(join(SPIN_APP, src), from, { recursive: true });
        }
        return `--dir=${from}::${dest}`;
      });
      const envFlags = Object.entries({ NODE_ENV: 'production', ...component.env }).map(([k, v]) => `--env=${k}=${v}`);
      cmd = BIN.wasmtime;
      argv = [
        'serve',
        `--addr=127.0.0.1:${port}`,
        '-Ccache=y',
        '-Scli=y,p3=y',
        '-Wcomponent-model-async=y',
        ...(reuse ? [`--max-instance-reuse-count=${reuse}`] : []),
        ...mounts,
        ...envFlags,
        join(SPIN_APP, `${appName}.wasm`),
      ];
      break;
    }
    case 'spin':
      cmd = BIN.spin;
      argv = [
        'up',
        '--from',
        join(SPIN_APP, 'spin.toml'),
        '--listen',
        `127.0.0.1:${port}`,
        '--allow-transient-write',
        '--quiet',
        '--state-dir',
        join(dataDir, 'spin-state'),
        '--log-dir',
        join(dataDir, 'spin-logs'),
        ...(reuse ? ['--max-instance-reuse-count', reuse] : []),
      ];
      cwd = dataDir;
      break;
    case 'node':
      [cmd, argv] = [BIN.node, [entry]];
      break;
    case 'deno':
      [cmd, argv] = [BIN.deno, ['run', '-A', '--unstable-detect-cjs', entry]];
      break;
    case 'bun':
      [cmd, argv] = [BIN.bun, [entry]];
      break;
    default:
      throw new Error(`unknown config ${config}`);
  }
  const child = spawn(cmd, argv, { cwd, env, stdio: ['ignore', 'pipe', 'pipe'], detached: true });
  servers.add(child);
  child.once('exit', () => servers.delete(child));
  child.output = '';
  child.stdout.on('data', (d) => (child.output = (child.output + d).slice(-4000)));
  child.stderr.on('data', (d) => (child.output = (child.output + d).slice(-4000)));
  return child;
}

/** The servers that are running, which are killed when this script exits. */
const servers = new Set();

/** Kill the process groups of the servers that are running. */
function killServers() {
  for (const child of servers) {
    try {
      process.kill(-child.pid, 'SIGKILL');
    } catch {}
  }
}
process.on('exit', killServers);
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    killServers();
    process.exit(130);
  });
}

function stopServer(child) {
  try {
    process.kill(-child.pid, 'SIGKILL');
  } catch {}
  return new Promise((r) => (child.exitCode !== null || child.signalCode ? r() : child.once('exit', r)));
}

// Starts the server and waits for its first successful response. Returns the startup timings.
async function launch(config, appName) {
  const app = APPS[appName];
  const port = await freePort();
  const dataDir = mkdtempSync(join(tmpdir(), 'express-bench-'));
  const t0 = performance.now();
  const child = startServer(config, appName, port, dataDir);
  const base = `http://127.0.0.1:${port}${app.prefix}`;
  // A server that fails to start is stopped before the error propagates.
  const fail = async (message) => {
    await shutdown({ child, dataDir });
    throw new Error(`${config} ${appName}: ${message}:\n${child.output}`);
  };
  let connectedAt;
  try {
    connectedAt = await waitForListener(child, port, { timeoutMs: 300_000, pollMs: 5 });
  } catch (e) {
    await fail(e.message);
  }
  const listening = connectedAt - t0;
  const t1 = performance.now();
  let res;
  try {
    res = await fetch(base + (app.probe ?? app.routes[0].path));
    await res.arrayBuffer();
  } catch (e) {
    await fail(`first request failed: ${e.message}`);
  }
  const firstRequest = performance.now() - t1;
  if (res.status >= 400) await fail(`first request got ${res.status}`);
  return { child, base, dataDir, listening, firstRequest };
}

async function shutdown({ child, dataDir }) {
  await stopServer(child);
  rmSync(dataDir, { recursive: true, force: true });
}

function oha(url, route, extraHeaders, concurrency, seconds) {
  const argv = ['-z', `${seconds}s`, '-c', String(concurrency), '--no-tui', '--output-format', 'json'];
  argv.push('--worker-threads', String(clientThreads), '--stats-success-breakdown');
  if (route.method) argv.push('-m', route.method);
  for (const [k, v] of Object.entries({ ...extraHeaders, ...(route.headers ?? {}) })) argv.push('-H', `${k}: ${v}`);
  if (route.body !== undefined) argv.push('-d', route.body);
  argv.push(url);
  return new Promise((res, rej) => {
    const child = spawn(BIN.oha, argv, { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (err += d));
    child.on('exit', (code) => {
      if (code !== 0) return rej(new Error(`oha exited with ${code}: ${err}`));
      try {
        res(JSON.parse(out));
      } catch (e) {
        rej(new Error(`oha output: ${out.slice(0, 500)} ${err}`));
      }
    });
  });
}

function summarize(result, expected) {
  const statuses = result.statusCodeDistribution ?? {};
  const ok = statuses[String(expected)] ?? 0;
  const total = Object.values(statuses).reduce((a, b) => a + b, 0);
  // The requests in flight when the duration ends are aborted, and aren't errors.
  const transport = Object.entries(result.errorDistribution ?? {})
    .filter(([e]) => e !== 'aborted due to deadline')
    .reduce((a, [, n]) => a + n, 0);
  return {
    rps: ok / result.summary.total,
    // `oha` counts the responses with a 2xx status as successful.
    p50: (result.latencyPercentilesSuccessful?.p50 ?? NaN) * 1000,
    p99: (result.latencyPercentilesSuccessful?.p99 ?? NaN) * 1000,
    requests: total,
    errors: total - ok + transport,
    statuses,
  };
}

/** The 1, 5 and 15 minute load averages, as `{ 1.23 2.34 3.45 }`. */
function load() {
  return `{ ${loadavg().map((n) => n.toFixed(2)).join(' ')} }`;
}

async function benchApp(config, appName, settings, log) {
  const app = APPS[appName];
  const server = await launch(config, appName);
  const { child, base } = server;
  const run = { config, app: appName, load: load(), startup: { listening: server.listening, firstRequest: server.firstRequest } };
  try {
    await sleep(200);
    run.rssAfterStart = treeRss(child.pid);
    const headers = app.prepare ? await app.prepare(base) : {};
    let maxRss = run.rssAfterStart;
    const sampler = setInterval(() => (maxRss = Math.max(maxRss, treeRss(child.pid))), 500);
    run.routes = {};
    const routes = [...app.routes.filter((r) => !r.mutates), ...app.routes.filter((r) => r.mutates)];
    for (const route of routes) {
      if (settings.routeFilter && !route.name.includes(settings.routeFilter)) continue;
      const url = base + route.path;
      const top = Math.max(...settings.concurrency);
      if (settings.warmup > 0) await oha(url, route, headers, top, settings.warmup);
      run.routes[route.name] = {};
      for (const c of settings.concurrency) {
        const s = summarize(await oha(url, route, headers, c, settings.duration), route.status ?? 200);
        run.routes[route.name][`c${c}`] = s;
        log(
          `  ${config.padEnd(8)} ${appName.padEnd(10)} ${route.name.padEnd(48)} c=${String(c).padEnd(3)} ` +
            `${s.rps.toFixed(0).padStart(6)} req/s  p50 ${s.p50.toFixed(2).padStart(7)} ms  p99 ${s.p99.toFixed(2).padStart(7)} ms` +
            (s.errors ? `  ERRORS ${s.errors} ${JSON.stringify(s.statuses)}` : ''),
        );
      }
    }
    clearInterval(sampler);
    run.rssMax = Math.max(maxRss, treeRss(child.pid));
  } finally {
    await shutdown(server);
  }
  return run;
}

async function startupRuns(config, appName, n) {
  const out = [];
  for (let i = 0; i < n; i++) {
    const server = await launch(config, appName);
    await sleep(300);
    out.push({ listening: server.listening, firstRequest: server.firstRequest, rss: treeRss(server.child.pid) });
    await shutdown(server);
  }
  return out;
}

function versions() {
  const v = (cmd, argv) => {
    try {
      return execFileSync(cmd, argv, { encoding: 'utf8' }).trim().split('\n')[0];
    } catch {
      return 'unavailable';
    }
  };
  let commit = 'unknown';
  try {
    commit = execFileSync('git', ['-C', ROOT, 'rev-parse', '--short', 'HEAD'], { encoding: 'utf8' }).trim();
  } catch {}
  return {
    commit,
    node: v(BIN.node, ['--version']),
    deno: v(BIN.deno, ['--version']),
    bun: v(BIN.bun, ['--version']),
    wasmtime: v(BIN.wasmtime, ['--version']),
    spin: v(BIN.spin, ['--version']),
    oha: v(BIN.oha, ['--version']),
    cpu: cpus()[0]?.model,
    cpus: cpus().length,
    memory: `${Math.round(totalmem() / 2 ** 30)} GiB`,
  };
}

async function main() {
  const configs = list('configs', ALL_CONFIGS.join(','));
  const apps = list('apps', Object.keys(APPS).join(','));
  const settings = {
    duration: Number(opt('duration', '5')),
    warmup: Number(opt('warmup', '1')),
    concurrency: list('concurrency', '1,32').map(Number),
    routeFilter: opt('routes'),
  };
  const repeat = Number(opt('repeat', '3'));
  const startupN = Number(opt('startup-runs', '5'));
  const outFile = resolve(opt('out', join(ROOT, 'target/express-bench/results.json')));
  mkdirSync(dirname(outFile), { recursive: true });
  for (const a of apps) {
    if (!existsSync(join(HERE, a, 'node_modules'))) {
      execFileSync(join(dirname(BIN.node), 'npm'), ['ci', '--no-audit', '--no-fund'], { cwd: join(HERE, a), stdio: 'inherit' });
    }
  }
  const results = {
    date: new Date().toISOString(),
    settings: { ...settings, repeat, startupRuns: startupN, reuse: opt('reuse') ?? 'default', clientThreads },
    versions: versions(),
    runs: [],
    startup: {},
  };
  const save = () => writeFileSync(outFile, JSON.stringify(results, null, 1));
  const log = (line) => console.log(line);
  const pairs = apps.flatMap((a) =>
    configs.filter((c) => (APPS[a].configs ?? ALL_CONFIGS).includes(c)).map((c) => [c, a]),
  );
  // Compiles the components into wasmtime's and Spin's caches.
  for (const config of configs.filter((c) => WASM_CONFIGS.includes(c))) {
    for (const a of apps.filter((x) => (APPS[x].configs ?? ALL_CONFIGS).includes(config))) {
      await shutdown(await launch(config, a));
      if (config === 'spin') break;
    }
  }
  if (startupN > 0) {
    log('startup');
    for (const [config, a] of pairs) {
      const runs = await startupRuns(config, a, startupN);
      results.startup[`${config} ${a}`] = runs;
      const med = (k) => median(runs.map((r) => r[k]));
      log(
        `  ${config.padEnd(8)} ${a.padEnd(10)} listening ${med('listening').toFixed(0)} ms, first request ` +
          `${med('firstRequest').toFixed(1)} ms, RSS ${(med('rss') / 1024).toFixed(0)} MiB`,
      );
      save();
    }
  }
  for (let r = 0; r < repeat; r++) {
    log(`repeat ${r + 1}/${repeat}, load ${load()}`);
    for (const a of apps) {
      for (const [config, app] of pairs.filter(([, x]) => x === a)) {
        try {
          results.runs.push({ repeat: r, ...(await benchApp(config, app, settings, log)) });
        } catch (e) {
          log(`  ${config} ${app} FAILED: ${e.message}`);
          results.runs.push({ repeat: r, config, app, error: e.message });
        }
        save();
      }
    }
  }
  log(`wrote ${outFile}`);
}

function median(xs) {
  const s = [...xs].sort((a, b) => a - b);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
}

function stat(xs) {
  if (!xs.length) return undefined;
  return { median: median(xs), spread: (Math.max(...xs) - Math.min(...xs)) / 2 };
}

function fmt(s, digits = 0) {
  if (!s) return '–';
  const pm = s.spread ? ` ±${s.spread.toFixed(digits)}` : '';
  return `${s.median.toFixed(digits)}${pm}`;
}

function report(file) {
  const results = JSON.parse(readFileSync(file, 'utf8'));
  const configs = ALL_CONFIGS.filter((c) => results.runs.some((r) => r.config === c));
  const apps = Object.keys(APPS).filter((a) => results.runs.some((r) => r.app === a));
  const lines = [];
  for (const a of apps) {
    lines.push(`### \`${a}\``, '');
    const runs = results.runs.filter((r) => r.app === a && !r.error);
    const routes = [...new Set(runs.flatMap((r) => Object.keys(r.routes ?? {})))];
    const cs = results.settings.concurrency;
    for (const route of routes) {
      lines.push(`#### ${route}`, '');
      lines.push(`| Config | ${cs.map((c) => `c=${c} req/s | c=${c} p50 ms | c=${c} p99 ms`).join(' | ')} | Errors |`);
      lines.push(`|---|${cs.map(() => '--:|--:|--:').join('|')}|--:|`);
      for (const config of configs) {
        const rr = runs.filter((r) => r.config === config && r.routes?.[route]);
        if (!rr.length) continue;
        const cells = cs.map((c) => {
          const v = rr.map((r) => r.routes[route][`c${c}`]).filter(Boolean);
          return [fmt(stat(v.map((x) => x.rps))), fmt(stat(v.map((x) => x.p50)), 2), fmt(stat(v.map((x) => x.p99)), 2)].join(' | ');
        });
        const errors = rr.reduce((sum, r) => sum + cs.reduce((s, c) => s + (r.routes[route][`c${c}`]?.errors ?? 0), 0), 0);
        lines.push(`| ${config} | ${cells.join(' | ')} | ${errors} |`);
      }
      lines.push('');
    }
  }
  lines.push('### Startup and memory', '');
  lines.push('| App | Config | Listening ms | First request ms | RSS after start MiB | Max RSS under load MiB |');
  lines.push('|---|---|--:|--:|--:|--:|');
  for (const a of apps) {
    for (const config of configs) {
      const st = results.startup[`${config} ${a}`];
      const runs = results.runs.filter((r) => r.app === a && r.config === config && !r.error);
      if (!st && !runs.length) continue;
      const mib = (xs) => stat(xs.map((x) => x / 1024));
      lines.push(
        `| ${a} | ${config} | ${fmt(stat((st ?? []).map((r) => r.listening)))} | ${fmt(stat((st ?? []).map((r) => r.firstRequest)), 1)} | ` +
          `${fmt(mib((st ?? []).map((r) => r.rss)))} | ${fmt(mib(runs.map((r) => r.rssMax)))} |`,
      );
    }
  }
  const failed = results.runs.filter((r) => r.error);
  if (failed.length) {
    lines.push('', 'Failed runs:', '');
    for (const f of failed) lines.push(`- ${f.config} ${f.app}: ${f.error.split('\n')[0]}`);
  }
  console.log(lines.join('\n'));
}

if (opt('report')) report(resolve(opt('report')));
else await main();
