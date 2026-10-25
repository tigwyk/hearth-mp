# Hearth limits

**Hearth v0.1 rooms are event-driven. Anything needing more than ~20 Hz
authoritative simulation, UDP, or more than ~100MB of live state per match is
the wrong fit; use a dedicated server (Handoff mode arrives in v0.5).**

Hearth runs one authoritative Durable Object per match and hibernates between
events. That is what makes idle rooms cost about nothing, and it is also the
source of every limit on this page. Event mode is the only mode implemented in
v0.1.

## Cost profile per mode

| Mode | How it runs | Fits | Cost profile | Available |
| --- | --- | --- | --- | --- |
| Event (default) | Wakes per message or alarm, hibernates between | Turn-based, card/board, party games, async ("your move" over days), lobbies, social hubs | Billed per message; idle rooms near free | v0.1 |
| Ticked | Alarm-driven fixed tick, 5–20 Hz, while players are connected | Co-op, top-down action, .io-style, MMO-lite zones with tolerant netcode | Billed wall-clock while ticking; stops when room empties | v0.4 |
| Handoff | Room does matchmaking and session state, then issues a ticket to a dedicated server (Cloudflare Containers or the dev's own fleet) | Shooters, fighting games, physics-heavy action | Hearth only bills for the coordination part | v0.5 |

Incoming messages are what Cloudflare bills; outgoing patches and events are
free. Hearth's client SDKs therefore batch intents (20 messages/s and 32 intents
per message by default), so a 60 Hz input stream costs about two messages per
second per client, not sixty. Each message still wakes the room and burns CPU,
so do not send one per rendered frame.

## Per-room resource limits

- **Memory: 128 MB per Durable Object.** The whole room, persisted state plus
  in-flight work, must fit. The ~100 MB hard line above leaves headroom for the
  runtime, the SQLite page cache and the sockets.
- **CPU: 30 seconds per handler** (Cloudflare's default per request or WebSocket
  message, configurable up to 5 minutes). One wake runs one handler pass: decode
  the intents, run `on_intent`, persist state, flush patches. A handler that
  needs seconds of CPU is doing too much; split the work across messages.
- **Incoming frame: 64 KiB**, checked before any deserialisation. A larger frame
  is not decoded; the socket is closed with code 4006.
- **Players per room: `max_clients`** (1–255), declared on `#[hearth::room]`.
  The platform's 32,768 WebSockets per Durable Object is far above room scale.
- **State tree: JSON data types only**, object keys are always strings. State is
  persisted to SQLite on every handler exit, so steering the whole state on
  every message is also a disk-write cost.

## Abuse limits

Two separate layers stop a client from burning your Worker's CPU and your
Durable Objects' budget: the gateway rate-limits HTTP calls, and each room caps
intents per player. They are independent. After the WebSocket upgrade, `INTENT`
frames never pass through the gateway's HTTP limits; the room's intent caps do
not count HTTP calls.

### Gateway rate limits

Both limits use the Workers Rate Limiting binding. Defaults:

| Limit | Default | Applied to |
| --- | --- | --- |
| Per IP | 300 requests / 60 s | Every gateway route, including `POST /auth/*` and the WebSocket upgrade. Key: `ip:<CF-Connecting-IP>`. |
| Per player | 60 requests / 60 s | Authenticated routes after token verification: `POST /rooms/{type}`, the WebSocket connect, `POST /match`, `GET /lobby` and `GET /me`. Key: `p:<player_id>`. |

The per-IP limit is generous **on purpose**: behind CGNAT, in a school or in a
shared household, many players can share one IP, and a tight IP limit would
lock all of them out. The per-player limit is the tight one.

- The IP check runs first, on every gateway route. Token verification comes
  next, then the player check on authenticated routes. A 429 therefore means
  nothing reached a room or the matchmaker.
- `POST /auth/{provider}` is IP-limited only (there is no token yet), and
  `GET /schema/{type}` is IP-limited only.
- No binding means no limits: the gateway allows every request and logs
  `{"hearth":"ratelimit_disabled","binding":"HEARTH_IP_LIMIT"}` (and the same
  for the player binding) once per binding per isolate. That is the local-dev
  behaviour and a deliberate small-deployment mode.
- If the binding itself errors, the request is **allowed** and the error is
  logged as `{"hearth":"ratelimit_error",...}`. A broken limiter must not lock
  players out, so treat these limits as protective, not as a billing-grade
  quota.
- Without a `CF-Connecting-IP` header (some local setups do not send one) there
  is no IP key, so the per-IP check is skipped.

Copy both bindings into `wrangler.toml` (the sample project has this block):

```toml
# The per-IP limit is generous because many players can share one IP; the
# per-player limit is the tight one.
[[ratelimits]]
name = "HEARTH_IP_LIMIT"
namespace_id = "1001"
simple = { limit = 300, period = 60 }

[[ratelimits]]
name = "HEARTH_PLAYER_LIMIT"
namespace_id = "1002"
simple = { limit = 60, period = 60 }
```

The gateway takes the binding names and their periods from
`GatewayConfig.limits` (`hearth_gateway::ratelimit::Limits`):

```rust
limits: hearth_gateway::ratelimit::Limits::default(), // 60 s for both
```

`Limits` has `ip_binding`, `ip_period_s`, `player_binding` and
`player_period_s`. If you rename a binding or change its `period`, change the
struct too: the gateway only learns success or failure from the limiter and
reports its own configured `*_period_s` in `Retry-After`, so keep the two
numbers in sync by hand.

### The 429 contract

A limited request returns:

```http
HTTP/1.1 429 Too Many Requests
Retry-After: 60
Content-Type: application/json

{"error":"rate_limited","scope":"player"}
```

`scope` is `"ip"` or `"player"`. `Retry-After` is the configured period in whole
seconds (60 by default), not a precise remaining window: wait that long before
retrying.

The SDKs treat a 429 on any Hearth HTTP call as retryable: they wait
`Retry-After` seconds and retry at most **twice**, then surface `RATE_LIMITED`.
That covers login, `create_room`, `quick_join`, `list_rooms` and `get_me`; the
login flow and its error statuses are in [auth.md](auth.md).

### Room intent caps

Each room has a per-player intent token bucket, configured on the room:

| Attribute | Default | Meaning |
| --- | --- | --- |
| `intent_rate` | `60` | Tokens refilled per second, per player. |
| `intent_burst` | `120` | Bucket capacity: how many intents can arrive back-to-back. |

```rust
#[hearth::room(mode = "event", max_clients = 4, binding = "GAME",
               intent_rate = 30, intent_burst = 60)]
impl Room for Game { /* ... */ }
```

Every intent costs one token; the bucket starts full and refills at
`intent_rate` per second up to `intent_burst`. Emptying it is not silent:

- the offending intent is rejected with an `ERROR` frame, code `RATE_LIMITED`
  and `ref_seq` set to that intent's sequence number;
- the rest of that `INTENT` frame is dropped (one SDK batch can carry up to 32
  intents by default);
- the player is kicked: `on_leave` runs with reason `Kicked`, the seat is freed,
  and the socket is closed with code **4005**;
- the client can reconnect, but it should fix its send rate first.

Both values must be at least 1. `intent_rate = 0` or `intent_burst = 0` is a
compile error (`intent_rate must be at least 1`, `intent_burst must be at least
1`). The defaults match the old hard-coded bucket: 60 intents/s sustained and a
120-intent burst, about four full SDK batches.

### Tuning for a real game

- **Chatty HTTP is what hits the player limit.** Polling `/lobby` or `/me`, or
  calling `quick_join` repeatedly, spends the 60/60 s budget; poll less, cache
  the lobby, or raise the `HEARTH_PLAYER_LIMIT` for that game. Every reconnect
  also spends one HTTP request; for a chatty game, prefer several rooms over one
  very busy room.
- **Do not raise the IP limit blindly.** It is shared by everyone behind one
  address. Raising it to fix one household also raises it for an actual abuser,
  and lowering it risks locking out a school or a CGNAT block. Prefer tuning the
  player limit.
- **Intent traffic is capped by the room, not the gateway.** Tune
  `intent_rate`/`intent_burst` to the game's real input rate. The burst counts
  intents, not messages: one batch of 32 intents spends 32 tokens.
- **Several rooms are cheaper than one huge room.** Memory and CPU are per
  Durable Object, and every room has its own intent buckets.

## Numbers in JSON mode

JSON parses every number as a float64 in JavaScript and GDScript, so an `i64`
state value above 2^53 (9,007,199,254,740,992) loses precision on those clients.
Keep large identifiers as strings — `PlayerId` and the schema hash are strings
for exactly this reason — or use a MsgPack client (the native Godot extension,
Rust) which carries `i64` exactly. Do not rely on exact large integers in state
if a JSON or GDScript client can see them.
