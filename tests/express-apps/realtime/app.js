// A realtime chat on Express 4: WebSockets with ws sharing the HTTP server, rooms and broadcasts,
// cookie-session identities checked during the upgrade, long polling, and a JSON history API.
'use strict';

const http = require('http');
const express = require('express');
const cookieSession = require('cookie-session');
const { WebSocketServer, WebSocket } = require('ws');
const { EventEmitter, once } = require('events');

const app = express();
const sessions = cookieSession({ name: 'chat', keys: ['k1', 'k2'], maxAge: 3600_000 });
app.use(sessions);
app.use(express.json());

const history = [];
const errors = [];
const bus = new EventEmitter();
bus.setMaxListeners(100);
let seq = 0;

function post(room, from, text) {
  const message = { seq: ++seq, room, from, text };
  history.push(message);
  bus.emit('message', message);
  return message;
}

app.post('/login', (req, res) => {
  req.session.user = String(req.body.user || 'anon');
  res.json({ user: req.session.user });
});
app.get('/me', (req, res) => res.json({ user: req.session.user || null }));
app.post('/rooms/:room/messages', (req, res) => {
  if (!req.session.user) return res.status(401).json({ error: 'login first' });
  res.status(201).json(post(req.params.room, req.session.user, req.body.text));
});
app.get('/rooms/:room/messages', (req, res) => {
  const since = Number(req.query.since || 0);
  res.json(history.filter((m) => m.room === req.params.room && m.seq > since));
});
// Long polling: responds with the next message of the room, or with 204 after the timeout.
app.get('/rooms/:room/poll', async (req, res) => {
  const timeout = Number(req.query.timeout || 1000);
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), timeout);
  try {
    for (;;) {
      const [message] = await once(bus, 'message', { signal: ac.signal });
      if (message.room === req.params.room) {
        clearTimeout(timer);
        return res.json(message);
      }
    }
  } catch {
    res.status(204).end();
  }
});
app.get('/stats', (req, res) => {
  res.json({ clients: wss.clients.size, messages: history.length, errors });
});

const server = http.createServer(app);
const wss = new WebSocketServer({ noServer: true, maxPayload: 4096 });

server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url, 'http://localhost');
  if (!url.pathname.startsWith('/ws/')) {
    socket.write('HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n');
    socket.destroy();
    return;
  }
  // Read the cookie session the way the HTTP routes do.
  const res = new http.ServerResponse(req);
  sessions(req, res, () => {
    if (!req.session.user) {
      socket.write('HTTP/1.1 401 Unauthorized\r\nConnection: close\r\nContent-Length: 0\r\n\r\n');
      socket.destroy();
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => {
      ws.user = req.session.user;
      ws.room = url.pathname.slice(4);
      wss.emit('connection', ws, req);
    });
  });
});

wss.on('connection', (ws) => {
  const forward = (message) => {
    if (message.room === ws.room && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: 'message', ...message }));
    }
  };
  bus.on('message', forward);
  ws.send(JSON.stringify({ type: 'welcome', user: ws.user, room: ws.room }));
  ws.on('message', (data, isBinary) => {
    if (isBinary) {
      ws.send(Buffer.concat([Buffer.from('echo:'), data]), { binary: true });
      return;
    }
    let parsed;
    try {
      parsed = JSON.parse(data.toString());
    } catch {
      ws.send(JSON.stringify({ type: 'error', error: 'invalid json' }));
      return;
    }
    if (parsed.type === 'ping') ws.send(JSON.stringify({ type: 'pong', id: parsed.id }));
    else if (parsed.type === 'say') post(ws.room, ws.user, parsed.text);
    else if (parsed.type === 'bye') ws.close(4000, 'bye ' + ws.user);
  });
  ws.on('error', (error) => {
    errors.push(error.code);
  });
  ws.on('close', () => bus.off('message', forward));
});

const port = Number(process.env.PORT || 3000);
server.listen(port, () => console.log(`listening on ${port}`));
