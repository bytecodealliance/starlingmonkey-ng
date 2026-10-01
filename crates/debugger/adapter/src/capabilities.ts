// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

import type { DebugProtocol } from "@vscode/debugprotocol";

/** The capabilities the `initialize` response advertises. */
export const CAPABILITIES: DebugProtocol.Capabilities = {
  supportsConfigurationDoneRequest: true,
  supportsTerminateRequest: true,
  supportTerminateDebuggee: true,
  supportsBreakpointLocationsRequest: true,
  supportsSetVariable: true,
  supportsEvaluateForHovers: true,
  supportsLoadedSourcesRequest: true,
};
