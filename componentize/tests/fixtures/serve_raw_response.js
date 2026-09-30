// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application whose own `wasi:http/handler` returns `Response` objects,
// which lower to the handler's `wasi:http/types` `response`: the one `fetch`
// resolves to for `/proxy`, the one `fetch` resolves to when it sends the
// request itself for paths starting with `/echo`, one whose body a stream
// produces after a timer per chunk for `/slow`, one whose body never produces
// anything for `/stall`, one whose body was already read for `/used`, and a
// constructed one otherwise. `handle` is a bare export.

export async function handle(request) {
    const path = request.getPathWithQuery()
    if (path === "/proxy") {
        return await fetch("http://upstream.example/data")
    }
    if (path.startsWith("/echo")) {
        return await fetch(request)
    }
    if (path === "/slow") {
        const encoder = new TextEncoder()
        let remaining = ["one ", "two ", "three"]
        return new Response(
            new ReadableStream({
                async pull(controller) {
                    await new Promise((resolve) => setTimeout(resolve, 2))
                    const next = remaining.shift()
                    if (next === undefined) {
                        controller.close()
                    } else {
                        controller.enqueue(encoder.encode(next))
                    }
                },
            }),
        )
    }
    if (path === "/stall") {
        return new Response(new ReadableStream({ pull() { return new Promise(() => {}) } }))
    }
    if (path === "/used") {
        const response = new Response("read already")
        await response.text()
        return response
    }
    return new Response(`built ${path}`, {
        status: 201,
        headers: { "x-built": "yes" },
    })
}
