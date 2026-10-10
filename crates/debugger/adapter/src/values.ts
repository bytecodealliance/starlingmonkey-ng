// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

import type { DebugProtocol } from "@vscode/debugprotocol";

/**
 * What a variables reference names: the environments of a scope, an object's properties, or the
 * value an accessor property's getter returns.
 */
export type Container =
  | { kind: "scope"; envs: Debugger.Environment[]; thisValue: unknown }
  | { kind: "object"; object: Debugger.Object }
  | { kind: "getter"; name: string; getter: Debugger.Object; receiver: Debugger.Object };

/** Variables references. They stay valid until `clear`, which the adapter calls on each stop and resume. */
export class References {
  private readonly containers = new Map<number, Container>();
  private readonly objectReferences = new Map<Debugger.Object, number>();
  private next = 1;

  clear(): void {
    this.containers.clear();
    this.objectReferences.clear();
    this.next = 1;
  }

  scope(envs: Debugger.Environment[], thisValue?: unknown): number {
    return this.add({ kind: "scope", envs, thisValue });
  }

  object(object: Debugger.Object): number {
    let reference = this.objectReferences.get(object);
    if (reference === undefined) {
      reference = this.add({ kind: "object", object });
      this.objectReferences.set(object, reference);
    }
    return reference;
  }

  getter(name: string, getter: Debugger.Object, receiver: Debugger.Object): number {
    return this.add({ kind: "getter", name, getter, receiver });
  }

  get(reference: number): Container | undefined {
    return this.containers.get(reference);
  }

  private add(container: Container): number {
    const reference = this.next++;
    this.containers.set(reference, container);
    return reference;
  }
}

export interface Described {
  value: string;
  type: string;
  variablesReference: number;
  /** The number of array elements, for arrays and typed arrays. */
  indexedVariables?: number;
}

/**
 * How the client shows `value`, a debuggee value or a `getVariable` marker object. Describing a
 * value runs no debuggee code.
 */
export function describe(value: unknown, refs: References): Described {
  if (value instanceof Debugger.Object) {
    const described: Described = {
      value: preview(value),
      type: value.isProxy ? "Proxy" : value.callable ? "function" : value.class,
      variablesReference: refs.object(value),
    };
    const length = indexedLength(value);
    if (length !== undefined) {
      described.indexedVariables = length;
    }
    return described;
  }
  if (value !== null && typeof value === "object") {
    // `Debugger.Environment.getVariable` returns these for bindings it cannot read.
    if ("optimizedOut" in value) {
      return plain("<optimized out>", "unavailable");
    }
    if ("uninitialized" in value) {
      return plain("<uninitialized>", "uninitialized");
    }
    if ("missingArguments" in value) {
      return plain("<unavailable>", "unavailable");
    }
  }
  switch (typeof value) {
    case "string":
      return plain(JSON.stringify(value), "string");
    case "bigint":
      return plain(`${value}n`, "bigint");
    case "symbol":
      return plain(value.toString(), "symbol");
    case "undefined":
      return plain("undefined", "undefined");
    default:
      return plain(String(value), value === null ? "null" : typeof value);
  }
}

/**
 * The scopes of `frame`, innermost first. "Local" covers the frame's own environments up to and
 * including its function or module environment. The global object is last.
 */
export function scopesOf(frame: Debugger.Frame, refs: References): DebugProtocol.Scope[] {
  const local: Debugger.Environment[] = [];
  let env = frame.environment;
  while (env !== null) {
    local.push(env);
    const parent: Debugger.Environment | null = env.parent;
    const endsLocal =
      env.callee != null || env.scopeKind === "module" || parent === null || isGlobalObject(parent);
    env = parent;
    if (endsLocal) {
      break;
    }
  }
  const scopes: DebugProtocol.Scope[] = [
    {
      name: "Local",
      presentationHint: "locals",
      variablesReference: refs.scope(local, frame["this"]),
      expensive: false,
    },
  ];
  for (; env !== null; env = env.parent) {
    if (isGlobalObject(env) && env.object !== undefined) {
      scopes.push({ name: "Global", variablesReference: refs.object(env.object), expensive: true });
      break;
    }
    scopes.push({ name: scopeName(env), variablesReference: refs.scope([env]), expensive: false });
  }
  return scopes;
}

/**
 * The variables a reference names, as the `variables` request `args` selects them. Only an
 * accessor property's getter container runs debuggee code: it calls the getter.
 *
 * For an object, `filter: "indexed"` selects the elements `start` through `start + count - 1` of
 * an array or typed array, and `filter: "named"` selects its other properties. Without a filter,
 * every property is listed. A proxy lists its target and handler, and no properties.
 */
export function variablesOf(
  container: Container,
  refs: References,
  args: DebugProtocol.VariablesArguments,
): DebugProtocol.Variable[] {
  switch (container.kind) {
    case "object":
      return objectVariables(container.object, refs, args);
    case "getter":
      return [{ name: container.name, ...getterValue(container, refs) }];
    case "scope":
      break;
  }
  const variables: DebugProtocol.Variable[] = [];
  if (container.thisValue !== undefined) {
    variables.push({ name: "this", ...describe(container.thisValue, refs) });
  }
  const seen = new Set<string>();
  for (const env of container.envs) {
    if (env.optimizedOut) {
      continue;
    }
    if (env.type !== "declarative") {
      // `names` and `getVariable` of an object environment run proxy traps and getters.
      if (env.object !== undefined) {
        for (const variable of ownProperties(env.object, refs)) {
          if (!seen.has(variable.name)) {
            seen.add(variable.name);
            variables.push(variable);
          }
        }
      }
      continue;
    }
    for (const name of env.names()) {
      if (!seen.has(name)) {
        seen.add(name);
        variables.push({ name, ...describe(env.getVariable(name), refs) });
      }
    }
  }
  return variables;
}

function objectVariables(
  object: Debugger.Object,
  refs: References,
  args: DebugProtocol.VariablesArguments,
): DebugProtocol.Variable[] {
  if (object.isProxy) {
    return [
      { name: "[[Target]]", ...describe(object.proxyTarget ?? null, refs) },
      { name: "[[Handler]]", ...describe(object.proxyHandler ?? null, refs) },
    ];
  }
  const length = indexedLength(object);
  if (args.filter === "indexed" && length !== undefined) {
    const start = Math.min(args.start ?? 0, length);
    const end = Math.min(start + (args.count ?? length), length);
    const variables: DebugProtocol.Variable[] = [];
    for (let index = start; index < end; index++) {
      variables.push(property(object, String(index), refs));
    }
    return variables;
  }
  const variables =
    args.filter === "named" && length !== undefined
      ? namedProperties(object, length, refs)
      : ownProperties(object, refs);
  if (object.proto !== null) {
    variables.push({ name: "[[Prototype]]", ...describe(object.proto, refs) });
  }
  return variables;
}

/** The own properties of `object`, or none for a proxy. */
function ownProperties(object: Debugger.Object, refs: References): DebugProtocol.Variable[] {
  if (object.isProxy) {
    return [];
  }
  return object.getOwnPropertyNames().map((name) => property(object, name, refs));
}

/**
 * The own properties of `object`, an array or typed array with `length` elements, that are not
 * elements. The property names are listed only if `object` has properties besides its elements
 * and an array's `length`.
 */
function namedProperties(
  object: Debugger.Object,
  length: number,
  refs: References,
): DebugProtocol.Variable[] {
  const isArray = object.class === "Array";
  if (object.getOwnPropertyNamesLength() === length + (isArray ? 1 : 0)) {
    return isArray ? [property(object, "length", refs)] : [];
  }
  return object
    .getOwnPropertyNames()
    .filter((name) => !isIndex(name, length))
    .map((name) => property(object, name, refs));
}

function isIndex(name: string, length: number): boolean {
  return /^(0|[1-9][0-9]*)$/.test(name) && Number(name) < length;
}

const TYPED_ARRAY_CLASSES = new Set([
  "Int8Array",
  "Uint8Array",
  "Uint8ClampedArray",
  "Int16Array",
  "Uint16Array",
  "Int32Array",
  "Uint32Array",
  "Float16Array",
  "Float32Array",
  "Float64Array",
  "BigInt64Array",
  "BigUint64Array",
]);

/** `%TypedArray%.prototype.length`'s getter in the debugger's realm. */
const typedArrayLength = Object.getOwnPropertyDescriptor(
  Object.getPrototypeOf(Int8Array.prototype) as object,
  "length",
)?.get as (this: object) => number;

/** The number of elements of an array or typed array, or `undefined` for other objects. */
function indexedLength(object: Debugger.Object): number | undefined {
  if (object.isProxy) {
    return undefined;
  }
  if (object.class === "Array") {
    const length = object.getOwnPropertyDescriptor("length")?.value;
    return typeof length === "number" ? length : undefined;
  }
  if (TYPED_ARRAY_CLASSES.has(object.class)) {
    // The getter is a native of the debugger's realm, so calling it runs no debuggee code.
    return typedArrayLength.call(object.unsafeDereference());
  }
  return undefined;
}

/** The value the getter of a container returns, or the exception it throws. */
function getterValue(
  container: Extract<Container, { kind: "getter" }>,
  refs: References,
): Described {
  const completion = container.getter.call(container.receiver);
  if (completion === null) {
    return plain("<terminated>", "unavailable");
  }
  if ("throw" in completion) {
    return plain(`Uncaught ${describe(completion.throw, refs).value}`, "exception");
  }
  return describe(completion.return, refs);
}

function plain(value: string, type: string): Described {
  return { value, type, variablesReference: 0 };
}

function preview(object: Debugger.Object): string {
  if (object.isProxy) {
    return "Proxy";
  }
  if (object.callable) {
    return `ƒ ${object.name ?? object.displayName ?? ""}()`;
  }
  switch (object.class) {
    case "Array":
      return `Array(${String(object.getOwnPropertyDescriptor("length")?.value)})`;
    case "Object":
      return "{…}";
    default: {
      if (TYPED_ARRAY_CLASSES.has(object.class)) {
        return `${object.class}(${String(indexedLength(object))})`;
      }
      if (!object.class.endsWith("Error")) {
        return object.class;
      }
      const message = object.getOwnPropertyDescriptor("message")?.value;
      return message === undefined ? object.class : `${object.class}: ${String(message)}`;
    }
  }
}

/**
 * The variable for the own property `name` of `object`. An accessor property with a getter is
 * lazy: its value is the result of a `variables` request for its reference, which calls the getter.
 */
function property(object: Debugger.Object, name: string, refs: References): DebugProtocol.Variable {
  const descriptor = object.getOwnPropertyDescriptor(name);
  if (descriptor !== undefined && !("value" in descriptor)) {
    if (descriptor.get === undefined) {
      return { name, value: "(set)", type: "accessor", variablesReference: 0 };
    }
    return {
      name,
      value: descriptor.set === undefined ? "(get)" : "(get/set)",
      type: "accessor",
      presentationHint: { lazy: true },
      variablesReference: refs.getter(name, descriptor.get, object),
    };
  }
  return { name, ...describe(descriptor?.value, refs) };
}

function isGlobalObject(env: Debugger.Environment): boolean {
  return env.type === "object" && env.parent === null;
}

function scopeName(env: Debugger.Environment): string {
  if (env.type === "with") {
    return "With Block";
  }
  if (env.callee != null) {
    return `Closure (${env.callee.name ?? "anonymous"})`;
  }
  if (env.scopeKind === "module") {
    return "Module";
  }
  if (env.parent !== null && isGlobalObject(env.parent)) {
    return "Script";
  }
  return "Block";
}
