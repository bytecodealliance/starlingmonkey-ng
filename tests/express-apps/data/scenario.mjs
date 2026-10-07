import http from 'node:http';
import { serveUpstream } from './upstream.mjs';

export const entry = 'app.mjs';

// Under Spin, the upstream server is a component the app reaches at upstream.spin.internal.
export const spinMasks = [[/upstream\.spin\.internal/g, '127.0.0.1']];

// An upstream server under Node.js for the proxy and fetch routes.
export async function setup() {
  const server = http.createServer(serveUpstream);
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return {
    env: { UPSTREAM_URL: `http://127.0.0.1:${server.address().port}` },
    teardown: () => new Promise((r) => server.close(r)),
  };
}

// Reads an event stream to its end, and calls `onSubscribed` once the stream has the `subscribed`
// comment.
async function readEvents(res, onSubscribed) {
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let text = '';
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    text += decoder.decode(value, { stream: true });
    if (onSubscribed && text.includes('\n: subscribed\n')) {
      onSubscribed();
      onSubscribed = null;
    }
  }
  return text;
}

export default async function (t) {
  const id = (n) => ({ 'x-request-id': `id-${n}` });
  await t.get('/config');
  await t.get('/posts', { headers: id(1) });
  for (const [i, title] of ['First', 'Second', 'Third', 'Fourth'].entries()) {
    await t.post('/posts', { json: { title, body: `body ${i}` }, headers: id(10 + i) });
  }
  await t.post('/posts', { json: { nope: true }, headers: id(20), name: 'invalid' });
  await t.get('/posts?page=2', { headers: id(2) });
  await t.request('DELETE', '/posts/2', { headers: id(3) });
  await t.request('DELETE', '/posts/2', { headers: id(4), name: 'again' });
  await t.get('/db-file', { headers: id(5) });

  // Server-sent events while posts are created concurrently, after the server subscribed to them.
  let subscribed;
  const ready = new Promise((r) => (subscribed = r));
  const sse = t.get('/events?limit=6', { headers: id(30), stream: (res) => readEvents(res, subscribed), slot: true });
  await Promise.race([ready, sse]);
  await t.post('/posts', { json: { title: 'Live 1' }, headers: id(31) });
  await t.post('/posts', { json: { title: 'Live 2' }, headers: id(32) });
  await t.post('/posts', { json: { title: 'Live 3' }, headers: id(33) });
  await sse;
  await t.get('/ticks', { headers: id(34), stream: readEvents });

  await t.get('/export.ndjson', { headers: id(40) });
  await t.post('/import.ndjson', {
    body: '{"title":"imp 1","body":"a"}\n\n{"title":"imp 2"}\nnot json\n{"title":"imp 3"}\n',
    headers: { ...id(41), 'content-type': 'application/x-ndjson' },
  });
  await t.get('/posts?page=3', { headers: id(42) });

  // Proxy routes.
  await t.get('/proxy/json?q=1&r=two', { headers: id(50), headers_recorded: ['x-upstream', 'x-proxy-status'] });
  await t.post('/proxy/echo?z=9', { json: { proxied: true, n: [1, 2] }, headers: id(51) });
  await t.post('/proxy/echo', { body: 'x'.repeat(100_000), headers: { ...id(52), 'content-type': 'text/plain' }, name: 'large body' });
  await t.get('/proxy/slow', { headers: id(53) });
  await t.get('/proxy/redirect', { headers: id(54) });
  await t.get('/proxy/missing', { headers: id(55) });
  await t.get('/classic/json?c=1', { headers: id(56), headers_recorded: ['x-upstream', 'x-classic'] });
  await t.post('/classic/echo', { json: { classic: true }, headers: id(57) });
  await t.get('/classic/slow', { headers: id(58) });
  await t.get('/classic-down/x', { headers: id(59) });

  // Under Spin, the upstream's host name, which the body holds, has another length.
  await t.get('/aggregate', { headers: id(60), ignoreHeaders: t.ctx.mode === 'spin' ? ['content-length', 'etag'] : [] });
  await t.get('/fetch-stream', { headers: id(61) });
  await t.get('/fetch-error', { headers: id(62) });
  await t.get('/logs', { headers: id(70) });
}
