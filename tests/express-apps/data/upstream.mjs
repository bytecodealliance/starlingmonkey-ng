// The upstream server of the proxy and fetch routes: under Node.js in the scenario, and as a
// component of the Spin application `scripts/build-spin-app.sh` builds.
import http from 'node:http';

export async function serveUpstream(req, res) {
  const url = new URL(req.url, 'http://upstream');
  const chunks = [];
  for await (const c of req) chunks.push(c);
  const body = Buffer.concat(chunks).toString();
  if (url.pathname === '/upstream/json') {
    res.writeHead(200, { 'content-type': 'application/json', 'x-upstream': 'yes' });
    res.end(JSON.stringify({ query: Object.fromEntries(url.searchParams), host: req.headers.host.replace(/:\d+$/, '') }));
  } else if (url.pathname === '/upstream/echo') {
    res.writeHead(201, { 'content-type': 'application/json' });
    res.end(
      JSON.stringify({
        method: req.method,
        path: url.pathname + url.search,
        body,
        contentType: req.headers['content-type'] ?? null,
        proxiedBy: req.headers['x-proxied-by'] ?? null,
        forwardedHost: req.headers['host'].replace(/:\d+$/, ''),
      }),
    );
  } else if (url.pathname === '/upstream/slow') {
    res.writeHead(200, { 'content-type': 'text/plain' });
    for (let i = 0; i < 5; i++) {
      res.write(`part ${i}\n`);
      await new Promise((r) => setTimeout(r, 5));
    }
    res.end('done\n');
  } else if (url.pathname === '/upstream/redirect') {
    res.writeHead(302, { location: '/upstream/json?redirected=1' });
    res.end();
  } else {
    res.writeHead(404, { 'content-type': 'text/plain' });
    res.end(`upstream 404 ${url.pathname}`);
  }
}

if (import.meta.main) {
  http.createServer(serveUpstream).listen(Number(process.env.PORT ?? 3000));
}
