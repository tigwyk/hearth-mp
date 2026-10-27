# ffny-ladder

A ranked ladder for a peer-to-peer game mod, as one Cloudflare Durable Object
(SQLite inside). Built for the FFNY online addon for the Def Jam: Fight for NY
recompilation, but nothing in it is specific to that game.

The fights run on the players' machines. This Worker does the part a mod needs a
server for: a ranked queue, pairing, settlement of agreed results with Glicko-2,
and a leaderboard. While nobody plays, nothing runs and nothing is billed.

```sh
cp .dev.vars.example .dev.vars   # or: wrangler secret put HEARTH_SECRET
hearth-mp dev                    # or: wrangler dev
curl -X POST localhost:8787/auth/anonymous -d '{"device_id":"a"}'   # -> token
curl -X POST localhost:8787/ladder/search -H "authorization: Bearer $TOKEN"
```

| Route | |
|---|---|
| `POST /ladder/search` | Join the queue, or poll. Returns `searching` (with the current rating window) or `matched` (pairing id and opponent). Poll every 2-3 s. |
| `DELETE /ladder/search` | Leave the queue. |
| `POST /ladder/report` | `{pairing, result: win\|loss\|draw}`. Counts only when both paired players report and agree. |
| `GET /ladder/me` | Your rating and record. |
| `GET /ladder/top?limit=` | The leaderboard (no token). |

The window starts at +-100 rating points and widens by 25 per second waited.

Not here yet: the accept/decline step, the daily cap on repeat pairings, rate
limits on `/ladder/*`, seasons. See the FFNY relay's `docs/ranked-design.md`.

`cargo test -p ffny-ladder` runs the rating math (against the worked example in
Glickman's paper) and the service in the native test world.
