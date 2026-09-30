// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application served by the runtime's native `wasi:http/handler`.
//
// The application registers a `fetch` listener synchronously at top level.
// `serve_after_await.js` registers one after a top-level `await`.
//
// For `/count`, the listener responds with the number of requests this instance
// has served, and for `/echo-body` with the request's body. For `/no-response`
// it does not call `respondWith`. Otherwise the handler echoes the request
// method and the URL's pathname into the response body, after a real
// `setTimeout` turn. The timer proves the per-request event
// loop actually drives between dispatch and the response settling (the same async
// proof the cli/run and async-export suites use): `respondWith` is handed a
// promise that only resolves after the timer fires, so a runtime that did not
// drive the request's loop would never produce the body.
//
// The same source serves identically when the runtime component reads it from
// the script path `STARLINGMONKEY_CONFIG` names (`serve.rs`,
// `script_path_serves_like_the_component`).

let served = 0;

addEventListener("fetch", (event) => {
  served += 1;
  const { pathname } = new URL(event.request.url);
  if (pathname === "/count") {
    event.respondWith(new Response(String(served)));
  } else if (pathname === "/echo-body") {
    event.respondWith(event.request.text().then((text) => new Response(text)));
  } else if (pathname !== "/no-response") {
    event.respondWith(handle(event.request));
  }
});

async function handle(request) {
  // Await a real timer so the request's event loop must take a turn before the
  // response is final, proving the async drive rather than a synchronous dispatch.
  await new Promise((resolve) => setTimeout(resolve, 5));

  const url = new URL(request.url);
  const body = `${request.method} ${url.pathname}`;
  return new Response(body, {
    status: 200,
    headers: { "content-type": "text/plain" },
  });
}
