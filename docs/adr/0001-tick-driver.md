# ADR 0001: The tick driver and the maximum advertised tick rate

- **Status:** provisional — local measurements only; the deployed validation in
  v0.4 Task 1 Step 2 / Task 4 Step 3 is pending (this workspace has no Cloudflare
  credentials, and the user chose local-only runs for now).
- **Date:** 2026-10-25
- **Deciders:** Hearth maintainers
- **Plan:** [v0.4 ticked rooms](../superpowers/plans/2026-10-25-hearth-v0.4-ticked-rooms.md),
  Task 1.

## Context

Ticked mode runs a fixed simulation at 5–20 Hz while players are connected. The
Durable Object has one alarm, and room timers and the outbox already share it, so
the tick can be driven two ways:

1. **Alarm driver** — keep calling `set_alarm(next_tick)` from `alarm()`. This is
   the existing path: the DO can hibernate between ticks (no wall-clock charge
   between them) but each tick needs an alarm wake-up, and Cloudflare does not
   document alarm latency as a hard guarantee.
2. **Loop driver** — hold the room awake and drive ticks from `spawn_local` plus
   `worker::Delay`. Jitter is under the runtime's control, but the DO is billed
   wall-clock for the whole session.

The spec leaves the alarm-granularity question open. This ADR settles it with
measurements rather than opinion, and sets the ceiling the framework advertises.

## Decision rule

The rule is fixed before looking at the numbers, so the numbers cannot be
fitted afterwards:

- **Driver:** the alarm driver wins unless it misses the bar and the loop driver
  meets it. "Meets the bar" at 20 Hz is: p99 interval error **≤ 12.5 ms** (25%
  of the 50 ms period), missed / expected ≤ 1%, and no eviction mid-run.
- **Maximum advertised rate:** the highest of 10/15/20 Hz whose p99 error stays
  under 25% of the period with missed / expected ≤ 1% and no evictions. A tick
  rate above that ceiling is a compile error that points at Handoff (v0.5).
- **Cost:** if both drivers meet the bar, prefer the alarm driver: it can
  hibernate, and it is already the path every other timer uses. A local cost
  comparison is impossible (billing is per deployed request), so cost only breaks
  a tie in the deployed run.

## Measurements

### Native diff benchmark (bench/diff-bench)

Run on 2026-10-25 (Windows, release build), 50 measured passes + 5 warmup per
row, 10% of all JSON leaves changed per pass:

```
CLIENTS        VIEW   CURRENT(ms)   SPLIT(ms)   SPEEDUP   OPS/PASS
      4      1.0 KB         0.081       0.032     2.57x         21
      4     10.1 KB         1.437       0.355     4.05x        223
      4     99.9 KB        16.320       3.943     4.14x       2247
     16      1.0 KB         0.297       0.058     5.10x         94
     16     10.0 KB         4.922       0.782     6.30x        887
     16    100.2 KB        60.285       9.246     6.52x       9021
     64      1.0 KB         1.221       0.202     6.05x        352
     64      9.9 KB        20.045       2.699     7.43x       3610
     64    100.0 KB       235.145      31.898     7.37x      35690
```

The Task 2 split is faster in 9/9 rows (2.6–7.4x) with identical client-visible
op counts. At 64 clients and a 100 KB view, one pass goes from ~235 ms to
~32 ms: ticked rooms are 20 Hz, so the old per-client diff could not have held
the rate at that size. This is independent evidence for Task 2 apart from the
tick driver.

### Local tick benchmark (bench/tick-bench under wrangler dev)

2026-10-25, local workerd (one machine), 30 s per run, 8 runs; `interval_err`
is |actual − scheduled|, `missed` counts skipped slots of the absolute
schedule:

```
DRIVER   HZ   WORK_US    TICKS   EXPECTED   MISSED   P99_ERR_MS   P99_RX_MS
alarm    10         0      300        300        0       25.000     117.627
alarm    10      2000      300        300        0       34.000     121.995
alarm    20         0      598        600        2       34.000      74.575
alarm    20      2000      598        600        2       33.000      76.660
loop     10         0      300        300        0       16.000     113.910
loop     10      2000      300        300        0       15.000     113.913
loop     20         0      600        600        0       17.000      67.284
loop     20      2000      600        600        0       15.000      64.988
```

**Local workerd is not Cloudflare**, so this table is a harness smoke check,
not evidence about production alarm latency: both drivers are dominated by the
host timer quantum here (~15–35 ms), which is why neither meets the 12.5 ms p99
bar at 20 Hz even with no CPU burn. What it does show is that the driver choice
cannot be decided by local numbers, that the loop driver keeps 600/600 slots at
20 Hz locally while the alarm driver loses 2/600 (missed / expected = 0.33%,
inside the 1% allowance), and that the benchmark harness produces the numbers
the decision needs.

### Deployed tick benchmark

**Not measured yet.** Requires `wrangler login` (or a
`CLOUDFLARE_API_TOKEN`/`CLOUDFLARE_ACCOUNT_ID` pair) and the user's explicit
approval to deploy a benchmark Worker for ~10 minutes. When it happens:
`node bench/tick-bench/run.mjs --url <deployed> --hz 10,15,20 --secs 180 --work-us 0,2000`,
plus the GraphQL `durableObjectsInvocationsAdaptiveGroups` /
`durableObjectsPeriodicGroups` request counts, recorded here.

## Decision

**Provisional (pending deployed numbers): the alarm driver, 20 Hz ceiling.**

Reasoning that holds regardless of the deployed numbers: the alarm path already
exists for timers and the outbox, it hibernates between ticks (no charged
seconds while idle), and `next_wake()` already merges tick, timer and outbox
deadlines. Choosing the loop driver needs positive evidence that the alarm
driver misses the bar *in production*; the local numbers above cannot supply
that (both drivers are inside the host's timer quantum there), so adding a
second driver now would be speculative. The ceiling stays at 20 Hz because the
plan requires the 5–20 Hz range; the deployed run may lower it.

If the deployed run shows the alarm driver missing the bar and the loop driver
meeting it, this ADR flips to the loop driver and Task 4's adapter must grow the
`spawn_local` path (the runtime side needs no change: it exposes `next_wake()` and
`tick_if_due()` and does not care who calls them).

## Consequences

- The runtime (`RoomRuntime::tick_if_due`) is driver-agnostic; only the Durable
  Object adapter decides how the wake happens.
- Tick rates below 5 Hz and above 20 Hz are compile errors, with the >20 message
  naming Handoff.
- The advertised ceiling is provisional until the deployed benchmark and the
  Task 4 soak test run.

## Validation

Task 4's soak test (`node soak.mjs`, 8 clients, 300 s deployed / 60 s local)
checks the patch jitter and the stop-ticking rule:

- **Local soak (2026-10-25, `wrangler dev`, 20 Hz, `--secs 45`):** passed —
  room tick 918, adjacent-tick interval p50 4 ms / p95 18 ms / p99 20 ms over
  60 s wall; every `PATCH` carried `tick` and `t`; ticks were monotonic; the
  admin inspect showed the room stopped ticking after every player left (tick
  unchanged 10 s apart). Local p99 is above the 12.5 ms bar, which says nothing
  about production: it is the same host timer quantum as the local tick
  benchmark above.
- **Deployed soak:** _pending_ (local-only decision, see the status line).
- **Deployed p99 patch interval:** _pending_.

The local soak is the correctness check: it exercises the arena room (20 Hz,
buffered intents, catch-up, persist every 20, one alarm) over a real WebSocket
server, plus interpolation and prediction on the Godot clients.
