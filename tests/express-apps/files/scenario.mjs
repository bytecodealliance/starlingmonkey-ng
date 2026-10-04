import { gzipSync } from 'node:zlib';
import { Readable } from 'node:stream';

export const entry = 'app.mjs';

// Uploaded files have new modification times in each run.
export const masks = [[/W\/"([0-9a-f]+)-[0-9a-f]+"/, 'W/"$1-<mtime>"']];

function bytes(n, seed = 1) {
  const b = Buffer.alloc(n);
  for (let i = 0; i < n; i++) b[i] = (i * seed + 13) & 0xff;
  return b;
}

function form(entries) {
  const fd = new FormData();
  for (const [name, value, filename, type] of entries) {
    if (filename !== undefined) fd.append(name, new Blob([value], { type: type ?? 'application/octet-stream' }), filename);
    else fd.append(name, value);
  }
  return fd;
}

export default async function (t) {
  await t.post('/upload/single', { body: form([['title', 'hello'], ['file', 'plain text file\n', 'notes.txt', 'text/plain']]) });
  await t.post('/upload/single', { body: form([['file', bytes(150_000, 7), 'big image.png', 'image/png']]), name: 'binary' });
  await t.post('/upload/single', { body: form([['file', bytes(250_000), 'huge.bin']]), name: 'too large' });
  await t.post('/upload/single', { body: form([['title', 'no file']]), name: 'no file' });
  await t.post('/upload/single', { body: form([['file', 'MZ', 'evil.exe', 'application/x-msdownload']]), name: 'filtered' });
  await t.post('/upload/single', { body: form([['other', 'x', 'x.txt']]), name: 'unexpected field' });
  await t.post('/upload/many', {
    body: form([
      ['docs', 'one', 'a.txt', 'text/plain'],
      ['docs', 'two two', 'b.txt', 'text/plain'],
      ['tags', 'x'],
      ['tags', 'y'],
      ['meta[author]', 'me'],
    ]),
  });
  await t.post('/upload/many', {
    body: form([1, 2, 3, 4].map((i) => ['docs', `doc ${i}`, `${i}.txt`, 'text/plain'])),
    name: 'too many',
  });
  await t.post('/upload/fields', {
    body: form([
      ['avatar', bytes(1000, 3), 'me.jpg', 'image/jpeg'],
      ['gallery', bytes(2000, 5), 'g1.jpg', 'image/jpeg'],
      ['gallery', bytes(3000, 9), 'g2.jpg', 'image/jpeg'],
      ['caption', 'Ünïcødé ✓'],
    ]),
  });
  await t.post('/upload/memory', {
    body: form([
      ['a', bytes(5000, 11), 'a.bin'],
      ['b', JSON.stringify({ x: 1 }), 'b.json', 'application/json'],
      ['note', 'memory'],
    ]),
  });
  await t.post('/upload/text-only', { body: form([['a', '1'], ['b', '2'], ['b', '3']]) });
  await t.post('/upload/text-only', { body: form([['f', 'x', 'f.txt']]), name: 'file rejected' });

  // A hand-written multipart body with a fixed boundary, for byte-exact parsing.
  const boundary = '----StarlingBoundary7MA4YWxkTrZu0gW';
  const raw = [
    `--${boundary}\r\nContent-Disposition: form-data; name="field1"\r\n\r\nvalue1\r\n`,
    `--${boundary}\r\nContent-Disposition: form-data; name="file1"; filename="hello.txt"\r\nContent-Type: text/plain\r\n\r\nHello\r\nWorld\r\n`,
    `--${boundary}\r\nContent-Disposition: form-data; name="file2"; filename="big.bin"\r\nContent-Type: application/octet-stream\r\n\r\n`,
  ];
  const body = Buffer.concat([
    Buffer.from(raw.join('')),
    bytes(60_000, 3),
    Buffer.from(`\r\n--${boundary}\r\nContent-Disposition: form-data; name="empty"\r\n\r\n\r\n--${boundary}--\r\n`),
  ]);
  await t.post('/upload/busboy', { body, headers: { 'content-type': `multipart/form-data; boundary=${boundary}` } });
  await t.post('/upload/busboy', { body: 'x', headers: { 'content-type': 'text/plain' }, name: 'not multipart' });
  await t.post('/upload/busboy', {
    body: 'a=1&b=%E2%9C%93&c',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    name: 'urlencoded',
  });
  // A streamed request body in uneven chunks.
  const streamed = bytes(100_000, 17);
  const chunks = [];
  for (let i = 0; i < streamed.length; i += 7777) chunks.push(streamed.subarray(i, i + 7777));
  await t.request('PUT', '/raw/streamed.bin', { body: Readable.from(chunks) });
  await t.request('PUT', '/raw/empty.bin', { body: '' , name: 'empty' });

  await t.get('/uploads');
  await t.get('/uploads/1-notes.txt');
  await t.get('/uploads/2-big_image.png');
  await t.get('/uploads/2-big_image.png', { headers: { range: 'bytes=10-19' }, name: 'range' });
  await t.get('/uploads/does-not-exist');
  await t.get('/uploads/..%2fsecret', { name: 'traversal' });
  await t.get('/download/1-notes.txt');
  await t.get('/download/missing.txt');
  await t.get('/stream/numbers?n=20');
  await t.get('/stream/numbers?n=50000', { name: 'large' });
  await t.get('/stream/gzip/raw-streamed.bin', { headers: { 'accept-encoding': 'gzip' } });
  await t.get('/stream/gzip/nope.bin', { name: 'missing' });
  await t.get('/stream/range/raw-streamed.bin?start=100&end=5099');
  await t.get('/stream/chunks');
  await t.post('/echo-gzip', { body: gzipSync('compressed request body ✓'), headers: { 'content-encoding': 'gzip', 'content-type': 'application/octet-stream' } });
  await t.get('/pass-through');
  await t.request('DELETE', '/uploads/1-notes.txt');
  await t.request('DELETE', '/uploads/1-notes.txt', { name: 'again' });
  await t.get('/uploads', { name: 'after delete' });
}
