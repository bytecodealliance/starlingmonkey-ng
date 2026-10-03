// `node:os`.

import os, { EOL, constants } from "node:os";
import { eq } from "./harness.mjs";

eq(EOL, "\n", "EOL");
eq(os.platform(), process.platform, "platform");
eq(os.arch(), process.arch, "arch");
eq(typeof os.type(), "string", "type");
eq(os.endianness(), "LE", "endianness");
eq(os.tmpdir().endsWith("/"), false, "tmpdir has no trailing slash");
eq(typeof os.homedir(), "string", "homedir");
eq(os.cpus().length, os.availableParallelism(), "one cpu entry per processor");
eq(os.availableParallelism() >= 1, true, "availableParallelism");
eq(os.totalmem() > 0 && os.freemem() > 0, true, "memory");
eq(os.loadavg().length, 3, "loadavg");
eq(typeof os.networkInterfaces(), "object", "networkInterfaces");
eq(constants.signals.SIGTERM, 15, "signals");
eq(constants.errno.ENOENT, 2, "errno");
eq(typeof os.userInfo().username, "string", "userInfo");
eq(os.devNull, "/dev/null", "devNull");
const originalTmp = process.env.TMPDIR;
const originalHome = process.env.HOME;
process.env.TMPDIR = "/custom/tmp/";
process.env.HOME = "/custom/home";
eq(os.tmpdir(), "/custom/tmp", "tmpdir reads process.env");
eq(os.homedir(), "/custom/home", "homedir reads process.env");
if (originalTmp === undefined) delete process.env.TMPDIR;
else process.env.TMPDIR = originalTmp;
process.env.HOME = originalHome;
