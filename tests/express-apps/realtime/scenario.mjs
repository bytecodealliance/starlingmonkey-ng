export const entry = 'app.js';

// Served requests have no socket to upgrade to a WebSocket.
export const modes = ['native', 'wasm'];

// The value of the session cookie, which signs the user name.
export const masks = [[/chat(\.sig)?=[^;\s]+/g, 'chat$1=<cookie>']];

// A WebSocket client that collects every event as a string.
function connect(t, path, cookie) {
  const url = t.base.replace('http:', 'ws:') + path;
  const ws = new WebSocket(url, { headers: cookie ? { cookie } : {} });
  ws.binaryType = 'arraybuffer';
  const events = [];
  const waiters = [];
  const push = (event) => {
    events.push(event);
    for (const w of waiters.splice(0)) w();
  };
  ws.onopen = () => push('open');
  ws.onmessage = (e) =>
    push(typeof e.data === 'string' ? `text ${e.data}` : `binary ${Buffer.from(e.data).toString()}`);
  ws.onerror = () => push('error');
  ws.onclose = (e) => push(`close ${e.code} ${e.reason}`);
  const until = async (predicate) => {
    const deadline = Date.now() + 5000;
    while (!events.some(predicate)) {
      if (Date.now() > deadline) throw new Error(`timed out waiting on ${path}: ${events}`);
      await new Promise((r) => {
        waiters.push(r);
        setTimeout(r, 100);
      });
    }
  };
  return { ws, events, until };
}

// Requests `/stats` without recording the response until `predicate` holds for its body.
async function untilStats(t, predicate) {
  const deadline = Date.now() + 5000;
  for (;;) {
    const stats = await fetch(t.base + '/stats').then((r) => r.json());
    if (predicate(stats)) return;
    if (Date.now() > deadline) throw new Error(`timed out waiting on /stats: ${JSON.stringify(stats)}`);
    await new Promise((r) => setTimeout(r, 10));
  }
}

export default async function (t) {
  await t.get('/me');
  const anon = connect(t, '/ws/lobby');
  await anon.until((e) => e.startsWith('close') || e === 'error');
  t.note('unauthenticated upgrade', anon.events);
  const nope = connect(t, '/elsewhere');
  await nope.until((e) => e.startsWith('close') || e === 'error');
  t.note('unknown upgrade path', nope.events);

  await t.post('/login', { json: { user: 'alice' } });
  const alice = [...t.cookies].map(([k, v]) => `${k}=${v}`).join('; ');
  await t.get('/me', { name: 'alice' });

  const a = connect(t, '/ws/lobby', alice);
  await a.until((e) => e.includes('welcome'));
  const b = connect(t, '/ws/lobby', alice);
  await b.until((e) => e.includes('welcome'));
  const c = connect(t, '/ws/other', alice);
  await c.until((e) => e.includes('welcome'));
  await t.get('/stats');

  a.ws.send(JSON.stringify({ type: 'ping', id: 7 }));
  await a.until((e) => e.includes('pong'));
  a.ws.send('not json');
  await a.until((e) => e.includes('invalid json'));
  a.ws.send(new TextEncoder().encode('bytes ✓'));
  await a.until((e) => e.startsWith('binary'));
  a.ws.send(JSON.stringify({ type: 'say', text: 'hello lobby' }));
  await b.until((e) => e.includes('hello lobby'));
  await t.post('/rooms/lobby/messages', { json: { text: 'from http' } });
  await a.until((e) => e.includes('from http'));
  await b.until((e) => e.includes('from http'));
  await t.post('/rooms/lobby/messages', { json: { text: 'x' }, jar: false, name: 'anonymous' });

  const poll = t.get('/rooms/other/poll?timeout=3000');
  await untilStats(t, (stats) => stats.polling === 1);
  c.ws.send(JSON.stringify({ type: 'say', text: 'polled' }));
  await poll;
  await t.get('/rooms/other/poll?timeout=50', { name: 'timeout' });
  await t.get('/rooms/lobby/messages?since=1');

  a.ws.send('x'.repeat(5000));
  await a.until((e) => e.startsWith('close'));
  b.ws.send(JSON.stringify({ type: 'bye' }));
  await b.until((e) => e.startsWith('close'));
  c.ws.close(1000, 'done');
  await c.until((e) => e.startsWith('close'));
  t.note('a', a.events);
  t.note('b', b.events);
  t.note('c', c.events);
  // We wait until the server removed the closed clients.
  await untilStats(t, (stats) => stats.clients === 0);
  await t.get('/stats', { name: 'after close' });
}
