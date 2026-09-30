// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest providing `test:naming/api` through the interface layer.
import type * as Guest from 'starling:guest';

export const api: typeof Guest.api = {
  greet(name) {
    return `hello ${name}`;
  },
  Counter: class {
    constructor(private start: number) {}

    get() {
      return this.start;
    }
  },
};
