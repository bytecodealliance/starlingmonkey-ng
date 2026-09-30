// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest of a world where two exported interfaces are named `api` and three
// items are named `greet`.
import type * as Guest from 'starling:guest';

export const testNaming: typeof Guest.testNaming = {
  api: {
    greet: (name) => `api ${name}`,
    Counter: class {
      get() {
        return 0;
      }
    },
  },
  left: {
    greet: (name) => `left ${name}`,
    onlyLeft: () => 1,
  },
  right: {
    greet: (name) => `right ${name}`,
  },
};

export const testOther: typeof Guest.testOther = {
  api: { greet: (name) => `other ${name}` },
};

// The world-level `greet` is the only bare `greet`.
export function greet(): string {
  return 'world';
}
greet satisfies typeof Guest.greet;

// Neither `api` has an interface-layer object.
// @ts-expect-error
type Api = typeof Guest.api;

// `onlyLeft` is unique, so it may be bare.
export const onlyLeft: typeof Guest.onlyLeft = () => 1;
