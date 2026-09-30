// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The wit-dir world guest module.
//
// The two versions of `local:hello/hello` are distinct modules, imported under
// their full specifiers. The two exported versions share every other export
// name, so each is exported under its full WIT name. The inline `exports`
// interface is in the interface layer, and the world-level `make-bar` is a
// bare export.

import { hello as hello1 } from "local:hello/hello@1.0.0";
import { hello as hello2 } from "local:hello/hello@2.0.0";
import { hello as helloOther } from "local:other/hello";

const helloV1 = {
  hello: (name) => hello1(name),
};

const helloV2 = {
  hello: (name) => hello2(name),
};

export { helloV1 as "local:hello/hello@1.0.0", helloV2 as "local:hello/hello@2.0.0" };

export const exports = {
  hello(name) {
    if (name === "hello") {
      return `world ${name} (${hello1("world")})`;
    }
    if (name === "other") {
      return `world ${name} (${helloOther("world")})`;
    }
    return `world unknown ${name}`;
  },
};

export function makeBar() {
  return { thing: 5 };
}
