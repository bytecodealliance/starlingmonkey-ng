#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Writes a Markdown summary of Node.js test-suite results: overall counts per target, counts and
// the largest blockers per area, the blockers by failure signature with example tests, the skip
// reasons, and the tests that pass natively but not on wasm.
//
// Usage:
//   node tests/node-harness/report.mjs --native=FILE [--wasm=FILE] [--out=FILE]
//
// The input files are what `run-node-tests.mjs --json=FILE` writes. Without --out, the report goes
// to standard output.

import { readFileSync, writeFileSync } from "node:fs";

/** The area of each test, by the start of its file name, longest prefix first. */
const AREAS = [
  ["test-async-local-storage-", "async_hooks"],
  ["test-diagnostics-channel-", "diagnostics_channel"],
  ["test-event-emitter-", "events"],
  ["test-string-decoder", "string_decoder"],
  ["test-dns-lookup", "dns"],
  ["test-querystring", "querystring"],
  ["test-punycode", "punycode"],
  ["test-readline-", "readline"],
  ["test-stream2-", "stream"],
  ["test-stream3-", "stream"],
  ["test-stream-", "stream"],
  ["test-process-", "process"],
  ["test-events-", "events"],
  ["test-assert", "assert"],
  ["test-buffer-", "buffer"],
  ["test-timers", "timers"],
  ["test-http-", "http"],
  ["test-path-", "path"],
  ["test-util-", "util"],
  ["test-zlib-", "zlib"],
  ["test-url-", "url"],
  ["test-net-", "net"],
  ["test-fs-", "fs"],
  ["test-os", "os"],
];

/**
 * The known blockers: structural reasons tests fail or skip, with the failure signatures and skip
 * reasons they show up as.
 */
const KNOWN_BLOCKERS = [
  {
    title: "HTTP servers in scripts run as commands",
    match: (entry) => entry.signature === "http-command-mode",
    text: "Node's HTTP tests start a server and request it from the same script. `listen()` serves requests only in serve mode (`--serve`) and in `wasi:http` components, where requests arrive through the host, so in a command it emits `ERR_SERVE_MODE_REQUIRED`. A real listener in command mode is the largest remaining lever.",
  },
  {
    title: "Child processes, workers, cluster and the inspector",
    match: (entry) => /child processes|worker threads|cluster|inspector/.test(entry.reason ?? ""),
    text: "WASI has no processes or threads, so these tests are skipped without running.",
  },
  {
    title: "Runtime flags and Node.js internals",
    match: (entry) => /runtime flags|Node\.js internals/.test(entry.reason ?? ""),
    text: "Tests that need V8 or Node flags, or `require('internal/...')`, are skipped without running.",
  },
  {
    title: "Unimplemented APIs",
    match: (entry) => (entry.signature ?? "").startsWith("unimplemented:"),
    text: "Stubs that throw `ERR_METHOD_NOT_IMPLEMENTED`, such as brotli and zstd compression, crypto keys and ciphers (there is no OpenSSL), `net` IPC paths and `dgram`. The blockers list below names each API.",
  },
  {
    title: "Engine differences",
    match: (entry) => /SharedArrayBuffer|vm\.|getHeapStatistics|v8\./.test(entry.excerpt ?? ""),
    text: "SpiderMonkey has no `SharedArrayBuffer` global here, `vm` and `v8` are stubs, and stack traces have SpiderMonkey's frames.",
  },
  {
    title: "Timing and resource exhaustion",
    match: (entry) => entry.signature === "timeout" || entry.signature === "crash",
    text: "Tests that time out waiting for events that never come, and crashes. Each needs a look of its own.",
  },
];

function parseArgs(argv) {
  const config = { native: null, wasm: null, out: null };
  for (const arg of argv) {
    const [name, value] = arg.split(/=(.*)/s);
    if (name === "--native") config.native = value;
    else if (name === "--wasm") config.wasm = value;
    else if (name === "--out") config.out = value;
    else {
      console.error(`Unknown argument ${arg}`);
      process.exit(2);
    }
  }
  if (!config.native) {
    console.error("--native=FILE is required");
    process.exit(2);
  }
  return config;
}

function areaOf(file) {
  return AREAS.find(([prefix]) => file.startsWith(prefix))?.[1] ?? "other";
}

/** What a failure signature means. */
function describe(signature) {
  const [kind, detail] = signature.split(/:(.*)/s);
  switch (kind) {
    case "http-command-mode":
      return "`http.Server.listen()` in a script run as a command. The tests request their own server, which needs a real listener in command mode; only serve mode and `wasi:http` components serve requests.";
    case "unimplemented":
      return `\`${detail}\` is not implemented.`;
    case "timeout":
      return "The test didn't finish in time, typically waiting for an event or callback that never comes.";
    case "crash":
      return "The runtime crashed (a panic or an engine assertion).";
    case "assertion":
      return "An assertion failed: a value, error, message or call count differs from Node.js's.";
    case "not-a-function":
      return `\`${detail}\` is missing or not a function.`;
    case "error":
      return `An uncaught \`${detail}\`.`;
    default:
      return "Another failure; see the example tests.";
  }
}

/** Counts of each status, and the failure signatures, in `entries`. */
function tally(entries) {
  const counts = { PASS: 0, FAIL: 0, SKIP: 0 };
  const signatures = new Map();
  for (const [file, entry] of entries) {
    counts[entry.status]++;
    if (entry.status === "FAIL") {
      if (!signatures.has(entry.signature)) signatures.set(entry.signature, []);
      signatures.get(entry.signature).push(file);
    }
  }
  const sorted = [...signatures].sort(([a, x], [b, y]) => y.length - x.length || a.localeCompare(b));
  return { counts, signatures: sorted };
}

/** The share of the run (not skipped) tests that pass, as a percentage. */
function passRate({ PASS, FAIL }) {
  return PASS + FAIL === 0 ? "n/a" : `${((100 * PASS) / (PASS + FAIL)).toFixed(1)}%`;
}

/** A Markdown table of `rows` under `header`, with numeric columns aligned right. */
function table(header, rows) {
  const numeric = header.map((_, index) => rows.length > 0 && rows.every((row) => typeof row[index] === "number"));
  return [
    `| ${header.join(" | ")} |`,
    `| ${numeric.map((right) => (right ? "---:" : "---")).join(" | ")} |`,
    ...rows.map((row) => `| ${row.join(" | ")} |`),
  ].join("\n");
}

function main() {
  const config = parseArgs(process.argv.slice(2));
  const native = JSON.parse(readFileSync(config.native, "utf8"));
  const wasm = config.wasm ? JSON.parse(readFileSync(config.wasm, "utf8")) : null;
  const nativeEntries = Object.entries(native.tests);
  const nativeTally = tally(nativeEntries);
  const lines = [];

  lines.push("# Node.js test-suite results", "");
  lines.push(
    "Results of running Node.js v24.21.0's own tests from `test/parallel` against `--node-compat`, " +
      "with `tests/node-harness/run-node-tests.mjs`. `tests/node-harness/tests.txt` selects the tests " +
      "of the implemented modules. A test passes if it exits with status 0. Tests that need runtime " +
      "flags, child processes, workers, `cluster`, the inspector or Node.js internals are skipped " +
      "without running, and tests that call `common.skip()` count as skipped.",
    "",
    `Generated by \`tests/node-harness/report.mjs\` from runs on ${native.date.slice(0, 10)}.`,
    "",
  );

  lines.push("## Summary", "");
  const summaryRows = [["native", nativeTally.counts]];
  if (wasm) summaryRows.push(["wasm", tally(Object.entries(wasm.tests)).counts]);
  lines.push(
    table(
      ["Target", "Passed", "Failed", "Skipped", "Total", "Pass rate of run tests"],
      summaryRows.map(([target, counts]) => [
        target,
        counts.PASS,
        counts.FAIL,
        counts.SKIP,
        counts.PASS + counts.FAIL + counts.SKIP,
        passRate(counts),
      ]),
    ),
    "",
  );
  lines.push("The largest blockers, natively:", "");
  for (const [signature, files] of nativeTally.signatures.slice(0, 8)) {
    lines.push(`- **${signature}** (${files.length} tests): ${describe(signature)}`);
  }
  lines.push("");

  lines.push("## Known blockers", "");
  lines.push(
    table(
      ["Blocker", "Failing", "Skipped", "What it is"],
      KNOWN_BLOCKERS.map((blocker) => {
        const hits = nativeEntries.filter(([, entry]) => blocker.match(entry));
        const failing = hits.filter(([, entry]) => entry.status === "FAIL").length;
        const skipped = hits.filter(([, entry]) => entry.status === "SKIP").length;
        return [blocker.title, failing, skipped, blocker.text];
      }),
    ),
    "",
  );

  lines.push("## By area", "");
  const areas = new Map();
  for (const entry of nativeEntries) {
    const area = areaOf(entry[0]);
    if (!areas.has(area)) areas.set(area, []);
    areas.get(area).push(entry);
  }
  const areaRows = [...areas]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([area, entries]) => {
      const { counts, signatures } = tally(entries);
      const blockers = signatures
        .slice(0, 3)
        .map(([signature, files]) => `${signature} (${files.length})`)
        .join(", ");
      return [`\`${area}\``, counts.PASS, counts.FAIL, counts.SKIP, passRate(counts), blockers || "none"];
    });
  lines.push(table(["Area", "Passed", "Failed", "Skipped", "Pass rate", "Largest blockers"], areaRows), "");

  lines.push("## Blockers", "");
  lines.push(
    "Native failures grouped by signature: the unimplemented API, the error, or the kind of " +
      "failure the test's output shows first. Assertion failures are the long tail of behavioral " +
      "differences, which the differential fuzzer (`tests/node-diff`) narrows down module by module.",
    "",
  );
  for (const [signature, files] of nativeTally.signatures) {
    const examples = files.slice(0, 5).map((file) => `\`${file}\``).join(", ");
    const more = files.length > 5 ? `, and ${files.length - 5} more` : "";
    lines.push(`- **${signature}** (${files.length}): ${describe(signature)} ${examples}${more}.`);
  }
  lines.push("");

  lines.push("## Skips", "");
  const reasons = new Map();
  for (const [, entry] of nativeEntries) {
    if (entry.status !== "SKIP") continue;
    const reason = entry.reason.replace(/\s+/g, " ").slice(0, 120);
    reasons.set(reason, (reasons.get(reason) ?? 0) + 1);
  }
  lines.push(
    table(
      ["Reason", "Tests"],
      [...reasons].sort(([a, x], [b, y]) => y - x || a.localeCompare(b)).map(([reason, count]) => [reason, count]),
    ),
    "",
  );

  if (wasm) {
    lines.push("## Differences on wasm", "");
    const regressed = nativeEntries
      .filter(([file, entry]) => entry.status === "PASS" && wasm.tests[file]?.status === "FAIL")
      .map(([file]) => [file, wasm.tests[file].signature]);
    const wasmTally = tally(regressed.map(([file]) => [file, wasm.tests[file]]));
    lines.push(
      `${regressed.length} tests pass natively but fail on wasm. By signature:`,
      "",
      ...wasmTally.signatures.map(
        ([signature, files]) =>
          `- **${signature}** (${files.length}): ${describe(signature)} ${files
            .slice(0, 5)
            .map((file) => `\`${file}\``)
            .join(", ")}${files.length > 5 ? `, and ${files.length - 5} more` : ""}.`,
      ),
      "",
    );
  }

  const markdown = lines.join("\n");
  if (config.out) writeFileSync(config.out, markdown);
  else process.stdout.write(markdown);
}

main();
