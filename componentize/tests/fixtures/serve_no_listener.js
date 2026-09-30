// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// A serve application that neither implements `wasi:http/handler` nor registers
// a `fetch` listener. Its component could not serve any request, so
// componentizing it fails.

globalThis.__ready = true;
