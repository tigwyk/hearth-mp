# Hearth conformance kit

Everything a Hearth SDK in any language needs to check itself against protocol
v1 (`spec/protocol-v1.md`), without Cloudflare. **The vectors are normative:**
where the spec text and a vector disagree, the vector wins and the text is the
bug.

## Vectors (`vectors/`)

Each file is a JSON array of cases. A test that loads one MUST fail when the
file is missing or holds no cases. A loader that quietly returns an empty list
turns every conformance test into a pass.

| File | Level | Case format |
| --- | --- | --- |
| `frames.json` | MUST | `{name, direction: "c2s"\|"s2c", json, msgpack_hex}`: `json` is the frame object (the JSON-mode text), `msgpack_hex` the exact binary frame (opcode byte + named MessagePack), lowercase hex. Encoding `json` MUST give exactly `msgpack_hex`, and decoding `msgpack_hex` MUST give exactly `json`. |
| `patch.json` | MUST | `{name, before, ops, after}` or `{name, before, ops, error: true}`: apply `ops` in order to `before`. |
| `forward.json` | MUST | `{name, direction, json\|null, msgpack_hex\|null, expect, inject}`: frames from a newer protocol revision, in the encodings they exist in. `expect` is `"ignored"` (no event, state untouched), `"event_or_ignored"`, or `{"event": <pattern>, "state_after"?}`. After every case the client MUST still apply a normal patch. `inject` marks the frames the reference server sends in the `forward_compat` scenario. |
| `reconnect.json` | SHOULD | `{name, code, first_drop_ms, attempt, now_ms, expect}`: `expect` is the absolute retry time, or `"give_up"`. |
| `interp.json` | SHOULD | `{name, tick_hz, snapshots: [[server_t, value]], render_t, expect, track?, sample?}`: the value is stored at `["value"]` unless `track`/`sample` give paths (`"*"` matches every key). `expect` is a value or `null`. |

A minimal SDK passes `frames`, `patch` and `forward`. `reconnect` and `interp`
cover recommended client behaviour.

## Scenarios (`scenarios/`) and the reference server

`hearth-conformance-server` runs the real room runtime over local WebSockets and
hosts the conformance room `Conf` (`crates/hearth-conformance-server/src/room.rs`):
two seats, state `{n, seats, secret (owner-only)}`, intents
`Add(i64) | SetSecret(String) | Echo(String) | Fail | Future`.

```sh
cargo run -p hearth-conformance-server            # 127.0.0.1:8790; --port to change
```

- `POST /auth/anonymous {"device_id": "a"}` answers `{"token": "a", "player_id": "a"}`:
  the token is the player id, so an SDK runs its normal login and join flow.
  It deliberately accepts any `[A-Za-z0-9_-]+` id, so scenarios can name clients
  `a` and `b`; a real gateway requires 8–128 characters (`spec/protocol-v1.md`
  section 13), so an SDK's generated device ids must be at least 8 long.
- `GET /schema/conf` answers `{"schema_hash": "..."}`.
- `ws://127.0.0.1:8790/rooms/conf/<room>?token=<player>` is the room socket.

It has **no authentication**. It is a local test server only; never expose it.

A scenario is
`{name, about, clients: ["a", ...], steps: [...]}`, and the steps run in order:

| Step | Meaning |
| --- | --- |
| `{"client": "a", "send": "connect"}` | log in as `a` and join the scenario's room (`HELLO` with the right hash, `opts: {}`) |
| `{"client": "a", "send": {"op": "HELLO", ...}}` | join with this `schema_hash` and `opts` |
| `{"client": "a", "send": {"op": "INTENT", "intents": [...]}}` | queue each `intent` (the SDK numbers them itself) |
| `{"client": "a", "send": {"op": "LEAVE"}}` | leave |
| `{"client": "a", "send": "drop"}` | drop the socket abruptly; the SDK reconnects by itself |
| `{"client": "a", "expect": <pattern>}` | scan `a`'s observed frames forward for the first one matching |
| `{"wait_ms": n}` | wait |

A runner turns what its SDK reports into **observed frames**:
`{"op":"JOIN_OK", player_id, seat}`, `{"op":"FULL_STATE", state}`,
`{"op":"PATCH", ops, state}` (the state after the ops),
`{"op":"ACK", intent_seqs}`, `{"op":"EVENT", name, data}`,
`{"op":"ERROR", code, message, ref_seq}` and `{"op":"CLOSE", code}`.

A pattern matches when every field it gives is equal. `"*"` matches any value,
`"$absent"` means the key must be missing, arrays match element-wise with equal
length, and numbers compare as numbers (`2` equals `2.0`).

Runners in this repository:

- Rust: `cargo test -p hearth-conformance-server` (starts the server in-process).
- Godot: `examples/tictactoe-godot/tests/scenario_runner.gd`, once with
  `HEARTH_FORCE_WEB=1` (the GDScript client) and once without (the GDExtension).
- Unity: `ScenarioTests` in the package's PlayMode tests.

The engine runners read `HEARTH_CONFORMANCE_URL` (for example
`http://127.0.0.1:8790`). They skip when it is unset, unless
`HEARTH_REQUIRE_CONFORMANCE=1`, which makes that a failure.

To run the scenarios against your own SDK, write a runner with the same observed
frames and pattern rules (the Rust runner,
`crates/hearth-conformance-server/tests/scenarios.rs`, is about 200 lines), start
the server, and run every file in `scenarios/`.
