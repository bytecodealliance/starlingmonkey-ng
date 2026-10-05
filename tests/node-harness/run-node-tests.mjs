#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Runs Node.js's own tests from test/parallel against StarlingMonkey's `--node-compat` mode, and
// compares the results with tests/node-harness/expectations.json. Tests run with
// `--node-http-listen`, so their HTTP servers accept connections.
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
//   --target=TARGET         Execution target: native, wasm, or deno or bun for comparison runs
//                           (default: native)
//   --runtime=PATH          The starlingmonkey binary, starling.wasm for --target=wasm, or the
//                           deno or bun binary (default: debug/starlingmonkey or
//                           wasm32-wasip3/release/starling.wasm in CARGO_TARGET_DIR or target/,
//                           deps/deno-bin/deno or deps/bun-bin/bun)
//   --timeout=MS            Per-test timeout (default: 30000)
//   -j N                    Tests run at once (default: the number of CPUs)
//   --update-expectations   Record the current results as the expectations
//   --mark-flaky            Record the selected tests as FLAKY on the target, without running them
//   --json=FILE             Write each test's status, skip reason, failure signature and output
//                           excerpt to FILE, for tests/node-harness/report.mjs
//   -v                      Print the output of failing tests
//
// tests.txt lists the tests to run, as file name globs, and the tests to leave out, as globs with a
// leading `!`. Tests whose source shows that they need something the runtime can't provide are
// skipped without running, with the reason recorded: runtime flags the runtime rejects, child
// processes, workers, `cluster` or `inspector`. The runtime gets the other flags of a test's
// `// Flags:` line with `--allow-node-runtime-flags`.
//
// `--target=deno` and `--target=bun` run the tests under Deno or Bun, as
// scripts/fetch-other-runtimes.sh downloads them, without the tests' runtime flags. These runs are
// not compared with expectations.json and can't update it. Their `--json` output gives
// tests/node-harness/report.mjs the tests those runtimes pass.
//
// expectations.json maps each test to `{ "status": "PASS" | "FAIL" | "SKIP" | "FLAKY" }`, with
// `wasm_status` where the wasm target's result differs. `FLAKY` marks a test whose result varies
// between runs: its result is neither compared nor recorded. A native update keeps a test's previous
// status as its `wasm_status` when the status changes, so the wasm expectation changes only with a
// wasm update. The run exits with status 1 if a test's status changes to anything but `PASS`, and
// with status 2 if no test was selected.

import { spawn, execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { cpus } from "node:os";
import path from "node:path";

const HARNESS_DIR = path.dirname(new URL(import.meta.url).pathname);
const REPO_ROOT = path.resolve(HARNESS_DIR, "../..");
// The directory Cargo builds into: `CARGO_TARGET_DIR`, or the repository's `target`.
const TARGET_DIR = process.env.CARGO_TARGET_DIR
  ? path.resolve(process.env.CARGO_TARGET_DIR)
  : path.join(REPO_ROOT, "target");
const TESTS_FILE = path.join(HARNESS_DIR, "tests.txt");
const EXPECTATIONS_FILE = path.join(HARNESS_DIR, "expectations.json");
// The directory for each test's temporary directory. The wasm target preopens it under the same
// name. macOS's per-user temporary directory doesn't work there, since it is reached through the
// `/var` symlink, outside the preopen.
const TEMP_ROOT = "/tmp";

/** The reasons to skip a test without running it, by a pattern its source matches. */
const SKIP_PATTERNS = [
  [/require\(['"](node:)?child_process['"]\)/, "spawns child processes"],
  [/require\(['"](node:)?cluster['"]\)/, "uses cluster"],
  [/new Worker\(|require\(['"](node:)?worker_threads['"]\)/, "uses worker threads"],
  [/require\(['"](node:)?inspector(\/promises)?['"]\)/, "uses the inspector"],
  [/common\.spawnPromisified|common\.childShouldThrowAndAbort|spawnSyncAndExit/, "spawns child processes"],
];

/**
 * The Node.js runtime flags the runtime rejects, by name with dashes. A test whose `// Flags:`
 * line has one is skipped. The runtime gets the other flags with `--allow-node-runtime-flags`.
 * `REJECTED` in `crates/builtins/node/src/runtime_flags.rs` lists the same flags.
 */
const UNSUPPORTED_FLAGS = [
  "allow-natives-syntax",
  "expose-externalize-string",
  "permission",
  "allow-fs-read",
  "allow-fs-write",
  "allow-child-process",
  "allow-worker",
  "allow-addons",
  "allow-wasi",
];

/** The flags of the `// Flags:` line in `source`, as test/common reads them. */
function testFlags(source) {
  const match = /\/\/ Flags:\s+(--.*)$/m.exec(source);
  return match ? match[1].split(/\s+/).filter(Boolean) : [];
}

/** The name of runtime flag `flag`, without leading dashes or a value, and with dashes. */
function flagName(flag) {
  return flag.replace(/^-+/, "").split("=")[0].replaceAll("_", "-");
}

/** The runtime arguments that pass the `// Flags:` of `file` to the runtime. */
function flagArgs(config, file) {
  const flags = testFlags(readFileSync(path.join(config.nodeRoot, "test/parallel", file), "utf8"));
  return flags.length > 0 ? ["--allow-node-runtime-flags", ...flags] : [];
}

/** The comparison targets: each runtime's default binary and its arguments before the test. */
const OTHER_RUNTIMES = {
  deno: {
    runtime: "deps/deno-bin/deno",
    args: [
      "run",
      "-A",
      "--quiet",
      "--unstable-detect-cjs",
      "--unstable-bare-node-builtins",
      "--unstable-node-globals",
      "--unstable-unsafe-proto",
    ],
  },
  bun: { runtime: "deps/bun-bin/bun", args: [] },
};

function parseArgs(argv) {
  const config = {
    nodeRoot: path.join(REPO_ROOT, "deps/node"),
    target: "native",
    runtime: null,
    timeout: 30000,
    jobs: cpus().length,
    update: false,
    markFlaky: false,
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
    else if (arg === "--mark-flaky") config.markFlaky = true;
    else if (arg.startsWith("--json=")) config.json = path.resolve(value("--json"));
    else if (arg === "-v") config.verbose = true;
    else if (arg === "--help") {
      const lines = readFileSync(new URL(import.meta.url), "utf8").split("\n").slice(1);
      console.log(lines.slice(0, lines.findIndex((line) => !line.startsWith("//"))).join("\n"));
      process.exit(0);
    } else config.patterns.push(arg);
  }
  config.runtime ??= OTHER_RUNTIMES[config.target]
    ? path.join(REPO_ROOT, OTHER_RUNTIMES[config.target].runtime)
    : path.join(TARGET_DIR, config.target === "wasm" ? "wasm32-wasip3/release/starling.wasm" : "debug/starlingmonkey");
  if ((config.update || config.markFlaky) && OTHER_RUNTIMES[config.target]) {
    console.error(`--update-expectations and --mark-flaky can't be used with --target=${config.target}`);
    process.exit(2);
  }
  if (config.markFlaky && (config.update || config.patterns.length === 0)) {
    console.error("--mark-flaky needs test patterns and can't be combined with --update-expectations");
    process.exit(2);
  }
  return config;
}

/** A regular expression for the file name glob `glob`, where `*` matches any characters. */
function globToRegExp(glob) {
  return new RegExp(`^${glob.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`);
}

/**
 * The tests tests.txt selects that match one of `patterns`, or all of them without patterns. A
 * test is selected if it matches a glob line and no `!` glob line.
 */
function selectTests(config) {
  const lines = readFileSync(TESTS_FILE, "utf8")
    .split("\n")
    .map((line) => line.replace(/#.*/, "").trim())
    .filter(Boolean);
  const included = lines.filter((line) => !line.startsWith("!")).map(globToRegExp);
  const excluded = lines.filter((line) => line.startsWith("!")).map((line) => globToRegExp(line.slice(1)));
  const files = readdirSync(path.join(config.nodeRoot, "test/parallel")).filter(
    (file) =>
      /\.(js|mjs|cjs)$/.test(file) &&
      included.some((glob) => glob.test(file)) &&
      !excluded.some((glob) => glob.test(file)),
  );
  return files
    .filter((file) => config.patterns.length === 0 || config.patterns.some((pattern) => file.includes(pattern)))
    .sort();
}

/** The reason to skip `file` without running it, or `null`. */
function skipReason(config, file) {
  const source = readFileSync(path.join(config.nodeRoot, "test/parallel", file), "utf8");
  const unsupported = testFlags(source).find((flag) => UNSUPPORTED_FLAGS.includes(flagName(flag)));
  if (unsupported) return `needs the unsupported runtime flag ${unsupported}`;
  for (const [pattern, reason] of SKIP_PATTERNS) {
    if (pattern.test(source)) return reason;
  }
  return null;
}

/** The command and arguments that run `file` on the configured target, with `env`. */
function command(config, file, env) {
  const testPath = path.join(config.nodeRoot, "test/parallel", file);
  const other = OTHER_RUNTIMES[config.target];
  if (other) {
    return { command: config.runtime, args: [...other.args, testPath], env: { ...process.env, ...env } };
  }
  if (config.target !== "wasm") {
    return {
      command: config.runtime,
      args: ["--node-compat", "--node-http-listen", ...flagArgs(config, file), testPath],
      env: { ...process.env, ...env },
    };
  }
  // wasmtime maps the repository to the guest's root and the temporary directory to itself, so
  // guest paths are the repository-relative ones. The Node.js clone is mapped to its guest path
  // too, since WASI doesn't follow a symbolic link out of a preopened directory, and a clone
  // outside the repository isn't under the root.
  const guestNodeRoot = config.nodeRoot.startsWith(REPO_ROOT + path.sep)
    ? "/" + path.relative(REPO_ROOT, config.nodeRoot)
    : "/node";
  const guest = (host) => path.posix.join(guestNodeRoot, path.relative(config.nodeRoot, host));
  return {
    command: "wasmtime",
    args: [
      "run",
      `--dir=${REPO_ROOT}::/`,
      `--dir=${realpathSync(config.nodeRoot)}::${guestNodeRoot}`,
      `--dir=${TEMP_ROOT}`,
      "-Sinherit-network=y,tcp=y,udp=y,http=y,p3=y",
      "-Wcomponent-model-async=y",
      ...Object.entries(env).map(([name, value]) => `--env=${name}=${value}`),
      config.runtime,
      "--node-compat",
      "--node-http-listen",
      ...flagArgs(config, file),
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
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      if (signal && !output.includes("[timed out after")) output += `\n[killed by ${signal}]`;
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
  if (/panicked at|Segmentation fault|SIGSEGV|SIGABRT|Assertion failure:|\[killed by SIG/.test(output)) return "crash";
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

/** Warn if `runtime` is older than the repository's last commit, for a binary built before it. */
function warnIfStale(runtime) {
  if (!existsSync(runtime) || !runtime.startsWith(REPO_ROOT)) return;
  let committed;
  try {
    committed = Number(execFileSync("git", ["-C", REPO_ROOT, "log", "-1", "--format=%ct"], { encoding: "utf8" }));
  } catch {
    return;
  }
  if (statSync(runtime).mtimeMs / 1000 < committed) {
    console.warn(`Warning: ${path.relative(REPO_ROOT, runtime)} is older than the last commit.`);
  }
}

/** Write `expectations` to expectations.json, sorted by test name. */
function writeExpectations(expectations) {
  const sorted = Object.fromEntries(Object.entries(expectations).sort(([a], [b]) => a.localeCompare(b, 'en')));
  writeFileSync(EXPECTATIONS_FILE, JSON.stringify(sorted, null, 2) + "\n");
  console.log(`Updated ${EXPECTATIONS_FILE}`);
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
  warnIfStale(config.runtime);
  const compared = !OTHER_RUNTIMES[config.target];
  const expectations = compared && existsSync(EXPECTATIONS_FILE) ? JSON.parse(readFileSync(EXPECTATIONS_FILE, "utf8")) : {};
  const tests = selectTests(config);
  if (config.markFlaky) {
    if (tests.length === 0) {
      console.error("No tests selected.");
      process.exit(2);
    }
    for (const file of tests) {
      const entry = { ...(expectations[file] ?? {}) };
      if (config.target === "wasm") {
        // The native status stays as it is. A test without one gets the wasm status as both.
        entry.status ??= "FLAKY";
        if (entry.status === "FLAKY") delete entry.wasm_status;
        else entry.wasm_status = "FLAKY";
      } else {
        // The previous status stays the wasm expectation.
        if (entry.status && entry.status !== "FLAKY" && !entry.wasm_status) entry.wasm_status = entry.status;
        entry.status = "FLAKY";
        if (entry.wasm_status === "FLAKY") delete entry.wasm_status;
      }
      expectations[file] = entry;
      console.log(`FLAKY ${file}`);
    }
    writeExpectations(expectations);
    return;
  }
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
        expected === undefined
          ? "?"
          : expected === result.status || expected === "FLAKY"
            ? " "
            : result.status === "PASS"
              ? "+"
              : "-";
      if ((compared && marker !== " ") || config.verbose) {
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
    if (expected === "FLAKY") continue;
    if (expected === undefined) unexpected.push(`${file} (${status}, no expectation)`);
    else if (expected !== "PASS" && status === "PASS") improvements.push(file);
    else if (expected !== status) regressions.push(`${file} (${status}, expected ${expected})`);
  }

  if (config.update) {
    for (const file of tests) {
      if (expectedStatus(config, expectations, file) === "FLAKY") continue;
      const { status, reason } = results.get(file);
      const entry = { ...(expectations[file] ?? {}) };
      if (config.target === "wasm") {
        // A wasm run records the native status only for a test without one.
        entry.status ??= status;
        if (status === entry.status) delete entry.wasm_status;
        else entry.wasm_status = status;
      } else {
        // The previous status stays the wasm expectation until a wasm update records it.
        if (entry.status && entry.status !== status && !entry.wasm_status) entry.wasm_status = entry.status;
        entry.status = status;
        if (entry.wasm_status === status) delete entry.wasm_status;
      }
      if (reason) entry.reason = reason;
      else delete entry.reason;
      expectations[file] = entry;
    }
    writeExpectations(expectations);
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
  if (compared && unexpected.length > 0 && !config.update) {
    console.log(`Without expectations:\n${unexpected.map((file) => `  ${file}`).join("\n")}`);
  }
  if (regressions.length > 0) {
    console.log(`Regressions:\n${regressions.map((file) => `  ${file}`).join("\n")}`);
    if (!config.update) process.exit(1);
  }
}

main();
