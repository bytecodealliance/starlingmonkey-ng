// A `FileHandle` that is collected while open closes its descriptor, unless the descriptor was
// closed and now refers to another file. Runs with `--expose-gc`.

import { closeSync, mkdtempSync, openSync, rmSync, writeFileSync } from "node:fs";
import { open } from "node:fs/promises";
import { eq } from "./harness.mjs";

const dir = mkdtempSync("/tmp/starling-fs-finalize-");
const file = `${dir}/file.txt`;
const other = `${dir}/other.txt`;
writeFileSync(file, "");
writeFileSync(other, "");
const collect = async () => {
  for (let i = 0; i < 3; i++) {
    globalThis.gc();
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
};

let dropped;
await (async () => {
  dropped = (await open(file, "r")).fd;
})();
await collect();
const reopened = await open(file, "r");
eq(reopened.fd, dropped, "a collected FileHandle closes its descriptor");
await reopened.close();

let reused;
await (async () => {
  const handle = await open(file, "r");
  closeSync(handle.fd);
  reused = openSync(other, "r");
  eq(reused, handle.fd, "the descriptor number is reused");
})();
await collect();
const next = await open(file, "r");
eq(next.fd === reused, false, "a reused descriptor stays open");
closeSync(reused);
await next.close();
rmSync(dir, { recursive: true });
