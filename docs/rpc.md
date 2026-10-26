# Typed RPC between objects

Hearth v0.3a can call from one Durable Object to another: a room can call a
service, a service can call a room that opts in, and a service can call another
service. Calls are typed, addressed by a Durable Object binding plus an object
name, and come in two forms:

- **`ctx.call`** — send now and wait for the typed reply.
- **`ctx.notify`** — send after the handler commits; durable, retried, applied
  once.

Both travel over an internal path (`POST /__hearth/call`) that a player can
never reach. The callers are Hearth objects and the gateway itself, never
clients.

## 1. Objects, addresses and `Callable`

Every Hearth object (a room, a service) is a Durable Object, so a call names it
by `Target`:

```rust
pub struct Target {
    pub binding: String, // wrangler DO binding, e.g. "LEADERBOARD"
    pub name: String,    // id_from_name name, e.g. "global" or a room name
}
```

The callable side is described at compile time by `Callable`, which both
`#[hearth_mp::service]` and `#[hearth_mp::room(..., rpc)]` generate:

```rust
pub trait Callable {
    const BINDING: &'static str;
    type Rpc: Serialize + DeserializeOwned;
    type Reply: Serialize + DeserializeOwned;
}
```

`Rpc` is the payload type and `Reply` the answer type. The payload is
JSON-shaped (any serde type that survives a `serde_json::Value` round trip);
only the internal envelope is MessagePack. A type that does not decode is an
error on the receiving side, never a panic.

## 2. `ctx.call`: typed request/reply

```rust
let reply = ctx
    .call::<Leaderboard>("global", LeaderboardRpc::Submit { player, score })
    .await?;
```

`ctx.call` is available on `Ctx` (a room handler) and `ServiceCtx` (a service
handler). It sends the call at once and awaits the reply. The receiver's
`on_call` runs as one handler pass, so its state changes, timers, events and
notifies behave as they do for any other handler pass.

**A call is not rolled back.** The call is already sent when `call` returns,
so if your handler then returns `Err`, the receiver keeps the effect and your
state is rolled back. There is no compensation. Design for it: make the
receiver's operation idempotent, or do the call as the last step of the
handler.

**A call is attempted once.** The framework does not retry, because a retry
after a lost reply would run the handler twice. A lost reply is reported as
`Unreachable`; if you retry yourself, the receiver must tolerate duplicates.
`CallError::retryable()` is true only for `Unreachable`.

A failed call is a `CallError`:

| Variant | Meaning |
| --- | --- |
| `ReentrantCall(Target)` | The target is already waiting somewhere in this call path. It answered at once instead of deadlocking; see section 7. |
| `BadRpc(String)` | The payload did not decode into the target's `Rpc`, the reply did not decode into the caller's `Reply`, or the target does not accept calls at all. |
| `Handler(String)` | The target's handler ran and returned `Err`. Its state and buffered effects were rolled back. |
| `Unreachable(String)` | The call reached no handler: unknown binding, invalid object name, a platform/stub error, a non-200 answer, or an undecodable reply. |

The `String` payloads are diagnostics; do not parse them, match on the variant.

A call to an object that is already handling another pass waits for that
object's current turn to finish, because each Durable Object runs one turn at a
time. While your handler awaits the reply, your own object is likewise busy
and cannot start another pass.

## 3. `ctx.notify`: durable fire-after-commit

```rust
ctx.notify::<Leaderboard>(
    "global",
    LeaderboardRpc::Submit { player, score },
);
```

`ctx.notify` is buffered with the handler's other effects. When the handler
returns `Ok`, the notify is written to the durable outbox **in the same commit
as the state save**, so a committed handler can never lose it. A handler that
returns `Err` never sends it.

After the pass, and again on the outbox alarm, pending notifies are delivered:

- Delivery is retried until the target answers, with capped exponential
  backoff (about 2 s doubling, topping out near five minutes). Retries survive
  hibernation.
- Notifies to one target are delivered in send order. A failed delivery blocks
  later notifies to that same target; notifies to other targets carry on.
- The receiver deduplicates on `Envelope.msg_id` in the same commit as its
  state change, so a retried delivery is applied at most once. A duplicate
  delivery answers success without running `on_call` again.
- Rows older than 24 hours are dropped and logged (`"hearth":"outbox_dropped"`).
  Delivery errors are logged (`"hearth":"outbox_error"`) and never fail the
  sending handler.

Use `notify` for work that must happen after the commit: recording a match
result, crediting stats, sending a mailbox message to another object. Do not
use it as a synchronous call — it returns nothing and may be delivered after
your handler, or much later.

A notify to an object that does not accept calls (a room without `rpc`, for
example) is retried until the 24-hour window closes and then dropped.

## 4. Delivery guarantees

| | `ctx.call` | `ctx.notify` |
| --- | --- | --- |
| Shape | typed request and reply, awaited | typed one-way message |
| Wire attempt | one | repeated until the target answers |
| Caller handler fails later | receiver keeps the effect | never sent |
| Duplicates | none from the framework | at-least-once delivery |
| Receiver application | at most once | at most once within the 24-hour window |
| Ordering | only in the order your own awaits impose | ordered per target |
| Errors | returned as `CallError` | logged, then retried; never returned |
| After 24 hours | not applicable | dropped and logged |

## 5. Services

A `Service` is the socket-less sibling of a room. It has the same durable
state, timers and SQLite storage, but no sockets: it is reachable only through
object-to-object calls. Player records, a matchmaker, a leaderboard — anything
that outlives or spans matches — is a service.

```rust
pub trait Service: Default + 'static {
    type State: Serialize + DeserializeOwned + Default;
    type Rpc: Serialize + DeserializeOwned;
    type Reply: Serialize + DeserializeOwned;

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        from: &Target,
        rpc: Self::Rpc,
    ) -> Result<Self::Reply>;

    async fn on_timer(&mut self, ctx: &mut ServiceCtx<Self>, key: TimerKey) -> Result<()> {
        Ok(())
    }
}
```

`on_call` is required; `on_timer` is optional. `from` is the caller's
`Target` (the sending object for a notify), which lets a service check who is
calling. Write the impl block, then annotate it:

```rust
#[hearth_mp::service(binding = "LEADERBOARD")]
impl Service for Leaderboard { /* ... */ }
```

The macro re-emits the impl, adds the `Callable` impl (binding, `Rpc`, `Reply`
taken from the impl's associated types), and generates the Durable Object
class `LeaderboardObject` that wraps the runtime. Bind that class name in
`wrangler.toml` and declare it in a migration — the worked example in section
9 shows both.

Add the `player` flag — `#[hearth_mp::service(binding = "TTT_PLAYER", player)]` —
to make the service a per-player object: it must also implement `PlayerService`,
whose one method `client_view(state) -> serde_json::Value` is the JSON the
gateway returns from `GET /me` for the token's own player id. Players can never
name the target, so `/me` cannot read someone else's record.

What a service handler can do (`ServiceCtx`):

- `state() -> &mut Self::State` — persisted when the handler returns `Ok`.
- `sql() -> &dyn Sql` — the object's own SQLite database.
- `timer(key, after)` / `cancel_timer(key)` — durable timers. A second timer
  with the same key replaces the first.
- `now_ms()` — the object clock.
- `me() -> &Target` — this object's own address.
- `call` and `notify` — exactly as in sections 2 and 3.

The service also has the room's **single Durable Object alarm**: timers are
multiplexed onto it, and it also drives outbox retries. A timer handler that
returns `Err` is logged and rolled back; the timer row is already consumed, so
it is not retried.

The service struct's fields are not durable. It is rebuilt from `Default` on
every wake, so keep durable data in `State` (or SQL, or timers), not in fields.

A service learns its name from the first call's `X-Hearth-Name` header and
persists it. Until then it has no name to put in a reply or report.

`ServiceCtx` has no sockets: no `send`, `broadcast`, `kick`, `lock`, `dispose`
or `set_metadata`. Those are room-only effects; see section 6.

## 6. Rooms that accept calls

A room accepts calls only if it opts in with the `rpc` flag and implements
`RoomRpc`:

```rust
#[hearth_mp::room(mode = "event", max_clients = 2, binding = "TICTACTOE", rpc)]
impl Room for TicTacToe { /* ... */ }

impl RoomRpc for TicTacToe {
    type Rpc = TttRpc;
    type Reply = serde_json::Value;

    async fn on_call(
        &mut self,
        ctx: &mut Ctx<Self>,
        from: &hearth_mp::Target,
        rpc: TttRpc,
    ) -> Result<Self::Reply> {
        // ...
    }
}
```

`on_call` runs as a normal handler pass: whatever it changes in the state is
patched to the room's clients, and its events and timers are applied on
commit. A failed `on_call` rolls the state back. This is the way a service can
reach a live match (kick a player, start a round, read the board).

A room without the `rpc` flag answers `BadRpc` ("room does not accept rpc") to
every call; it can still be a caller.

`Ctx` is the room's handler context. It has everything `ServiceCtx` has, plus
the room-only effects: `send`/`broadcast` events, `kick`, `lock`, `dispose`,
`clients()` and `set_metadata(value)` (the lobby metadata; see
[matchmaking.md](matchmaking.md)).

| Capability | `Ctx` (room) | `ServiceCtx` (service) |
| --- | --- | --- |
| `state`, `sql`, timers, `now_ms`, `me` | yes | yes |
| `call`, `notify` | yes | yes |
| player roster, `send`/`broadcast`, `kick`, `lock`, `dispose` | yes | no |
| `set_metadata` | yes | no |

## 7. Reentrancy

Each Durable Object runs one turn at a time. If object A calls B, B calls A,
and A is still awaiting B, then A cannot take the second call: not while it is
inside the first handler. The call chain detects this instead of hanging.

Every call carries `chain`, the list of objects currently awaiting on this
path, outermost first. Before taking its runtime lock, a receiver checks
whether it is already in the chain and, if so, answers
`CallError::ReentrantCall(target)` at once:

```text
A ──call──▶ B            chain: [A]
B ──call──▶ A            chain: [A, B]  → A answers ReentrantCall(A)
```

A call to yourself is reentrant by definition. The inner caller sees
`ReentrantCall`; if its handler propagates that error, the next caller out sees
`Handler(...)` whose message contains the reentrancy diagnostic.

The chain is per call path, not global: two independent callers into the same
object do not see each other's chains; they simply queue for the object's next
turn.

## 8. The internal wire path

Calls are Hearth-internal. `POST /__hearth/call` is never a public route:

1. The caller's runtime builds an `Envelope`:
   `{ msg_id, from, chain, rpc }`. `msg_id` is `None` for a call and
   `Some(id)` for a notify. `from` is the caller's `Target`, `chain` is the
   incoming chain plus the caller, and `rpc` is the payload as a
   JSON-shaped `Value`. Hand-written calls (the gateway's) use the same
   envelope.
2. It resolves the target with
   `env.durable_object(binding).id_from_name(name)`, encodes the envelope as
   MessagePack, and POSTs it to the stub at `https://hearth/__hearth/call`
   with headers `X-Hearth-Name: <target name>` and
   `Content-Type: application/msgpack`.
3. The macro-generated Durable Object wrapper routes `/__hearth/call` to
   `RoomObject::call` (rooms with `rpc` only) or `ServiceObject`. The receiver
   requires POST and the `X-Hearth-Name` header, decodes the envelope, checks
   the chain **before** taking its runtime mutex, then runs the runtime.
4. The runtime decodes `rpc` into `T::Rpc`, runs `on_call`, encodes the reply
   as a JSON-shaped `Value`, and returns it as a MessagePack
   `Result<Value, CallError>` with a 200 status.
5. The caller decodes the reply into `T::Reply`. A non-200 answer or an
   undecodable reply is `Unreachable`.

Who sets the header: the calling Hearth object's `Host::call` (rooms and
services) and the gateway's `call_object` helper. No public route forwards a
player request to this path, so a player can never choose the target name, the
chain or the payload. The public gateway parses only its fixed routes and
404s everything else; `/__hearth/*` exists only on the Durable Object classes,
which are reached through DO bindings.

## 9. Worked example: a leaderboard service

A service in the game Worker crate. `GameState`, `Move` and the score helpers
are the game's own code and are abridged here.

```rust
use hearth_mp::{PlayerId, Result, Service, ServiceCtx, Target};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct Score {
    pub player: PlayerId,
    pub score: u32,
}

#[derive(Serialize, Deserialize, Default)]
pub struct LeaderboardState {
    scores: Vec<Score>,
}

#[derive(Serialize, Deserialize)]
pub enum LeaderboardRpc {
    /// Record `score` if it beats the player's best; return the top 10.
    Submit { player: PlayerId, score: u32 },
    /// Return the top `limit` scores.
    Top { limit: usize },
}

#[derive(Default)]
pub struct Leaderboard;

#[hearth_mp::service(binding = "LEADERBOARD")]
impl Service for Leaderboard {
    type State = LeaderboardState;
    type Rpc = LeaderboardRpc;
    type Reply = Vec<Score>;

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &Target,
        rpc: LeaderboardRpc,
    ) -> Result<Self::Reply> {
        let limit = match rpc {
            LeaderboardRpc::Submit { player, score } => {
                let scores = &mut ctx.state().scores;
                let index = scores.iter().position(|entry| entry.player == player);
                match index {
                    Some(index) => scores[index].score = scores[index].score.max(score),
                    None => scores.push(Score { player, score }),
                }
                scores.sort_by(|a, b| b.score.cmp(&a.score));
                scores.truncate(100);
                10
            }
            LeaderboardRpc::Top { limit } => limit,
        };
        Ok(ctx.state().scores.iter().take(limit).cloned().collect())
    }
}
```

A room calls it. The reply is typed; the call is sent immediately and is not
rolled back if the handler later fails. The final score is sent with
`notify`, so it survives a failure after that point.

```rust
use hearth_mp::{Client, Ctx, Leave, Result, Room};

#[hearth_mp::room(mode = "event", max_clients = 2, binding = "GAME")]
impl Room for Game {
    type State = GameState;
    type Intent = Move;

    async fn on_intent(
        &mut self,
        ctx: &mut Ctx<Self>,
        who: &Client,
        intent: Move,
    ) -> Result<()> {
        let Move::Place { cell } = intent;
        // ... validate and apply the move (game code) ...
        let score = score_of(cell); // the game's rule, not a Hearth API

        let top = ctx
            .call::<Leaderboard>(
                "global",
                LeaderboardRpc::Submit {
                    player: who.player_id.clone(),
                    score,
                },
            )
            .await?;
        ctx.broadcast("leaderboard", &top);
        Ok(())
    }

    async fn on_leave(
        &mut self,
        ctx: &mut Ctx<Self>,
        who: &Client,
        _reason: Leave,
    ) -> Result<()> {
        // Written with this handler's state commit, retried until delivered,
        // applied once: the right place for a final score.
        ctx.notify::<Leaderboard>(
            "global",
            LeaderboardRpc::Submit {
                player: who.player_id.clone(),
                score: final_score(&who.player_id),
            },
        );
        Ok(())
    }

    // on_join is required; the room's state derive and intent schema are
    // omitted here (see the README for a complete room).
}
```

`Leaderboard` is a single Durable Object named `global`; its address is
`Target { binding: "LEADERBOARD", name: "global" }`. Any name works — the
matchmaker uses `"{room_type}:{region}"`, player objects use the player id.

Declare the class in `wrangler.toml` and add a migration:

```toml
[[durable_objects.bindings]]
name = "LEADERBOARD"
class_name = "LeaderboardObject"

# New Durable Object classes need a new migration tag. Use the next tag in
# your project; the sample uses v1-v3 for its room, registry and player classes.
[[migrations]]
tag = "v4"
new_sqlite_classes = ["LeaderboardObject"]
```

The class must be SQLite-backed (`new_sqlite_classes`): service state, timers
and the outbox live in SQLite storage. `#[hearth_mp::service]` generates the
`LeaderboardObject` class name from the type name; `#[hearth_mp::room]` does the
same for rooms (`TicTacToe` gives `TicTacToeObject`).

## Further reading

- [Matchmaking](matchmaking.md): `ctx.set_metadata`, quick-join, the lobby.
- [Wire protocol v1](protocol.md): the player-facing protocol and its own
  request/reply frames. RPC is separate and internal.
