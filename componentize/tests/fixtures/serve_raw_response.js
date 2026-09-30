// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application whose own `wasi:http/handler` returns `Response` objects,
// which lower to the handler's `wasi:http/types` `response`: the one `fetch`
// resolves to for `/proxy`, the one `fetch` resolves to when it sends the
// request itself for paths starting with `/echo`, one whose body a stream
// produces after a timer per chunk for `/slow`, one whose body never produces
// anything for `/stall`, one whose body was already read for `/used`, and a
// constructed one otherwise. `handle` is a bare export.
//
// For `/plain-object` and `/unusable`, it passes a plain object and unusable
// `Response`s where `Response.consumeBody` of `wasi:http/types` takes an owned
// `response`, and responds with what each attempt threw.

import { Response as WasiResponse } from "wasi:http/types@0.3";

function attempt(out, label, response) {
  try {
    WasiResponse.consumeBody(response, Promise.resolve());
    out.push(`${label}: sent`);
  } catch (e) {
    out.push(`${label}: ${e.constructor.name}: ${e.message}`);
  }
}

export async function handle(request) {
  const path = request.getPathWithQuery();
  if (path === "/plain-object") {
    let outcome;
    try {
      // @ts-expect-error A plain object is passed where a `Response` is expected.
      WasiResponse.consumeBody({}, Promise.resolve());
      outcome = "returned";
    } catch (e) {
      outcome = `threw ${e.constructor.name}: ${e.message}`;
    }
    return new Response(outcome);
  }
  if (path === "/unusable") {
    const out = [];
    const read = new Response("read");
    await read.text();
    attempt(out, "read", read);
    const locked = new Response("locked");
    locked.body?.getReader();
    attempt(out, "locked", locked);
    const fetched = await fetch("http://upstream.example/data");
    attempt(out, "fetched", fetched);
    attempt(out, "again", fetched);
    await new Promise((resolve) => setTimeout(resolve, 5));
    return new Response(out.join("\n"));
  }
  if (path === "/proxy") {
    return await fetch("http://upstream.example/data");
  }
  if (path.startsWith("/echo")) {
    return await fetch(request);
  }
  if (path === "/slow") {
    const encoder = new TextEncoder();
    let remaining = ["one ", "two ", "three"];
    return new Response(
      new ReadableStream({
        async pull(controller) {
          await new Promise((resolve) => setTimeout(resolve, 2));
          const next = remaining.shift();
          if (next === undefined) {
            controller.close();
          } else {
            controller.enqueue(encoder.encode(next));
          }
        },
      }),
    );
  }
  if (path === "/stall") {
    return new Response(new ReadableStream({ pull() { return new Promise(() => {}); } }));
  }
  if (path === "/used") {
    const response = new Response("read already");
    await response.text();
    return response;
  }
  return new Response(`built ${path}`, {
    status: 201,
    headers: { "x-built": "yes" },
  });
}
