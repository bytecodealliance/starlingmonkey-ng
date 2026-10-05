// Node's timer globals, `node:timers` and `node:timers/promises`.

import timers, { setTimeout as setTimeoutExport, setImmediate as setImmediateExport } from "node:timers";
import { setTimeout as sleep, setImmediate as immediate, scheduler } from "node:timers/promises";
import { eq, throws } from "./harness.mjs";

eq(setTimeoutExport, setTimeout, "node:timers' setTimeout is the global");
eq(setImmediateExport, setImmediate, "setImmediate export");
eq(timers.promises.setTimeout, sleep, "timers.promises");

const timeout = setTimeout(() => {
  throw new Error("cleared timeout ran");
}, 10);
eq(typeof timeout, "object", "setTimeout returns a Timeout");
eq(timeout.unref(), timeout, "unref returns the timeout");
eq(timeout.hasRef(), false, "hasRef after unref");
eq(timeout.ref().hasRef(), true, "hasRef after ref");
eq(typeof +timeout, "number", "Timeouts convert to their ID");
clearTimeout(timeout);
throws(() => setTimeout("not a function"), "ERR_INVALID_ARG_TYPE", "setTimeout validates");

const order = [];
await new Promise((resolve) => {
  setTimeout((a, b) => order.push(`timeout ${a}${b}`), 1, "x", "y");
  setImmediate((v) => order.push(`immediate ${v}`), "i");
  const interval = setInterval(() => {
    order.push("interval");
    if (order.filter((entry) => entry === "interval").length === 2) {
      clearInterval(interval);
      resolve();
    }
  }, 5);
});
eq(order.includes("timeout xy") && order.includes("immediate i"), true, "callbacks get their arguments");

// An interval's next run is due one interval after its previous run started, so after a first run
// that takes longer than the interval, the second run comes before a timeout the first run set
// for later than the interval.
const runs = [];
await new Promise((resolve) => {
  const slow = setInterval(() => {
    runs.push("interval");
    if (runs.length === 1) {
      setTimeout(() => {
        runs.push("timeout");
        clearInterval(slow);
        resolve();
      }, 50);
      const end = performance.now() + 60;
      while (performance.now() < end);
    }
  }, 40);
});
eq(runs.slice(0, 3).join(), "interval,interval,timeout", "an interval whose run takes longer than the interval");

let cleared = true;
const immediateHandle = setImmediate(() => (cleared = false));
clearImmediate(immediateHandle);
await new Promise((resolve) => setTimeout(resolve, 5));
eq(cleared, true, "clearImmediate");

let refreshed = 0;
const refreshable = setTimeout(() => refreshed++, 5);
eq(refreshable.refresh(), refreshable, "refresh returns the timeout");
await new Promise((resolve) => setTimeout(resolve, 20));
eq(refreshed, 1, "a refreshed timeout runs once");

eq(await sleep(1, "value"), "value", "timers/promises setTimeout");
eq(await immediate("now"), "now", "timers/promises setImmediate");
eq(await scheduler.wait(1), undefined, "scheduler.wait");
await scheduler.yield();

let unreferencedRan = false;
setTimeout(() => (unreferencedRan = true), 1).unref();
await new Promise((resolve) => setTimeout(resolve, 20));
eq(unreferencedRan, true, "unreferenced timers run while other work keeps the loop running");

// Callbacks get the timer as `this`, `refresh()` keeps the ID, and cleared timers stay cleared.
await new Promise((resolve) => {
  const timer = setTimeout(function () {
    eq(this, timer, "timer callbacks get the timer as this");
    resolve();
  }, 1);
});
let clearedRan = false;
const clearedTimer = setTimeout(() => (clearedRan = true), 1);
clearTimeout(clearedTimer);
clearedTimer.refresh();
const closedTimer = setTimeout(() => (clearedRan = true), 1);
closedTimer.close().refresh();
const refreshedTimer = setTimeout(() => (clearedRan = true), 1);
const refreshedId = +refreshedTimer;
eq(+refreshedTimer.refresh(), refreshedId, "refresh keeps the ID");
clearTimeout(refreshedId);
{
  using disposed = setTimeout(() => (clearedRan = true), 1);
}
await new Promise((resolve) => setTimeout(resolve, 10));
eq(clearedRan, false, "refresh doesn't restart a cleared timer");
throws(() => setTimeout(() => {}, Symbol()), TypeError, "setTimeout converts the delay");
const abort = new AbortController();
const aborted = sleep(60_000, "v", { signal: abort.signal });
abort.abort("why");
eq(await aborted.catch((error) => `${error.name} ${error.cause}`), "AbortError why", "abort rejects");
eq(await sleep(1, "v", { signal: 1 }).catch((error) => error.code), "ERR_INVALID_ARG_TYPE", "signal validation");

// The script finishes with these pending: unreferenced timers don't keep the loop running.
setInterval(() => {}, 1).unref();
const refreshedUnref = setTimeout(() => {
  throw new Error("unreferenced timeout kept the loop running");
}, 60_000).unref();
eq(refreshedUnref.refresh().hasRef(), false, "refresh keeps a timer unreferenced");
setImmediate(() => {}).unref();

// A fired immediate is unreferenced and drops its callback, and refreshing a fired timeout gives it
// a new async ID.
{
  const fired = setImmediate(() => {});
  await new Promise((resolve) => setTimeout(resolve, 5));
  eq(fired.hasRef(), false, "a fired immediate is unreferenced");
  eq(fired._onImmediate, undefined, "a fired immediate drops its callback");
  const timeout = setTimeout(() => {}, 1);
  const asyncId = Object.getOwnPropertySymbols(timeout).find((symbol) => String(symbol) === "Symbol(asyncId)");
  const before = timeout[asyncId];
  await new Promise((resolve) => setTimeout(resolve, 5));
  timeout.refresh();
  eq(timeout[asyncId] !== before, true, "refreshing a fired timeout gives it a new async ID");
  clearTimeout(timeout);
}
