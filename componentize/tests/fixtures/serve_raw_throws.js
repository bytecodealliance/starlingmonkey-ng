// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application whose own `handle` throws an `Error`, which the
// `error-code` of `wasi:http/handler` cannot hold.

export function handle() {
    throw new Error("boom from handle")
}
