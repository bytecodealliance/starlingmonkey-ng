// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest implementing `wasi:http/handler` itself in the package layer,
// returning either a web `Response` or the WIT `response`, forwarding the WIT
// request with `fetch`, and failing with an `ErrorCode`.
import type * as Guest from 'starling:guest';
import { ErrorCode, Response as WitResponse } from 'wasi:http/types@0.3.0';

export const wasiHttp: typeof Guest.wasiHttp = {
  handler: {
    async handle(request) {
      const path = request.path();
      if (path === '/fail') {
        throw new ErrorCode({ tag: 'internalError', val: 'failed' });
      }
      if (path === '/wit') {
        return new WitResponse(204);
      }
      if (path === '/forward') {
        return await fetch(request);
      }
      return new Response(`hello ${path}`);
    },
  },
};
