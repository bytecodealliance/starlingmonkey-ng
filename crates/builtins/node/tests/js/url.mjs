// `node:url`.

import url, { fileURLToPath, pathToFileURL, URL as UrlExport } from "node:url";
import { deepEq, eq, throws } from "./harness.mjs";

eq(UrlExport, URL, "URL is the global's");
eq(url.URLSearchParams, URLSearchParams, "URLSearchParams is the global's");

eq(fileURLToPath("file:///a/b%20c.txt"), "/a/b c.txt", "fileURLToPath");
eq(fileURLToPath(new URL("file:///x/y")), "/x/y", "fileURLToPath(URL)");
eq(fileURLToPath(import.meta.url).endsWith("/url.mjs"), true, "fileURLToPath(import.meta.url)");
throws(() => fileURLToPath("http://x/y"), "ERR_INVALID_URL_SCHEME", "non-file URL");
throws(() => fileURLToPath("file://host/y"), "ERR_INVALID_FILE_URL_HOST", "file URL with a host");
const encodedSlash = throws(() => fileURLToPath("file:///a%2Fb"), "ERR_INVALID_FILE_URL_PATH", "encoded slash");
eq(encodedSlash.input instanceof URL && encodedSlash.input.href, "file:///a%2Fb", "the encoded slash error's input is the URL");
const notUrl = throws(() => fileURLToPath("not a url"), "ERR_INVALID_URL", "invalid URL");
eq(notUrl.input, "not a url", "the invalid URL error's input");
eq(String(notUrl), "TypeError: Invalid URL", "fileURLToPath throws the URL constructor's error");
eq(pathToFileURL("/x/a[b]c^d|e~f\"g").href, "file:///x/a%5Bb%5Dc%5Ed%7Ce%7Ef%22g", "pathToFileURL encodes Node's unsafe characters");
const fileUrl = pathToFileURL("/tmp/a b#c?d%e");
eq(fileUrl instanceof URL, true, "pathToFileURL returns a URL");
eq(fileUrl.href, "file:///tmp/a%20b%23c%3Fd%25e", "pathToFileURL encodes");
eq(pathToFileURL("/dir/").href, "file:///dir/", "trailing slash is kept");
eq(fileURLToPath(pathToFileURL("/tmp/a b#c")), "/tmp/a b#c", "round trip");

const parsed = url.parse("https://user:pw@Example.COM:8080/p/a/t/h?query=string#hash", true);
eq(parsed.protocol, "https:", "protocol");
eq(parsed.slashes, true, "slashes");
eq(parsed.auth, "user:pw", "auth");
eq(parsed.host, "example.com:8080", "host");
eq(parsed.hostname, "example.com", "hostname");
eq(parsed.port, "8080", "port");
eq(parsed.pathname, "/p/a/t/h", "pathname");
eq(parsed.search, "?query=string", "search");
deepEq({ ...parsed.query }, { query: "string" }, "query object");
eq(parsed.hash, "#hash", "hash");
eq(parsed.path, "/p/a/t/h?query=string", "path");
eq(parsed.href, "https://user:pw@example.com:8080/p/a/t/h?query=string#hash", "href");

const relative = url.parse("/foo/bar?x=1");
eq(relative.protocol, null, "relative protocol");
eq(relative.host, null, "relative host");
eq(relative.pathname, "/foo/bar", "relative pathname");
eq(relative.query, "x=1", "query string");
eq(relative.href, "/foo/bar?x=1", "relative href");
eq(url.parse("http://example.com").pathname, "/", "empty path is /");
eq(url.parse("//host/p", false, true).host, "host", "slashesDenoteHost");
eq(url.parse("//host/p").host, null, "no host without slashesDenoteHost");
const long = url.parse(`http://[${"a".repeat(300)}]/p`);
deepEq([long.host, long.hostname, long.pathname, long.href], ["", "", "/p", "http:///p"], "a bracketed host over 255 characters");

eq(url.format({ protocol: "https", hostname: "example.com", pathname: "/x", query: { a: 1 } }), "https://example.com/x?a=1", "format(object)");
eq(url.format(new URL("http://a.com/b?c")), "http://a.com/b?c", "format(URL)");
eq(url.format({ protocol: "http:", host: "x", pathname: "/p", href: "http://y/z" }), "http://x/p", "format formats an object with an href from its fields");
eq(url.resolve("/one/two/three", "four"), "/one/two/four", "resolve paths");
eq(url.resolve("http://example.com/", "/one"), "http://example.com/one", "resolve absolute");
eq(url.resolve("http://example.com/one", "/two"), "http://example.com/two", "resolve root-relative");
eq(url.domainToASCII("español.com"), "xn--espaol-zwa.com", "domainToASCII");
eq(url.domainToUnicode("xn--espaol-zwa.com"), "español.com", "domainToUnicode");
const options = url.urlToHttpOptions(new URL("https://a:b@[::1]:81/p?q"));
eq(options.hostname, "::1", "urlToHttpOptions hostname");
eq(options.port, 81, "urlToHttpOptions port");
eq(options.path, "/p?q", "urlToHttpOptions path");
eq(options.auth, "a:b", "urlToHttpOptions auth");

eq(throws(() => url.parse("http://\uff03"), "ERR_INVALID_URL", "url.parse of an invalid host").input, "http://\uff03", "url.parse's error has the input");
{
  const resolved = url.resolveObject("http://a/b?x=1#h", { slashes: true, host: "c", pathname: "/d" });
  deepEq([resolved.hostname, resolved.search, resolved.path, resolved.href], ["a", "?x=1", "/b?x=1", "http://c/d?x=1"], "resolveObject keeps the fields the relative object lacks");
  const mailto = url.resolveObject("http://a/b?x=1#h", { protocol: "mailto:", pathname: "x@y" });
  deepEq([mailto.href, mailto.slashes, mailto.host], ["mailto://a/x@y?x=1", true, "a"], "resolveObject with only a protocol and a pathname");
}

// `Url.prototype.parse` keeps the fields a parse doesn't assign.
{
  const parsed = new url.Url();
  parsed.parse("http://a/b");
  parsed.parse("/x");
  deepEq([parsed.protocol, parsed.host, parsed.pathname, parsed.href], ["http:", "a", "/x", "/x"], "re-parse");
}
throws(() => url.resolveObject("http://a/b", undefined), TypeError, "resolveObject of undefined");
throws(() => url.resolveObject("http://a/b", null), TypeError, "resolveObject of null");
deepEq([url.parse.name, url.format.name, url.resolve.name, url.resolveObject.name], ["urlParse", "urlFormat", "urlResolve", "urlResolveObject"], "legacy function names");
eq(
  Object.keys(url).join(),
  "Url,parse,resolve,resolveObject,format,URL,URLPattern,URLSearchParams,domainToASCII,domainToUnicode,pathToFileURL,fileURLToPath,fileURLToPathBuffer,urlToHttpOptions",
  "key order",
);
throws(() => fileURLToPath({ href: "file:///a", protocol: "file:" }), "ERR_INVALID_FILE_URL_HOST", "fileURLToPath reads hostname");
eq(fileURLToPath({ href: "file:///a", protocol: "file:", hostname: "", pathname: "/b" }), "/b", "fileURLToPath reads pathname");
deepEq(
  [null, true, { valueOf() { return 7; }, toString() { return "x"; } }].map((port) => url.urlToHttpOptions({ port }).port),
  [0, 1, 7],
  "urlToHttpOptions converts port with Number",
);
eq(url.domainToASCII(undefined), "undefined", "domainToASCII(undefined)");
eq(url.domainToUnicode(undefined), "undefined", "domainToUnicode(undefined)");
throws(() => url.domainToASCII(), "ERR_MISSING_ARGS", "domainToASCII()");
throws(() => url.format(new URL("http://a/"), []), "ERR_INVALID_ARG_TYPE", "format rejects array options");
throws(() => url.format(new URL("http://a/"), () => {}), "ERR_INVALID_ARG_TYPE", "format rejects function options");
eq(url.format({ protocol: "http", host: "h", auth: "a:b:c", pathname: "p#q", search: "s#" }), "http://a:b:c@h/p%23q?s%23", "format");
