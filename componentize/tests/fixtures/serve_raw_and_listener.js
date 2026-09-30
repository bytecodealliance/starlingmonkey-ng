// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application that both implements `wasi:http/handler` and registers a
// `fetch` listener. Only one of them can serve the component's export, so
// componentizing it fails. The handler is in the interface layer.

export const handler = {
  async handle(_request) {
    throw new ComponentError({ tag: "internalError", val: "unreachable" });
  },
};

addEventListener("fetch", (event) => {
  event.respondWith(new Response("unreachable"));
});
