// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application importing interfaces the runtime imports, which the
// `--cli` and `--serve` worlds import for it.

import { getArguments } from "wasi:cli/environment@0.3.0";
import { getRandomBytes } from "wasi:random/random@0.3.0";
import { now } from "wasi:clocks/monotonic-clock@0.3.0";

addEventListener("fetch", (event) => {
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
