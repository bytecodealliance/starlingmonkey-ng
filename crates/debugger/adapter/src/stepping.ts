// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

export type StepKind = "next" | "stepIn" | "stepOut";

/**
 * Steps with `onStep`, `onPop` and `onEnterFrame` hooks. A step stops at the first breakpoint
 * position on a line other than the one the frame was on when the hook was set. A frame that
 * suspends at `await` or `yield` keeps its hooks and continues the step when it resumes.
 */
export class Stepper {
  private readonly hooked = new Set<Debugger.Frame>();

  constructor(
    private readonly dbg: Debugger,
    private readonly onStop: (frame: Debugger.Frame) => void,
  ) {}

  start(kind: StepKind, frame: Debugger.Frame): void {
    this.clear();
    if (kind !== "stepOut") {
      this.stopOnNewLine(frame);
    }
    this.continueInCaller(frame);
    if (kind === "stepIn") {
      this.dbg.onEnterFrame = (entered) => {
        this.stop(entered);
        return undefined;
      };
    }
  }

  /** Remove every hook a step set. */
  clear(): void {
    for (const frame of this.hooked) {
      if (!frame.terminated) {
        frame.onStep = undefined;
        frame.onPop = undefined;
      }
    }
    this.hooked.clear();
    this.dbg.onEnterFrame = undefined;
  }

  private stopOnNewLine(frame: Debugger.Frame): void {
    this.hooked.add(frame);
    const startLine = lineOf(frame);
    frame.onStep = () => {
      const position = frame.script?.getOffsetMetadata(frame.offset);
      if (position !== undefined && position.isBreakpoint && position.lineNumber !== startLine) {
        this.stop(frame);
      }
      return undefined;
    };
  }

  /** When `frame` returns or throws, continue the step in its caller. */
  private continueInCaller(frame: Debugger.Frame): void {
    this.hooked.add(frame);
    frame.onPop = (completion: CompletionValue) => {
      if (completion !== null && "return" in completion && (completion.await || completion.yield)) {
        return undefined;
      }
      const caller = frame.older;
      if (caller !== null) {
        this.stopOnNewLine(caller);
        this.continueInCaller(caller);
      }
      return undefined;
    };
  }

  private stop(frame: Debugger.Frame): void {
    this.clear();
    this.onStop(frame);
  }
}

function lineOf(frame: Debugger.Frame): number | undefined {
  return frame.script?.getOffsetMetadata(frame.offset).lineNumber;
}
