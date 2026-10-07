// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// `serve_timers.js` for an application that implements `wasi:http/handler`
// itself: its top level leaves an interval and a timeout pending, and its
// handler waits 150 ms, then responds with how often each has fired so far.

import { Fields, Response as WasiResponse } from "wasi:http/types@0.3.0"

globalThis.ticks = 0
globalThis.fired = 0
setInterval(() => {
    globalThis.ticks += 1
}, 50)
setTimeout(() => {
    globalThis.fired += 1
}, 600)

export const wasiHttp = {
    handler: {
        async handle() {
            await new Promise((resolve) => setTimeout(resolve, 150))
            const body = new TextEncoder().encode(`${globalThis.ticks} ${globalThis.fired}`)
            const [response] = WasiResponse.new(new Fields(), [body], Promise.resolve(undefined))
            return response
        },
    },
}
