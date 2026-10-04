// Token and session-less authentication on Express 5 as an ES module: bcryptjs password hashes,
// jsonwebtoken HS256/HS512 tokens, passport-local, express-validator, and refresh-token rotation.
import express from 'express';
import jwt from 'jsonwebtoken';
import bcrypt from 'bcryptjs';
import passport from 'passport';
import { Strategy as LocalStrategy } from 'passport-local';
import { body, query, validationResult, matchedData } from 'express-validator';
import {
  createHash,
  createHmac,
  createSecretKey,
  hkdfSync,
  pbkdf2,
  pbkdf2Sync,
  randomBytes,
  randomUUID,
  scrypt,
  scryptSync,
  timingSafeEqual,
} from 'node:crypto';
import { promisify } from 'node:util';

const SECRET = 'a very secret signing key, at least 32 bytes long';
const REFRESH_SECRET = Buffer.from('refresh-secret-bytes-0123456789abcdef');
const app = express();
app.use(express.json());
app.use(express.urlencoded({ extended: false }));
app.use(passport.initialize());

const users = new Map();
const refreshTokens = new Map();
const signAsync = promisify(jwt.sign);
const verifyAsync = promisify(jwt.verify);
const FIXED_IAT = 1_700_000_000;

passport.use(
  new LocalStrategy({ usernameField: 'email' }, async (email, password, done) => {
    try {
      const user = users.get(email);
      if (!user) return done(null, false, { message: 'Unknown user' });
      const ok = await bcrypt.compare(password, user.hash);
      if (!ok) return done(null, false, { message: 'Wrong password' });
      return done(null, user);
    } catch (e) {
      return done(e);
    }
  }),
);

function checkValidation(req, res, next) {
  const errors = validationResult(req);
  if (!errors.isEmpty()) {
    return res.status(400).json({ errors: errors.array().map((e) => ({ field: e.path, msg: e.msg })) });
  }
  next();
}

function issueTokens(user, now) {
  const access = jwt.sign({ sub: user.email, role: user.role, iat: now }, SECRET, {
    expiresIn: '15m',
    issuer: 'express-app',
    audience: 'api',
    jwtid: createHash('sha256').update(`${user.email}:${now}`).digest('hex').slice(0, 16),
  });
  const refreshId = createHmac('sha256', REFRESH_SECRET).update(`${user.email}:${now}:${refreshTokens.size}`).digest('base64url');
  refreshTokens.set(refreshId, { email: user.email, now });
  return { access, refresh: refreshId };
}

const auth = (roles) => async (req, res, next) => {
  const header = req.get('authorization') || '';
  const [scheme, token] = header.split(' ');
  if (scheme !== 'Bearer' || !token) {
    res.set('WWW-Authenticate', 'Bearer realm="api"');
    return res.status(401).json({ error: 'missing token' });
  }
  try {
    const clockTimestamp = Number(req.get('x-clock') || FIXED_IAT + 60);
    req.user = await verifyAsync(token, SECRET, {
      algorithms: ['HS256'],
      issuer: 'express-app',
      audience: 'api',
      clockTimestamp,
    });
  } catch (e) {
    return res.status(401).json({ error: e.name, message: e.message });
  }
  if (roles && !roles.includes(req.user.role)) return res.status(403).json({ error: 'forbidden' });
  next();
};

app.post(
  '/register',
  body('email').isEmail().normalizeEmail(),
  body('password').isLength({ min: 8 }).withMessage('too short').matches(/\d/).withMessage('needs a digit'),
  body('role').optional().isIn(['user', 'admin']),
  checkValidation,
  async (req, res) => {
    const { email, password, role = 'user' } = matchedData(req);
    if (users.has(email)) return res.status(409).json({ error: 'exists' });
    const salt = await bcrypt.genSalt(4);
    const hash = await bcrypt.hash(password, salt);
    users.set(email, { email, hash, role });
    res.status(201).json({ email, role, hashPrefix: hash.slice(0, 7), hashLength: hash.length });
  },
);

app.post('/login', (req, res, next) => {
  passport.authenticate('local', { session: false }, (err, user, info) => {
    if (err) return next(err);
    if (!user) return res.status(401).json({ error: info?.message ?? 'unauthorized' });
    res.json(issueTokens(user, FIXED_IAT));
  })(req, res, next);
});

// Redirects are relative to the path the app is mounted at.
const authenticateForm = (req, res, next) =>
  passport.authenticate('local', { session: false, failureRedirect: `${req.baseUrl}/login-failed` })(req, res, next);
app.post('/login-form', authenticateForm, (req, res) => {
  res.redirect(303, `${req.baseUrl}/welcome?email=${encodeURIComponent(req.user.email)}`);
});
app.get('/welcome', query('email').isEmail(), checkValidation, (req, res) => res.send(`welcome ${req.query.email}`));
app.get('/login-failed', (req, res) => res.status(401).send('login failed'));

app.post('/refresh', body('refresh').isString().notEmpty(), checkValidation, (req, res) => {
  const entry = refreshTokens.get(req.body.refresh);
  if (!entry) return res.status(401).json({ error: 'invalid refresh token' });
  refreshTokens.delete(req.body.refresh);
  res.json(issueTokens(users.get(entry.email), entry.now + 1));
});

app.get('/me', auth(), (req, res) => {
  res.json({ sub: req.user.sub, role: req.user.role, exp: req.user.exp, iss: req.user.iss, jti: req.user.jti });
});

app.get('/admin', auth(['admin']), (req, res) => res.json({ secret: 42 }));

app.get('/decode', (req, res) => {
  const decoded = jwt.decode(req.query.token, { complete: true });
  res.json(decoded);
});

app.post('/sign', async (req, res, next) => {
  try {
    const { payload, alg = 'HS256', expiresIn } = req.body;
    const opts = { algorithm: alg, noTimestamp: true };
    if (expiresIn) opts.expiresIn = expiresIn;
    const token = await signAsync(payload, SECRET, opts);
    const verified = jwt.verify(token, SECRET, { algorithms: [alg], clockTimestamp: 0 });
    res.json({ token, verified });
  } catch (e) {
    next(e);
  }
});

app.get('/hash-sync', (req, res) => {
  const hash = bcrypt.hashSync(req.query.pw ?? 'pw', 4);
  res.json({ ok: bcrypt.compareSync(req.query.pw ?? 'pw', hash), rounds: bcrypt.getRounds(hash) });
});

app.get('/known-hash', async (req, res) => {
  // A hash produced by Node.js with bcryptjs and cost 4.
  const known = '$2b$04$qV3emKkj2M3yyE0k4RZmLODLy6FNoxL.tvzPrqzo90bwBuu74mHJa';
  res.json({ match: await bcrypt.compare('correct horse', known), mismatch: await bcrypt.compare('nope', known) });
});

app.get('/compare', (req, res) => {
  const a = Buffer.from(String(req.query.a ?? ''));
  const b = Buffer.from(String(req.query.b ?? ''));
  res.json({ equal: a.length === b.length && timingSafeEqual(a, b) });
});

// Password storage with node:crypto's scrypt instead of bcrypt, and API keys derived with PBKDF2
// and HKDF.
const scryptAsync = promisify(scrypt);
const scryptUsers = new Map();
app.post('/scrypt/register', async (req, res) => {
  const { user, password } = req.body;
  const salt = createHash('sha256').update(`salt:${user}`).digest().subarray(0, 16);
  const hash = await scryptAsync(password, salt, 32, { N: 1024, r: 8, p: 1 });
  scryptUsers.set(user, `${salt.toString('hex')}:${hash.toString('hex')}`);
  res.status(201).json({ stored: scryptUsers.get(user) });
});
app.post('/scrypt/login', async (req, res) => {
  const { user, password } = req.body;
  const stored = scryptUsers.get(user);
  if (!stored) return res.status(404).json({ error: 'unknown user' });
  const [salt, hash] = stored.split(':').map((h) => Buffer.from(h, 'hex'));
  const candidate = scryptSync(password, salt, 32, { cost: 1024 });
  res.status(timingSafeEqual(candidate, hash) ? 200 : 401).json({ ok: timingSafeEqual(candidate, hash) });
});
app.get('/derive', (req, res, next) => {
  const secret = req.query.secret ?? 'master';
  pbkdf2(secret, 'api-key-salt', 2000, 32, 'sha512', (err, key) => {
    if (err) return next(err);
    const hk = Buffer.from(hkdfSync('sha256', key, 'salt', 'info:v1', 24));
    const keyObject = createSecretKey(hk);
    res.json({
      pbkdf2: key.toString('base64url'),
      hkdf: hk.toString('hex'),
      jwk: keyObject.export({ format: 'jwk' }),
      size: keyObject.symmetricKeySize,
      same: keyObject.equals(createSecretKey(hk.toString('hex'), 'hex')),
      hmac: createHmac('sha256', keyObject).update('payload').digest('hex'),
      sync: pbkdf2Sync(secret, 'api-key-salt', 2000, 32, 'sha512').equals(key),
    });
  });
});
app.get('/derive-error', (req, res) => {
  try {
    scryptSync('pw', 'salt', 16, { N: 3 });
  } catch (e) {
    return res.status(400).json({ name: e.name, code: e.code, message: e.message });
  }
  res.json({ ok: true });
});

app.get('/random', (req, res) => {
  res.json({ bytes: randomBytes(16).length, uuid: randomUUID().length });
});

app.use((err, req, res, next) => {
  res.status(err.status ?? 500).json({ error: err.name, message: err.message });
});

export default app;

if (import.meta.main) {
  const port = Number(process.env.PORT ?? 3000);
  app.listen(port, () => console.log(`listening on ${port}`));
}
