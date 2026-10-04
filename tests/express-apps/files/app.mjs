// File uploads and downloads on Express 5 as an ES module: multer with disk and memory storage,
// busboy used directly, res.download and res.sendFile, streamed and piped responses, and
// gzip of streamed files.
import express from 'express';
import multer from 'multer';
import busboy from 'busboy';
import { createReadStream, createWriteStream, promises as fsp, mkdirSync } from 'node:fs';
import { join, extname, basename } from 'node:path';
import { createHash } from 'node:crypto';
import { pipeline } from 'node:stream/promises';
import { Readable, Transform, PassThrough } from 'node:stream';
import { createGzip, gunzipSync } from 'node:zlib';
import { once } from 'node:events';

const dataDir = process.env.DATA_DIR;
const uploadDir = join(dataDir, 'uploads');
mkdirSync(uploadDir, { recursive: true });

const app = express();
// Spin leaves empty directories out of the files it mounts, so each request creates the upload
// directory if it is missing.
app.use(async (req, res, next) => {
  await fsp.mkdir(uploadDir, { recursive: true });
  next();
});

let counter = 0;
const disk = multer({
  storage: multer.diskStorage({
    destination: (req, file, cb) => cb(null, uploadDir),
    filename: (req, file, cb) => cb(null, `${++counter}-${file.originalname.replace(/[^\w.-]/g, '_')}`),
  }),
  limits: { fileSize: 200_000, files: 3 },
  fileFilter: (req, file, cb) => {
    if (file.mimetype === 'application/x-msdownload') return cb(new Error('executables not allowed'));
    cb(null, true);
  },
});
const memory = multer({ storage: multer.memoryStorage(), limits: { fileSize: 1_000_000 } });

const sha = (buf) => createHash('sha256').update(buf).digest('hex').slice(0, 16);
const describe = (f) => ({
  field: f.fieldname,
  name: f.originalname,
  type: f.mimetype,
  size: f.size,
  stored: f.filename,
  encoding: f.encoding,
});

app.post('/upload/single', disk.single('file'), async (req, res) => {
  if (!req.file) return res.status(400).json({ error: 'no file' });
  const content = await fsp.readFile(req.file.path);
  res.status(201).json({ file: describe(req.file), sha: sha(content), body: req.body });
});

app.post('/upload/many', disk.array('docs', 3), (req, res) => {
  res.json({ files: req.files.map(describe), body: req.body });
});

app.post('/upload/fields', disk.fields([{ name: 'avatar', maxCount: 1 }, { name: 'gallery', maxCount: 2 }]), (req, res) => {
  res.json({
    avatar: (req.files.avatar || []).map(describe),
    gallery: (req.files.gallery || []).map(describe),
    body: req.body,
  });
});

app.post('/upload/memory', memory.any(), (req, res) => {
  res.json({
    files: req.files.map((f) => ({ ...describe(f), sha: sha(f.buffer), head: f.buffer.subarray(0, 8).toString('hex') })),
    body: req.body,
  });
});

app.post('/upload/text-only', memory.none(), (req, res) => res.json(req.body));

// Busboy directly, streaming each file to disk while hashing it.
app.post('/upload/busboy', (req, res, next) => {
  let bb;
  try {
    bb = busboy({ headers: req.headers, limits: { fileSize: 50_000 }, preservePath: false });
  } catch (e) {
    return res.status(400).json({ error: e.message });
  }
  const files = [];
  const fields = [];
  const pending = [];
  bb.on('file', (name, stream, info) => {
    const hash = createHash('sha256');
    const target = join(uploadDir, `bb-${files.length}-${basename(info.filename || 'unnamed')}`);
    const record = { name, filename: info.filename, mimeType: info.mimeType, bytes: 0, truncated: false };
    files.push(record);
    stream.on('data', (d) => {
      record.bytes += d.length;
      hash.update(d);
    });
    stream.on('limit', () => (record.truncated = true));
    pending.push(
      pipeline(stream, createWriteStream(target)).then(async () => {
        record.sha = hash.digest('hex').slice(0, 16);
        record.onDisk = (await fsp.stat(target)).size;
      }),
    );
  });
  bb.on('field', (name, value, info) => fields.push({ name, value, truncated: info.valueTruncated }));
  bb.on('error', next);
  bb.on('close', async () => {
    await Promise.all(pending);
    res.json({ files, fields });
  });
  req.pipe(bb);
});

// Raw body streamed to disk.
app.put('/raw/:name', async (req, res) => {
  const target = join(uploadDir, `raw-${basename(req.params.name)}`);
  const hash = createHash('sha256');
  let bytes = 0;
  const tap = new Transform({
    transform(chunk, enc, cb) {
      bytes += chunk.length;
      hash.update(chunk);
      cb(null, chunk);
    },
  });
  await pipeline(req, tap, createWriteStream(target));
  res.status(201).json({ bytes, sha: hash.digest('hex').slice(0, 16), size: (await fsp.stat(target)).size });
});

app.get('/uploads', async (req, res) => {
  const names = (await fsp.readdir(uploadDir)).sort();
  const entries = await Promise.all(
    names.map(async (n) => ({ name: n, size: (await fsp.stat(join(uploadDir, n))).size })),
  );
  res.json(entries);
});

app.get('/uploads/:name', (req, res, next) => {
  res.sendFile(req.params.name, { root: uploadDir, dotfiles: 'deny', lastModified: false, etag: false }, (err) => {
    if (err) next(err);
  });
});

app.get('/download/:name', (req, res, next) => {
  res.download(join(uploadDir, req.params.name), `download${extname(req.params.name)}`, { lastModified: false, etag: false }, (err) => {
    if (err && !res.headersSent) res.status(404).json({ error: 'missing', code: err.code });
  });
});

app.delete('/uploads/:name', async (req, res) => {
  try {
    await fsp.unlink(join(uploadDir, basename(req.params.name)));
    res.sendStatus(204);
  } catch (e) {
    res.status(404).json({ code: e.code });
  }
});

// A generated stream with backpressure.
app.get('/stream/numbers', (req, res) => {
  const count = Number(req.query.n ?? 1000);
  let i = 0;
  const source = new Readable({
    read() {
      while (i < count) {
        if (!this.push(`${i++}\n`)) return;
      }
      this.push(null);
    },
  });
  res.type('text/plain');
  source.pipe(res);
});

app.get('/stream/gzip/:name', (req, res, next) => {
  const file = join(uploadDir, basename(req.params.name));
  const input = createReadStream(file);
  input.on('error', (e) => next(Object.assign(e, { status: 404 })));
  input.once('open', () => {
    res.set('Content-Encoding', 'gzip');
    res.type('application/octet-stream');
    pipeline(input, createGzip(), res).catch(next);
  });
});

app.get('/stream/range/:name', async (req, res) => {
  const file = join(uploadDir, basename(req.params.name));
  const { size } = await fsp.stat(file);
  const start = Number(req.query.start ?? 0);
  const end = Math.min(Number(req.query.end ?? size - 1), size - 1);
  res.set('Content-Length', String(end - start + 1));
  res.type('application/octet-stream');
  createReadStream(file, { start, end, highWaterMark: 1024 }).pipe(res);
});

app.get('/stream/chunks', async (req, res) => {
  res.writeHead(200, { 'Content-Type': 'text/plain; charset=utf-8', 'X-Chunked': 'yes' });
  for (let i = 0; i < 5; i++) {
    const ok = res.write(`chunk ${i} ${'.'.repeat(i * 1000)}\n`);
    if (!ok) await once(res, 'drain');
    await new Promise((r) => setTimeout(r, 5));
  }
  res.end('end\n');
});

app.post('/echo-gzip', async (req, res) => {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  const body = Buffer.concat(chunks);
  const text = req.get('content-encoding') === 'gzip' ? gunzipSync(body).toString() : body.toString();
  res.json({ length: body.length, text });
});

app.get('/pass-through', (req, res) => {
  const pt = new PassThrough();
  res.type('json');
  pt.pipe(res);
  pt.write('{"parts":[');
  setTimeout(() => {
    pt.write('1,2,');
    setImmediate(() => pt.end('3]}'));
  }, 2);
});

app.use((err, req, res, next) => {
  if (err instanceof multer.MulterError) {
    return res.status(413).json({ error: 'MulterError', code: err.code, field: err.field ?? null });
  }
  res.status(err.status ?? 500).json({ error: err.message.replace(dataDir, '<data>'), code: err.code ?? null });
});

export default app;

if (import.meta.main) {
  const port = Number(process.env.PORT ?? 3000);
  app.listen(port, () => console.log(`listening on ${port}`));
}
