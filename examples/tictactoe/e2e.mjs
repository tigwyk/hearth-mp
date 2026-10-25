// Hearth end-to-end test for the tictactoe sample (v0.1 through v0.3b).
//
// Requires `wrangler dev` in examples/tictactoe (default http://127.0.0.1:8787).
// Run: node e2e.mjs
//   HEARTH_ADMIN_TOKEN must match the one in examples/tictactoe/.dev.vars for the
//   admin checks. E2E_NO_ADMIN=1 runs the "admin API disabled" phase against a
//   server started without HEARTH_ADMIN_TOKEN. E2E_RATELIMIT=1 runs only the
//   rate-limit phase against a server started from `wrangler.e2e-ratelimit.toml`,
//   whose tiny limits are what make a 429 reachable in a few requests.
//   E2E_KEEP_ROOM=1 skips the force-dispose step (for `hearth inspect` by hand).
// Exits 0 on success; prints the failed check and exits 1 otherwise.
import http from 'node:http';
import { generateKeyPairSync, randomBytes } from 'node:crypto';
import { ensureKeys, mintJwt, startJwksServer, EC_KID, PORT as JWKS_PORT, RSA_KID } from './jwks.mjs';

const BASE = process.env.HEARTH_URL ?? 'http://127.0.0.1:8787';
const WS_BASE = BASE.replace(/^http/, 'ws');
const TIMEOUT_MS = 10_000;
const ADMIN_TOKEN = process.env.HEARTH_ADMIN_TOKEN ?? '';
const NO_ADMIN = process.env.E2E_NO_ADMIN === '1';
const RATELIMIT = process.env.E2E_RATELIMIT === '1';
const JWT = process.env.E2E_JWT === '1';
const KEEP_ROOM = process.env.E2E_KEEP_ROOM === '1';

function fail(message) {
  console.error(`e2e FAILED: ${message}`);
  process.exit(1);
}

function check(condition, message) {
  if (!condition) fail(message);
  console.log(`ok - ${message}`);
}

async function request(path, { method = 'GET', body, token, upgrade = false } = {}) {
  const headers = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (token) headers.authorization = `Bearer ${token}`;
  if (upgrade) headers.upgrade = 'websocket';
  const response = await fetch(`${BASE}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    // not JSON; leave as text for the caller
  }
  return { status: response.status, json, text, headers: response.headers };
}

function openSocket(url) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url);
    ws.addEventListener('open', () => resolve(ws), { once: true });
    ws.addEventListener(
      'error',
      () => reject(new Error(`websocket error for ${url}`)),
      { once: true },
    );
  });
}

// --- Patch application (mirrors docs/protocol.md; also validates server patches) ---

function resolvePath(state, path) {
  let cursor = state;
  for (const segment of path) {
    if (typeof segment === 'string' && cursor !== null && typeof cursor === 'object' && !Array.isArray(cursor)) {
      cursor = cursor[segment];
    } else if (typeof segment === 'number' && Array.isArray(cursor)) {
      cursor = cursor[segment];
    } else {
      throw new Error(`bad patch path ${JSON.stringify(path)}`);
    }
    if (cursor === undefined) throw new Error(`bad patch path ${JSON.stringify(path)}`);
  }
  return cursor;
}

function applyPatch(state, op) {
  const path = op.path ?? [];
  const parentPath = path.slice(0, -1);
  const last = path[path.length - 1];
  switch (op.op) {
    case 'set': {
      if (path.length === 0) return op.value;
      const parent = resolvePath(state, parentPath);
      if (typeof last === 'string' && parent !== null && typeof parent === 'object' && !Array.isArray(parent)) {
        parent[last] = op.value;
      } else if (typeof last === 'number' && Array.isArray(parent) && last < parent.length) {
        parent[last] = op.value;
      } else {
        throw new Error(`bad set path ${JSON.stringify(path)}`);
      }
      return state;
    }
    case 'del': {
      if (path.length === 0) throw new Error('cannot delete the root');
      const parent = resolvePath(state, parentPath);
      if (typeof last === 'string' && parent && typeof parent === 'object' && !Array.isArray(parent)) {
        if (!(last in parent)) throw new Error(`bad del path ${JSON.stringify(path)}`);
        delete parent[last];
      } else if (typeof last === 'number' && Array.isArray(parent) && last < parent.length) {
        parent.splice(last, 1);
      } else {
        throw new Error(`bad del path ${JSON.stringify(path)}`);
      }
      return state;
    }
    case 'map_insert': {
      const target = resolvePath(state, path);
      if (target === null || typeof target !== 'object' || Array.isArray(target)) {
        throw new Error(`bad map_insert path ${JSON.stringify(path)}`);
      }
      target[op.key] = op.value;
      return state;
    }
    case 'list_splice': {
      const target = resolvePath(state, path);
      if (!Array.isArray(target)) throw new Error(`bad list_splice path ${JSON.stringify(path)}`);
      if (op.index > target.length || op.index + op.delete > target.length) {
        throw new Error(`bad list_splice path ${JSON.stringify(path)}`);
      }
      target.splice(op.index, op.delete, ...op.insert);
      return state;
    }
    default:
      throw new Error(`unknown patch op ${op.op}`);
  }
}

/** A JSON-mode client for one Hearth socket. Tracks state from FULL_STATE/PATCH. */
class JsonSocket {
  constructor(ws) {
    this.ws = ws;
    this.queue = [];
    this.waiters = [];
    this.state = null;
    this.seq = -1;
    this.closed = null;
    ws.addEventListener('message', (event) => {
      const data = event.data;
      if (typeof data !== 'string') return;
      let frame;
      try {
        frame = JSON.parse(data);
      } catch {
        frame = { op: 'TEXT', text: data };
      }
      if (frame.op === 'FULL_STATE') {
        this.state = frame.state;
        this.seq = frame.seq;
      } else if (frame.op === 'PATCH' && frame.seq > this.seq && this.state !== null) {
        try {
          for (const op of frame.ops) this.state = applyPatch(this.state, op);
          this.seq = frame.seq;
        } catch (error) {
          fail(`client could not apply a server patch: ${error.message}`);
        }
      }
      this.#push(frame);
    });
    ws.addEventListener('close', (event) => {
      this.closed = { code: event.code, reason: event.reason };
      this.#push({ op: 'SOCKET_CLOSE', code: event.code, reason: event.reason });
    });
  }

  #push(frame) {
    const waiter = this.waiters.shift();
    if (waiter) waiter(frame);
    else this.queue.push(frame);
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

async function hello(socket, schemaHash) {
  socket.send({ op: 'HELLO', protocol: 1, schema_hash: schemaHash, opts: {} });
  const joinOk = await socket.waitFor((f) => f.op === 'JOIN_OK', 'JOIN_OK');
  await socket.waitFor((f) => f.op === 'FULL_STATE', 'FULL_STATE');
  return { joinOk };
}

/** Minimal raw WebSocket client: lets us set request headers like a browser cannot. */
function rawUpgrade(url, headers) {
  return new Promise((resolve, reject) => {
    const target = new URL(url);
    const key = randomBytes(16).toString('base64');
    const request = http.request({
      hostname: target.hostname,
      port: target.port,
      path: `${target.pathname}${target.search}`,
      headers: {
        Connection: 'Upgrade',
        Upgrade: 'websocket',
        'Sec-WebSocket-Version': '13',
        'Sec-WebSocket-Key': key,
        ...headers,
      },
    });
    request.on('upgrade', (response, socket, pending) =>
      resolve({ status: response.statusCode, socket, pending }),
    );
    request.on('response', (response) => resolve({ status: response.statusCode }));
    request.on('error', reject);
    request.end();
  });
}

function decodeFrame(buffer) {
  if (buffer.length < 2) return null;
  const opcode = buffer[0] & 0x0f;
  const masked = (buffer[1] & 0x80) !== 0;
  let length = buffer[1] & 0x7f;
  let offset = 2;
  if (length === 126) {
    if (buffer.length < 4) return null;
    length = buffer.readUInt16BE(2);
    offset = 4;
  } else if (length === 127) {
    if (buffer.length < 10) return null;
    length = Number(buffer.readBigUInt64BE(2));
    offset = 10;
  }
  if (masked) offset += 4;
  if (buffer.length < offset + length) return null;
  const payload = buffer.subarray(offset, offset + length);
  return {
    opcode,
    text: opcode === 1 ? payload.toString('utf8') : null,
    payload,
    consumed: offset + length,
  };
}

function encodeClientText(text) {
  const payload = Buffer.from(text, 'utf8');
  if (payload.length >= 65536) throw new Error('raw client frame too large');
  const mask = randomBytes(4);
  const header = [];
  header.push(0x81);
  if (payload.length < 126) {
    header.push(0x80 | payload.length);
  } else {
    header.push(0x80 | 126, payload.length >> 8, payload.length & 0xff);
  }
  const masked = Buffer.alloc(payload.length);
  for (let index = 0; index < payload.length; index += 1) {
    masked[index] = payload[index] ^ mask[index % 4];
  }
  return Buffer.concat([Buffer.from([...header, ...mask]), masked]);
}

class RawSocket {
  constructor(socket, pending) {
    this.socket = socket;
    this.buffer = pending ?? Buffer.alloc(0);
    this.frames = [];
    this.closeCode = null;
    socket.on('data', (chunk) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this.#drain();
    });
    this.#drain();
  }

  #drain() {
    for (;;) {
      const frame = decodeFrame(this.buffer);
      if (!frame) return;
      this.buffer = this.buffer.subarray(frame.consumed);
      if (frame.opcode === 8) {
        this.closeCode = frame.payload.length >= 2 ? frame.payload.readUInt16BE(0) : 1005;
        this.frames.push({ opcode: 8, closeCode: this.closeCode });
        continue;
      }
      this.frames.push(frame);
    }
  }

  /** The next parsed JSON text frame. */
  async nextText(timeoutMs = TIMEOUT_MS) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const frame = this.frames.find((candidate) => candidate.opcode === 1);
      if (frame) {
        this.frames.splice(this.frames.indexOf(frame), 1);
        return JSON.parse(frame.text);
      }
      if (Date.now() > deadline) throw new Error('timed out waiting for raw frame');
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
  }

  /** The close code, waiting for the close frame if necessary. */
  async waitForClose(timeoutMs = TIMEOUT_MS) {
    const deadline = Date.now() + timeoutMs;
    while (this.closeCode === null) {
      if (Date.now() > deadline) throw new Error('timed out waiting for the socket to close');
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    return this.closeCode;
  }

  sendText(text) {
    this.socket.write(encodeClientText(text));
  }

  close() {
    this.socket.destroy();
  }
}

let intentSeq = 0;
function intentFrame(intent) {
  intentSeq += 1;
  return {
    op: 'INTENT',
    intents: [{ seq: intentSeq, intent }],
  };
}

function intent(cell) {
  return intentFrame({ Place: { cell } });
}

/** An authenticated admin API request. */
function adminRequest(path, { method = 'GET' } = {}) {
  return request(path, { method, token: ADMIN_TOKEN });
}

/** The close code the server sent to a JSON socket, waiting for the close. */
async function waitForJsonClose(socket, timeoutMs = TIMEOUT_MS) {
  const deadline = Date.now() + timeoutMs;
  while (socket.closed === null) {
    if (Date.now() > deadline) throw new Error('timed out waiting for the socket to close');
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  return socket.closed.code;
}

async function login(deviceId) {
  const response = await request('/auth/anonymous', {
    method: 'POST',
    body: { device_id: deviceId },
  });
  check(response.status === 200, `anonymous login returns 200 (device ${deviceId})`);
  return response.json;
}

// Rate limiting (v0.3b). Runs only under E2E_RATELIMIT=1, because it needs the
// limits from `wrangler.e2e-ratelimit.toml`: a player limit of 5 per minute and
// an IP limit of 1000. The IP limit is deliberately high so the player checks
// finish first no matter how much traffic the server already saw; the phase then
// bursts up to it, which proves the IP limit without depending on exact counters.
async function ratelimitPhase() {
  const { player_id: player_id1, token } = await login(`device-rl-${randomBytes(8).toString('hex')}`);
  check(typeof player_id1 === 'string', 'the rate-limit phase can log in');

  // 5 requests per player and minute: the first five pass.
  for (let index = 1; index <= 5; index++) {
    const response = await request('/me', { token });
    check(response.status === 200, `player request ${index} of 5 is allowed`);
  }

  // The sixth is rejected by the player limit, not the IP limit.
  const limitedPlayer = await request('/me', { token });
  check(limitedPlayer.status === 429, 'the sixth player request is 429');
  check(limitedPlayer.json?.scope === 'player', 'the 429 names the player scope');
  check(limitedPlayer.headers.get('retry-after') !== null, 'the 429 carries Retry-After');

  // Bust the IP limit. Every request from here is charged to this machine's IP.
  let attempts = 0;
  let limitedIp = null;
  while (attempts < 1100) {
    attempts += 1;
    const response = await request('/schema/tictactoe');
    if (response.status === 429) {
      limitedIp = response;
      break;
    }
    if (response.status !== 200) fail(`request ${attempts} answered ${response.status}`);
  }
  check(limitedIp !== null, `the IP limit is reached after ${attempts} requests`);
  check(limitedIp.json?.scope === 'ip', 'the IP 429 names the ip scope');
  check(limitedIp.headers.get('retry-after') !== null, 'the IP 429 carries Retry-After');

  // A limited request never reached the router: an invalid body still just 429s.
  const limitedAuth = await request('/auth/anonymous', {
    method: 'POST',
    body: { device_id: 'short' },
  });
  check(limitedAuth.status === 429, 'the IP limit covers /auth/* too');

  console.log('e2e ok (rate limits)');
  process.exit(0);
}

// JWT provider (v0.3b). Runs only under E2E_JWT=1, because the deployment needs
// the JWKS fixture configured in .dev.vars:
//
//   HEARTH_JWT_JWKS_URL=http://127.0.0.1:8799/jwks.json
//   HEARTH_JWT_ISS=https://e2e.hearth.test
//   HEARTH_JWT_AUD=hearth-e2e
//   HEARTH_JWT_NAME_CLAIM=name
//
// The phase serves the JWKS itself (so the gateway has something to fetch) and
// signs every token locally.
async function jwtPhase() {
  const keys = ensureKeys();
  const jwks = startJwksServer(keys, { port: JWKS_PORT });
  await new Promise((resolve) => jwks.server.once('listening', resolve));

  const auth = async (jwt) => request('/auth/jwt', { method: 'POST', body: { jwt } });
  // An unknown kid is the cheapest way to make the gateway fetch the JWKS: the
  // first one misses the (empty) cache, the second must hit it.
  const unknownKid = (kid) =>
    auth(mintJwt(keys, { kid, overrides: { sub: `ghost-${kid}` } }));

  check((await unknownKid('ghost-one')).status === 401, 'an unknown kid is rejected');
  const firstFetch = jwks.state.fetches;
  check(firstFetch >= 1, `the gateway fetched the JWKS once (${firstFetch})`);
  check((await unknownKid('ghost-two')).status === 401, 'a second unknown kid is rejected');
  check(
    jwks.state.fetches === firstFetch,
    'a second unknown kid inside 60s does not refetch the JWKS',
  );

  const rs = await auth(mintJwt(keys, { alg: 'RS256', kid: RSA_KID, sub: 'e2e-player' }));
  check(rs.status === 200, 'an RS256 token is accepted');
  check(rs.json?.player_id === 'jwt_e2e-player', 'the JWT player id is namespaced jwt_');
  check(rs.json?.display_name === 'E2E Player', 'the name claim becomes the display name');
  const jwtToken = rs.json?.token;

  const es = await auth(mintJwt(keys, { alg: 'ES256', kid: EC_KID, sub: 'e2e-ec' }));
  check(es.status === 200, 'an ES256 token is accepted');
  check(String(es.json?.player_id).startsWith('jwt_'), 'the ES256 player id is namespaced too');

  // A known kid signed by a key that is not in the JWKS.
  const rogue = generateKeyPairSync('rsa', { modulusLength: 2048 });
  const rogueToken = mintJwt(
    { ...keys, rsaPrivate: rogue.privateKey },
    { alg: 'RS256', kid: RSA_KID, sub: 'rogue' },
  );
  check((await auth(rogueToken)).status === 401, 'a token signed by another key is rejected');

  // Algorithm confusion: HS256 keyed with the RSA public key material.
  const confused = await auth(mintJwt(keys, { alg: 'HS256', kid: RSA_KID, sub: 'confused' }));
  check(confused.status === 401, 'an HS256 token is rejected by a JWKS deployment');
  const none = await auth(mintJwt(keys, { alg: 'none', kid: RSA_KID, sub: 'none' }));
  check(none.status === 401, 'alg none is rejected');

  const now = Math.floor(Date.now() / 1000);
  const rejections = [
    // `undefined` drops the claim entirely, which is how "missing exp" is built.
    ['no exp', { exp: undefined }],
    ['expired beyond leeway', { exp: now - 120 }],
    ['nbf in the future', { nbf: now + 120 }],
    ['wrong issuer', { iss: 'https://elsewhere.test' }],
    ['wrong audience', { aud: 'another-app' }],
  ];
  for (const [label, claims] of rejections) {
    check((await auth(mintJwt(keys, { overrides: claims }))).status === 401, `a JWT with ${label} is rejected`);
  }

  const me = await request('/me', { token: jwtToken });
  check(me.status === 200, 'a JWT-issued token works on the authenticated routes');

  jwks.server.close();
  console.log('e2e ok (jwt)');
  process.exit(0);
}

async function main() {
  if (process.argv.includes('--mint-jwt')) {
    process.stdout.write(`${mintJwt(ensureKeys())}\n`);
    return process.exit(0);
  }
  if (RATELIMIT) return ratelimitPhase();
  if (JWT) return jwtPhase();
  // 1. Two anonymous logins are different players.
  const playerA = await login(`device-a-${randomBytes(8).toString('hex')}`);
  const playerB = await login(`device-b-${randomBytes(8).toString('hex')}`);
  check(
    typeof playerA.player_id === 'string' && playerA.player_id.length > 0,
    'first login returns a player id',
  );
  check(
    playerA.player_id !== playerB.player_id,
    'two anonymous logins give different player ids',
  );

  // 2. Mint a room name.
  const minted = await request('/rooms/tictactoe', { method: 'POST', token: playerA.token });
  check(minted.status === 200 && typeof minted.json.room === 'string', 'room mint returns a name');
  const room = minted.json.room;

  // 3. Schema hash is a 16-char hex string.
  const schema = await request('/schema/tictactoe');
  check(schema.status === 200, 'schema endpoint returns 200');
  check(
    typeof schema.json.schema_hash === 'string' && /^[0-9a-f]{16}$/.test(schema.json.schema_hash),
    'schema hash is a 16-character hex string',
  );
  const schemaHash = schema.json.schema_hash;

  // 4. Both sockets connect and complete the handshake.
  const urlA = `${WS_BASE}/rooms/tictactoe/${encodeURIComponent(room)}?token=${encodeURIComponent(playerA.token)}`;
  const urlB = `${WS_BASE}/rooms/tictactoe/${encodeURIComponent(room)}?token=${encodeURIComponent(playerB.token)}`;
  const socketA = new JsonSocket(await openSocket(urlA));
  const socketB = new JsonSocket(await openSocket(urlB));
  const helloA = await hello(socketA, schemaHash);
  const helloB = await hello(socketB, schemaHash);
  check(helloA.joinOk.player_id === playerA.player_id, 'JOIN_OK carries the verified player id (A)');
  check(helloA.joinOk.seat === 0, 'first player gets seat 0');
  check(helloB.joinOk.seat === 1, 'second player gets seat 1');
  check(socketB.state.status === 'Playing', 'game starts when the second player joins');

  // 5. X wins the top row and both clients see it.
  const moves = [
    [socketA, 0],
    [socketB, 3],
    [socketA, 1],
    [socketB, 4],
    [socketA, 2],
  ];
  for (const [socket, cell] of moves) {
    const frame = intent(cell);
    socket.send(frame);
    const seq = frame.intents[0].seq;
    await socket.waitFor(
      (candidate) => candidate.op === 'ACK' && candidate.intent_seqs.includes(seq),
      `ACK for intent ${seq}`,
    );
  }
  const wonByA = (state) => state.status && state.status.Won === playerA.player_id;
  await socketA.waitForState(wonByA, 'A sees the winning status');
  await socketB.waitForState(wonByA, 'B sees the winning status');
  check(true, 'both clients see the winning status');

  // 6. A burst of intents is answered per intent; no socket is closed by an internal error.
  const perSocket = 50;
  const sockets = [socketA, socketB];
  for (const socket of sockets) {
    for (let index = 0; index < perSocket; index += 1) socket.send(intent(index % 9));
  }
  for (const socket of sockets) {
    let answered = 0;
    while (answered < perSocket) {
      const frame = await socket.next(TIMEOUT_MS);
      if (frame.op === 'ACK') answered += frame.intent_seqs.length;
      else if (frame.op === 'ERROR') answered += 1;
    }
    check(socket.closed === null, 'socket stays open under a burst of intents');
  }
  check(
    socketA.ws.readyState === WebSocket.OPEN && socketB.ws.readyState === WebSocket.OPEN,
    'both sockets are still open after the burst',
  );

  // 7. An invalid token is rejected with HTTP 401.
  const badToken = await rawUpgrade(urlA.replace(playerA.token, 'not-a-token'), {});
  check(badToken.status === 401, 'a bad token is rejected with HTTP 401');

  // 8. A forged player header is overwritten by the gateway. Use a second room
  // so the extra socket for player A does not replace the game socket above.
  const forgeRoom = (
    await request('/rooms/tictactoe', { method: 'POST', token: playerA.token })
  ).json.room;
  const forgeUrl = `${WS_BASE}/rooms/tictactoe/${encodeURIComponent(forgeRoom)}?token=${encodeURIComponent(playerA.token)}`;
  const forged = await rawUpgrade(forgeUrl, { 'X-Hearth-Player-Id': playerB.player_id });
  check(forged.status === 101, 'valid token upgrades even with a forged header');
  const raw = new RawSocket(forged.socket, forged.pending);
  raw.sendText(JSON.stringify({ op: 'HELLO', protocol: 1, schema_hash: schemaHash, opts: {} }));
  const rawJoinOk = await raw.nextText();
  check(
    rawJoinOk.op === 'JOIN_OK' && rawJoinOk.player_id === playerA.player_id,
    'gateway overwrites a forged X-Hearth-Player-Id with the token identity',
  );
  raw.close();

  // 9. The keepalive text frame is answered by the DO auto-response.
  socketA.ws.send('hearth:ping');
  const pong = await socketA.waitFor(
    (frame) => frame.op === 'TEXT' && frame.text === 'hearth:pong',
    'hearth:pong keepalive response',
  );
  check(pong.text === 'hearth:pong', 'hearth:ping gets hearth:pong');

  // --- v0.3a: matchmaking, player data and the internal call path ---

  // 10. Two players quick-join the same matchmaker room.
  const playerC = await login(`device-c-${randomBytes(8).toString('hex')}`);
  const playerD = await login(`device-d-${randomBytes(8).toString('hex')}`);
  const playerE = await login(`device-e-${randomBytes(8).toString('hex')}`);
  const matchC = await request('/match/tictactoe', {
    method: 'POST',
    token: playerC.token,
    body: {},
  });
  check(matchC.status === 200, 'quick-join returns 200');
  check(
    /^m_(enam|sam|weur|apac|oc|afr|any)_[0-9a-f]{16}$/.test(matchC.json.room ?? ''),
    'quick-join returns a matchmaker room name',
  );
  check(
    matchC.json.room.split('_')[1] === matchC.json.region,
    'the room name carries the region the match was made in',
  );
  const matchD = await request('/match/tictactoe', {
    method: 'POST',
    token: playerD.token,
    body: { pool: 'default' },
  });
  check(
    matchD.status === 200 && matchD.json.room === matchC.json.room,
    'two quick-joins share one room until it fills',
  );
  const matchRoom = matchC.json.room;

  // 11. Both connect to the matchmade room and play a game.
  const mmUrlC = `${WS_BASE}/rooms/tictactoe/${encodeURIComponent(matchRoom)}?token=${encodeURIComponent(playerC.token)}`;
  const mmUrlD = `${WS_BASE}/rooms/tictactoe/${encodeURIComponent(matchRoom)}?token=${encodeURIComponent(playerD.token)}`;
  const mmC = new JsonSocket(await openSocket(mmUrlC));
  const mmD = new JsonSocket(await openSocket(mmUrlD));
  const mmHelloC = await hello(mmC, schemaHash);
  await hello(mmD, schemaHash);
  check(mmHelloC.joinOk.player_id === playerC.player_id, 'the matchmade room seats the quick-joined player');
  for (const [socket, cell] of [
    [mmC, 0],
    [mmD, 3],
    [mmC, 1],
    [mmD, 4],
    [mmC, 2],
  ]) {
    const frame = intent(cell);
    socket.send(frame);
    await socket.waitFor(
      (candidate) => candidate.op === 'ACK' && candidate.intent_seqs.includes(frame.intents[0].seq),
      `matchmade ACK for intent ${frame.intents[0].seq}`,
    );
  }
  await mmC.waitForState(
    (state) => state.status && state.status.Won === playerC.player_id,
    'the game finishes in the matchmade room',
  );

  // 12. `GET /me` shows the winner's wins and the loser's losses.
  const meC = await request('/me', { token: playerC.token });
  check(meC.status === 200, '/me returns 200');
  check(
    meC.json.wins === 1 && meC.json.losses === 0 && meC.json.draws === 0,
    `the winner's /me shows one win: ${JSON.stringify(meC.json)}`,
  );
  const meD = await request('/me', { token: playerD.token });
  check(
    meD.json.losses === 1 && meD.json.wins === 0 && meD.json.draws === 0,
    `the loser's /me shows one loss: ${JSON.stringify(meD.json)}`,
  );
  const meE = await request('/me', { token: playerE.token });
  check(
    meE.status === 200 && meE.json.wins === 0 && meE.json.losses === 0 && meE.json.draws === 0,
    'a player who never played has an empty /me',
  );
  check(
    JSON.stringify(meC.json) !== JSON.stringify(meD.json),
    "another player's token never returns the caller's stats",
  );
  const meAnonymous = await request('/me');
  check(meAnonymous.status === 401, '/me without a token is 401');

  // 13. A third quick-join lands in a new room, which the lobby lists.
  const matchE = await request('/match/tictactoe', {
    method: 'POST',
    token: playerE.token,
    body: {},
  });
  check(
    matchE.status === 200 && matchE.json.room !== matchRoom,
    'a third quick-join gets a different room',
  );
  const lobby = await request('/lobby/tictactoe?pool=default', { token: playerE.token });
  check(lobby.status === 200 && Array.isArray(lobby.json.rooms), 'the lobby returns a room list');
  const listedE = lobby.json.rooms.find((candidate) => candidate.name === matchE.json.room);
  check(
    listedE !== undefined && listedE.capacity === 2 && (listedE.players === 0 || listedE.players === 1),
    `the lobby lists the new room with capacity 2: ${JSON.stringify(listedE)}`,
  );
  check(
    !lobby.json.rooms.some((candidate) => candidate.name === matchRoom),
    'a full matchmade room is not offered by the lobby',
  );
  const lobbyAnonymous = await request('/lobby/tictactoe');
  check(lobbyAnonymous.status === 401, '/lobby without a token is 401');
  const badMatch = await request('/match/tictactoe', {
    method: 'POST',
    token: playerE.token,
    body: { pool: 'not a valid pool!' },
  });
  check(badMatch.status === 400, 'an invalid pool is rejected with 400');

  // 14. A self-minted m_ name is an ordinary room and never reaches the lobby.
  const forgedName = `m_any_${randomBytes(8).toString('hex')}`;
  const forgedSocket = new JsonSocket(
    await openSocket(
      `${WS_BASE}/rooms/tictactoe/${encodeURIComponent(forgedName)}?token=${encodeURIComponent(playerE.token)}`,
    ),
  );
  const forgedHello = await hello(forgedSocket, schemaHash);
  check(forgedHello.joinOk.seat === 0, 'a self-minted m_ name connects as a normal room');
  const lobbyAfterForged = await request('/lobby/tictactoe?pool=default', { token: playerE.token });
  check(
    !lobbyAfterForged.json.rooms.some((candidate) => candidate.name === forgedName),
    'a self-minted m_ room never appears in the lobby',
  );
  forgedSocket.close();

  // 15. Players cannot reach the internal call path.
  const callPost = await request(
    `/rooms/tictactoe/${encodeURIComponent(room)}/__hearth/call`,
    { method: 'POST', token: playerC.token, body: {} },
  );
  check(
    callPost.status === 404 || callPost.status === 400,
    'POST to a room internal call path is rejected',
  );
  const callGet = await request('/__hearth/call', { token: playerC.token });
  check(
    callGet.status === 404 || callGet.status === 400,
    'GET /__hearth/call is rejected',
  );
  const matchAnonymous = await request('/match/tictactoe', { method: 'POST', body: {} });
  check(matchAnonymous.status === 401, '/match without a token is 401');

  mmC.close();
  mmD.close();
  if (NO_ADMIN) {
    // 10. A deployment without HEARTH_ADMIN_TOKEN has no admin API at all.
    const unauthenticated = await request('/admin/rooms');
    check(unauthenticated.status === 404, 'admin API is a 404 without HEARTH_ADMIN_TOKEN');
    const withToken = await request('/admin/rooms', { token: 'f'.repeat(64) });
    check(withToken.status === 404, 'admin API stays a 404 even with a token when disabled');
  } else {
    if (!ADMIN_TOKEN) {
      fail('set HEARTH_ADMIN_TOKEN to the token in examples/tictactoe/.dev.vars for the admin checks');
    }

    // 10. The rooms list is token-gated and shows the room with both players.
    const unauthenticated = await request('/admin/rooms');
    check(unauthenticated.status === 401, 'admin rooms without a token is 401');
    const listed = await adminRequest('/admin/rooms');
    check(
      listed.status === 200 && Array.isArray(listed.json.rooms),
      'admin rooms returns a room list',
    );
    const listedRoom = listed.json.rooms.find((candidate) => candidate.name === room);
    check(listedRoom?.players === 2, 'the room is listed with 2 players');

    // 11. Inspect shows the full server state and both clients.
    const inspected = await adminRequest(`/admin/rooms/tictactoe/${encodeURIComponent(room)}`);
    check(inspected.status === 200, 'inspect returns 200');
    check(
      JSON.stringify(inspected.json.state?.cells) === JSON.stringify([1, 1, 1, 2, 2, 0, 0, 0, 0]),
      'inspect shows the full board',
    );
    check(
      inspected.json.clients.length === 2 &&
        inspected.json.clients.every((client) => client.connected === true),
      'inspect lists both connected clients',
    );
    check(inspected.json.room.room_type === 'tictactoe', 'inspect names the room type');
    check(inspected.json.seq > 0, 'inspect reports a state seq');

    // 12. A tail socket receives the unfiltered, caused patch for one move.
    const tailUpgrade = await rawUpgrade(
      `${BASE}/admin/rooms/tictactoe/${encodeURIComponent(room)}/tail`,
      { Authorization: `Bearer ${ADMIN_TOKEN}` },
    );
    check(tailUpgrade.status === 101, 'the admin tail upgrades');
    const tail = new RawSocket(tailUpgrade.socket, tailUpgrade.pending);
    const rematch = intentFrame('Rematch');
    socketA.send(rematch);
    await socketA.waitFor(
      (candidate) =>
        candidate.op === 'ACK' && candidate.intent_seqs.includes(rematch.intents[0].seq),
      'ACK for the rematch',
    );
    const tailMsg = await tail.nextText();
    check(tailMsg.cause?.[0]?.cause === 'intent', 'the tail frame is caused by the intent');
    check(tailMsg.cause[0].player === playerA.player_id, 'the tail names the player');
    check(tailMsg.cause[0].intent_seq === rematch.intents[0].seq, 'the tail names the intent seq');
    check(Array.isArray(tailMsg.ops) && tailMsg.ops.length > 0, 'the tail carries the diff');

    // 13. Inspecting a never-used name is a 404 and writes nothing.
    const ghost = `ghost-${randomBytes(4).toString('hex')}`;
    const ghostInspect = await adminRequest(`/admin/rooms/tictactoe/${ghost}`);
    check(ghostInspect.status === 404, 'inspect of a never-used name is 404');
    const listedAgain = await adminRequest('/admin/rooms');
    check(
      !listedAgain.json.rooms.some((candidate) => candidate.name === ghost),
      'a never-used name is not registered',
    );

    // 14. Players cannot reach the room's internal paths.
    const internalDispose = await request(
      `/rooms/tictactoe/${encodeURIComponent(room)}/__hearth/dispose`,
    );
    check(
      internalDispose.status === 404 || internalDispose.status === 400,
      'an internal path is rejected by the gateway',
    );
    const traversal = await request('/rooms/tictactoe/..%2F__hearth%2Finspect');
    check(traversal.status === 404 || traversal.status === 400, 'a traversal room name is rejected');
    const survived = await adminRequest(`/admin/rooms/tictactoe/${encodeURIComponent(room)}`);
    check(survived.status === 200, 'the room survived the internal-path attempts');

    // 15. Force-dispose wipes the room, closes players and tail with 4008, and
    // drops the registry row.
    if (KEEP_ROOM) {
      console.log('keeping the room for manual checks (E2E_KEEP_ROOM=1)');
    } else {
      const disposed = await request(
        `/admin/rooms/tictactoe/${encodeURIComponent(room)}/dispose`,
        { method: 'POST', token: ADMIN_TOKEN },
      );
      check(disposed.status === 204, 'force-dispose returns 204');
      const codeA = await waitForJsonClose(socketA);
      const codeB = await waitForJsonClose(socketB);
      check(codeA === 4008 && codeB === 4008, 'both players are closed with 4008');
      check((await tail.waitForClose()) === 4008, 'the admin tail is closed with 4008');
      const listedAfter = await adminRequest('/admin/rooms');
      check(
        !listedAfter.json.rooms.some((candidate) => candidate.name === room),
        'the disposed room is gone from the list',
      );
      const gone = await adminRequest(`/admin/rooms/tictactoe/${encodeURIComponent(room)}`);
      check(gone.status === 404, 'inspect after dispose is 404');
    }
  }

  // 16. A wrong schema hash is a hard reject with close code 4001.
  const rejected = new JsonSocket(
    await openSocket(`${urlA}&throwaway=${randomBytes(4).toString('hex')}`),
  );
  rejected.send({ op: 'HELLO', protocol: 1, schema_hash: '0000000000000000', opts: {} });
  await rejected.waitFor((frame) => frame.op === 'ERROR', 'SCHEMA_MISMATCH ERROR frame');
  const closed = await rejected.waitFor((frame) => frame.op === 'SOCKET_CLOSE', 'close frame');
  check(closed.code === 4001, 'wrong schema hash closes with 4001');

  socketA.close();
  socketB.close();
  console.log('e2e ok');
  process.exit(0);
}

main().catch((error) => fail(error?.stack ?? String(error)));
