// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// `serve_raw.js` for a runtime built for WASIp2: a serve application that implements
// `wasi:http/incoming-handler` itself instead of registering a `fetch` listener.
//
// The handler awaits a real `setTimeout`, so the export's per-call event loop takes a turn, then
// responds with a `200` whose body holds the request's method and path, which come from the
// imported `incoming-request` resource's methods. A request for `/fail` fails with the
// `error-code` variant's `internal-error` case instead, thrown as an instance of `ErrorCode`. A
// request for `/throw` throws an `Error` before `handle` returns, and the `error-code` cannot
// hold it. The handler is in the package layer.

import { ErrorCode } from "wasi:http/types@0.2";

export const wasiHttp = {
  incomingHandler: {
    handle(request) {
      if (request.pathWithQuery() === "/throw") {
        throw new Error("boom from handle");
      }
      return respond(request);
    },
  },
};

async function respond(request) {
  await new Promise((resolve) => setTimeout(resolve, 5));
  const method = request.method().tag;
  const path = request.pathWithQuery();
  if (path === "/fail") {
    throw new ErrorCode({ tag: "internalError", val: `raw ${method} ${path}` });
  }
  return new Response(`raw ${method} ${path}`);
}
