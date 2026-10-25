# Hearth

Authoritative multiplayer rooms in Rust on Cloudflare Durable Objects. Implement
one `Room` trait; the framework owns sockets, state sync, persistence,
hibernation and auth. A Godot SDK is included.

**Hard line:** Hearth v0.1 rooms are event-driven. Anything needing more than
~20 Hz authoritative simulation, UDP, or more than ~100 MB of live state per
match is the wrong fit; use a dedicated server (Handoff mode arrives in v0.5).
The cost profile per mode and every hard limit are in
[docs/limits.md](docs/limits.md).

## Quickstart

```sh
cargo install hearth-cli        # or: cargo install --path crates/hearth-cli
hearth new my-game && cd my-game
```

1. Implement your room in `src/lib.rs` (example below). The generated file also
   has the `#[event(fetch)]` gateway entry that authenticates and routes sockets
   to your room, so a fresh project deploys as-is.
2. Store the auth secret once: the gateway (`hearth-gateway`) refuses to serve
   without it. Local dev reads it from `.dev.vars` (copy `.dev.vars.example`, min
   32 bytes); deployed Workers use a Worker secret:
   ```sh
   wrangler secret put HEARTH_SECRET
   ```
3. Run it against workerd, then deploy:
   ```sh
   hearth dev
   hearth deploy
   ```

The room type is the name you passed to `hearth new` in kebab case (`my-game`
above). Clients log in anonymously and fetch the schema hash from the gateway:
`POST /auth/anonymous`, `POST /rooms/<room-type>`, `GET /schema/<room-type>`,
then upgrade `GET /rooms/<room-type>/<room-name>` to a WebSocket.

`hearth dev` and `hearth deploy` shell out to wrangler; install it with
`npm i -g wrangler`. `hearth new` takes `--hearth-path <repo>` to generate path
dependencies against a local checkout instead of the published crates.

Godot: copy `godot/addons/hearth` into your project's `addons/` directory. The
`Hearth` autoload exposes `login_anonymous`, `create_room`, `join`,
`send_intent`, `leave` and a `state` dictionary, keyed by the same room type
string; the tic-tac-toe Godot sample wires them up.

## A room

```rust
use hearth::{Client, Ctx, HearthState, JoinOpts, Leave, PlayerId, Room, SyncMap};
use serde::{Deserialize, Serialize};
use worker::{event, Context, Env, Request, Response};

#[derive(HearthState, Serialize, Deserialize, Default)]
#[hearth(crate = "hearth")]
pub struct Match {
    #[sync] pub count: i64,                             // synced to everyone
    #[sync] pub seats: SyncMap<PlayerId, u8>,           // synced map
    #[sync(owner)] pub hand: SyncMap<PlayerId, u8>,     // sent only to its owner
}

#[derive(Deserialize)]
pub enum Intent {
    Add(i64),
}

#[derive(Default)]
pub struct Counter;

#[hearth::room(mode = "event", max_clients = 4)]
impl Room for Counter {
    type State = Match;
    type Intent = crate::Intent;

    async fn on_join(&mut self, ctx: &mut Ctx<Self>, who: &Client, _opts: JoinOpts) -> hearth::Result<()> {
        ctx.state().seats.insert(who.player_id.clone(), who.seat);
        Ok(())
    }

    async fn on_intent(&mut self, ctx: &mut Ctx<Self>, _who: &Client, intent: Self::Intent) -> hearth::Result<()> {
        match intent {
            Intent::Add(n) => ctx.state().count += n,
        }
        Ok(())
    }

    async fn on_leave(&mut self, ctx: &mut Ctx<Self>, who: &Client, _reason: Leave) -> hearth::Result<()> {
        ctx.state().seats.remove(&who.player_id);
        Ok(())
    }
}

/// Authenticate, route the socket to this room's Durable Object, stay out of
/// the way. `binding` matches wrangler.toml.
#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    hearth_gateway::serve(
        req,
        env,
        &[hearth_gateway::RoomRoute {
            room_type: "my-game",
            binding: "ROOM",
            schema_hash: <Match as HearthState>::schema_hash_hex,
        }],
    )
    .await
}
```

**Keep durable data in `State`; the room struct resets on every wake.** Only
`#[sync]` fields leave the server (and `#[sync(owner)]` fields only reach their
owner); unmarked fields stay server-side. State changes are diffed and pushed as
patches, and one handler pass is atomic: if a handler returns `Err`, its state
changes, events and timers are rolled back.

## Docs

- [Wire protocol v1](docs/protocol.md): frames, opcodes, patch ops, handshake.
- [Limits and when not to use Hearth](docs/limits.md): the hard line, per-mode
  cost profile, memory/CPU budgets, JSON number precision.
- [v0.1 implementation plan](docs/superpowers/plans/2026-10-25-hearth-v0.1-event-rooms.md)

## Licence

MIT OR Apache-2.0
