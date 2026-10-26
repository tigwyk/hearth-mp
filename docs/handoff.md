# Handoff to a dedicated server

Hearth v0.5 adds **handoff**: a room hands its seated players to a dedicated
server when the game outgrows a Durable Object. Hearth keeps the lobby, the
session state and the results; the match runs on Cloudflare Containers or the
developer's own fleet, and players reach it with a single-use ticket. The room
scores the result through `on_handoff`, so the leaderboard, player stats and
rematch logic stay where they already are.

The Godot SDK ships a client half (signals plus `connect_handoff`) and a
headless-server helper (`HearthServer`). The Rust server SDK is
`hearth-mp-handoff-server`; Rust clients read the reserved events and use their own
transport. The Unity SDK is **not shipped yet** (v0.5 Task 5 is deferred).

## 1. When to use a handoff

The spec's hard line is deliberately blunt: use a dedicated server for anything
needing **UDP**, **more than about 20 Hz authoritative simulation**, or **more
than about 100 MB of live state** per match (see the Target games table in
[the spec](../Hearth%20a%20Rust%20framework%20for%20Durable%20Object%20game%20servers.md#target-games)).
Ticked rooms cover the rest up to 20 Hz ([ticked.md](ticked.md)); a handoff is
the escape hatch after that.

**A handoff is a room capability, not a room mode.** `mode = "handoff"` is a
compile error:

```text
handoff is not a room mode; call ctx.handoff(...) from an event or ticked room (see docs/handoff.md)
```

Any event or ticked room can call `ctx.handoff`. The room half of a handoff is
ordinary coordination — a lobby, ready-up, session state, results — which is
exactly what an event-mode room already does. The dedicated server owns the
simulation; Hearth owns the session.

Pick the allocator once:

| | Cloudflare Containers | Webhook fleet |
| --- | --- | --- |
| Where the server runs | Cloudflare, one container per session | wherever your allocator starts it |
| How Hearth reaches it | the session's Durable Object starts the container | signed webhook to your allocator returns an endpoint |
| What players get | `<HEARTH_PUBLIC_URL>/handoff/{sid}/connect` (WebSocket) | whatever the allocator returns (`udp`, `wss`, `https`, `ws` in dev) |
| Transport through Hearth | WebSocket proxied by the gateway | none: Hearth only hands out the address |
| Billing | container time on your Cloudflare account | your fleet |
| Needs | a `[[containers]]` entry; Docker locally (not Windows `wrangler dev`) | a small HTTP service that implements the webhook |

UDP and WebRTC never flow through Hearth (a v1 non-goal). Only the fleet path
can return a `udp://` endpoint, because then the client connects directly and
Hearth is not in the data path.

## 2. The flow

```text
handler pass                  room SQLite                adapter (outside the runtime lock)
------------                  -----------                ---------------------------------
ctx.handoff(request) ─commit─▶ allocating session
                                                           take_jobs()
                                                             │ webhook POST, or start container
                                                             ▼
                              ready + endpoint  ◀──job_done──┘
                              one ticket per player
players ◀── EVENT hearth.handoff {sid, endpoint, ticket, expires_ms}
dedicated server ── POST /handoff/{sid}/redeem  (secret + ticket) ──▶ player identity
                 ── POST /handoff/{sid}/report  (started, heartbeat, players, result, ended)
room ── on_handoff(Ready / Started / PlayerJoined / PlayerLeft / Result / Ended / Failed)
players ◀── EVENT hearth.handoff_ended {sid, reason}   (when the session ends or fails)
```

- **`ctx.handoff` is buffered like every other effect.** The session row is
  written in the handler's commit, so a handler that returns `Err` creates
  nothing; the adapter later derives the allocation job from the `allocating`
  session. The method returns a [`SessionId`](#session-ids) immediately — the
  session itself exists only after the commit.
- **Allocation runs outside the runtime lock.** The adapter takes the jobs, runs
  the allocation subrequest while the Durable Object stays alive (through
  `State::wait_until`), and only takes the lock again to deliver the result. A
  slow allocator therefore never blocks a player's intent; the fleet e2e checks
  an intent is answered while a 10-second allocation is in flight.
- **Every player in the request must be seated.** "Seated" means connected or in
  the room's reconnect grace (`held`). Otherwise the request is dropped, the log
  gets `handoff_unknown_player`, and the returned `SessionId` never exists.
- **A live session keeps the room alive.** While a session is `allocating`,
  `ready` or `started`, the empty-room dispose timer is postponed even when every
  lobby socket has dropped. When the session ends, normal empty-room rules
  resume.
- **Tickets are private.** Each player in the request gets their own
  `hearth.handoff` event on their own socket; there is no broadcast. A player who
  reconnects to the room while the session is `ready` or `started` gets a fresh
  ticket event, so a dropped lobby socket does not strand them.
- **`on_handoff` is a normal handler pass.** State changes are diffed, patches
  and events flush, and a returned error is logged like any other handler error.

## 3. The room API

```rust
use std::time::Duration;

use hearth_mp::server::{HandoffEvent, HandoffRequest, SessionId};
use hearth_mp::{Client, Ctx, Result, Room};

#[hearth_mp::room(
    mode = "event",
    max_clients = 8,
    binding = "ARENA_LOBBY",
    allocator = "container",
    container = "ARENA_SERVER"
)]
impl Room for ArenaLobby {
    type State = LobbyState;   // sync fields as in any event room
    type Intent = LobbyIntent; // Ready / Unready

    // ... on_intent: when everyone is ready and at least two are seated:
    // let sid = ctx.handoff(HandoffRequest::new(players, serde_json::json!({"map": "arena"})));

    async fn on_handoff(
        &mut self,
        ctx: &mut Ctx<Self>,
        _sid: &SessionId,
        event: HandoffEvent,
    ) -> Result<()> {
        match event {
            HandoffEvent::Ready { endpoint } => { /* server is up, tickets are out */ }
            HandoffEvent::Started => { /* the dedicated server reported started */ }
            HandoffEvent::Result(data) => { /* data is whatever the server reported */ }
            HandoffEvent::Ended { reason } | HandoffEvent::Failed { reason } => {
                // score, forward to a stats service, allow a rematch
            }
            _ => {}
        }
        Ok(())
    }
}
```

### The request

```rust
use std::time::Duration;

use hearth_mp::server::HandoffRequest;

let request = HandoffRequest::new(players, claims)   // Vec<PlayerId>, serde_json::Value
    .region("eu")
    .start_timeout(Duration::from_secs(60))
    .heartbeat_timeout(Duration::from_secs(60))
    .max_duration(Duration::from_secs(2 * 60 * 60));
let sid = ctx.handoff(request);
```

| Field | Default | Meaning |
| --- | --- | --- |
| `players` | required | the players to hand over; every one must be seated |
| `claims` | required | JSON handed to the dedicated server with every redeem (mode, map, rules, …) |
| `region` | `None` | a placement hint; passed to the allocator verbatim |
| `start_timeout` | 60 s | time allowed between `ready` and the server's `started` report |
| `heartbeat_timeout` | 60 s | longest silence between reports once `started` |
| `max_duration` | 2 h | hard cap on the session, measured from creation |

`claims` is duplicated into every `Redeemed` answer, so it must be acceptable to
expose to the players in the session — do not put secrets there.

### Session ids

`SessionId` is `hs_<base64url(room_type/name)>_<16 hex>`, with no base64 padding.
The gateway decodes the room from the id alone, so neither a redeem nor a report
takes a room name. The 16 hex characters of randomness make an id unguessable;
the room keeps the session rows.

### Attributes and the runtime override

`allocator` enables `ctx.handoff` for the room type: `"webhook"` or `"container"`.
`container` names the Durable Object binding of the session's container class and
is required with `allocator = "container"`. Compile errors are explicit:

| Mistake | Message |
| --- | --- |
| `mode = "handoff"` | `handoff is not a room mode; call ctx.handoff(...) from an event or ticked room (see docs/handoff.md)` |
| `allocator = "kubernetes"` | `allocator must be "webhook" or "container"` |
| `container = ""` | `container must name the DO binding, e.g. container = "ARENA_SERVER"` |
| no `allocator` on the room and no env override | jobs fail with `allocator not configured` (logged), retried, then the session fails |

The attribute is the compile-time default; the deployment can override it with
`HEARTH_ALLOCATOR_MODE=webhook|container`. That is how
[`examples/arena-dedicated`](../examples/arena-dedicated/README.md) serves both
e2e suites from one room. An invalid value fails every job with a message naming
the variable.

### `on_handoff` events

| Event | When | Data |
| --- | --- | --- |
| `Ready { endpoint }` | allocation succeeded; tickets were minted and sent | the endpoint players received |
| `Started` | the server reported `started` | — |
| `PlayerJoined(PlayerId)` | the server reported `player_joined` | the player |
| `PlayerLeft(PlayerId)` | the server reported `player_left` | the player |
| `Result(Value)` | the server reported `result` | the server's JSON |
| `Ended { reason }` | the server reported `ended` | the server's reason |
| `Failed { reason }` | allocation failed for good, or a timeout | error text, `start timeout`, `lost` or `max duration` |

`Ready` fires after the tickets are minted and sent. The room sees the server's
reports in the order the dedicated server sent them.

`Started` can arrive **before** `Ready`: a container reports `started` as soon as
its process boots, and that report can beat the allocator's RPC answer. The room
then keeps the `started` status, records the endpoint when the answer lands, and
sends the tickets at that point. Treat both events as readiness signals and do
not depend on their relative order.

## 4. Timeouts, retries and retention

| Thing | Value | Behaviour |
| --- | --- | --- |
| Ticket TTL | 60 s | single use, bound to `(session, player)`; expired, used or foreign tickets answer 403 |
| Start timeout | 60 s | `ready` without `started` → session `failed` with `start timeout` |
| Heartbeat timeout | 60 s | `started` with no report for 60 s → session `failed` with `lost` |
| Max duration | 2 h (default) | hard cap from session creation; expiry → `failed` with `max duration` |
| Allocation attempts | 3 | a failed attempt is retried after `2^attempt` seconds (2 s, then 4 s); the third failure fails the session with the allocator's error text |
| In-flight window | 30 s | an allocation that has not answered after 30 s is re-issued as a new attempt |
| Retention | 24 h | ended and failed sessions (and their tickets) are pruned after a day |

Notes:

- The three timeout deadlines are all capped by the session's hard maximum, so
  `start_timeout` and `heartbeat_timeout` can never extend a session past
  `max_duration`.
- Once the session is `started`, **any** report refreshes the heartbeat deadline
  (`heartbeat`, `player_joined`, `player_left`, `result`), not only the explicit
  heartbeat.
- A report for a finished session is accepted and ignored (204), but only with
  the right session secret. A redeem for a finished session answers 410.
- When a session ends or times out and an endpoint is known, a **stop job** is
  queued. The adapter runs it outside the runtime lock; a failed stop is logged
  and not retried. Players in the session receive `hearth.handoff_ended` with the
  reason. Allocation failures that end the session also notify the players.
- Once the session ends, the room's normal empty-room timer resumes: with no
  live clients it disposes after the usual empty grace.

## 5. The webhook allocator

A webhook allocator is a small HTTP service the room's adapter calls. Hearth does
not care how servers are started (Kubernetes, Agones, Edgegap, Hathora, a laptop
script); it only requires the signed contract below.

### Allocate

`POST <HEARTH_ALLOCATOR_URL>` with:

| Header | Value |
| --- | --- |
| `Content-Type` | `application/json` |
| `X-Hearth-Timestamp` | unix seconds |
| `X-Hearth-Signature` | `hex(HMAC-SHA256(HEARTH_ALLOCATOR_SECRET, "<timestamp>.<body>"))` |

```json
{
  "sid": "hs_YXJlbmEvbG9iYnktMQ_0123456789abcdef",
  "session_secret": "<64 hex characters>",
  "room": { "room_type": "arena", "name": "lobby-1" },
  "players": ["anon_a1b2", "anon_c3d4"],
  "claims": { "map": "arena", "mode": "ffa" },
  "region": "eu",
  "report_url": "https://example.workers.dev/handoff/hs_.../0123456789abcdef"
}
```

`session_secret` is the only secret the dedicated server will have; `report_url`
is where the server posts reports (`/report` is appended by the SDK).
`region` is `null` when the room did not ask for one.

Answer **`200`** for a fresh allocation, or **`409`** when this `sid` was already
allocated (the idempotent retry after an in-flight re-issue). Both must answer:

```json
{ "endpoint": "udp://1.2.3.4:7777" }
```

Any other status, a transport error or a body without a usable `endpoint` is a
failed attempt (and is retried). The endpoint is validated before a client ever
sees it:

| Scheme | Accepted |
| --- | --- |
| `udp://`, `wss://`, `https://` | always |
| `ws://` | only when insecure endpoints are allowed (`wrangler dev` debug builds, or `HEARTH_ALLOW_INSECURE_ENDPOINTS`) |

The tail after the scheme must be 1–250 ASCII characters with no whitespace and
no control characters, so an allocator cannot inject a `javascript:` URL, a
header break or an absurd string.

### Stop

`POST` the same URL, signed the same way:

```json
{ "sid": "hs_..._0123456789abcdef", "action": "stop" }
```

Any 2xx is success. A stop is best effort: the room has already ended the
session, and a failed stop is only logged (the container backstop and the
allocator's own timeouts are the safety nets).

### Environment

| Variable | Required | Meaning |
| --- | --- | --- |
| `HEARTH_ALLOCATOR_URL` | webhook | where allocate and stop are POSTed |
| `HEARTH_ALLOCATOR_SECRET` | webhook | HMAC key, at least 32 bytes |
| `HEARTH_PUBLIC_URL` | both | gateway public base; `report_url` becomes `<it>/handoff/<sid>` |
| `HEARTH_ALLOW_INSECURE_ENDPOINTS` | webhook, optional | `1`/`true`/`yes` allows `ws://` endpoints; debug builds allow them anyway |
| `HEARTH_CONTAINER_PORT` | containers, optional | the port inside the container (default 8080) |
| `HEARTH_ALLOCATOR_MODE` | both, optional | overrides the room's `allocator` attribute |

Missing configuration fails jobs with a specific message (`HEARTH_ALLOCATOR_URL
is not set`, `HEARTH_ALLOCATOR_SECRET must be at least 32 bytes`, or `allocator
not configured` when no allocator was selected at all). The jobs go through the
normal retry budget, the failure is logged, and stop jobs are skipped with a log.

Keep `HEARTH_ALLOCATOR_SECRET` out of `wrangler.toml`: put it in `.dev.vars` for
local runs and set it with `wrangler secret put` for a deployment (the sample's
`.dev.vars.example` shows it next to the gateway's `HEARTH_SECRET`).

### Verifying the signature

The allocator must verify the signature **before** parsing the body (Mac and
Linux example; this is the check `fleet/allocator.mjs` uses):

```js
import { createHmac, timingSafeEqual } from 'node:crypto';

const MAX_SKEW_S = 300;

function verify(headers, rawBody, secret) {
  const timestamp = headers['x-hearth-timestamp'];
  const signature = headers['x-hearth-signature'];
  if (!timestamp || !signature) return false;
  const now = Math.floor(Date.now() / 1000);
  if (Math.abs(now - Number(timestamp)) > MAX_SKEW_S) return false;
  const expected = Buffer.from(
    createHmac('sha256', secret).update(`${timestamp}.${rawBody}`).digest('hex'),
    'hex',
  );
  const given = Buffer.from(String(signature), 'hex');
  return expected.length === given.length && timingSafeEqual(expected, given);
}
```

Rust allocators can call the same check instead of reimplementing it:

```rust
use hearth_mp::server::verify_webhook;

let ok = verify_webhook(secret, timestamp, raw_body, signature_hex, now_s);
```

Rules the check enforces: the timestamp is unix **seconds** and may be at most
300 s away from the receiver's clock; hex is case-insensitive; the comparison is
constant time; the signed bytes are exactly `<timestamp>.<raw body>` — do not
re-serialise the JSON before verifying.

### Over your fleet

The allocator is the only glue between Hearth and your infrastructure, and you
write it. Typically:

- **Agones** — allocate from a `Fleet` (`GameServerAllocation`) or read the
  `GameServer`'s address/port; stop = release the allocation or scale down.
  Return the game port as `udp://…` or `wss://…`.
- **Edgegap** — create a deployment over their API, return the deployment's host
  and port, stop = delete the deployment.
- **Hathora** — create/allocate a process, return its host and port, stop = stop
  the process.

In every case: key idempotency by `sid`, answer 409 with the same endpoint on a
repeated allocate, and do the work outside the response path if it takes longer
than the room's 30-second in-flight window (otherwise the attempt is re-issued).

## 6. Cloudflare Containers

The container allocator starts one container per session. A session's
`ContainerObject` (generated by `container_object!`) owns that container, so the
room's adapter never touches the container runtime directly.

### Configuration

`wrangler.toml`:

```toml
# Add this after the room's existing bindings and migrations.
[[durable_objects.bindings]]
name = "ARENA_SERVER"
class_name = "ArenaServer"

[[migrations]]
tag = "v4"
new_sqlite_classes = ["ArenaServer"]

[[containers]]
class_name = "ArenaServer"
image = "./server/Dockerfile"
image_build_context = "../.."
max_instances = 10

[vars]
HEARTH_PUBLIC_URL = "https://example.workers.dev"
# HEARTH_CONTAINER_PORT = "8080"   # optional, default 8080
```

The Rust side defines the class once at the Worker crate root:

```rust
hearth_mp::container_object!(ArenaServer);
```

and the gateway route for the room type names the same binding:

```rust
hearth_mp_gateway::RoomRoute {
    room_type: "arena",
    binding: "ARENA_LOBBY",
    container: Some("ARENA_SERVER"),
    // schema_hash, max_clients ...
}
```

`image_build_context = "../.."` builds from the repository root, because the
sample server path-depends on the `hearth-*` crates. A class that is missing from
`[[containers]]` starts nothing: the container object answers 501 and the
allocation fails.

### How a session starts

The adapter sends `ContainerRpc::Start` to the container object named after the
session id, with the sid, the plaintext session secret, the `report_url`, the
configured port, the room's extra `env` and the session's `max_duration` (the
room passes it per session; the 2 h request default is only a fallback). The
object then:

1. Refuses with 501 when the class has no `[[containers]]` entry.
2. Starts the container with `HEARTH_SID`, `HEARTH_SESSION_SECRET`,
   `HEARTH_REPORT_URL` and `HEARTH_PORT` plus the developer's env. Any
   developer-supplied `HEARTH_*` name is ignored, so a session's secret and report
   URL cannot be overridden.
3. Polls `GET /healthz` on the container's port every 500 ms for up to 60 s.
   A container that is not healthy in time is destroyed and the start fails.
4. Sets a backstop alarm 5 min after the session's `max_duration`. The room's own
   deadline normally fires first and queues a stop; the backstop only covers a
   room that never sends one (a hard crash before the room records the end).

A start for the session that is already running is idempotent (that is the
in-flight re-issue path); a running container for another session is replaced.
`Stop` destroys the container, clears the session record and the alarm.

The container should answer `GET /healthz` with 200 as soon as it can serve, and
read the session from `HEARTH_SID`, `HEARTH_SESSION_SECRET` and
`HEARTH_REPORT_URL` (see [`examples/arena-dedicated/server`](../examples/arena-dedicated/server/README.md)).

### The player connect proxy

`GET <HEARTH_PUBLIC_URL>/handoff/{sid}/connect?ticket=<ticket>` (WebSocket) is
enabled by `container: Some(...)` on the room's route. Before the upgrade is
forwarded, the gateway redeems the ticket with the room; only then does it proxy
to the session's container object, which forwards to the container's TCP port.
The container therefore only ever sees connections that have already redeemed.

| Status | Meaning |
| --- | --- |
| 400 | `ticket` is missing |
| 403 | the ticket is used, expired, or minted for another session |
| 404 | unknown session id, unknown room type, or the room type has no container binding |
| 404 / 410 | the container object has no session record / the container is not running |
| 410 | the session already ended or failed |
| 426 | the request is not a WebSocket upgrade |
| 501 | the container binding has no `[[containers]]` entry |
| 502 | the room returned a malformed redeem |

The proxy builds its own request; client-supplied `X-Hearth-*` headers (and
`Host`/`Authorization`) are dropped. The container receives the identity the
gateway redeemed, never anything the client sent:

| Header | Value |
| --- | --- |
| `X-Hearth-Player-Id` | the redeemed player id |
| `X-Hearth-Provider` | the auth provider that minted the identity |
| `X-Hearth-Display-Name` | percent-encoded display name (only when non-empty) |
| `X-Hearth-Claims` | base64url JSON of the request's `claims` |

### Local development

Cloudflare Containers cannot run under `wrangler dev` on Windows ("Local
development with containers is currently not supported on Windows"). The
containers e2e (`e2e-container.mjs`) detects that and exits 2 with the message.
Use Linux, WSL or macOS locally, or deploy a preview (billed — ask first).

For the fleet path no container is needed; pass `--enable-containers=false` to
`wrangler dev` and set `HEARTH_ALLOCATOR_MODE=webhook`.

## 7. The HTTP surface

The dedicated server talks to the gateway. These routes take **no player token**:
the session secret is the credential, and the gateway's per-IP rate limit applies.

| Route | Credential | Body | Success |
| --- | --- | --- | --- |
| `POST /handoff/{sid}/redeem` | `X-Hearth-Session-Secret` | `{"ticket": "..."}` | `200` + `Redeemed` JSON |
| `POST /handoff/{sid}/report` | `X-Hearth-Session-Secret` | `ServerReport` JSON | `204` |
| `GET /handoff/{sid}/connect?ticket=...` | the ticket | — | `101` upgrade |

Failure statuses are `400` (malformed JSON or missing ticket), `403` (missing or
wrong header, or an unusable ticket), `404` (unknown sid or room type) and `410`
(session ended or failed). Error bodies are short text; do not parse them.

A successful redeem answers the identity the server trusts for that connection:

```json
{
  "player_id": "anon_a1b2",
  "provider": "anon",
  "display_name": null,
  "claims": { "map": "arena", "mode": "ffa" },
  "sid": "hs_..._0123456789abcdef"
}
```

Reports use `type` as the tag:

| Report | Body |
| --- | --- |
| started | `{"type": "started"}` |
| heartbeat | `{"type": "heartbeat"}` |
| player joined | `{"type": "player_joined", "player": "<player_id>"}` |
| player left | `{"type": "player_left", "player": "<player_id>"}` |
| result | `{"type": "result", "data": { ... }}` |
| ended | `{"type": "ended", "reason": "match over"}` |

The gateway forwards to the room's internal paths (`/__hearth/redeem`,
`/__hearth/report`, `/__hearth/redeem_trusted` for the container proxy). Those
paths, and the container object's internal `/__hearth/call` and
`/__hearth/proxy/connect`, are **not public**: the gateway builds them itself and
a player request never reaches them.

## 8. Server SDKs

### Rust — `hearth-mp-handoff-server`

The SDK is blocking (`ureq`), because a dedicated server is a normal process, not
a Worker. Run it from a thread or a blocking task if your server has an async
runtime.

```rust
use std::time::Duration;

use hearth_mp_handoff_server::{ServerReport, Session};

let session = Session::from_env()?;          // HEARTH_SID, HEARTH_SESSION_SECRET, HEARTH_REPORT_URL
session.report(&ServerReport::Started)?;

// Reports `heartbeat` every 20 s; stops on an `Ended` report or a `Gone` answer.
let _heartbeat = session.spawn_heartbeat(Duration::from_secs(20));

let identity = session.redeem(ticket)?;      // single-use; answers the player identity
// ... run the match ...

session.report(&ServerReport::Result {
    data: serde_json::json!({ "winner": identity.player_id }),
})?;
session.report(&ServerReport::Ended { reason: "match over".to_owned() })?;
```

- `Session::from_env()` reads `HEARTH_SID`, `HEARTH_SESSION_SECRET` and
  `HEARTH_REPORT_URL`; `Session::new(sid, secret, report_url)` builds one in
  tests. The SDK appends `/redeem` and `/report` to `report_url`.
- `redeem` and `report` send the `X-Hearth-Session-Secret` header and map status
  codes to `ServerError`: 403 → `Forbidden`, 404 → `NotFound`, 410 → `Gone`,
  anything else or a transport error → `Http`.
- `spawn_heartbeat(every)` reports on its own thread and stops after an `Ended`
  report or a `Gone` answer. The room's default heartbeat timeout is 60 s, so the
  default 20 s cadence leaves two lost beats of slack.
- Outbound HTTP calls time out after 10 s.

### Godot — `HearthServer`

```gdscript
var server := HearthServer.from_env()      # null when the HEARTH_* vars are missing
if server == null:
    push_error("not started by a Hearth allocator")
    return
add_child(server)                          # HTTPRequest needs the scene tree
var identity: Dictionary = await server.redeem(ticket)
await server.report("started")
server.start_heartbeat()                   # 20 s default
# ...
await server.report("result", {"winner": identity["player_id"]})
await server.report("ended", {"reason": "match over"})
```

`redeem` returns the `Redeemed` dictionary (`player_id`, `provider`,
`display_name`, `claims`, `sid`), or `{}` with a pushed error on any failure.
`report(type, data := {})` merges `data` into the JSON body and returns whether
the room accepted it; an `ended` report or a 410 stops the heartbeat.

### Unity

Not shipped in v0.5. The Unity SDK has no handoff client or server helper yet, so
Unity projects must use the HTTP contract in section 7 directly if they need one.

## 9. Client side

**Godot.** The client raises two dedicated signals; the generic `event` signal
never fires for `hearth.*` names:

```gdscript
client.handoff.connect(_on_handoff)             # (sid: String, endpoint: String, ticket: String)
client.handoff_ended.connect(_on_handoff_ended) # (sid: String, reason: String)

func _on_handoff(sid: String, endpoint: String, ticket: String) -> void:
    if endpoint.begins_with("udp://"):
        # connect with your own netcode and send the ticket there
        return
    var peer: WebSocketPeer = client.connect_handoff(endpoint, ticket)
    if peer == null:
        return   # connect_handoff already emitted error("HANDOFF_ENDPOINT", ...)
```

`connect_handoff(endpoint, ticket)` opens `WebSocketPeer` to
`endpoint + "?ticket=" + ticket` for `ws://`/`wss://` endpoints. For `udp://` it
returns `null` and emits `error("HANDOFF_ENDPOINT", ...)`; the client uses its own
transport and sends the ticket there.

**Rust clients.** The reserved events arrive as ordinary `ClientEvent::Event`
values; read `name` and `data` and use your own transport:

```rust
match event {
    ClientEvent::Event { name, data } if name == "hearth.handoff" => {
        let endpoint = data["endpoint"].as_str().unwrap_or_default();
        let ticket = data["ticket"].as_str().unwrap_or_default();
        // connect_out_of_band(endpoint, ticket)
    }
    ClientEvent::Event { name, data } if name == "hearth.handoff_ended" => {
        // session over: data["sid"], data["reason"]
    }
    _ => {}
}
```

**Rules for both SDKs.**

- A ticket is single use and dies 60 s after it is minted. Do not log it, share
  it with other players, or retry a redeem with it; reconnecting to the room
  while the session is `ready` or `started` issues a fresh one.
- Do not forward `hearth.*` events to the application's generic event handler.
  Godot already routes them; a Rust client must do the same.
- `hearth.handoff_ended` can arrive before the client has connected to the
  dedicated server (for example if the server died); treat it as the session
  being over and discard any unused ticket.
- The lobby socket and the dedicated-server socket are separate. Finding a
  rematch means going back to the lobby room; the session id is only for the
  match.

## 10. Security

| Property | Implementation |
| --- | --- |
| Ticket | 32 random hex characters (16 random bytes); only its SHA-256 is stored |
| Ticket lifetime | 60 s, single use, bound to `(session, player)`; the redeem only accepts the session id it was minted for |
| Session secret | 32 random bytes as 64 hex characters, generated per session; only its SHA-256 is stored |
| Secret delivery | once to the allocator (webhook body) or the container (start env); never in a client-visible value |
| Comparisons | constant time for ticket and secret hashes |
| Redeem errors | 403 deliberately says nothing about which check failed (bad secret, used, expired, foreign ticket) |
| Server isolation | one session secret per session; a server cannot read or redeem another session's players |
| Container identity | the gateway sets identity headers from the redeem; client-sent `X-Hearth-*` headers are stripped |
| Reserved events | `ctx.send`/`ctx.broadcast` drop names starting with `hearth.` and log `reserved_event_name`; clients must not surface them generically |
| Endpoint validation | scheme allowlist and a bounded, whitespace-free ASCII tail before a client sees the endpoint |

The room's internal handoff paths are not routed publicly; the gateway builds
them itself. The session secret never appears in the room state, a patch or an
event.

## 11. Billing

Hearth bills the **coordination**: the room's own requests, rows and duration
while it exists, plus the gateway requests for redeems and reports. The match
itself is not Hearth's bill:

- **Cloudflare Containers** bill vCPU, memory and disk per running second on
  **your** Cloudflare account, from the moment the container starts until it is
  destroyed or hits the backstop. The gateway proxy requests are ordinary Worker
  requests on top.
- **Your own fleet** bills whatever your infrastructure bills. Hearth only sends
  the signed allocation requests (up to one per attempt, at most three) and one
  best-effort stop request per session.
- `hearth-mp cost` models Workers, Durable Objects and ticked-mode rooms; it does
  **not** model container time or your fleet (see [cost.md](cost.md)).

## 12. The sample and validation status

[`examples/arena-dedicated`](../examples/arena-dedicated/README.md) is the
worked example: `ArenaLobby` (event mode, `Ready`/`Unready` intents) hands two
ready players to a dedicated arena server, then records the result. The same room
code serves both allocators — `allocator = "container"` is the compile-time
default, and `HEARTH_ALLOCATOR_MODE=webhook` switches to the fleet path:

| File | What it is |
| --- | --- |
| `src/lib.rs` | the lobby room, the `container_object!(ArenaServer)` class, gateway routes |
| `server/` | the dedicated arena server: `/healthz`, WebSocket match, reports |
| `fleet/allocator.mjs` | the reference webhook allocator (signature check, 200/409, stop) |
| `e2e-fleet.mjs` | fleet end-to-end test |
| `e2e-container.mjs` | containers end-to-end test |
| `godot-client/tests/handoff_smoke.gd` | Godot client handoff smoke test |

Run the fleet path locally with `wrangler dev` and `node e2e-fleet.mjs` (see the
sample README). The containers path needs a host where `wrangler dev` can run
Containers.

**Validation status.**

- Green: the handoff runtime suite, the allocator tests, the Rust SDK tests and
  the Godot `HearthServer` helper test (`tests/server_helper.gd`). The fleet e2e
  passes locally against `wrangler dev`: per-player private tickets, lifecycle
  through `ended` with a winner in player stats, replaying a used ticket answers
  403, a report with the wrong secret answers 403, and an intent is answered
  while a 10-second allocation is in flight.
- `e2e-container.mjs` **partly verified online** (2026-10-25): checks 1-5 passed
  against a fresh deployed Worker with Cloudflare Containers — a real container
  start with the session env, the gateway WebSocket proxy with per-player
  tickets, the redeemed identity echoed by the server (a forged
  `X-Hearth-Player-Id` ignored), 150 broadcasts per player at 30 Hz with
  monotonic timestamps, the lobby lifecycle through `result`/`ended`, and
  replaying a used ticket answering 403. Check 6 (kill the container mid-match)
  runs only against local Docker and remains unverified; local `wrangler dev`
  Containers on Windows remains unsupported, so the script exits 2 there.
  The dedicated-server image builds with `docker build` from the repository root.
- Godot `tests/handoff_smoke.gd` **passes** against that deployment, natively and
  with `HEARTH_FORCE_WEB=1` — two clients ready up, receive tickets, connect
  through the gateway, see the redeemed identity and the 30 Hz broadcasts, and
  see the session end.
- Unverified: only `e2e-container.mjs` check 6 (kill the container mid-match →
  `failed` with `lost` after the 60 s heartbeat timeout), which needs local
  Docker. Local `wrangler dev` Containers on Windows remains unsupported.
  The dedicated-server image builds with `docker build` from the repository root.
- Deferred: the Unity SDK (v0.5 Task 5); `mode = "handoff"` is a compile error
  that points at `ctx.handoff`.

## Further reading

- [protocol.md](protocol.md): the reserved `hearth.handoff` and
  `hearth.handoff_ended` events and the `EVENT` frame.
- [limits.md](limits.md): the hard line and the per-mode cost profile.
- [cost.md](cost.md): the `hearth-mp cost` estimate and what it leaves out.
- [ticked.md](ticked.md): the 5–20 Hz rooms that stay on Hearth.
- [The spec](../Hearth%20a%20Rust%20framework%20for%20Durable%20Object%20game%20servers.md),
  Target games and Roadmap v0.5.
