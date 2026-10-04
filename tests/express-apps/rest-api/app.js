// A JSON REST API on Express 4: routers, body parsing, zod validation, async handlers, error
// middleware, CORS, helmet, compression, morgan and rate limiting.
'use strict';

const express = require('express');
const cors = require('cors');
const helmet = require('helmet');
const compression = require('compression');
const morgan = require('morgan');
const { rateLimit } = require('express-rate-limit');
const { z } = require('zod');
const { v4: uuidv4, v5: uuidv5, validate: uuidValidate } = require('uuid');
const { EventEmitter } = require('node:events');

const app = express();
app.set('json spaces', 0);
app.set('etag', 'strong');
app.disable('x-powered-by');

const logLines = [];
morgan.token('id', (req) => req.id);
app.use((req, res, next) => {
  req.id = req.get('x-request-id') || 'generated';
  res.set('X-Request-Id', req.id);
  next();
});
app.use(
  morgan(':id :method :url :status :res[content-length]', {
    stream: { write: (line) => logLines.push(line.trim()) },
  }),
);
app.use(helmet({ contentSecurityPolicy: { directives: { defaultSrc: ["'self'"] } } }));
app.use(
  cors({
    origin: (origin, cb) => cb(null, !origin || origin.endsWith('.example.com')),
    credentials: true,
    exposedHeaders: ['X-Total-Count', 'X-Request-Id'],
    maxAge: 600,
  }),
);
app.use(compression({ threshold: 512 }));
app.use(express.json({ limit: '10kb' }));
app.use(express.text({ type: 'text/plain' }));

const NAMESPACE = '1b671a64-40d5-491e-99b0-da01ff1f3341';
const events = new EventEmitter();

// An in-memory store with artificial async latency.
class Store {
  constructor() {
    this.items = new Map();
    this.seq = 0;
  }
  async list({ tag, q, sort, limit, offset }) {
    await new Promise((r) => setImmediate(r));
    let all = [...this.items.values()];
    if (tag) all = all.filter((i) => i.tags.includes(tag));
    if (q) all = all.filter((i) => i.title.toLowerCase().includes(q.toLowerCase()));
    if (sort) {
      const desc = sort.startsWith('-');
      const key = desc ? sort.slice(1) : sort;
      all.sort((a, b) => (a[key] < b[key] ? -1 : a[key] > b[key] ? 1 : 0) * (desc ? -1 : 1));
    }
    return { total: all.length, items: all.slice(offset, offset + limit) };
  }
  async get(id) {
    await Promise.resolve();
    return this.items.get(id);
  }
  async create(data) {
    await new Promise((r) => setTimeout(r, 1));
    this.seq += 1;
    const id = uuidv5(String(this.seq), NAMESPACE);
    const item = { id, seq: this.seq, done: false, tags: [], ...data, createdAt: this.seq * 1000 };
    this.items.set(id, item);
    events.emit('created', item);
    return item;
  }
  async update(id, patch) {
    const item = this.items.get(id);
    if (!item) return undefined;
    Object.assign(item, patch);
    return item;
  }
  async remove(id) {
    return this.items.delete(id);
  }
}
const store = new Store();

const TodoInput = z.object({
  title: z.string().min(1).max(100),
  priority: z.number().int().min(1).max(5).default(3),
  tags: z.array(z.string().regex(/^[a-z]+$/)).max(5).optional(),
  due: z.string().datetime().optional(),
});
const TodoPatch = TodoInput.partial().extend({ done: z.boolean().optional() });
const ListQuery = z.object({
  tag: z.string().optional(),
  q: z.string().optional(),
  sort: z.enum(['seq', '-seq', 'title', '-title', 'priority', '-priority']).optional(),
  limit: z.coerce.number().int().min(1).max(50).default(10),
  offset: z.coerce.number().int().min(0).default(0),
});

class HttpError extends Error {
  constructor(status, message, details) {
    super(message);
    this.status = status;
    this.details = details;
  }
}

const validate = (schema, where = 'body') => (req, res, next) => {
  const result = schema.safeParse(req[where]);
  if (!result.success) {
    return next(
      new HttpError(
        422,
        'Validation failed',
        result.error.issues.map((i) => ({ path: i.path.join('.'), code: i.code })),
      ),
    );
  }
  req.valid = result.data;
  next();
};

const asyncHandler = (fn) => (req, res, next) => Promise.resolve(fn(req, res, next)).catch(next);

const todos = express.Router({ caseSensitive: false, strict: false });
todos.param('id', (req, res, next, id) => {
  if (!uuidValidate(id)) return next(new HttpError(400, 'Malformed id'));
  next();
});
todos.get(
  '/',
  validate(ListQuery, 'query'),
  asyncHandler(async (req, res) => {
    const { total, items } = await store.list(req.valid);
    res.set('X-Total-Count', String(total));
    res.json(items);
  }),
);
todos.post(
  '/',
  validate(TodoInput),
  asyncHandler(async (req, res) => {
    const item = await store.create(req.valid);
    res.status(201).location(`${req.baseUrl}/${item.id}`).json(item);
  }),
);
todos
  .route('/:id')
  .get(
    asyncHandler(async (req, res) => {
      const item = await store.get(req.params.id);
      if (!item) throw new HttpError(404, 'Not found');
      res.json(item);
    }),
  )
  .patch(
    validate(TodoPatch),
    asyncHandler(async (req, res) => {
      const item = await store.update(req.params.id, req.valid);
      if (!item) throw new HttpError(404, 'Not found');
      res.json(item);
    }),
  )
  .delete(
    asyncHandler(async (req, res) => {
      if (!(await store.remove(req.params.id))) throw new HttpError(404, 'Not found');
      res.sendStatus(204);
    }),
  );

const limiter = rateLimit({
  windowMs: 60_000,
  limit: 3,
  standardHeaders: 'draft-7',
  legacyHeaders: false,
  message: { error: 'Too many requests' },
});

const api = express.Router();
api.use('/todos', todos);
api.get('/limited', limiter, (req, res) => res.json({ ok: true }));
api.get('/big', (req, res) => {
  const rows = Array.from({ length: 200 }, (_, i) => ({ i, text: `row number ${i}`.repeat(3) }));
  res.json(rows);
});
api.post('/echo', (req, res) => {
  res.json({
    type: req.get('content-type'),
    body: req.body,
    is: { json: req.is('json'), text: req.is('text/*') },
    accepts: req.accepts(['json', 'html']),
    ip: typeof req.ip,
    protocol: req.protocol,
    secure: req.secure,
    hostname: req.hostname,
    xhr: req.xhr,
    path: req.path,
    query: req.query,
  });
});
api.get('/search', (req, res) => {
  res.json({ query: req.query, originalUrl: req.originalUrl });
});
api.get('/format', (req, res) => {
  res.format({
    'text/plain': () => res.send('plain'),
    'text/html': () => res.send('<p>html</p>'),
    'application/json': () => res.json({ format: 'json' }),
    default: () => res.status(406).send('Not Acceptable'),
  });
});
api.get('/stats', (req, res) => {
  res.json({ logged: logLines.length, created: events.listenerCount('created') });
});
api.get('/log', (req, res) => {
  res.type('text').send(logLines.slice(-5).join('\n'));
});
api.get('/throws', () => {
  throw new Error('sync boom');
});
api.get(
  '/rejects',
  asyncHandler(async () => {
    await null;
    throw new HttpError(503, 'Upstream unavailable');
  }),
);
api.get('/uuid', (req, res) => {
  const id = uuidv4();
  res.json({ valid: uuidValidate(id), version: id[14], length: id.length });
});
api.all('/method', (req, res) => res.json({ method: req.method }));

app.use('/api/v1', api);
app.get('/health', (req, res) => res.send('ok'));

app.use((req, res, next) => next(new HttpError(404, `No route for ${req.method} ${req.path}`)));
// eslint-disable-next-line no-unused-vars
app.use((err, req, res, next) => {
  if (err.type === 'entity.parse.failed') {
    return res.status(400).json({ error: 'Invalid JSON' });
  }
  if (err.type === 'entity.too.large') {
    return res.status(413).json({ error: 'Payload too large', limit: err.limit });
  }
  const status = err.status || 500;
  res.status(status).json({ error: err.message, details: err.details });
});

module.exports = app;

if (require.main === module) {
  const port = Number(process.env.PORT || 3000);
  const server = app.listen(port, () => {
    console.log(`listening on ${server.address().port}`);
  });
}
