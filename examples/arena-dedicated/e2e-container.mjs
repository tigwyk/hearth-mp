// Hearth v0.5 Cloudflare Containers end-to-end test.
//
// Start Docker, then run `wrangler dev --port 8792 --persist-to
// .wrangler/state-container --var HEARTH_PUBLIC_URL:http://127.0.0.1:8792` in
// this directory (the container image is built from the repo root, see
// wrangler.toml), then: node e2e-container.mjs
//
// PLATFORM CAVEAT: wrangler dev cannot run local containers on Windows ("Local
// development with containers is currently not supported on Windows"). Run this
// test on Linux, inside WSL, on macOS, or against a deployed Worker (ask before
// deploying: containers bill per running second). The script exits 2 with a
// clear message on Windows against a loopback URL.
//
// Checks (plan Task 3 Step 3):
//   1. two players ready up, get `/handoff/<sid>/connect` endpoints and connect
//      with their own tickets,
//   2. both receive the dedicated server's 30 Hz time broadcasts, and the
//      server's hello echoes the identity the gateway redeemed,
//   3. the lobby's synced `log` reaches `ended` with a `result`,
//   4. reconnecting with an already-used ticket is 403 before any upgrade,
//   5. a forged `X-Hearth-Player-Id` header is ignored (the server sees the
//      redeemed identity),
//   6. killing the container mid-match leads to `failed` after the 60 s
//      heartbeat timeout, and both players get `hearth.handoff_ended`.
//
// Requires Docker (`docker ps` must work) and `wrangler dev` with local
// container support.
import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BASE = process.env.HEARTH_URL ?? 'http://127.0.0.1:8792';
const WS_BASE = BASE.replace(/^http/, 'ws');
const PORT = Number(new URL(BASE).port);
const TIMEOUT_MS = 20_000;
/// The room's heartbeat timeout is 60 s; leave room for the report round trips.
const LOST_TIMEOUT_MS = 90_000;

if (
  process.platform === 'win32' &&
  /^(127\.0\.0\.1|localhost)$/i.test(new URL(BASE).hostname)
) {
  console.error(
    'e2e-container SKIPPED: wrangler dev cannot run local Cloudflare Containers on Windows.',
  );
  console.error('  Use Linux, WSL, or macOS locally; ask before deploying a preview.');
  process.exit(2);
}

const openSockets = [];

function fail(message) {
  console.error(`e2e-container FAILED: ${message}`);
  for (const socket of openSockets) {
    try {
      socket.close();
    } catch {
      // already closed
    }
  }
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

// --------------------------------------------------------------- room protocol

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

/// A room WebSocket that keeps the synced state and can wait for frames.
class JsonSocket {
  constructor(url) {
    this.ws = new WebSocket(url);
    this.queue = [];
    this.waiters = [];
    this.state = null;
    this.seq = 0;
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
    this.ws.addEventListener('close', () => {
      this.closed = true;
    });
  }

  static async open(url) {
    const socket = new JsonSocket(url);
    openSockets.push(socket);
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
        reject(new Error('timed out waiting for a room frame'));
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

/// A forwarded dedicated-server socket (the container's 1-byte/tick stream).
class FrameSocket {
  constructor(url) {
    this.ws = new WebSocket(url);
    this.queue = [];
    this.waiters = [];
    this.ws.addEventListener('message', (event) => {
      let frame;
      try {
        frame = JSON.parse(String(event.data));
      } catch {
        return;
      }
      const waiter = this.waiters.shift();
      if (waiter) waiter(frame);
      else this.queue.push(frame);
    });
  }

  static async open(url) {
    const socket = new FrameSocket(url);
    openSockets.push(socket);
    await new Promise((resolve, reject) => {
      socket.ws.addEventListener('open', resolve, { once: true });
      socket.ws.addEventListener('error', () => reject(new Error(`websocket failed: ${url}`)), {
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
        reject(new Error('timed out waiting for a dedicated-server frame'));
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

  close() {
    try {
      this.ws.close();
    } catch {
      // already closed
    }
  }
}

/// A raw WebSocket upgrade: needed for checks 4 and 5, where the request must
/// carry headers (a used ticket, a forged identity) that the browser WebSocket
/// API cannot set. Returns the HTTP answer; on 101, `nextFrame()` reads frames.
function rawUpgrade(pathname, extraHeaders = {}) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(PORT, '127.0.0.1');
    const key = crypto.randomBytes(16).toString('base64');
    const headers = {
      Host: `127.0.0.1:${PORT}`,
      Connection: 'Upgrade',
      Upgrade: 'websocket',
      'Sec-WebSocket-Version': '13',
      'Sec-WebSocket-Key': key,
      ...extraHeaders,
    };
    let buffer = Buffer.alloc(0);
    let upgraded = false;
    let settle = { resolve, reject };
    const state = { status: 0, headers: {}, frames: [], closed: false };
    const waiters = [];
    const pushFrame = (frame) => {
      state.frames.push(frame);
      const waiter = waiters.shift();
      if (waiter) waiter(frame);
    };
    state.nextFrame = (timeoutMs = TIMEOUT_MS) =>
      new Promise((res, rej) => {
        if (state.frames.length > 0) return res(state.frames.shift());
        const timer = setTimeout(
          () => rej(new Error('timed out waiting for a raw ws frame')),
          timeoutMs,
        );
        waiters.push((frame) => {
          clearTimeout(timer);
          res(frame);
        });
      });
    state.close = () => {
      try {
        socket.destroy();
      } catch {
        // already gone
      }
    };
    openSockets.push(state);

    socket.on('connect', () => {
      const lines = [`GET ${pathname} HTTP/1.1`];
      for (const [name, value] of Object.entries(headers)) lines.push(`${name}: ${value}`);
      lines.push('', '');
      socket.write(lines.join('\r\n'));
    });
    socket.on('data', (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      if (!upgraded) {
        const end = buffer.indexOf('\r\n\r\n');
        if (end < 0) return;
        const head = buffer.subarray(0, end).toString('utf8').split('\r\n');
        state.status = Number(head[0].split(' ')[1]);
        for (const line of head.slice(1)) {
          const colon = line.indexOf(':');
          if (colon > 0) {
            state.headers[line.slice(0, colon).trim().toLowerCase()] = line
              .slice(colon + 1)
              .trim();
          }
        }
        buffer = buffer.subarray(end + 4);
        upgraded = true;
        if (state.status !== 101) {
          socket.destroy();
          settle.resolve(state);
          settle = { resolve: () => {}, reject: () => {} };
          return;
        }
        settle.resolve(state);
        settle = { resolve: () => {}, reject: () => {} };
      }
      // Server frames are unmasked (RFC 6455: only client frames are masked).
      for (;;) {
        if (buffer.length < 2) return;
        const opcode = buffer[0] & 0x0f;
        let length = buffer[1] & 0x7f;
        let offset = 2;
        if (length === 126) {
          if (buffer.length < 4) return;
          length = buffer.readUInt16BE(2);
          offset = 4;
        } else if (length === 127) {
          if (buffer.length < 10) return;
          length = Number(buffer.readBigUInt64BE(2));
          offset = 10;
        }
        if (buffer.length < offset + length) return;
        const payload = buffer.subarray(offset, offset + length);
        buffer = buffer.subarray(offset + length);
        if (opcode === 0x1) {
          try {
            pushFrame(JSON.parse(payload.toString('utf8')));
          } catch {
            // a non-JSON frame: the sample does not send any
          }
        } else if (opcode === 0x8) {
          state.closed = true;
          socket.destroy();
          return;
        }
      }
    });
    socket.on('error', (error) => {
      settle.reject(error);
      settle = { resolve: () => {}, reject: () => {} };
    });
    socket.on('close', () => {
      state.closed = true;
    });
  });
}

// ------------------------------------------------------------------ scenario

let deviceCounter = 0;

function deviceId() {
  deviceCounter += 1;
  return `e2e-container-${process.pid}-${deviceCounter}-${Math.random().toString(16).slice(2, 10)}`;
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
  return { ws, playerId: joined.player_id };
}

function ready(who) {
  who.seq = (who.seq ?? 0) + 1;
  who.ws.send({ op: 'INTENT', intents: [{ seq: who.seq, intent: 'Ready' }] });
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

/// Create a lobby, seat two players, make both ready, and return their handoff
/// tickets. Does not connect to the session container.
async function readySession(schemaHash, label) {
  const room = await mintRoom();
  const first = await player(room, schemaHash);
  const second = await player(room, schemaHash);
  ready(first);
  ready(second);
  const handoffA = await waitHandoff(first);
  const handoffB = await waitHandoff(second);
  console.log(`-- ${label}: session ${handoffA.sid}`);
  return { room, first, second, handoffA, handoffB };
}

function endpointPath(handoff) {
  const endpoint = handoff.endpoint;
  if (!endpoint.startsWith(`${WS_BASE}/handoff/`)) fail(`bad endpoint ${endpoint}`);
  return endpoint;
}

function socketUrl(handoff, ticket) {
  return `${endpointPath(handoff)}?ticket=${encodeURIComponent(ticket)}`;
}

// ---------------------------------------------------------------- docker kill

function docker(args) {
  return spawnSync('docker', args, { encoding: 'utf8' });
}

function dockerContainerIds() {
  const result = docker(['ps', '-q', '--no-trunc']);
  if (result.status !== 0) fail(`docker ps failed: ${result.stderr.trim()}`);
  return new Set(result.stdout.split(/\s+/).filter(Boolean));
}

async function killNewContainer(before) {
  for (let attempt = 0; attempt < 300; attempt += 1) {
    const now = dockerContainerIds();
    const fresh = [...now].filter((id) => !before.has(id));
    if (fresh.length > 0) {
      const id = fresh[0];
      console.log(`-- killing container ${id}`);
      const killed = docker(['kill', id]);
      if (killed.status !== 0) fail(`docker kill ${id} failed: ${killed.stderr.trim()}`);
      return id;
    }
    await sleep(200);
  }
  fail('no new container appeared to kill');
}

// ------------------------------------------------------------------ run

async function run() {
  const schema = await request('/schema/arena');
  check(schema.status === 200, 'schema route');
  const schemaHash = schema.json.schema_hash;
  const dockerProbe = docker(['ps']);
  if (dockerProbe.status !== 0) fail('docker is not running (check `docker ps`)');

  // --- checks 1-3: a full match through the container proxy ------------------
  const one = await readySession(schemaHash, 'check 1: container handoff');
  check(
    one.handoffA.endpoint.startsWith(`${WS_BASE}/handoff/${one.handoffA.sid}/connect`),
    `players got the gateway connect endpoint (${one.handoffA.endpoint})`,
  );
  check(one.handoffA.sid === one.handoffB.sid, 'both players are in the same session');
  check(one.handoffA.ticket !== one.handoffB.ticket, 'each player got its own ticket');
  check(
    !JSON.stringify(one.handoffA).includes(one.handoffB.ticket),
    "player A's event does not carry player B's ticket",
  );

  const containerA = await FrameSocket.open(socketUrl(one.handoffA, one.handoffA.ticket));
  const containerB = await FrameSocket.open(socketUrl(one.handoffB, one.handoffB.ticket));
  const helloA = await containerA.waitFor((frame) => frame.type === 'hello', 'server hello A');
  const helloB = await containerB.waitFor((frame) => frame.type === 'hello', 'server hello B');
  check(
    helloA.player_id === one.first.playerId && helloB.player_id === one.second.playerId,
    'the container echoes each redeemed player identity',
  );

  // --- check 2: both receive the 30 Hz time broadcasts ----------------------
  const ticksA = [];
  const ticksB = [];
  const deadline = Date.now() + 8_000;
  while (Date.now() < deadline) {
    for (const [socket, ticks] of [
      [containerA, ticksA],
      [containerB, ticksB],
    ]) {
      if (socket.queue.length > 0) {
        const frame = await socket.next(1_000).catch(() => null);
        if (frame && frame.type === 'tick') ticks.push(frame.t);
        continue;
      }
      await sleep(5);
    }
  }
  check(ticksA.length >= 60, `player A received the 30 Hz broadcast (${ticksA.length} ticks)`);
  check(ticksB.length >= 60, `player B received the 30 Hz broadcast (${ticksB.length} ticks)`);
  check(
    ticksA.every((value, index) => index === 0 || value >= ticksA[index - 1]),
    'the broadcast timestamps are monotonic',
  );

  // --- check 3: the lobby reaches ended with a result -----------------------
  const ended = await one.first.ws.waitForState(
    (state) =>
      Array.isArray(state.log) &&
      state.log.includes('ended') &&
      typeof state.result === 'string' &&
      state.result.length > 0,
    'lobby log ended with a result',
    30_000,
  );
  check(
    ended.log.includes('started') && ended.log.includes('player_joined'),
    `lobby lifecycle log: ${ended.log.join(',')}`,
  );
  check(ended.result.includes('winner'), `lobby stored the result (${ended.result})`);

  // --- checks 4 and 5: used tickets and forged identity ---------------------
  const two = await readySession(schemaHash, 'checks 4-5: redeem and identity');
  // Use player A's ticket, then replay it: the session is still started, so the
  // room answers 403 (a used ticket) before the gateway touches the container.
  const used = await FrameSocket.open(socketUrl(two.handoffA, two.handoffA.ticket));
  await used.waitFor((frame) => frame.type === 'hello', 'first hello for the used ticket');
  used.close();
  const replay = await rawUpgrade(
    `/handoff/${encodeURIComponent(two.handoffA.sid)}/connect?ticket=${encodeURIComponent(
      two.handoffA.ticket,
    )}`,
  );
  check(replay.status === 403, `replaying a used ticket is 403 (${replay.status})`);
  replay.close();

  // Player B's ticket is still unused; a forged identity header must be dropped
  // and replaced with the identity the gateway redeemed.
  const forged = await rawUpgrade(
    `/handoff/${encodeURIComponent(two.handoffA.sid)}/connect?ticket=${encodeURIComponent(
      two.handoffB.ticket,
    )}`,
    {
      'X-Hearth-Player-Id': 'attacker',
      'X-Hearth-Provider': 'evil',
      'X-Hearth-Display-Name': 'attacker',
    },
  );
  if (forged.status !== 101) fail(`forged-identity upgrade failed (${forged.status})`);
  const forgedHello = await forged.nextFrame();
  check(
    forgedHello.type === 'hello' && forgedHello.player_id === two.second.playerId,
    `the container saw the redeemed identity, not the forged header (${forgedHello.player_id})`,
  );
  check(
    forgedHello.provider !== 'evil',
    `the container saw the redeemed provider (${forgedHello.provider})`,
  );
  forged.close();
  for (const who of [one.first, one.second, two.first, two.second]) who.ws.close();

  // --- check 6: kill the container; the room must report the loss -----------
  const before = dockerContainerIds();
  const three = await readySession(schemaHash, 'check 6: kill mid-match');
  const liveA = await FrameSocket.open(socketUrl(three.handoffA, three.handoffA.ticket));
  const liveB = await FrameSocket.open(socketUrl(three.handoffB, three.handoffB.ticket));
  await liveA.waitFor((frame) => frame.type === 'hello', 'live hello A');
  await liveB.waitFor((frame) => frame.type === 'hello', 'live hello B');
  await killNewContainer(before);

  const failed = await three.first.ws.waitForState(
    (state) => Array.isArray(state.log) && state.log.includes('failed'),
    'lobby failed state after the kill',
    LOST_TIMEOUT_MS,
  );
  check(!failed.log.includes('ended'), 'the killed session did not report a clean end');
  for (const [who, label] of [
    [three.first, 'A'],
    [three.second, 'B'],
  ]) {
    const frame = await who.ws.waitFor(
      (candidate) => candidate.op === 'EVENT' && candidate.name === 'hearth.handoff_ended',
      `hearth.handoff_ended for player ${label}`,
      LOST_TIMEOUT_MS,
    );
    check(frame.data?.reason === 'lost', `player ${label} was told the server was lost`);
  }
  liveA.close();
  liveB.close();
  three.first.ws.close();
  three.second.ws.close();

  console.log('\ne2e-container ok');
}

run().catch((error) => {
  fail(String(error?.stack ?? error));
});
