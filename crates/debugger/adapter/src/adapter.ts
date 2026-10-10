// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

import type { DebugProtocol } from "@vscode/debugprotocol";
import { Breakpoints } from "./breakpoints";
import { CAPABILITIES } from "./capabilities";
import { Sources } from "./sources";
import { evaluateWithoutSideEffects } from "./side-effects";
import { Stepper, type StepKind } from "./stepping";
import { References, describe, scopesOf, variablesOf } from "./values";

/** Returned by a request handler that sent its own response. */
export const RESPONDED = Symbol("responded");

/** The one thread the adapter reports. All content JavaScript runs on it. */
export const THREAD_ID = 1;

type Handler = (request: DebugProtocol.Request) => unknown;

/** The `launch` and `attach` arguments the adapter reads. */
export interface LaunchArguments {
  stopOnEntry?: boolean;
  localRoot?: string;
  remoteRoot?: string;
}

export function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export class Adapter {
  private readonly handlers = new Map<string, Handler>();
  private readonly afterResponseActions: Array<() => void> = [];
  private readonly detachActions: Array<() => void> = [];
  private detached = false;
  readonly sources = new Sources();
  private readonly breakpoints: Breakpoints;
  private readonly stopActions: Array<() => void> = [];
  private readonly resumeActions: Array<() => void> = [];
  private readonly refs = new References();
  private readonly stepper: Stepper;
  /** The frame the debuggee is paused in, while a pause is active. */
  pausedFrame: Debugger.Frame | undefined;
  launchArguments: LaunchArguments = {};
  /** The global `evaluate` and `setVariable` run in while nothing is paused. */
  private readonly contentGlobal: Debugger.Object;
  /**
   * How much the client's line and column numbers are below the adapter's, which start at 1.
   * `initialize` sets them.
   */
  private lineOffset = 0;
  private columnOffset = 0;

  constructor(
    readonly dbg: Debugger,
    private readonly rawContentGlobal: object,
  ) {
    dbg.addAllGlobalsAsDebuggees();
    this.contentGlobal = dbg.addDebuggee(rawContentGlobal);
    dbg.onNewGlobalObject = (global) => {
      dbg.addDebuggee(global);
    };
    dbg.uncaughtExceptionHook = (error) => {
      this.output("important", `Debug adapter error: ${describeError(error)}\n`);
      return undefined;
    };

    this.on("initialize", (request) => {
      const args = (request as DebugProtocol.InitializeRequest).arguments;
      this.lineOffset = args.linesStartAt1 === false ? 1 : 0;
      this.columnOffset = args.columnsStartAt1 === false ? 1 : 0;
      return CAPABILITIES;
    });
    this.on("launch", (request) => this.attach(request));
    this.on("attach", (request) => this.attach(request));
    this.on("configurationDone", () => {
      if (this.launchArguments.stopOnEntry) {
        this.stopAtNextFrame("entry");
      }
      configured();
      return undefined;
    });
    this.on("threads", () => ({ threads: [{ id: THREAD_ID, name: "JavaScript" }] }));
    this.on("disconnect", (request) =>
      this.disconnect(request as DebugProtocol.DisconnectRequest),
    );
    this.on("terminate", () => {
      this.afterResponse(() => terminate());
      return undefined;
    });

    this.breakpoints = new Breakpoints(dbg, this.sources, (frame, id) =>
      this.stop(frame, "breakpoint", { hitBreakpointIds: [id] }),
    );
    for (const script of dbg.findScripts()) {
      this.sources.add(script.source);
    }
    dbg.onNewScript = (script) => this.scriptLoaded(script);
    this.onDetach(() => this.breakpoints.clear());

    this.on("setBreakpoints", (request) => {
      const args = (request as DebugProtocol.SetBreakpointsRequest).arguments;
      const requested = args.breakpoints ?? [];
      if (args.source.path === undefined) {
        return {
          breakpoints: requested.map(() => ({
            verified: false,
            message: "Breakpoints need a source path",
          })),
        };
      }
      const breakpoints = this.breakpoints.set(
        args.source.path,
        requested.map((breakpoint) => ({
          ...breakpoint,
          line: this.lineFromClient(breakpoint.line),
          column: this.columnFromClient(breakpoint.column),
        })),
      );
      return { breakpoints: breakpoints.map((breakpoint) => this.breakpointToClient(breakpoint)) };
    });
    this.on("breakpointLocations", (request) => {
      const args = (request as DebugProtocol.BreakpointLocationsRequest).arguments;
      const path = args?.source.path;
      if (args === undefined || path === undefined) {
        return { breakpoints: [] };
      }
      const locations = this.breakpoints.locations(
        path,
        this.lineFromClient(args.line),
        args.endLine === undefined ? undefined : this.lineFromClient(args.endLine),
      );
      return {
        breakpoints: locations.map((location) => ({
          line: location.line - this.lineOffset,
          column: location.column === undefined ? undefined : location.column - this.columnOffset,
        })),
      };
    });
    this.on("continue", () => {
      resume();
      return { allThreadsContinued: true };
    });

    this.onStop(() => this.refs.clear());
    this.onResume(() => this.refs.clear());

    this.stepper = new Stepper(dbg, (frame) => this.stop(frame, "step"));
    this.onStop(() => this.stepper.clear());
    this.onDetach(() => this.stepper.clear());
    dbg.onDebuggerStatement = (frame) => {
      this.stop(frame, "debugger statement");
      return undefined;
    };

    for (const kind of ["next", "stepIn", "stepOut"] as const) {
      this.on(kind, () => this.step(kind));
    }
    this.on("pause", () => {
      if (this.pausedFrame === undefined) {
        this.stopAtNextFrame("pause");
      }
      return undefined;
    });

    this.on("stackTrace", (request) => {
      const args = (request as DebugProtocol.StackTraceRequest).arguments;
      const frames = this.frames();
      const start = args.startFrame ?? 0;
      const end = args.levels ? start + args.levels : frames.length;
      return {
        stackFrames: frames.slice(start, end).map((frame, i) => this.dapFrame(frame, start + i)),
        totalFrames: frames.length,
      };
    });
    this.on("scopes", (request) => {
      const args = (request as DebugProtocol.ScopesRequest).arguments;
      return { scopes: scopesOf(this.frame(args.frameId), this.refs) };
    });
    this.on("variables", (request) => {
      const args = (request as DebugProtocol.VariablesRequest).arguments;
      const container = this.refs.get(args.variablesReference);
      if (container === undefined) {
        throw new Error("Unknown variables reference");
      }
      return { variables: variablesOf(container, this.refs, args) };
    });
    this.on("setVariable", (request) =>
      this.setVariable((request as DebugProtocol.SetVariableRequest).arguments),
    );
    this.on("evaluate", (request) => {
      const args = (request as DebugProtocol.EvaluateRequest).arguments;
      const frame = this.pausedFrame !== undefined ? this.frame(args.frameId) : undefined;
      const completion =
        args.context === "hover"
          ? evaluateWithoutSideEffects(args.expression, frame, this.rawContentGlobal)
          : frame !== undefined
            ? frame.eval(args.expression)
            : this.contentGlobal.executeInGlobal(args.expression);
      if (completion === null && args.context === "hover") {
        throw new Error("Evaluation stopped before a possible side effect");
      }
      const { value, type, variablesReference, indexedVariables } = describe(
        this.completionValue(completion),
        this.refs,
      );
      return { result: value, type, variablesReference, indexedVariables };
    });
    this.on("source", (request) => {
      const args = (request as DebugProtocol.SourceRequest).arguments;
      const reference = args.source?.sourceReference ?? args.sourceReference;
      const content = this.sources.text(reference);
      if (content === undefined) {
        throw new Error(`Unknown source reference ${reference}`);
      }
      return { content, mimeType: "text/javascript" };
    });
    this.on("loadedSources", () => ({
      sources: this.sources.loaded().map((source) => this.sources.dapSource(source)),
    }));
  }

  on(command: string, handler: Handler): void {
    this.handlers.set(command, handler);
  }

  /** Run `action` after the current request's response is sent. */
  afterResponse(action: () => void): void {
    this.afterResponseActions.push(action);
  }

  /** Run `action` when the client detaches. */
  onDetach(action: () => void): void {
    this.detachActions.push(action);
  }

  handle(message: DebugProtocol.ProtocolMessage): void {
    if (message.type !== "request") {
      return;
    }
    const request = message as DebugProtocol.Request;
    const handler = this.handlers.get(request.command);
    if (handler === undefined) {
      this.respondError(request, `Unsupported request '${request.command}'`);
      return;
    }
    let body: unknown;
    try {
      body = handler(request);
    } catch (error) {
      this.afterResponseActions.length = 0;
      this.respondError(request, describeError(error));
      return;
    }
    if (body !== RESPONDED) {
      this.respond(request, body);
    }
    for (const action of this.afterResponseActions.splice(0)) {
      action();
    }
  }

  respond(request: DebugProtocol.Request, body?: unknown): void {
    send(
      JSON.stringify({
        type: "response",
        request_seq: request.seq,
        command: request.command,
        success: true,
        body,
      }),
    );
  }

  respondError(request: DebugProtocol.Request, message: string): void {
    send(
      JSON.stringify({
        type: "response",
        request_seq: request.seq,
        command: request.command,
        success: false,
        message,
      }),
    );
  }

  sendEvent(event: string, body?: unknown): void {
    send(JSON.stringify({ type: "event", event, body }));
  }

  output(category: string, output: string): void {
    this.sendEvent("output", { category, output });
  }

  /** Run `action` when the debuggee stops, before the `stopped` event is sent. */
  onStop(action: () => void): void {
    this.stopActions.push(action);
  }

  /** Run `action` when the debuggee resumes from a stop. */
  onResume(action: () => void): void {
    this.resumeActions.push(action);
  }

  /** Pause in `frame` until the client resumes. Does nothing while already paused. */
  stop(frame: Debugger.Frame, reason: string, extra: Record<string, unknown> = {}): void {
    if (this.pausedFrame !== undefined) {
      return;
    }
    if (!mayPause()) {
      this.output("console", `Skipped a ${reason} stop: this code cannot pause.\n`);
      return;
    }
    for (const action of this.stopActions) {
      action();
    }
    this.pausedFrame = frame;
    this.sendEvent("stopped", { reason, threadId: THREAD_ID, allThreadsStopped: true, ...extra });
    try {
      pause();
    } finally {
      this.pausedFrame = undefined;
      for (const action of this.resumeActions) {
        action();
      }
    }
  }

  private step(kind: StepKind): unknown {
    if (this.pausedFrame === undefined) {
      throw new Error("The debuggee is not paused");
    }
    this.stepper.start(kind, this.pausedFrame);
    resume();
    return undefined;
  }

  /** Stop in the next content frame that runs. */
  private stopAtNextFrame(reason: string): void {
    this.dbg.onEnterFrame = (frame) => {
      this.dbg.onEnterFrame = undefined;
      this.stop(frame, reason);
      return undefined;
    };
  }

  private scriptLoaded(script: Debugger.Script): void {
    // Code `evaluate` compiles in the content realm is no loaded source.
    if (script.source.introductionType === "debugger eval") {
      return;
    }
    if (this.sources.add(script.source)) {
      this.sendEvent("loadedSource", { reason: "new", source: this.sources.dapSource(script.source) });
    }
    for (const breakpoint of this.breakpoints.scriptLoaded(script)) {
      this.sendEvent("breakpoint", { reason: "changed", breakpoint: this.breakpointToClient(breakpoint) });
    }
  }

  private attach(request: DebugProtocol.Request): unknown {
    this.launchArguments = (request.arguments ?? {}) as LaunchArguments;
    this.sources.setRoots(this.launchArguments.localRoot, this.launchArguments.remoteRoot);
    this.afterResponse(() => this.sendEvent("initialized"));
    return undefined;
  }

  private disconnect(request: DebugProtocol.DisconnectRequest): unknown {
    if (request.arguments?.terminateDebuggee) {
      this.afterResponse(() => terminate());
      return undefined;
    }
    if (this.detached) {
      // The session passes a request with `seq` 0 when the connection closes. It gets no response.
      return request.seq === 0 ? RESPONDED : undefined;
    }
    this.detached = true;
    for (const action of this.detachActions) {
      action();
    }
    this.dbg.onNewScript = undefined;
    this.dbg.onEnterFrame = undefined;
    this.dbg.onDebuggerStatement = undefined;
    this.dbg.onNewGlobalObject = undefined;
    this.dbg.removeAllDebuggees();
    configured();
    resume();
    this.afterResponse(() => detached());
    return undefined;
  }

  /** The paused stack, innermost frame first. */
  private frames(): Debugger.Frame[] {
    if (this.pausedFrame === undefined) {
      throw new Error("The debuggee is not paused");
    }
    const frames: Debugger.Frame[] = [];
    for (let frame: Debugger.Frame | null = this.pausedFrame; frame !== null; frame = frame.older) {
      frames.push(frame);
    }
    return frames;
  }

  /** The paused frame with DAP id `frameId`, or the innermost one without an id. */
  private frame(frameId: number | undefined): Debugger.Frame {
    const frame = this.frames()[(frameId ?? 1) - 1];
    if (frame === undefined) {
      throw new Error(`Unknown frame ${frameId}`);
    }
    return frame;
  }

  private dapFrame(frame: Debugger.Frame, index: number): DebugProtocol.StackFrame {
    const id = index + 1;
    const name =
      frame.callee !== null
        ? (frame.callee.name ?? frame.callee.displayName ?? "(anonymous)")
        : `(${frame.type})`;
    if (frame.script === null) {
      return { id, name, line: 0, column: 0 };
    }
    const position = frame.script.getOffsetMetadata(frame.offset);
    return {
      id,
      name,
      line: position.lineNumber - this.lineOffset,
      column: position.columnNumber - this.columnOffset,
      source: this.sources.dapSource(frame.script.source),
    };
  }

  private lineFromClient(line: number): number {
    return line + this.lineOffset;
  }

  private columnFromClient(column: number | undefined): number | undefined {
    return column === undefined ? undefined : column + this.columnOffset;
  }

  private breakpointToClient(breakpoint: DebugProtocol.Breakpoint): DebugProtocol.Breakpoint {
    return {
      ...breakpoint,
      line: breakpoint.line === undefined ? undefined : breakpoint.line - this.lineOffset,
      column: breakpoint.column === undefined ? undefined : breakpoint.column - this.columnOffset,
    };
  }

  /** The value of a completion, or an `Error` describing why there is none. */
  private completionValue(completion: CompletionValue): unknown {
    if (completion === null) {
      throw new Error("Evaluation was terminated");
    }
    if ("throw" in completion) {
      throw new Error(`Uncaught ${describe(completion.throw, this.refs).value}`);
    }
    return completion.return;
  }

  private setVariable(args: DebugProtocol.SetVariableArguments): DebugProtocol.SetVariableResponse["body"] {
    const container = this.refs.get(args.variablesReference);
    if (container === undefined) {
      throw new Error("Unknown variables reference");
    }
    if (container.kind === "getter") {
      throw new Error("The value a getter returned cannot be set");
    }
    const value = this.completionValue(
      this.pausedFrame !== undefined
        ? this.frame(undefined).eval(args.value)
        : this.contentGlobal.executeInGlobal(args.value),
    );
    let current: unknown;
    if (container.kind === "object") {
      this.completionValue(container.object.setProperty(args.name, value));
      current = container.object.getOwnPropertyDescriptor(args.name)?.value;
    } else {
      const env = container.envs.find((candidate) => candidate.names().includes(args.name));
      if (env === undefined) {
        throw new Error(`No variable named '${args.name}'`);
      }
      env.setVariable(args.name, value);
      current = env.getVariable(args.name);
    }
    const { value: shown, type, variablesReference } = describe(current, this.refs);
    return { value: shown, type, variablesReference };
  }
}
