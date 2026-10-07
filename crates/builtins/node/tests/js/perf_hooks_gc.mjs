// The `gc` entries of `node:perf_hooks`, with `--expose-gc`.

import { constants, PerformanceObserver } from "node:perf_hooks";
import { eq } from "./harness.mjs";

const kinds = [
  constants.NODE_PERFORMANCE_GC_MAJOR,
  constants.NODE_PERFORMANCE_GC_MINOR,
  constants.NODE_PERFORMANCE_GC_INCREMENTAL,
];
const entries = await new Promise((resolve) => {
  const observer = new PerformanceObserver((list) => {
    observer.disconnect();
    resolve(list.getEntries());
  });
  observer.observe({ entryTypes: ["gc"] });
  globalThis.gc();
  // The entries are delivered by an unreferenced timer, which needs the loop to keep running.
  setTimeout(() => {}, 100);
});
const forced = entries.find((entry) => entry.detail.kind === constants.NODE_PERFORMANCE_GC_MAJOR);
eq(forced !== undefined, true, "gc() is a major collection");
eq(forced.name, "gc", "name");
eq(forced.entryType, "gc", "entryType");
eq(forced.detail.flags, constants.NODE_PERFORMANCE_GC_FLAGS_FORCED, "gc() is forced");
eq(forced.flags, forced.detail.flags, "the deprecated flags accessor");
eq(entries.every((entry) => kinds.includes(entry.kind)), true, "the kinds");
eq(forced.startTime > 0 && forced.startTime < 1e5 && forced.duration >= 0, true, "the timing");
