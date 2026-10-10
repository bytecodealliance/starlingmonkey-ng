// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

import type { DebugProtocol } from "@vscode/debugprotocol";
import type { Sources } from "./sources";

/** How many lines past the requested one a line breakpoint may move to find code. */
const MAX_LINE_SLIDE = 20;

interface Candidate {
  script: Debugger.Script;
  position: Debugger.BreakpointPosition;
}

interface Entry {
  id: number;
  line: number;
  column: number | undefined;
  handler: Debugger.BreakpointHandler;
  /** The position the breakpoint is set at, once it is set in a script. */
  bound: { line: number; column: number } | undefined;
  /** The URLs of the scripts the breakpoint is set in. */
  urls: Set<string>;
}

/**
 * Line breakpoints, keyed by client path. A breakpoint is set in every loaded script of its file,
 * including the scripts each global compiles from that file, and in each one that loads later.
 * Lines and columns are 1-based.
 */
export class Breakpoints {
  private nextId = 1;
  private readonly byPath = new Map<string, Entry[]>();

  constructor(
    private readonly dbg: Debugger,
    private readonly sources: Sources,
    private readonly onHit: (frame: Debugger.Frame, id: number) => void,
  ) {}

  /** Replace the breakpoints in `clientPath`, setting each in the loaded code where there is some. */
  set(clientPath: string, requested: DebugProtocol.SourceBreakpoint[]): DebugProtocol.Breakpoint[] {
    for (const entry of this.byPath.get(clientPath) ?? []) {
      this.unbind(entry);
    }
    if (requested.length === 0) {
      this.byPath.delete(clientPath);
      return [];
    }
    const entries = requested.map((breakpoint) => this.create(breakpoint));
    this.byPath.set(clientPath, entries);
    const scripts = this.urlsFor(clientPath).flatMap((url) => this.dbg.findScripts({ url }));
    for (const entry of entries) {
      this.bind(entry, scripts);
    }
    return entries.map((entry) => describe(entry));
  }

  /**
   * Set the breakpoints of the file `script` was compiled from in the scripts of that file in the
   * global of `script`. Returns the breakpoints that were not set in any script before.
   */
  scriptLoaded(script: Debugger.Script): DebugProtocol.Breakpoint[] {
    const changed: DebugProtocol.Breakpoint[] = [];
    let scripts: Debugger.Script[] | undefined;
    for (const [clientPath, entries] of this.byPath) {
      if (!this.sources.matches(script.url, clientPath)) {
        continue;
      }
      scripts ??= this.dbg.findScripts({ url: script.url, global: script.global });
      for (const entry of entries) {
        const wasBound = entry.bound !== undefined;
        if (this.bind(entry, scripts) && !wasBound) {
          changed.push(describe(entry));
        }
      }
    }
    return changed;
  }

  /** The positions on lines `line` through `endLine` of `clientPath` a breakpoint can be set at. */
  locations(clientPath: string, line: number, endLine = line): DebugProtocol.BreakpointLocation[] {
    const scripts = this.urlsFor(clientPath).flatMap((url) => this.dbg.findScripts({ url }));
    const seen = new Set<string>();
    const locations: DebugProtocol.BreakpointLocation[] = [];
    for (const { position } of this.candidates(scripts, line, endLine)) {
      const key = `${position.lineNumber}:${position.columnNumber}`;
      if (!seen.has(key)) {
        seen.add(key);
        locations.push({ line: position.lineNumber, column: position.columnNumber });
      }
    }
    return locations;
  }

  clear(): void {
    for (const entries of this.byPath.values()) {
      for (const entry of entries) {
        this.unbind(entry);
      }
    }
    this.byPath.clear();
  }

  private create(requested: DebugProtocol.SourceBreakpoint): Entry {
    const id = this.nextId++;
    return {
      id,
      line: requested.line,
      column: requested.column,
      handler: {
        hit: (frame) => {
          this.onHit(frame, id);
          return undefined;
        },
      },
      bound: undefined,
      urls: new Set(),
    };
  }

  private urlsFor(clientPath: string): string[] {
    return this.sources.loadedUrls().filter((url) => this.sources.matches(url, clientPath));
  }

  /**
   * Set the breakpoint in `scripts` at the first position on the first line at or after its line
   * that has code, preferring its requested column on that line. Each script gets the breakpoint
   * at most once. Returns whether any script has code there.
   */
  private bind(entry: Entry, scripts: Debugger.Script[]): boolean {
    const candidates = this.candidates(scripts, entry.line, entry.line + MAX_LINE_SLIDE);
    if (candidates.length === 0) {
      return false;
    }
    const line = candidates[0].position.lineNumber;
    const onLine = candidates.filter((candidate) => candidate.position.lineNumber === line);
    const column = (
      onLine.find((candidate) => candidate.position.columnNumber === entry.column) ?? onLine[0]
    ).position.columnNumber;
    for (const { script, position } of onLine) {
      if (
        position.columnNumber === column &&
        !script.getBreakpoints(position.offset).includes(entry.handler)
      ) {
        script.setBreakpoint(position.offset, entry.handler);
        entry.urls.add(script.url);
      }
    }
    entry.bound ??= { line, column };
    return true;
  }

  /** Remove the breakpoint from every script it is set in, which are found by URL. */
  private unbind(entry: Entry): void {
    for (const url of entry.urls) {
      for (const script of this.dbg.findScripts({ url })) {
        script.clearBreakpoint(entry.handler);
      }
    }
    entry.urls.clear();
    entry.bound = undefined;
  }

  /** Breakpoint positions on lines `minLine` through `maxLine` of `scripts`, in source order. */
  private candidates(scripts: Debugger.Script[], minLine: number, maxLine: number): Candidate[] {
    const found: Candidate[] = [];
    for (const script of scripts) {
      // `maxLine` is exclusive.
      for (const position of script.getPossibleBreakpoints({ minLine, maxLine: maxLine + 1 })) {
        found.push({ script, position });
      }
    }
    return found.sort(
      (a, b) =>
        a.position.lineNumber - b.position.lineNumber ||
        a.position.columnNumber - b.position.columnNumber,
    );
  }
}

function describe(entry: Entry): DebugProtocol.Breakpoint {
  if (entry.bound === undefined) {
    return { id: entry.id, verified: false, line: entry.line, message: "No loaded code at this line" };
  }
  return { id: entry.id, verified: true, line: entry.bound.line, column: entry.bound.column };
}
