// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A command guest exporting `run`.
import type * as Guest from 'starling:guest';

export async function run(): Promise<void> {
  console.log('hello');
}
run satisfies typeof Guest.run;
