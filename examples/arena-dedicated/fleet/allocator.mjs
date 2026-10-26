// Hearth v0.5 sample fleet allocator: the webhook the room's adapter calls.
//
// This is a local-only reference allocator. It verifies the signed webhook,
// spawns one `fake-server` process per session and answers the session's
// endpoint. In production you would replace this with your real fleet control
// plane (Kubernetes, GameLift, Agones, ...) using the same signed contract.
//
// Env:
//   PORT                      listen port (default 8791)
//   HEARTH_ALLOCATOR_SECRET   the shared HMAC secret (>= 32 bytes)
//   FAKE_SERVER_BIN           path to the built fake-server binary
//   LISTEN_BASE               first TCP port handed to a fake server (default 9300)
//
// Endpoints:
//   POST /allocate            the signed webhook (allocate or stop)
//   GET  /health              readiness for e2e
//   GET  /session/<sid>       dev-only: the session's secret/endpoint/report url
//   POST /control/slow        dev-only: {"slow": true|false}; 10 s allocations
import { createHmac, timingSafeEqual } from 'node:crypto';
import { spawn } from 'node:child_process';
import http from 'node:http';

const PORT = Number(process.env.PORT ?? 8791);
const SECRET = process.env.HEARTH_ALLOCATOR_SECRET ?? '';
const FAKE_SERVER_BIN = process.env.FAKE_SERVER_BIN ?? '';
const LISTEN_BASE = Number(process.env.LISTEN_BASE ?? 9300);
const SKEW_S = 300;
const SLOW_MS = 10_000;

if (SECRET.length < 32) {
  console.error('allocator: HEARTH_ALLOCATOR_SECRET must be at least 32 bytes');
  process.exit(2);
}
if (!FAKE_SERVER_BIN) {
  console.error('allocator: FAKE_SERVER_BIN is required');
  process.exit(2);
}

/** sid -> { endpoint, secret, reportUrl, port, child } */
const sessions = new Map();
const children = new Set();
let slow = false;
let nextPort = LISTEN_BASE;

function sign(timestamp, body) {
  return createHmac('sha256', SECRET).update(`${timestamp}.${body}`).digest('hex');
}

function verify(headers, body) {
  const timestamp = headers['x-hearth-timestamp'];
  const signature = headers['x-hearth-signature'];
  if (!timestamp || !signature) return false;
  const now = Math.floor(Date.now() / 1000);
  if (Math.abs(now - Number(timestamp)) > SKEW_S) return false;
  const expected = Buffer.from(sign(timestamp, body), 'hex');
  const given = Buffer.from(String(signature), 'hex');
  return expected.length === given.length && timingSafeEqual(expected, given);
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on('data', (chunk) => chunks.push(chunk));
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

function json(res, status, value) {
  const body = JSON.stringify(value);
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(body);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function freePort() {
  const port = nextPort;
  nextPort += 1;
  return port;
}

function spawnServer(session) {
  const child = spawn(FAKE_SERVER_BIN, [], {
    env: {
      ...process.env,
      HEARTH_SID: session.sid,
      HEARTH_SESSION_SECRET: session.secret,
      HEARTH_REPORT_URL: session.reportUrl,
      HEARTH_LISTEN_PORT: String(session.port),
      HEARTH_PLAYERS: JSON.stringify(session.players ?? []),
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  children.add(child);
  child.stdout.on('data', (chunk) => process.stdout.write(`[fake-server ${session.sid}] ${chunk}`));
  child.stderr.on('data', (chunk) => process.stderr.write(`[fake-server ${session.sid}] ${chunk}`));
  child.on('exit', () => children.delete(child));
  return child;
}

function allocate(body) {
  const sid = body.sid;
  const existing = sessions.get(sid);
  if (existing) return { status: 409, endpoint: existing.endpoint };
  const port = freePort();
  const session = {
    sid,
    secret: body.session_secret,
    reportUrl: body.report_url,
    players: body.players,
    port,
    endpoint: `udp://127.0.0.1:${port}`,
  };
  session.child = spawnServer(session);
  sessions.set(sid, session);
  return { status: 200, endpoint: session.endpoint };
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);
  if (req.method === 'GET' && url.pathname === '/health') {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('ok');
    return;
  }
  if (req.method === 'GET' && url.pathname.startsWith('/session/')) {
    const sid = decodeURIComponent(url.pathname.slice('/session/'.length));
    const session = sessions.get(sid);
    if (!session) {
      json(res, 404, { error: 'no such session' });
      return;
    }
    json(res, 200, {
      sid,
      secret: session.secret,
      endpoint: session.endpoint,
      report_url: session.reportUrl,
    });
    return;
  }
  if (req.method === 'POST' && url.pathname === '/control/slow') {
    const body = JSON.parse((await readBody(req)) || '{}');
    slow = body.slow === true;
    json(res, 200, { slow });
    return;
  }
  if (req.method !== 'POST' || url.pathname !== '/allocate') {
    json(res, 404, { error: 'not found' });
    return;
  }
  const raw = await readBody(req);
  if (!verify(req.headers, raw)) {
    json(res, 401, { error: 'bad signature' });
    return;
  }
  let body;
  try {
    body = JSON.parse(raw);
  } catch {
    json(res, 400, { error: 'bad json' });
    return;
  }
  if (body.action === 'stop') {
    const session = sessions.get(body.sid);
    if (session?.child) session.child.kill();
    sessions.delete(body.sid);
    json(res, 200, { stopped: true });
    return;
  }
  if (slow) {
    console.log(`allocator: slow allocation for ${body.sid}`);
    await sleep(SLOW_MS);
  }
  const answer = allocate(body);
  console.log(`allocator: ${answer.status} ${answer.endpoint} for ${body.sid}`);
  json(res, answer.status, { endpoint: answer.endpoint });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`allocator listening on http://127.0.0.1:${PORT}`);
});

function shutdown() {
  for (const child of children) child.kill();
  server.close(() => process.exit(0));
}
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
