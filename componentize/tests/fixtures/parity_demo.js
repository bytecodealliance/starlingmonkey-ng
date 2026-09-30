// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The combined-world parity demo guest module.
//
// `test:parity-demo/demo`'s `run` and `delayed` are bare exports.

let inFlight = 0

const demo = {
    // The combined-builtins parity export. Under the ONE per-call event loop the
    // async export drives, this exercises three builtin surfaces:
    //   1. a timer turn: `await new Promise(r => setTimeout(r, ms))` sleeps the
    //      per-call loop on the WASIp3 monotonic clock, then resumes.
    //   2. a `ReadableStream` (web-streams builtin, pure JS, no host) built from
    //      literal chunks and drained through a reader, and the chunks are
    //      concatenated into the result.
    //   3. a `fetch` through the WASIp3 HTTP client: the request body is built
    //      by the wasm fetch backend via wasip3 `wit_stream`/`wit_future`, whose
    //      stream and future vtable thunks the forwarding shim resolves.
    //      The harness responds to every request with a fixed body, which we read
    //      back and fold into the result.
    // The three results are combined into the returned string, so a bug in any
    // leg changes the host's asserted output.
    run: async function (ms) {
        // Leg 1: a real timer turn.
        await new Promise((resolve) => setTimeout(resolve, ms))

        // Leg 2: construct and drain a ReadableStream built from literal chunks.
        const chunks = ["part-a", "part-b", "part-c"]
        const stream = new ReadableStream({
            start(controller) {
                for (const chunk of chunks) {
                    controller.enqueue(chunk)
                }
                controller.close()
            },
        })
        const reader = stream.getReader()
        let streamed = ""
        for (;;) {
            const { value, done } = await reader.read()
            if (done) {
                break
            }
            streamed += value
        }

        // Leg 3: a real `fetch` through the WASIp3 outgoing HTTP client. The
        // request body is built by the wasm fetch backend via wasip3
        // `wit_stream`/`wit_future`, whose vtable thunks the forwarding shim now
        // resolves. The harness responds to every request with a fixed body,
        // which we read back and fold into the result, so a broken thunk
        // forwarding (or a stubbed vtable) changes the host's asserted output.
        const response = await fetch("https://parity.invalid/")
        const fetched = await response.text()

        return `timer=${ms} stream=${streamed} fetch=${fetched}`
    },

    // Sleep `ms`, then return `tag` and the number of calls in flight,
    // counting this one.
    delayed: async function (tag, ms) {
        inFlight += 1
        await new Promise((resolve) => setTimeout(resolve, ms))
        const count = inFlight
        inFlight -= 1
        return `${tag}:${count}`
    },
}

export const { run, delayed } = demo
