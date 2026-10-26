# Hearth v0.5 arena-dedicated Godot client

Minimal Godot 4.4 project for the v0.5 handoff sample. The Hearth addon is
single-sourced at `godot/addons/hearth` and copied in by `scripts/sync-addon.ps1`
(the copy is gitignored); re-run it after changing the addon.

## What is here

- `main.tscn` / `main.gd` — project shell (the real clients are the tests).
- `tests/server_helper.gd` — runs `HearthServer` (the headless dedicated-server
  helper) against the Task 2 fleet fixture: `from_env`, `redeem`, `report`,
  `start_heartbeat`. `tests/` one level up (`../tests/run-server-helper.ps1`)
  creates a fresh session with `node e2e-fleet.mjs --print-session`, exports the
  session env and runs this script twice (env missing and env present).
- `tests/handoff_smoke.gd` — the Task 3 container handoff smoke test: two
  clients ready up, receive `handoff`, `connect_handoff`, get the
  gateway-redeemed identity and the 30 Hz time broadcasts, then the session ends.
  **Validated on 2026-10-25 against a deployed container-capable gateway**, both
  natively and with `HEARTH_FORCE_WEB=1` (exit 0, all checks pass). It still
  cannot run locally on Windows: `wrangler dev` does not run Cloudflare
  Containers there (it needs WSL), so use a Linux-hosted `wrangler dev`/CI or a
  deployed gateway. Only the Task 3 e2e's check 6 (killing the container
  mid-match → `Failed{lost}` after 60 s) remains unverified, since it is
  local-Docker-only. With a container-capable gateway URL:

      $env:HEARTH_URL = "<container-capable gateway>"
      godot --headless --path examples/arena-dedicated/godot-client -s tests/handoff_smoke.gd

  Set `HEARTH_FORCE_WEB=1` to run the same scenario through the web backend
  (client-only check).

## Fleet path: starting the gateway

The sample's compile-time default allocator is the container mode, so the fleet
fixture must start the gateway in webhook mode. In `examples/arena-dedicated`
(with `.dev.vars` copied from `.dev.vars.example`):

    npx wrangler@4 dev --port 8790 --persist-to .wrangler/fleet-state `
      --enable-containers=false `
      --var HEARTH_ALLOCATOR_MODE:webhook `
      --var HEARTH_ALLOCATOR_URL:http://127.0.0.1:8791/allocate `
      --var HEARTH_ALLOW_INSECURE_ENDPOINTS:1

- **Windows:** `--enable-containers=false` is required; `wrangler dev` cannot
  run Cloudflare Containers locally on Windows (it needs WSL). The fleet path
  never starts a container, so switching the container dev support off is safe.
- **Linux:** the flag is optional but harmless; keep the same command so the
  fixture line is identical on both hosts.
- `HEARTH_ALLOCATOR_MODE=webhook` and the two allocator vars are required on both
  hosts for the fleet path. `.dev.vars` supplies `HEARTH_SECRET` and
  `HEARTH_ALLOCATOR_SECRET` (the runner gives the allocator the same secret the
  room verifies the webhook with).

## Checks

    godot --headless --import --path examples/arena-dedicated/godot-client
    godot --headless --path examples/arena-dedicated/godot-client --quit
    godot --headless --check-only --path examples/arena-dedicated/godot-client -s tests/server_helper.gd
    godot --headless --check-only --path examples/arena-dedicated/godot-client -s tests/handoff_smoke.gd
    pwsh -File examples/arena-dedicated/tests/run-server-helper.ps1   # needs the fleet gateway
