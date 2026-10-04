// A server-rendered site on Express 4: EJS and Pug views, static files, signed cookies, sessions,
// flash messages, form posts and method override.
'use strict';

const path = require('path');
const fs = require('fs');
const crypto = require('crypto');
const express = require('express');
const cookieParser = require('cookie-parser');
const session = require('express-session');
const methodOverride = require('method-override');

const app = express();
const dataDir = process.env.DATA_DIR;

app.set('view engine', 'ejs');
app.set('views', path.join(__dirname, 'views'));
app.engine('pug', require('pug').__express);
app.set('trust proxy', 'loopback');
app.locals.formatBody = (body) =>
  body.replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' })[c]).replace(/\*(\w+)\*/g, '<em>$1</em>');

// A data file with a fixed modification time, so its validators are reproducible.
const blob = Buffer.alloc(70000);
for (let i = 0; i < blob.length; i++) blob[i] = (i * 31 + 7) % 251;
fs.writeFileSync(path.join(dataDir, 'blob.bin'), blob);
fs.writeFileSync(path.join(dataDir, 'notes.txt'), 'line one\nline two\n');
const fixed = new Date('2024-05-06T07:08:09Z');
fs.utimesSync(path.join(dataDir, 'blob.bin'), fixed, fixed);
fs.utimesSync(path.join(dataDir, 'notes.txt'), fixed, fixed);

app.use('/static', express.static(path.join(__dirname, 'public'), { maxAge: '1d', index: 'index.html' }));
app.use(
  '/files',
  express.static(dataDir, {
    etag: true,
    lastModified: true,
    setHeaders: (res, file) => res.set('X-File', path.basename(file)),
  }),
);
app.use(cookieParser('cookie secret'));
app.use(express.urlencoded({ extended: true }));
app.use(methodOverride('_method', { methods: ['POST'] }));
app.use(methodOverride('X-HTTP-Method-Override'));
app.use(
  session({
    name: 'sid',
    secret: ['new secret', 'old secret'],
    resave: false,
    saveUninitialized: false,
    rolling: true,
    cookie: { httpOnly: true, sameSite: 'lax', maxAge: 3600_000 },
  }),
);

// Flash messages kept in the session until the next render.
app.use((req, res, next) => {
  req.flash = (type, message) => {
    req.session.flash = req.session.flash || [];
    req.session.flash.push({ type, message });
  };
  res.locals.flash = (req.session && req.session.flash) || [];
  if (req.session) delete req.session.flash;
  res.locals.user = req.session.user;
  // Links and redirects are relative to the path the app is mounted at.
  res.locals.base = req.baseUrl;
  res.locals.theme = req.signedCookies.theme || req.cookies.theme_plain || 'light';
  next();
});

const notes = [];
let nextId = 1;
const users = { alice: { name: 'Alice', password: 'wonderland' } };

const requireUser = (req, res, next) => {
  if (!req.session.user) {
    req.flash('error', 'Please log in first');
    return res.redirect(303, req.baseUrl + '/login?next=' + encodeURIComponent(req.originalUrl));
  }
  next();
};

app.get('/', (req, res) => {
  req.session.visits = (req.session.visits || 0) + 1;
  res.render('index', { notes, draft: req.session.draft || {}, visits: req.session.visits });
});

app.get('/login', (req, res) => {
  req.session.csrf = crypto.randomBytes(16).toString('hex');
  res.render('login', { csrf: req.session.csrf });
});

app.post('/login', (req, res, next) => {
  const { name, password, _csrf, remember } = req.body;
  if (!_csrf || _csrf !== req.session.csrf) {
    return res.status(403).render('error', { status: 403, message: 'Bad CSRF token' });
  }
  const user = users[String(name).toLowerCase()];
  if (!user || user.password !== password) {
    req.flash('error', 'Invalid credentials');
    return res.redirect(req.baseUrl + '/login');
  }
  req.session.regenerate((err) => {
    if (err) return next(err);
    req.session.user = { id: name, name: user.name };
    req.flash('info', `Welcome back, ${user.name}!`);
    if (remember) req.session.cookie.maxAge = 30 * 24 * 3600_000;
    req.session.save((err2) => {
      if (err2) return next(err2);
      res.redirect(req.query.next || req.baseUrl + '/');
    });
  });
});

app.post('/logout', (req, res, next) => {
  req.session.destroy((err) => {
    if (err) return next(err);
    res.clearCookie('sid');
    res.redirect(req.baseUrl + '/');
  });
});

app.post('/notes', requireUser, (req, res) => {
  const { title, body, tags } = req.body;
  if (!title) {
    req.session.draft = { title, body };
    req.flash('error', 'Title is required');
    return res.redirect('back');
  }
  delete req.session.draft;
  const note = { id: nextId++, title, body: body || '', tags: tags || [], author: req.session.user.name };
  notes.push(note);
  req.flash('info', `Created "${title}"`);
  res.redirect(303, `${req.baseUrl}/notes/${note.id}`);
});

app.get('/notes/:id(\\d+)', (req, res, next) => {
  const note = notes.find((n) => n.id === Number(req.params.id));
  if (!note) return next();
  res.render('note.pug', { note, notes, title: note.title });
});

app.delete('/notes/:id', requireUser, (req, res) => {
  const i = notes.findIndex((n) => n.id === Number(req.params.id));
  if (i >= 0) notes.splice(i, 1);
  req.flash('info', i >= 0 ? 'Deleted' : 'Already gone');
  res.redirect(303, req.baseUrl + '/');
});

app.get('/theme/:name', (req, res) => {
  res.cookie('theme', req.params.name, { signed: true, path: '/', maxAge: 86400_000 });
  res.cookie('theme_plain', req.params.name, { expires: new Date('2030-01-01T00:00:00Z') });
  res.cookie('prefs', { compact: true, lang: 'en' });
  res.redirect(req.baseUrl + '/');
});

app.get('/cookies', (req, res) => {
  res.json({ cookies: req.cookies, signed: req.signedCookies });
});

app.get('/download/notes', (req, res, next) => {
  res.download(path.join(dataDir, 'notes.txt'), 'my notes.txt', (err) => err && next(err));
});

app.get('/send/blob', (req, res, next) => {
  res.sendFile('blob.bin', { root: dataDir, headers: { 'X-Sent': 'yes' } }, (err) => err && next(err));
});

app.get('/send/missing', (req, res, next) => {
  res.sendFile(path.join(dataDir, 'missing.txt'), (err) => {
    if (err) {
      res.status(err.status || 500).send(`sendFile failed: ${err.code} ${err.status}`);
    }
  });
});

app.get('/redirects/:n', (req, res) => {
  const n = Number(req.params.n);
  if (n <= 0) return res.send('done');
  res.redirect(301, `${req.baseUrl}/redirects/${n - 1}`);
});

app.get('/render-string', (req, res) => {
  app.render('error', { status: 418, message: "I'm a teapot" }, (err, html) => {
    res.type('text/plain').send(`rendered ${html.length} chars: ${html.replace(/\s+/g, ' ')}`);
  });
});

app.get('/req-info', (req, res) => {
  res.json({
    ip: req.ip,
    ips: req.ips,
    protocol: req.protocol,
    hostname: req.hostname,
    fresh: req.fresh,
    stale: req.stale,
    subdomains: req.subdomains,
    acceptsLanguages: req.acceptsLanguages('de', 'en'),
    acceptsCharsets: req.acceptsCharsets('utf-8', 'latin1'),
    range: req.range(1000),
  });
});

app.use((req, res) => {
  res.status(404).render('error', { status: 404, message: 'Page not found' });
});
// eslint-disable-next-line no-unused-vars
app.use((err, req, res, next) => {
  res.status(err.status || 500).render('error', { status: err.status || 500, message: err.message });
});

// Each view is compiled once at startup, so requests render them from the view cache.
if (app.enabled('view cache')) {
  const note = { id: 0, title: '', body: '', tags: [], author: '' };
  const locals = { notes: [], draft: {}, visits: 0, flash: [], user: null, theme: '', base: '', csrf: '', status: 0, message: '', note };
  for (const view of ['index', 'login', 'error', 'note.pug']) app.render(view, { ...locals, title: '' }, () => {});
}

module.exports = app;

if (require.main === module) {
  const port = Number(process.env.PORT || 3000);
  app.listen(port, () => console.log(`listening on ${port}`));
}
