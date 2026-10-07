// `node:perf_hooks`: the performance timeline shared with the global `performance`, observers,
// histograms, `timerify`, `eventLoopUtilization` and `monitorEventLoopDelay`. The expected
// histogram statistics are Node.js v24's.

import perfHooks, {
  createHistogram,
  monitorEventLoopDelay,
  performance,
  PerformanceEntry,
  PerformanceMark,
  PerformanceObserver,
  timerify,
} from "node:perf_hooks";
import { inspect } from "node:util";
import { deepEq, eq, throws } from "./harness.mjs";

eq(performance, globalThis.performance, "one performance object");
eq(PerformanceObserver, globalThis.PerformanceObserver, "the observer class is the global");
eq(perfHooks.eventLoopUtilization, performance.eventLoopUtilization, "shared eventLoopUtilization");
eq(perfHooks.timerify, performance.timerify, "shared timerify");
eq(perfHooks.constants.NODE_PERFORMANCE_GC_MAJOR, 4, "constants");
deepEq(
  PerformanceObserver.supportedEntryTypes,
  ["dns", "function", "gc", "http", "http2", "mark", "measure", "net", "quic", "resource"],
  "Node.js's entry types",
);

// Before the event loop's first step, the utilization is all zeros, and loopStart is -1.
deepEq(performance.eventLoopUtilization(), { idle: 0, active: 0, utilization: 0 }, "elu before the loop");
eq(performance.nodeTiming.loopStart, -1, "loopStart before the loop");
const timing = performance.nodeTiming;
for (const name of ["name", "entryType", "startTime"]) {
  const descriptor = Object.getOwnPropertyDescriptor(timing, name);
  deepEq([descriptor.writable, descriptor.enumerable, descriptor.configurable], [false, true, true], `nodeTiming.${name} is read-only`);
}
eq(timing.nodeStart <= timing.environment && timing.environment <= timing.bootstrapComplete, true, "milestone order");
deepEq(Object.keys(performance.toJSON()), ["nodeTiming", "timeOrigin", "eventLoopUtilization"], "toJSON");

// Marks and measures, delivered to observers in one batch after the current task.
const delivered = [];
const observer = new PerformanceObserver((list, self) => {
  delivered.push(...list.getEntries().map((entry) => `${entry.entryType}:${entry.name}`));
  eq(self, observer, "the observer is the callback's second argument");
  self.disconnect();
});
observer.observe({ entryTypes: ["mark", "measure"] });
const mark = performance.mark("a", { startTime: 1, detail: { x: 1 } });
eq(mark instanceof PerformanceMark && mark instanceof PerformanceEntry, true, "marks are entries");
deepEq(mark.toJSON(), { name: "a", entryType: "mark", startTime: 1, duration: 0, detail: { x: 1 } }, "mark");
performance.mark("b", { startTime: 1e7 });
const measure = performance.measure("a to b", "a", "b");
deepEq([measure.startTime, measure.duration], [1, 1e7 - 1], "measure between marks");
eq(performance.measure("detail only", { detail: 1 }).startTime, 0, "measure options without start or end");
eq(performance.measure("from start", "nodeStart").startTime, timing.nodeStart, "milestone names");
throws(() => performance.measure("x", "missing"), DOMException, "an unknown mark");
eq(delivered.length, 0, "delivery waits for a task");
deepEq(performance.getEntriesByType("mark").map((entry) => entry.name), ["a", "b"], "the timeline");
performance.clearMarks("a");
deepEq(performance.getEntriesByType("mark").map((entry) => entry.name), ["b"], "clearMarks");
await new Promise((resolve) => setImmediate(resolve));
// `nodeStart` can fall before mark `a`'s start time of 1 when the runtime starts within 1 ms.
const sorted = timing.nodeStart < 1
  ? ["measure:detail only", "measure:from start", "mark:a", "measure:a to b", "mark:b"]
  : ["measure:detail only", "mark:a", "measure:a to b", "measure:from start", "mark:b"];
deepEq(delivered, sorted, "entries sorted by start time");
eq(inspect(measure).startsWith("PerformanceMeasure {"), true, "entries inspect as Node.js's");
performance.clearMarks();
performance.clearMeasures();

// Histograms.
const empty = createHistogram();
eq(empty.min, 9223372036854776000, "an empty histogram's min");
eq(empty.minBigInt, 9223372036854775807n, "minBigInt");
eq(Number.isNaN(empty.mean) && Number.isNaN(empty.stddev), true, "an empty histogram's mean");
deepEq([...empty.percentiles], [[100, 0]], "an empty histogram's percentiles");
throws(() => new empty.constructor(), "ERR_ILLEGAL_CONSTRUCTOR", "histograms can't be constructed");
throws(() => empty.record(0), "ERR_OUT_OF_RANGE", "record validates");
throws(() => empty.record("1"), "ERR_INVALID_ARG_TYPE", "record validates its type");
throws(() => createHistogram({ figures: 6 }), "ERR_OUT_OF_RANGE", "createHistogram validates");

const one = createHistogram();
one.record(1);
deepEq([...one.percentiles], [[0, 1], [100, 1]], "one value");

let seed = 42;
const random = () => {
  seed = (seed * 1103515245 + 12345) % 2147483648;
  return seed / 2147483648;
};
const h = createHistogram();
for (let i = 0; i < 5000; i++) h.record(Math.ceil(random() * random() * 5e6) + 1);
deepEq([h.count, h.min, h.max, h.mean], [5000, 42, 4890623, 1226304.1836], "statistics");
deepEq([h.percentile(1), h.percentile(50), h.percentile(99.9), h.percentile(100)],
  [7583, 888319, 4694015, 4890623], "percentiles round as HdrHistogram_c does");
eq(h.percentileBigInt(50), 888319n, "percentileBigInt");
deepEq([...h.percentiles].slice(0, 4), [[0, 42], [50, 887808], [75, 1876992], [87.5, 2713600]],
  "the percentiles map holds the lowest value of each bucket");
deepEq([...h.percentilesAt([99, 50, 1])], [[1, 7583], [50, 888319], [99, 4313087]], "percentilesAt");
deepEq([h.cdf(1e6), h.countAt(1e6)], [0.5404, 0], "cdf and countAt");
deepEq([...h.linearBuckets(1000000)].slice(0, 2), [[999936, 2702], [1999872, 1157]], "linearBuckets");
deepEq(h.percentileCI(90), { value: 2928639, lower: 2850815, upper: 3002367 }, "percentileCI");
const sum = createHistogram();
sum.add(h);
sum.add(h);
eq(sum.count, 10000, "add");
sum.subtract(h);
deepEq([sum.count, sum.max], [5000, 4890623], "subtract");
const small = createHistogram({ lowest: 1, highest: 100, figures: 1 });
small.record(1000);
small.record(10n);
deepEq([small.count, small.exceeds], [1, 1], "values out of range count as exceeding");
const ewma = createHistogram({ halfLife: 2, threshold: 5 });
for (const value of [1, 10, 1, 10]) ewma.record(value);
eq(ewma.ewmaErrorRate > 0 && ewma.ewmaErrorRate < 1, true, "the moving error rate");
deepEq(Object.keys(h.toJSON()),
  ["count", "min", "max", "mean", "exceeds", "stddev", "skewness", "kurtosis", "ewmaMean",
    "ewmaStddev", "ewmaErrorRate", "percentiles"], "toJSON");
eq(inspect(one).startsWith("Histogram {"), true, "histograms inspect as Node.js's");

// timerify queues a function entry and records into a histogram.
const functions = [];
const functionObserver = new PerformanceObserver((list) => {
  functions.push(...list.getEntries());
  functionObserver.disconnect();
});
functionObserver.observe({ type: "function" });
const durations = createHistogram();
function add(a, b) {
  return a + b;
}
const timed = timerify(add, { histogram: durations });
deepEq([timed.name, timed.length, timed(1, 2)], ["timerified add", 2, 3], "a timerified function");
class Point {
  constructor(x) {
    this.x = x;
  }
}
eq(new (timerify(Point))(4).x, 4, "timerified constructors");
throws(() => timerify(add, { histogram: {} }), "ERR_INVALID_ARG_TYPE", "the histogram option");
await new Promise((resolve) => setImmediate(resolve));
deepEq(functions.map((entry) => [entry.name, entry.entryType, entry[0], entry.detail.length]),
  [["add", "function", 1, 2], ["Point", "function", 4, 1]], "the function entries");
eq(durations.count, 1, "the duration is recorded");

// The event loop's utilization and delay.
const delay = monitorEventLoopDelay({ resolution: 5 });
eq(delay.enable(), true, "enable");
eq(delay.enable(), false, "enable again");
const before = performance.eventLoopUtilization();
await new Promise((resolve) => setTimeout(resolve, 60));
const busyUntil = Date.now() + 30;
while (Date.now() < busyUntil);
await new Promise((resolve) => setTimeout(resolve, 30));
eq(delay.count > 0 && delay.min > 0, true, "the delay is sampled");
eq(delay.disable(), true, "disable");
eq(delay.disable(), false, "disable again");
const elu = performance.eventLoopUtilization(before);
eq(elu.idle > 0 && elu.active > 0 && elu.utilization > 0 && elu.utilization < 1, true, "utilization");
eq(performance.nodeTiming.loopStart > 0 && performance.nodeTiming.idleTime > 0, true, "loop timing");
