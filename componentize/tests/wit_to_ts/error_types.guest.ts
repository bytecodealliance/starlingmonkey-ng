// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A guest calling imports that fail with error classes, and failing its own
// exports with them.
import type * as Guest from 'starling:guest';
import { Code, E as ErrsE } from 'test:errors/errs';
import { E, Failure, get } from 'test:errors/user';
import { MyErr } from 'test:errors/shared-errs';
import { Werr, worldOp } from 'external';

export function worldRun(): number {
  try {
    worldOp();
    return get();
  } catch (error) {
    if (error instanceof E || error instanceof ErrsE) {
      const payload: E['payload'] = error.payload;
      if (payload.tag === 'bad') {
        throw new Werr(0);
      }
    }
    if (error instanceof Failure && error.payload === Code.b) {
      throw new Werr(1);
    }
    throw error;
  }
}
worldRun satisfies typeof Guest.worldRun;

export const svc: typeof Guest.svc = {
  run() {
    throw new ComponentError(0);
  },
};

export const api: typeof Guest.api = {
  doIt() {
    throw new MyErr({ tag: 'b', val: 'failed' });
  },
};
