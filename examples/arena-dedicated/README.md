# arena-dedicated — the handoff sample

A lobby room that hands two ready players to a dedicated arena server. The
sample wires **both** handoff allocators through the same room code; the
allocator is chosen at runtime:

| mode | how it works | where the server runs | e2e |
| --- | --- | --- | --- |
| **containers** (default) | `#[hearth::room(allocator = "container", container = "ARENA_SERVER")]`; each session's `ArenaServer` object starts one Cloudflare Container from `server/Dockerfile` | Cloudflare Containers | `e2e-container.mjs` |
| **webhook fleet** | `HEARTH_ALLOCATOR_MODE = "webhook"`; the adapter POSTs a signed allocation to `fleet/allocator.mjs`, which starts `fleet/fake-server` | your own fleet | `e2e-fleet.mjs` |

The room attribute is the compile-time default; `HEARTH_ALLOCATOR_MODE` overrides
it per deployment. That is how one sample serves both e2e suites.

Contents:

- `src/lib.rs` — the `ArenaLobby` room, `ArenaPlayer` service, the
  `container_object!(ArenaServer)` class, and the gateway routes (with
  `container: Some("ARENA_SERVER")`).
- `server/` — the dedicated arena server that runs in the container
  (`GET /healthz`, WebSocket match, 30 Hz time broadcast, lifecycle reports).
  Its Dockerfile is built from the repository root; see `server/README.md`.
- `fleet/` — the webhook allocator and a local fake dedicated server for the
  webhook mode.
- `e2e-container.mjs` / `e2e-fleet.mjs` — the two end-to-end tests.

## Run the fleet (webhook) e2e locally

The fleet path has no containers, so it works on every platform:

```powershell
# terminal 1
cd examples/arena-dedicated
npx wrangler dev --port 8790 --persist-to .wrangler/state --enable-containers=false `
  --var HEARTH_ALLOCATOR_MODE:webhook `
  --var HEARTH_ALLOCATOR_URL:http://127.0.0.1:8791/allocate `
  --var HEARTH_ALLOW_INSECURE_ENDPOINTS:1

# terminal 2
cd examples/arena-dedicated
node e2e-fleet.mjs          # prints `e2e-fleet ok`
```

`.dev.vars` must hold `HEARTH_SECRET` and `HEARTH_ALLOCATOR_SECRET` (copy
`.dev.vars.example`). `--enable-containers=false` is required on Windows, and is
the supported escape hatch anywhere you do not want wrangler to build the
container image.

## Run the containers e2e

Local Cloudflare Containers are **not supported by `wrangler dev` on Windows**:

```text
Local development with containers is currently not supported on Windows.
```

Run it on Linux, inside WSL, on macOS, or against a deployed Worker (a preview
deploy creates billed resources, so ask first):

```bash
# terminal 1 (Linux/WSL/macOS, Docker running)
cd examples/arena-dedicated
npx wrangler dev --port 8792 --persist-to .wrangler/state-container \
  --var HEARTH_PUBLIC_URL:http://127.0.0.1:8792

# terminal 2
cd examples/arena-dedicated
node e2e-container.mjs      # prints `e2e-container ok`
```

The image is built from the repository root (the sample path-depends on the
`hearth-*` crates), which `wrangler.toml` expresses as:

```toml
[[containers]]
class_name = "ArenaServer"
image = "./server/Dockerfile"
image_build_context = "../.."
max_instances = 10
```

To build the image by hand: `docker build -f examples/arena-dedicated/server/Dockerfile .`
from the repository root.

## Environment

| variable | mode | meaning |
| --- | --- | --- |
| `HEARTH_SECRET` | both | the gateway's token secret (`.dev.vars`, ≥32 bytes) |
| `HEARTH_PUBLIC_URL` | both | the gateway's public base URL; players get `<it>/handoff/<sid>/connect` |
| `HEARTH_ALLOCATOR_MODE` | both | `"webhook"` or `"container"`; overrides the room attribute |
| `HEARTH_ALLOCATOR_URL` | webhook | where the adapter POSTs allocation requests |
| `HEARTH_ALLOCATOR_SECRET` | webhook | HMAC signing secret (≥32 bytes, `.dev.vars`) |
| `HEARTH_ALLOW_INSECURE_ENDPOINTS` | webhook | allow `ws://` endpoints (local dev) |
| `HEARTH_CONTAINER_PORT` | containers | the port inside the container (default 8080) |

The session secret is delivered only to the allocator/container; players only
ever receive their own single-use ticket.
