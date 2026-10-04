// A data service on Express 5 as an ES module: a lowdb JSON file store, server-sent events,
// NDJSON streaming, an upstream proxy with http-proxy-middleware and with fetch, winston and pino
// logging, and dotenv configuration.
import express from 'express';
import { Low } from 'lowdb';
import { JSONFile } from 'lowdb/node';
import { createProxyMiddleware } from 'http-proxy-middleware';
import httpProxy from 'http-proxy';
import winston from 'winston';
import pino from 'pino';
import dotenv from 'dotenv';
import { EventEmitter, on } from 'node:events';
import { join } from 'node:path';
import { writeFileSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { setTimeout as sleep } from 'node:timers/promises';
import { AsyncLocalStorage } from 'node:async_hooks';
import http from 'node:http';

const dataDir = process.env.DATA_DIR;
writeFileSync(join(dataDir, '.env'), 'APP_NAME="Data Service"\nPAGE_SIZE=3\n# comment\nMULTI="line1\\nline2"\nEMPTY=\n');
const env = {};
dotenv.config({ path: join(dataDir, '.env'), processEnv: env, quiet: true });

const upstream = process.env.UPSTREAM_URL;
const logFile = join(dataDir, 'app.log');
const winstonLines = [];
const logger = winston.createLogger({
  level: 'info',
  format: winston.format.combine(winston.format.label({ label: 'data' }), winston.format.json()),
  transports: [
    // The log file is opened on the first write, so no file is open when the app starts.
    new winston.transports.File({ filename: logFile, lazy: true }),
    new winston.transports.Stream({
      stream: new (await import('node:stream')).Writable({
        write(chunk, enc, cb) {
          winstonLines.push(JSON.parse(chunk.toString()));
          cb();
        },
      }),
    }),
  ],
});
const pinoLines = [];
const plog = pino(
  { base: { svc: 'data' }, timestamp: false },
  { write: (line) => pinoLines.push(JSON.parse(line)) },
);
const context = new AsyncLocalStorage();

const db = new Low(new JSONFile(join(dataDir, 'db.json')), { posts: [], seq: 0 });
await db.read();
// The database file exists from the start, for `/db-file`.
await db.write();

const bus = new EventEmitter();
const app = express();
app.use((req, res, next) => {
  const reqId = req.get('x-request-id') ?? `r${Math.floor(Math.random() * 1e6)}`;
  context.run({ reqId }, () => {
    plog.child({ reqId }).info({ method: req.method, url: req.url }, 'request');
    next();
  });
});

app.use('/proxy', createProxyMiddleware({
  target: upstream,
  changeOrigin: true,
  pathRewrite: { '^/': '/upstream/' },
  on: {
    proxyReq: (proxyReq, req) => {
      proxyReq.setHeader('x-proxied-by', 'hpm');
    },
    proxyRes: (proxyRes, req, res) => {
      res.setHeader('x-proxy-status', String(proxyRes.statusCode));
    },
    error: (err, req, res) => {
      res.writeHead(502, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ error: 'bad gateway', code: err.code }));
    },
  },
}));
// The classic http-proxy, as http-proxy-middleware v2 and many gateways use it.
const classic = httpProxy.createProxyServer({ target: upstream, xfwd: true });
classic.on('proxyRes', (proxyRes, req, res) => res.setHeader('x-classic', String(proxyRes.statusCode)));
classic.on('error', (err, req, res) => {
  res.writeHead(502, { 'content-type': 'application/json' });
  res.end(JSON.stringify({ error: 'classic bad gateway', code: err.code }));
});
app.use('/classic', (req, res) => {
  req.url = '/upstream' + req.url;
  classic.web(req, res);
});
app.use('/classic-down', (req, res) => classic.web(req, res, { target: 'http://127.0.0.1:9' }));
app.use(express.json());

app.get('/config', (req, res) => res.json({ env, name: env.APP_NAME }));

app.get('/posts', async (req, res) => {
  await db.read();
  const page = Number(req.query.page ?? 1);
  const size = Number(env.PAGE_SIZE);
  const { posts } = db.data;
  res.json({ page, total: posts.length, posts: posts.slice((page - 1) * size, page * size) });
});

app.post('/posts', async (req, res) => {
  const { title, body = '' } = req.body ?? {};
  if (typeof title !== 'string') return res.status(400).json({ error: 'title required' });
  await db.update((data) => {
    data.seq += 1;
    data.posts.push({ id: data.seq, title, body });
  });
  const post = db.data.posts.at(-1);
  logger.info('post created', { id: post.id, reqId: context.getStore().reqId });
  bus.emit('post', post);
  res.status(201).json(post);
});

app.delete('/posts/:id', async (req, res) => {
  const id = Number(req.params.id);
  let removed = false;
  await db.update((data) => {
    const i = data.posts.findIndex((p) => p.id === id);
    if (i >= 0) {
      data.posts.splice(i, 1);
      removed = true;
    }
  });
  logger.warn('post delete', { id, removed });
  res.status(removed ? 204 : 404).end();
});

app.get('/db-file', async (req, res) => {
  res.type('json').send(await readFile(join(dataDir, 'db.json')));
});

// Server-sent events: replays the existing posts, then streams new ones until `limit` events.
app.get('/events', async (req, res) => {
  const limit = Number(req.query.limit ?? 5);
  res.set({ 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache', Connection: 'keep-alive' });
  res.flushHeaders();
  res.write('retry: 1000\n\n');
  let sent = 0;
  const send = (event, data) => {
    sent += 1;
    res.write(`id: ${sent}\nevent: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
  };
  await db.read();
  for (const p of db.data.posts) send('post', p);
  const ac = new AbortController();
  req.on('close', () => ac.abort());
  try {
    for await (const [post] of on(bus, 'post', { signal: ac.signal })) {
      send('post', post);
      if (sent >= limit) break;
    }
  } catch (e) {
    if (e.name !== 'AbortError') throw e;
  }
  res.end('event: end\ndata: bye\n\n');
});

app.get('/ticks', async (req, res) => {
  res.type('text/event-stream');
  for (let i = 0; i < 4; i++) {
    res.write(`data: tick ${i}\n\n`);
    await sleep(10);
  }
  res.end();
});

app.get('/export.ndjson', async (req, res) => {
  await db.read();
  const posts = db.data.posts;
  res.type('application/x-ndjson');
  await pipeline(
    Readable.from(
      (async function* () {
        for (const p of posts) {
          await sleep(1);
          yield JSON.stringify(p) + '\n';
        }
        yield JSON.stringify({ count: posts.length }) + '\n';
      })(),
    ),
    res,
  );
});

app.post('/import.ndjson', async (req, res) => {
  let buffered = '';
  let imported = 0;
  const errors = [];
  req.setEncoding('utf8');
  for await (const chunk of req) {
    buffered += chunk;
    let nl;
    while ((nl = buffered.indexOf('\n')) >= 0) {
      const line = buffered.slice(0, nl).trim();
      buffered = buffered.slice(nl + 1);
      if (!line) continue;
      try {
        const { title, body } = JSON.parse(line);
        db.data.seq += 1;
        db.data.posts.push({ id: db.data.seq, title, body });
        imported += 1;
      } catch (e) {
        errors.push(e.name);
      }
    }
  }
  await db.write();
  res.json({ imported, errors });
});

// Calls the upstream with fetch and with http.get, and combines the responses.
app.get('/aggregate', async (req, res, next) => {
  try {
    const [a, b] = await Promise.all([
      fetch(`${upstream}/upstream/json?x=1`).then((r) => r.json()),
      new Promise((resolve, reject) => {
        http
          .get(`${upstream}/upstream/json?x=2`, (r) => {
            let s = '';
            r.setEncoding('utf8');
            r.on('data', (d) => (s += d));
            r.on('end', () => resolve({ status: r.statusCode, body: JSON.parse(s) }));
          })
          .on('error', reject);
      }),
    ]);
    const posted = await fetch(`${upstream}/upstream/echo`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ combined: [a.query, b.body.query] }),
    });
    res.json({ a, b, echo: await posted.json(), status: posted.status });
  } catch (e) {
    next(e);
  }
});

app.get('/fetch-stream', async (req, res) => {
  const r = await fetch(`${upstream}/upstream/slow`);
  res.status(r.status).type(r.headers.get('content-type'));
  await pipeline(Readable.fromWeb(r.body), res);
});

app.get('/fetch-error', async (req, res) => {
  try {
    await fetch('http://127.0.0.1:1/');
    res.json({ ok: true });
  } catch (e) {
    res.status(502).json({ name: e.name });
  }
});

app.get('/logs', async (req, res) => {
  await sleep(50);
  const logged = await readFile(logFile, 'utf8').catch((e) => (e.code === 'ENOENT' ? '' : Promise.reject(e)));
  const fileLines = logged.split('\n').filter(Boolean).map((l) => JSON.parse(l));
  res.json({
    winston: winstonLines,
    file: fileLines,
    pino: pinoLines.map(({ reqId, ...rest }) => ({ ...rest, reqId: reqId.startsWith('r') && reqId !== 'r-fixed' ? '<random>' : reqId })),
  });
});

app.use((err, req, res, next) => {
  res.status(500).json({ error: err.message });
});

export default app;

if (import.meta.main) {
  const port = Number(process.env.PORT ?? 3000);
  app.listen(port, () => console.log(`listening on ${port}`));
}
