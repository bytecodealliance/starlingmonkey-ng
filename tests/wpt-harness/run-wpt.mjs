#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// WPT test runner for Starling-NG.
//
// Reads tests from tests.json, assembles a harness script for each test,
// runs it via the `starling` binary, and compares results against
// expectations files.
//
// Usage:
//   node tests/wpt-harness/run-wpt.mjs [options] [pattern]
//
// Options:
//   --wpt-root=PATH            Path to WPT checkout (default: deps/wpt)
//   --target=[native|wasm]     Execution target (default: native)
//   --permissive               Run with the request restrictions disabled
//   --runtime=PATH             Path to the starlingmonkey binary (default: target/debug/starlingmonkey)
//   --expectations=PATH        Path to expectations dir (default: tests/wpt-harness/expectations)
//   --update-expectations      Update expectation files with current results
//   -v                         Verbose output
//   -vv                        Very verbose output
//   --help                     Show help
//
// Three configurations share one set of expectation files, each recording its
// results in its own field (see getExpectedResults):
//
//   native, restrictions enforced   `status`             the baseline
//   wasm, restrictions enforced     `wasm_status`        --target=wasm
//   native, restrictions disabled   `permissive_status`  --permissive
//
// An override field is written only where that configuration's result differs
// from the baseline, so the vast majority of subtests — which behave the same
// everywhere — are still recorded once.
//
// The permissive dimension exists because the browser-security Fetch
// constraints are off by default, except in WPT mode. Without it, the whole
// suite would only ever cover the configuration that ordinary users do not run.

import { execFileSync, spawn, spawnSync } from "child_process";
import {
  existsSync,
  readFileSync,
  writeFileSync,
  mkdirSync,
  rmSync,
  statSync,
} from "fs";
import path from "path";
import { cpus } from "os";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

function relativePath(p) {
  return new URL(p, import.meta.url).pathname;
}

// tests.json line prefixes:
//   "SKIP "      — never run, on any target.
//   "SLOW "      — skipped when --skip-slow-tests is passed.
//   "SKIP-WASM " — run on native, skipped on the wasm target. For tests that
//                  depend on JS APIs the wasm build does not expose (e.g.
//                  `WebAssembly`, which the SpiderMonkey-in-wasm build omits).
//   "SKIP-NATIVE " — run on wasm, skipped on the native target. The mirror of
//                  SKIP-WASM, for tests only the wasm host can run at all.
//   "NET "       — needs the live WPT server; see NET_PREFIX below.
//
// Reach for SKIP-WASM/SKIP-NATIVE only when a test cannot run on that target at
// all. When it runs but some subtests behave differently — the two HTTP stacks
// do not have identical capabilities — record that per subtest with
// `wasm_status` instead (see getExpectedResults), so the subtests that *do*
// pass stay covered on both targets.
const SKIP_PREFIX = "SKIP";
const SLOW_PREFIX = "SLOW";
const SKIP_WASM_PREFIX = "SKIP-WASM";
const SKIP_NATIVE_PREFIX = "SKIP-NATIVE";
// Tests that need the live WPT server (they call `fetch()` over the network).
// When any selected test is `NET`, the harness ensures `wpt serve` is running.
const NET_PREFIX = "NET";

const LogLevel = { Quiet: 0, Verbose: 1, VeryVerbose: 2 };

const config = {
  // Default automatically adjusted to "target/wasm32-wasip2/debug/starling.wasm" for wasm target.
  runtime: "target/debug/starlingmonkey",
  target: "native", // "native" or "wasm"
  // How the runtime executes each test: "cli" runs it as a one-shot command, "serve" POSTs it to a
  // long-running server built from the same binary (see wpt-server.js). Serve mode is the
  // production-shaped configuration: the test runs inside a request handler.
  mode: "cli", // "cli" or "serve"
  servePort: 7877,
  wptRoot: process.env.WPT_ROOT || "deps/wpt",
  tmpDir: "target/tmp/wpt",
  tests: {
    list: relativePath("tests.json"),
    expectations: relativePath("expectations"),
    updateExpectations: false,
    pattern: "",
  },
  skipSlowTests: false,
  // Run with the browser-security request restrictions disabled (the runtime's
  // default outside WPT mode), checking `permissive_status` expectation
  // overrides. See getExpectedResults.
  permissive: false,
  // How many tests to have in flight at once. Defaults to number of CPUs * 2,
  // because many tests aren't CPU-bound, and can execute in parallel without
  // incurring compute contention.
  jobs: Math.max(1, cpus().length * 2),
  // Path to an ahead-of-time compiled runtime, set up for the wasm target
  // unless --no-precompile is passed. See ensurePrecompiledRuntime.
  precompiled: null,
  usePrecompiled: true,
  // Pre-initialize the wasm runtime with Wizer before serving it, so the engine and the content
  // script are already stood up in the snapshot. This harness retires an instance after every
  // request, so without this each test pays for the whole runtime again.
  wizen: false,
  wizened: null,
  logLevel: LogLevel.Quiet,
  // Per-test timeout for a serial run, scaled by --jobs. This is the outermost of three nested
  // limits, and has to stay the largest of them or it preempts the ones that can say something
  // useful: testharness.js times a test out internally at 10s (60s for `META: timeout=long`) and
  // reports per-subtest TIMEOUT statuses, and serve mode's server backstop
  // (`WAIT_LIMIT_MS` in wpt-server.js) sits between the two. Reaching *this* one means the
  // runtime hung with nothing to report, so it is a hard kill.
  timeout: 30000,
};

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

const ArgParsers = {
  "--runtime": {
    help: `Path to the starlingmonkey binary (default: ${config.runtime})`,
    cmd: (val) => {
      config.runtime = val;
    },
  },
  "--target": {
    help: `Execution target: native or wasm (default: native)`,
    cmd: (val) => {
      if (val !== "native" && val !== "wasm") {
        console.error(
          `Unknown --target value: ${val}. Use "native" or "wasm".`,
        );
        process.exit(1);
      }
      config.target = val;
    },
  },
  "--mode": {
    help: `How to run each test: cli or serve (default: ${config.mode})`,
    cmd: (val) => {
      if (val !== "cli" && val !== "serve") {
        console.error(`Unknown --mode value: ${val}. Use "cli" or "serve".`);
        process.exit(1);
      }
      config.mode = val;
    },
  },
  "--serve-port": {
    help: `Port for --mode=serve (default: ${config.servePort})`,
    cmd: (val) => {
      config.servePort = parseInt(val, 10);
    },
  },
  "--wpt-root": {
    help: `Path to WPT checkout (default: env var 'WPT_ROOT' or ${config.wptRoot})`,
    cmd: (val) => {
      config.wptRoot = val;
    },
  },
  "--expectations": {
    help: `Path to expectations directory`,
    cmd: (val) => {
      config.tests.expectations = val;
    },
  },
  "--update-expectations": {
    help: "Update expectation files with current results",
    cmd: () => {
      config.tests.updateExpectations = true;
    },
  },
  "--skip-slow-tests": {
    help: "Skip tests marked as SLOW",
    cmd: () => {
      config.skipSlowTests = true;
    },
  },
  "--permissive": {
    help: "Run with request restrictions disabled; checks permissive_status expectations",
    cmd: () => {
      config.permissive = true;
    },
  },
  "--jobs": {
    help: `Tests to run concurrently (default: ${config.jobs})`,
    cmd: (val) => {
      const jobs = parseInt(val, 10);
      if (!Number.isInteger(jobs) || jobs < 1) {
        throw new Error(`--jobs must be a positive integer, got: ${val}`);
      }
      config.jobs = jobs;
    },
  },
  "--wizen": {
    help: "Pre-initialize the wasm serve-mode runtime with `wasmtime wizer`",
    cmd: () => {
      config.wizen = true;
    },
  },
  "--no-precompile": {
    help: "Skip ahead-of-time compilation of the wasm runtime",
    cmd: () => {
      config.usePrecompiled = false;
    },
  },
  "--timeout": {
    help: `Timeout per test in ms for a serial run, scaled by --jobs (default: ${config.timeout})`,
    cmd: (val) => {
      config.timeout = parseInt(val, 10);
    },
  },
  "-v": {
    help: "Verbose output",
    cmd: () => {
      config.logLevel = LogLevel.Verbose;
    },
  },
  "-vv": {
    help: "Very verbose output",
    cmd: () => {
      config.logLevel = LogLevel.VeryVerbose;
    },
  },
  "--help": {
    help: "Show this help message",
    cmd: () => {
      console.log(`Usage: node run-wpt.mjs [options] [pattern]

If a pattern is provided, only tests whose path contains the pattern will be run.

Options:`);
      for (const [name, parser] of Object.entries(ArgParsers)) {
        console.log(
          `  ${(name + (parser.cmd.length > 0 ? "=value" : "")).padEnd(30)} ${parser.help}`,
        );
      }
      process.exit(0);
    },
  },
};

function applyConfig(argv) {
  for (const entry of argv.slice(2)) {
    if (entry[0] !== "-") {
      config.tests.pattern = entry;
      continue;
    }
    const [arg, ...rest] = entry.split("=");
    const val = rest.join("=");
    const parser = ArgParsers[arg];
    if (parser) {
      parser.cmd(val || undefined);
    } else {
      console.error(`Unknown argument: ${arg}`);
      process.exit(1);
    }
  }

  // When targeting wasm, adjust the runtime path if not explicitly set.
  if (config.target === "wasm" && config.runtime === "target/debug/starlingmonkey") {
    config.runtime = "target/wasm32-wasip2/debug/starling.wasm";
  }

  // The native server answers one request at a time: `--serve-isolated` (which serveCommand
  // passes, so each test does get its own global) has every request enter its own realm and hold
  // it across awaits, which only works serialized. Sending more in flight than it will serve just
  // queues them inside the server while inflating the per-test budget, which scales with --jobs.
  // The wasm server needs none of this: `wasmtime serve` is told to retire each instance after a
  // single request, so its tests are isolated even in flight.
  if (config.mode === "serve" && config.target !== "wasm" && config.jobs !== 1) {
    if (config.logLevel > LogLevel.Quiet) {
      console.log("The native serve-mode server is serial; running tests one at a time.");
    }
    config.jobs = 1;
  }

  // The permissive dimension records one `permissive_status` override per
  // subtest, with no per-target variant of it, because the fetch code the switch
  // gates is shared between the targets, so the native run covers both.
  if (config.permissive && config.target === "wasm") {
    console.error("--permissive runs are native-only; drop --target=wasm.");
    return false;
  }

  if (!existsSync(config.runtime)) {
    if (config.target === "wasm") {
      console.error(
        `Wasm runtime not found: ${config.runtime}. Run 'just build-wasm' first.`,
      );
    } else {
      console.error(
        `Runtime not found: ${config.runtime}. Run 'cargo build' first.`,
      );
    }
    return false;
  }

  if (config.target === "wasm") {
    // Verify wasmtime is available.
    try {
      execFileSync("wasmtime", ["--version"], { encoding: "utf-8" });
    } catch {
      console.error(
        "wasmtime not found. Install wasmtime to run WPT tests on wasm.",
      );
      return false;
    }
  }

  if (!existsSync(config.wptRoot)) {
    console.error(
      `WPT root not found: ${config.wptRoot}. Run 'just clone-wpt-tests' first, define 'WPT_ROOT' in your environment, or pass '--wpt-root'.`,
    );
    return false;
  }

  return true;
}

// ---------------------------------------------------------------------------
// Harness assembly
// ---------------------------------------------------------------------------

// Cache the base harness (pre-harness + testharness.js + post-harness)
let cachedBaseHarness = null;

function getBaseHarness() {
  if (cachedBaseHarness) return cachedBaseHarness;

  const preHarness = readFileSync(relativePath("pre-harness.js"), "utf-8");
  const testHarness = readFileSync(
    path.join(config.wptRoot, "resources", "testharness.js"),
    "utf-8",
  );
  const postHarness = readFileSync(relativePath("post-harness.js"), "utf-8");

  cachedBaseHarness =
    preHarness + "\n" + testHarness + "\n" + postHarness + "\n";
  return cachedBaseHarness;
}

/**
 * Assemble the complete test script for a given WPT test path.
 *
 * The assembled script:
 * 1. Runs the base harness (pre-harness + testharness.js + post-harness)
 *    in the global scope (via legacy script mode).
 * 2. Uses evalScript() for each META: script= dependency.
 * 3. Uses evalScript() for the test source itself.
 * 4. Calls done() to trigger the completion callback.
 */

// Legacy WPT path aliases. The WPT HTTP server handles some redirects that
// aren't present when loading files directly from disk.
const WPT_PATH_ALIASES = {
  "/resources/WebIDLParser.js": "/resources/webidl2/lib/webidl2.js",
};

// ---------------------------------------------------------------------------
// wptserve `{{...}}` template substitution
// ---------------------------------------------------------------------------
//
// The WPT server fills `{{host}}`-style placeholders into any `.sub.` file it
// serves. This harness loads files from disk, so without doing the same here,
// scripts like get-host-info.sub.js hand tests literal `{{host}}` strings.
//
// The values must describe the server the NET tests talk to. Host and domains
// are wptserve defaults; the ports come from wpt-server-config.json, which
// ensureWptServer passes to `wpt serve`.
const WPT_HOST = "web-platform.test";
const WPT_ALT_HOST = "not-web-platform.test";
const WPT_PORTS = { http: [8000, 8001], https: [8443, 8444] };

function isSubstitutedPath(filePath) {
  return path.basename(filePath).includes(".sub.");
}

function substituteWptTemplates(source) {
  const domain = (host, sub) => (sub ? `${sub}.${host}` : host);
  return source
    .replace(/\{\{host\}\}/g, WPT_HOST)
    .replace(/\{\{domains\[(\w*)\]\}\}/g, (_, sub) => domain(WPT_HOST, sub))
    .replace(/\{\{hosts\[\]\[(\w*)\]\}\}/g, (_, sub) => domain(WPT_HOST, sub))
    .replace(/\{\{hosts\[alt\]\[(\w*)\]\}\}/g, (_, sub) =>
      domain(WPT_ALT_HOST, sub),
    )
    .replace(
      // Unknown scheme/index (e.g. `{{ports[ws][0]}}`) is left as-is rather
      // than guessed at: a placeholder that survives is an obvious failure,
      // a wrong port number a baffling one.
      /\{\{ports\[(\w+)\]\[(\d+)\]\}\}/g,
      (match, scheme, index) => WPT_PORTS[scheme]?.[index]?.toString() ?? match,
    );
}

function assembleTestScript(testPath) {
  const fullPath = path.join(config.wptRoot, testPath);
  let testSource = readFileSync(fullPath, "utf-8");
  if (isSubstitutedPath(testPath)) {
    testSource = substituteWptTemplates(testSource);
  }

  // Parse META: script= directives from the test source.
  const metaScripts = [];
  for (const match of testSource.matchAll(/\/\/ *META: *script=(.+)/g)) {
    metaScripts.push(match[1].trim());
  }

  // Point `globalThis.location` at the test's canonical WPT URL before any
  // harness or test code runs.
  let script = `__setLocation(${JSON.stringify(
    "http://web-platform.test:8000/" + testPath,
  )});\n`;
  script += getBaseHarness();

  // If the test uses idl_test(), inject a minimal fetch polyfill that serves
  // IDL files from the WPT /interfaces/ directory on disk. This avoids
  // needing a real fetch() implementation or WPT HTTP server.
  const idlTestMatch = testSource.match(/idl_test\(\s*\[([^\]]*)\]/);
  if (idlTestMatch) {
    const idlSpecs = idlTestMatch[1]
      .split(",")
      .map((s) => s.trim().replace(/^['"]|['"]$/g, ""))
      .filter(Boolean);

    // Pre-read all referenced IDL files and build a fetch polyfill.
    const idlMap = {};
    for (const spec of idlSpecs) {
      const idlPath = path.join(config.wptRoot, "interfaces", spec + ".idl");
      if (existsSync(idlPath)) {
        idlMap["/interfaces/" + spec + ".idl"] = readFileSync(idlPath, "utf-8");
      }
    }

    // Also check for dependency IDL specs (second argument to idl_test).
    const depsMatch = testSource.match(
      /idl_test\(\s*\[[^\]]*\]\s*,\s*\[([^\]]*)\]/,
    );
    if (depsMatch) {
      const depSpecs = depsMatch[1]
        .split(",")
        .map((s) => s.trim().replace(/^['"]|['"]$/g, ""))
        .filter(Boolean);
      for (const spec of depSpecs) {
        const idlPath = path.join(config.wptRoot, "interfaces", spec + ".idl");
        if (existsSync(idlPath)) {
          idlMap["/interfaces/" + spec + ".idl"] = readFileSync(
            idlPath,
            "utf-8",
          );
        }
      }
    }

    if (Object.keys(idlMap).length > 0) {
      script += `// Minimal fetch polyfill for idl_test — serves pre-inlined IDL files.\n`;
      script += `globalThis.__wpt_idl_files = ${JSON.stringify(idlMap)};\n`;
      script += `globalThis.fetch = function(url) {\n`;
      script += `  var content = globalThis.__wpt_idl_files[url];\n`;
      script += `  if (content !== undefined) {\n`;
      script += `    return Promise.resolve({ ok: true, text: function() { return Promise.resolve(content); } });\n`;
      script += `  }\n`;
      script += `  return Promise.reject(new Error("fetch not available for: " + url));\n`;
      script += `};\n`;
    }
  }

  // Load META scripts via evalScript.
  for (const metaPath of metaScripts) {
    // Apply path aliases for legacy WPT paths.
    const effectivePath = WPT_PATH_ALIASES[metaPath] || metaPath;

    let resolvedPath;
    if (effectivePath.startsWith("/")) {
      // Absolute path within WPT root.
      resolvedPath = path.join(config.wptRoot, effectivePath);
    } else {
      // Relative to the test file.
      resolvedPath = path.join(path.dirname(fullPath), effectivePath);
    }
    if (!existsSync(resolvedPath)) {
      console.error(
        `  META script not found: ${metaPath} (resolved: ${resolvedPath})`,
      );
      continue;
    }
    let metaSource = readFileSync(resolvedPath, "utf-8");
    if (isSubstitutedPath(resolvedPath)) {
      metaSource = substituteWptTemplates(metaSource);
    }
    script += toEvalScriptCall(metaSource, metaPath);
  }

  // Load the test source via evalScript.
  script += toEvalScriptCall(testSource, testPath);

  // Signal test completion.
  script += `done();\n`;

  return script;
}

function toEvalScriptCall(source, url) {
  let escaped = source.split("\\").join("\\\\");
  escaped = escaped.split("`").join("\\`");
  escaped = escaped.split("${").join("\\${");
  return `// ${url}\nevalScript(\`${escaped}\`, ${JSON.stringify(url)});\n\n`;
}

// ---------------------------------------------------------------------------
// Test execution
// ---------------------------------------------------------------------------

/**
 * Extract and strip all prefixes from `path`, returning the set of prefixes and the stripped path.
 *
 * Prefixes have the format "PREFIX[(comment)]", and can be repeated.
 * The returned prefixes map will contain the prefix as the key and the optional comment as the value (`undefined` if no comment).
 */
function extractPrefixes(path) {
  const prefixes = new Map();
  let remaining = path;
  while (true) {
    // Test paths never contain spaces, so the last `") "` in a line always closes the comment.
    const match = remaining.match(/^([A-Z-]+)(\((.*)\))?[, ]/);
    if (!match) break;
    const prefix = match[1];
    const comment = match[3];
    prefixes.set(prefix, comment);
    remaining = remaining.slice(match[0].length);
  }
  return { prefixes, path: remaining };
}

function getTests(pattern) {
  const raw = JSON.parse(readFileSync(config.tests.list, "utf-8"));
  const totalCount = raw.length;
  let testPaths = [];
  let needsServer = false;
   for (const rawPath of raw) {
    const { prefixes, path } = extractPrefixes(rawPath);
    if (!path.includes(pattern) || prefixes.has(SKIP_PREFIX) ||
      (config.target === "wasm" && prefixes.has(SKIP_WASM_PREFIX)) ||
      (config.target === "native" && prefixes.has(SKIP_NATIVE_PREFIX)) ||
      (config.skipSlowTests && prefixes.has(SLOW_PREFIX))) {
      continue;
    }
    needsServer ||= prefixes.has(NET_PREFIX);
    testPaths.push(path);
  }

  return { testPaths, totalCount, needsServer };
}

function expectationsPath(testPath) {
  return path.join(config.tests.expectations, testPath + ".json");
}

// The expectation field holding the wasm target's status, iff it differs from
// native. Absent means "same as native".
const WASM_STATUS = "wasm_status";

// The expectation field holding a subtest's status with the request
// restrictions disabled (a `--permissive` run), iff it differs from the
// enforced-mode `status`. Absent means "same as enforced".
const PERMISSIVE_STATUS = "permissive_status";

// Every status field an entry can carry, in the order they are written.
const STATUS_FIELDS = ["status", WASM_STATUS, PERMISSIVE_STATUS];

// The field a run records its results under: the baseline `status` for the
// default native enforced run, or the matching override field.
function runStatusField() {
  if (config.permissive) return PERMISSIVE_STATUS;
  return config.target === "wasm" ? WASM_STATUS : "status";
}

/// Read the raw expectations file, exactly as stored.
function readExpectationsFile(testPath) {
  try {
    return JSON.parse(readFileSync(expectationsPath(testPath), "utf-8"));
  } catch {
    return {};
  }
}

// The expectations for the configuration being run, from one file shared by
// all of them.
//
// A subtest is stored as `{"status": "PASS"}` when every configuration agrees,
// and gains override fields where one does not:
//
//     "some subtest": { "status": "PASS", "wasm_status": "FAIL" }
//     "another":      { "status": "FAIL", "permissive_status": "PASS" }
//
// `status` is the native enforced-mode status and the default; `wasm_status`
// overrides it on the wasm target, and `permissive_status` overrides it when
// the request restrictions are disabled (`--permissive`). Keeping all of them
// in one file means a subtest that behaves the same everywhere — the
// overwhelming majority — is still written once.
//
// An entry with no status for this configuration (say, a `wasm_status`-only
// entry seen on native) is dropped, so the subtest reads as having no
// expectation rather than as an expectation that can never be met.
function getExpectedResults(testPath) {
  const raw = readExpectationsFile(testPath);
  const expectations = {};
  for (const [name, entry] of Object.entries(raw)) {
    const status =
      (config.permissive ? entry[PERMISSIVE_STATUS] : undefined) ??
      (config.target === "wasm" ? entry[WASM_STATUS] : undefined) ??
      entry.status;
    if (status !== undefined) {
      expectations[name] = { status };
    }
  }
  return expectations;
}

// Fold this run's results into the stored expectations, touching only the
// field for the configuration that ran. Running on wasm or permissive must not
// overwrite the native enforced statuses, and vice versa, so that updating one
// configuration never silently invents results for another.
function mergeExpectations(previous, results) {
  const field = runStatusField();
  const merged = {};
  for (const result of results) {
    const prev = previous[result.name] ?? {};
    const observed = result.status === 0 ? "PASS" : "FAIL";
    // Preserve a FLAKY marker: it records a deliberate known-intermittent
    // subtest, not the outcome of any single run.
    const keep = (stored) => (stored === "FLAKY" ? "FLAKY" : undefined);

    const values = { ...prev, [field]: keep(prev[field]) ?? observed };
    // Only record an override where it actually differs from the baseline, so
    // the file does not fill up with redundant duplicates. (After a baseline
    // run this also drops overrides the new baseline has caught up with.)
    for (const override of [WASM_STATUS, PERMISSIVE_STATUS]) {
      if (values[override] !== undefined && values[override] === values.status) {
        delete values[override];
      }
    }
    const entry = {};
    for (const f of STATUS_FIELDS) {
      if (values[f] !== undefined) entry[f] = values[f];
    }
    merged[result.name] = entry;
  }
  // Carry over what the *other* configurations recorded for subtests this run
  // did not observe, dropping only this run's own stale field.
  for (const [name, prev] of Object.entries(previous)) {
    if (merged[name]) continue;
    const entry = {};
    for (const f of STATUS_FIELDS) {
      if (f !== field && prev[f] !== undefined) entry[f] = prev[f];
    }
    if (Object.keys(entry).length > 0) {
      merged[name] = entry;
    }
  }
  return merged;
}

// Codegen options passed to both `wasmtime compile` and `wasmtime run`; a
// precompiled artifact is only loadable by a runtime configured the same way.
//
// Native unwind information is off because registering and — far worse —
// deregistering it dominates every wasm test. A sample of the wasmtime process
// during a trivial run puts essentially all of the ~1.1s of host time in
// `CodeMemory::drop` calling `__deregister_frame`, whose macOS implementation
// walks the registered frames linearly; with a module this size that is
// quadratic and swamps everything else. The runtime module is ~120MB with a
// correspondingly huge function count, so it hits this hard: dropping the
// unwind tables takes a wasm test from ~1.4s to ~0.35s.
//
// What it costs: native profilers and debuggers can no longer unwind through
// wasm frames. Guest-level diagnostics are unaffected — JS exceptions and their
// stacks come from SpiderMonkey, and wasm traps from wasmtime's own frame
// information — so error reporting in the harness is unchanged.
const WASMTIME_CODEGEN_FLAGS = ["-C", "native-unwind-info=n"];

// The WASI capabilities the runtime needs: environment (its configuration arrives there in serve
// mode), and the network/HTTP stack the `fetch` tests exercise.
const WASI_FLAGS = "inherit-env=y,inherit-network=y,http=y,tcp=y,udp=y,p3=y";

// Pre-initialize the wasm runtime with Wizer, baking the serve-mode WPT harness and its
// configuration into the snapshot.
//
// This is the configuration a deployed server has: the engine is initialized and the content
// script evaluated at build time, so serving a request does neither. It matters most exactly here,
// because this harness retires each instance after one request (see `serveCommand`).
//
// Rebuilt whenever the runtime or the baked configuration changes, on the same stamp scheme the
// precompiled artifact uses.
function ensureWizenedRuntime() {
  if (config.target !== "wasm" || config.mode !== "serve" || !config.wizen) return null;

  const output = path.join(config.tmpDir, "wizened-" + path.basename(config.runtime));
  const bakedConfig = `--wpt-mode --legacy-script ${guestServerScript()}`;
  const stamp = output + ".stamp";
  const want = `${statSync(config.runtime).mtimeMs}\n${bakedConfig}`;
  try {
    if (readFileSync(stamp, "utf-8") === want && existsSync(output)) {
      return output;
    }
  } catch {
    // No usable stamp; wizen below.
  }

  console.log("Pre-initializing the wasm runtime with Wizer (once for this run) ...");
  try {
    mkdirSync(config.tmpDir, { recursive: true });
    execFileSync(
      "wasmtime",
      [
        "wizer",
        `-S${WASI_FLAGS},cli=y`,
        "-Wcomponent-model-async=y",
        // The component's `wizer-initialize` export is declared by a world of its own, so the
        // component type still names it after the snapshot. Dropping the core function (the
        // default) would leave that declaration dangling and the component unloadable.
        "--keep-init-func=true",
        "--dir=.::/",
        "--env",
        `STARLINGMONKEY_CONFIG=${bakedConfig}`,
        "-o",
        output,
        config.runtime,
      ],
      { stdio: "pipe" },
    );
    writeFileSync(stamp, want);
    return output;
  } catch (e) {
    console.warn(`  Wizening failed, serving the un-wizened runtime: ${e.message}`);
    return null;
  }
}

// Compile the wasm runtime ahead of time, once, and run the tests against the
// result.
//
// Roughly a fifth of each wasm test's wall clock is wasmtime compiling the
// runtime module — it is ~120 MB, and the compilation is not cached between
// processes, so every test paid for it again. Compiling once up front and
// passing `--allow-precompiled` moves that cost from per-test to per-run.
//
// The artifact is tied to both the runtime build and the wasmtime version, so
// it is rebuilt whenever either is newer than it. On any failure we fall back
// to running the module directly rather than failing the run.
function ensurePrecompiledRuntime() {
  if (config.target !== "wasm" || !config.usePrecompiled) return null;

  const runtime = config.wizened ?? config.runtime;
  const output = path.join(config.tmpDir, path.basename(runtime) + ".cwasm");
  let version = "";
  try {
    version = execFileSync("wasmtime", ["--version"], { encoding: "utf-8" }).trim();
  } catch {
    return null;
  }
  const stamp = output + ".stamp";
  const want = `${version}\n${statSync(runtime).mtimeMs}\n${WASMTIME_CODEGEN_FLAGS.join(" ")}`;
  try {
    if (readFileSync(stamp, "utf-8") === want && existsSync(output)) {
      return output;
    }
  } catch {
    // No usable stamp; compile below.
  }

  console.log("Precompiling the wasm runtime (once for this run) ...");
  try {
    mkdirSync(config.tmpDir, { recursive: true });
    execFileSync(
      "wasmtime",
      [
        "compile",
        ...WASMTIME_CODEGEN_FLAGS,
        "-W",
        "component-model-async=y",
        runtime,
        "-o",
        output,
      ],
      { stdio: "pipe" },
    );
    writeFileSync(stamp, want);
    return output;
  } catch (e) {
    console.warn(`  Precompilation failed, running uncompiled: ${e.message}`);
    return null;
  }
}

/// Spawn `command`, collecting stdout/stderr, with the same timeout and
/// output-cap semantics `spawnSync` gave us — but without blocking the loop, so
/// several tests can be in flight at once.
function spawnCollecting(command, args, { timeout, maxBuffer }) {
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(command, args);
    } catch (e) {
      resolve({ error: e, stdout: "", stderr: "" });
      return;
    }
    // Collect raw chunks and decode once at the end. Decoding each chunk as it
    // arrives corrupts any multi-byte UTF-8 sequence that straddles a chunk
    // boundary, turning it into replacement characters — which shows up as a
    // subtest whose name no longer matches its expectation, and only
    // intermittently, since where the boundaries fall depends on timing.
    const stdoutChunks = [];
    const stderrChunks = [];
    let stdoutBytes = 0;
    let error = null;
    const kill = (e) => {
      if (error) return;
      error = e;
      child.kill("SIGKILL");
    };
    const timer = setTimeout(
      () => kill(Object.assign(new Error("ETIMEDOUT"), { code: "ETIMEDOUT" })),
      timeout,
    );
    child.stdout.on("data", (d) => {
      stdoutChunks.push(d);
      stdoutBytes += d.length;
      if (stdoutBytes > maxBuffer) kill(new Error("stdout exceeded maxBuffer"));
    });
    child.stderr.on("data", (d) => {
      stderrChunks.push(d);
    });
    const decode = () => ({
      stdout: Buffer.concat(stdoutChunks).toString("utf-8"),
      stderr: Buffer.concat(stderrChunks).toString("utf-8"),
    });
    child.on("error", (e) => {
      clearTimeout(timer);
      resolve({ error: error ?? e, ...decode() });
    });
    child.on("close", (status, signal) => {
      clearTimeout(timer);
      resolve({ error, status, signal, ...decode() });
    });
  });
}

// Where this run writes `testPath`'s assembled script.
function runScriptPath(testPath) {
  return path.join(
    config.tmpDir,
    `${config.target}-${testPath.replace(/\//g, "_")}`,
  );
}

/// The runtime flags that carry the run's restriction dimension. Every mode that starts a runtime
/// has to pass these: expectation lookup switches to `permissive_status` on `--permissive`, so a
/// mode that silently kept the restrictions on would check permissive expectations against a
/// restricted runtime — and `--update-expectations` would then write those results into the
/// `permissive_status` fields, corrupting them for the runs that do disable the restrictions.
function restrictionFlags() {
  return config.permissive ? ["--enforce-fetch-restrictions=false"] : [];
}

// Build the command that runs `scriptFile` as a one-shot command: the runtime binary directly on
// native, and the same component under `wasmtime run` on wasm — the component exports
// `wasi:cli/run` alongside `wasi:http/handler`, so this is the very binary serve mode uses.
//
// `optimized` selects the flags a measured run uses (ahead-of-time compiled runtime, no native
// unwind information); `reproLine` turns it off so the command it prints stays runnable by hand.
function testCommand(scriptFile, { optimized = true } = {}) {
  const runtimeArgs = [
    "--legacy-script",
    "--wpt-mode",
    ...restrictionFlags(),
    scriptFile,
  ];
  if (config.target !== "wasm") {
    return { command: config.runtime, args: runtimeArgs };
  }
  // wasmtime sees the guest filesystem, where --dir=.::/ maps the host CWD to /, so the script
  // has to be named by its path relative to the CWD. A host path fails with "Error reading script".
  const wasiPath = "/" + path.relative(process.cwd(), scriptFile);
  const precompiled = optimized ? config.precompiled : null;
  return {
    command: "wasmtime",
    args: [
      "run",
      ...(optimized ? WASMTIME_CODEGEN_FLAGS : []),
      ...(precompiled ? ["--allow-precompiled"] : []),
      "--dir=.::/",
      "--dir=.",
      "--dir=/tmp",
      `-S${WASI_FLAGS}`,
      "-Wcomponent-model-async=y",
      precompiled ?? config.runtime,
      ...runtimeArgs.slice(0, -1),
      wasiPath,
    ],
  };
}

async function runSingleTest(testPath) {
  const script = assembleTestScript(testPath);

  // Write the assembled script to a temp file of its own.
  //
  // The name carries the target and the test, which is what keeps tests in flight within a run
  // — and a wasm suite alongside a native spot-check — from overwriting each other's script
  // between the write and the spawn and reporting one test's results under another's name.
  //
  // These files are deliberately left behind, so the "To reproduce" command printed for a
  // failure stays runnable. Naming them by target and test alone bounds that to one file per
  // test per target, which the directory can carry indefinitely. The name used to carry the
  // pid as well, isolating concurrent runs of the same target; that made the directory grow
  // without limit for a case where both runs write the same bytes anyway — the script is a
  // pure function of the test path and the WPT checkout.
  mkdirSync(config.tmpDir, { recursive: true });
  const tmpFile = runScriptPath(testPath);
  writeFileSync(tmpFile, script);

  if (config.mode === "serve") {
    return runTestOnServer(script);
  }

  const { command, args } = testCommand(tmpFile);
  try {

    const result = await spawnCollecting(command, args, {
      // A test's wall clock includes time spent waiting for a core, so with N
      // tests in flight the budget has to grow with N or the slowest legitimate
      // tests start timing out purely from contention. `--timeout` is therefore
      // the budget for a serial run; the limit scales from there. The point of
      // the timeout is to catch a hang, which no amount of contention explains.
      timeout: config.timeout * Math.max(1, config.jobs),
      maxBuffer: 10 * 1024 * 1024,
    });

    const stdout = result.stdout || "";
    const stderr = result.stderr || "";

    if (result.error) {
      return { error: result.error, stdout, stderr };
    }
    if (result.status !== 0 || result.signal) {
      const reason = result.signal
        ? `killed by signal ${result.signal}`
        : `exited with status ${result.status}`;
      return { error: new Error(reason), stdout, stderr };
    }

    // Parse results from stdout — look for the WPT_RESULTS_JSON marker.
    const marker = "WPT_RESULTS_JSON:";
    const lines = stdout.split("\n");
    for (const line of lines) {
      if (line.startsWith(marker)) {
        return { results: JSON.parse(line.slice(marker.length)), stdout, stderr };
      }
    }

    return { error: new Error("No WPT_RESULTS_JSON found in output"), stdout, stderr };
  } catch (e) {
    return { error: e, stdout: e.stdout || "", stderr: e.stderr || "" };
  }
}

// Run `testPaths` with up to `config.jobs` in flight, handing each result to
// `report` strictly in test order.
//
// Ordering matters more than it looks: `report` prints, accumulates the totals
// and rewrites expectation files, so letting it run in completion order would
// make both the output and `--update-expectations` depend on which test
// happened to finish first. Only the spawning is concurrent; everything that
// has an effect stays sequential and deterministic.
async function runTestsConcurrently(testPaths, report) {
  const jobs = Math.max(1, config.jobs);
  const outcomes = new Array(testPaths.length);
  const running = new Set();
  let nextToLaunch = 0;
  let nextToReport = 0;

  while (nextToReport < testPaths.length) {
    while (running.size < jobs && nextToLaunch < testPaths.length) {
      const index = nextToLaunch++;
      const task = (async () => {
        const started = Date.now();
        const outcome = await runSingleTest(testPaths[index]);
        outcomes[index] = { ...outcome, duration: Date.now() - started };
      })();
      running.add(task);
      task.finally(
        () => running.delete(task),
      );
    }

    if (outcomes[nextToReport] === undefined) {
      // Nothing to report yet; wait for whichever test finishes next.
      await Promise.race(running);
      continue;
    }

    await report(testPaths[nextToReport], outcomes[nextToReport]);
    // Release the captured output; a full suite would otherwise hold every
    // test's stdout until the end.
    outcomes[nextToReport] = null;
    nextToReport++;
  }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

function formatStats(stats) {
  return `${pad(stats.pass, 4)} / ${pad(stats.count, 4)} (${pad("+" + stats.unexpectedPass, 5)}, ${pad("-" + stats.unexpectedFail, 5)}, ${pad("?" + stats.missing, 5)}) passing in ${pad(stats.duration, 4)}ms`;
}

function pad(v, n) {
  return (v + "").padStart(n);
}

// ---------------------------------------------------------------------------
// Serve mode (`--mode=serve`): one runtime server, one request per test
// ---------------------------------------------------------------------------
//
// The runtime runs `wpt-server.js` as its content script, which registers a `fetch` handler that
// evaluates the assembled test script a request carries and answers with its results. Nothing
// here is WPT-specific on the runtime side: it is the ordinary serve path, which is the point —
// this exercises the tests inside a request handler, the configuration that ships.
//
// The same component provides command mode's `wasi:cli/run`, so both modes run the one binary.

const SERVE_HOST = "127.0.0.1";
let serveUrl = null;
let managedServer = null;

/// The serve-mode harness script, as the guest sees it under the `--dir=.::/` mapping.
function guestServerScript() {
  return "/" + path.relative(process.cwd(), relativePath("wpt-server.js"));
}

/// How to start the runtime as a server, and how to recognize that it is listening.
function serveCommand() {
  const serverScript = relativePath("wpt-server.js");
  if (config.target !== "wasm") {
    return {
      command: config.runtime,
      args: [
        "--serve",
        String(config.servePort),
        "--wpt-mode",
        // Each request in its own global, so one test's top-level declarations are not still
        // there for the next. Without it tests collide and the run is simply wrong; the wasm
        // server asks its host for the same property with `--max-instance-reuse-count` below.
        "--serve-isolated",
        "--legacy-script",
        ...restrictionFlags(),
        serverScript,
      ],
      ready: /serving on (http:\/\/[\d.]+:\d+)/,
    };
  }
  // `wasmtime serve` passes the guest no arguments, so the runtime's configuration goes through
  // the environment — except on a wizened runtime, which already holds an initialized engine with
  // the harness evaluated, and so needs neither the configuration nor the script.
  //
  // No `restrictionFlags()` here: `--permissive` runs are native-only (applyConfig rejects the
  // combination), and a wizened runtime takes no configuration at all to carry them in.
  const precompiled = config.precompiled;
  const wizened = config.wizened !== null;
  return {
    command: "wasmtime",
    args: [
      "serve",
      ...WASMTIME_CODEGEN_FLAGS,
      ...(precompiled ? ["--allow-precompiled"] : []),
      "--dir=.::/",
      "--dir=.",
      "--dir=/tmp",
      `-S${WASI_FLAGS},cli=y`,
      "-Wcomponent-model-async=y",
      ...(wizened
        ? []
        : [
            "--env",
            `STARLINGMONKEY_CONFIG=--wpt-mode --legacy-script ${guestServerScript()}`,
          ]),
      // One request per instance, so each test gets a fresh global — what a fresh process gives
      // command mode. WASIp3 components default to 128 requests per instance, so this is load
      // bearing, and it holds under concurrency: an instance is retired after its one request.
      "--max-instance-reuse-count",
      "1",
      "--addr",
      `${SERVE_HOST}:${config.servePort}`,
      precompiled ?? config.wizened ?? config.runtime,
    ],
    ready: /Serving HTTP on (http:\/\/[\d.]+:\d+)/,
  };
}

/// Start the runtime in serve mode; resolve once it is listening.
function startServer() {
  const { command, args, ready } = serveCommand();
  return new Promise((resolve, reject) => {
    if (config.logLevel > LogLevel.Quiet) {
      console.log(`Starting ${config.target} serve-mode runtime: ${command} ${args.join(" ")}`);
    }
    managedServer = spawn(command, args, { stdio: ["ignore", "pipe", "pipe"] });
    let output = "";
    const timer = setTimeout(
      () => reject(new Error(`serve-mode runtime did not start within 60s. Output:\n${output}`)),
      60000,
    );
    // The readiness line goes to stderr for both runtimes; watch stdout too, so a runtime that
    // logs it elsewhere still starts rather than hanging until the timeout.
    const watch = (chunk) => {
      output += chunk.toString();
      const match = output.match(ready);
      if (match && !serveUrl) {
        serveUrl = match[1];
        clearTimeout(timer);
        if (config.logLevel > LogLevel.Quiet) {
          console.log(`serve-mode runtime is ready at ${serveUrl}`);
        }
        resolve();
      }
      if (config.logLevel >= LogLevel.VeryVerbose) {
        process.stderr.write(`runtime: ${chunk}`);
      }
    };
    managedServer.stderr.on("data", watch);
    managedServer.stdout.on("data", watch);
    managedServer.on("error", (e) => {
      clearTimeout(timer);
      reject(e);
    });
    managedServer.on("exit", (code) => {
      clearTimeout(timer);
      if (!serveUrl) {
        reject(new Error(`serve-mode runtime exited (${code}) before listening. Output:\n${output}`));
      }
    });
  });
}

function stopServer() {
  if (managedServer) {
    try {
      managedServer.kill("SIGKILL");
    } catch {
      // Already gone.
    }
    managedServer = null;
  }
}

/// Run one test by POSTing its assembled script to the serve-mode runtime.
async function runTestOnServer(script) {
  try {
    // The budget scales with --jobs like command mode's: a request queued behind the other
    // in-flight tests is not a hang.
    const response = await fetch(serveUrl, {
      method: "POST",
      body: script,
      signal: AbortSignal.timeout(config.timeout * Math.max(1, config.jobs)),
    });
    const body = await response.text();
    if (!response.ok) {
      return {
        error: new Error(`serve-mode runtime returned HTTP ${response.status}`),
        stdout: body,
        stderr: "",
      };
    }
    return { results: JSON.parse(body), stdout: body, stderr: "" };
  } catch (e) {
    return { error: e, stdout: "", stderr: String(e?.message ?? e) };
  }
}

// ---------------------------------------------------------------------------
// WPT server (for network `fetch()` tests)
// ---------------------------------------------------------------------------

/** A `wpt serve` instance this process started, so it can be stopped on exit. */
let managedWptServer = null;

/** Whether the server answers on one HTTP port (`curl` exits non-zero if not). */
function isWptPortReachable(port) {
  try {
    execFileSync(
      "curl",
      ["-s", "-o", "/dev/null", "--max-time", "3", `http://${WPT_HOST}:${port}/`],
      { stdio: "ignore" },
    );
    return true;
  } catch {
    return false;
  }
}

/**
 * Every HTTP port in `WPT_PORTS` must answer, not just the first: those are the
 * ports `substituteWptTemplates` bakes into `.sub.` tests, and a server missing
 * one still serves everything else. Short-circuits, so the usual case costs a
 * single probe.
 */
function isWptServerReachable() {
  return WPT_PORTS.http.every(port => isWptPortReachable(port));
}

/** The HTTP ports that are not being served, for diagnostics. */
function unreachableWptPorts() {
  return WPT_PORTS.http.filter(port => !isWptPortReachable(port));
}

/**
 * Ensure the WPT server is running on `web-platform.test`. If one is already
 * reachable (started externally), use it. Otherwise start `wpt serve` and wait
 * for it; it is stopped on process exit. The `web-platform.test` hosts entries
 * must already be present (`just wpt-setup` installs them from `deps/wpt-hosts`).
 */
async function ensureWptServer() {
  if (isWptServerReachable()) {
    if (config.logLevel > LogLevel.Quiet) {
      console.log(`Using already-running WPT server at http://${WPT_HOST}:${WPT_PORTS.http[0]}/`);
    }
    return;
  }

  // An external server holding the first port but not the rest is reported rather
  // than used: it cannot be replaced (the port is taken) and its failures are
  // indistinguishable from test regressions — `.sub.` tests substituting an
  // unserved port fail with a bare connection error.
  if (isWptPortReachable(WPT_PORTS.http[0])) {
    const missing = unreachableWptPorts();
    console.error(
      `A WPT server is running on ${WPT_HOST}:${WPT_PORTS.http[0]} but is not serving ` +
        `port(s) ${missing.join(", ")}, which '.sub.' tests are substituted to use.\n` +
        `It was most likely started without '--config ${relativePath("wpt-server-config.json")}', ` +
        `which pins the ports; without it wptserve picks the second HTTP port at random.\n` +
        `Stop it (e.g. 'pkill -f \"wpt serve\"') and re-run, to let this harness start its own.`,
    );
    process.exit(1);
  }

  if (config.logLevel > LogLevel.Quiet) {
    console.info(`Starting WPT server (cmd: ${config.wptRoot}/wpt serve)...`);
  }
  // The config pins every port so that substituteWptTemplates' static table
  // matches the running server; the default config picks the second HTTP port
  // at random ("auto").
  managedWptServer = spawn(
    path.join(config.wptRoot, "wpt"),
    ["serve", "--config", relativePath("wpt-server-config.json")],
    {
      detached: true,
    },
  );
  managedWptServer.on("error", event => {
    console.log(`error starting WPT server: ${event}`);
  });

  if (config.logLevel >= LogLevel.VeryVerbose) {
    managedWptServer.stderr.on("data", data => {
      console.log(`WPT server stderr: ${stripTrailingNewline(data)}`);
    });
    managedWptServer.stdout.on("data", data => {
      console.log(`WPT server stdout: ${stripTrailingNewline(data)}`);
    });
  }

  for (let i = 1; i <= 20; i++) {
    console.log(`Waiting for WPT server to become reachable... (${i}/20)`);
    if (isWptServerReachable()) {
      if (config.logLevel > LogLevel.Quiet) {
        console.log("WPT server is ready.");
      }
      return;
    }
    let resolve;
    let promise = new Promise((r) => (resolve = r));
    setTimeout(() => {resolve();}, 1000);
    await promise;
  }
  const missing = unreachableWptPorts();
  stopWptServer();
  console.error(
    `WPT server did not become reachable on port(s) ${missing.join(", ")}. ` +
      "Ensure the hosts entries exist ('just wpt-setup').",
  );
  process.exit(1);
}

function stripTrailingNewline(str) {
  if (str[str.length - 1] === '\n') {
    return str.substr(0, str.length - 1);
  }
  return str;
}

function stopWptServer() {
  if (managedWptServer) {
    try {
      // Kill the whole process group: `wpt serve` spawns per-protocol children.
      process.kill(-managedWptServer.pid);
    } catch {
      // Already gone.
    }
    managedWptServer = null;
  }
}

process.on("exit", stopWptServer);
process.on("exit", stopServer);

async function run() {
  if (!applyConfig(process.argv)) {
    process.exit(1);
  }

  const { testPaths, totalCount, needsServer } = getTests(config.tests.pattern);

  if (needsServer) {
    await ensureWptServer();
  }
  const pathLength = testPaths.reduce((len, p) => Math.max(p.length, len), 0);

  config.wizened = ensureWizenedRuntime();
  config.precompiled = ensurePrecompiledRuntime();
  if (config.mode === "serve") {
    await startServer();
  }
  const suiteStart = Date.now();

  const concurrency = config.jobs > 1 ? `, ${config.jobs} at a time` : "";
  console.log(
    `Running ${testPaths.length} of ${totalCount} tests${concurrency} ...\n`,
  );

  let expectationsUpdated = 0;
  let unexpectedFailure = false;

  const totalStats = {
    duration: 0,
    count: 0,
    pass: 0,
    missing: 0,
    unexpectedPass: 0,
    unexpectedFail: 0,
  };

  await runTestsConcurrently(testPaths, (testPath, outcome) => {
    const expectations = getExpectedResults(testPath);
    const { results, error, stdout, stderr, duration } = outcome;

    // Make sure a test's entire output is written atomically.
    const lines = [];
    const emit = () => process.stdout.write(lines.join("\n") + "\n");

    if (config.logLevel >= LogLevel.Verbose) {
      lines.push(`Running test ${testPath}`);
    }

    const stats = {
      count: 0,
      pass: 0,
      missing: 0,
      unexpectedPass: 0,
      unexpectedFail: 0,
      duration,
    };

    if (error) {
      const expectPath = expectationsPath(testPath);
      const hasExpectations = existsSync(expectPath);

      if (hasExpectations) {
        lines.push(`UNEXPECTED ERROR: ${testPath} (${duration}ms)`);
        lines.push(`  MESSAGE: ${error.message}`);
        if (config.logLevel > LogLevel.Quiet) {
          lines.push(runtimeOutputBlocks(stdout, stderr));
        } else if (stdout || stderr) {
          // Show the tail rather than the head: WPT_RESULTS_JSON would be at the
          // end on success, and crash diagnostics from stderr are appended last.
          const output = stdout + (stderr ? "\n--- stderr ---\n" + stderr : "");
          const limit = 30;
          const outputLines = output.trim().split("\n");
          const tail =
            outputLines.length > limit
              ? outputLines.slice(-limit).join("\n")
              : output;
          const prefix =
            outputLines.length > limit ? `... (last ${limit} lines)\n` : "";
          lines.push(`  OUTPUT:\n${prefix}${tail}`);
        }
        if (config.tests.updateExpectations) {
          lines.push(`  Removing expectations file ${expectPath}`);
          rmSync(expectPath);
          expectationsUpdated++;
        } else {
          unexpectedFailure = true;
          lines.push(reproLine(testPath));
        }
      } else {
        lines.push(`EXPECTED ERROR: ${testPath} (${duration}ms)`);
      }

      totalStats.duration += duration;
      totalStats.missing += Object.keys(expectations).length;
      emit();
      return;
    }

    // Per-subtest diagnostics, printed after the stat line below.
    const details = [];

    for (const result of results) {
      stats.count++;

      const expectation = expectations[result.name];
      if (expectation) {
        expectation.did_run = true;
      }

      if (result.status === 0) {
        stats.pass++;
      }

      // A FLAKY expectation marks a known-intermittent subtest (e.g. one that
      // depends on connection-reuse timing against a non-compliant server):
      // both outcomes are tolerated, so neither is reported as unexpected.
      if (expectation && expectation.status === "FLAKY") {
        continue;
      }

      if (result.status === 0) {
        if (!expectation || expectation.status === "FAIL") {
          details.push(
            `${expectation ? "UNEXPECTED" : "NEW"} PASS\n  NAME: ${result.name}`,
          );
          stats.unexpectedPass++;
        }
      } else if (!expectation || expectation.status === "PASS") {
        details.push(
          `${expectation ? "UNEXPECTED" : "NEW"} FAIL\n  NAME: ${result.name}\n  MESSAGE: ${result.message}`,
        );
        stats.unexpectedFail++;
      }
    }

    for (const [name, expectation] of Object.entries(expectations)) {
      if (!expectation.did_run) {
        stats.missing++;
        details.push(
          `MISSING TEST\n  NAME: ${name}\n  EXPECTED: ${expectation.status}`,
        );
      }
    }

    totalStats.count += stats.count;
    totalStats.pass += stats.pass;
    totalStats.missing += stats.missing;
    totalStats.unexpectedPass += stats.unexpectedPass;
    totalStats.unexpectedFail += stats.unexpectedFail;
    totalStats.duration += stats.duration;

    lines.push(`${testPath.padEnd(pathLength)} ${formatStats(stats)}`);
    lines.push(...details);

    if (stats.unexpectedFail + stats.unexpectedPass + stats.missing > 0) {
      if (config.tests.updateExpectations) {
        const expectPath = expectationsPath(testPath);
        lines.push(`  Writing expectations to ${expectPath}`);
        // Merge against the file as stored, not against `expectations` (which has
        // already been narrowed to this target), so the other target's statuses survive.
        const newExpectations = mergeExpectations(
          readExpectationsFile(testPath),
          results,
        );
        mkdirSync(path.dirname(expectPath), { recursive: true });
        writeFileSync(
          expectPath,
          JSON.stringify(newExpectations, null, 2) + "\n",
        );
        expectationsUpdated++;
      } else {
        if (config.logLevel > LogLevel.Quiet) {
          lines.push(runtimeOutputBlocks(stdout, stderr));
        }
        lines.push(reproLine(testPath));
      }
    }
    emit();
  });

  // Stop a server this run started, explicitly rather than from the "exit"
  // handler: its open stdout/stderr pipes hold the event loop open, so node
  // never begins exiting on its own and the handler never runs. (The failure
  // paths below force the issue with process.exit, but a fully green run
  // would hang here.)
  stopWptServer();
  stopServer();

  // Report the suite's wall-clock time, not the sum of the per-test durations:
  // with tests running concurrently that sum exceeds the elapsed time, and it
  // grows as concurrency rises because each test's own duration absorbs the
  // contention. The per-test figures above stay as measured.
  totalStats.duration = Date.now() - suiteStart;
  console.log(
    `\n${"Done. Stats:".padEnd(pathLength)} ${formatStats(totalStats)}`,
  );

  if (config.tests.updateExpectations) {
    console.log(`Expectations updated: ${expectationsUpdated}`);
  } else if (
    totalStats.unexpectedFail + totalStats.unexpectedPass + totalStats.missing >
      0 ||
    unexpectedFailure
  ) {
    console.log(
      "\nUnexpected results. Run with --update-expectations to update.",
    );
    process.exitCode = 1;
  }
}

run();

/// The full output the runtime produced, as fenced blocks per stream. Printed
/// for unexpected results when the log level asks for it, so a CI log carries
/// enough to diagnose a result that doesn't reproduce locally.
function runtimeOutputBlocks(stdout, stderr) {
  const block = (name, contents) =>
    `StarlingMonkey ${name}:\n=====\n${contents ? stripTrailingNewline(contents) + "\n" : ""}=====`;
  return block("stdout", stdout) + "\n" + block("stderr", stderr);
}

function reproLine(testPath) {
  if (config.mode === "serve") {
    // Needs a serve-mode runtime running; `just wpt-test-*-serve` starts one.
    return `  To reproduce, run $ curl --data-binary @${runScriptPath(testPath)} ${serveUrl ?? `http://${SERVE_HOST}:${config.servePort}`}`;
  }
  // Name the script the run actually used. Its name is stable across runs, so it
  // stays valid in a command someone runs later; it used to carry a pid, which is
  // why this once had to copy it to a stable name of its own first.
  const { command, args } = testCommand(runScriptPath(testPath), {
    optimized: false,
  });
  return `  To reproduce, run $ ${command} ${args.join(" ")}`;
}
