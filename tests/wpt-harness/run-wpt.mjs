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
// from the baseline. The browser-security Fetch restrictions are off by default
// outside WPT mode, so `--permissive` covers the configuration users run.

import { execFileSync, spawn } from "child_process";
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

// tests.json line prefixes, each optionally followed by a `(comment)`:
//   "SKIP "        never run, on any target.
//   "SLOW "        skipped when --skip-slow-tests is passed.
//   "SKIP-WASM "   run on native, skipped on the wasm target.
//   "SKIP-NATIVE " run on wasm, skipped on the native target.
//   "NET "         needs the live WPT server, which the harness starts if no
//                  server is running.
//
// SKIP-WASM and SKIP-NATIVE are for tests that cannot run on that target at all.
// Subtests that behave differently per target get a `wasm_status` instead.
const SKIP_PREFIX = "SKIP";
const SLOW_PREFIX = "SLOW";
const SKIP_WASM_PREFIX = "SKIP-WASM";
const SKIP_NATIVE_PREFIX = "SKIP-NATIVE";
const NET_PREFIX = "NET";

const LogLevel = { Quiet: 0, Verbose: 1, VeryVerbose: 2 };

const config = {
  // Default automatically adjusted to "target/wasm32-wasip2/debug/starling.wasm" for wasm target.
  runtime: "target/debug/starlingmonkey",
  target: "native", // "native" or "wasm"
  // "cli" runs each test as a one-shot command, "serve" POSTs it to a long-running server built
  // from the same binary (see wpt-server.js).
  mode: "cli",
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
  // Run with the request restrictions disabled, checking `permissive_status`.
  permissive: false,
  // Twice the number of CPUs, since many tests aren't CPU-bound.
  jobs: Math.max(1, cpus().length * 2),
  // The ahead-of-time compiled wasm runtime, unless --no-precompile is passed.
  precompiled: null,
  usePrecompiled: true,
  // The wasm runtime pre-initialized with Wizer, with --wizen in serve mode.
  wizen: false,
  wizened: null,
  logLevel: LogLevel.Quiet,
  // Per-test timeout for a serial run, scaled by --jobs. It has to stay above testharness.js's own
  // test timeout and `WAIT_LIMIT_MS` in wpt-server.js, so that those fire first.
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

  // The native server runs each request in its own realm with `--serve-isolated`, which only works
  // with one request at a time. The wasm server uses one instance per request instead.
  if (config.mode === "serve" && config.target !== "wasm" && config.jobs !== 1) {
    if (config.logLevel > LogLevel.Quiet) {
      console.log("The native serve-mode server is serial; running tests one at a time.");
    }
    config.jobs = 1;
  }

  // The restrictions are implemented in fetch code both targets share, so there is no
  // per-target `permissive_status`.
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

// Legacy WPT path aliases. The WPT HTTP server handles some redirects that
// aren't present when loading files directly from disk.
const WPT_PATH_ALIASES = {
  "/resources/WebIDLParser.js": "/resources/webidl2/lib/webidl2.js",
};

// ---------------------------------------------------------------------------
// wptserve `{{...}}` template substitution
// ---------------------------------------------------------------------------
//
// The WPT server fills `{{host}}`-style placeholders into the `.sub.` files it
// serves. This harness loads files from disk, so it substitutes them itself,
// with the values of the server the NET tests talk to: wptserve's default hosts,
// and the ports of wpt-server-config.json, which ensureWptServer passes to
// `wpt serve`.
const WPT_HOST = "web-platform.test";
const WPT_ALT_HOST = "not-web-platform.test";
const WPT_SERVER_CONFIG = relativePath("wpt-server-config.json");
const WPT_PORTS = JSON.parse(readFileSync(WPT_SERVER_CONFIG, "utf-8")).ports;

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
      // A port the config doesn't list as a number, such as wptserve's "auto", is left as a
      // placeholder.
      /\{\{ports\[(\w+)\]\[(\d+)\]\}\}/g,
      (match, scheme, index) => {
        const port = WPT_PORTS[scheme]?.[index];
        return Number.isInteger(port) ? String(port) : match;
      },
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
  // the IDL files of its first two arguments from the WPT /interfaces/ directory.
  const idlTestMatch = testSource.match(/idl_test\(\s*\[([^\]]*)\](?:\s*,\s*\[([^\]]*)\])?/);
  if (idlTestMatch) {
    const idlSpecs = [idlTestMatch[1], idlTestMatch[2] ?? ""]
      .flatMap((list) => list.split(","))
      .map((s) => s.trim().replace(/^['"]|['"]$/g, ""))
      .filter(Boolean);
    const idlMap = {};
    for (const spec of idlSpecs) {
      const idlPath = path.join(config.wptRoot, "interfaces", spec + ".idl");
      if (existsSync(idlPath)) {
        idlMap["/interfaces/" + spec + ".idl"] = readFileSync(idlPath, "utf-8");
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

// The expectations for the configuration being run. `wasm_status` overrides
// `status` on the wasm target, and `permissive_status` with `--permissive`. An
// entry with no status for this configuration (say, a `wasm_status`-only entry
// seen on native) is dropped, so the subtest has no expectation.
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
// field for the configuration that ran.
function mergeExpectations(previous, results) {
  const field = runStatusField();
  const merged = {};
  for (const result of results) {
    const prev = previous[result.name] ?? {};
    const observed = result.status === 0 ? "PASS" : "FAIL";
    // A FLAKY marker is kept.
    const keep = (stored) => (stored === "FLAKY" ? "FLAKY" : undefined);

    const values = { ...prev, [field]: keep(prev[field]) ?? observed };
    // An override is recorded only where it differs from the baseline.
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
  // Subtests this run did not observe keep the other configurations' fields.
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

// Codegen options passed to both `wasmtime compile` and `wasmtime run`, since a
// precompiled artifact only loads in a runtime configured the same way.
//
// Native unwind information is off because deregistering it on macOS is
// quadratic in the function count: with it, a wasm test took ~1.4s instead of
// ~0.35s. Native profilers and debuggers then can't unwind through wasm frames.
const WASMTIME_CODEGEN_FLAGS = ["-C", "native-unwind-info=n"];

// The WASI capabilities the runtime needs: environment (its configuration arrives there in serve
// mode), and the network/HTTP stack the `fetch` tests exercise.
const WASI_FLAGS = "inherit-env=y,inherit-network=y,http=y,tcp=y,udp=y,p3=y";

// Pre-initialize the wasm runtime with Wizer, with the serve-mode WPT harness and its
// configuration in the snapshot. Rebuilt whenever the runtime or the configuration changes.
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
        // The component type still declares `wizer-initialize` after the snapshot, so dropping the
        // core function would make the component unloadable.
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

// Compile the wasm runtime ahead of time, once per runtime build and wasmtime
// version, and run the tests against the result. On failure, the tests run the
// module directly.
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

/// Spawn `command`, collecting stdout and stderr, and kill it after `timeout` ms
/// or once stdout exceeds `maxBuffer` bytes.
function spawnCollecting(command, args, { timeout, maxBuffer }) {
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(command, args);
    } catch (e) {
      resolve({ error: e, stdout: "", stderr: "" });
      return;
    }
    // Decoded once at the end, since a chunk can end inside a UTF-8 sequence.
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

/// The runtime flags for `--permissive`, which every mode that starts a runtime passes.
function restrictionFlags() {
  return config.permissive ? ["--web-security-restrictions=false"] : [];
}

// Build the command that runs `scriptFile` as a one-shot command: the runtime binary on native,
// and the component under `wasmtime run` on wasm. `optimized` adds the precompiled runtime and the
// codegen flags, which `reproLine` leaves out.
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

  // The script file is named by target and test, so concurrent tests and runs of the other target
  // don't overwrite it. It is left behind for the "To reproduce" command.
  mkdirSync(config.tmpDir, { recursive: true });
  const tmpFile = runScriptPath(testPath);
  writeFileSync(tmpFile, script);

  if (config.mode === "serve") {
    return runTestOnServer(script);
  }

  const { command, args } = testCommand(tmpFile);
  try {

    const result = await spawnCollecting(command, args, {
      // A test's wall clock includes waiting for a core, so the budget grows with --jobs.
      timeout: config.timeout * Math.max(1, config.jobs),
      maxBuffer: 10 * 1024 * 1024,
    });

    const { stdout, stderr } = result;

    if (result.error) {
      return { error: result.error, stdout, stderr };
    }
    if (result.status !== 0 || result.signal) {
      const reason = result.signal
        ? `killed by signal ${result.signal}`
        : `exited with status ${result.status}`;
      return { error: new Error(reason), stdout, stderr };
    }

    const marker = "WPT_RESULTS_JSON:";
    for (const line of stdout.split("\n")) {
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
// `report` in test order, so the output and the expectation updates don't
// depend on which test finished first.
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
      task.finally(() => running.delete(task));
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
// The runtime runs `wpt-server.js` as its content script, whose `fetch` handler evaluates the
// assembled test script in the request body and responds with its results.

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
        // Each request in its own global, so tests don't see each other's declarations.
        "--serve-isolated",
        "--legacy-script",
        ...restrictionFlags(),
        serverScript,
      ],
      ready: /serving on (http:\/\/[\d.]+:\d+)/,
    };
  }
  // `wasmtime serve` passes the guest no arguments, so the configuration goes through the
  // environment, unless the wizened runtime already has it. `--permissive` is native-only.
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
      // One request per instance, so each test gets a fresh global. WASIp3 components default to
      // 128 requests per instance.
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
    // Both runtimes print the readiness line to stderr. stdout is watched too.
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
 * Whether a server responds on every HTTP port in `WPT_PORTS`, since `.sub.`
 * tests use all of them.
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

  // An external server on the first port that doesn't serve the others fails the run.
  if (isWptPortReachable(WPT_PORTS.http[0])) {
    const missing = unreachableWptPorts();
    console.error(
      `A WPT server is running on ${WPT_HOST}:${WPT_PORTS.http[0]} but is not serving ` +
        `port(s) ${missing.join(", ")}, which '.sub.' tests are substituted to use.\n` +
        `It was most likely started without '--config ${WPT_SERVER_CONFIG}', ` +
        `which pins the ports; without it wptserve picks the second HTTP port at random.\n` +
        `Stop it (e.g. 'pkill -f \"wpt serve\"') and re-run, to let this harness start its own.`,
    );
    process.exit(1);
  }

  if (config.logLevel > LogLevel.Quiet) {
    console.info(`Starting WPT server (cmd: ${config.wptRoot}/wpt serve)...`);
  }
  // The config pins every port, which wptserve's default config doesn't.
  managedWptServer = spawn(
    path.join(config.wptRoot, "wpt"),
    ["serve", "--config", WPT_SERVER_CONFIG],
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
    await new Promise((resolve) => setTimeout(resolve, 1000));
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
          // The tail, which has the crash diagnostics from stderr.
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

      // A FLAKY subtest's result is not compared.
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
        // Merged with the file as stored, so the other configurations' statuses stay.
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

  // The servers' open pipes keep the event loop alive, so they are stopped here
  // rather than only from the "exit" handler.
  stopWptServer();
  stopServer();

  // The suite's wall-clock time, since concurrent tests' durations overlap.
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

/// The full output the runtime produced, as fenced blocks per stream.
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
  const { command, args } = testCommand(runScriptPath(testPath), {
    optimized: false,
  });
  return `  To reproduce, run $ ${command} ${args.join(" ")}`;
}
