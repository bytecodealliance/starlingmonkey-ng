// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest providing two versions of one interface under their full WIT names.
import type * as Guest from 'starling:guest';

const v1: (typeof Guest)['test:versioned/api@1.0.0'] = {
  greet: (name) => `hello ${name}`,
};

const v2: (typeof Guest)['test:versioned/api@2.0.0'] = {
  greet: (name) => `hi ${name}`,
  wave: () => 'wave',
};

export { v1 as 'test:versioned/api@1.0.0', v2 as 'test:versioned/api@2.0.0' };
