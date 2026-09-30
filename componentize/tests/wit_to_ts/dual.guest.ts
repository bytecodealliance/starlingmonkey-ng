// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest implementing an interface it also imports: its own `Item` class
// wraps the host's.
import type * as Guest from 'starling:guest';
import { Item as HostItem, Kind, get as hostGet } from 'test:dual/store';

class Item {
  #host: HostItem;

  constructor(v: number) {
    this.#host = new HostItem(v);
  }

  value(): number {
    return this.#host.value();
  }
}

export const store: typeof Guest.store = {
  get(k) {
    const entry = hostGet(k);
    return { item: new Item(entry.item.value()), tag: k === Kind.a ? 'a' : 'b' };
  },
  Item,
};

// The host's class is not the guest's.
// @ts-expect-error
const wrong: HostItem = new Item(1);
void wrong;
