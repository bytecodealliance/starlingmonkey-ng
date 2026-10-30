// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest implementing the `wasi:http/incoming-handler` of a runtime built for WASIp2 itself in
// the package layer, returning either a web `Response` or the WIT `outgoing-response`, forwarding
// the WIT request with `fetch`, and failing with an `ErrorCode`.
import type * as Guest from 'starling:guest';
import { ErrorCode, OutgoingResponse } from 'wasi:http/types@0.2';

export const wasiHttp: typeof Guest.wasiHttp = {
  incomingHandler: {
    async handle(request) {
      const path = request.pathWithQuery();
      if (path === '/fail') {
        throw new ErrorCode({ tag: 'internalError', val: 'failed' });
      }
      if (path === '/wit') {
        return new OutgoingResponse();
      }
      if (path === '/forward') {
        return await fetch(request);
      }
      return new Response(`hello ${path}`);
    },
  },
};
