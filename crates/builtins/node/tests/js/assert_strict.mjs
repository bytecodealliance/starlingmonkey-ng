// `node:assert/strict` imported without `node:assert`.

import strict, { strict as strictExport } from "node:assert/strict";
import { eq } from "./harness.mjs";

eq(strict.strict, strict, "strict.strict is the module's default export");
eq(strictExport, strict, "the strict export is the module's default export");
strictExport(true);
