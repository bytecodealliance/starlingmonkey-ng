// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

// Native functions the debug session defines on the debugger global, and the parts of
// SpiderMonkey's `Debugger` API the adapter uses.

/** Queue an outgoing DAP message. The session assigns its `seq`. */
declare function send(json: string): void;
/** Record that the client sent `configurationDone`, which lets the content script start. */
declare function configured(): void;
/**
 * Record that the client detached with a `disconnect` request. A `listen` or `connect` session
 * then closes the connection.
 */
declare function detached(): void;
/**
 * Block, passing incoming messages to `handleMessage`, until `resume` is called or the client
 * disconnects. Returns `false` without blocking when this code cannot pause.
 */
declare function pause(): boolean;
/** End the current pause once the current `handleMessage` call returns. */
declare function resume(): void;
/** Whether `pause` would block. */
declare function mayPause(): boolean;
/** Send `exited` and `terminated`, flush the transport and exit the process. */
declare function terminate(): void;

/** The default content global. `evaluate` and `setVariable` run in it while nothing is paused. */
declare const contentGlobal: object;

type CompletionValue =
  | { return: unknown; yield?: boolean; await?: boolean }
  | { throw: unknown; stack?: unknown }
  | null;

type ResumptionValue = CompletionValue | undefined;

declare class Debugger {
  constructor();
  addAllGlobalsAsDebuggees(): void;
  addDebuggee(global: unknown): Debugger.Object;
  removeAllDebuggees(): void;
  findScripts(query?: { url?: string; line?: number; global?: Debugger.Object }): Debugger.Script[];
  /** The frame of this debugger that refers to the same stack frame as `frame`. */
  adoptFrame(frame: Debugger.Frame): Debugger.Frame;
  onNewScript: ((script: Debugger.Script) => void) | undefined;
  onEnterFrame: ((frame: Debugger.Frame) => ResumptionValue) | undefined;
  onDebuggerStatement: ((frame: Debugger.Frame) => ResumptionValue) | undefined;
  onNewGlobalObject: ((global: Debugger.Object) => void) | undefined;
  /**
   * Called before a native function runs inside an evaluation this debugger performs. `reason` is
   * `"get"`, `"set"`, `"call"` or another call reason.
   */
  onNativeCall: ((callee: Debugger.Object, reason: string) => ResumptionValue) | undefined;
  uncaughtExceptionHook: ((error: unknown) => ResumptionValue) | undefined;
  /** Whether the hooks of other debuggers are not called during this debugger's evaluations. */
  exclusiveDebuggerOnEval: boolean;
  /** Whether natives that check it avoid side effects. */
  shouldAvoidSideEffects: boolean;
}

declare namespace Debugger {
  interface BreakpointPosition {
    offset: number;
    lineNumber: number;
    columnNumber: number;
  }

  interface OffsetMetadata {
    lineNumber: number;
    columnNumber: number;
    isBreakpoint: boolean;
    isStepStart: boolean;
  }

  interface BreakpointHandler {
    hit(frame: Frame): ResumptionValue;
  }

  class Script {
    readonly url: string;
    readonly source: Source;
    getPossibleBreakpoints(query: {
      line?: number;
      minLine?: number;
      maxLine?: number;
    }): BreakpointPosition[];
    getOffsetMetadata(offset: number): OffsetMetadata;
    setBreakpoint(offset: number, handler: BreakpointHandler): void;
    /** The handlers of this debugger's breakpoints at `offset`. */
    getBreakpoints(offset: number): BreakpointHandler[];
    clearBreakpoint(handler: BreakpointHandler): void;
    /** The offsets of the bytecodes that can have side effects outside the running frame. */
    getEffectfulOffsets(): number[];
    readonly global: Object;
  }

  class Source {
    readonly url: string;
    readonly text: string;
    readonly introductionType: string | undefined;
  }

  class Frame {
    readonly type: string;
    readonly callee: Object | null;
    readonly script: Script | null;
    readonly offset: number;
    readonly older: Frame | null;
    readonly environment: Environment | null;
    readonly "this": unknown;
    readonly terminated: boolean;
    onStep: ((this: Frame) => ResumptionValue) | undefined;
    onPop: ((this: Frame, completion: CompletionValue) => ResumptionValue) | undefined;
    eval(code: string): CompletionValue;
  }

  class Environment {
    readonly type: "declarative" | "object" | "with";
    readonly parent: Environment | null;
    readonly object: Object | undefined;
    readonly callee: Object | null | undefined;
    readonly optimizedOut: boolean;
    readonly scopeKind: string | undefined;
    names(): string[];
    getVariable(name: string): unknown;
    setVariable(name: string, value: unknown): void;
  }

  class Object {
    readonly class: string;
    readonly callable: boolean;
    readonly name: string | undefined;
    readonly displayName: string | undefined;
    readonly proto: Object | null;
    readonly isProxy: boolean;
    /** The target of a scripted proxy: `null` once it is revoked, `undefined` for other objects. */
    readonly proxyTarget: Object | null | undefined;
    /** The handler of a scripted proxy, like `proxyTarget`. */
    readonly proxyHandler: Object | null | undefined;
    getOwnPropertyNames(): string[];
    getOwnPropertyNamesLength(): number;
    getOwnPropertyDescriptor(name: string): PropertyDescriptor | undefined;
    setProperty(name: string, value: unknown): CompletionValue;
    call(thisValue: unknown, ...args: unknown[]): CompletionValue;
    executeInGlobal(code: string): CompletionValue;
    /** The referent itself, wrapped for the debugger's realm. */
    unsafeDereference(): object;
  }

  interface PropertyDescriptor {
    value?: unknown;
    get?: Object;
    set?: Object;
  }
}
