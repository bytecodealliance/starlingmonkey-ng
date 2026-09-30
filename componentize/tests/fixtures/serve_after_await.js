// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application that registers its `fetch` listener after a top-level
// `await` on a timer. The listener is in place before the component is
// wizened, so the first request reaches it.

const greeting = await new Promise((resolve) => setTimeout(() => resolve("ready"), 20));
// Both the top level and the listener write to stdout, so the listener's write
// runs after the snapshot on a stream libc opens again.
console.log(`top level ${greeting}`);

addEventListener("fetch", (event) => {
  console.log(`handling ${event.request.url}`);
  event.respondWith(new Response(`${greeting} ${new URL(event.request.url).pathname}`));
});
