// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

import tseslint from "typescript-eslint";

const synchronous = "The adapter must stay synchronous: a pause runs no microtasks.";

export default [
  ...tseslint.configs.recommended,
  {
    rules: {
      "no-restricted-syntax": [
        "error",
        { selector: ":function[async=true]", message: synchronous },
        { selector: "AwaitExpression", message: synchronous },
        { selector: "NewExpression[callee.name='Promise']", message: synchronous },
        { selector: "MemberExpression[object.name='Promise']", message: synchronous },
      ],
    },
  },
];
