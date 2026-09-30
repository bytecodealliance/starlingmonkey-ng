// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application that implements `wasi:http/handler` itself instead of
// registering a `fetch` listener, so the finalized component exports this
// implementation as `wasi:http/handler`.
//
// The handler awaits a real `setTimeout`, so the export's per-call event loop
// takes a turn, then responds with a `200` whose body holds the request's method
// and path, which come from the imported `request` resource's methods. The
// response is a `wasi:http/types` one, imported as `WasiResponse` so the global
// `Response` stays visible. A request for `/fail` fails with the `error-code`
// variant's `internal-error` case instead, thrown as an instance of `ErrorCode`,
// the class of the handler's `err` type. The handler is in the package layer.

import { ErrorCode, Fields, Response as WasiResponse } from "wasi:http/types@0.3.0"

export const wasiHttp = {
    handler: {
        async handle(request) {
            await new Promise((resolve) => setTimeout(resolve, 5))
            const method = request.getMethod().tag
            const path = request.getPathWithQuery()
            if (path === "/fail") {
                throw new ErrorCode({ tag: "internalError", val: `raw ${method} ${path}` })
            }
            const body = new TextEncoder().encode(`raw ${method} ${path}`)
            // The body is any iterable of chunks, and the trailers a promise of none.
            const [response] = WasiResponse.new(new Fields(), [body], Promise.resolve(undefined))
            return response
        },
    },
}
