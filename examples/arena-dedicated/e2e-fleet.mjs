// Hearth v0.5 fleet end-to-end test.
//
// Run `wrangler dev --port 8790 --persist-to .wrangler/state` in this directory
// first (with .dev.vars present), then: node e2e-fleet.mjs
//
// The script starts the sample fleet allocator itself (fleet/allocator.mjs) and
// builds fleet/fake-server if needed. Checks:
//   1. two players ready up in a lobby; each receives a private hearth.handoff
//      event with a udp:// endpoint and its own single-use ticket,
//   2. the dedicated server redeems both tickets over TCP, the lobby log
//      reaches ended and the winner's /me has a win,
//   3. replaying a used ticket is 403,
//   4. a report with the wrong session secret is 403,
//   5. while a 10 s allocation runs, another lobby intent is answered in
//      under 500 ms (the runtime lock is free during allocation).
import { spawn, spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BASE = process.env.HEARTH_URL ?? 'http://127.0.0.1:8790';
const WS_BASE = BASE.replace(/^http/, 'ws');
const ALLOCATOR = process.env.HEARTH_ALLOCATOR_URL ?? 'http://127.0.0.1:8791';
const ALLOCATOR_SECRET =
  process.env.HEARTH_ALLOCATOR_SECRET ??
  'fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210';
const TIMEOUT_MS = 20_000;

function fail(message) {
  console.error(`e2e-fleet FAILED: ${message}`);
  stopAllocator();
  process.exit(1);
}

function check(condition, message) {
  if (!condition) fail(message);
  console.log(`ok - ${message}`);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function request(pathname, { method = 'GET', body, headers = {} } = {}) {
  const response = await fetch(`${BASE}${pathname}`, {
    method,
    headers: {
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
      ...headers,
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    // not JSON
  }
  return { status: response.status, json, text };
}

// ---------------------------------------------------------------- fake server

function fakeServerBinary() {
  const names = process.platform === 'win32' ? ['fake-server.exe'] : ['fake-server'];
  for (const name of names) {
    const candidate = path.join(HERE, 'fleet', 'fake-server', 'target', 'release', name);
    if (existsSync(candidate)) return candidate;
  }
  console.log('building fleet/fake-server (release)...');
  const result = spawnSync('cargo', ['build', '--release'], {
    cwd: path.join(HERE, 'fleet', 'fake-server'),
    stdio: 'inherit',
  });
  if (result.status !== 0) fail('cargo build --release for fake-server failed');
  for (const name of names) {
    const candidate = path.join(HERE, 'fleet', 'fake-server', 'target', 'release', name);
    if (existsSync(candidate)) return candidate;
  }
  fail('fake-server binary not found after build');
}

// ------------------------------------------------------------------ allocator

let allocatorProcess = null;

function stopAllocator() {
  if (allocatorProcess && !allocatorProcess.killed) {
    allocatorProcess.kill();
    allocatorProcess = null;
  }
}

async function startAllocator() {
  const binary = fakeServerBinary();
  allocatorProcess = spawn(process.execPath, [path.join(HERE, 'fleet', 'allocator.mjs')], {
    env: {
      ...process.env,
      PORT: new URL(ALLOCATOR).port,
      HEARTH_ALLOCATOR_SECRET: ALLOCATOR_SECRET,
      FAKE_SERVER_BIN: binary,
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  for (let attempt = 0; attempt < 50; attempt += 1) {
    try {
      const response = await fetch(`${ALLOCATOR}/health`);
      if (response.ok) return;
    } catch {
      // not listening yet
    }
    await sleep(100);
  }
  fail('allocator did not become healthy');
}

async function allocatorSession(sid) {
  const response = await fetch(`${ALLOCATOR}/session/${encodeURIComponent(sid)}`);
  if (!response.ok) fail(`allocator has no session ${sid}`);
  return response.json();
}

// ------------------------------------------------------------------- protocol

function applyOps(state, ops) {
  const get = (path) => {
    let node = state;
    for (const segment of path) node = node[segment];
    return node;
  };
  const parent = (path) => get(path.slice(0, -1));
  for (const op of ops) {
    const last = op.path[op.path.length - 1];
    switch (op.op) {
      case 'set':
        if (op.path.length === 0) {
          state = op.value;
        } else {
          parent(op.path)[last] = op.value;
        }
        break;
      case 'del':
        if (Array.isArray(parent(op.path))) parent(op.path).splice(last, 1);
        else delete parent(op.path)[last];
        break;
      case 'map_insert':
        parent(op.path)[op.key] = op.value;
        break;
      case 'list_splice':
        get(op.path).splice(op.index, op.delete, ...op.insert);
        break;
      default:
        throw new Error(`unknown patch op ${op.op}`);
    }
  }
  return state;
}

class JsonSocket {
  constructor(url) {
    this.ws = new WebSocket(url);
    this.queue = [];
    this.waiters = [];
    this.state = null;
    this.seq = 0;
    this.closed = null;
    this.ws.addEventListener('message', (event) => {
      const frame = JSON.parse(String(event.data));
      if (frame.op === 'FULL_STATE') {
        this.state = frame.state;
        this.seq = frame.seq;
      } else if (frame.op === 'PATCH') {
        if (frame.seq > this.seq) {
          this.state = applyOps(this.state ?? {}, frame.ops);
          this.seq = frame.seq;
        }
      }
      const waiter = this.waiters.shift();
      if (waiter) waiter(frame);
      else this.queue.push(frame);
    });
    this.ws.addEventListener('close', (event) => {
      this.closed = event.code;
    });
  }

  static async open(url) {
    const socket = new JsonSocket(url);
    await new Promise((resolve, reject) => {
      socket.ws.addEventListener('open', resolve, { once: true });
      socket.ws.addEventListener('error', () => reject(new Error('websocket error')), {
        once: true,
      });
    });
    return socket;
  }

  next(timeoutMs = TIMEOUT_MS) {
    if (this.queue.length > 0) return Promise.resolve(this.queue.shift());
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiters = this.waiters.filter((waiter) => waiter !== onFrame);
        reject(new Error('timed out waiting for a frame'));
      }, timeoutMs);
      const onFrame = (frame) => {
        clearTimeout(timer);
        resolve(frame);
      };
      this.waiters.push(onFrame);
    });
  }

  async waitFor(predicate, description, timeoutMs = TIMEOUT_MS) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const remaining = deadline - Date.now();
      if (remaining <= 0) throw new Error(`timed out waiting for ${description}`);
      const frame = await this.next(remaining);
      if (predicate(frame)) return frame;
    }
  }

  async waitForState(predicate, description, timeoutMs = TIMEOUT_MS) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (this.state !== null && predicate(this.state)) return this.state;
      const remaining = deadline - Date.now();
      if (remaining <= 0) throw new Error(`timed out waiting for ${description}`);
      await this.next(remaining);
    }
  }

  send(frame) {
    this.ws.send(JSON.stringify(frame));
  }

  close() {
    try {
      this.ws.close();
    } catch {
      // already closed
    }
  }
}

let deviceCounter = 0;

function deviceId() {
  deviceCounter += 1;
  return `e2e-fleet-${process.pid}-${deviceCounter}-${Math.random().toString(16).slice(2, 10)}`;
}

async function anonymousToken() {
  const auth = await request('/auth/anonymous', {
    method: 'POST',
    body: { device_id: deviceId() },
  });
  check(auth.status === 200, `anonymous auth (${auth.status})`);
  return auth.json;
}

async function player(room, schemaHash) {
  const auth = await anonymousToken();
  const ws = await JsonSocket.open(
    `${WS_BASE}/rooms/arena/${encodeURIComponent(room)}?token=${encodeURIComponent(auth.token)}`,
  );
  ws.send({ op: 'HELLO', protocol: 1, schema_hash: schemaHash, opts: {} });
  const joined = await ws.waitFor((frame) => frame.op === 'JOIN_OK', 'JOIN_OK');
  await ws.waitFor((frame) => frame.op === 'FULL_STATE', 'FULL_STATE');
  return { ws, token: auth.token, playerId: joined.player_id, seq: 0 };
}

function ready(who) {
  who.seq += 1;
  who.ws.send({ op: 'INTENT', intents: [{ seq: who.seq, intent: 'Ready' }] });
}

function unready(who) {
  who.seq += 1;
  who.ws.send({ op: 'INTENT', intents: [{ seq: who.seq, intent: 'Unready' }] });
  return who.seq;
}

function handoffEvents(who) {
  return who.ws.queue
    .concat(who.ws.state ? [] : [])
    .filter((frame) => frame.op === 'EVENT' && frame.name === 'hearth.handoff');
}

async function waitHandoff(who, timeoutMs = TIMEOUT_MS) {
  const frame = await who.ws.waitFor(
    (candidate) => candidate.op === 'EVENT' && candidate.name === 'hearth.handoff',
    'hearth.handoff event',
    timeoutMs,
  );
  return frame.data;
}

async function mintRoom() {
  const token = (await anonymousToken()).token;
  const minted = await request('/rooms/arena', {
    method: 'POST',
    headers: { authorization: `Bearer ${token}` },
  });
  check(minted.status === 200 && typeof minted.json.room === 'string', 'mint a lobby room');
  return minted.json.room;
}

// ------------------------------------------------------------ ticket delivery

function sendTicketOnce(port, ticket) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(port, '127.0.0.1');
    let buffer = '';
    socket.on('connect', () => socket.write(`TICKET ${ticket}\n`));
    socket.on('data', (chunk) => {
      buffer += chunk.toString('utf8');
      if (buffer.includes('\n')) {
        socket.end();
        resolve(buffer.trim());
      }
    });
    socket.on('error', reject);
    socket.setTimeout(5_000, () => reject(new Error('ticket TCP timed out')));
  });
}

async function sendTicket(port, ticket) {
  let lastError = null;
  for (let attempt = 0; attempt < 80; attempt += 1) {
    try {
      return await sendTicketOnce(port, ticket);
    } catch (error) {
      lastError = error;
      if (error?.code !== 'ECONNREFUSED') break;
      await sleep(100);
    }
  }
  throw lastError;
}

async function deliverTickets(items) {
  const responses = [];
  for (const item of items) {
    responses.push(await sendTicket(item.port, item.ticket));
  }
  return responses;
}

function endpointPort(endpoint) {
  const match = /^udp:\/\/127\.0\.0\.1:(\d+)$/.exec(endpoint);
  if (!match) fail(`bad endpoint ${endpoint}`);
  return Number(match[1]);
}

// ------------------------------------------------------------------- scenario

async function run() {
  const schema = await request('/schema/arena');
  check(schema.status === 200, 'schema route');
  const schemaHash = schema.json.schema_hash;

  // --- check 1: two players get private endpoint + tickets ------------------
  const roomA = await mintRoom();
  const a = await player(roomA, schemaHash);
  const b = await player(roomA, schemaHash);
  ready(a);
  ready(b);
  const handoffA = await waitHandoff(a);
  const handoffB = await waitHandoff(b);
  check(
    typeof handoffA.endpoint === 'string' && handoffA.endpoint.startsWith('udp://127.0.0.1:'),
    `player A got an endpoint (${handoffA.endpoint})`,
  );
  check(handoffA.sid === handoffB.sid, 'both players are in the same session');
  check(handoffA.ticket !== handoffB.ticket, 'each player got its own ticket');
  check(
    !JSON.stringify(handoffA).includes(handoffB.ticket),
    "player A's event does not carry player B's ticket",
  );
  const session = await allocatorSession(handoffA.sid);

  // --- check 2: redeem the first ticket, then prove replay/wrong secret fail
  // while the session is still started (a fresh session, since an ended one
  // answers `Gone` before it can distinguish a used ticket).
  const port = endpointPort(handoffA.endpoint);
  const firstReply = await sendTicket(port, handoffA.ticket);
  check(firstReply.startsWith('OK '), 'the dedicated server redeemed ticket A');
  const replay = await request(`/handoff/${encodeURIComponent(handoffA.sid)}/redeem`, {
    method: 'POST',
    headers: { 'x-hearth-session-secret': session.secret },
    body: { ticket: handoffA.ticket },
  });
  check(replay.status === 403, `ticket replay is 403 (got ${replay.status})`);
  const forged = await request(`/handoff/${encodeURIComponent(handoffA.sid)}/report`, {
    method: 'POST',
    headers: { 'x-hearth-session-secret': 'nope' },
    body: { type: 'started' },
  });
  check(forged.status === 403, `wrong-secret report is 403 (got ${forged.status})`);
  const accepted = await request(`/handoff/${encodeURIComponent(handoffA.sid)}/report`, {
    method: 'POST',
    headers: { 'x-hearth-session-secret': session.secret },
    body: { type: 'heartbeat' },
  });
  check(accepted.status === 204, `the real secret is accepted (204, got ${accepted.status})`);
  const secondReply = await sendTicket(port, handoffB.ticket);
  check(secondReply.startsWith('OK '), 'the dedicated server redeemed ticket B');
  const stateA = await a.ws.waitForState(
    (state) => Array.isArray(state.log) && state.log.includes('ended'),
    'lobby log ended',
  );
  check(stateA.log.includes('result'), 'the lobby saw the result event');
  check(stateA.result.includes('winner'), 'the result names a winner');
  const winnerId = JSON.parse(stateA.result).winner;
  let wins = 0;
  for (let attempt = 0; attempt < 40 && wins === 0; attempt += 1) {
    const me = await request('/me', { headers: { authorization: `Bearer ${a.token}` } });
    if (me.status === 200) wins = Math.max(wins, me.json.wins ?? 0);
    if (wins === 0) await sleep(100);
  }
  check(winnerId === a.playerId, `the first redeeming player (A) won: ${winnerId}`);
  check(wins >= 1, "the winner's /me shows a win");

  // --- check 5: a slow allocation does not block the lobby ------------------
  const slow = await fetch(`${ALLOCATOR}/control/slow`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ slow: true }),
  });
  check(slow.ok, 'allocator switched to slow mode');
  const roomB = await mintRoom();
  const c = await player(roomB, schemaHash);
  const d = await player(roomB, schemaHash);
  ready(c);
  ready(d);
  await sleep(300);
  const started = Date.now();
  const intentSeq = unready(c);
  const ack = await c.ws.waitFor(
    (frame) =>
      frame.op === 'ACK' && Array.isArray(frame.intent_seqs) && frame.intent_seqs.includes(intentSeq),
    'ACK for the intent sent during allocation',
  );
  const latency = Date.now() - started;
  check(ack.op === 'ACK', 'the intent sent during allocation was ACKed');
  check(latency < 500, `lobby intent answered during a 10 s allocation (${latency} ms < 500 ms)`);

  const slowA = await waitHandoff(c, 25_000);
  const slowB = await waitHandoff(d, 25_000);
  const elapsed = Date.now() - started;
  check(elapsed > 9_000, `the allocation really was slow (${elapsed} ms)`);
  const slowSession = await allocatorSession(slowA.sid);
  const slowPort = endpointPort(slowA.endpoint);
  await deliverTickets([
    { port: slowPort, ticket: slowA.ticket },
    { port: slowPort, ticket: slowB.ticket },
  ]);
  await c.ws.waitForState(
    (state) => Array.isArray(state.log) && state.log.includes('ended'),
    'slow lobby log ended',
    25_000,
  );
  check(slowSession.secret.length >= 32, 'the slow session had a real secret');
  await fetch(`${ALLOCATOR}/control/slow`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ slow: false }),
  });

  a.ws.close();
  b.ws.close();
  c.ws.close();
  d.ws.close();
  console.log('e2e-fleet ok');
}

await startAllocator();
try {
  await run();
} catch (error) {
  fail(error instanceof Error ? error.message : String(error));
} finally {
  stopAllocator();
}
process.exit(0);
