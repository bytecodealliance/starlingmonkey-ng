// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The `stream<u8>` and `fetch` guest module. A lifted `stream<u8>` is a
// `ReadableStream` of bytes, which `fetch` takes as a request body, and a
// response's `body` is a `ReadableStream` of bytes, which lowers to a
// `stream<u8>`.

async function postStream(body) {
  // TypeScript's `RequestInit` has no `duplex` member, so `init` is built
  // first, which exempts it from the excess-property check.
  const init = { method: "POST", body, duplex: "half" };
  const response = await fetch("http://example.com/echo", init);
  return await response.text();
}

async function getBody() {
  const response = await fetch("http://example.com/data");
  return response.body;
}

let pipedBodySettled = false;

async function getPipedBody() {
  const response = await fetch("http://example.com/data");
  const transform = new TransformStream();
  /** @type {ReadableStream} */ (response.body)
    .pipeTo(transform.writable)
    .finally(() => { pipedBodySettled = true; });
  return transform.readable;
}

async function getBrokenBody() {
  const response = await fetch("http://example.com/broken");
  return response.body;
}

async function postAndStream(body) {
  // TypeScript's `RequestInit` has no `duplex` member, so `init` is built
  // first, which exempts it from the excess-property check.
  const init = { method: "POST", body, duplex: "half" };
  const response = await fetch("http://example.com/echo", init);
  return response.body;
}

/** @type {Response[]} */
let held = [];
/** @type {string[]} */
let readMessages = [];

async function holdResponses() {
  held = [await fetch("http://example.com/data"), await fetch("http://example.com/data")];
}

/** @param {Promise<unknown>} read */
function recordRead(read) {
  read.then(
    () => { readMessages.push("the read succeeded in a sync export"); },
    (e) => { readMessages.push(e instanceof TypeError ? e.message : String(e)); },
  );
}

function syncReadMessages() {
  const recorded = readMessages;
  readMessages = [];
  for (const [i, response] of held.entries()) {
    recordRead(i == 0 ? response.text() : /** @type {ReadableStream} */ (response.body).getReader().read());
  }
  held = [];
  return recorded;
}

// The timer, the stream and the fetch each change the result, so a bug in any
// of them changes the host's asserted output.
async function run(ms) {
  await new Promise((resolve) => setTimeout(resolve, ms));

  const stream = new ReadableStream({
    start(controller) {
      for (const chunk of ["part-a", "part-b", "part-c"]) {
        controller.enqueue(chunk);
      }
      controller.close();
    },
  });
  const reader = stream.getReader();
  let streamed = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) {
      break;
    }
    streamed += value;
  }

  const response = await fetch("http://example.com/data");
  const fetched = await response.text();

  return `timer=${ms} stream=${streamed} fetch=${fetched}`;
}

let inFlight = 0;

async function delayed(tag, ms) {
  inFlight += 1;
  await new Promise((resolve) => setTimeout(resolve, ms));
  const count = inFlight;
  inFlight -= 1;
  return `${tag}:${count}`;
}

export const fetching = {
  postStream,
  getBody,
  getPipedBody,
  pipedBodySettled: () => pipedBodySettled,
  getBrokenBody,
  postAndStream,
  holdResponses,
  syncReadMessages,
  run,
  delayed,
};
