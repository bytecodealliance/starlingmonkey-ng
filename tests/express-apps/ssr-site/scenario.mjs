import { utimesSync } from 'node:fs';
import { join } from 'node:path';

export const entry = 'app.js';

// Spin serves copies of the static files, whose modification times are in their ETags.
export const spinMasks = [[/W\/"([0-9a-f]+)-[0-9a-f]+"/g, 'W/"$1-<mtime>"']];

// The static files get a fixed modification time, so their validators are reproducible.
export function setup(ctx) {
  const fixed = new Date('2024-05-06T07:08:09Z');
  for (const f of ['css/site.css', 'index.html', 'robots.txt', '.hidden']) {
    utimesSync(join(ctx.appDir, 'public', f), fixed, fixed);
  }
  return {};
}

// The app compiles its views at startup, but EJS checks that an included template exists with
// `fs.existsSync` on every render, which blocks, so serving needs blocking host I/O.
export const serveFlags = ['--allow-blocking-host-io'];

// Session IDs, CSRF tokens and the ETags of pages that show a CSRF token differ between runs.
export const masks = [[/sid=s%3A[^;]+/g, "sid=<session>"]];
const CSRF = [[/name="_csrf" value="[0-9a-f]+"/, 'name="_csrf" value="<csrf>"'], [/"[^"]*"$/, "<etag>"]];

export default async function (t) {
  await t.get('/');
  await t.get('/', { name: 'second visit, no session' });

  // Static files: index, ETag/Last-Modified, conditional requests, ranges, dotfiles.
  const css = await t.get('/static/css/site.css');
  await t.get('/static/css/site.css', { headers: { 'if-none-match': css.headers.get('etag') }, name: 'etag 304' });
  await t.get('/static/css/site.css', {
    headers: { 'if-modified-since': css.headers.get('last-modified') },
    name: 'ims 304',
  });
  await t.get('/static/css/site.css', { headers: { 'if-none-match': '"other"' }, name: 'etag mismatch' });
  await t.request('HEAD', '/static/css/site.css');
  await t.get('/static/');
  await t.get('/static', { name: 'redirect to slash' });
  await t.get('/static/robots.txt');
  await t.get('/static/.hidden');
  await t.get('/static/../app.js', { name: 'traversal' });
  await t.get('/static/%2e%2e/app.js', { name: 'encoded traversal' });
  const blob = await t.get('/files/blob.bin', { headers_recorded: ['x-file'] });
  await t.get('/files/blob.bin', { headers: { range: 'bytes=0-99' }, name: 'range' });
  await t.get('/files/blob.bin', { headers: { range: 'bytes=-500' }, name: 'suffix range' });
  await t.get('/files/blob.bin', { headers: { range: 'bytes=69990-' }, name: 'open range' });
  await t.get('/files/blob.bin', { headers: { range: 'bytes=100000-' }, name: 'unsatisfiable' });
  await t.get('/files/blob.bin', { headers: { range: 'bytes=0-1,5-6' }, name: 'multi range' });
  await t.get('/files/blob.bin', {
    headers: { range: 'bytes=0-9', 'if-range': blob.headers.get('etag') },
    name: 'if-range match',
  });
  await t.get('/files/blob.bin', { headers: { range: 'bytes=0-9', 'if-range': '"stale"' }, name: 'if-range stale' });
  await t.get('/files/notes.txt');

  // Login flow with CSRF, sessions and flash messages.
  await t.post('/notes', { form: { title: 'nope' }, name: 'not logged in' });
  const login = await t.get('/login?next=%2F', { mask: CSRF });
  const csrf = /name="_csrf" value="([0-9a-f]+)"/.exec(login.text)[1];
  await t.post('/login', { form: { _csrf: 'bad', name: 'alice', password: 'x' }, name: 'bad csrf' });
  await t.post('/login', { form: { _csrf: csrf, name: 'alice', password: 'wrong' }, name: 'bad password' });
  await t.get('/login', { mask: CSRF, name: 'flash shown' });
  const login2 = await t.get('/login', { mask: CSRF, name: 'fresh token' });
  const csrf2 = /name="_csrf" value="([0-9a-f]+)"/.exec(login2.text)[1];
  await t.post('/login?next=/%3Fwelcome%3D1', {
    form: { _csrf: csrf2, name: 'Alice', password: 'wonderland', remember: '1' },
  });
  await t.get('/', { name: 'logged in' });

  // Notes CRUD with urlencoded bodies and method override.
  await t.post('/notes', { form: { title: '', body: 'draft body' }, headers: { referer: '/' }, name: 'missing title' });
  await t.get('/', { name: 'draft kept' });
  await t.post('/notes', {
    body: 'title=First+note&body=Hello+*world*%0Aline+2+%3Cb%3E&tags[]=x&tags[]=y',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
  });
  await t.get('/notes/1');
  await t.post('/notes', { form: { title: 'Ünïcødé ✓', body: 'Grüße' }, name: 'unicode' });
  await t.get('/notes/2');
  await t.get('/notes/abc');
  await t.get('/', { name: 'list' });
  await t.post('/notes/1?_method=DELETE');
  await t.post('/notes/1', { headers: { 'x-http-method-override': 'DELETE' }, name: 'header override' });
  await t.get('/', { name: 'after delete' });

  // Cookies.
  await t.get('/theme/dark');
  await t.get('/cookies');
  await t.get('/', { name: 'themed' });
  await t.get('/cookies', { headers: { cookie: 'theme=s%3Adark.forged; a=1; b=%E2%9C%93' }, jar: false, name: 'forged' });

  // sendFile, download, redirects, render callbacks.
  await t.get('/download/notes');
  await t.get('/send/blob', { headers_recorded: ['x-sent'] });
  await t.get('/send/missing');
  await t.get('/redirects/2');
  await t.get('/redirects/0');
  await t.get('/render-string');
  // Spin's `wasi:http` requests have no client address, so `trust proxy` trusts no proxy there.
  await t.get('/req-info', {
    ignoreIn: { spin: 'no client address' },
    headers: {
      'x-forwarded-for': '203.0.113.7, 10.0.0.1',
      'x-forwarded-proto': 'https',
      'x-forwarded-host': 'shop.eu.example.com',
      'accept-language': 'en-US,en;q=0.8,de;q=0.5',
      'accept-charset': 'latin1',
      range: 'bytes=0-10',
    },
  });

  await t.post('/logout');
  await t.get('/', { name: 'logged out' });
  await t.get('/no/such/page');
}
