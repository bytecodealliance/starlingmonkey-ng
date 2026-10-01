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
  bound: { script: Debugger.Script; line: number; column: number } | undefined;
}

/** Line breakpoints, keyed by client path. A breakpoint binds once a matching script loads. */
export class Breakpoints {
  private nextId = 1;
  private readonly byPath = new Map<string, Entry[]>();

  constructor(
    private readonly dbg: Debugger,
    private readonly sources: Sources,
    private readonly onHit: (frame: Debugger.Frame, id: number) => void,
  ) {}

  /** Replace the breakpoints in `clientPath`, binding each to loaded code where there is some. */
  set(clientPath: string, requested: DebugProtocol.SourceBreakpoint[]): DebugProtocol.Breakpoint[] {
    for (const entry of this.byPath.get(clientPath) ?? []) {
      this.unbind(entry);
    }
    const entries = requested.map((breakpoint) => this.create(breakpoint));
    this.byPath.set(clientPath, entries);
    const urls = this.urlsFor(clientPath);
    for (const entry of entries) {
      this.bind(entry, urls);
    }
    return entries.map((entry) => describe(entry));
  }

  /** Bind unbound breakpoints to the scripts of a newly loaded `url`. Returns those it bound. */
  scriptLoaded(url: string): DebugProtocol.Breakpoint[] {
    const changed: DebugProtocol.Breakpoint[] = [];
    for (const [clientPath, entries] of this.byPath) {
      if (!this.sources.matches(url, clientPath)) {
        continue;
      }
      for (const entry of entries) {
        if (entry.bound === undefined && this.bind(entry, [url])) {
          changed.push(describe(entry));
        }
      }
    }
    return changed;
  }

  /** The positions on lines `line` through `endLine` of `clientPath` a breakpoint can bind to. */
  locations(clientPath: string, line: number, endLine = line): DebugProtocol.BreakpointLocation[] {
    return this.candidates(this.urlsFor(clientPath), line, endLine).map(({ position }) => ({
      line: position.lineNumber,
      column: position.columnNumber,
    }));
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
    };
  }

  private urlsFor(clientPath: string): string[] {
    return this.sources.loadedUrls().filter((url) => this.sources.matches(url, clientPath));
  }

  /**
   * Set the breakpoint at the first position on the first line at or after its line that has
   * code, preferring its requested column on that line.
   */
  private bind(entry: Entry, urls: string[]): boolean {
    const candidates = this.candidates(urls, entry.line, entry.line + MAX_LINE_SLIDE);
    if (candidates.length === 0) {
      return false;
    }
    const line = candidates[0].position.lineNumber;
    const onLine = candidates.filter((candidate) => candidate.position.lineNumber === line);
    const chosen =
      onLine.find((candidate) => candidate.position.columnNumber === entry.column) ?? onLine[0];
    chosen.script.setBreakpoint(chosen.position.offset, entry.handler);
    entry.bound = {
      script: chosen.script,
      line: chosen.position.lineNumber,
      column: chosen.position.columnNumber,
    };
    return true;
  }

  private unbind(entry: Entry): void {
    entry.bound?.script.clearBreakpoint(entry.handler);
    entry.bound = undefined;
  }

  /** Breakpoint positions on lines `minLine` through `maxLine`, in source order. */
  private candidates(urls: string[], minLine: number, maxLine: number): Candidate[] {
    const found: Candidate[] = [];
    for (const url of urls) {
      for (const script of this.dbg.findScripts({ url })) {
        // `maxLine` is exclusive.
        for (const position of script.getPossibleBreakpoints({ minLine, maxLine: maxLine + 1 })) {
          found.push({ script, position });
        }
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
