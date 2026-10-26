# Matchmaking

Hearth v0.3a ships a matchmaker: a built-in `Service` that hands out room
names for quick-join, holds seats for players while they connect, and serves
the lobby listing. The room is still the authority for its match — the
matchmaker only remembers which rooms exist, who is seated, and which seats are
reserved.

The matchmaker is optional. Enable it in the gateway config to get
`POST /match/{type}` and `GET /lobby/{type}`; with `matchmaker: false` both
routes answer 501 (section 11). One matchmaker Durable Object exists per
`(room_type, region)`, named `"{room_type}:{region}"`.

## 1. The quick-join flow

1. The client authenticates (`POST /auth/anonymous`) and gets a bearer token.
2. It calls `POST /match/{type}` with that token, optionally naming a pool:
   `{"pool": "default"}`. A missing body or a body without `pool` means the
   default pool, and the player is the token's player id, never a body field.
3. The gateway picks the region from the request's Cloudflare continent (see
   section 6) and calls the matchmaker instance `"{type}:{region}"` with
   `QuickJoin { player, pool, capacity }`, where `capacity` is
   `RoomRoute.max_clients`.
4. The matchmaker picks a room, reserves the player a seat, and answers
   `200 {"room": "m_enam_0123456789abcdef", "region": "enam"}`.
5. The client opens the WebSocket at
   `GET /rooms/{type}/{room}?token=<token>` and sends `HELLO`, exactly as for a
   room it minted itself (see [protocol.md](protocol.md)). The room patches its
   clients and reports its roster to the matchmaker.

A reserved seat is held for 30 seconds (`RESERVATION_MS`). If the connect does
not happen in time, the reservation expires and the seat can be given to
someone else. A matched room can still refuse the join (the room is the
authority); on `ROOM_FULL` or a rejection, call `POST /match` again for a new
room.

## 2. How quick-join chooses a room

The matchmaker runs these steps in order, per player:

1. **Seated already.** If the player appears in a room's roster, return that
   room. Reports can lag an actual join, so this makes a repeated ask
   idempotent.
2. **Reserved already.** If the player still has an unexpired reservation,
   return that room. Asking twice never moves a player.
3. **Fill a room.** Consider the unlocked rooms in the requested pool, in this
   `(room_type, region)` matchmaker. A room's free seats are
   `capacity - seated players - unexpired reservations`. Pick the room with
   the fewest free seats (fill it before starting another), breaking ties by
   oldest minted room, then by name.
4. **Mint a room.** If no room has a free seat, mint
   `m_<region>_<16 hex>`, insert it in the requested pool with the requested
   capacity, and reserve the player in it.

The capacity comes from the request (`RoomRoute.max_clients`), so a just-minted
room can take the next quick-join before any room report has arrived. Later
room reports overwrite the stored capacity with the room's real
`max_clients`.

The room's own rules still apply once the player connects: a matchmaker room
whose `on_join` errors will refuse the join, and the client falls back to
`POST /match` (the seat is held for it for 30 seconds, so the retry is cheap).

## 3. Seat reservations

`RESERVATION_MS = 30_000` (30 seconds). A reservation counts as a used seat
until it expires or the room reports the player as seated. That is what keeps
two players from taking the last seat: both quick-joins are serialised inside
the matchmaker Durable Object, the first one reserves the last seat, and the
second one sees no free seat and gets a new room.

- Same player asking twice gets the same room (seat reservation, then roster).
- A player who never connects frees the seat after 30 seconds.
- Reservations are not listed in the lobby and are not visible to clients.
- Once the room reports the player in its roster, the reservation stops
  counting; the leftover row expires on its own.

## 4. Pools

A pool is a string that groups rooms that should be matched together. Nothing
else changes: the room, the reports and the wire protocol are the same.

- `pool` defaults to `"default"` and must be 1–32 characters of
  `[A-Za-z0-9_-]`; anything else is a 400.
- The pool is recorded when the room is minted, in the `QuickJoin` call. The
  room never learns its pool and does not need to.
- Quick-join only considers rooms in the requested pool, and the lobby only
  lists one pool at a time, so a new match never puts players from different
  pools in one room. One exception, on purpose: a player who is already seated
  in a room, or still holds a seat reservation, gets *that* room back whatever
  pool they name — asking twice must never move a player mid-game.
- Use pools for modes, maps or skill buckets (for example `"default"`,
  `"ranked"`, `"island"`).

A matchmaker Durable Object holds all pools for its `(room_type, region)`; the
pool is a column, not another Durable Object.

## 5. The lobby

`GET /lobby/{type}?pool=&limit=` lists the rooms a player can currently join.
It requires the same bearer token as `/match` (the token is only used to
authenticate; any player may list).

```json
{
  "rooms": [
    {
      "name": "m_enam_0123456789abcdef",
      "players": 1,
      "capacity": 2,
      "metadata": { "map": "island", "mode": "duel" }
    }
  ]
}
```

- `players` is the seated roster size, including seats held across a reconnect.
  It does not count unexpired reservations: a freshly minted room whose first
  player is still connecting lists `players: 0`.
- `capacity` is the room's `max_clients` as last reported (or as requested at
  mint time, before the first report).
- `metadata` is whatever the room set with `ctx.set_metadata` (section 6);
  `null` when it never set any.
- `limit` defaults to 20 and is clamped by the matchmaker to 1–50.
- Full rooms (no free seat after reservations) and locked rooms are omitted.
- Order: fewest free seats first, then oldest room, then name. This is the
  order quick-join fills them in, so the first row is the room most likely to
  be filled next.
- Only matchmaker-minted rooms in this `(room_type, region)` can appear. A
  room minted by `POST /rooms/{type}` is never listed, and rooms are listed
  only while their matchmaker knows about them: an empty room reports itself,
  and its disposal removes the row.

There is no separate lobby Durable Object. The lobby is a read of the open-room
table the matchmaker already keeps, so it cannot disagree with quick-join.

## 6. Regions and location hints

The region is derived from the request's Cloudflare `cf.continent`, in the
gateway, with no client input:

| `cf.continent` | Region key |
| --- | --- |
| `NA` | `enam` |
| `SA` | `sam` |
| `EU` | `weur` |
| `AS` | `apac` |
| `OC` | `oc` |
| `AF` | `afr` |
| missing / unknown | `any` |

The key picks the matchmaker instance (`"{room_type}:{region}"`), is embedded
in every minted room name (`m_<region>_<16 hex>`), and is passed to the room
Durable Object as a placement hint when the player connects (the same
continent map as `GET /rooms/...`). A player whose continent is unknown uses
the `any` matchmaker.

Because the matchmaker is regional, two players in different regions may get
different rooms. Sharing a room name across regions still works — a room name
is a Durable Object name, not a regional object — but neither player's
matchmaker lists it for the other.

## 7. Room metadata

A room can publish a small JSON value for the lobby:

```rust
ctx.set_metadata(serde_json::json!({ "map": "island", "mode": "duel" }));
```

`ctx.set_metadata` is a room-only effect (`Ctx`, not `ServiceCtx`). It is
buffered like any other effect: it applies when the handler returns `Ok` and is
dropped and rolled back when the handler fails. The value is persisted in the
room's `hearth_meta` table, survives hibernation, and is re-reported to the
matchmaker when it changes. A report is also sent when the roster or the lock
flag changes, so the lobby row follows the room.

Keep it small and display-only: a map name, a mode, a player-count override if
you must. It is not state sync — clients get lobby rows through `GET /lobby`,
not from the room's patches.

### Locked rooms

`ctx.lock()` marks a room locked (there is no unlock in v0.3a). Both the lobby
and quick-join omit locked rooms, so `/match` never hands out a seat that the
room would then refuse. Keep a room out of matchmaking entirely by using a
private `POST /rooms/{type}` name instead of a matchmaker-minted one.

## 8. Room reports

A room minted by the matchmaker reports itself through the durable outbox. The
gateway adds an `X-Hearth-Matchmaker: {type}:{region}` header when a room name
with the matchmaker form is connected; the room persists that and reports:

- **Open** — the first join ever, with room name, creation time, roster,
  capacity, lock flag and metadata.
- **Players** — whenever the roster, capacity, lock flag or metadata changes.
- **Disposed** — when the room is disposed, which deletes the matchmaker's
  row and its reservations.

Reports are queued as durable outbox rows and retried, so a matchmaker outage
delays the lobby but never touches gameplay. A report whose room name the
matchmaker never minted is ignored — that is the defence against forged room
names (section 10).

## 9. Private rooms

`POST /rooms/{type}` is the v0.1 minting route and is unchanged. It requires
the bearer token and mints a random 16-hex name such as `9f3c1a7d0b2e4c58`.
Those rooms are outside matchmaking:

- not pooled, never listed in `/lobby`, and they never report to the
  matchmaker;
- connect directly at `GET /rooms/{type}/{name}?token=...`;
- the name (plus a player token) is all you need to join, so keep it private
  if you do not want strangers in the room.

Use this path for friend lobbies, custom games, and tests. `POST /match` is the
only route that creates pooled rooms.

## 10. Forged `m_` names

Any valid room name can be connected, including a name the client made up. If
that name has the matchmaker form `m_<region>_<16 hex>`, the gateway treats it
as matchmaker-minted and tells the room to report to that region's matchmaker.

The room connects and plays as an ordinary room. It never appears in
`GET /lobby`, because the matchmaker ignores reports for names it never minted;
the forged name cannot add a lobby row, change a real room, or reserve a seat.
Room names are addresses, not credentials — anyone with a token and the name
can try to join a room that has a seat.

## 11. The HTTP API

Both routes require `Authorization: Bearer <player token>` and return JSON on
success. `{type}` must be one of the gateway's `RoomRoute` room types.

### `POST /match/{type}`

Seat the caller through the matchmaker.

```json
{ "pool": "default" }
```

The body is optional; `pool` defaults to `"default"`. A body that is not JSON
is treated as empty.

```json
{ "room": "m_enam_0123456789abcdef", "region": "enam" }
```

### `GET /lobby/{type}?pool=default&limit=20`

The open rooms in one pool, in matchmaker fill order. See section 5 for the
response shape.

| Status | When |
| --- | --- |
| 200 | Room reserved, or lobby listing. |
| 400 | `pool` is not 1–32 characters of `[A-Za-z0-9_-]`, or the lobby query does not parse. |
| 401 | Missing or invalid bearer token. |
| 404 | `{type}` is not in the gateway's room table. |
| 501 | Matchmaking is disabled in the gateway config. Body: `{"error": "HEARTH_MATCHMAKER binding not configured"}`. |
| 502 | The matchmaker call failed (for example the `HEARTH_MATCHMAKER` binding is not deployed). Body: `{"error": "..."}`. |

4xx bodies are plain text from the gateway; 501 and 502 are JSON. A 502 means
the call never reached a working matchmaker, so the client should treat it as
"try again later", not as "no room".

No route accepts a player id from the client: `/match` seats the token's
player, and `/lobby` ignores the player entirely.

## 12. Deploy and configure

1. Declare the matchmaker class in the game Worker crate:

   ```rust
   hearth_mp::matchmaker::matchmaker_object!();
   ```

   That expands to the `HearthMatchmaker` Durable Object class wrapping
   `hearth_mp::matchmaker::MatchmakerService`.

2. Bind it in `wrangler.toml`, with a migration for the new class:

   ```toml
   [[durable_objects.bindings]]
   name = "HEARTH_MATCHMAKER"
   class_name = "HearthMatchmaker"

   [[migrations]]
   tag = "v3"
   new_sqlite_classes = ["HearthMatchmaker"]
   ```

   The migration tag must be the next one in your project and the class must
   be SQLite-backed: the matchmaker keeps its rooms and reservations in
   SQLite.

3. Tell the gateway where the matchmaker is, and give each room type its seat
   count:

   ```rust
   let config = hearth_mp_gateway::GatewayConfig {
       rooms: &[hearth_mp_gateway::RoomRoute {
           room_type: "tictactoe",
           binding: "TICTACTOE",
           schema_hash: hearth_mp::server::schema_hash_hex::<TicTacToe>,
           max_clients: 2,
       }],
       player: None,
       matchmaker: true,
   };
   hearth_mp_gateway::serve(req, env, &config).await
   ```

   `RoomRoute.max_clients` is the capacity passed to quick-join; it must match
   the room's `#[hearth_mp::room(max_clients = ...)]`. `matchmaker: false` makes
   both routes 501 while leaving the binding (and existing rooms) alone.

4. Deploy. The matchmaker Durable Object is created on the first `/match` or
   room report; there is nothing to seed.

## 13. Under the hood

The matchmaker is `crates/hearth-mp-matchmaker` (re-exported as
`hearth_mp::matchmaker`) and is a normal `Service`, so an advanced deployment can
also call it with `ctx.call` / `ctx.notify`:

- `MatchmakerService` — binding `HEARTH_MATCHMAKER`, `type Rpc = MmRpc`,
  `type Reply = MmReply`.
- `MmRpc::QuickJoin { player, pool, capacity }`,
  `MmRpc::List { pool, limit }`, `MmRpc::Report(RoomReport)`.
- `MmReply::Room { name }`, `MmReply::Rooms(Vec<OpenRoom>)`, `MmReply::Ok`.
- `OpenRoom { name, players, capacity, metadata }`.
- `RESERVATION_MS = 30_000`, one instance per `(room_type, region)` with the
  instance name `"{room_type}:{region}"` (a missing `:` reads as region
  `"any"`).
- `parse_room_name` / the gateway's `mm_room` recognise
  `m_<region>_<16 hex>` for the keys in section 6.

## 14. Limits and shortcuts

- **One matchmaker Durable Object per `(room_type, region)`.** All pools share
  it; shard by pool when one pool outgrows a single object.
- **Quick-join only.** There is no skill, rating or team matching; add a queue
  when a game needs it.
- **No separate lobby object.** The lobby reads the matchmaker's open-room
  table.
- **Reservations expire after 30 seconds** and are never exposed to clients.
- **The lobby is capped at 50 rows per request** (the matchmaker clamps
  `limit`), and rows come from room reports, so they can trail reality by one
  report/outbox cycle.
