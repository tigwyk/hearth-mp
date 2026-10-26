# Hearth wire protocol, version 1

Status: **frozen** (Hearth 1.0). This document is normative. The key words
MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are to be read as described in
RFC 2119. Where this text and a file under `conformance/vectors/` disagree,
the vector file wins and this text is the bug.

It is written so an SDK can be implemented in any language (Defold, Bevy, the
web, an engine plugin) without reading the Rust code. The reference
implementation is the `hearth-protocol` and `hearth-client` crates, and
`hearth-conformance-server` is a local server to test against (see
`conformance/README.md`).

## 1. Transport

- The transport is a WebSocket. The HTTP gateway (section 13) authenticates the
  upgrade and hands the socket to one room; the room owns it from then on.
- A **binary** WebSocket message carries a MessagePack frame; a **text** message
  carries a JSON frame (section 2).
- The client selects the socket's encoding with its first frame, `HELLO`: a
  binary `HELLO` makes the socket MessagePack, a text `HELLO` makes it JSON.
  Both sides MUST use that encoding for every later frame on that socket.
- **Frame size.** The server MUST reject a frame from a client larger than
  **65536 bytes** before decoding it, and closes the socket with 4006. Frames
  from the server (`FULL_STATE` above all) have no protocol cap: a client MUST
  NOT apply the 65536-byte limit to them.
- **Keepalive.** A client MAY send the text message `hearth:ping`; the server
  answers with the text message `hearth:pong` without waking the room. These two
  messages are not frames: they MUST NOT be parsed as JSON, and they are valid
  on MessagePack sockets too. No other text message is valid on a MessagePack
  socket.

## 2. Encoding

- Every frame is an object with a string field `op`, the frame name in
  `SCREAMING_SNAKE_CASE` (`{"op": "PATCH", ...}`).
- **MessagePack:** the message is one opcode byte (section 3) followed by the
  MessagePack encoding of the frame object, with fields encoded **by name**
  (a map, never the compact array form). The opcode byte MUST agree with the
  body's `op`; a mismatch is a protocol error.
- **JSON:** the message is the frame object as UTF-8 JSON text.
- State values and every payload are JSON data types only: `null`, boolean,
  number, string, array, object. Object keys are always strings. Integers above
  2^53 lose precision in JSON and GDScript, so identifiers are strings, never
  numbers: `player_id` is a string and the schema hash is 16 lowercase hex
  characters.
- In this document `u8`/`u16`/`u32`/`u64` are non-negative integers (MessagePack
  integers, JSON numbers), `any` is any JSON value and `T|null` is nullable.

`conformance/vectors/frames.json` gives the exact MessagePack bytes for every
frame shape. An SDK MUST encode and decode each case byte for byte.

## 3. Opcode table

| Name | Byte | Direction | Purpose |
| --- | --- | --- | --- |
| `HELLO` | `0x01` | client → server | handshake; selects the encoding |
| `JOIN_OK` | `0x02` | server → client | join accepted; seat assigned |
| `FULL_STATE` | `0x03` | server → client | complete state snapshot |
| `PATCH` | `0x04` | server → client | incremental state change |
| `INTENT` | `0x05` | client → server | one or more player intents |
| `EVENT` | `0x06` | server → client | out-of-band event |
| `ACK` | `0x07` | server → client | intent acknowledgement |
| `PING` | `0x08` | reserved | never sent; keepalive is the text message in section 1 |
| `ERROR` | `0x09` | server → client | error, optionally tied to an intent |
| `LEAVE` | `0x0A` | client → server | voluntary leave |

## 4. Frames

Fields are **required** unless marked optional. A receiver MUST ignore object
fields it does not know (section 12).

### `HELLO` (client → server)

```json
{ "op": "HELLO", "protocol": 1, "schema_hash": "0123456789abcdef", "opts": {} }
```

| Field | Type | |
| --- | --- | --- |
| `protocol` | `u16` | required; `1` |
| `schema_hash` | string | required; the client build's schema hash (section 5) |
| `opts` | `any` | required; passed to the room's `on_join` hook; `{}` when there is nothing to pass |

### `JOIN_OK` (server → client)

```json
{ "op": "JOIN_OK", "player_id": "anon_2f8c...", "seat": 0 }
```

| Field | Type | |
| --- | --- | --- |
| `player_id` | string | required; the verified player identity |
| `seat` | `u8` | required; `0 ≤ seat < max_clients` |

### `FULL_STATE` (server → client)

```json
{ "op": "FULL_STATE", "seq": 12, "state": { "turn": "anon_a" } }
```

| Field | Type | |
| --- | --- | --- |
| `seq` | `u64` | required; the state sequence number of this snapshot |
| `state` | `any` | required; this viewer's complete state (section 4.1) |

A client MUST replace its whole state tree with `state`.

### `PATCH` (server → client)

```json
{ "op": "PATCH", "seq": 13, "ops": [ { "op": "set", "path": ["turn"], "value": "anon_b" } ] }
```

| Field | Type | |
| --- | --- | --- |
| `seq` | `u64` | required; the state sequence number after the ops |
| `ops` | array of patch ops | required; applied in order (section 7) |
| `tick` | `u64` | optional; ticked rooms only: the tick this patch results from |
| `t` | `u64` | optional; ticked rooms only: server wall-clock ms at that tick |

Ordering:

- **Event rooms** omit `tick` and `t`. A client MUST ignore a `PATCH` whose
  `seq` is not greater than the last applied `seq`.
- **Ticked rooms** send `tick` and `t`. `seq` only advances when the room
  persists, so consecutive patches repeat it. A client MUST order by `tick`:
  ignore a patch whose `tick` is not greater than the last applied tick, and
  apply the first ticked patch after a `FULL_STATE` whatever its tick.
- A patch op that fails (section 7) is a **desync**: the client MUST stop
  applying patches and wait for the next `FULL_STATE`.

### `INTENT` (client → server)

```json
{ "op": "INTENT", "intents": [ { "seq": 1, "intent": { "Place": { "cell": 4 } } } ] }
```

| Field | Type | |
| --- | --- | --- |
| `intents` | array of `{ "seq": u32, "intent": any }` | required; at least one, in application order |

`intent` is the room's intent value in serde's external tagging
(`spec/schema-v1.md` section 5). Section 6 covers sequencing and batching.

### `EVENT` (server → client)

```json
{ "op": "EVENT", "name": "shout", "data": { "text": "hi" } }
```

| Field | Type | |
| --- | --- | --- |
| `name` | string | required; names starting with `hearth.` are reserved (section 10) |
| `data` | `any` | required |

Events are not state. They arrive after the `PATCH` of the same handler, in the
order the room emitted them.

### `ACK` (server → client)

```json
{ "op": "ACK", "state_seq": 14, "intent_seqs": [1, 2] }
```

| Field | Type | |
| --- | --- | --- |
| `state_seq` | `u64` | required; the state sequence number after the accepted intents |
| `intent_seqs` | array of `u32` | required; the accepted intents |
| `tick` | `u64` | optional; ticked rooms only: the tick that applied them |

### `ERROR` (server → client)

```json
{ "op": "ERROR", "code": "BAD_INTENT", "message": "intent did not decode", "ref_seq": 3 }
```

| Field | Type | |
| --- | --- | --- |
| `code` | string | required; section 8 |
| `message` | string | required; for humans; clients MUST NOT parse it |
| `ref_seq` | `u32|null` | required (may be `null`); the rejected intent, or `null` when the error is not about one intent |

### `LEAVE` (client → server)

```json
{ "op": "LEAVE" }
```

The client leaves voluntarily; the room's `on_leave` runs with reason `Left`.
Closing the socket instead is equivalent.

### 4.1 The state view

The state a client sees is the room's synced fields only. Fields marked
owner-only (`spec/schema-v1.md`, `vis: "owner"`) carry only the viewer's own
entry. The shape is described by the room's `SchemaDoc`.

## 5. Handshake and the schema hash

1. The client gets a token and a room name from the gateway (section 13) and
   opens `GET /rooms/<type>/<name>?token=<token>` as a WebSocket.
2. The client MUST send `HELLO` as its first frame.
3. The server checks `protocol` (else `ERROR UNSUPPORTED_PROTOCOL`, close 4000)
   and `schema_hash` (else `ERROR SCHEMA_MISMATCH`, whose message names both
   hashes, close 4001), runs `on_join` (an error there is `JOIN_REJECTED`, close
   4010; a full room is `ROOM_FULL`, close 4002), then sends `JOIN_OK` followed
   by `FULL_STATE`.
4. Any other frame before a successful `HELLO` is answered with `NOT_JOINED`.

The schema hash covers the room's state type **and** its intent type, so a stale
client fails at the handshake instead of desyncing. It is opaque: SDKs MUST
compare it only for equality and MUST NOT derive it themselves. Get it from the
generated code (`hearth codegen`) or from `GET /schema/<type>`.

A second socket for the same player replaces the first: the old socket is closed
with 4009, the seat is kept, `on_join`/`on_leave` do not run again, and the new
socket receives `JOIN_OK` with the same seat and a fresh `FULL_STATE`.

## 6. Intents, batching and `ACK`

- The client assigns intent sequence numbers per socket session, starting at
  **1** and increasing by exactly 1 per queued intent.
- A client SHOULD batch queued intents into one `INTENT` frame and SHOULD cap
  its sending rate. The defaults are at most **32 intents per frame** and at
  most **20 frames per second**; incoming messages are what the platform bills.
- **Event rooms** run each intent at once. Accepted intents are answered with
  one `ACK` (after the `PATCH` they caused) and each rejected one with `ERROR`
  carrying its `ref_seq` (`BAD_INTENT` when it did not decode, `INTENT_REJECTED`
  when the room refused it). A rejected intent leaves no state change.
- **Ticked rooms** buffer intents until the next tick. The `ACK` for them carries
  that `tick`; a predicting client replays its unacknowledged inputs from there.
- When the socket drops, intents that were sent but neither acknowledged nor
  rejected have an unknown fate. A client MUST NOT resend them silently; it
  SHOULD report them to the application. Queued, unsent intents MAY be sent
  after the next `JOIN_OK`.
- State sequence numbers are per room, `u64`, and increase by one per state
  flush that changed anything.

## 7. Patch operations

A patch op is an object with an `op` field in `snake_case`:

| `op` | Fields | Effect |
| --- | --- | --- |
| `set` | `path`, `value` | replace the value at `path` |
| `del` | `path` | remove the object key or array index at `path` |
| `map_insert` | `path`, `key`, `value` | insert or replace string `key` in the object at `path` |
| `list_splice` | `path`, `index`, `delete`, `insert` | splice `insert` into the array at `path` |

`path` is an array of segments: a string selects an object key, an integer an
array index. `[]` is the root.

- `set`: `[]` replaces the root. Otherwise the parent MUST exist; a missing
  object key is created; an array index MUST already exist.
- `del`: the parent MUST exist and the final key or index MUST be present.
- `map_insert`: `path` MUST resolve to an object.
- `list_splice`: `path` MUST resolve to an array, with `index ≤ length` and
  `index + delete ≤ length`. `delete` values are removed at `index`, then the
  `insert` values are inserted there.
- Ops apply in order; each sees the result of the previous one.
- Anything else is a bad path, which is a desync (section 4, `PATCH`).

`conformance/vectors/patch.json` is normative for these semantics.

## 8. Error codes

| Code | Meaning |
| --- | --- |
| `BAD_FRAME` | the frame could not be decoded |
| `UNSUPPORTED_PROTOCOL` | `HELLO.protocol` is not supported |
| `SCHEMA_MISMATCH` | the schema hash differs from the room's |
| `ROOM_FULL` | no seat left |
| `ROOM_LOCKED` | the room refuses new players |
| `NOT_JOINED` | a frame arrived before a successful `HELLO` |
| `BAD_INTENT` | the intent did not decode into the room's intent type |
| `INTENT_REJECTED` | the room refused the intent; no state change was kept |
| `JOIN_REJECTED` | the room's `on_join` refused the player |
| `RATE_LIMITED` | the per-player intent rate cap was exceeded |
| `FRAME_TOO_LARGE` | an incoming frame exceeded 64 KiB |
| `STATE_INCOMPATIBLE` | the persisted room state cannot be read by this build |
| `INTERNAL` | an unexpected server error |

New codes MAY be added within v1. A client MUST treat a code it does not know as
a generic error (section 12).

## 9. Close codes

| Code | Name | Meaning |
| --- | --- | --- |
| 4000 | `BAD_FRAME` | bad frame or unsupported protocol version |
| 4001 | `SCHEMA_MISMATCH` | schema hash mismatch |
| 4002 | `ROOM_FULL` | room full |
| 4003 | `ROOM_LOCKED` | room locked |
| 4004 | `KICKED` | kicked by the room |
| 4005 | `RATE_LIMITED` | rate limited |
| 4006 | `FRAME_TOO_LARGE` | frame too large |
| 4007 | `STATE_INCOMPATIBLE` | persisted state incompatible with this build |
| 4008 | `DISPOSED` | room disposed |
| 4009 | `REPLACED` | replaced by a newer socket for the same player |
| 4010 | `JOIN_REJECTED` | join rejected |

- 4000–4010 are assigned above. **4011–4099 are reserved** for future Hearth
  codes; **4100–4999** are for applications.
- The server normally sends an `ERROR` frame before closing so the client can
  report why. Some WebSocket stacks (Godot's among them) cannot deliver a frame
  that arrives in the same read as the close, so a client that sees one of these
  closes without an `ERROR` on that socket SHOULD report the corresponding error
  code, with the close reason as the message: 4000 `BAD_FRAME`, 4001
  `SCHEMA_MISMATCH`, 4002 `ROOM_FULL`, 4003 `ROOM_LOCKED`, 4005 `RATE_LIMITED`,
  4006 `FRAME_TOO_LARGE`, 4007 `STATE_INCOMPATIBLE`, 4010 `JOIN_REJECTED`.
- Every code in 4000–4999 is **fatal**: a client MUST NOT reconnect after it
  (section 11). 1000 is a deliberate close and also ends the session.

## 10. Reserved events

`EVENT` names starting with `hearth.` are reserved for the framework; rooms
cannot send them. An SDK MUST route the ones it knows to dedicated handlers and
not to the application's generic event callback. Unknown `hearth.*` names follow
section 12.

`hearth.handoff` (private to one player): the dedicated server to connect to.

```json
{ "op": "EVENT", "name": "hearth.handoff",
  "data": { "sid": "hs_..._0123456789abcdef", "endpoint": "udp://1.2.3.4:7777",
            "ticket": "<32 hex characters>", "expires_ms": 1760000000000 } }
```

| Field | Type | |
| --- | --- | --- |
| `sid` | string | the handoff session id |
| `endpoint` | string | `ws://`, `wss://` or `udp://` address of the dedicated server |
| `ticket` | string | this player's single-use credential, valid for 60 s |
| `expires_ms` | `u64` | ticket expiry, server wall-clock ms |

For `ws://`/`wss://` endpoints the client connects to `<endpoint>?ticket=<ticket>`.

`hearth.handoff_ended` (private to one player): the session is over.

```json
{ "op": "EVENT", "name": "hearth.handoff_ended",
  "data": { "sid": "hs_..._0123456789abcdef", "reason": "match over" } }
```

## 11. Reconnect

The server keeps a dropped player's seat for the room's reconnect grace period.
A client that reconnects within it with the same identity gets `JOIN_OK` with the
same seat and a fresh `FULL_STATE`.

A client SHOULD reconnect with this policy, and `conformance/vectors/reconnect.json`
is normative for it:

- Close code 1000 or any code in 4000–4999: give up.
- Otherwise retry after `min(500 ms × 2^attempt, 8000 ms)`, `attempt` starting
  at 0, until 30 s have passed since the first drop; then give up.

## 12. Forward compatibility

Within protocol v1 only additive changes are made (section 14). Every SDK MUST:

- Ignore unknown object fields.
- Ignore unknown `EVENT` names (passing them to the app is allowed).
- Ignore frames with unknown opcodes, with no error and no close.
- Treat unknown `ErrorCode` strings as a generic error.
- Treat unknown close codes in `4000–4999` as fatal (no reconnect), and other
  unknown codes by the existing policy.

Servers keep rejecting unknown opcodes with `BAD_FRAME`.
`conformance/vectors/forward.json` is normative for these rules.

## 13. HTTP gateway

Clients use these routes on the gateway's origin. Request and response bodies
are JSON. Authenticated routes take `Authorization: Bearer <token>`, except the
room socket, which takes `?token=`. Every route MAY answer **429** with a
`Retry-After` header (seconds); a client SHOULD wait that long and retry a
bounded number of times. Unknown room types answer **404**.

| Method and path | Auth | Request | Success | Errors |
| --- | --- | --- | --- | --- |
| `POST /auth/anonymous` | none | `{"device_id": string}` (8–128 characters) | 200 `{"token", "player_id", "display_name"}` | 400 bad body, 404 provider disabled |
| `POST /auth/jwt` | none | `{"jwt": string}` | 200 as above | 400, 401 rejected, 404 disabled, 502 upstream failure |
| `POST /auth/steam` | none | `{"ticket": hex string}` | 200 as above | 400, 401, 404, 502 |
| `POST /auth/itch` | none | `{"api_key": string}` | 200 as above | 400, 401, 404, 502 |
| `POST /auth/discord` | none | `{"code": string}` | 200 as above | 400, 401, 404, 502 |
| `POST /rooms/<type>` | bearer | none | 200 `{"room": string}` (a new room name) | 401, 404 |
| `GET /rooms/<type>/<name>?token=<token>` | query | WebSocket upgrade | 101, then section 5 | 400 bad name, 401, 404, 426 not an upgrade |
| `GET /schema/<type>` | none | none | 200 `{"schema_hash": string}` | 404 |
| `POST /match/<type>` | bearer | `{"pool": string}` (1–32 of `[A-Za-z0-9_-]`) | 200 `{"room": string, "region": string}` | 400, 401, 404, 501 not configured, 502 |
| `GET /lobby/<type>?pool=<pool>&limit=<n>` | bearer | none | 200 `{"rooms": [{"name", "players", "capacity", "metadata"}]}` | 400, 401, 404, 501, 502 |
| `GET /me` | bearer | none | 200 the player's own data (app-defined JSON) | 401, 501, 502 |

`display_name` is a string or `null`. Tokens expire after 24 hours; a client
then logs in again. Paths under `/__hearth/` and the admin routes are internal
and not part of this protocol.

## 14. Change policy

- This document is protocol version **1**. Within v1 only these additive
  changes are allowed: new optional fields, new `EVENT` names under `hearth.`,
  new error codes, new close codes in 4011–4099, and new opcodes that clients
  ignore.
- Anything else (removing or renaming a field, changing a type or a semantic,
  making an optional field required, a new required frame) needs protocol
  version 2, negotiated through `HELLO.protocol`.
- Every additive change updates this document and the conformance vectors in the
  same release.
