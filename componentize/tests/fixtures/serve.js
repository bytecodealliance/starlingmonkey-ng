// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application served by the runtime's native `wasi:http/handler`.
//
// The application registers its `fetch` listener after a top-level `await` on a
// timer. The listener is in place before the component is wizened, so the first
// request reaches it.
//
// For `/ready`, the listener writes to stdout and responds with the value the
// top-level `await` waited for and the path. The top level writes to stdout
// too, so the listener's write runs after the snapshot on a stream libc opens
// again. For `/count`, the listener responds with the number of requests this instance
// has served, and for `/echo-body` with the request's body. For `/no-response`
// it does not call `respondWith`. For `/reject`, it leaves a rejected promise
// unhandled and responds without waiting on anything. Otherwise the handler
// echoes the request method and the URL's pathname into the response body,
// after a `setTimeout` turn: `respondWith` is handed a promise that only
// resolves after the timer fires, so a runtime that did not drive the request's
// loop would never produce the body.

const greeting = await new Promise((resolve) => setTimeout(() => resolve("ready"), 20));
console.log(`top level ${greeting}`);

let served = 0;

addEventListener("fetch", (event) => {
  served += 1;
  const { pathname } = new URL(event.request.url);
  if (pathname === "/ready") {
    console.log(`handling ${event.request.url}`);
    event.respondWith(new Response(`${greeting} ${pathname}`));
  } else if (pathname === "/count") {
    event.respondWith(new Response(String(served)));
  } else if (pathname === "/echo-body") {
    event.respondWith(event.request.text().then((text) => new Response(text)));
  } else if (pathname === "/reject") {
    Promise.reject(new Error("listener rejection"));
    event.respondWith(new Response("ok"));
  } else if (pathname !== "/no-response") {
    event.respondWith(handle(event.request));
  }
});

async function handle(request) {
  await new Promise((resolve) => setTimeout(resolve, 5));

  const url = new URL(request.url);
  const body = `${request.method} ${url.pathname}`;
  return new Response(body, {
    status: 200,
    headers: { "content-type": "text/plain" },
  });
}
