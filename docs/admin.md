# Admin and observability

A Hearth deployment can expose a reserved, token-gated admin API at `/admin/*`:
list live rooms, read any room's full server state, force-dispose a room, and
tail its patches live. `hearth-mp inspect` is the CLI client; the JSON API below is
stable enough to script against directly.

The admin API is a library (`hearth-mp-admin`) that the game Worker mounts itself.
It is **off unless `HEARTH_ADMIN_TOKEN` is set**; the token must be at least 32
bytes, and there is no admin HTML UI.

## 1. Set the token

Generate 32 random bytes as 64 hex characters, then store it as a Worker secret:

```sh
openssl rand -hex 32
wrangler secret put HEARTH_ADMIN_TOKEN
```

For `wrangler dev`, put the same value in `.dev.vars` in your project (never
commit that file):

```sh
HEARTH_ADMIN_TOKEN=<64 hex characters>
```

**With no token, or a token shorter than 32 bytes, every `/admin/*` path returns
404** — the feature does not exist. A wrong token on an enabled deployment
returns 401. The token is only ever read from the `Authorization: Bearer` header,
never from a query string.

## 2. Use the CLI

```sh
hearth-mp inspect rooms [--type T] [--all]
hearth-mp inspect room <type>/<name>
hearth-mp inspect dispose <type>/<name> [--yes]
hearth-mp inspect tail <type>/<name> [--ops]
```

| Command | What it does |
| --- | --- |
| `rooms` | Table of live rooms: `TYPE NAME PLAYERS AGE`. `--type` filters by room type; `--all` follows the API's pagination cursors to print every room. |
| `room` | Pretty-printed `InspectDoc` JSON: seats, timers, room flags and the **full** server state. |
| `dispose` | Prompts before wiping the room's storage and disconnecting everyone. `--yes` skips the prompt. Also the remedy for a room in `STATE_INCOMPATIBLE` mode. |
| `tail` | Prints one line per handler pass: `#<seq> <causes> <n> ops`, for example `#4 intent a#1 3 ops`. `--ops` appends the raw patch ops as JSON on the same line. Stop with Ctrl+C. |

Flags and environment:

- `--url <base>` — default `HEARTH_URL`, else `http://127.0.0.1:8787`.
- `HEARTH_ADMIN_TOKEN` — the token. When the URL is localhost the CLI falls back
  to `HEARTH_ADMIN_TOKEN` in `.dev.vars` in the current directory.
- Exit codes: `0` success, `1` request or API error, `2` usage error or missing
  token. With no token it says `set HEARTH_ADMIN_TOKEN (see docs/admin.md)`.

`hearth-mp inspect room` shows **all** state, not the filtered client view:
`#[sync(owner)]` fields (other players' hands), fields with no `#[sync]` at all,
and server-only bookkeeping are all included. Treat its output as sensitive.

## 3. The JSON API

All routes require `Authorization: Bearer <HEARTH_ADMIN_TOKEN>`. `{type}` must be
one of the deployment's configured room types; `{name}` must be 1–64 characters
of `[A-Za-z0-9_-]`. Responses other than 2xx are `{"error": "..."}` JSON.

| Status | Meaning |
| --- | --- |
| 200 / 204 | Success. |
| 400 | Invalid room name. |
| 401 | `admin token rejected`. |
| 404 | The admin API is disabled (no token on that deployment), the room type is not routed, or the room does not exist. |
| 501 | The registry is not configured on this deployment (see below). |

### `GET /admin/rooms?type=&cursor=&limit=`

Lists the rooms that reported themselves to the registry. `type` filters by room
type; `limit` is clamped to 1–100; `cursor` is the opaque `next` string the
previous page returned (internally a base64url, no-padding encoding of the JSON
`{"room_type": "...", "name": "..."}` pair). Pass `next` back verbatim.

```json
{
  "rooms": [
    {
      "room_type": "tictactoe",
      "name": "9f3c1a",
      "players": 2,
      "created_ms": 1750000000000,
      "updated_ms": 1750000001000
    }
  ],
  "next": "eyJyb29tX3R5cGUiOiJ0aWN0YWN0b2UiLCJuYW1lIjoiOWYzYzFhIn0"
}
```

`next` is `null` on the last page. The list is ordered by `(room_type, name)`.
`players` counts seated players, including held seats awaiting reconnect.

The registry is optional. If the deployment has no `HEARTH_REGISTRY` Durable
Object binding, rooms skip reporting and this route returns
`501 {"error":"HEARTH_REGISTRY binding not configured"}`. Inspect, dispose and
tail still work, because they address a room by name instead of searching for it.

### `GET /admin/rooms/{type}/{name}`

Read-only: a name that never opened a room returns 404 and creates no storage.

```json
{
  "room": {"room_type": "tictactoe", "name": "9f3c1a"},
  "created_ms": 1750000000000,
  "seq": 12,
  "locked": false,
  "incompatible": false,
  "clients": [
    {"player_id": "p-1", "seat": 0, "connected": true, "held_until_ms": null},
    {"player_id": "p-2", "seat": 1, "connected": false, "held_until_ms": 1750000060000}
  ],
  "timers": [["u:turn", 1750000005000]],
  "state": {"cells": [null, "X", null], "secrets": {"p-1": "s"}},
  "state_bytes": 84
}
```

- `clients` lists seats; a held seat has `connected: false` and its reconnect
  deadline in `held_until_ms`.
- `timers` lists every scheduled timer as `[key, due_ms]`.
- `state` is the **full** server state as JSON, with server-only and
  owner-only fields included. It is `null` when `incompatible` is true.
- `state_bytes` is the size of the persisted state snapshot.

### `POST /admin/rooms/{type}/{name}/dispose`

Returns 204. Closes every socket with code 4008, runs `on_dispose` when the state
is compatible (an error is logged and ignored), wipes the storage and reports the
disposal to the registry. Force-dispose works in `STATE_INCOMPATIBLE` mode, which
is the way out of a state that no longer matches the deployed schema. 404 when
the room never existed.

### `GET /admin/rooms/{type}/{name}/tail` (WebSocket)

Upgrades to a WebSocket admin tail and then sends one text frame per handler pass
that changed the unfiltered full state or rejected an intent:

```json
{
  "seq": 4,
  "cause": [{"cause": "intent", "player": "a", "intent_seq": 1}],
  "ops": [{"op": "set", "path": ["cells", 1], "value": "X"}],
  "rejected": []
}
```

- `ops` is the unfiltered full-state diff: fields that are hidden from players
  appear here. The same `PatchOp` encoding as the player wire protocol.
- `cause` names what produced the pass: `join`/`reconnect`/`leave`/`disconnect`
  carry `player`, `intent` carries `player` and `intent_seq`, `timer` carries
  `key`, and `dispose` carries nothing.
- `rejected` lists the intent seqs that failed in that pass.
- Tail sockets never take a seat, never receive player frames, and do not keep an
  empty room alive. They are closed with 4008 when the room is disposed.

The bearer token must be sent on the WebSocket upgrade request. Browsers cannot
set headers on `new WebSocket(...)`, so use the CLI or a server-side client.

## 4. Put Cloudflare Access in front of `/admin/*`

The bearer token is the last line of defence, not the first. In production,
**strongly recommended:** put [Cloudflare Access](https://developers.cloudflare.com/cloudflare-one/policies/access/)
in front of the admin routes (or the separate admin Worker's hostname) so every
request must pass your SSO policy before it reaches the Worker. Then a leaked
token alone is not enough to read or wipe rooms, and Access gives you an audit
trail of who tried. Keep `HEARTH_ADMIN_TOKEN` enabled as well.

Access evaluates the WebSocket upgrade like any other request; policy it so the
machines that run `hearth-mp inspect` are allowed. If an interactive Access challenge
blocks the CLI, call the Worker's direct `workers.dev` URL from a trusted network
path, or give those machines an Access service token.

## 5. Run the admin as a separate Worker

By default the game Worker mounts `/admin/*` itself. For tighter isolation —
separate secrets, separate Access policy, no admin routes on the public game
hostname — deploy `hearth-mp-admin` as its own Worker. Its Durable Object bindings
use `script_name` to reach the game Worker's classes.

A second `wrangler.admin.toml` in the project:

```toml
name = "my-game-admin"
main = "admin/src/lib.rs"
compatibility_date = "2026-10-25"
workers_dev = false

# Same binding names as the game Worker, but to classes it owns.
[[durable_objects.bindings]]
name = "HEARTH_REGISTRY"
class_name = "HearthRegistry"
script_name = "my-game"

[[durable_objects.bindings]]
name = "ROOM"
class_name = "MyGameObject"
script_name = "my-game"

[observability]
enabled = true
```

Notes:

- Repeat one binding per room class the game Worker owns. `HearthRegistry` comes
  from the `hearth_mp::admin::registry_object!()` macro you added to the game
  Worker; the room class names are the ones in the game Worker's
  `wrangler.toml`.
- Do **not** copy the game Worker's `[[migrations]]` block: the game Worker owns
  those classes and applies their migrations.
- The admin Worker's entry is the same library call the game Worker makes:

  ```rust
  use worker::{event, Context, Env, Request, Response};

  #[event(fetch)]
  async fn fetch(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
      hearth_mp::admin::serve_admin(req, env, &routes()).await
  }
  ```

  where `routes()` is the same `&[hearth_mp_gateway::RoomRoute]` list the game
  Worker puts in its `hearth_mp_gateway::GatewayConfig`. Everything that is not
  `/admin/*` must 404 (or be left unrouted).
- The admin Worker's only secret is `HEARTH_ADMIN_TOKEN`
  (`wrangler secret put HEARTH_ADMIN_TOKEN -c wrangler.admin.toml`). The game
  Worker can leave its own `HEARTH_ADMIN_TOKEN` unset, which keeps `/admin/*`
  404 on the public game hostname while still reporting rooms to
  `HEARTH_REGISTRY`.

## 6. Workers Logs

When the game Worker has `[observability] enabled = true` (the generated
`wrangler.toml` sets it), the room runtime writes one JSON line per interesting
event to Workers Logs: the `wrangler dev` console locally, and **Workers Logs**
in the dashboard in production. Filter on the `hearth` field to find them.

| `"hearth"` value | When |
| --- | --- |
| `join` | A player joined a room. |
| `leave` | A player left or disconnected (a held seat logs its later expiry). |
| `dispose` | A room was disposed, by the empty-room timer or by force-dispose. |
| `handler_error` | A room handler returned an error; that pass's state changes, events and timers were rolled back. |
| `state_incompatible` | The persisted state does not match this build's schema. Force-dispose the room. |
| `tick_dropped` | A ticked room woke late and skipped `n` ticks (at most 3 catch-up steps run per wake). |
| `tick_overrun` | A ticked room's steps took longer than one tick period (`ms` holds the time spent). |
| `handoff_retry` | A handoff allocation attempt failed and will be retried after the backoff (`attempt` and `msg` hold the attempt number and error). |
| `handoff_unknown_player` | A room asked for a handoff with a player who is not seated; the request was dropped. |
| `reserved_event_name` | A room tried to send a framework-reserved `hearth.*` event; the send was dropped (`name` holds it). |
| `meta_error` | The room's lobby/matchmaker metadata could not be written; the room keeps running. |
| `report_error` | A room report could not be queued for the matchmaker. |
| `outbox_dropped` | A queued `ctx.notify` was dropped after 24 h without a successful delivery. |
| `outbox_error` | A queued notify delivery failed; it retries with backoff. |

Each line also carries the room, `player` when one applies, and `msg` for
errors. Logs are for operators, not for game data: keep chat and secrets out of
`msg`.
