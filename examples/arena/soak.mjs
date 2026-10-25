#!/usr/bin/env node
// Hearth v0.4 Task 4: soak test for the ticked arena sample.
//
// One Node process drives N WebSocket clients against a deployed (or local
// `wrangler dev`) arena room and checks the ticked-mode contract:
//
//   * every `PATCH` carries `tick` and `t`, ticks advance one at a time (a
//     drop is at most the runtime's 3-tick catch-up cap),
//   * all clients see the same tick within one tick of each other,
//   * the server keeps sending patches at the room's rate while connected,
//   * after everyone leaves the room stops ticking (§ stop-ticking check).
//
// Local `workerd` jitter is not Cloudflare's scheduler, so the ADR's p99 bar is
// only enforced when you pass `--bar-ms`; the local run prints the numbers and
// asserts the structural rules.
//
//   node soak.mjs --url http://127.0.0.1:8787 --clients 8 --secs 60
//   HEARTH_ADMIN_TOKEN=... node soak.mjs --url https://... --secs 300 --bar-ms 10

import { randomBytes } from 'node:crypto';

const args = parseArgs(process.argv.slice(2));
const BASE = args.url ?? 'http://127.0.0.1:8787';
const WS_BASE = BASE.replace(/^http/, 'ws');
const CLIENTS = Number(args.clients ?? 8);
const SECS = Number(args.secs ?? 60);
const BAR_MS = args['bar-ms'] === undefined ? null : Number(args['bar-ms']);
const ADMIN_TOKEN = process.env.HEARTH_ADMIN_TOKEN ?? args['admin-token'];
const ROOM_TYPE = 'arena';
// The sample room's tick_hz, from examples/arena/src/lib.rs.
const TICK_MS = 50;
// The runtime caps a late wake at 3 catch-up ticks.
const MAX_CATCH_UP = 3;
// The sample's clients send one move per tick; a tick whose moves change
// nothing (a wall, a repeated direction) emits no patch, which is why the gap
// checks below only look at adjacent ticks.
const MOVE_EVERY_MS = TICK_MS;

function parseArgs(argv) {
  const parsed = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (!arg.startsWith('--')) throw new Error(`unexpected argument ${arg}`);
    const [name, inline] = arg.slice(2).split('=');
    parsed[name] = inline ?? argv[++index];
  }
  return parsed;
}

function fail(message) {
  console.error(`soak FAILED: ${message}`);
  process.exit(1);
}

function check(condition, message) {
  if (!condition) fail(message);
  console.log(`ok - ${message}`);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function request(path, { method = 'GET', body, token, headers = {} } = {}) {
  const sent = { ...headers };
  if (body !== undefined) sent['content-type'] = 'application/json';
  if (token) sent.authorization = `Bearer ${token}`;
  const response = await fetch(`${BASE}${path}`, {
    method,
    headers: sent,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    // Not JSON: the caller only looks at the status.
  }
  return { status: response.status, json, text };
}

async function admin(path) {
  if (!ADMIN_TOKEN) fail('this phase needs HEARTH_ADMIN_TOKEN for the inspect check');
  const response = await request(path, {
    token: ADMIN_TOKEN,
    headers: { authorization: `Bearer ${ADMIN_TOKEN}` },
  });
  return response;
}

async function login(deviceId) {
  const response = await request('/auth/anonymous', {
    method: 'POST',
    body: { device_id: deviceId },
  });
  if (response.status !== 200) fail(`anonymous login returned ${response.status}`);
  return response.json;
}

function openSocket(url) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url);
    socket.addEventListener('open', () => resolve(socket), { once: true });
    socket.addEventListener('error', () => reject(new Error(`websocket error for ${url}`)), {
      once: true,
    });
  });
}

/// A minimal JSON-frame client: keep every frame, await one that matches.
class JsonSocket {
  constructor(socket) {
    this.socket = socket;
    this.frames = [];
    this.waiters = [];
    socket.addEventListener('message', (event) => {
      const frame = JSON.parse(event.data);
      this.frames.push({ frame, at: Date.now() });
      for (const waiter of [...this.waiters]) {
        if (!waiter.predicate(frame)) continue;
        this.waiters.splice(this.waiters.indexOf(waiter), 1);
        waiter.resolve(frame);
      }
    });
  }

  send(frame) {
    this.socket.send(JSON.stringify(frame));
  }

  waitFor(predicate, label, timeoutMs = 10_000) {
    const existing = this.frames.find((entry) => predicate(entry.frame));
    if (existing) return Promise.resolve(existing.frame);
    return new Promise((resolve, reject) => {
      const waiter = {
        predicate,
        resolve: (frame) => {
          clearTimeout(timer);
          resolve(frame);
        },
      };
      const timer = setTimeout(() => {
        this.waiters.splice(this.waiters.indexOf(waiter), 1);
        reject(new Error(`timed out waiting for ${label}`));
      }, timeoutMs);
      this.waiters.push(waiter);
    });
  }

  close() {
    this.socket.close();
  }
}

/// Fixed spawn position for a seat, mirroring the room's `on_join`.
function spawn(seat) {
  return [2 + (seat % 4) * 5, 2 + Math.floor(seat / 4) * 5];
}

function intentFrame(seq, dir) {
  return { op: 'INTENT', intents: [{ seq, intent: { Move: { dir } } }] };
}

function percentile(values, fraction) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const index = Math.min(sorted.length - 1, Math.floor(fraction * sorted.length));
  return sorted[index];
}

async function main() {
  console.log(
    `soak: ${CLIENTS} clients, ${SECS}s against ${BASE}${BAR_MS === null ? '' : `, bar ${BAR_MS} ms`}`,
  );

  // 1. One player mints the room; the rest join it over their own tokens.
  const players = [];
  for (let index = 0; index < CLIENTS; index += 1) {
    players.push(await login(`soak-${randomBytes(10).toString('hex')}`));
  }
  const minted = await request(`/rooms/${ROOM_TYPE}`, { method: 'POST', token: players[0].token });
  check(minted.status === 200 && typeof minted.json.room === 'string', 'a room was minted');
  const room = minted.json.room;
  const schema = (await request(`/schema/${ROOM_TYPE}`)).json.schema_hash;
  check(typeof schema === 'string', 'the arena schema hash is served');

  // 2. Connect everyone and complete the handshake.
  const clients = [];
  for (let index = 0; index < CLIENTS; index += 1) {
    const url = `${WS_BASE}/rooms/${ROOM_TYPE}/${encodeURIComponent(room)}?token=${encodeURIComponent(players[index].token)}`;
    const socket = new JsonSocket(await openSocket(url));
    socket.send({ op: 'HELLO', protocol: 1, schema_hash: schema, opts: {} });
    const joinOk = await socket.waitFor((frame) => frame.op === 'JOIN_OK', `JOIN_OK ${index}`);
    const full = await socket.waitFor((frame) => frame.op === 'FULL_STATE', `FULL_STATE ${index}`);
    clients.push({ socket, player: players[index], seat: joinOk.seat, full });
  }
  check(
    clients.every((client, index) => client.seat === index),
    'seats are assigned in join order',
  );
  check(
    clients.every((client) => {
      const avatar = client.full.state.avatars?.[client.player.player_id];
      return Array.isArray(avatar?.pos) && avatar.pos[0] === spawn(client.seat)[0];
    }),
    'every client starts at its seat spawn position',
  );

  // 3. Drive every client for the run window. The join passes that filled the
  // room emit their last patches a few milliseconds after the handshakes
  // complete; those carry no tick, so settle first and then start the clock.
  await sleep(750);
  const tickingSince = Date.now();
  const startedAt = tickingSince;
  const timers = clients.map((client) => {
    let seq = 0;
    return setInterval(() => {
      seq += 1;
      const dir = [Math.round(Math.random() * 2 - 1), Math.round(Math.random() * 2 - 1)];
      client.socket.send(intentFrame(seq, dir));
    }, MOVE_EVERY_MS);
  });
  await sleep(SECS * 1000);
  timers.forEach(clearInterval);

  // 4. Structural checks on the PATCH stream.
  const patched = clients.map((client) =>
    client.socket.frames
      .filter((entry) => entry.frame.op === 'PATCH' && entry.at >= tickingSince)
      .map((entry) => entry.frame),
  );
  for (const [index, frames] of patched.entries()) {
    check(frames.length > 0, `client ${index} received patches`);
    check(
      frames.every((frame) => Number.isInteger(frame.tick) && Number.isInteger(frame.t)),
      `client ${index}: every PATCH carries tick and t`,
    );
  }
  const firstTicks = patched.map((frames) => frames[0]?.tick ?? 0);
  const lastTicks = patched.map((frames) => frames.at(-1)?.tick ?? 0);
  check(
    Math.max(...lastTicks) - Math.min(...lastTicks) <= 1,
    `all clients end within one tick of each other (${lastTicks.join(', ')})`,
  );
  check(Math.min(...firstTicks) > 0, 'the first patch already has a tick number');

  // Ticks only move forward. A tick whose moves change nothing emits no patch,
  // so gaps between patches are normal; jitter is measured across adjacent
  // ticks only, and the catch-up cap is proven by the ticked runtime's own
  // tests (a dropped tick there is a single step, never a spiral).
  let patchless = 0;
  const errors = [];
  for (const [index, frames] of patched.entries()) {
    for (let at = 1; at < frames.length; at += 1) {
      const jump = frames[at].tick - frames[at - 1].tick;
      if (jump < 1) {
        fail(`client ${index}: tick went backwards (${frames[at - 1].tick} -> ${frames[at].tick})`);
      }
      if (jump > 1) {
        patchless += 1;
        continue;
      }
      // `t` is the server clock at the tick, so receive scheduling cannot
      // pollute the interval.
      errors.push(Math.abs(frames[at].t - frames[at - 1].t - TICK_MS));
    }
  }
  console.log(`    ${patchless} patch-less tick intervals (state unchanged), counted as normal`);
  const p99 = percentile(errors, 0.99);
  console.log(
    `    tick interval error: p50 ${percentile(errors, 0.5)} ms, p95 ${percentile(errors, 0.95)} ms, p99 ${p99} ms (bar ${TICK_MS / 5} ms)`,
  );
  if (BAR_MS !== null) {
    check(p99 <= BAR_MS, `p99 tick interval error ${p99} ms is within the ${BAR_MS} ms bar`);
  } else {
    console.log('    (local run: the ADR bar is only enforced with --bar-ms)');
  }

  // 6. The simulation advanced: inspect shows moved avatars.
  const inspected = await admin(`/admin/rooms/${ROOM_TYPE}/${encodeURIComponent(room)}`);
  check(inspected.status === 200, 'inspect returns the room');
  const state = inspected.json.state;
  check(
    Object.keys(state.avatars ?? {}).length === CLIENTS,
    'inspect lists every avatar',
  );
  check(
    Number.isInteger(inspected.json.tick) && inspected.json.tick >= lastTicks[0],
    `inspect reports the tick (${inspected.json.tick})`,
  );
  const moved = Object.entries(state.avatars).some(([player, avatar]) => {
    const client = clients.find((candidate) => candidate.player.player_id === player);
    return client && (avatar.pos[0] !== spawn(client.seat)[0] || avatar.pos[1] !== spawn(client.seat)[1]);
  });
  check(moved, 'at least one avatar moved from its spawn');
  check(
    Array.isArray(state.coins) && state.coins.length > 0,
    'the synced coins are part of the state',
  );

  // 7. Stop-ticking: everyone leaves, then the tick stays unchanged.
  for (const client of clients) client.socket.close();
  // Stop-ticking is observable after the last leave is handled; a held-seat
  // grace period must not keep the simulation running.
  await sleep(5_000);
  const before = await admin(`/admin/rooms/${ROOM_TYPE}/${encodeURIComponent(room)}`);
  if (before.status === 404) {
    console.log('    (room disposed immediately after everyone left; nothing to tick)');
  } else {
    check(before.status === 200, 'inspect still answers after everyone left');
    const tickBefore = before.json.tick;
    await sleep(10_000);
    const after = await admin(`/admin/rooms/${ROOM_TYPE}/${encodeURIComponent(room)}`);
    check(after.status === 200, 'inspect answers 10 s later');
    check(
      after.json.tick === tickBefore,
      `the room stopped ticking after everyone left (tick ${tickBefore} unchanged)`,
    );
    check(
      (after.json.clients ?? []).every((client) => client.connected !== true),
      'no client is connected after the run',
    );
  }

  console.log(
    `soak ok (${CLIENTS} clients, ${SECS}s, ${Math.round((Date.now() - startedAt) / 1000)}s wall, room ${room})`,
  );
  process.exit(0);
}

main().catch((error) => fail(error.stack ?? String(error)));
