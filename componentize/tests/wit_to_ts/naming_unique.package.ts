// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest providing `test:naming/api` through the package layer.
import type * as Guest from 'starling:guest';

class Counter {
  #value: number;

  constructor(start: number) {
    this.#value = start;
  }

  get(): number {
    return this.#value;
  }
}

export const testNaming: typeof Guest.testNaming = {
  api: {
    greet: (name) => `hello ${name}`,
    Counter,
  },
};
