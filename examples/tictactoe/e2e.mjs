// Hearth v0.1/v0.2c end-to-end test for the tictactoe sample.
//
// Requires `wrangler dev` in examples/tictactoe (default http://127.0.0.1:8787).
// Run: node e2e.mjs
//   HEARTH_ADMIN_TOKEN must match the one in examples/tictactoe/.dev.vars for the
//   admin checks. E2E_NO_ADMIN=1 runs the "admin API disabled" phase against a
//   server started without HEARTH_ADMIN_TOKEN. E2E_KEEP_ROOM=1 skips the
//   force-dispose step (for `hearth inspect` by hand).
// Exits 0 on success; prints the failed check and exits 1 otherwise.
import http from 'node:http';
import { randomBytes } from 'node:crypto';

const BASE = process.env.HEARTH_URL ?? 'http://127.0.0.1:8787';
const WS_BASE = BASE.replace(/^http/, 'ws');
const TIMEOUT_MS = 10_000;
const ADMIN_TOKEN = process.env.HEARTH_ADMIN_TOKEN ?? '';
const NO_ADMIN = process.env.E2E_NO_ADMIN === '1';
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
  return { status: response.status, json, text };
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

async function main() {
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
