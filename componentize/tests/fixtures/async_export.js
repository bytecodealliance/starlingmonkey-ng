// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The async-export world guest module.
//
// Each exported interface is an object named after the interface, the
// interface layer of the export names: `simple-export`'s `foo` is
// `simpleExport.foo`. `foo` is in both interfaces, so it cannot be a bare
// export.

import { release } from "test:async-export/host";

let drained = 0;
let fetchMessage = "none";

export const simpleExport = {
  // A plain (sync) export: lowered synchronously through `export_call`.
  foo: function (v) {
    return v + 3;
  },

  timerMessage: function () {
    try {
      setTimeout(() => {}, 1);
    } catch (e) {
      if (e instanceof TypeError) {
        return e.message;
      }
      throw e;
    }
    throw new Error("setTimeout succeeded in a sync export");
  },

  fetchMessage: function () {
    const recorded = fetchMessage;
    fetch("http://example.com/").then(
      () => { fetchMessage = "fetch succeeded in a sync export"; },
      (e) => { fetchMessage = e instanceof TypeError ? e.message : String(e); },
    );
    return recorded;
  },

  drainedMicrotasks: function () {
    const before = drained;
    Promise.resolve().then(() => { drained += 1; });
    return before;
  }
};

export const simpleAsyncExport = {
  // A genuine `async function`: it `await`s a real timer, so the returned
  // promise is pending when the runtime first observes it. Driving it to
  // settlement forces the per-call event loop through a `setTimeout` timer
  // turn, so the loop-driving the runtime must do is real, not a
  // synchronously-resolved `Promise.resolve(...)`.
  foo: async function (v) {
    await new Promise((resolve) => setTimeout(resolve, 1));
    return v + 3;
  },

  // Awaits a promise that never resolves, with no timer or other loop work
  // behind it. The per-call event loop goes idle while the returned promise is
  // still pending, which is a guest deadlock. The runtime must trap cleanly here, not
  // panic-abort.
  hang: async function () {
    await new Promise(() => {});
    return 0;
  },

  // Delivers `v + 3`, but first arms a perpetual `setInterval` that never
  // clears. The result promise settles immediately, so `task-return` lowers
  // `v + 3`, and the interval then keeps the per-call event loop non-idle forever.
  // It is not a Component Model waitable, so the runtime must complete this
  // export's subtask (returning the result) and abandon the timer when the
  // call's loop is dropped, and never wait on the interval, which would hang the
  // host. The body itself does not await anything, so the returned promise is
  // already settled when the runtime first observes it.
  perpetual: async function (v) {
    setInterval(() => {}, 1);
    return v + 3;
  },

  // Rejects after a timer turn, so the runtime sees the returned promise
  // pending first. An `Error` becomes the `err` string by its message.
  fail: async function (message) {
    await new Promise((resolve) => setTimeout(resolve, 1));
    throw new Error(message);
  },

  parked: async function () {
    const before = drained;
    await release();
    return drained - before;
  }
};
