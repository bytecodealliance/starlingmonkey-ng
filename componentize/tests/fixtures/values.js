// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The values-world guest module.
//
// Flags arrive and leave as numbers holding the bit set, enums as case
// indices, `u64`s as bigints, `list<u64>` as a `BigUint64Array`, and a
// `result` argument as `{ tag, val }`. Every function of an imported and
// exported interface forwards to the host import of the same name and adds a
// change of its own on the way back.
//
// The exports use every shape of export names: `flags`, `variants` and `many`
// in the package layer, `keywords` as bare exports, `strings` and `checks` in
// the interface layer, and the world-level `fallible` as a bare export.

import * as flags from "test:values/flags";
import { type as hostType } from "test:values/keyword-imports";
import { echoRecord as hostEchoRecord } from "test:values/keywords";
import * as variants from "test:values/variants";
import * as many from "test:values/many";
import { Failure, KvError } from "external";
import { KvError as KvErrorCases } from "test:values/errors";

const flagsImpl = {
  roundtripFlag1: (f) => flags.roundtripFlag1(f) | 1,
  roundtripFlag2: (f) => flags.roundtripFlag2(f) | 1,
  roundtripFlag4: (f) => flags.roundtripFlag4(f) | 1,
  roundtripFlag8: (f) => flags.roundtripFlag8(f) | 1,
  roundtripFlag16: (f) => flags.roundtripFlag16(f) | 1,
  // `1 << 31` is a negative int32, which lowers as bit 31.
  roundtripFlag32: (f) => flags.roundtripFlag32(f) | (1 << 31),
};

export const func = (v) => hostType(v) + 1;
export const echoRecord = (record) => hostEchoRecord(record);

const variantsImpl = {
  casts: (a, b, c, d, e, f) => variants.casts(a, b, c, d, e, f),
  echoListVariant: (v) => variants.echoListVariant(v),
  echoResult(v) {
    try {
      return variants.echoResult(v);
    } catch (e) {
      // A host `err` arrives as a `ComponentError` whose payload is the
      // `list<u8>`. Anything else is a guest bug and traps.
      if (!(e instanceof ComponentError) || !(e.payload instanceof Uint8Array)) {
        throw new Error(`unexpected import failure: ${e}`);
      }
      throw new ComponentError(e.payload);
    }
  },
  failWith(bytes) {
    throw new ComponentError(bytes);
  },
};

const manyImpl = {
  manyArgs(...args) {
    // @ts-expect-error The sixteen arguments are passed on as they came.
    const list = many.manyArgs(...args);
    if (!(list instanceof BigUint64Array)) {
      throw new Error(`list<u64> must lift to a BigUint64Array, got ${list}`);
    }
    return list;
  },
  bigArgument: (x) => many.bigArgument(x),
};

export const strings = {
  echo: (s) => s,
  hello: () => "hello",
};

export const checks = {
  bytesFromArray: (n) => Array.from({ length: n }, (_, i) => i & 0xff),
  badPoint: () => ({ x: 310, y: 1 }),
  badImportArgument() {
    const field = (i) => `value${i}`;
    // Deliberately not a `BigStruct`: `a1` becomes a number below.
    /** @type {any} */
    const record = Object.fromEntries(
      Array.from({ length: 20 }, (_, i) => [`a${i + 1}`, field(i + 1)]),
    );
    record.a1 = 5;
    try {
      many.bigArgument(record);
    } catch (e) {
      if (e instanceof TypeError) {
        return e.message;
      }
      throw e;
    }
    throw new Error("bigArgument accepted a number for a string field");
  },
  async promised() {
    return "never lowered";
  },
  enumObjects: () =>
    JSON.stringify([
      flags.Flag2.b1,
      flags.Flag32.b31,
      flags.Flag32[-2147483648],
      Object.isFrozen(flags.Flag4),
      KvErrorCases.denied,
      KvErrorCases[0],
      Object.isFrozen(KvErrorCases),
      KvError.prototype instanceof ComponentError,
    ]),
  shiftingImportArgument() {
    let reads = 0;
    /** @type {any} */
    const record = Object.fromEntries(
      Array.from({ length: 20 }, (_, i) => [`a${i + 1}`, `value${i + 1}`]),
    );
    Object.defineProperty(record, "a1", {
      get: () => (++reads === 1 ? "first" : 5),
      enumerable: true,
    });
    return many.bigArgument(record).a1;
  },
  loneSurrogates: () => "a\uD800b\uDC00c",
  loneSurrogateChar: () => "\uD800",
};

export function fallible(worse) {
  if (!(Failure.prototype instanceof ComponentError)) {
    throw new Error("Failure must extend ComponentError");
  }
  throw new Failure(worse ? { tag: "worse" } : { tag: "bad", val: "bad" });
}

// An `Error` names an enum `err` case by its message, and a `KvError` holds the
// case's index.
export function lookup(key) {
  if (key === "missing") {
    throw new Error("notFound");
  }
  if (key === "secret") {
    throw new KvError(1);
  }
  return key;
}

export async function lookupAsync(key) {
  await new Promise((resolve) => setTimeout(resolve, 1));
  return lookup(key);
}

export const testValues = { flags: flagsImpl, variants: variantsImpl, many: manyImpl };
