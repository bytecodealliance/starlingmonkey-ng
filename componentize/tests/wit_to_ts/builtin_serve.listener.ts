// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve guest registering a `fetch` listener.
addEventListener('fetch', (event) => {
  event.waitUntil(Promise.resolve());
  event.respondWith(new Response(`hello ${event.request.url}`));
});

export {};
