// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve guest implementing `handle` as a bare export, forwarding the WIT
// request with `fetch`.
import type * as Guest from 'starling:guest';

export async function handle(request: Parameters<typeof Guest.handle>[0]): Promise<Response> {
  return await fetch(request);
}
handle satisfies typeof Guest.handle;
