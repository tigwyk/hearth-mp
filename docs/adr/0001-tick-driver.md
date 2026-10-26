# ADR 0001: The tick driver and the maximum advertised tick rate

- **Status:** accepted (2026-10-25) — the deployed benchmark rejects the alarm
  driver, so the shipped adapter uses the in-object loop driver with a 20 Hz
  ceiling.
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

Run on 2026-10-25 against `hearth-tick-bench` (`wrangler deploy`, account
`0db7de82d434c5218aca4e0eaa86c2cc`) with

```sh
node bench/tick-bench/run.mjs --url https://hearth-tick-bench.lee-0db.workers.dev \
  --hz 10,15,20 --secs 60 --work-us 0,2000
```

Twelve durable objects ran in parallel, one per driver × rate × CPU load. Both
CPU loads agreed within noise (the `work_us = 2000` rows differ by less than one
tick and less than one millisecond of p99), so the table shows the
`work_us = 0` rows:

| driver | metric | 10 Hz | 15 Hz | 20 Hz |
| --- | --- | --- | --- | --- |
| Alarms API | ticks / expected | 507 / 600 | 506 / 900 | 505 / 1200 |
| Alarms API | missed slots | 15.5% | 43.8% | 57.9% |
| Alarms API | p99 schedule error | 98.0 ms | 65.7 ms | 49.0 ms |
| in-object loop | ticks / expected | 600 / 600 | 900 / 900 | 1200 / 1200 |
| in-object loop | missed slots | 0 | 0 | 0 |
| in-object loop | p99 schedule error | 0.0 ms | 1.0 ms | 0.0 ms |

The client-side receive jitter (one byte per tick over a hibernatable WebSocket)
tells the same story: p99 receive interval 279–353 ms for the alarm driver and
52–106 ms for the loop driver, i.e. the loop's 50 ms cadence survived the network
hop with normal jitter and the alarm's did not.

Two harness bugs had to be fixed before this table was valid. Both are
deployed-runtime behaviour that local workerd cannot show, and both are recorded
in the bench:

- `Date.now()` is frozen during synchronous execution in the deployed Workers
  runtime (a Spectre mitigation), so a "burn N µs by spinning on the clock" loop
  never exits there and the object hits its CPU limit. The bench now burns a
  fixed iteration count (approximate microseconds).
- At 15 Hz, `start + n * 66.666… ms` can round onto a millisecond that the float
  division floors one short. The loop woke with nothing to do and spun until the
  CPU limit. It now waits 1 ms when a wake makes no progress.

GraphQL request counts were not captured: the Analytics API rate-limited this
account before the matrix ran, and `run.mjs` reads only
`CLOUDFLARE_API_TOKEN`/`CLOUDFLARE_ACCOUNT_ID`, which this workspace does not
have. The billed shape is derivable from the measured ticks: the alarm driver
pays roughly one request per wake (ticks plus the watchdog), the loop driver pays
only its start/stats requests, and both bill duration for the whole run because a
ticking room never meets the 10 s idle-hibernation condition under either driver.

## Decision

**The in-object loop driver, with a 20 Hz ceiling (accepted 2026-10-25).**

The decision rule required the alarm driver's p99 error to stay at or under
12.5 ms and its missed/expected ratio at or under 1%. The deployed numbers miss
both by a wide margin at every rate (49–98 ms, 15–58%), while the loop driver met
the bar at every rate (0–1 ms, no missed slots). The rule therefore selects the
loop driver, and the highest measured rate that meets the bar is 20 Hz, so the
plan's 5–20 Hz range is the advertised range.

Why holding the object open is the cheaper option, not just the accurate one:

- A ticking room bills duration for as long as it ticks. The alarm driver would
  have billed the same in practice: with a tick every 50–100 ms the object never
  sees the 10 s of idleness that hibernation requires, so it never hibernates
  while ticking. The loop driver additionally removes the per-wake request
  charge: the alarm fires only for timers and the five-minute watchdog.
- The loop needs a keepalive. A single outstanding `State::wait_until` operation
  protects an object from eviction for at most 15 minutes, so the runtime
  schedules a watchdog alarm every five minutes. The alarm renews the keepalive
  (the adapter starts a fresh loop generation) and is the wake that restarts a
  loop lost to an eviction. It still carries the room's timers and outbox
  retries.
- An eviction can lose the unpersisted ticks and the in-memory schedule. On the
  first wake the room reloads the persisted tick, sends every still-connected
  client a `FULL_STATE` (clients order ticked patches by tick, so they must reset
  their high-water mark), and ticking resumes one period after the watchdog
  wake. Room code does not change.

## Consequences

- The runtime stays driver-agnostic: `RoomRuntime::tick_if_due`,
  `tick_deadline()` and a tick watchdog. `Host::loops_ticks()` tells the runtime
  to leave the tick deadline out of the alarm schedule (the Durable Object
  adapter returns `true`; the test harness keeps the alarm driver).
- Tick rates below 5 Hz and above 20 Hz stay compile errors, with the >20 message
  naming Handoff.
- `hearth-mp cost --ticked` models the loop driver: duration for the ticking
  session, one watchdog alarm per room per five minutes, and no per-tick request
  charge.
- A deployment still has one alarm per room; the loop is per-room in-memory work
  that runs only while that room is ticking.

## Validation

The Task 4 soak test (`node soak.mjs`, 8 clients, 300 s) checks the ticked
contract end to end against a deployed arena room:

- **Deployed soak (2026-10-25, `https://hearth-arena.lee-0db.workers.dev`, 20 Hz,
  `--secs 300 --bar-ms 12.5`):** passed — 8 clients, room tick 6055, every
  `PATCH` carried `tick` and `t`, all clients ended within one tick of each other
  (6048), adjacent-tick interval error p50/p95/p99 **0/0/0 ms** against the
  12.5 ms bar, the admin inspect showed the room stopped ticking after every
  player left (tick 6055 unchanged 10 s apart), and the synced avatars and coins
  matched the arena sample.
- **Local soak (2026-10-25, `wrangler dev`, 20 Hz, `--secs 45`):** passed —
  room tick 918, adjacent-tick interval p50 4 ms / p95 18 ms / p99 20 ms;
  same stop-ticking check. Local p99 is the host timer quantum, not production.

The local soak and the Godot prediction smoke test are the correctness checks:
they exercise the arena room (20 Hz, buffered intents, catch-up, persist every
20, loop driver, watchdog) over a real WebSocket server, plus interpolation and
prediction on the Godot clients.
