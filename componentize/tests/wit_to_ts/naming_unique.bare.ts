// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest providing each item of `test:naming/api` as a bare export.
import type * as Guest from 'starling:guest';

export function greet(name: string): string {
  return `hello ${name}`;
}
greet satisfies typeof Guest.greet;

export class Counter {
  constructor(private start: number) {}

  get(): number {
    return this.start;
  }

  [Symbol.dispose]() {}
}
Counter satisfies typeof Guest.Counter;

// A handle the guest receives is an instance of its own class.
const counter: Guest.Counter = new Counter(1);
void counter;
