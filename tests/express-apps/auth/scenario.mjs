export const entry = 'app.mjs';

export default async function (t) {
  await t.post('/register', { json: { email: 'Alice@Example.COM', password: 'secret123', role: 'admin' } });
  await t.post('/register', { json: { email: 'bob@example.com', password: 'hunter22' } });
  await t.post('/register', { json: { email: 'bob@example.com', password: 'hunter22' }, name: 'duplicate' });
  await t.post('/register', { json: { email: 'not-an-email', password: 'short' }, name: 'invalid' });
  await t.post('/register', { json: { email: 'carol@example.com', password: 'longenough', role: 'root' }, name: 'bad role' });

  await t.post('/login', { json: { email: 'alice@example.com', password: 'nope' }, name: 'wrong password' });
  await t.post('/login', { json: { email: 'nobody@example.com', password: 'x' }, name: 'unknown' });
  await t.post('/login', { json: { email: 'alice@example.com' }, name: 'missing password' });
  const alice = await t.post('/login', { json: { email: 'alice@example.com', password: 'secret123' } });
  const bob = await t.post('/login', { json: { email: 'bob@example.com', password: 'hunter22' } });
  await t.post('/login-form', { form: { email: 'bob@example.com', password: 'hunter22' } });
  await t.post('/login-form', { form: { email: 'bob@example.com', password: 'wrong' }, name: 'failure' });
  await t.get('/welcome?email=bob%40example.com');

  const bearer = (tok) => ({ authorization: `Bearer ${tok}` });
  await t.get('/me', { headers_recorded: ['www-authenticate'], name: 'no token' });
  await t.get('/me', { headers: bearer(alice.json.access) });
  await t.get('/me', { headers: bearer(bob.json.access), name: 'bob' });
  await t.get('/me', { headers: { ...bearer(alice.json.access), 'x-clock': String(1_700_000_000 + 3600) }, name: 'expired' });
  await t.get('/me', { headers: bearer(alice.json.access.slice(0, -2) + 'xx'), name: 'tampered' });
  await t.get('/me', { headers: bearer('garbage'), name: 'malformed' });
  await t.get('/admin', { headers: bearer(alice.json.access) });
  await t.get('/admin', { headers: bearer(bob.json.access), name: 'forbidden' });
  await t.get(`/decode?token=${alice.json.access}`);

  const r1 = await t.post('/refresh', { json: { refresh: alice.json.refresh } });
  await t.post('/refresh', { json: { refresh: alice.json.refresh }, name: 'reused' });
  await t.post('/refresh', { json: {}, name: 'missing' });
  await t.get('/me', { headers: bearer(r1.json.access), name: 'refreshed' });

  await t.post('/sign', { json: { payload: { a: 1, nested: { b: [1, 2] } } } });
  await t.post('/sign', { json: { payload: { a: 1 }, alg: 'HS384' }, name: 'HS384' });
  // The expiry is an hour after the current second, which the two runs can see differently.
  const expiry = [[/"exp":\d+/g, '"exp":<exp>'], [/"token":"[^"]+"/, '"token":"<token>"'], [/W\/"[^"]+"/, 'W/"<etag>"']];
  await t.post('/sign', { json: { payload: { a: 1 }, alg: 'HS512', expiresIn: '1h' }, name: 'HS512', mask: expiry });
  await t.post('/sign', { json: { payload: { a: 1 }, alg: 'RS256' }, name: 'RS256 with secret' });
  await t.post('/sign', { json: { payload: 'a string payload', alg: 'HS256' }, name: 'string payload' });
  await t.post('/sign', { json: { payload: { a: 1 }, alg: 'none' }, name: 'none' });

  await t.get('/hash-sync?pw=p%C3%A4ssw%C3%B6rd');
  await t.get('/known-hash');
  await t.get('/compare?a=abc&b=abc');
  await t.get('/compare?a=abc&b=abd');
  await t.post('/scrypt/register', { json: { user: 'dave', password: 'pa55word' } });
  await t.post('/scrypt/login', { json: { user: 'dave', password: 'pa55word' } });
  await t.post('/scrypt/login', { json: { user: 'dave', password: 'wrong' }, name: 'wrong' });
  await t.post('/scrypt/login', { json: { user: 'erin', password: 'x' }, name: 'unknown' });
  await t.get('/derive');
  await t.get('/derive?secret=%E2%9C%93');
  await t.get('/derive-error');
  await t.get('/random');
}
