# Hearth wire protocol, version 1

Language-neutral specification for Hearth v0.1 (event-mode rooms). This is enough
to implement an SDK in any language; the reference implementation is
`hearth-protocol` and the patch fixture in
`crates/hearth-protocol/tests/fixtures/patch_cases.json` is the conformance
suite.

## 1. Transport and framing

- Transport is WebSocket. The Gateway Worker authenticates the connection and
  forwards it to one Durable Object; the room owns the socket from then on.
- The client chooses the encoding with its first frame (`HELLO`):
  - **Binary frame** — one opcode byte (`u8`) followed by the MessagePack body.
    Struct fields are encoded by name (not the compact tuple form).
  - **Text frame** — a JSON body.
  - A binary `HELLO` makes the socket MsgPack; a text `HELLO` makes it JSON.
    Every frame in either direction then uses that encoding for the life of the
    socket.
- Every frame body is a JSON-style object with a string `op` tag, the frame name
  in `SCREAMING_SNAKE_CASE` (for example `{"op":"PATCH", ...}`). In a binary
  frame the leading opcode byte must agree with the `op` tag; a mismatch is a
  protocol error.
- **Maximum incoming frame size is 65536 bytes (64 KiB)**, enforced before any
  deserialisation. An oversized frame is not decoded; the socket is closed with
  code 4006.
- The state tree and all payloads use JSON data types only: `null`, `bool`,
  number, string, array, object. Object keys are always strings. Integers beyond
  2^53 may lose precision in JSON/GDScript/JS clients, so identifiers are
  strings and never numbers (`PlayerId` is a string; the schema hash is a
  16-character hex string).
- **Keepalive.** The client may send the *text* frame `hearth:ping`; the Durable
  Object auto-responds with the text frame `hearth:pong` without waking the
  room. These two text frames are not protocol frames and are never decoded as
  JSON. No other text frame is valid in MsgPack mode.

## 2. Opcode table

| Name | Byte | Direction | Purpose |
| --- | --- | --- | --- |
| `HELLO` | `0x01` | client → server | handshake, selects encoding |
| `JOIN_OK` | `0x02` | server → client | join accepted, seat assigned |
| `FULL_STATE` | `0x03` | server → client | complete state snapshot |
| `PATCH` | `0x04` | server → client | incremental state change |
| `INTENT` | `0x05` | client → server | one or more player intents |
| `EVENT` | `0x06` | server → client | out-of-band event (chat, sound cues) |
| `ACK` | `0x07` | server → client | intent acknowledgement |
| `PING` | `0x08` | *reserved* | not used; keepalive is the text frame above |
| `ERROR` | `0x09` | server → client | error with optional intent reference |
| `LEAVE` | `0x0A` | client → server | voluntary leave |

## 3. Frames

Field types are JSON types: `u16`/`u32`/`u64` are numbers (MessagePack integers),
`string`, `object`, `array`, `any` is any JSON value, `|null` is nullable.

### `HELLO` — client → server

```json
{
  "op": "HELLO",
  "protocol": 1,
  "schema_hash": "0123456789abcdef",
  "opts": {}
}
```

- `protocol`: protocol version, currently `1`.
- `schema_hash`: the client's build's schema hash, 16 lowercase hex characters.
  The hash covers both the room's state type and its `Intent` type, so a change
  to either makes stale clients fail loudly instead of desyncing.
- `opts`: pass-through join options for the room's `on_join` hook (for example a
  reconnect token in a later version). May be `{}`.

`HELLO` must be the first frame. The encoding of this frame selects the socket
encoding. Errors: wrong `protocol` → `UNSUPPORTED_PROTOCOL` and close 4000; wrong
`schema_hash` → `SCHEMA_MISMATCH` (the message names both hashes) and close 4001.

### `JOIN_OK` — server → client

```json
{ "op": "JOIN_OK", "player_id": "anon_2f8c...", "seat": 0 }
```

- `player_id`: stable string identity verified by the gateway.
- `seat`: this player's seat, `0..MAX_CLIENTS`.

### `FULL_STATE` — server → client

```json
{ "op": "FULL_STATE", "seq": 12, "state": { "turn": "anon_a", "players": {} } }
```

- `seq`: state sequence number this snapshot corresponds to.
- `state`: the client's complete view of the state (sync fields only, with owner
  fields filtered to this viewer). The state tree is replaced wholesale; a
  client that receives `FULL_STATE` again (reconnect) discards its local tree.

### `PATCH` — server → client

```json
{ "op": "PATCH", "seq": 13, "ops": [ /* PatchOp */ ] }
```

- `ops`: ordered list of patch operations. Applying them in order to the state
  tree yields the client's view at sequence number `seq`.
- A client that already holds `seq` or a higher sequence number ignores the
  frame. A missing sequence number means frames were lost; v0.1 clients should
  treat that as a desync (reconnect for a fresh `FULL_STATE`).
- Applying an op with a bad path is a desync: drop the patched tree, emit a
  local desync signal, and wait for `FULL_STATE`.

### `INTENT` — client → server

```json
{ "op": "INTENT", "intents": [ { "seq": 1, "intent": { "Move": { "cell": 4 } } } ] }
```

- `intents`: at least one intent message, in application order, from one client.
- `seq`: client-assigned intent sequence number, starting at 1 and increasing by
  exactly 1 per intent queued on that socket.
- `intent`: the room's intent payload, decoded by the server's room type. A
  payload that does not decode is `BAD_INTENT`.
- SDKs batch intents into one `INTENT` frame: default sending rate is at most
  20 messages/s and at most 32 intents per message. Incoming WebSocket messages
  are what Cloudflare bills, so batching is the cost control.

### `ACK` — server → client

```json
{ "op": "ACK", "state_seq": 14, "intent_seqs": [1, 2] }
```

- `state_seq`: state sequence number after the accepted intents in this handler
  were applied.
- `intent_seqs`: the intent sequence numbers that were accepted (a batch may mix
  accepted and rejected intents; see `ERROR.ref_seq`). `ACK` is the client
  prediction/reconciliation hook. One `ACK` may cover several `INTENT` frames
  when they are handled together.

### `EVENT` — server → client

```json
{ "op": "EVENT", "name": "shout", "data": { "text": "hi" } }
```

Out-of-band events are not part of state sync. They arrive after the `PATCH`
for the same handler, in the order the handler emitted them.

### `ERROR` — server → client

```json
{ "op": "ERROR", "code": "BAD_INTENT", "message": "intent did not decode", "ref_seq": 3 }
```

- `code`: one of the error codes in section 5.
- `message`: human-readable detail; clients must not parse it.
- `ref_seq`: the intent sequence number this error refers to, or `null` for
  errors not tied to an intent (for example `SCHEMA_MISMATCH`).

### `LEAVE` — client → server

```json
{ "op": "LEAVE" }
```

The client is leaving voluntarily. The server calls `on_leave` with reason
`Left`. The server may also close the socket instead; both are equivalent.

## 4. Patch operations

A `PATCH` carries an ordered list of operations. Each operation is a JSON object
with an `op` tag in `snake_case`:

| `op` | Fields | Effect |
| --- | --- | --- |
| `set` | `path`, `value` | replace the value at `path` |
| `del` | `path` | remove the map key or list index at `path` |
| `map_insert` | `path`, `key`, `value` | insert or replace string `key` in the object at `path` |
| `list_splice` | `path`, `index`, `delete`, `insert` | splice `insert` into the array at `path` |

`path` is an array of segments. A **string** segment selects an object key
(field); an **integer** segment selects an array index. The empty path `[]`
selects the root of the state tree.

Semantics, shared by the reference implementation and every SDK:

- `set`: an empty path replaces the whole root (the state becomes `value`).
  Otherwise the parent path must exist. A missing object key is created;
  an array index must already exist (setting past the end is a bad path).
- `del`: the parent path must exist and the final segment must be present:
  a missing object key, an out-of-range index, or a non-string/non-integer
  segment is a bad path.
- `map_insert`: `path` must resolve to an object. The key is inserted, or its
  existing value replaced.
- `list_splice`: `path` must resolve to an array. `index <= length` and
  `index + delete <= length`; otherwise bad path. `delete` values are removed
  starting at `index`, then `insert` values are inserted in their place.
- Operations are applied in order; later operations see the result of earlier
  ones.
- The fixture `crates/hearth-protocol/tests/fixtures/patch_cases.json` is the
  normative conformance suite for these semantics. Each case has `name`,
  `before`, `ops`, and either `after` or `"error": true`.

Server diffs only emit the operations needed to turn the previous view into the
new view: object keys are deleted, inserted, then recursed in a deterministic
order; array changes are one `list_splice` computed from the common prefix and
suffix; any other difference is a `set`.

## 5. Error codes

| Code | Meaning |
| --- | --- |
| `BAD_FRAME` | frame could not be decoded |
| `UNSUPPORTED_PROTOCOL` | `HELLO.protocol` is not supported |
| `SCHEMA_MISMATCH` | schema hash differs from the room's |
| `ROOM_FULL` | no seat left (`MAX_CLIENTS` reached) |
| `ROOM_LOCKED` | room is locked and refuses new players |
| `NOT_JOINED` | frame arrived before a successful `HELLO` |
| `BAD_INTENT` | intent payload did not decode into the room's intent type |
| `INTENT_REJECTED` | `on_intent` returned an error; no state change was kept |
| `JOIN_REJECTED` | `on_join` returned an error |
| `RATE_LIMITED` | per-player intent rate cap exceeded |
| `FRAME_TOO_LARGE` | incoming frame exceeded 64 KiB |
| `STATE_INCOMPATIBLE` | persisted room state cannot be read by this build |
| `INTERNAL` | unexpected server error |

## 6. Close codes

| Code | Meaning |
| --- | --- |
| 4000 | bad frame or bad protocol version |
| 4001 | schema mismatch |
| 4002 | room full |
| 4003 | room locked |
| 4004 | kicked by the room |
| 4005 | rate limited |
| 4006 | frame too large |
| 4007 | persisted state incompatible with this build |
| 4008 | room disposed |
| 4009 | replaced by a newer socket for the same player |
| 4010 | join rejected |

An `ERROR` frame is normally sent before the close so the client can report the
reason.

## 7. Handshake and sequencing

1. The client obtains a room name and token from the gateway, then opens the
   WebSocket. Authentication happens in the gateway; the room never sees
   credentials.
2. Client sends `HELLO` as its first frame. The frame's encoding selects the
   socket encoding (`HELLO` is a request for the whole socket, so sending it
   binary means MsgPack from then on).
3. Server validates protocol version and schema hash, runs the room's `on_join`,
   then sends `JOIN_OK` followed by `FULL_STATE`.
4. Client replaces local state with `FULL_STATE.state` and records its `seq`.
5. Client sends `INTENT` frames. The server applies accepted intents, persists
   if state changed, sends one `PATCH` (state observers) and one `ACK` to the
   sender for accepted intents, plus `ERROR` frames for rejected ones.
6. State sequence numbers (`FULL_STATE.seq` and `PATCH.seq`) are monotonic u64
   per room and increase by one per state flush that changed anything. Intent
   sequence numbers are per socket, assigned by the client, monotonic u32.
7. A second socket for the same player replaces the first: the old socket is
   closed with 4009 `REPLACED`, the seat is kept, and `on_join`/`on_leave` are
   not called again. The new socket receives `JOIN_OK` with the same seat and a
   fresh `FULL_STATE`.

## 8. Conformance checklist for a new SDK

- [ ] Select encoding from the `HELLO` you send; use it for every later frame.
- [ ] Reject incoming frames over 64 KiB before parsing.
- [ ] In binary mode, check the opcode byte against the body's `op` tag.
- [ ] Parse the schema hash as a string of 16 hex characters, never a number.
- [ ] Apply `FULL_STATE` by replacing the tree; apply `PATCH` ops in order.
- [ ] Ignore a `PATCH` whose `seq` is not greater than the current sequence.
- [ ] Treat a bad patch path as a desync and wait for `FULL_STATE`.
- [ ] Batch intents (default ≤ 32 per message) and cap send rate (default ≤ 20/s).
- [ ] Assign intent sequence numbers from 1, incrementing by exactly 1.
- [ ] Send `hearth:ping` as a text frame; accept `hearth:pong` text frames.
- [ ] Pass every case in `crates/hearth-protocol/tests/fixtures/patch_cases.json`.
