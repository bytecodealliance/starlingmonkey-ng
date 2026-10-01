// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

import type { DebugProtocol } from "@vscode/debugprotocol";
import { Adapter } from "./adapter";

const adapter = new Adapter(new Debugger(), contentGlobal);

(globalThis as unknown as { handleMessage(json: string): void }).handleMessage = (json) => {
  adapter.handle(JSON.parse(json) as DebugProtocol.ProtocolMessage);
};
