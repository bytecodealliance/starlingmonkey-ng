// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The resources-world guest module.
//
// Each exported interface wraps the host resource of the imported interface of
// the same name in a guest class: the guest `Thing` holds a host `Thing` in
// `value`, and every exported function unwraps its arguments, forwards them to
// the host import and re-wraps the host's results. Each function adds its own
// arithmetic or string suffix, so the host can check the whole chain from one
// return value.

import { Thing as AggregatesThing, foo as aggregatesFoo } from "test:resources/aggregates";
import {
  Thing as BorrowInRecordThing,
  test as borrowInRecordTest,
} from "test:resources/borrow-in-record";
import { Thing as Alias1Thing, a as alias1A } from "test:resources/alias1";
import { b as alias2B } from "test:resources/alias2";
import { Thing as ImportAndExportThing } from "test:resources/import-and-export";
import { Counter, reject } from "test:resources/counters";

// ---- aggregates ----

class AggregatesGuestThing {
  constructor(value) {
    this.value = new AggregatesThing(value + 1);
  }
}

function foo(r1, r2, r3, t1, t2, v1, v2, l1, l2, o1, o2, result1, result2) {
  return (
    aggregatesFoo(
      { thing: r1.thing.value },
      { thing: r2.thing.value },
      { thing1: r3.thing1.value, thing2: r3.thing2.value },
      [t1[0].value, { thing: t1[1].thing.value }],
      [t2[0].value],
      { tag: v1.tag, val: v1.val.value },
      { tag: v2.tag, val: v2.val.value },
      l1.map((x) => x.value),
      l2.map((x) => x.value),
      o1 === undefined ? undefined : o1.value,
      o2 === undefined ? undefined : o2.value,
      result1.tag === "ok" ? { tag: "ok", val: result1.val.value } : { tag: "err" },
      result2.tag === "ok" ? { tag: "ok", val: result2.val.value } : { tag: "err" },
    ) + 4
  );
}

const aggregates = {
  Thing: AggregatesGuestThing,
  foo,
};

// ---- borrow-in-record ----

class BorrowInRecordGuestThing {
  constructor(value) {
    this.value = new BorrowInRecordThing(value + " Thing");
  }

  get() {
    return this.value.get() + " Thing.get";
  }
}

// Wrap a host thing the host handed back without running the constructor,
// which would create a second host thing.
function wrapBorrowInRecord(hostThing) {
  const thing = Object.create(BorrowInRecordGuestThing.prototype);
  thing.value = hostThing;
  return thing;
}

const borrowInRecord = {
  Thing: BorrowInRecordGuestThing,
  test(list) {
    return borrowInRecordTest(list.map((x) => ({ thing: x.thing.value }))).map(
      wrapBorrowInRecord,
    );
  },
};

// ---- alias1 / alias2 ----

class Alias1GuestThing {
  constructor(value) {
    this.value = new Alias1Thing(value + " Thing");
  }

  get() {
    return this.value.get() + " Thing.get";
  }
}

function wrapAlias1(hostThing) {
  const thing = Object.create(Alias1GuestThing.prototype);
  thing.value = hostThing;
  return thing;
}

export const alias1 = {
  Thing: Alias1GuestThing,
  a(f) {
    return alias1A({ thing: f.thing.value }).map(wrapAlias1);
  },
};

export const alias2 = {
  b(f, g) {
    return alias2B({ thing: f.thing.value }, { thing: g.thing.value }).map(wrapAlias1);
  },
};

// ---- import-and-export ----

class ImportAndExportGuestThing {
  constructor(value) {
    this.value = new ImportAndExportThing(value + 1);
  }

  foo() {
    return this.value.foo() + 2;
  }

  bar(value) {
    this.value.bar(value + 3);
  }

  static baz(a, b) {
    return new ImportAndExportGuestThing(ImportAndExportThing.baz(a.value, b.value).foo() + 4);
  }
}

export const importAndExport = {
  Thing: ImportAndExportGuestThing,
};

// ---- world-level exports ----

export function bump(counter) {
  counter.set(counter.get() + 1);
}

export function test(things) {
  return things;
}

export function disposeEarly(counter) {
  const value = counter.get();
  counter[Symbol.dispose]();
  return value;
}

export function forgedDisposeMessages(counter) {
  const dispose = Object.getPrototypeOf(counter)[Symbol.dispose];
  const forgeries = [{}, Object.create(Object.getPrototypeOf(counter))];
  return forgeries.map((forged) => {
    try {
      dispose.call(forged);
      return "no error";
    } catch (e) {
      return e instanceof TypeError ? e.message : `not a TypeError: ${e}`;
    }
  });
}

// Two interfaces in the package layer, the rest in the interface layer.
export const testResources = { aggregates, borrowInRecord };

export function usingOwned(counter) {
  let value;
  {
    using owned = counter;
    value = owned.get();
  }
  counter[Symbol.dispose]();
  return value;
}

export function describeResourceError(counter) {
  try {
    reject(counter);
    return "no error";
  } catch (e) {
    return `${e.constructor.name} ${e.payload instanceof Counter} ${e.payload.get()}`;
  }
}

export function plainObjectForResource() {
  try {
    // @ts-expect-error A plain object is not a `Counter`.
    reject({});
    return "no error";
  } catch (e) {
    return `${e instanceof TypeError} ${e.message}`;
  }
}

/** @param {() => unknown} f */
function typeErrorMessage(f) {
  try {
    f();
    return "no error";
  } catch (e) {
    return e instanceof TypeError ? e.message : `not a TypeError: ${e}`;
  }
}

export function resourceMisuseMessages(counter) {
  const thing = new ImportAndExportThing(1);
  const aliased = new Alias1Thing("aliased");
  const disposed = new Alias1Thing("disposed");
  const other = new Alias1Thing("other");
  const borrowed = new BorrowInRecordThing("borrowed");
  return [
    typeErrorMessage(() => ImportAndExportThing.baz(thing, thing)),
    typeErrorMessage(() => alias2B({ thing: aliased }, { thing: aliased })),
    typeErrorMessage(() => reject(counter)),
    typeErrorMessage(() =>
      alias2B(
        { thing: disposed },
        {
          get thing() {
            disposed[Symbol.dispose]();
            return other;
          },
        },
      ),
    ),
    String(borrowInRecordTest([{ thing: borrowed }, { thing: borrowed }]).length),
    typeErrorMessage(() => ImportAndExportThing.baz(thing, new ImportAndExportThing(2))),
  ];
}

export function failQuietly() {
  throw new Error("the reason for failing quietly");
}
