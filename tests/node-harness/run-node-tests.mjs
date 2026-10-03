#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Runs Node.js's own tests from test/parallel against StarlingMonkey's `--node-compat` mode, and
// compares the results with tests/node-harness/expectations.json.
//
// The tests come from a sparse clone of Node.js at the tag scripts/clone-node-tests.sh pins. Each
// test runs as its own process with Node's unmodified test/common, and passes if it exits with
// status 0 within the timeout. A test that calls `common.skip()` counts as skipped.
//
// Usage:
//   node tests/node-harness/run-node-tests.mjs [options] [pattern...]
//
// Options:
//   --node-root=PATH        The Node.js clone (default: deps/node)
//   --target=native|wasm    Execution target (default: native)
//   --runtime=PATH          The starlingmonkey binary, or starling.wasm for --target=wasm
//                           (default: target/debug/starlingmonkey, or
//                           target/wasm32-wasip3/release/starling.wasm for --target=wasm)
//   --timeout=MS            Per-test timeout (default: 30000)
//   -j N                    Tests run at once (default: the number of CPUs)
//   --update-expectations   Record the current results as the expectations
//   --json=FILE             Write each test's status, skip reason, failure signature and output
//                           excerpt to FILE, for tests/node-harness/report.mjs
//   -v                      Print the output of failing tests
//
// tests.txt lists the tests to run, as file name globs. Tests whose source shows that they need
// something the runtime can't provide are skipped without running, with the reason recorded:
// runtime flags (`// Flags:`), child processes, workers, `cluster`, `inspector` or Node.js
// internals.
//
// expectations.json maps each test to `{ "status": "PASS" | "FAIL" | "SKIP" }`, with
// `wasm_status` where the wasm target's result differs. The run exits with status 1 if a test
// expected to pass fails or skips, and with status 2 if no test was selected.

import { spawn, execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { cpus } from "node:os";
import path from "node:path";

const HARNESS_DIR = path.dirname(new URL(import.meta.url).pathname);
const REPO_ROOT = path.resolve(HARNESS_DIR, "../..");
const TESTS_FILE = path.join(HARNESS_DIR, "tests.txt");
const EXPECTATIONS_FILE = path.join(HARNESS_DIR, "expectations.json");
// The directory for each test's temporary directory. The wasm target preopens it under the same
// name. macOS's per-user temporary directory doesn't work there, since it is reached through the
// `/var` symlink, outside the preopen.
const TEMP_ROOT = "/tmp";

/** The reasons to skip a test without running it, by a pattern its source matches. */
const SKIP_PATTERNS = [
  [/^\/\/ Flags:/m, "needs runtime flags"],
  [/require\(['"](node:)?child_process['"]\)/, "spawns child processes"],
  [/require\(['"](node:)?cluster['"]\)/, "uses cluster"],
  [/new Worker\(|require\(['"](node:)?worker_threads['"]\)/, "uses worker threads"],
  [/require\(['"](node:)?inspector(\/promises)?['"]\)/, "uses the inspector"],
  [/require\(['"]internal\//, "uses Node.js internals"],
  [/common\.spawnPromisified|common\.childShouldThrowAndAbort|spawnSyncAndExit/, "spawns child processes"],
];

function parseArgs(argv) {
  const config = {
    nodeRoot: path.join(REPO_ROOT, "deps/node"),
    target: "native",
    runtime: null,
    timeout: 30000,
    jobs: cpus().length,
    update: false,
    json: null,
    verbose: false,
    patterns: [],
  };
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    const value = (name) => arg.slice(name.length + 1);
    if (arg.startsWith("--node-root=")) config.nodeRoot = path.resolve(value("--node-root"));
    else if (arg.startsWith("--target=")) config.target = value("--target");
    else if (arg.startsWith("--runtime=")) config.runtime = path.resolve(value("--runtime"));
    else if (arg.startsWith("--timeout=")) config.timeout = Number(value("--timeout"));
    else if (arg === "-j") config.jobs = Number(argv[++index]);
    else if (arg === "--update-expectations") config.update = true;
    else if (arg.startsWith("--json=")) config.json = path.resolve(value("--json"));
    else if (arg === "-v") config.verbose = true;
    else if (arg === "--help") {
      const lines = readFileSync(new URL(import.meta.url), "utf8").split("\n").slice(1);
      console.log(lines.slice(0, lines.findIndex((line) => !line.startsWith("//"))).join("\n"));
      process.exit(0);
    } else config.patterns.push(arg);
  }
  config.runtime ??= path.join(
    REPO_ROOT,
    config.target === "wasm" ? "target/wasm32-wasip3/release/starling.wasm" : "target/debug/starlingmonkey",
  );
  return config;
}

/** A regular expression for the file name glob `glob`, where `*` matches any characters. */
function globToRegExp(glob) {
  return new RegExp(`^${glob.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`);
}

/** The tests tests.txt selects that match one of `patterns`, or all of them without patterns. */
function selectTests(config) {
  const globs = readFileSync(TESTS_FILE, "utf8")
    .split("\n")
    .map((line) => line.replace(/#.*/, "").trim())
    .filter(Boolean)
    .map(globToRegExp);
  const files = readdirSync(path.join(config.nodeRoot, "test/parallel")).filter(
    (file) => /\.(js|mjs|cjs)$/.test(file) && globs.some((glob) => glob.test(file)),
  );
  return files
    .filter((file) => config.patterns.length === 0 || config.patterns.some((pattern) => file.includes(pattern)))
    .sort();
}

/** The reason to skip `file` without running it, or `null`. */
function skipReason(config, file) {
  const source = readFileSync(path.join(config.nodeRoot, "test/parallel", file), "utf8");
  for (const [pattern, reason] of SKIP_PATTERNS) {
    if (pattern.test(source)) return reason;
  }
  return null;
}

/** The command and arguments that run `file` on the configured target, with `env`. */
function command(config, file, env) {
  const testPath = path.join(config.nodeRoot, "test/parallel", file);
  if (config.target !== "wasm") {
    return { command: config.runtime, args: ["--node-compat", testPath], env: { ...process.env, ...env } };
  }
  // wasmtime maps the repository to the guest's root and the temporary directory to itself, so
  // guest paths are the repository-relative ones.
  const guest = (host) => "/" + path.relative(REPO_ROOT, host);
  return {
    command: "wasmtime",
    args: [
      "run",
      `--dir=${REPO_ROOT}::/`,
      `--dir=${TEMP_ROOT}`,
      "-Sinherit-network=y,tcp=y,udp=y,http=y,p3=y",
      "-Wcomponent-model-async=y",
      ...Object.entries(env).map(([name, value]) => `--env=${name}=${value}`),
      config.runtime,
      "--node-compat",
      guest(testPath),
    ],
    env: process.env,
  };
}

/** Run `file`, and resolve with its status and output. */
function runTest(config, file) {
  const testDir = mkdtempSync(path.join(TEMP_ROOT, "starling-node-test-"));
  const env = {
    NODE_TEST_DIR: testDir,
    NODE_TEST_KNOWN_GLOBALS: "0",
    NODE_SKIP_FLAG_CHECK: "1",
  };
  const { command: program, args, env: processEnv } = command(config, file, env);
  return new Promise((resolve) => {
    const child = spawn(program, args, { cwd: config.nodeRoot, env: processEnv });
    let output = "";
    child.stdout.on("data", (data) => (output += data));
    child.stderr.on("data", (data) => (output += data));
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      output += `\n[timed out after ${config.timeout} ms]`;
    }, config.timeout);
    child.on("close", (code) => {
      clearTimeout(timer);
      rmSync(testDir, { recursive: true, force: true });
      // A test that skips exits with status 0 after printing the skip line.
      const skipped = code === 0 && /^1\.\.0 # Skipped:/m.test(output);
      const status = skipped ? "SKIP" : code === 0 ? "PASS" : "FAIL";
      resolve({ status, output });
    });
  });
}

/**
 * The signature of a failing test's `output`, which groups failures with the same cause: the
 * unimplemented API, `http-command-mode` for a server that needs serve mode, `timeout`, `crash`,
 * `assertion`, `not-a-function:<expression>`, or the first error's name and code.
 */
function failureSignature(output) {
  if (output.includes("[timed out after")) return "timeout";
  const unimplemented = /The (\S+) method is not implemented/.exec(output);
  if (unimplemented) return `unimplemented:${unimplemented[1]}`;
  if (output.includes("ERR_SERVE_MODE_REQUIRED")) return "http-command-mode";
  if (/panicked at|Segmentation fault|SIGSEGV|SIGABRT|Assertion failure:/.test(output)) return "crash";
  const notFunction = /(\S+) is not a function/.exec(output);
  if (notFunction) return `not-a-function:${notFunction[1]}`;
  if (/AssertionError|ERR_ASSERTION|Expected values to be|Mismatched .* function calls/.test(output)) return "assertion";
  const code = /code: '([A-Z_0-9]+)'|code="?([A-Z_0-9]+)"?/.exec(output);
  const name = /\b([A-Z][A-Za-z]*Error)\b/.exec(output);
  if (name || code) return `error:${name?.[1] ?? "Error"}${code ? `[${code[1] ?? code[2]}]` : ""}`;
  return "other";
}

/** The skip message `common.skip()` printed in `output`, if any. */
function skipMessage(output) {
  return /^1\.\.0 # Skipped: (.*)$/m.exec(output)?.[1];
}

/** The expected status of `file` on the configured target, or `undefined` without one. */
function expectedStatus(config, expectations, file) {
  const entry = expectations[file];
  if (!entry) return undefined;
  return (config.target === "wasm" && entry.wasm_status) || entry.status;
}

async function main() {
  const config = parseArgs(process.argv.slice(2));
  if (!existsSync(path.join(config.nodeRoot, "test/parallel"))) {
    console.error(`No Node.js tests at ${config.nodeRoot}; run scripts/clone-node-tests.sh.`);
    process.exit(2);
  }
  if (config.target === "wasm") execFileSync("wasmtime", ["--version"]);
  const expectations = existsSync(EXPECTATIONS_FILE) ? JSON.parse(readFileSync(EXPECTATIONS_FILE, "utf8")) : {};
  const tests = selectTests(config);
  const results = new Map();
  const queue = [...tests];
  const started = Date.now();

  async function worker() {
    while (queue.length > 0) {
      const file = queue.shift();
      const reason = skipReason(config, file);
      const result = reason ? { status: "SKIP", output: reason, reason } : await runTest(config, file);
      results.set(file, result);
      const expected = expectedStatus(config, expectations, file);
      const marker =
        expected === undefined ? "?" : expected === result.status ? " " : result.status === "PASS" ? "+" : "-";
      if (marker !== " " || config.verbose) {
        console.log(`${marker} ${result.status.padEnd(4)} ${file}${result.reason ? ` (${result.reason})` : ""}`);
      }
      if (config.verbose && result.status === "FAIL") {
        console.log(result.output.split("\n").slice(0, 12).map((line) => `      ${line}`).join("\n"));
      }
    }
  }
  await Promise.all(Array.from({ length: Math.max(1, config.jobs) }, worker));

  if (tests.length === 0) {
    console.error("No tests selected.");
    process.exit(2);
  }
  const counts = { PASS: 0, FAIL: 0, SKIP: 0 };
  const regressions = [];
  const improvements = [];
  const unexpected = [];
  for (const file of tests) {
    const { status } = results.get(file);
    counts[status]++;
    const expected = expectedStatus(config, expectations, file);
    if (expected === undefined) unexpected.push(`${file} (${status}, no expectation)`);
    else if (expected === "PASS" && status !== "PASS") regressions.push(`${file} (${status})`);
    else if (expected !== "PASS" && status === "PASS") improvements.push(file);
  }

  if (config.update) {
    for (const file of tests) {
      const { status, reason } = results.get(file);
      const entry = { ...(expectations[file] ?? {}) };
      if (config.target === "wasm") {
        // A wasm run records the native status only for a test without one.
        entry.status ??= status;
        if (status === entry.status) delete entry.wasm_status;
        else entry.wasm_status = status;
      } else {
        entry.status = status;
      }
      if (reason) entry.reason = reason;
      else delete entry.reason;
      expectations[file] = entry;
    }
    const sorted = Object.fromEntries(Object.entries(expectations).sort(([a], [b]) => a.localeCompare(b)));
    writeFileSync(EXPECTATIONS_FILE, JSON.stringify(sorted, null, 2) + "\n");
    console.log(`Updated ${EXPECTATIONS_FILE}`);
  }

  if (config.json) {
    const report = {
      target: config.target,
      runtime: path.relative(REPO_ROOT, config.runtime),
      date: new Date().toISOString(),
      tests: Object.fromEntries(
        tests.map((file) => {
          const { status, output, reason } = results.get(file);
          const entry = { status };
          if (status === "SKIP") entry.reason = reason ?? skipMessage(output) ?? "skipped";
          if (status === "FAIL") {
            entry.signature = failureSignature(output);
            entry.excerpt = output.split("\n").filter((line) => line.trim()).slice(0, 6).join("\n").slice(0, 600);
          }
          return [file, entry];
        }),
      ),
    };
    writeFileSync(config.json, JSON.stringify(report, null, 2) + "\n");
    console.log(`Wrote ${config.json}`);
  }

  const seconds = ((Date.now() - started) / 1000).toFixed(1);
  console.log(
    `\n${counts.PASS} passed, ${counts.FAIL} failed, ${counts.SKIP} skipped of ${tests.length} tests ` +
      `(${config.target}, ${seconds}s): ${improvements.length} newly passing, ${regressions.length} regressed`,
  );
  if (unexpected.length > 0 && !config.update) {
    console.log(`Without expectations:\n${unexpected.map((file) => `  ${file}`).join("\n")}`);
  }
  if (regressions.length > 0) {
    console.log(`Regressions:\n${regressions.map((file) => `  ${file}`).join("\n")}`);
    if (!config.update) process.exit(1);
  }
}

main();
