// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application importing interfaces the runtime imports, which the
// `--export-cli` and `--export-http` worlds import for it. For `/seed`, it
// responds with the high and low 64 bits `getInsecureSeed` returns.

import { getArguments } from "wasi:cli/environment@0.3";
import { getRandomBytes } from "wasi:random/random@0.3";
import { now } from "wasi:clocks/monotonic-clock@0.3";
import { getInsecureSeed } from "wasi:random/insecure-seed@0.3";

addEventListener("fetch", (event) => {
  if (new URL(event.request.url).pathname === "/seed") {
    const [high, low] = getInsecureSeed();
    event.respondWith(new Response(`${high},${low}`));
    return;
  }
  event.respondWith(
    new Response(
      JSON.stringify({
        arguments: Array.isArray(getArguments()),
        random: getRandomBytes(16n).length,
        clock: typeof now(),
      }),
    ),
  );
});
