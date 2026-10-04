// The Express apps the portal links to, with example requests that work against each app's routes.
// Paths are relative to the app's mount point, which `app.mjs` prefixes. `home` is the page the
// portal opens an app at.
export default [
  {
    name: 'rest-api',
    home: '/health',
    title: 'REST API',
    stack: 'Express 4, CommonJS',
    summary:
      'A JSON API for todos with zod validation, CORS, helmet, compression, morgan logging, rate limiting and content negotiation.',
    links: [
      { path: '/health', label: 'Health check' },
      { path: '/api/v1/todos', label: 'List todos' },
      { path: '/api/v1/uuid', label: 'Generate a v4 UUID' },
      { path: '/api/v1/format', label: 'Content negotiation' },
      { path: '/api/v1/search?q=koa&tags=a&tags=b', label: 'Echo a query string' },
      { path: '/api/v1/big', label: 'A compressed JSON array' },
    ],
    forms: [
      {
        method: 'GET',
        action: '/api/v1/todos',
        label: 'Search todos',
        fields: [
          { name: 'q', label: 'Title contains', value: 'milk' },
          { name: 'sort', label: 'Sort by', value: '-seq' },
        ],
      },
      { method: 'POST', action: '/api/v1/method', label: 'Show the request method', fields: [] },
    ],
  },
  {
    name: 'ssr-site',
    home: '/',
    title: 'Server-rendered site',
    stack: 'Express 4, CommonJS',
    summary:
      'EJS and Pug views, static files with ranges and validators, signed cookies, sessions, flash messages and CSRF-protected forms.',
    links: [
      { path: '/', label: 'Home page' },
      { path: '/login', label: 'Log in as alice / wonderland' },
      { path: '/static/robots.txt', label: 'A static file' },
      { path: '/files/notes.txt', label: 'A file from the data directory' },
      { path: '/theme/dark', label: 'Switch to the dark theme' },
      { path: '/cookies', label: 'Show the cookies' },
      { path: '/req-info', label: 'Request details' },
      { path: '/redirects/3', label: 'Follow three redirects' },
      { path: '/download/notes', label: 'Download the notes' },
    ],
    forms: [],
  },
  {
    name: 'files',
    home: '/uploads',
    title: 'File uploads',
    stack: 'Express 5, ES module',
    summary:
      'Uploads with multer and busboy, streamed request and response bodies, ranges, downloads and gzip of streamed files.',
    links: [
      { path: '/uploads', label: 'List the uploaded files' },
      { path: '/stream/numbers?n=20', label: 'A generated stream' },
      { path: '/stream/chunks', label: 'A chunked response' },
      { path: '/pass-through', label: 'A PassThrough response' },
    ],
    forms: [
      {
        method: 'POST',
        action: '/upload/single',
        enctype: 'multipart/form-data',
        label: 'Upload a file',
        fields: [
          { name: 'file', label: 'File', type: 'file' },
          { name: 'note', label: 'Note', value: 'from the portal' },
        ],
      },
      {
        method: 'POST',
        action: '/upload/text-only',
        enctype: 'multipart/form-data',
        label: 'Post multipart fields',
        fields: [
          { name: 'title', label: 'Title', value: 'Hello' },
          { name: 'body', label: 'Body', value: 'multipart without files' },
        ],
      },
    ],
  },
  {
    name: 'auth',
    home: '/random',
    title: 'Authentication',
    stack: 'Express 5, ES module',
    summary:
      'bcryptjs password hashes, JSON web tokens, passport-local, express-validator and key derivation with node:crypto.',
    links: [
      { path: '/random', label: 'Random bytes and a UUID' },
      { path: '/hash-sync?pw=hello', label: 'Hash a password' },
      { path: '/known-hash', label: 'Check a known bcrypt hash' },
      { path: '/derive?secret=demo', label: 'Derive keys with PBKDF2 and HKDF' },
      { path: '/compare?a=abc&b=abc', label: 'Compare in constant time' },
      { path: '/welcome?email=visitor%40example.com', label: 'The welcome page' },
    ],
    forms: [
      {
        method: 'POST',
        action: '/register',
        label: 'Register',
        fields: [
          { name: 'email', label: 'Email', value: 'visitor@example.com' },
          { name: 'password', label: 'Password', type: 'password', value: 'portal123' },
        ],
      },
      {
        method: 'POST',
        action: '/login-form',
        label: 'Log in',
        fields: [
          { name: 'email', label: 'Email', value: 'visitor@example.com' },
          { name: 'password', label: 'Password', type: 'password', value: 'portal123' },
        ],
      },
    ],
  },
  {
    name: 'data',
    home: '/posts',
    title: 'Data service',
    stack: 'Express 5, ES module',
    summary:
      'A lowdb JSON store, server-sent events, NDJSON export and import, winston and pino logging and dotenv configuration.',
    links: [
      { path: '/config', label: 'The dotenv configuration' },
      { path: '/posts', label: 'List the posts' },
      { path: '/db-file', label: 'The database file' },
      { path: '/export.ndjson', label: 'Export the posts as NDJSON' },
      { path: '/ticks', label: 'A short event stream' },
      { path: '/logs', label: 'The collected logs' },
    ],
    forms: [
      {
        method: 'GET',
        action: '/posts',
        label: 'Page through the posts',
        fields: [{ name: 'page', label: 'Page', value: '2' }],
      },
    ],
  },
];
