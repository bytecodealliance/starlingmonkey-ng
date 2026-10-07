// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application whose top level leaves an interval and a timeout pending,
// which the snapshot holds. The listener waits 150 ms, so the instance's timers
// can fire while it handles the request, then responds with how often each has
// fired so far.

globalThis.ticks = 0;
globalThis.fired = 0;
setInterval(() => {
  globalThis.ticks += 1;
}, 50);
setTimeout(() => {
  globalThis.fired += 1;
}, 600);

addEventListener("fetch", (event) => {
  event.respondWith(
    new Promise((resolve) => setTimeout(resolve, 150)).then(
      () => new Response(`${globalThis.ticks} ${globalThis.fired}`),
    ),
  );
});
