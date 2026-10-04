#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Differential fuzzing of StarlingMonkey's `--node-compat` mode, with Node.js as the oracle.
//
// Each program is a sequence of operations drawn from the generators in generators.mjs, after
// prelude.js. The driver runs a program twice under Node.js and once under StarlingMonkey (with
// `--node-http-listen`, so HTTP servers accept connections), and compares the traces operation by
// operation. Operations whose two Node.js traces differ are nondeterministic and are not compared.
// Each difference is rerun as a program of its one operation: if that program still differs, it
// becomes the difference's reproduction, and otherwise the difference is reported as an
// interaction with the program it came from.
//
// Differences are grouped by signature: generator, variant, the kind of difference, and the
// error code involved. known-diffs.json lists the signatures that are accepted, with a reason
// each. A listed key ending in `*` accepts every signature that starts with the rest. The run
// exits with status 1 if it finds a signature outside that list that reproduces as a program of
// its one operation.
//
// Usage:
//   node tests/node-diff/fuzz.mjs [options]
//
// Options:
//   --seed=N               The first program's seed (default: random). Program i uses seed N + i.
//   --programs=N           Programs to run (default: 20)
//   --ops=N                Operations per program (default: 30)
//   --only=NAME[,NAME]     Generators to draw from, as `generator` or `generator/variant`
//   --runtime=PATH         The starlingmonkey binary (default: the more recently built of the
//                          release and the debug build)
//   --node=PATH            The Node.js binary (default: the release scripts/fetch-node-oracle.sh
//                          downloads, the one the Node.js test suite is pinned to, or else node)
//   --timeout=MS           Per-run timeout (default: 20000)
//   -j N                   Programs run at once (default: the number of CPUs)
//   --messages             Also compare error messages
//   --out=DIR              Reports and reproductions (default: target/node-diff)
//   --update-known         Add the signatures found to known-diffs.json
//   --repro=FILE           Run a reproduction from --out in both runtimes and print the traces
//   -v                     Print each difference as it is found

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { GENERATORS, Gen, makeOp } from "./generators.mjs";

const DIFF_DIR = path.dirname(new URL(import.meta.url).pathname);
const REPO_ROOT = path.resolve(DIFF_DIR, "../..");
const PRELUDE = fs.readFileSync(path.join(DIFF_DIR, "prelude.js"), "utf8");
const KNOWN_FILE = path.join(DIFF_DIR, "known-diffs.json");

/** The most recently modified of the files `paths`, or the last one if none exists. */
function newest(paths) {
  const modified = (file) => (fs.existsSync(file) ? fs.statSync(file).mtimeMs : -1);
  return paths.reduce((best, file) => (modified(file) > modified(best) ? file : best), paths[paths.length - 1]);
}

function parseArgs(argv) {
  const release = path.join(REPO_ROOT, "target/release/starlingmonkey");
  const oracle = path.join(REPO_ROOT, "deps/node-bin/bin/node");
  const config = {
    seed: Math.floor(Math.random() * 2 ** 31),
    programs: 20,
    ops: 30,
    only: null,
    runtime: newest([release, path.join(REPO_ROOT, "target/debug/starlingmonkey")]),
    node: fs.existsSync(oracle) ? oracle : "node",
    timeout: 20000,
    jobs: os.cpus().length,
    messages: false,
    out: path.join(REPO_ROOT, "target/node-diff"),
    updateKnown: false,
    repro: null,
    verbose: false,
  };
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    const [name, value] = arg.includes("=") ? [arg.slice(0, arg.indexOf("=")), arg.slice(arg.indexOf("=") + 1)] : [arg, null];
    switch (name) {
      case "--seed":
        config.seed = Number(value);
        break;
      case "--programs":
        config.programs = Number(value);
        break;
      case "--ops":
        config.ops = Number(value);
        break;
      case "--only":
        config.only = value.split(",");
        break;
      case "--runtime":
        config.runtime = path.resolve(value);
        break;
      case "--node":
        config.node = value;
        break;
      case "--timeout":
        config.timeout = Number(value);
        break;
      case "-j":
        config.jobs = Number(argv[++index]);
        break;
      case "--messages":
        config.messages = true;
        break;
      case "--out":
        config.out = path.resolve(value);
        break;
      case "--update-known":
        config.updateKnown = true;
        break;
      case "--repro":
        config.repro = path.resolve(value);
        break;
      case "-v":
        config.verbose = true;
        break;
      case "--help":
        const lines = fs.readFileSync(new URL(import.meta.url), "utf8").split("\n").slice(1);
        console.log(lines.slice(0, lines.findIndex((line) => !line.startsWith("//"))).join("\n"));
        process.exit(0);
        break;
      default:
        console.error(`Unknown option ${arg}`);
        process.exit(2);
    }
  }
  return config;
}

/** The generators and variants `only` selects, as `[generator, variant | null]` pairs. */
function selectGenerators(only) {
  if (!only) return GENERATORS.map((generator) => [generator, null]);
  return only.map((entry) => {
    const [name, variant = null] = entry.split("/");
    const generator = GENERATORS.find((candidate) => candidate.name === name);
    if (!generator || (variant && !generator.variants[variant])) {
      console.error(`Unknown generator ${entry}`);
      process.exit(2);
    }
    return [generator, variant];
  });
}

/** The program running `ops`, each as `{ id, source }`. */
function programSource(ops) {
  const body = ops
    .map((op) => `  await __op(${JSON.stringify(op.id)}, async () => {\n${op.source.replace(/^/gm, "    ")}\n  });`)
    .join("\n");
  return `${PRELUDE}\n(async () => {\n${body}\n  __done();\n})();\n`;
}

/** Remove the directory `dir`, after restoring the permissions operations' `chmod` calls took. */
function removeTree(dir) {
  const restore = (current) => {
    try {
      fs.chmodSync(current, 0o755);
      for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
        if (entry.isDirectory()) restore(path.join(current, entry.name));
      }
    } catch {
      // Nothing to restore.
    }
  };
  restore(dir);
  fs.rmSync(dir, { recursive: true, force: true, maxRetries: 5 });
}

/** Runs `file` with `program`, and resolves with its trace and exit status. */
function run(config, program, args, file, workDir) {
  const fsDir = path.join(workDir, "fs");
  removeTree(fsDir);
  fs.mkdirSync(fsDir, { recursive: true });
  const outFile = path.join(workDir, "stdout");
  const out = fs.openSync(outFile, "w");
  const env = { ...process.env, NODE_DIFF_DIR: fsDir, NODE_DIFF_MESSAGES: config.messages ? "1" : "0" };
  return new Promise((resolve) => {
    const child = spawn(program, [...args, file], { cwd: workDir, env, stdio: ["ignore", out, "pipe"] });
    let stderr = "";
    let timedOut = false;
    child.stderr.on("data", (data) => (stderr += data));
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, config.timeout);
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      fs.closeSync(out);
      const stdout = fs.readFileSync(outFile, "utf8");
      resolve({ ...parseTrace(stdout), code, signal, timedOut, stderr });
    });
  });
}

/** The lines of `stdout` grouped by operation id, and whether the trace ended with `done`. */
function parseTrace(stdout) {
  const groups = new Map();
  let done = false;
  for (const line of stdout.split("\n")) {
    if (line === "done") {
      done = true;
      continue;
    }
    const space = line.indexOf(" ");
    if (space < 0) continue;
    const id = line.slice(0, space);
    if (!groups.has(id)) groups.set(id, []);
    groups.get(id).push(line.slice(space + 1));
  }
  return { groups, done };
}

const nodeRun = (config, file, workDir) => run(config, config.node, ["--no-warnings"], file, workDir);
const starlingRun = (config, file, workDir) => run(config, config.runtime, ["--node-compat", "--node-http-listen"], file, workDir);

/** Runs `ops` in a program in `workDir`, and resolves with the differences found. */
async function compareProgram(config, ops, workDir) {
  fs.mkdirSync(workDir, { recursive: true });
  const file = path.join(workDir, "program.js");
  fs.writeFileSync(file, programSource(ops));
  const first = await nodeRun(config, file, workDir);
  const second = await nodeRun(config, file, workDir);
  const starling = await starlingRun(config, file, workDir);
  const differences = [];
  let nondeterministic = 0;
  let compared = 0;
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  for (const op of ops) {
    const expected = first.groups.get(op.id);
    if (!expected) continue;
    if (!same(expected, second.groups.get(op.id))) {
      nondeterministic++;
      continue;
    }
    const actual = starling.groups.get(op.id);
    // After Node.js crashes, the operations the two runtimes got to before ending can differ.
    if (!actual && !first.done) break;
    compared++;
    if (actual && same(expected, actual)) continue;
    differences.push({ op, expected, actual: actual ?? [], ...classify(expected, actual, starling) });
    // Operations after a crash have no trace. The first missing one is the one reported.
    if (!actual) break;
  }
  // Lines written after the last operation, and a run that doesn't end as Node.js's does.
  const late = (trace) => trace.groups.get("-") ?? [];
  if (differences.length === 0 && first.done && same(late(first), late(second))) {
    const last = ops[ops.length - 1];
    const ended = starling.done && !starling.timedOut && starling.code === first.code;
    if (!ended || !same(late(first), late(starling))) {
      const kind = starling.timedOut ? "hang" : !ended ? "crash" : "log-differs";
      const detail = kind === "crash" ? (starling.signal ?? `exit ${starling.code}`) : "after-last";
      differences.push({ op: last, expected: late(first), actual: late(starling), kind, detail });
    }
  }
  return { differences, nondeterministic, compared, node: first, starling };
}

/** The result line among `lines`: the last one, if it is a result. */
function result(lines) {
  const last = lines?.[lines.length - 1] ?? "";
  if (last.startsWith("ok ")) return { kind: "ok", value: last.slice(3) };
  if (last.startsWith("throws ")) return { kind: "throws", value: last.slice(7) };
  return { kind: "none", value: "" };
}

/** The code, or else the name, of the error a `throws` line serializes. */
function errorKey(value) {
  return /code="([^"]*)"/.exec(value)?.[1] ?? /name=([^\s}]+)/.exec(value)?.[1] ?? "?";
}

/** The type of a serialized value: the text before its first delimiter, such as `Buffer`. */
function valueType(value) {
  if (value.startsWith('"')) return "string";
  if (value.startsWith("[")) return "array";
  if (/^-?[\d.]|^NaN$|^-?Infinity$/.test(value)) return "number";
  return /^[^<{(]*/.exec(value)[0] || "object";
}

/** The kind of difference between `expected` and `actual`, and its signature detail. */
function classify(expected, actual, starling) {
  if (!actual) {
    if (starling.timedOut) return { kind: "hang", detail: "" };
    if (!starling.done) return { kind: "crash", detail: starling.signal ?? `exit ${starling.code}` };
    return { kind: "missing", detail: "" };
  }
  const want = result(expected);
  const got = result(actual);
  if (got.kind === "none") return { kind: starling.timedOut ? "hang" : "crash", detail: starling.signal ?? "" };
  const unimplemented = /code="ERR_METHOD_NOT_IMPLEMENTED" message="The (.*) method is not implemented"/.exec(got.value);
  if (got.kind === "throws" && unimplemented) return { kind: "unimplemented", detail: unimplemented[1] };
  if (want.kind === "ok" && got.kind === "throws") return { kind: "unexpected-throw", detail: errorKey(got.value) };
  if (want.kind === "throws" && got.kind === "ok") return { kind: "missing-throw", detail: errorKey(want.value) };
  if (want.kind === "throws") return { kind: "error-differs", detail: `${errorKey(want.value)}/${errorKey(got.value)}` };
  if (JSON.stringify(expected.slice(0, -1)) !== JSON.stringify(actual.slice(0, -1))) {
    // The detail is the label of the first log line that differs, such as `"statSync"`.
    const index = expected.findIndex((line, at) => line !== actual[at]);
    const line = expected[index] ?? actual[index] ?? "";
    return { kind: "log-differs", detail: /^log ("[^"]*")?/.exec(line)?.[1] ?? "extra lines" };
  }
  return { kind: "value-differs", detail: `${valueType(want.value)}/${valueType(got.value)}` };
}

/** The signature of `difference`. Calls to unimplemented functions share one per function. */
function signature(difference) {
  const { op, kind, detail } = difference;
  if (kind === "unimplemented") return `unimplemented:${detail}`;
  return `${op.generator}/${op.variant}:${kind}${detail ? `[${detail}]` : ""}`;
}

/**
 * The operation up to `difference`'s in its program that crashes or hangs StarlingMonkey when run
 * alone, as an example difference, or `null` if there is none. We try the operations in reverse
 * order, starting with the most recent.
 */
async function findCulprit(config, difference, workDir) {
  const index = difference.program.indexOf(difference.op);
  for (const candidate of difference.program.slice(0, index + 1).reverse()) {
    const op = { ...candidate, id: "op" };
    const alone = await compareProgram(config, [op], workDir);
    const crashed = !alone.starling.done && alone.starling.timedOut === (difference.kind === "hang");
    if (alone.node.done && crashed) {
      return {
        ...difference,
        op: candidate,
        expected: [`(the operation completes and the program exits)`],
        actual: [...(alone.starling.groups.get("op") ?? []), alone.starling.timedOut ? "(timed out)" : `(exit ${alone.starling.code ?? alone.starling.signal})`],
        stderr: alone.starling.stderr,
        alone: true,
        source: programSource([op]),
      };
    }
  }
  return null;
}

/** Runs `tasks` with at most `jobs` at once. */
async function parallel(jobs, tasks) {
  const results = new Array(tasks.length);
  let next = 0;
  async function worker() {
    while (next < tasks.length) {
      const index = next++;
      results[index] = await tasks[index]();
    }
  }
  await Promise.all(Array.from({ length: Math.max(1, jobs) }, worker));
  return results;
}

/** Prints the traces of the reproduction `file` in both runtimes. */
async function repro(config) {
  const workDir = fs.mkdtempSync(path.join(config.out, "repro-run-"));
  for (const [name, runner] of [["node", nodeRun], ["starling", starlingRun]]) {
    const trace = await runner(config, config.repro, workDir);
    console.log(`--- ${name} (exit ${trace.code ?? trace.signal}${trace.timedOut ? ", timed out" : ""})`);
    for (const [id, lines] of trace.groups) for (const line of lines) console.log(`${id} ${line}`);
    if (trace.stderr) console.log(trace.stderr.trimEnd().split("\n").slice(-20).join("\n"));
  }
  fs.rmSync(workDir, { recursive: true, force: true });
}

async function main() {
  const config = parseArgs(process.argv.slice(2));
  fs.mkdirSync(config.out, { recursive: true });
  if (config.repro) return repro(config);

  const selected = selectGenerators(config.only);
  const nodeVersion = await new Promise((resolve) => {
    const child = spawn(config.node, ["--version"]);
    let out = "";
    child.stdout.on("data", (data) => (out += data));
    child.on("close", () => resolve(out.trim()));
  });
  console.log(`Oracle: Node.js ${nodeVersion}; runtime: ${path.relative(REPO_ROOT, config.runtime)}; seed: ${config.seed}`);

  const workRoot = fs.mkdtempSync(path.join(config.out, "work-"));
  const started = Date.now();
  let nondeterministic = 0;
  let compared = 0;
  // The programs Node.js didn't finish, and the operations after the last one it finished, which
  // aren't compared.
  let unfinished = 0;
  let uncompared = 0;
  const bySignature = new Map();

  const programs = await parallel(
    config.jobs,
    Array.from({ length: config.programs }, (_, index) => async () => {
      const seed = config.seed + index;
      const g = new Gen(seed);
      const ops = Array.from({ length: config.ops }, (_, opIndex) => {
        const [generator, variant] = g.pick(selected);
        const op = variant ? { generator: generator.name, variant, source: generator.variants[variant](g) } : makeOp(generator, g);
        return { ...op, id: `${seed}.${opIndex}` };
      });
      const outcome = await compareProgram(config, ops, path.join(workRoot, String(seed)));
      if (!outcome.node.done) {
        const last = [...outcome.node.groups.keys()].pop();
        const skipped = ops.length - (ops.findIndex((op) => op.id === last) + 1);
        unfinished++;
        uncompared += skipped;
        const ending = outcome.node.timedOut
          ? "timed out"
          : `exit ${outcome.node.code ?? outcome.node.signal}: ${outcome.node.stderr.trim().split("\n")[0]}`;
        console.log(`  Node.js didn't finish program ${seed} after operation ${last} (${ending}); ${skipped} operations not compared`);
      }
      nondeterministic += outcome.nondeterministic;
      compared += outcome.compared;
      for (const difference of outcome.differences) {
        difference.program = ops;
        difference.stderr = outcome.starling.stderr;
        if (config.verbose) console.log(`  ${signature(difference)} at ${difference.op.id}`);
      }
      return outcome.differences;
    }),
  );

  for (const difference of programs.flat()) {
    const key = signature(difference);
    if (!bySignature.has(key)) bySignature.set(key, []);
    bySignature.get(key).push(difference);
  }

  // Minimize: rerun differences alone until one reproduces, trying up to three per signature.
  const entries = [...bySignature.entries()];
  await parallel(
    config.jobs,
    entries.map(([key, differences]) => async () => {
      for (const difference of differences.slice(0, 3)) {
        const op = { ...difference.op, id: "op" };
        const workDir = path.join(workRoot, `min-${difference.op.id}`);
        const alone = await compareProgram(config, [op], workDir);
        const found = alone.differences.find((candidate) => signature(candidate) === key);
        if (found) {
          differences.example = { ...found, op: difference.op, stderr: alone.starling.stderr, alone: true, source: programSource([op]) };
          return;
        }
      }
      const first = differences[0];
      differences.example = { ...first, alone: false, source: programSource(first.program) };
      if (first.kind === "crash" || first.kind === "hang") {
        const culprit = await findCulprit(config, first, path.join(workRoot, `culprit-${first.op.id}`));
        if (culprit) differences.example = culprit;
      }
    }),
  );

  // A crash's culprit can be another generator's operation, which changes the signature.
  const merged = new Map();
  for (const [, differences] of entries) {
    const key = signature(differences.example);
    if (merged.has(key)) merged.get(key).push(...differences);
    else merged.set(key, Object.assign([...differences], { example: differences.example }));
  }
  entries.splice(0, entries.length, ...merged);

  const known = fs.existsSync(KNOWN_FILE) ? JSON.parse(fs.readFileSync(KNOWN_FILE, "utf8")) : {};
  const severity = (key) => (/:(crash|hang)/.test(key) ? 0 : 1);
  entries.sort(([a, x], [b, y]) => severity(a) - severity(b) || y.length - x.length || a.localeCompare(b, 'en'));

  const reproDir = path.join(config.out, "repro");
  fs.rmSync(reproDir, { recursive: true, force: true });
  fs.mkdirSync(reproDir, { recursive: true });
  const report = [`# Differential fuzzing report\n`, `Oracle: Node.js ${nodeVersion}. Seed ${config.seed}, ${config.programs} programs of ${config.ops} operations.\n`];
  const fresh = [];
  for (const [key, differences] of entries) {
    const example = differences.example;
    const reproFile = path.join(reproDir, `${key.replace(/[^A-Za-z0-9_.-]+/g, "_")}.js`);
    fs.writeFileSync(reproFile, example.source);
    const knownKey = Object.hasOwn(known, key)
      ? key
      : Object.keys(known).find((pattern) => pattern.endsWith("*") && key.startsWith(pattern.slice(0, -1)));
    const isKnown = knownKey !== undefined;
    // A difference that needs the whole program comes from work an earlier operation left
    // running, so it is reported but neither fails the run nor gets recorded.
    if (!isKnown && example.alone) fresh.push(key);
    const status = isKnown ? `known: ${known[knownKey] || "no reason given"}` : "new";
    console.log(`${isKnown ? " " : "!"} ${String(differences.length).padStart(4)} ${key}${example.alone ? "" : " (interaction)"}`);
    report.push(
      `## ${key}\n`,
      `${differences.length} occurrence(s), ${status}. Reproduction: \`${path.relative(REPO_ROOT, reproFile)}\`` +
        `${example.alone ? "" : " (needs the whole program)"}\n`,
      "```js",
      example.op.source,
      "```",
      "",
      "```diff",
      ...example.expected.map((line) => `- ${line}`),
      ...example.actual.map((line) => `+ ${line}`),
      "```\n",
    );
    if (example.stderr && /crash|hang/.test(example.kind)) {
      report.push("```", example.stderr.trimEnd().split("\n").slice(-15).join("\n"), "```\n");
    }
  }
  const reportFile = path.join(config.out, "report.md");
  fs.writeFileSync(reportFile, report.join("\n"));

  if (config.updateKnown) {
    for (const key of fresh) known[key] = "";
    const sorted = Object.fromEntries(Object.entries(known).sort(([a], [b]) => a.localeCompare(b, 'en')));
    fs.writeFileSync(KNOWN_FILE, JSON.stringify(sorted, null, 2) + "\n");
  }
  removeTree(workRoot);

  const seconds = ((Date.now() - started) / 1000).toFixed(1);
  console.log(
    `\n${compared} operations compared, ${nondeterministic} nondeterministic in Node.js; ` +
      `${entries.length} signatures, ${fresh.length} new (${seconds}s). Report: ${path.relative(REPO_ROOT, reportFile)}`,
  );
  if (unfinished > 0) {
    console.log(`Node.js didn't finish ${unfinished} programs; ${uncompared} of their operations weren't compared.`);
  }
  if (fresh.length > 0 && !config.updateKnown) process.exit(1);
}

main();
