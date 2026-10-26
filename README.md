# Hearth

Authoritative multiplayer rooms in Rust on Cloudflare Durable Objects. Implement
one `Room` trait; the framework owns sockets, state sync, persistence,
hibernation and auth. Godot and Unity SDKs are included.

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
above). Clients log in anonymously (`POST /auth/anonymous`), mint a room
(`POST /rooms/<room-type>`), then upgrade
`GET /rooms/<room-type>/<room-name>` to a WebSocket. The schema hash guards
against stale clients: `GET /schema/<room-type>` exposes it for hand-written
clients, and the Godot codegen below embeds it in a typed class.

`hearth dev` and `hearth deploy` shell out to wrangler; install it with
`npm i -g wrangler`. `hearth new` takes `--hearth-path <repo>` to generate path
dependencies against a local checkout instead of the published crates.

## Godot

Copy `godot/addons/hearth` into your project's `addons/` directory, then generate
typed state classes from the server crate so the client's schema hash can never
drift:

```sh
hearth codegen --godot my-godot-project/hearth --manifest-path my-game/Cargo.toml
```

The generated classes are emitted from the server crate's exported `SchemaDoc`
(the `hearth_schema` JSON the room macro writes); re-run the command after every
schema change. Naming is the room name plus the Rust type name, so room
`TicTacToe` with state `Board` gives `TicTacToeBoard`. Use that class as the
schema and render from the typed state:

```gdscript
func _ready() -> void:
	Hearth.use_schema(TicTacToeBoard)   # typed state + embedded schema hash
	Hearth.state.cells.changed.connect(_render_board)
	Hearth.state.marks.item_added.connect(_on_mark_added)
	Hearth.state.status_changed.connect(_render_status)
	...
	Hearth.send_intent(TicTacToeIntents.place(4))
```

`Hearth.state` is the typed mirror after `use_schema`: one signal per leaf,
enum or option field (`status_changed`), `HearthMap`/`HearthList` signals for
containers (`item_added`/`item_removed`/`item_changed`, `changed`), and enums
expose `variant`/`value`. Without a schema, `Hearth.state` stays the raw
dictionary. The `Hearth` autoload also exposes `login_anonymous`, `create_room`,
`join`, `send_intent` and `leave`; the tic-tac-toe Godot sample wires them up.

## Unity

Unity 2022.3 or newer. Build the native plugin for your platforms, then add the
package by git URL (Package Manager → Add package from git URL):

```sh
scripts/build-unity-native.sh            # or --only windows|linux|macos|android|ios|webgl
```

```
https://github.com/<owner>/hearth-rs.git?path=unity/com.hearth.client
```

Add a `HearthClient` component and generate typed state from the server crate:

```sh
hearth codegen --unity MyUnityProject/Assets/Hearth --manifest-path my-game/Cargo.toml
```

```csharp
await client.LoginAnonymousAsync();
var name = await client.CreateRoomAsync("tictactoe");
var room = await client.JoinAsync<TicTacToeBoard>("tictactoe", name);
room.State.Marks.OnAdd += (player, mark) => ...;
room.State.Cells.OnChange += RenderBoard;
room.State.OnStatusChange += status => ...;   // status.Variant == TicTacToeStatus.Won
room.Send(TicTacToeIntents.Place(4));
```

Every callback runs on the main thread. Drops are retried (`OnReconnecting`,
`OnReconnected`); `OnDisconnected` fires only when the client gives up. WebGL
uses the wasm core (`--only webgl`) and the browser's WebSocket. `Window/Hearth`
shows each client's live state and patches, and can start `hearth dev` when you
press Play. The sample is `examples/tictactoe-unity`. Prediction with your
shared Rust logic in a custom native plugin: [docs/ticked.md](docs/ticked.md#8-prediction-in-unity)
and `examples/arena-unity`.

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

#[hearth::room(mode = "event", max_clients = 4, binding = "ROOM")]
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
    static ROOMS: [hearth_gateway::RoomRoute; 1] = [hearth_gateway::RoomRoute {
        room_type: "my-game",
        binding: "ROOM",
        schema_hash: <Match as HearthState>::schema_hash_hex,
        max_clients: 4,
    }];
    hearth_gateway::serve(
        req,
        env,
        &hearth_gateway::GatewayConfig {
            rooms: &ROOMS,
            // `GET /me` and `POST /match` + `GET /lobby` are opt-in.
            player: None,
            matchmaker: false,
            // Login providers come from the worker env; the default is anonymous
            // only. See docs/auth.md for JWT, Steam, itch.io and Discord.
            auth: hearth_gateway::AuthConfig::default(),
            // Per-IP and per-player rate limits; add the [[ratelimits]] bindings
            // in wrangler.toml to turn them on.
            limits: hearth_gateway::ratelimit::Limits::default(),
        },
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
- [Cost estimate](docs/cost.md): model the monthly Cloudflare bill with
  `hearth cost`, and the versioned pricing table behind it.
- [Auth providers](docs/auth.md): JWT, Steam, itch.io and Discord logins, the
  player id namespaces, and the 24-hour session token.
- [Admin and observability](docs/admin.md): `hearth inspect` (list rooms, inspect
  full state, force-dispose, tail patches), the `/admin/*` JSON API, and Workers
  Logs events.
- [Typed RPC](docs/rpc.md): `ctx.call` and durable `ctx.notify` between rooms
  and services, reentrancy, and the internal call path.
- [Matchmaking](docs/matchmaking.md): quick-join, seat reservations, pools, the
  lobby, region hints, room metadata, and private rooms.
- [Ticked rooms](docs/ticked.md): fixed-step 5–20 Hz rooms, snapshot
  interpolation, and shared-logic prediction.
- [Handoff to a dedicated server](docs/handoff.md): single-use tickets, a signed
  webhook allocator for your own fleet, Cloudflare Containers, and the server
  SDKs.
- [v0.1 implementation plan](docs/superpowers/plans/2026-10-25-hearth-v0.1-event-rooms.md)

## Licence

MIT OR Apache-2.0
