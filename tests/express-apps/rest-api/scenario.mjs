export const entry = 'app.js';

export default async function (t) {
  await t.get('/health');
  await t.get('/api/v1/todos');
  const created = [];
  for (const [title, priority, tags] of [
    ['Write report', 2, ['work']],
    ['Buy milk', 4, ['home', 'shopping']],
    ['Plan trip', 1, ['home']],
    ['Review PR', 3, ['work', 'code']],
  ]) {
    const r = await t.post('/api/v1/todos', { json: { title, priority, tags } });
    created.push(r.json);
  }
  await t.post('/api/v1/todos', { json: { title: '', priority: 9, tags: ['BAD'] }, name: 'invalid' });
  await t.post('/api/v1/todos', { body: '{"title": ', headers: { 'content-type': 'application/json' }, name: 'malformed' });
  await t.post('/api/v1/todos', { json: { title: 'x'.repeat(20000) }, name: 'too large' });
  await t.get('/api/v1/todos?tag=home&sort=-priority');
  await t.get('/api/v1/todos?q=re&limit=1&offset=1');
  await t.get('/api/v1/todos?sort=title&limit=2');
  await t.get('/api/v1/todos?limit=0', { name: 'invalid query' });
  const id = created[1].id;
  await t.get(`/api/v1/todos/${id}`);
  await t.get(`/api/v1/TODOS/${id}`, { name: 'case-insensitive' });
  await t.get('/api/v1/todos/not-a-uuid');
  await t.request('PATCH', `/api/v1/todos/${id}`, { json: { done: true, title: 'Buy oat milk' } });
  const etagRes = await t.get(`/api/v1/todos/${id}`, { name: 'after patch' });
  await t.get(`/api/v1/todos/${id}`, { headers: { 'if-none-match': etagRes.headers.get('etag') }, name: 'conditional' });
  await t.request('DELETE', `/api/v1/todos/${created[0].id}`);
  await t.request('DELETE', `/api/v1/todos/${created[0].id}`, { name: 'again' });
  await t.get('/api/v1/todos?sort=seq');
  await t.get('/api/v1/big', { headers: { 'accept-encoding': 'gzip' }, name: 'gzip' });
  await t.get('/api/v1/big', { headers: { 'accept-encoding': 'deflate' }, name: 'deflate' });
  await t.get('/api/v1/big', { headers: { 'accept-encoding': 'identity' }, name: 'identity' });
  await t.request('OPTIONS', '/api/v1/todos', {
    headers: {
      origin: 'https://app.example.com',
      'access-control-request-method': 'PATCH',
      'access-control-request-headers': 'content-type,x-request-id',
    },
    headers_recorded: [
      'access-control-allow-origin',
      'access-control-allow-methods',
      'access-control-allow-headers',
      'access-control-allow-credentials',
      'access-control-max-age',
    ],
    name: 'preflight',
  });
  await t.get('/api/v1/todos?limit=1', {
    headers: { origin: 'https://app.example.com', 'x-request-id': 'abc-123' },
    headers_recorded: ['access-control-allow-origin', 'access-control-expose-headers', 'x-request-id', 'x-total-count'],
    name: 'cors',
  });
  await t.get('/api/v1/todos?limit=1', {
    headers: { origin: 'https://evil.test' },
    headers_recorded: ['access-control-allow-origin'],
    name: 'cors denied',
  });
  await t.get('/health', {
    headers_recorded: [
      'content-security-policy',
      'strict-transport-security',
      'x-content-type-options',
      'x-frame-options',
      'referrer-policy',
      'cross-origin-opener-policy',
      'cross-origin-resource-policy',
      'x-dns-prefetch-control',
      'x-download-options',
      'x-permitted-cross-domain-policies',
      'x-xss-protection',
      'origin-agent-cluster',
    ],
    name: 'helmet',
  });
  for (let i = 0; i < 5; i++) {
    await t.get('/api/v1/limited', { headers_recorded: ['ratelimit', 'ratelimit-policy', 'retry-after'] });
  }
  // Spin's `wasi:http` requests have no client address, so `req.ip` is undefined there.
  const noClientAddress = { spin: 'no client address' };
  await t.post('/api/v1/echo?a=1&b=2&a=3&nested[x]=y', {
    json: { hello: 'world', n: [1, 2, 3] },
    headers: { accept: 'text/html,application/json;q=0.9', 'x-requested-with': 'XMLHttpRequest' },
    ignoreIn: noClientAddress,
  });
  await t.post('/api/v1/echo', {
    body: 'just text',
    headers: { 'content-type': 'text/plain; charset=utf-8' },
    ignoreIn: noClientAddress,
  });
  await t.get('/api/v1/search?q=%E2%9C%93+ok&list=1&list=2&empty=');
  for (const accept of ['text/plain', 'text/html', 'application/json', 'image/png', '*/*']) {
    await t.get('/api/v1/format', { headers: { accept }, name: accept });
  }
  await t.get('/api/v1/throws', { mask: [[/"error":"[^"]*"/, '"error":"<msg>"']] });
  await t.get('/api/v1/rejects');
  await t.get('/api/v1/uuid');
  for (const method of ['GET', 'POST', 'PUT', 'DELETE', 'PATCH']) {
    await t.request(method, '/api/v1/method');
  }
  await t.request('HEAD', '/api/v1/method', { name: 'head' });
  await t.get('/nope');
  await t.get('/api/v1/stats');
  await t.get('/api/v1/log');
}
