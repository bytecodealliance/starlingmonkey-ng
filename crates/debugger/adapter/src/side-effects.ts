// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

/** How long an evaluation without side effects may run, in milliseconds. */
const TIME_LIMIT = 100;

/** How many steps run between two checks of the time limit. */
const STEPS_PER_CHECK = 256;

/** The handler of the breakpoints at effectful bytecodes, which terminates the evaluation. */
const TERMINATE: Debugger.BreakpointHandler = { hit: () => null };

/**
 * Evaluate `expression` in `frame`, or in `global` without a frame, and terminate the evaluation
 * when it reaches a bytecode that can have a side effect outside its frame, when it calls a native
 * function, or once it ran for `TIME_LIMIT` milliseconds. A terminated evaluation completes with
 * `null`.
 *
 * The evaluation runs with its own `Debugger`, and the hooks of other debuggers are not called
 * while it runs.
 */
export function evaluateWithoutSideEffects(
  expression: string,
  frame: Debugger.Frame | undefined,
  global: object,
): CompletionValue {
  const dbg = new Debugger();
  dbg.exclusiveDebuggerOnEval = true;
  dbg.shouldAvoidSideEffects = true;
  const checked = new Set<Debugger.Script>();
  try {
    dbg.addAllGlobalsAsDebuggees();
    const debuggeeGlobal = dbg.addDebuggee(global);
    const deadline = Date.now() + TIME_LIMIT;
    let steps = 0;
    const outOfTime = () => ++steps % STEPS_PER_CHECK === 0 && Date.now() > deadline;
    dbg.onEnterFrame = (entered) => {
      if (outOfTime()) {
        return null;
      }
      const script = entered.script;
      if (script !== null && !checked.has(script)) {
        checked.add(script);
        for (const offset of script.getEffectfulOffsets()) {
          script.setBreakpoint(offset, TERMINATE);
        }
      }
      entered.onStep = () => (outOfTime() ? null : undefined);
      return undefined;
    };
    dbg.onNativeCall = () => null;
    return frame !== undefined
      ? dbg.adoptFrame(frame).eval(expression)
      : debuggeeGlobal.executeInGlobal(expression);
  } finally {
    dbg.onEnterFrame = undefined;
    dbg.onNativeCall = undefined;
    for (const script of checked) {
      script.clearBreakpoint(TERMINATE);
    }
    dbg.removeAllDebuggees();
  }
}
