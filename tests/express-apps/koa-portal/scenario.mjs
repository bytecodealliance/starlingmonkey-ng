import { utimesSync } from 'node:fs';
import { join } from 'node:path';

export const entry = 'app.mjs';

// The session cookie holds its expiry time, and its signature changes with it.
export const masks = [
  [/portal:sess=[\w-]+/g, 'portal:sess=<session>'],
  [/portal:sess\.sig=[\w-]+/g, 'portal:sess.sig=<signature>'],
];

// Spin serves copies of the static files, whose modification times are in their ETags.
export const spinMasks = [[/W\/"([0-9a-f]+)-[0-9a-f]+"/g, 'W/"$1-<mtime>"']];

// Static files get a fixed modification time, so their validators are reproducible.
export function setup(ctx) {
  const fixed = new Date('2024-05-06T07:08:09Z');
  for (const f of ['css/portal.css', 'robots.txt']) utimesSync(join(ctx.appDir, 'public', f), fixed, fixed);
  return {};
}

export default async function (t) {
  await t.get('/');
  await t.get('/', { name: 'second visit' });
  await t.request('HEAD', '/', { name: 'head' });
  await t.get('/apps/rest-api');
  await t.get('/apps/files');
  await t.get('/apps/auth', { name: 'recently viewed' });
  await t.get('/apps/unknown');
  await t.get('/nowhere');
  await t.get('/docs');
  await t.get('/go/ssr-site?path=/login');
  await t.get('/go/ssr-site?path=//evil.example', { name: 'protocol-relative' });
  await t.post('/apps/auth', { name: 'method not allowed', headers_recorded: ['allow'] });

  // Static files, ETags and conditional requests.
  const css = await t.get('/css/portal.css');
  await t.get('/css/portal.css', { headers: { 'if-none-match': css.headers.get('etag') }, name: 'etag 304' });
  await t.get('/css/portal.css', {
    headers: { 'if-modified-since': css.headers.get('last-modified') },
    name: 'ims 304',
  });
  await t.get('/css/portal.css', { headers: { 'accept-encoding': 'gzip' }, name: 'gzip' });
  await t.get('/css/portal.css', { headers: { 'accept-encoding': 'br' }, name: 'brotli' });
  await t.get('/css/portal.css', { headers: { 'accept-encoding': 'br;q=0.5, gzip' }, name: 'gzip preferred' });
  await t.get('/css/portal.css', { headers: { 'accept-encoding': 'identity' }, name: 'identity' });
  await t.get('/robots.txt');
  await t.get('/../app.mjs', { name: 'traversal' });

  // Generated bodies: compression of JSON, text and HTML, and ETags of generated bodies.
  const apps = await t.get('/api/apps', { headers_recorded: ['x-feedback-count'] });
  await t.get('/api/apps', { headers: { 'if-none-match': apps.headers.get('etag') }, name: 'json 304' });
  await t.get('/api/apps', { headers: { 'accept-encoding': 'br' }, name: 'json brotli' });
  await t.get('/api/apps', { headers: { 'accept-encoding': 'zstd, gzip' }, name: 'json zstd or gzip' });
  await t.get('/api/apps/data');
  await t.get('/api/apps/nope');
  await t.get('/api/digest', { headers: { 'accept-encoding': 'gzip' }, headers_recorded: ['x-digest'] });
  await t.get('/', { headers: { 'accept-encoding': 'gzip, deflate' }, name: 'html gzip' });

  // CORS.
  await t.request('OPTIONS', '/api/feedback', {
    headers: {
      origin: 'https://app.example.com',
      'access-control-request-method': 'POST',
      'access-control-request-headers': 'content-type',
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
  await t.get('/api/stats', {
    headers: { origin: 'https://app.example.com' },
    headers_recorded: ['access-control-allow-origin', 'access-control-expose-headers', 'access-control-allow-credentials'],
    name: 'cors allowed',
  });
  await t.get('/api/stats', {
    headers: { origin: 'https://evil.test' },
    headers_recorded: ['access-control-allow-origin'],
    name: 'cors refused',
  });

  // Feedback through forms and JSON, with flash messages in the session.
  await t.post('/feedback', { form: { name: '', message: 'x', app: 'nope', rating: '9' }, name: 'invalid form' });
  await t.get('/', { name: 'flash error' });
  await t.post('/feedback', { form: { name: 'Ada', message: 'Fast & small', app: 'auth', rating: '5' } });
  await t.get('/apps/auth', { name: 'flash info' });
  await t.get('/apps/auth', { name: 'flash gone' });
  const fd = new FormData();
  fd.append('name', 'Grace');
  fd.append('message', 'multipart works');
  fd.append('app', 'files');
  await t.post('/feedback', { body: fd, name: 'multipart form' });
  await t.post('/api/feedback', { json: { name: 'Linus', message: 'JSON too', app: 'data', rating: 4 } });
  await t.post('/api/feedback', { json: { name: 'Nobody' }, name: 'invalid json' });
  await t.post('/api/feedback', { body: '{"name":', headers: { 'content-type': 'application/json' }, name: 'malformed json' });
  await t.post('/api/feedback', { json: { name: 'x'.repeat(3000), message: 'big', app: 'auth' }, name: 'too large' });
  await t.post('/api/feedback', { body: 'name=Text', headers: { 'content-type': 'text/plain' }, name: 'text body' });
  await t.get('/api/feedback/2');
  await t.get('/api/feedback/99');
  await t.get('/feedback');
  await t.get('/api/stats');

  // Errors.
  await t.get('/api/boom');
  await t.get('/api/teapot', { headers_recorded: ['x-teapot'] });
  await t.request('DELETE', '/api/apps', { name: 'api method not allowed', headers_recorded: ['allow'] });
}
