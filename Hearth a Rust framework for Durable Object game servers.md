# Hearth: a Rust framework for Durable Object game servers

Oct 25, 2026 · @Lee Ingram

## Summary

Hearth is an MIT/Apache-2.0 Rust framework that turns Cloudflare Durable Objects into an authoritative game backend. Game logic is written once as a Rust `Room` trait and compiled to Wasm via `workers-rs`. Godot and Unity clients connect through first-party SDKs. The pitch is a Colyseus/Nakama-style developer experience with no servers to run, near-zero idle cost and hibernation built in.

**Goals**

- An indie dev ships a working multiplayer prototype in under an hour: `cargo hearth new`, implement one trait, `hearth deploy`, drop the plugin into Godot or Unity.
- Rooms cost nothing while idle. Hibernation is the default, not an optimisation the dev has to understand.
- Server-authoritative by design. Clients send intents; the room validates and mutates state.
- Automatic state sync via schema'd delta patches, so devs never hand-write netcode for the common case.
- Shared Rust logic crate that can also run client-side (GDExtension, or native plugin in Unity) for prediction.
- Fully self-hostable on the dev's own Cloudflare account. No Hearth-run SaaS required.

**Non-goals**

- Twitch-shooter netcode: rollback, lag compensation, 60Hz physics. Out of scope; Hearth hands off to a dedicated server instead (see Target games).
- UDP or WebRTC transport in v1.
- A JS/TS server API. Rust only; JS exists only as generated glue.
- Replacing an engine's scene graph or physics on the server.

## Target games

Hearth supports two room modes and is explicit about where each stops. A room picks its mode at compile time; the framework enforces the cost implications.

| Mode | How it runs | Fits | Cost profile |
| --- | --- | --- | --- |
| Event (default) | Wakes per message or alarm, hibernates between | Turn-based, card/board, party games, async ("your move" over days), lobbies, social hubs | Billed per message; idle rooms near free |
| Ticked | Alarm-driven fixed tick, 5–20 Hz, while players are connected | Co-op, top-down action, .io-style, MMO-lite zones with tolerant netcode | Billed wall-clock while ticking; stops when room empties |
| Handoff | Room does matchmaking and session state, then issues a ticket to a dedicated server (Cloudflare Containers or the dev's own fleet) | Shooters, fighting games, physics-heavy action | Hearth only bills for the coordination part |

The hard line: anything needing more than \~20 Hz authoritative simulation, UDP, or more than \~100MB of live state per match goes to Handoff. Hearth should say so loudly in docs rather than let people discover it in production.

## Architecture

A single Gateway Worker authenticates every connection and routes it to a named Durable Object. Each match is one `Room` DO; everything else is a small set of coordination DOs.

&#91;embedded content: Hearth topology · 1 Worker, 4 DO classes, optional dedicated server\]

The Matchmaker hands out room names; the Room is the authority for its match and reports results to Player and Leaderboard objects. Handoff-mode rooms issue a ticket to a dedicated server and stay involved only for session state.

**Room lifecycle**

1. **Create.** The Matchmaker (or a direct `create` call) mints a room name with a location hint from the first player's region. The DO instantiates on first request.
2. **Join.** The Gateway forwards the WebSocket upgrade; the room accepts it through the hibernation API, tags it with player ID and seat, and sends `FULL_STATE`.
3. **Active.** Each incoming intent wakes the room, runs `on_intent`, flushes one patch to clients and persists. Ticked rooms run on a self-rescheduling alarm instead.
4. **Idle.** No handlers running → the runtime hibernates the object. Sockets stay open; in-memory state is dropped and rebuilt from SQLite on next wake.
5. **Empty.** Last player leaves → grace timer → `on_dispose` hook → storage wiped or archived to R2 for replays.

**Why one DO per room, not one per region or per shard:** it matches the single-threaded model exactly, isolates failures, and makes idle rooms free. The 32,768 sockets-per-object ceiling is irrelevant at room scale; the real limits are CPU per handler and 128MB memory.

## Server API

A game dev implements one trait per room type. The framework owns sockets, hibernation, persistence and sync; the dev owns rules.

```rust
#[derive(HearthState, Serialize, Deserialize, Default)]
pub struct Match {
    #[sync] pub players: SyncMap<PlayerId, Player>,
    #[sync] pub turn: PlayerId,
    #[sync(owner)] pub hand: SyncMap<PlayerId, Vec<Card>>, // only sent to owner
    pub deck: Vec<Card>,                                   // server-only
}

#[derive(Deserialize)]
pub enum Intent { PlayCard(CardId), EndTurn }

#[hearth::room(mode = "event", max_clients = 4)]
impl Room for CardMatch {
    type State = Match;
    type Intent = Intent;

    async fn on_join(&mut self, ctx: &mut Ctx<Self>, who: &Client, opts: JoinOpts) -> Result<()>;
    async fn on_intent(&mut self, ctx: &mut Ctx<Self>, who: &Client, intent: Intent) -> Result<()>;
    async fn on_leave(&mut self, ctx: &mut Ctx<Self>, who: &Client, reason: Leave) -> Result<()>;
    async fn on_timer(&mut self, ctx: &mut Ctx<Self>, key: TimerKey) -> Result<()> { Ok(()) }
    async fn on_tick(&mut self, ctx: &mut Ctx<Self>, dt: Duration) -> Result<()> { Ok(()) } // ticked mode only
}
```

`Ctx` exposes the room's capabilities:

- `ctx.state()` — mutable state; changes are tracked and flushed as one patch at the end of the handler.
- `ctx.send(who, msg)` / `ctx.broadcast(msg)` — ad-hoc events outside state sync (sound cues, chat).
- `ctx.timer(key, after)` — durable timers multiplexed onto the single DO alarm. Survive hibernation and redeploys.
- `ctx.sql()` — direct access to the room's SQLite for logs, replays, leaderboard snapshots.
- `ctx.call::<OtherRoom>(id, rpc)` — typed DO-to-DO RPC (e.g. room reports result to a `Leaderboard`).
- `ctx.kick`, `ctx.lock` (stop new joins), `ctx.dispose` (wipe storage, release the name).

Design rules the framework enforces:

- Handlers are short and synchronous in spirit. No `setTimeout`-style futures held across handlers; a lint and a debug-mode runtime assert catch them, since they silently block hibernation.
- State is persisted to SQLite on every handler exit in event mode, every N ticks in ticked mode. Rehydration after hibernation is automatic.
- Per-client data (identity, seat, reconnect token) rides in the WebSocket attachment so it survives hibernation without a storage read.

## Wire protocol and state sync

One binary protocol over WebSocket, specified independently of the Rust code so third-party SDKs (Defold, Bevy, web) can implement it.

- **Encoding:** MessagePack frames with a 1-byte opcode. JSON mode available for debugging. Schema hash exchanged in the handshake; mismatch is a hard reject with a clear error.
- **Opcodes:** `HELLO`, `JOIN_OK`, `FULL_STATE`, `PATCH`, `INTENT`, `EVENT`, `ACK`, `PING`, `ERROR`, `LEAVE`.
- **State sync:** server sends `FULL_STATE` on join/reconnect, then `PATCH` frames: a sequence number plus a list of path-level ops (`set`, `del`, `map_insert`, `list_splice`) generated by the `#[sync]` derive. Visibility filters (`#[sync(owner)]`, `#[sync(team)]`, custom predicate) produce per-client patches.
- **Intents:** client-assigned sequence IDs; server `ACK`s with the state seq where it was applied. That's the hook for client prediction and reconciliation.
- **Reconnect:** client holds a reconnect token from `JOIN_OK`. Within a grace window (default 30s, seat held), it resumes and gets either missed patches from a ring buffer or a fresh `FULL_STATE`.
- **Keepalive:** uses DO auto-response for ping/pong so idle connections never wake the room.

Billing note baked into the design: incoming messages are what Cloudflare bills, outgoing are free. The SDKs batch intents per frame by default and expose a send-rate cap.

## Client SDKs

Both SDKs wrap one shared Rust core (`hearth-client`): protocol, patch application, reconnect, intent queue. The core is exposed through each engine's native extension path, so protocol bugs get fixed once. Codegen from the server's state schema produces typed engine-native classes.

|  | Godot | Unity |
| --- | --- | --- |
| Integration | GDExtension built with `godot-rust`; distributed via Asset Library | Native plugin (C ABI via `cbindgen`) + C# wrapper; distributed as UPM git package and OpenUPM |
| Typed state | `hearth codegen --godot` emits GDScript resource classes with signals per field | `hearth codegen --unity` emits C# classes with `OnChange` events |
| Usage shape | `HearthClient` autoload node; `room.state.players.item_added.connect(...)` | `HearthClient` MonoBehaviour; `room.State.Players.OnAdd += ...`; async/await join |
| Web export | Falls back to a pure-GDScript protocol implementation (GDExtension on web is fragile) | WebGL build uses a JS WebSocket bridge + Wasm core |
| Prediction | Optional: compile the game's shared logic crate into the same GDExtension | Optional: shared logic crate in the native plugin |
| Editor tools | Dock showing connected room, live state tree, patch log | Editor window with the same, plus a "local server" play-mode toggle |

Codegen is the key DX feature. The server crate is the single source of truth for state shape, and both engines get autocomplete for it. A schema change bumps the hash, so stale clients fail loudly instead of desyncing.

## Persistence, auth, ops, cost

**Persistence.** Each room's state lives in its own DO SQLite: a `state` row (latest snapshot), an optional `events` table (intent log for replays and anti-cheat audits), and dev-defined tables via `ctx.sql()`. Per-player data lives in a `Player` DO keyed by player ID, so it outlives any one match. Cross-room aggregates (global leaderboards) use a sharded `Leaderboard` DO or D1, the dev's choice.

**Auth.** The Gateway Worker verifies a token before any DO is touched. Pluggable providers: anonymous device ID, signed JWT from the dev's own backend, Steam session ticket, itch.io, Discord OAuth. Identity is passed to the room as `Client` and stored in the socket attachment. Rooms never see raw credentials.

**Abuse limits.** Gateway-level rate limiting per IP and per player; per-room intent rate caps with configurable kick thresholds; max message size enforced before deserialisation.

**Ops.** `hearth deploy` wraps `wrangler`, generates the JS shim and migrations, and runs DO class migrations safely on schema changes. Observability via Workers Logs plus a Hearth admin Worker: list live rooms, inspect state, force-dispose, tail patches. Local dev uses `wrangler dev` (workerd) so the full stack runs offline.

**Cost model.** Event-mode rooms bill mostly per incoming message (WebSocket messages are billed at a 20:1 ratio to requests) and are near-free while idle. Ticked rooms bill wall-clock duration while players are present. `hearth cost` estimates monthly spend from CCU, room size, intents/sec and tick rate, so devs see the bill before launch.

## Crate layout and roadmap

| Crate | Purpose |
| --- | --- |
| `hearth-protocol` | Opcodes, frame types, patch ops. `no_std`-friendly; the spec's reference implementation |
| `hearth-sync` | `SyncMap`/`SyncVec`, change tracking, `#[derive(HearthState)]`, visibility filters |
| `hearth-server` | `Room` trait, `Ctx`, timer multiplexing, hibernation plumbing on top of `workers-rs` |
| `hearth-gateway` | Router Worker: auth, matchmaking, room ID resolution |
| `hearth-client` | Engine-agnostic client core: socket, patch apply, reconnect, intent queue |
| `hearth-godot` | GDExtension wrapper + GDScript fallback |
| `hearth-unity` | C ABI + C# package |
| `hearth-cli` | `new`, `dev`, `codegen`, `deploy`, `cost`, `inspect` |
| `hearth-test` | In-process room harness: simulated clients, virtual clock, hibernation fuzzing |

**Roadmap**

1. **v0.1 — Event rooms.** Protocol, sync derive, Room trait, anonymous auth, Godot SDK, one sample (tic-tac-toe → card game).
2. **v0.2 — Unity + codegen.** Unity package, typed codegen for both engines, reconnect, admin Worker.
3. **v0.3 — Matchmaking + players.** Lobby and Matchmaker DOs, Player DO, JWT/Steam auth, `hearth cost`.
4. **v0.4 — Ticked rooms.** Alarm tick loop, interpolation helpers in SDKs, shared-logic prediction sample.
5. **v0.5 — Handoff.** Ticket API and a Cloudflare Containers adapter for dedicated servers.
6. **v1.0.** Protocol frozen, semver guarantees, docs site, three shipped sample games.

**Open questions**

- Is alarm granularity reliable enough for a steady 20 Hz tick, or does ticked mode need a held-open event loop (and its cost)? Needs benchmarking first.
- Should `hearth-sync` reuse an existing CRDT/patch format (Automerge, Yrs) or stay a minimal custom one? Leaning custom: authoritative server, no merge semantics needed.
- Wasm bundle size limits on Workers once games pull in serious logic crates.
- Licence for the Unity package on the Asset Store vs UPM only.
- Naming: "Hearth" is a placeholder; check crates.io and trademark collisions.

**Sources**

- [Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing)
- [Durable Objects WebSocket API](https://developers.cloudflare.com/durable-objects/api/websockets)
- [Durable Objects FAQ](https://developers.cloudflare.com/durable-objects/reference/faq)
- [Durable Objects Rust API (workers-rs)](https://developers.cloudflare.com/durable-objects/api/workers-rs)
- [worker-macros: durable\_object attribute](https://docs.rs/crate/worker-macros/0.8.0/source/src/lib.rs)
