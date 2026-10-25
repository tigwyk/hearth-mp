# Cost estimate

`hearth cost` estimates your monthly Cloudflare bill before launch: pick a CCU
target, a room size and an intent rate, and it prints the billable dimensions,
the included allowances, and the dollar total.

It is **offline and pure**: no Cloudflare account, no API call, no network. Every
price and included allowance comes from a versioned table checked into the repo
(`crates/hearth-cli/pricing.toml`), so the same command gives the same answer on
any machine and a price change is a data edit, not a code change.

```sh
hearth cost --ccu 1000 --room-size 4 --intents 2
```

```
ITEM                         QUANTITY           BILLABLE   USD
workers requests             120000 requests    0          0.00
durable objects requests     262860000 requests 261860000  39.28
durable objects duration     84096000 GB-s      83696000   1046.20
durable objects rows written 5256000000 rows    5206000000 5206.00
durable objects storage      0.001000 GB-month  0          0.00
------------------------------------------------------------------
TOTAL                                                      6296.48
prices as of 2026-10-25
assumptions:
  - S = 2628000 seconds per month (30.4-day average month)
  - rooms = ccu / room_size = 1000 / 4 = 250.00
  - ...
```

`prices as of` is the `as_of` date of the table that produced the estimate.
`assumptions` reprints every intermediate number, with your inputs substituted,
so a surprising total can be audited line by line.

## Flags

| Flag | Meaning |
| --- | --- |
| `--ccu <n>` | Target concurrent connected players. Required, > 0. |
| `--room-size <n>` | Players per room. Required, > 0. |
| `--intents <n>` | Player intents per second, per player. Required, ≥ 0. |
| `--ticked <hz>` | Model a ticked-mode room at this tick rate instead of event mode. |
| `--persist-every <ticks>` | Ticked mode only: persist state once every N ticks. Default 20. |
| `--state-kb <kb>` | Persisted state per room in KB. Default 4. |
| `--sessions <n>` | Sessions per player per day. Default 2. |
| `--pricing <file>` | Use a different pricing table (next month's prices, an Enterprise rate, a what-if). |
| `--json` | Print the estimate as JSON instead of the table. |

Bad input exits with code 2 and names the flag; an unreadable `--pricing` file
exits 1.

## The model

All quantities are monthly. `S` is `seconds_per_month` (30.4 days).

```
rooms            = ccu / room_size
msgs_per_player  = min(intents, 20)                  # SDK batching cap
ws_msgs          = ccu * msgs_per_player * S         # incoming WebSocket messages
ticks            = rooms * hz * S                    # ticked mode only: one alarm per tick
connects         = ccu * sessions * 30
do_requests      = ws_msgs / 20 + connects + ticks   # 20 incoming WS msgs = 1 request
worker_requests  = connects + connects               # auth call + WebSocket upgrade
room_msg_rate    = room_size * msgs_per_player
active_fraction  = event: min(1, room_msg_rate * 10) # 10 s idle window before hibernation
                   ticked: 1
gb_s             = rooms * active_fraction * S * 0.128   # billed at 128 MB per object
rows_written     = event:  rooms * room_msg_rate * S     # one state row per handler pass
                   ticked: rooms * (hz / persist_every) * S
storage_gb       = rooms * state_kb / 1_000_000

billable(x)      = max(0, x - included)
usd              = billable / 1_000_000 * price_per_million   # storage: * $/GB-month
total            = $5 Workers Paid base + sum(usd)
```

Two consequences worth internalising:

- **Event-mode cost is driven by incoming messages, not by CCU.** A room is billed
  for duration only while it cannot hibernate; a quiet room drops out of memory
  after ~10 idle seconds. That is why `active_fraction` falls to 0 when nobody
  sends intents, and why `--intents 0` is a valid "rooms just sit there" estimate.
- **Batching is the cost lever.** The SDK batches intents into at most 20 frames
  per second per player, and Cloudflare bills 20 incoming WebSocket messages as
  one request. Sending more than 20 intents/s does not add cost in this model —
  and sending one intent per frame instead of batches multiplies it. Keep the
  SDK's batching on.

`--ticked <hz>` models the ticked-mode runtime planned for v0.4 (not shipped yet):
every tick is an alarm invocation, the room never hibernates, and state persists
every `--persist-every` ticks (`setAlarm` and each persisted row are rows
written).

## The pricing table

`crates/hearth-cli/pricing.toml` is embedded in the binary at build time. It holds
`as_of`, the `sources` URLs, and every price and included allowance:

- Workers Paid: `$5`/month base, 10M requests/month included, `$0.30`/million after.
- Durable Objects (SQLite): 1M requests/month included, `$0.15`/million after;
  400k GB-s/month included, `$12.50`/million GB-s after, billed for the full
  **128 MB** each object is allocated; 50M rows written/month included,
  `$1.00`/million after; 5 GB-month storage included, `$0.20`/GB-month after.
- Incoming WebSocket messages bill at **20:1** to requests; outgoing messages and
  protocol pings are free.

Current as of **2026-10-25**, refreshed from
[Workers pricing](https://developers.cloudflare.com/workers/platform/pricing/) and
[Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/).
Prices change: when Cloudflare updates either page, edit `pricing.toml`, set
`as_of` to the update date, and re-run `cargo test -p hearth-cli --test cost`.
`--pricing <file>` previews a table without touching the shipped one.

## What the estimate deliberately leaves out

- **Worker CPU time.** 30M CPU ms/month are included, then `$0.02`/million. The
  estimate assumes your handlers stay inside the allowance; a CPU-heavy room mode
  is a v0.4 concern.
- **Durable Object rows read.** 25 billion/month included; Hearth's own state
  table is small and this has never been the binding limit.
- **Rounding.** Cloudflare rounds each billable unit up before charging (500k
  billable GB-s is billed as 1M). The estimate does not round, so small overages
  are understated by less than one billing unit.
- **KV, D1, R2, Queues and any other product** you add yourself. A default Hearth
  deployment uses none of them.
- **Additional Worker requests** beyond two per session (auth + WebSocket
  upgrade). Matchmaking, lobby and admin traffic are not modelled.
- **Egress.** Data transfer out of Workers is free, so it is not a line item.
- **Game-client hosting.** Wherever you host the Godot or Unity build is not
  Cloudflare and not counted here.
- **The Workers Free plan.** The model assumes the Workers Paid plan, because
  Durable Objects in production want it and the `$5` base is the total's floor.

## Reading `--json`

`--json` prints the same `Estimate` the table is rendered from:

```json
{
  "items": [
    {"name": "workers requests", "quantity": 120000.0, "unit": "requests", "billable": 0.0, "usd": 0.0},
    {"name": "durable objects requests", "quantity": 262860000.0, "unit": "requests", "billable": 261860000.0, "usd": 39.279},
    {"name": "durable objects duration", "quantity": 84096000.0, "unit": "GB-s", "billable": 83696000.0, "usd": 1046.2},
    {"name": "durable objects rows written", "quantity": 5256000000.0, "unit": "rows", "billable": 5206000000.0, "usd": 5206.0},
    {"name": "durable objects storage", "quantity": 0.001, "unit": "GB-month", "billable": 0.0, "usd": 0.0}
  ],
  "total_usd": 6296.479,
  "assumptions": ["..."]
}
```

Note that `total_usd` includes the `$5` plan base, which is not an item. That is
the number to compare across CCU and room-size settings.
