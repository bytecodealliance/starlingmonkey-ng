// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A CLI application for the cli/run-via-pipeline e2e.
//
// `wasi:cli/run.run()` forwards to the exported `run` function. It awaits a
// `setTimeout`, so the event loop must take a timer turn before the returned
// promise settles, writes its lines to stdout, and resolves. A componentized snapshot of this app
// run as a CLI tool prints the start and done lines in order and exits 0.

// Written under Wizer, so libc's stdout stream is opened before the snapshot.
// `run` writes to stdout again in the resumed instance, which works only if the
// snapshot holds no stale stream handle.
console.log("logged under Wizer");

export async function run() {
  console.log("cli run start");
  // Measured against a time origin recorded before the snapshot. The resume fixups
  // advance the clock past everything the snapshot holds, so this is the time since the
  // resumed instance started rather than the seconds `init` spent under Wizer.
  console.log(`cli run elapsed ${performance.now()}`);
  // Await a timer whose callback allocates garbage while the runtime holds the
  // returned promise across the suspension, so a collection can run while the
  // promise needs its root. A missing root surfaces as a stale-pointer crash.
  await new Promise((resolve) =>
    setTimeout(() => {
      for (let i = 0; i < 200000; i++) {
        const junk = { a: i, b: `chunk-${i}`, c: [i, i + 1, i + 2] };
        if (junk.a < 0) console.log(junk.b);
      }
      resolve(undefined);
    }, 10),
  );
  console.log("cli run done");
}
