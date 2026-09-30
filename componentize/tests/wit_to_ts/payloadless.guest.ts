// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest that receives and hands over a `stream` and a `future` without a
// payload type, in the forms the runtime accepts.
import type * as Guest from 'starling:guest';
import { count, done, ticks, wait } from 'test:payloadless/signals';

export const relay: typeof Guest.relay = {
  async *echoTicks(s) {
    for await (const tick of s) {
      yield tick;
    }
  },
  async echoDone(f) {
    await f;
    await wait(done());
    await wait(Promise.resolve());
    await wait(undefined);
  },
  async countTicks(s) {
    const received = await ticks(2);
    const chunk: undefined = (await received.getReader().read()).value;
    await count([undefined, chunk]);
    await count(
      (async function* () {
        yield;
      })(),
    );
    let n = 0;
    for await (const _ of s) {
      n++;
    }
    return n;
  },
};
