// The parts of `node:http`, `node:net` and `node:tty` that do no I/O.

import http, { METHODS, STATUS_CODES, validateHeaderName, validateHeaderValue } from "node:http";
import { isIP, isIPv4, isIPv6 } from "node:net";
import tty, { isatty } from "node:tty";
import { eq, throws } from "./harness.mjs";

eq(http.METHODS, METHODS, "default export");
eq(METHODS.includes("GET") && METHODS.includes("M-SEARCH"), true, "METHODS");
eq(METHODS.length, 35, "every method");
eq(STATUS_CODES[404], "Not Found", "STATUS_CODES");
eq(STATUS_CODES[418], "I'm a Teapot", "STATUS_CODES 418");
validateHeaderName("x-custom");
throws(() => validateHeaderName("bad name"), "ERR_INVALID_HTTP_TOKEN", "header names are tokens");
throws(() => validateHeaderName(""), "ERR_INVALID_HTTP_TOKEN", "header names aren't empty");
validateHeaderValue("x-custom", "value\twith tab");
throws(() => validateHeaderValue("x-custom", undefined), "ERR_HTTP_INVALID_HEADER_VALUE", "header values are defined");
throws(() => validateHeaderValue("x-custom", "line\nbreak"), "ERR_INVALID_CHAR", "header values have no control characters");

eq(isIP("127.0.0.1"), 4, "isIP v4");
eq(isIP("::1"), 6, "isIP v6");
eq(isIP("fe80::1%eth0"), 6, "isIP with a zone ID");
eq(isIP("127.000.000.001"), 0, "leading zeros");
eq(isIP("not an ip"), 0, "isIP neither");
eq(isIPv4("10.0.0.256"), false, "isIPv4 range");
eq(isIPv6("2001:db8::ff00:42:8329"), true, "isIPv6");
eq(isIPv6("::ffff:192.0.2.1"), true, "isIPv6 with an embedded IPv4 address");
eq(isIPv6("fe80::1%"), false, "an empty zone ID");

eq(isatty(99), false, "file descriptors other than the standard streams aren't terminals");
eq(typeof isatty(1), "boolean", "isatty");
eq(isatty(Symbol()), false, "isatty of a symbol is false without converting it");
eq(isatty({ valueOf() { throw new Error("converted"); } }), false, "isatty doesn't convert objects");
for (const fd of [undefined, -1, 1.5, "0", 2 ** 31]) {
  throws(() => new tty.ReadStream(fd), "ERR_INVALID_FD", `ReadStream(${String(fd)})`);
}
