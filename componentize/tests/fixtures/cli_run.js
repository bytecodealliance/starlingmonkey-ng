// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A CLI application for the cli/run-via-pipeline e2e.
//
// `wasi:cli/run.run()` forwards to the exported `run` function. It runs
// some top-level-independent work, awaits a real `setTimeout` (so the runtime's
// per-call event loop must take a timer turn before the returned promise
// settles, proving the async drive, exactly as the async-export suite does),
// writes its lines to stdout, and resolves. A componentized snapshot of this app
// run as a CLI tool prints the start and done lines in order and exits 0.
//
// The same source runs identically when the runtime reads it from a script
// path, evaluates it, and then resolves and drives `run`.

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
  // Await a real timer whose callback allocates short-lived garbage while the
  // runtime is driving the event loop, meaning while the returned promise is held
  // across the suspension. That is exactly the window the promise's root must
  // survive: the runtime keeps it on a GC-traced call stack (not a bare,
  // untraced `Heap`), so a collection triggered by this garbage finds it. A
  // missing root would surface as a stale-pointer crash. (In release the GC is
  // best-effort, so this strengthens but does not guarantee the check, and the
  // rooting matches `export_call_async`'s proven path.)
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
