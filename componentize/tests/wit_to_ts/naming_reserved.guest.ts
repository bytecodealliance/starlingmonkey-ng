// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest providing the interface named `default` through its interface-layer
// object, which is the module's default export, and `local`'s `for` as a bare
// export under a reserved name.
import type * as Guest from 'starling:guest';

const implementation: typeof Guest.default = {
  new: () => 1,
};
export default implementation;

function forEach(): number {
  return 2;
}
forEach satisfies typeof Guest.for;
export { forEach as for };
