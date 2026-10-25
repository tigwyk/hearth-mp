# tick-bench — Hearth v0.4 Task 1

Decides the ticked-room driver for ADR 0001: an **alarm-driven** fixed tick versus
an **in-isolate loop**, measured on Cloudflare's platform rather than on the
Hearth runtime.

This is a standalone Worker (its own Cargo workspace, `worker` only). It has two
Durable Object classes that do identical work — one byte per connected client per
tick, plus a configurable CPU burn — and differ only in how the next tick is
scheduled:

| class | driver |
| --- | --- |
| `AlarmTicker` | `alarm()` runs one tick, then `set_alarm(start + n * period)`. An absolute schedule, so lateness does not accumulate. Alarms are durable: an evicted object wakes to handle them. |
| `LoopTicker` | `/start` holds the object open with `state.wait_until`, and a `worker::Delay` loop waits out the remaining time to each slot. No alarms. |

Both accept one hibernatable WebSocket and send one byte per tick, so
`run.mjs` also records client-side receive jitter. The durable `tick-bench-run`
marker lets a fresh instance report `evicted: true` when the in-memory run
vanished mid-run (hibernation/eviction).

## Local smoke run

Local workerd is a smoke check only: its scheduling and eviction behaviour are
not production's, and it does not bill. The numbers for the ADR come from the
deployed matrix below.

```powershell
cd bench/tick-bench
npx wrangler dev            # terminal 1; builds with worker-build
node run.mjs --url http://127.0.0.1:8787 --hz 20 --secs 60   # terminal 2
```

Expected: `results.json` is written for both classes, `ticks` is close to
`expected`, `missed` is 0 or very small, `evicted` is `false`. The tick loop is
visible in `wrangler dev` output.

## Deployed matrix (asks the user first)

Deploying creates a Worker and costs a few cents. Ask the user before running
this, then remove the Worker afterwards.

```powershell
cd bench/tick-bench
npx wrangler deploy
node run.mjs --url https://<worker>.<account>.workers.dev --hz 10,20 --secs 600 --work-us 0,2000
npx wrangler delete
```

The full matrix is 8 runs (2 drivers × 2 rates × 2 CPU loads), all in parallel,
about 10 minutes of wall time per deployment.

## GraphQL Analytics (optional)

With credentials in the environment, `run.mjs` also queries the GraphQL
Analytics API for the observed Durable Object request counts over the run window:

```powershell
$env:CLOUDFLARE_API_TOKEN = '...'   # token with Account Analytics: Read
$env:CLOUDFLARE_ACCOUNT_ID = '...'
node run.mjs --url https://... --hz 10,20 --secs 600 --work-us 0,2000
```

The query is best effort: dataset fields differ per account and plan, so check
them with GraphQL introspection if it errors; the error is recorded in
`results.json` and the run itself still succeeds. Without the variables the
section is skipped. Either way the local billing estimate is always included
(requests ≈ ticks + 3, duration ≈ DOs × wall seconds × 0.128 GB, matching the
Hearth v0.3b cost model).

## results.json

```jsonc
{
  "generated_at": "...",
  "url": "https://...",
  "secs": 600,
  "window": { "started_at": "...", "ended_at": "..." },
  "runs": [
    {
      "driver": "alarm",          // or "loop"
      "hz": 20,
      "work_us": 2000,
      "name": "alarm-20-2000-...", // Durable Object name used
      "server": {
        "ticks": 12000,
        "expected": 12000,        // secs * hz
        "missed": 0,              // skipped slots; ticks + missed == expected
        "interval_err_ms": { "p50": 0.4, "p95": 1.1, "p99": 2.0, "max": 9.0 },
        "evicted": false
      },
      "client": {
        "received": 11998,
        "expected": 12000,
        "interval_ms": { "p50": 50.0, "p95": 51.2, "p99": 55.0, "max": 120.0, "mean": 50.1 },
        "drop_ratio": 0.0001
      },
      "error": null
    }
  ],
  "analytics": { "estimate": { "per_driver": { "alarm": {...}, "loop": {...} } }, "data": {...} }
}
```

- `ticks` — slots actually executed.
- `expected` — `secs * hz`.
- `missed` — slots skipped because a wake was late, plus the unrun tail if the
  run ended late. A late wake runs one tick at the current slot instead of a
  catch-up burst, so `ticks + missed == expected` once stopped.
- `interval_err_ms` — `|actual − scheduled|` of every executed tick (server clock).
- `evicted` — the in-memory run marker disappeared mid-run (a hibernation or
  eviction); the ADR requires no evictions for the chosen driver.
- `client.interval_ms` — client-side receive intervals of the 1-byte tick frames
  (`performance.now()` at the WebSocket message event).

## ADR 0001 decision rule

Pick the first option that holds, using the 20 Hz / `work_us = 2000` row:

1. **Alarm** if `server.p99 interval_err_ms ≤ 12.5`, `missed / expected ≤ 1%`
   and no run was evicted.
2. Otherwise **Loop**, if it meets the same bar.
3. Otherwise lower the supported `tick_hz` to the highest tested rate that meets
   the bar for either driver, and choose that driver.

## run.mjs options

| flag | default | meaning |
| --- | --- | --- |
| `--url` | `http://127.0.0.1:8787` | Worker base URL (`http` or `https`; WebSocket scheme is derived) |
| `--hz` | `20` | comma-separated tick rates |
| `--secs` | `60` | run length; the matrix uses `600` |
| `--work-us` | `0` | comma-separated CPU burn per tick in microseconds |
| `--out` | `bench/tick-bench/results.json` | output path |
| `--script-name` | `hearth-tick-bench` | deployed Worker name for the analytics filter |
| `--no-analytics` | off | skip the GraphQL query even with credentials |

## Notes

- `Date.now()` in Workers has 1 ms resolution, so `work_us` rounds up to the next
  millisecond of CPU burn. At 20 Hz / `work_us = 2000` that is a few percent of
  the 50 ms period, which is the same for both drivers.
- The loop driver uses `state.wait_until` rather than a bare
  `wasm_bindgen_futures::spawn_local`: a detached future is cancelled when the
  `/start` request finishes, while `wait_until` is the Workers API that keeps the
  object alive for the whole run.
- The Durable Object URL is built by the Worker entry
  (`/<alarm|loop>/<name>/<ws|start|stats>`), so the client never talks to a DO
  directly.
