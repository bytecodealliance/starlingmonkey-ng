// A landing page on Koa 3 as an ES module that links to the Express apps: @koa/router, koa-body,
// koa-static, Handlebars views compiled at startup, koa-compress, @koa/cors, koa-session, and
// conditional GETs with ETags.
import Koa from 'koa';
import { Router } from '@koa/router';
import { koaBody } from 'koa-body';
import serve from 'koa-static';
import compress from 'koa-compress';
import cors from '@koa/cors';
import session from 'koa-session';
import conditional from 'koa-conditional-get';
import etag from '@koa/etag';
import Handlebars from 'handlebars';
import { readFileSync, readdirSync } from 'node:fs';
import { join, basename } from 'node:path';
import { constants as zlibConstants } from 'node:zlib';
import { createHash } from 'node:crypto';
import catalog from './catalog.mjs';

const here = import.meta.dirname;
// The prefix of the Express apps' mount points, which are `/<name>` below it.
const appsBase = process.env.APPS_BASE ?? '';

// Templates and partials are read and compiled once, when the module is evaluated.
const hbs = Handlebars.create();
hbs.registerHelper('appUrl', (name, path) => `${appsBase}/${name}${path === '/' ? '/' : path}`);
hbs.registerHelper('eq', (a, b) => a === b);
hbs.registerHelper('upper', (s) => String(s).toUpperCase());
for (const file of readdirSync(join(here, 'views/partials')).sort()) {
  const source = readFileSync(join(here, 'views/partials', file), 'utf8');
  hbs.registerPartial(basename(file, '.hbs'), hbs.compile(source));
}
const views = {};
for (const file of readdirSync(join(here, 'views')).filter((f) => f.endsWith('.hbs'))) {
  views[basename(file, '.hbs')] = hbs.compile(readFileSync(join(here, 'views', file), 'utf8'), { strict: false });
}
// Handlebars compiles a template when it first renders, so each view and partial renders once here.
for (const view of Object.values(views)) {
  view({ apps: catalog, entry: catalog[0], feedback: [], recent: [catalog[0].name], status: 500 });
}

const app = new Koa();
app.keys = ['portal key 2', 'portal key 1'];
app.proxy = true;

const feedback = [];
const stats = { views: 0, byApp: Object.fromEntries(catalog.map((a) => [a.name, 0])) };

// Renders `name` inside the layout, with the session's flash messages.
app.context.render = function (name, locals = {}) {
  const flash = this.session?.flash ?? [];
  if (this.session) this.session.flash = undefined;
  const page = views[name]({ ...this.state, ...locals });
  this.type = 'html';
  this.body = views.layout({ ...this.state, ...locals, flash, body: page });
};
app.context.flash = function (type, message) {
  this.session.flash = [...(this.session.flash ?? []), { type, message }];
};

// Errors render as JSON below `/api` and as an HTML page elsewhere.
app.use(async (ctx, next) => {
  try {
    await next();
    if (ctx.status === 404 && ctx.body === undefined) ctx.throw(404, `No page at ${ctx.path}`);
  } catch (err) {
    const status = err.status ?? err.statusCode ?? 500;
    ctx.status = status;
    if (err.headers) ctx.set(err.headers);
    const message = err.expose ? err.message : 'Internal Server Error';
    if (ctx.path.startsWith('/api/')) ctx.body = { error: message, status };
    else ctx.render('error', { title: `${status}`, status, message });
    if (status >= 500) ctx.app.emit('error', err, ctx);
  }
});
app.on('error', (err) => {
  if (!err.expose) console.error(`error: ${err.message}`);
});

app.use(conditional());
app.use(etag());
app.use(
  compress({
    threshold: 1024,
    br: { params: { [zlibConstants.BROTLI_PARAM_QUALITY]: 5 } },
    deflate: false,
  }),
);
app.use(
  cors({
    origin: (ctx) => {
      const origin = ctx.get('Origin');
      return origin.endsWith('.example.com') ? origin : '';
    },
    allowMethods: ['GET', 'POST'],
    exposeHeaders: ['X-Feedback-Count'],
    maxAge: 600,
    credentials: true,
  }),
);
app.use(serve(join(here, 'public'), { maxage: 60_000, index: false }));
app.use(session({ key: 'portal:sess', maxAge: 3600_000, renew: false, rolling: false }, app));
app.use(async (ctx, next) => {
  ctx.state.visits = ctx.session.visits = (ctx.session.visits ?? 0) + (ctx.method === 'GET' ? 1 : 0);
  ctx.state.recent = ctx.session.recent ?? [];
  await next();
});

const router = new Router();

router.get('home', '/', (ctx) => {
  stats.views += 1;
  ctx.render('index', { title: 'StarlingMonkey Express apps', apps: catalog, feedbackCount: feedback.length });
});

router.param('app', (name, ctx, next) => {
  ctx.state.app = catalog.find((a) => a.name === name);
  if (!ctx.state.app) ctx.throw(404, `Unknown app ${name}`);
  return next();
});

router.get('app', '/apps/:app', (ctx) => {
  const { app: entry } = ctx.state;
  stats.byApp[entry.name] += 1;
  ctx.session.recent = [entry.name, ...(ctx.session.recent ?? []).filter((n) => n !== entry.name)].slice(0, 3);
  ctx.render('app', { title: entry.title, entry, home: router.url('home') });
});

router.get('/docs', (ctx) => {
  ctx.status = 301;
  ctx.redirect(router.url('home'));
});

router.get('/go/:app', (ctx) => {
  const target = String(ctx.query.path ?? '/');
  if (!target.startsWith('/') || target.startsWith('//')) ctx.throw(400, 'path must be absolute');
  ctx.redirect(`${appsBase}/${ctx.state.app.name}${target}`);
});

const body = koaBody({
  multipart: true,
  urlencoded: true,
  json: true,
  text: false,
  formLimit: '2kb',
  jsonLimit: '2kb',
  formidable: { maxFileSize: 0, allowEmptyFiles: false },
});

function validateFeedback(input) {
  const errors = [];
  const name = String(input.name ?? '').trim();
  const message = String(input.message ?? '').trim();
  const app = String(input.app ?? '');
  if (!name) errors.push('name is required');
  if (message.length < 3) errors.push('message is too short');
  if (!catalog.some((a) => a.name === app)) errors.push('app is unknown');
  const rating = Number(input.rating ?? 3);
  if (!Number.isInteger(rating) || rating < 1 || rating > 5) errors.push('rating must be 1 to 5');
  return { errors, entry: { name, message, app, rating } };
}

router.post('/feedback', body, (ctx) => {
  const { errors, entry } = validateFeedback(ctx.request.body);
  if (errors.length) {
    ctx.flash('error', errors.join(', '));
    ctx.redirect(router.url('home'));
    return;
  }
  feedback.push({ id: feedback.length + 1, ...entry });
  ctx.flash('info', `Thanks, ${entry.name}! Your note on ${entry.app} was saved.`);
  ctx.status = 303;
  ctx.redirect(router.url('app', { app: entry.app }));
});

router.get('/feedback', (ctx) => {
  ctx.render('feedback', { title: 'Feedback', feedback, empty: feedback.length === 0 });
});

const api = new Router({ prefix: '/api' });
api.get('/apps', (ctx) => {
  ctx.set('X-Feedback-Count', String(feedback.length));
  ctx.body = catalog.map((a) => ({ ...a, url: `${appsBase}/${a.name}${a.home}` }));
});
api.get('/apps/:app', (ctx) => {
  ctx.body = ctx.state.app;
});
api.param('app', (name, ctx, next) => {
  ctx.state.app = catalog.find((a) => a.name === name);
  if (!ctx.state.app) ctx.throw(404, `Unknown app ${name}`);
  return next();
});
api.post('/feedback', body, (ctx) => {
  const { errors, entry } = validateFeedback(ctx.request.body);
  if (errors.length) ctx.throw(422, errors.join(', '));
  const saved = { id: feedback.length + 1, ...entry };
  feedback.push(saved);
  ctx.status = 201;
  ctx.set('Location', `/api/feedback/${saved.id}`);
  ctx.body = saved;
});
api.get('/feedback/:id', (ctx) => {
  const item = feedback[Number(ctx.params.id) - 1];
  if (!item) ctx.throw(404, 'No such feedback');
  ctx.body = item;
});
api.get('/stats', (ctx) => {
  ctx.body = { ...stats, feedback: feedback.length, session: { visits: ctx.session.visits } };
});
api.get('/digest', (ctx) => {
  const text = catalog.map((a) => `${a.name}: ${a.summary}`).join('\n').repeat(20);
  ctx.type = 'text/plain';
  ctx.body = text;
  ctx.set('X-Digest', createHash('sha256').update(text).digest('hex').slice(0, 16));
});
api.get('/boom', () => {
  throw new Error('portal failure');
});
api.get('/teapot', (ctx) => ctx.throw(418, 'short and stout', { headers: { 'X-Teapot': 'yes' } }));

app.use(router.routes()).use(router.allowedMethods());
app.use(api.routes()).use(api.allowedMethods({ throw: false }));

export default app;

if (import.meta.main) {
  const port = Number(process.env.PORT ?? 3000);
  app.listen(port, () => console.log(`listening on ${port}`));
}
