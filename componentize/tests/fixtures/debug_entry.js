// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A `fetch` listener answering `42`, componentized by `debug.rs` from a relative
// input path. Line 7 runs for every request.

addEventListener("fetch", (event) => {
  event.respondWith(new Response(String(6 * 7)));
});
