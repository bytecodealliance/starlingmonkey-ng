// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A module that serves requests and runs as a command, for a component built
// with both `--cli` and `--serve`.

const greeting = "hello from one component";

addEventListener("fetch", (event) => {
  event.respondWith(
    new Response(`${greeting}, serving ${new URL(event.request.url).pathname}`),
  );
});

export async function run() {
  await new Promise((resolve) => setTimeout(resolve, 10));
  console.log(`${greeting}, running`);
}
