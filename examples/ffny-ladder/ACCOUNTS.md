# The account service on Durable Objects

`src/accounts.rs` is a drop-in for the FFNY account service (`netplay/relay/accounts`,
the Go program behind play.defjam.online), for **ranked play only**. The game and the
relay talk to it exactly as they talk to the Go one, so neither needs a new protocol:

| Caller | Endpoints |
|---|---|
| Game | `/auth/discord/start`, `/auth/poll`, `/auth/logout`, `/api/me`, `/api/ranked/ticket`, `/api/matches`, `/api/me/stats?format=text`, `/api/leaderboard` |
| Relay | `/internal/ranked/redeem`, `/internal/ranked/pairing` (header `X-Internal-Token`) |

One Durable Object (`ACCOUNTS`, name `global`) holds accounts, sessions, tickets, pairings
and reports in SQLite. A ranked game counts when the relay paired the two accounts and both
games report the same result; then Glicko-2 moves both ratings.

## Not the same as the Go service

- **Login is a dev sign-in** (pick a name, no password), on only when `DEV_LOGIN=1`. The
  game's start/poll handshake is kept, so Discord OAuth would replace only the start page.
- Ranked games only: no history (`recent`), no favorite fighter, no streaks, no bug reports.
- Not ported: the daily cap on repeat pairings, the 3-hour set window rules, "left" and
  "crashed" reports, account deletion beyond the basics, rate limits.
- The ranked queue stays in the Go relay; this service never sees it.

## Run it locally

```sh
cd examples/ffny-ladder
printf 'HEARTH_SECRET=%s\nINTERNAL_TOKEN=%s\nDEV_LOGIN=1\n' "$(openssl rand -hex 32)" "$(openssl rand -hex 16)" > .dev.vars
wrangler dev --port 8787
```

Point the game at it (plain HTTP, no game change needed):

```sh
RECOMP_ACCOUNT_SERVER=127.0.0.1:8787
```

and the relay (`-accounts-internal` is the existing flag; the token is new):

```sh
ACCOUNTS_INTERNAL_TOKEN=<the INTERNAL_TOKEN above> relay ... -accounts-internal http://127.0.0.1:8787
```

## Deploy it

```sh
wrangler login
wrangler secret put HEARTH_SECRET      # 32+ bytes
wrangler secret put INTERNAL_TOKEN     # shared with the relay
wrangler deploy --var DEV_LOGIN:1      # test servers only
```

A deployed Worker is HTTPS, so the game build takes `RECOMP_ACCOUNT_SERVER=https://<name>.workers.dev`
(this needs the `account.c` change that accepts `https://`).
