// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

import type { DebugProtocol } from "@vscode/debugprotocol";

/** What a variables reference names: the environments of a scope, or an object's properties. */
export type Container =
  | { kind: "scope"; envs: Debugger.Environment[]; thisValue: unknown }
  | { kind: "object"; object: Debugger.Object };

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
}

/** How the client shows `value`, a debuggee value or a `getVariable` marker object. */
export function describe(value: unknown, refs: References): Described {
  if (value instanceof Debugger.Object) {
    return {
      value: preview(value),
      type: value.callable ? "function" : value.class,
      variablesReference: refs.object(value),
    };
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

/** The variables a reference names. */
export function variablesOf(container: Container, refs: References): DebugProtocol.Variable[] {
  if (container.kind === "object") {
    const { object } = container;
    const variables = object.getOwnPropertyNames().map((name) => property(object, name, refs));
    if (object.proto !== null) {
      variables.push({ name: "[[Prototype]]", ...describe(object.proto, refs) });
    }
    return variables;
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
    for (const name of env.names()) {
      if (!seen.has(name)) {
        seen.add(name);
        variables.push({ name, ...describe(env.getVariable(name), refs) });
      }
    }
  }
  return variables;
}

function plain(value: string, type: string): Described {
  return { value, type, variablesReference: 0 };
}

function preview(object: Debugger.Object): string {
  if (object.callable) {
    return `ƒ ${object.name ?? object.displayName ?? ""}()`;
  }
  switch (object.class) {
    case "Array":
      return `Array(${String(object.getOwnPropertyDescriptor("length")?.value)})`;
    case "Object":
      return "{…}";
    default: {
      if (!object.class.endsWith("Error")) {
        return object.class;
      }
      const message = object.getOwnPropertyDescriptor("message")?.value;
      return message === undefined ? object.class : `${object.class}: ${String(message)}`;
    }
  }
}

function property(object: Debugger.Object, name: string, refs: References): DebugProtocol.Variable {
  const descriptor = object.getOwnPropertyDescriptor(name);
  if (descriptor !== undefined && !("value" in descriptor)) {
    const kinds = [descriptor.get && "get", descriptor.set && "set"].filter(Boolean).join("/");
    return { name, value: `(${kinds})`, type: "accessor", variablesReference: 0 };
  }
  return { name, ...describe(descriptor?.value, refs) };
}

function isGlobalObject(env: Debugger.Environment): boolean {
  return env.type === "object" && env.parent === null;
}

function scopeName(env: Debugger.Environment): string {
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
