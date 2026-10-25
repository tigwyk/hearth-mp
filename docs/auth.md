# Hearth auth providers

Hearth never sees a password and a room never sees a credential. Every provider
is a pure *verify* step on the gateway that turns an external login into an
[`Identity`](#player-id-namespaces), and the gateway then mints the same 24-hour
Hearth session token for it. Rooms only ever receive the player id, the provider
name and (optionally) a display name.

```
client ──credential──▶ POST /auth/{provider} ──verify──▶ Identity ──▶ Hearth token
```

One route serves every provider:

```
POST /auth/{provider}
Content-Type: application/json
<provider-specific body, see below>

200 {"token": "<24h token>", "player_id": "steam_7656…", "display_name": "Ann"}
```

The token is what the client uses from then on: `Authorization: Bearer <token>`
on Hearth HTTP calls, and `?token=<token>` on the WebSocket upgrade. Tokens are
HS256 with the deployment's `HEARTH_SECRET` (32 bytes minimum) and last 24 hours;
there are no refresh tokens, so a client re-authenticates when its token expires.

## Errors

| Status | Meaning | Body |
| --- | --- | --- |
| 404 | Provider not enabled in this deployment | `{"error":"provider not enabled"}` |
| 400 | The request body is missing a field or is malformed | `{"error":"<reason>"}` |
| 401 | The credential was verified as invalid | `{"error":"<reason>"}` |
| 502 | The provider's upstream is down, slow or answered nonsense | `{"error":"<provider> unavailable"}` |

Upstream calls give up after 5 s. A 5xx, a timeout, or a non-JSON answer is a
502; a 4xx answer is a 401. A Hearth token is only minted from a **verified OK**,
never from an upstream error or an unreadable response. A provider whose secret
is missing is a deployment error and also answers 502 "unavailable" — the gateway
never says which secret is missing.

## Player ID namespaces

Providers cannot mint ids outside their own namespace, and token verification
re-checks the prefix, so a `steam_…` id can never appear in a `jwt_` token:

| Provider | Namespace | Example |
| --- | --- | --- |
| Anonymous device id | `anon_` | `anon_9f2c…` |
| Signed JWT from your backend | `jwt_` | `jwt_user-123` |
| Steam session ticket | `steam_` | `steam_76561198000000000` |
| itch.io API key | `itch_` | `itch_123456` |
| Discord OAuth code | `discord_` | `discord_80351110224678912` |

The subject of a namespaced id is restricted to `A-Z a-z 0-9 _ . : @ -` and at
most 128 characters. Display names are trimmed to at most 64 Unicode scalar
values with controls and line/paragraph separators stripped, so a name can never
inject a header; the gateway forwards it percent-encoded in
`X-Hearth-Display-Name`.

## Anonymous

Enabled by default; the only provider that needs no external service.

- **Enable:** `HEARTH_AUTH_ANONYMOUS` (default `true`).
- **Client sends:** `{"device_id": "<8 to 128 chars>"}` — a per-device identity the
  SDK persists. The id is `anon_` + `HMAC-SHA256(HEARTH_SECRET, "anon:" + device_id)`,
  so it is stable but unguessable.

## JWT (your own backend)

For accounts you already own: your backend signs a short-lived JWT and the
client hands it to Hearth.

- **Config:**
  - `HEARTH_JWT_ISS`, `HEARTH_JWT_AUD` — required issuer and audience.
  - `HEARTH_JWT_JWKS_URL` — a JWKS endpoint (RS256/ES256). Preferred.
  - or `HEARTH_JWT_SECRET` — an HS256 shared secret of at least 32 bytes, stored
    with `wrangler secret put HEARTH_JWT_SECRET`.
  - `HEARTH_JWT_NAME_CLAIM` — optional claim to read the display name from,
    for example `name`.
- **Client sends:** `{"jwt": "<compact JWS>"}`.
- **Claims:** `exp` is required (60 s leeway), `nbf` is honoured when present,
  `iss`/`aud` must match the config, and `sub` is required. The algorithm is
  bound to the key: HS256 keys accept only HS256, JWKS keys accept only the
  algorithm their JWK declares (or the `kty` implies), and `alg: none` is always
  rejected.

## Steam

Players prove ownership of a Steam account with a session ticket from
`GetAuthTicketForWebApi`.

- **Config:**
  - `HEARTH_STEAM_APP_ID` — your Steam app id; setting it enables the provider.
  - `HEARTH_STEAM_KEY` — the publisher Web API key, stored with
    `wrangler secret put HEARTH_STEAM_KEY`.
  - `HEARTH_STEAM_IDENTITY` — the identity string requested with the ticket;
    default `hearth`. It must match the string the client passes to Steam.
  - `HEARTH_STEAM_ALLOW_BANNED` — when `1`/`true`, tickets owned by VAC- or
    publisher-banned accounts are accepted; default rejects them.
- **Client sends:** `{"ticket": "<hex>"}`, at most 4096 hex characters.
- **Getting the ticket (Steamworks):** call
  `SteamUser.GetAuthTicketForWebApi("hearth")`, wait for the
  `GetAuthTicketForWebApiResponse_t` callback, and hex-encode the ticket bytes.
  The same `"hearth"` string must be configured as `HEARTH_STEAM_IDENTITY`.
- **Upstream:** one `GET` to
  `https://partner.steam-api.com/ISteamUserAuth/AuthenticateUserTicket/v1/` with
  `key`, `appid`, `ticket` and `identity`. The ticket is accepted when
  `response.params.result == "OK"`; `response.error.errordesc` is a 401. The API
  key only appears in the outbound URL and is never echoed in an error or a log.

## itch.io

For games distributed on itch.io: the player's itch API key identifies them.

- **Config:** `HEARTH_AUTH_ITCH=1` enables the provider. There is no server-side
  secret: the API key is the credential.
- **Client sends:** `{"api_key": "<key>"}`. The itch app exposes the player's key
  as `ITCHIO_API_KEY`; SDKs read that environment variable when no key is passed.
- **Upstream:** `GET https://itch.io/api/1/jwt/me` with
  `Authorization: Bearer <key>`. A 200 with `user.id` is an identity; the display
  name is `display_name`, falling back to `username`. itch's `errors` array or a
  401 is a rejected key.

## Discord

For Discord Activities and any client that can obtain an OAuth2 authorization
code (the gateway does not run the browser redirect itself, only the exchange).

- **Config:**
  - `HEARTH_DISCORD_CLIENT_ID` and `HEARTH_DISCORD_SECRET` — the application
    credentials; `HEARTH_DISCORD_SECRET` is stored with
    `wrangler secret put HEARTH_DISCORD_SECRET`. Both are required to enable the
    provider.
  - `HEARTH_DISCORD_REDIRECT_URI` — optional; sent with the token exchange when
    the code came from your own redirect flow. Discord Activities do not need it.
- **Client sends:** `{"code": "<oauth2 authorization code>"}`.
- **Upstream:** a form `POST https://discord.com/api/oauth2/token` (`client_id`,
  `client_secret`, `grant_type=authorization_code`, `code`, optional
  `redirect_uri`), then `GET https://discord.com/api/users/@me` with the returned
  access token as a bearer credential. Only then is the identity minted; the
  access token is used once and thrown away. The display name is `global_name`,
  falling back to `username`.

## Display names

Display names are optional and provider-specific: Steam has none (querying
`GetPlayerSummaries` is deliberately out of scope), itch and Discord carry one.
The room sees `display_name` and `provider` beside the `player_id`, so a UI can
show "Ann (Steam)" without knowing how Ann signed in.
