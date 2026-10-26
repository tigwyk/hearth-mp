# arena-dedicated/server — the containerised dedicated server

The dedicated arena server used by the `arena-dedicated` sample. It is the
container the lobby hands two players to (Task 3 of the v0.5 plan).

## What it does

- `GET /healthz` → `200 ok`. This is the container adapter's readiness check, and
  the only non-WebSocket request it serves.
- WebSocket connections are the gateway's forwarded `/handoff/{sid}/connect`
  proxy. The gateway has already redeemed the player's single-use ticket and sets
  these headers, which the server trusts (only the gateway can reach the port):

  | header | meaning |
  | --- | --- |
  | `X-Hearth-Player-Id` | the redeemed player id |
  | `X-Hearth-Provider` | the auth provider that minted the identity |
  | `X-Hearth-Display-Name` | percent-encoded display name |

  A connection without `X-Hearth-Player-Id` gets `400`.
- It reports the handoff lifecycle through `hearth-mp-handoff-server`:
  `started` on boot, `player_joined` per new player, then `result`
  (`{"winner": ...}`) and `ended` when the five-second match finishes, then the
  process exits.
- Once two players are connected it broadcasts `{"type":"tick","t":<ms>}` at
  30 Hz for five seconds. Every accepted socket first receives a
  `{"type":"hello","player_id":...}` echo of the redeemed identity.

## Environment

| variable | default | meaning |
| --- | --- | --- |
| `HEARTH_SID` | — | handoff session id (from the container adapter) |
| `HEARTH_SESSION_SECRET` | — | session secret; the only credential the server has |
| `HEARTH_REPORT_URL` | — | `<public_url>/handoff/<sid>`; reports go to `/report` |
| `HEARTH_PORT` | `9000` | the port to listen on (the adapter passes its configured port) |

Without `HEARTH_*` the process still serves `/healthz` and WebSockets, so it can
be smoke-tested locally; it just does not report anything.

## Run it locally

```powershell
cd examples/arena-dedicated/server
$env:HEARTH_PORT = '9000'
cargo run
curl.exe http://127.0.0.1:9000/healthz    # 200 ok
```

## Build the container image

The crate is its own Cargo workspace, but the **Docker build context is the
repository root**, because the sample path-depends on the `hearth-*` crates and
their manifests inherit the root workspace:

```powershell
docker build -f examples/arena-dedicated/server/Dockerfile .
```

`Dockerfile.dockerignore` keeps that root context small. The lobby's
`wrangler.toml` points at this file as `image = "./server/Dockerfile"`; the
`[[containers]]` entry sets `class_name = "ArenaServer"`, which is the class
defined with `container_object!(ArenaServer)` in the lobby Worker.
