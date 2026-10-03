// Assertions for the Node.js compatibility test scripts. They don't use `node:assert`, so a broken
// `node:assert` can't hide failures.

function format(value) {
  if (typeof value === "string") return JSON.stringify(value);
  if (typeof value === "symbol" || typeof value === "bigint") return String(value);
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}

/** Throw unless `actual` and `expected` are the same value. */
export function eq(actual, expected, message = "") {
  if (!Object.is(actual, expected)) {
    throw new Error(`${message}: expected ${format(expected)}, got ${format(actual)}`);
  }
}

/** Throw unless `actual` and `expected` have the same JSON representation. */
export function deepEq(actual, expected, message = "") {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) throw new Error(`${message}: expected ${b}, got ${a}`);
}

/**
 * Throw unless `fn` throws an error whose `code` is `code` (or, if `code` is a constructor, an
 * instance of it). Returns the error.
 */
export function throws(fn, code, message = "") {
  try {
    fn();
  } catch (error) {
    if (typeof code === "string" && error?.code !== code) {
      throw new Error(`${message}: expected code ${code}, got ${error?.code} (${error})`);
    }
    if (typeof code === "function" && !(error instanceof code)) {
      throw new Error(`${message}: expected a ${code.name}, got ${error}`);
    }
    return error;
  }
  throw new Error(`${message}: did not throw`);
}
