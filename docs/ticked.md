# Ticked rooms

A ticked room runs a fixed-step simulation at 5–20 Hz while at least one client
is connected, and stops when the last one leaves. The room is the authority;
clients interpolate between snapshots, and a Godot client can also predict its
own avatar from the same logic code. A room picks its mode at compile time.

## 1. When to choose ticked mode

| Mode | How it runs | Fits | Cost |
| --- | --- | --- | --- |
| Event (default) | Wakes per message or alarm, hibernates between | Turn-based, card/board, party games, async, lobbies, social hubs | Billed per message; idle rooms near free |
| Ticked | Fixed tick at 5–20 Hz while players are connected, stops when the room empties | Co-op, top-down action, .io-style, MMO-lite zones with tolerant netcode | Billed wall-clock while ticking |
| Handoff | Room coordinates, then issues a ticket to a dedicated server | Shooters, fighting games, physics-heavy action | Hearth bills only the coordination part |

The spec's [Target games](../Hearth%20a%20Rust%20framework%20for%20Durable%20Object%20game%20servers.md#target-games)
table is the long version. The trade-off in one line: a ticked room is awake
for the whole session and pays for it; an event room hibernates between messages
and is near-free when nobody is playing.

Choose ticked mode when the game needs continuous motion at a steady rate
(input every tick, positions that must be applied on a common clock). Stay in
event mode for anything that only reacts to player input. Above ~20 Hz, or for
UDP, use a dedicated server (Handoff, v0.5) — 20 Hz is the framework's current
hard ceiling (provisional, see section 3).

## 2. The tick contract

A ticked room declares its rate and persist interval on the room attribute, and
implements `on_tick`:

```rust
use std::time::Duration;
use hearth::{Ctx, Result, Room};

#[hearth::room(mode = "ticked", tick_hz = 20, persist_every = 20,
               max_clients = 16, binding = "ARENA")]
impl Room for ArenaRoom {
    type State = Arena;
    type Intent = ArenaIntent;

    async fn on_tick(&mut self, ctx: &mut Ctx<Self>, dt: Duration) -> Result<()> {
        // `dt` is always 1 s / tick_hz, including catch-up ticks.
        Ok(())
    }
}
```

`on_tick` is optional (it defaults to a no-op), but a room that never changes
state on a tick is paying for nothing. `ctx.tick()` returns the tick number the
handler is running for (`0` in event mode).

The rules:

- **Ticking is on while at least one client is *connected*.** Held seats (a
  dropped player inside the reconnect grace) and admin tail sockets do not
  count. The first join starts the schedule (first tick one period later); the
  last leave, drop or kick stops it after running any buffered intents and
  persisting once.
- **Intents are buffered.** A valid `INTENT` is decoded and rate-limited
  immediately, then queued and applied at the start of the next tick, in
  arrival order, before that tick's `on_tick`. A frame that does not decode
  still gets an immediate `BAD_INTENT`; a rate-limited batch is handled as in
  [limits.md](limits.md). Intents from a player who has already left are
  dropped. The `ACK` for buffered intents is sent by the tick that applies
  them and carries its `tick`.
- **Each due step runs in order:** `tick += 1`, then the buffered intents (each
  with its own rollback on error), then `on_tick(dt)`. A failed `on_tick` rolls
  back that step's changes, logs `handler_error`, and the schedule continues.
  One wake can run up to three steps but still emits a single patch.
- **The timestep is fixed.** `dt` is `1 s / tick_hz` (floored to whole
  microseconds; exact at 5, 10 and 20 Hz) for the life of the room, so the
  server and a predicting client step the same amount.
- **Catch-up is capped at 3 ticks.** If a wake arrives late, it runs at most
  three steps, then the schedule continues; the skipped slots are gone (the
  schedule is absolute, not a backlog). More than three due periods logs
  `{"hearth":"tick_dropped","n":N}`. After a 2 s stall a 20 Hz room therefore
  runs 3 ticks, not 40.
- **One PATCH per wake.** A wake emits one patch per client after its steps,
  carrying `tick` (the newest step) and `t` (the server clock in ms). During
  catch-up the `tick` in consecutive patches can advance by more than one.
  `ACK`s for intents applied across those steps are aggregated into one frame
  per sender.
- **Overruns are logged.** When a wake's steps take longer than one period of
  wall time, the room logs `{"hearth":"tick_overrun","ms":N}`.
- **Timers and the outbox keep working.** Each room has one Durable Object
  alarm; `next_wake()` is the earliest of the next tick and the next timer (the
  outbox retry is one of those timer rows). A ticked room that stops ticking
  (nobody connected) still wakes for its timers.

Event rooms are unchanged: no buffering, no `tick`/`t`, one handler pass per
incoming message.

## 3. The 5–20 Hz range and the provisional driver decision

`tick_hz` is required in ticked mode and must be in `5..=20`; `persist_every`
defaults to 20 and must be at least 1. Mistakes are compile errors:

| Mistake | Message |
| --- | --- |
| `mode = "ticked"` without `tick_hz` | `mode = "ticked" needs tick_hz = N (5..=20)` |
| `tick_hz = 40` (or below 5) | `tick_hz must be 5..=20; above 20 Hz use a dedicated server (Handoff, v0.5)` |
| `tick_hz`/`persist_every` on an event room | `tick_hz needs mode = "ticked"` / `persist_every needs mode = "ticked"` |
| `persist_every = 0` | `persist_every must be at least 1` |
| `mode = "handoff"` | `handoff rooms arrive in Hearth v0.5` |

The scheduler works in whole milliseconds, so rates that do not divide 1000
(e.g. 15 Hz) schedule ticks every 66 ms while still handing `on_tick` the same
fixed `dt` (66.666 ms, floored to whole microseconds). 5, 10 and 20 Hz are
exact.

**The driver choice is provisional.** [ADR 0001](adr/0001-tick-driver.md) picks
an **alarm driver** — the tick is one alarm wake per period, so the room can
hibernate between ticks and shares its single alarm with timers and the outbox —
and advertises a **20 Hz ceiling**. That decision comes from local-only
measurements on `wrangler dev`, not from Cloudflare: local workerd is dominated
by the host timer quantum, so it cannot settle the deployed latency question.
The deployed benchmark and the deployed soak run that could lower the ceiling or
flip the driver have not been executed yet (only local runs exist). If the
deployed numbers show the alarm driver missing the bar, ADR 0001 flips to a
held-open loop driver; the runtime is driver-agnostic (`next_wake()` plus
`tick_if_due()`), so room code does not change.

## 4. Persistence and the crash window

Ticked rooms persist the **state and the tick number** together, every
`persist_every` ticks (default 20: about once a second at 20 Hz), and also when
ticking stops and before an admin inspect. On the next wake the room continues
from the stored tick number.

The cost of that interval is the crash window: an eviction or crash loses at
most the ticks since the last persist — up to `persist_every - 1` complete
ticks — and always restores a consistent state at a persisted tick. The server
never replays intents after a restart. Intents that were buffered in memory when
the room was evicted are lost; the client already sees every
sent-but-unacknowledged intent as `IntentsUnconfirmed` when the socket drops,
and must never resend them silently.

Event rooms keep their v0.1 rule (persist on every handler pass), so this
section only applies to ticked rooms.

## 5. What the protocol carries

Ticked rooms add three optional fields; event rooms omit them and decoders must
already ignore unknown fields:

- `PATCH.tick` — the tick the patch is the result of.
- `PATCH.t` — server wall-clock milliseconds for that tick. This is the clock
  sync input for interpolation.
- `ACK.tick` — the tick that applied the acknowledged intents.

`PATCH.seq` only advances when the room persists, so consecutive ticked patches
repeat it. Order and dedupe patches by `tick`, and apply any patch that follows
a `FULL_STATE` (a disposed and recreated room restarts its tick count). A room
that never persists would otherwise let a client drop every patch after the
first.

`FULL_STATE` carries no tick; a client's clock starts at the first ticked
`PATCH`. See [protocol.md](protocol.md) for the frame shapes rather than
repeating them here.

## 6. Interpolation

A ticked server sends snapshots at 5–20 Hz; rendering them directly looks
choppy. The client-side interpolator keeps a short snapshot history and renders
slightly in the past, so motion is smooth without guessing the future.

How it behaves:

- Each ticked `PATCH` records a snapshot of every tracked path, stamped with
  `t`. Snapshots that are not newer than the newest kept are dropped
  (out-of-order and duplicate frames).
- The client renders at **two tick periods behind** the synchronised server
  clock (100 ms at 20 Hz), so there is always a newer snapshot to interpolate
  towards.
- Between two snapshots, numbers and equal-length arrays of numbers are lerped;
  any other value (strings, objects, unequal arrays) steps to the older
  snapshot's value. Before the oldest snapshot the oldest value is returned; a
  single snapshot is held.
- Past the newest snapshot, the value is extrapolated for **at most one period**
  and then held. It never runs further ahead.
- Snapshots are kept for a one-second window, but the two newest are always
  kept, so a two-second stall still lerps across the gap instead of freezing.
- The clock estimate is the largest `(server_t - local_receive_ms)` over the
  last 32 patches — the least-delayed arrival wins. A jump of more than one
  second clears the window (a server clock jump, or a long disconnect).
- A `FULL_STATE` (join/reconnect) clears the snapshot history, because the old
  state's snapshots cannot be interpolated into the new one; tracked paths
  survive.
- If a path is missing from either bracketing snapshot (or from the newest one
  when extrapolating), the sample is missing: `null` in `sample`, `Vector2.ZERO`
  in `sample_v2`.

### Godot

```gdscript
var avatars := Hearth.interpolator([["avatars", "*", "pos"]])

func _process(_delta: float) -> void:
    $Self.position = avatars.sample_v2(["avatars", Hearth.player_id, "pos"])
    # Any other avatar: avatars.sample_v2(["avatars", other_player_id, "pos"])
```

`Hearth.interpolator(paths)` returns the native `HearthInterp` on desktop builds
and a `hearth_interp.gd` instance on web; both expose `sample(path)` and
`sample_v2(path)` and are fed automatically from every ticked `PATCH`. A number
comes back as a `float`, a two-number array as a `Vector2`, other values in
their wire shape; `sample_v2` returns `Vector2.ZERO` when the path is missing.
Paths may contain a `"*"` segment, which tracks every key or list index at that
level.

The wrappers assume a 20 Hz room: the wire protocol does not carry `tick_hz`,
so the render delay is two periods of 20 Hz (100 ms). At lower tick rates
interpolation still works; only the delay is smaller than ideal.

### Unity

```csharp
var interp = room.Interpolator(new[] { new[] { "avatars", "*", "pos" } });
Vector2 pos = interp.SampleVector2("avatars", playerId, "pos");
```

`HearthInterpolator` exposes `Sample`, `SampleVector2` and `SampleFloat`, is fed
from every `OnPatched` with the patch's `t`, and implements `IDisposable`.
v0.4 ships interpolation for Unity; prediction is the next plan (v0.4b).

## 7. Prediction (Godot)

Prediction is optional and only Godot supports it in v0.4. The server stays the
authority: the client predicts locally to hide its own input latency and then
reconciles to the server when an authoritative state arrives.

The recipe:

1. **One shared logic crate.** `examples/arena-logic` is plain Rust with no
   Hearth types: `step(&mut state, inputs, dt)` mutates a state whose shape
   mirrors the room's synced view (player ids as strings, so it deserialises
   straight from the client view). The room's `on_tick` and the client
   predictor call the same function.
2. **`Logic` and `Predictor`** live in `hearth-client`:
   - `Logic` has `type State`, `type Input`, and
     `step(state, inputs, dt)` — the deterministic simulation.
   - `Predictor::new(me, tick_hz)`; `input(seq, input)` records the input and
     steps the predicted state one tick immediately; `on_authoritative(state,
     acked)` drops acked inputs, rebuilds the predicted state from the server
     view, and replays the remaining inputs in order; `on_unconfirmed(seqs)`
     drops inputs the server may never have applied; `predicted()` returns the
     predicted state.
3. **One input per tick.** Each tick the game sends one `Move` intent and calls
   `predictor.input(seq, input)` with the same input and the sequence number
   `Hearth.send_intent` returned, so the local step and the server step match.
   The predictor's input is the logic crate's payload — for the arena,
   `predictor.input(seq, {"dir": [1, 0]})`, not the `{"Move": ...}` intent
   wrapper. The server floors its step to whole microseconds while the
   predictor derives it from `tick_hz` as an `f64`; use 5, 10 or 20 Hz when you
   need them bit-identical (everything else is reconciled on the next ACK).
4. **Reconcile on every patch.** A `PATCH` updates the authoritative state; the
   latest `ACK` says which inputs the server applied. Replaying only the
   unacknowledged inputs keeps the prediction close, and the server always wins
   when the two disagree.
5. **Ship a custom GDExtension.** The stock `hearth-godot` addon does not
   contain the game's logic, so `examples/arena-godot-ext` builds a cdylib that
   depends on `hearth-godot` and `arena-logic` and calls
   `hearth_godot::predictor!(ArenaPredictor, arena_logic::Arena)`. The macro
   generates the Godot class (`input(seq, input)`, `predicted_state()` and
   `attach(hearth_node)`, which connects the `joined`, `state_reset`, `patched`,
   `acked` and `intents_unconfirmed` signals). The extension crate registers its
   own `#[gdextension]` init and declares `extern crate hearth_godot;`, so the
   stock classes are linked in too. The project's `.gdextension` replaces the
   stock addon's library with this one, same entry symbol, so `HearthNative` is
   still registered.
6. **Render.** Draw the local avatar from `predicted_state()` and other avatars
   from the interpolator. When the drawn predicted position and the newest
   authoritative position differ by more than 0.01, smooth the drawn position
   toward the corrected prediction at 15% per frame; snap when the difference is
   above 2 units. `HearthNative.debug_latency_ms` delays sends and receives for
   testing.

Like the interpolator, the generated class assumes a 20 Hz room
(`PREDICT_TICK_HZ`); the wire protocol does not carry `tick_hz`.

Determinism rules for the shared logic: use only `+`, `-`, `*`, `/`, `sqrt` and
comparisons on `f32`; no transcendental functions (`sin`, `exp`, …), no platform
intrinsics, and no iteration over hash-ordered collections (use a `BTreeMap` or a
sorted list). The simulation must produce the same state from the same inputs
and `dt`. Mispredictions are reconciled anyway, but a deterministic `step` keeps
them rare.

The server also clamps what it accepts: the arena's `Move` clamps each `dir`
component to `-1..=1`, so a client sending `{"dir":[5,0]}` moves exactly like
`{"dir":[1,0]}`. Prediction cannot cheat the simulation.

Not in v0.4: rollback, lag compensation, Unity prediction. Do not build
gameplay that depends on the client's predicted state being authoritative.

## 8. Cost of a ticked room

Ticked mode is billed differently from event mode: the room does not hibernate
until the last client leaves, so duration is billed for the whole session, and
every tick is an alarm invocation. `hearth cost` models it:

```sh
hearth cost --ccu 1000 --room-size 16 --intents 10 --ticked 20 --persist-every 20
```

`--ticked <hz>` models one alarm request per tick, `active_fraction = 1` (never
hibernating) and one state row written per `--persist-every` ticks, on top of
the incoming-message and connection lines. The full model and the pricing table
are in [cost.md](cost.md). Tick rate is the main lever: halving `tick_hz`
halves the tick requests, and raising `persist_every` raises the crash window
while lowering rows written.

The cost numbers assume the alarm driver from ADR 0001. If the deployed
benchmark flips the driver to a held-open loop, the tick billing shape changes
(current numbers are provisional for the same reason as the 20 Hz ceiling).

## 9. The arena sample

`examples/arena` is a 20 Hz, 16-player top-down arena; `examples/arena-logic` is
its shared deterministic simulation. The room buffers `Move` intents, applies
them in `on_tick` through `arena_logic::step`, and the Godot sample
(`examples/arena-godot`) uses interpolation plus the prediction recipe above.

Run the Worker and the soak test locally:

```sh
cd examples/arena
# Copy .dev.vars.example to .dev.vars first: HEARTH_SECRET + HEARTH_ADMIN_TOKEN.
npx wrangler dev
node soak.mjs --url http://127.0.0.1:8787 --clients 8 --secs 60
```

`soak.mjs` checks the ticked contract against a real server: every `PATCH`
carries `tick` and `t`, ticks advance one at a time (a larger gap is at most the
3-tick catch-up cap), all clients end within one tick of each other, the
simulation moved, and after every client leaves the room **stops ticking** —
the admin inspect's `tick` does not change over 10 s. The stop-ticking phase
needs `HEARTH_ADMIN_TOKEN`. Local workerd is not Cloudflare's scheduler, so the
p99 interval bar is only enforced when you pass `--bar-ms <ms>`; the structural
checks always run.

## Further reading

- [Wire protocol v1](protocol.md): frames, patch ops, the optional `tick`/`t`.
- [Limits](limits.md): the event/ticked/handoff cost profile and hard limits.
- [Cost estimate](cost.md): the model behind `hearth cost --ticked`.
- [ADR 0001](adr/0001-tick-driver.md): the provisional driver decision and its
  measurements.
