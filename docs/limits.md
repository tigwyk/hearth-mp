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

## Numbers in JSON mode

JSON parses every number as a float64 in JavaScript and GDScript, so an `i64`
state value above 2^53 (9,007,199,254,740,992) loses precision on those clients.
Keep large identifiers as strings — `PlayerId` and the schema hash are strings
for exactly this reason — or use a MsgPack client (the native Godot extension,
Rust) which carries `i64` exactly. Do not rely on exact large integers in state
if a JSON or GDScript client can see them.
