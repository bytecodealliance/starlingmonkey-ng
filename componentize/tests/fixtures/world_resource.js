// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The world-resource guest module: add one to the borrowed host thing.

export function f(thing) {
  thing.set(thing.get() + 1);
}
