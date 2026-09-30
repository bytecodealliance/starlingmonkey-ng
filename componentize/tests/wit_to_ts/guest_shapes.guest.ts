// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest whose resource class takes constructor arguments the WIT doesn't
// declare, and which hands over values in the forms the runtime accepts.
import type * as Guest from 'starling:guest';
import { bytes, maybe, send } from 'test:guest-shapes/host';

let opened = 0;

class Session {
  #id: number;

  constructor(id: number) {
    this.#id = id;
    opened++;
  }

  get(): number {
    return this.#id;
  }

  async later(): Promise<number> {
    return this.#id;
  }

  static async openCount(): Promise<number> {
    return opened;
  }
}

export const api: typeof Guest.api = {
  open(id) {
    return new Session(id);
  },
  check(n) {
    send({ data: new Uint8Array(2), body: new ReadableStream<Uint8Array>() });
    bytes(new Uint8ClampedArray(2));
    maybe(null);
    if (n.inner === undefined) {
      return { tag: 'internal' };
    }
    return n.inner.val === undefined ? { tag: 'internal', val: 'none' } : { tag: 'other' };
  },
  Session,
};
