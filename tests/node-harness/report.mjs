#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Writes a Markdown summary of Node.js test-suite results: overall counts per target, counts and
// the largest blockers per area, the blockers by failure signature with example tests, the skip
// reasons, the tests that pass natively but not on wasm, and the tests that other runtimes pass
// and the native target fails.
//
// Usage:
//   node tests/node-harness/report.mjs --native=FILE [--wasm=FILE] [--compare=NAME=FILE...]
//                                      [--out=FILE]
//
// The input files are what `run-node-tests.mjs --json=FILE` writes. Each `--compare` names
// another runtime's run, such as `--compare=Deno=deno.json`. Without --out, the report goes
// to standard output.

import { readFileSync, writeFileSync } from "node:fs";

/**
 * The area of each test, by the start of its file name after `test-`. The first matching entry
 * wins. Tests that match no entry count as `other`.
 */
const AREAS = [
  [/^(async-hooks|async-wrap|async-local-storage|asyncresource|als-)/, "async_hooks"],
  [/^(socketaddress|blocklist|tcp-)/, "net"],
  [/^diagnostics?-channel/, "diagnostics_channel"],
  [/^(event-emitter|events-|eventemitter)/, "events"],
  [/^(eventtarget|nodeeventtarget|abortcontroller|abortsignal|aborted|abort-controller|event-target)/, "events"],
  [/^string-decoder/, "string_decoder"],
  [/^(stream|readable|duplex|stream2|stream3)-/, "stream"],
  [/^(webstream|webstreams|streams|whatwg-readablebytestream|whatwg-webstreams)/, "webstreams"],
  [/^(whatwg-url|url|fileurltopathbuffer|urlpattern)/, "url"],
  [/^(whatwg-encoding|whatwg-events|whatwg|domexception|btoa|blob|file\.|structuredClone|messag|broadcastchannel|eventsource|websocket|webstorage|navigator|fetch|global|web-|webapi|mime|compression)/, "web APIs"],
  [/^(process|next-tick|tick|beforeexit|uncaught|unhandled|exception|cwd|env|kill|setproctitle|signal|sigint|exit|memory-usage)/, "process"],
  [/^(timers|microtask|queue-microtask|promise)/, "timers and promises"],
  [/^(module|require|esm|preload|package|vfs|directory-import)/, "module"],
  [/^(stdin|stdout|stdio|tty|ttywrap|pipe)/, "tty and stdio"],
  [/^(http-|outgoing|listen)/, "http"],
  [/^(http2|h2)/, "http2"],
  [/^https/, "https"],
  [/^(tls|crypto|webcrypto|x509|dsa|openssl)/, "crypto and tls"],
  [/^(child-process|spawn)/, "child_process"],
  [/^worker/, "worker_threads"],
  [/^cluster/, "cluster"],
  [/^(performance|perf-|performanceobserver)/, "perf_hooks"],
  [/^(error|errors|err-|throw|stack|source-map|warn)/, "errors"],
  [/^(fs|file-|filehandle|fastutf8stream)/, "fs"],
  [/^(buffer|stringbytes|utf8)/, "buffer"],
  [/^(util|inspect)/, "util"],
  [/^assert/, "assert"],
  [/^console/, "console"],
  [/^dgram/, "dgram"],
  [/^dns/, "dns"],
  [/^domain/, "domain"],
  [/^net-/, "net"],
  [/^os/, "os"],
  [/^path/, "path"],
  [/^permission/, "permission"],
  [/^punycode/, "punycode"],
  [/^querystring/, "querystring"],
  [/^readline/, "readline"],
  [/^repl/, "repl"],
  [/^(runner|mock)/, "test"],
  [/^sqlite/, "sqlite"],
  [/^(v8|gc|finalization|weakref|hash-seed)/, "v8"],
  [/^vm/, "vm"],
  [/^zlib/, "zlib"],
  [/^(icu|intl|tz|datetime|unicode)/, "intl"],
];

/**
 * The known blockers: structural reasons tests fail or skip, with the failure signatures and skip
 * reasons they show up as.
 */
const KNOWN_BLOCKERS = [
  {
    title: "The HTTP client",
    match: (entry) => entry.area === "http" && entry.status === "FAIL",
    text: "Tests run with `--node-http-listen`, so their servers accept connections, and most remaining HTTP failures are on the client side. The client is built on reqwest, which adds `accept: */*`, lowercases header names, normalizes paths, strips hop-by-hop headers and reports the standard reason phrase. 1xx responses, `Agent` socket pools and `req.socket` are not exposed.",
  },
  {
    title: "Child processes, workers, cluster and the inspector",
    match: (entry) => /child processes|worker threads|cluster|inspector/.test(entry.reason ?? ""),
    text: "WASI has no processes or threads, so these tests are skipped without running.",
  },
  {
    title: "Unsupported runtime flags",
    match: (entry) => /unsupported runtime flag/.test(entry.reason ?? ""),
    text: "Tests that need a runtime flag the runtime rejects, such as `--allow-natives-syntax` or the permission model's flags, are skipped without running. The runtime gets the other flags with `--allow-node-runtime-flags`.",
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
  const config = { native: null, wasm: null, out: null, compare: [] };
  for (const arg of argv) {
    const [name, value] = arg.split(/=(.*)/s);
    if (name === "--native") config.native = value;
    else if (name === "--wasm") config.wasm = value;
    else if (name === "--out") config.out = value;
    else if (name === "--compare") {
      const [runtime, file] = value.split(/=(.*)/s);
      config.compare.push({ name: runtime, results: JSON.parse(readFileSync(file, "utf8")) });
    }
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
  const name = file.replace(/^test-/, "");
  return AREAS.find(([pattern]) => pattern.test(name))?.[1] ?? "other";
}

/** What a failure signature means. */
function describe(signature) {
  const [kind, detail] = signature.split(/:(.*)/s);
  switch (kind) {
    case "http-command-mode":
      return "`http.Server.listen()` in a script run as a command without `--node-http-listen`.";
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
  const sorted = [...signatures].sort(([a, x], [b, y]) => y.length - x.length || a.localeCompare(b, 'en'));
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
      "with `tests/node-harness/run-node-tests.mjs`. `tests/node-harness/tests.txt` selects every test " +
      "except those of Node's build tooling, V8's heap and trace tooling, the inspector and QUIC. " +
      "A test passes if it exits with status 0. Tests that need runtime " +
      "flags the runtime rejects, child processes, workers, `cluster` or the inspector are skipped " +
      "without running, and tests that call `common.skip()` count as skipped.",
    "",
    `Generated by \`tests/node-harness/report.mjs\` from runs on ${native.date.slice(0, 10)}.`,
    "",
  );

  lines.push("## Summary", "");
  const summaryRows = [["native", nativeTally.counts]];
  if (wasm) summaryRows.push(["wasm", tally(Object.entries(wasm.tests)).counts]);
  for (const other of config.compare) summaryRows.push([other.name, tally(Object.entries(other.results.tests)).counts]);
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
        const hits = nativeEntries.filter(([file, entry]) => blocker.match({ ...entry, area: areaOf(file) }));
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
    .sort(([a], [b]) => a.localeCompare(b, 'en'))
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
      [...reasons].sort(([a, x], [b, y]) => y - x || a.localeCompare(b, 'en')).map(([reason, count]) => [reason, count]),
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

  if (config.compare.length > 0) {
    const names = config.compare.map((other) => other.name);
    lines.push(`## Passing in ${names.join(" or ")}`, "");
    lines.push(
      `The tests that fail natively and pass in ${names.join(" or ")}, by area. ` +
        `These runtimes run the tests without their runtime flags. Each test lists the runtimes that pass it.`,
      "",
    );
    const passingElsewhere = new Map();
    for (const [file, entry] of nativeEntries) {
      if (entry.status !== "FAIL") continue;
      const passing = config.compare.filter((other) => other.results.tests[file]?.status === "PASS");
      if (passing.length === 0) continue;
      const area = areaOf(file);
      if (!passingElsewhere.has(area)) passingElsewhere.set(area, []);
      passingElsewhere.get(area).push([file, entry, passing.map((other) => other.name)]);
    }
    const sortedAreas = [...passingElsewhere].sort(([a, x], [b, y]) => y.length - x.length || a.localeCompare(b, 'en'));
    lines.push(
      table(
        ["Area", ...names.map((name) => `Passing in ${name}`), "Passing in any"],
        sortedAreas.map(([area, tests]) => [
          `\`${area}\``,
          ...names.map((name) => tests.filter(([, , passing]) => passing.includes(name)).length),
          tests.length,
        ]),
      ),
      "",
    );
    for (const [area, tests] of sortedAreas) {
      lines.push(`### \`${area}\``, "");
      for (const [file, entry, passing] of tests) {
        lines.push(`- \`${file}\` (${passing.join(", ")}): ${entry.signature}`);
      }
      lines.push("");
    }
  }

  const markdown = lines.join("\n");
  if (config.out) writeFileSync(config.out, markdown);
  else process.stdout.write(markdown);
}

main();
