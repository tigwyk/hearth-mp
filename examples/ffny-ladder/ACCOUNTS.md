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

- **Login is Discord OAuth** when `DISCORD_CLIENT_ID` is set (see below), otherwise a dev sign-in (pick a name,
  no password) that is on only when `DEV_LOGIN=1`. The game's start/poll handshake is the same either way.
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

## Running for real

This is deployed for the Def Jam: Fight for NY community at **https://ladder.ffny.live/**
(a custom domain on a Worker; the page at `/` is `src/board.html`, which reads `/api/leaderboard`
every 15 seconds). Checked against the live Worker:

- The game's own sign-in handshake over HTTP (`RECOMP_ACCOUNT_SERVER=127.0.0.1:8787`, `wrangler dev`).
- The Go relay (`netplay/relay`) running a full ranked match against it: both tickets redeemed with
  `X-Internal-Token`, the pairing posted, both players' reports settled (1500 vs 1500 became 1662 vs 1338).

The whole backend is one Durable Object with SQLite, on the free plan. Idle, it is not billed
(see `docs/cost.md`). Login: see "Discord sign-in".

## Discord sign-in

1. In the [Discord developer portal](https://discord.com/developers/applications) make an application. Under
   OAuth2 add the redirect `https://<your host>/auth/discord/callback` (the host the game talks to).
2. Set the Worker up:

```sh
wrangler secret put DISCORD_CLIENT_SECRET
wrangler deploy --var DISCORD_CLIENT_ID:<client id> --var PUBLIC_URL:https://<your host>
```

`PUBLIC_URL` is the address registered in step 1, without the path. With `DISCORD_CLIENT_ID` set the dev
page is gone: `/auth/discord/start` sends the browser to Discord (scope `identify`), the callback trades
the code for the Discord user, and the Durable Object opens the session the game is polling for. The
Discord token is not kept. One Discord user is one account, so a returning player keeps their rating.

An older deployment needs its database recreated once: the `accounts` table gained a `discord` column.

