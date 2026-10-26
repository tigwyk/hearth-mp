//! HTTP surface: auth providers, room minting, schema lookup, WebSocket routing,
//! quick-join, the lobby listing and the caller's own player view.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hearth_matchmaker::{MmReply, MmRpc};
use hearth_protocol::Value;
use hearth_server::call::{Envelope, Target};
use hearth_server::handoff::{Redeemed, SessionId};
use hearth_server::worker::call_object;
use serde::Deserialize;
use worker::{Env, Headers, Method, Request, RequestInit, Response, Result};

use crate::auth::{issue_token, mint_room_name, valid_room_name, verify_token, Identity};
use crate::providers::{self, AuthConfig, WorkerFetcher};
use crate::ratelimit::{self, Limit, Scope};

/// One room type this Worker serves.
pub struct RoomRoute {
    /// URL segment, e.g. `"tictactoe"`.
    pub room_type: &'static str,
    /// Durable Object binding name, e.g. `"TICTACTOE"`.
    pub binding: &'static str,
    /// The session container class's Durable Object binding, when this room type
    /// uses `allocator = "container"`. It enables
    /// `GET /handoff/{sid}/connect`, the WebSocket proxy into the container.
    pub container: Option<&'static str>,
    /// The state's schema hash, which clients echo in `HELLO`.
    pub schema_hash: fn() -> String,
    /// The room's seat capacity, passed to the matchmaker so a minted room is
    /// filled before a second one is created.
    pub max_clients: u8,
}

/// Where `GET /me` reads the caller's own player data from.
pub struct PlayerRoute {
    /// Durable Object binding name, e.g. `"TTT_PLAYER"`.
    pub binding: &'static str,
}

/// Everything a deployment's gateway needs to know.
pub struct GatewayConfig {
    pub rooms: &'static [RoomRoute],
    /// `None` disables `GET /me` (it returns 501).
    pub player: Option<PlayerRoute>,
    /// `false` disables `POST /match` and `GET /lobby` (both return 501).
    pub matchmaker: bool,
    /// Which login providers `POST /auth/{provider}` accepts.
    pub auth: AuthConfig,
    /// Where the per-IP and per-player rate limits live.
    pub limits: ratelimit::Limits,
}

const SECRET_BINDING: &str = "HEARTH_SECRET";
const MIN_SECRET_BYTES: usize = 32;
const SECRET_HELP: &str =
    "HEARTH_SECRET is not set (min 32 bytes); run: wrangler secret put HEARTH_SECRET";
const PLAYER_HEADER: &str = "X-Hearth-Player-Id";
/// The room's type and name, as one `"<type>/<name>"` header. The gateway builds
/// this itself, so a player can never choose it.
const ROOM_HEADER: &str = "X-Hearth-Room";
/// Which `(room_type, region)` matchmaker pools this room, when its name was
/// minted by one.
const MATCHMAKER_HEADER: &str = "X-Hearth-Matchmaker";
/// The verified provider, so a room can tell an anonymous player from a signed-in
/// one without ever seeing a credential.
const PROVIDER_HEADER: &str = "X-Hearth-Provider";
/// The verified display name, percent-encoded so CR/LF can never reach a header.
const DISPLAY_NAME_HEADER: &str = "X-Hearth-Display-Name";
/// The Durable Object's internal connect path. The gateway builds the URL, so no
/// client path or query reaches the room.
const CONNECT_URL: &str = "https://room/__hearth/connect";
/// The player DO's read-only view path.
const PLAYER_VIEW_URL: &str = "https://player/__hearth/view";
const MATCHMAKER_BINDING: &str = "HEARTH_MATCHMAKER";
/// The dedicated server's session secret, the only credential for a handoff
/// redeem or report (there is no player token on these routes).
const SESSION_SECRET_HEADER: &str = "X-Hearth-Session-Secret";
/// The room's internal handoff paths; the gateway builds these URLs itself.
const REDEEM_PATH: &str = "/__hearth/redeem";
const REPORT_PATH: &str = "/__hearth/report";
/// The gateway's ticket-only redeem path on the room: no session secret exists
/// on the client side of a container handoff, so the single-use ticket is the
/// only credential and the gateway is the only caller.
const REDEEM_TRUSTED_PATH: &str = "/__hearth/redeem_trusted";
/// The session container object's WebSocket proxy path.
const CONTAINER_PROXY_URL: &str = "https://container/__hearth/proxy/connect";
/// The redeemed player's claims as base64url JSON, forwarded to the dedicated
/// server. It is a header, so it cannot contain JSON's braces/quotes verbatim.
const CLAIMS_HEADER: &str = "X-Hearth-Claims";

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
struct TicketQuery {
    ticket: Option<String>,
}

#[derive(Deserialize)]
struct MatchBody {
    pool: Option<String>,
}

#[derive(Deserialize)]
struct LobbyQuery {
    pool: Option<String>,
    limit: Option<u32>,
}

enum Route<'a> {
    Auth(&'a str),
    MintRoom(&'a str),
    Schema(&'a str),
    Connect(&'a str, &'a str),
    Match(&'a str),
    Lobby(&'a str),
    Me,
    HandoffRedeem(&'a str),
    HandoffReport(&'a str),
    HandoffConnect(&'a str),
}

/// Dispatch one request. `cfg` is the Worker's static gateway configuration.
pub async fn serve(req: Request, env: Env, cfg: &GatewayConfig) -> Result<Response> {
    // No default secret: a missing or short binding is a deployment error, not a 401.
    let secret = env
        .secret(SECRET_BINDING)
        .map(|secret| secret.to_string())
        .unwrap_or_default();
    if secret.len() < MIN_SECRET_BYTES {
        return Response::error(SECRET_HELP, 500);
    }
    let secret = secret.as_bytes();

    let path = req.path();
    let Some(route) = parse_route(&req.method(), &path) else {
        return Response::error("not found", 404);
    };
    // Per-IP first, on every route. Many players can share one IP (CGNAT, a
    // school), so this limit is generous; the per-player one is the tight one.
    if let Some(key) = ratelimit::key(Scope::Ip, &client_ip(&req)?, None) {
        let limit =
            ratelimit::check(&env, cfg.limits.ip_binding, &key, cfg.limits.ip_period_s).await;
        if let Limit::Limited { retry_after_s } = limit {
            return ratelimit::limited(Scope::Ip, retry_after_s);
        }
    }
    match route {
        Route::Auth(provider) => auth(req, &env, secret, cfg, provider).await,
        Route::MintRoom(room_type) => mint_room(req, &env, secret, cfg, room_type).await,
        Route::Schema(room_type) => schema(cfg.rooms, room_type),
        Route::Connect(room_type, name) => connect(req, &env, secret, cfg, room_type, name).await,
        Route::Match(room_type) => match_room(req, &env, secret, cfg, room_type).await,
        Route::Lobby(room_type) => lobby(req, &env, secret, cfg, room_type).await,
        Route::Me => player_view(req, &env, secret, cfg).await,
        Route::HandoffRedeem(sid) => handoff_redeem(req, &env, cfg, sid).await,
        Route::HandoffReport(sid) => handoff_report(req, &env, cfg, sid).await,
        Route::HandoffConnect(sid) => handoff_connect(req, &env, cfg, sid).await,
    }
}

/// `CF-Connecting-IP`, which Cloudflare always sets. Locally it may be absent,
/// which just means no per-IP limit can be keyed.
fn client_ip(req: &Request) -> Result<String> {
    Ok(req.headers().get("CF-Connecting-IP")?.unwrap_or_default())
}

/// The per-player limit, checked after a route resolves the caller's token.
async fn player_limit(
    env: &Env,
    cfg: &GatewayConfig,
    player: Option<&Identity>,
) -> Result<Option<Response>> {
    let Some(player) = player else {
        return Ok(None);
    };
    let Some(key) = ratelimit::key(Scope::Player, "", Some(&player.player_id)) else {
        return Ok(None);
    };
    let limit = ratelimit::check(
        env,
        cfg.limits.player_binding,
        &key,
        cfg.limits.player_period_s,
    )
    .await;
    match limit {
        Limit::Limited { retry_after_s } => {
            Ok(Some(ratelimit::limited(Scope::Player, retry_after_s)?))
        }
        _ => Ok(None),
    }
}

fn parse_route<'a>(method: &Method, path: &'a str) -> Option<Route<'a>> {
    let mut parts = path.trim_start_matches('/').split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("auth"), Some(provider), None, None) if *method == Method::Post => {
            Some(Route::Auth(provider))
        }
        (Some("rooms"), Some(room_type), None, None) if *method == Method::Post => {
            Some(Route::MintRoom(room_type))
        }
        (Some("schema"), Some(room_type), None, None) if *method == Method::Get => {
            Some(Route::Schema(room_type))
        }
        (Some("rooms"), Some(room_type), Some(name), None) if *method == Method::Get => {
            Some(Route::Connect(room_type, name))
        }
        (Some("match"), Some(room_type), None, None) if *method == Method::Post => {
            Some(Route::Match(room_type))
        }
        (Some("lobby"), Some(room_type), None, None) if *method == Method::Get => {
            Some(Route::Lobby(room_type))
        }
        (Some("handoff"), Some(sid), Some("redeem"), None) if *method == Method::Post => {
            Some(Route::HandoffRedeem(sid))
        }
        (Some("handoff"), Some(sid), Some("report"), None) if *method == Method::Post => {
            Some(Route::HandoffReport(sid))
        }
        (Some("handoff"), Some(sid), Some("connect"), None) if *method == Method::Get => {
            Some(Route::HandoffConnect(sid))
        }
        (Some("me"), None, None, None) if *method == Method::Get => Some(Route::Me),
        _ => None,
    }
}

/// `POST /auth/{provider}`: verify a credential and mint a Hearth token for it.
///
/// The provider never sees the deployment secret and the client never sees a
/// provider credential after this call: the response is the only way forward.
async fn auth(
    mut req: Request,
    env: &Env,
    secret: &[u8],
    cfg: &GatewayConfig,
    provider: &str,
) -> Result<Response> {
    let body: Value = req.json().await.unwrap_or(Value::Null);
    let identity = match providers::verify(&cfg.auth, provider, &body, env, &WorkerFetcher).await {
        Ok(identity) => identity,
        Err(err) => {
            return Response::from_json(&serde_json::json!({ "error": err.message() }))
                .map(|response| response.with_status(err.status()));
        }
    };
    let token = issue_token(secret, &identity, now_s());
    Response::from_json(&serde_json::json!({
        "token": token,
        "player_id": identity.player_id,
        "display_name": identity.display_name,
    }))
}

/// Mints a room name without touching any Durable Object.
async fn mint_room(
    req: Request,
    env: &Env,
    secret: &[u8],
    cfg: &GatewayConfig,
    room_type: &str,
) -> Result<Response> {
    if !cfg.rooms.iter().any(|route| route.room_type == room_type) {
        return Response::error("unknown room type", 404);
    }
    let Some(token) = bearer_token(&req) else {
        return Response::error("missing bearer token", 401);
    };
    let Ok(identity) = verify_token(secret, &token, now_s()) else {
        return Response::error("invalid or expired token", 401);
    };
    if let Some(response) = player_limit(env, cfg, Some(&identity)).await? {
        return Ok(response);
    }
    Response::from_json(&serde_json::json!({ "room": mint_room_name() }))
}

fn schema(routes: &[RoomRoute], room_type: &str) -> Result<Response> {
    let Some(route) = routes.iter().find(|route| route.room_type == room_type) else {
        return Response::error("unknown room type", 404);
    };
    Response::from_json(&serde_json::json!({ "schema_hash": (route.schema_hash)() }))
}

/// Resolve a handoff session id to the room that owns it and the route that
/// reaches its Durable Object. The room is decoded from the sid; the caller
/// never supplies a room type.
fn handoff_route<'a>(
    cfg: &'a GatewayConfig,
    sid_text: &str,
) -> Option<(&'a RoomRoute, hearth_server::RoomName)> {
    let room = SessionId(sid_text.to_owned()).room()?;
    let route = cfg
        .rooms
        .iter()
        .find(|route| route.room_type == room.room_type)?;
    Some((route, room))
}

/// `POST /handoff/{sid}/redeem`: the dedicated server exchanges a player's
/// single-use ticket. The session secret is the credential; there is no player
/// token on this route.
async fn handoff_redeem(
    mut req: Request,
    env: &Env,
    cfg: &GatewayConfig,
    sid_text: &str,
) -> Result<Response> {
    #[derive(Deserialize)]
    struct RedeemBody {
        ticket: String,
    }
    let Some((route, room)) = handoff_route(cfg, sid_text) else {
        return Response::error("unknown handoff session", 404);
    };
    let Some(secret) = req.headers().get(SESSION_SECRET_HEADER)? else {
        return Response::error("missing X-Hearth-Session-Secret header", 403);
    };
    let Ok(body) = req.json::<RedeemBody>().await else {
        return Response::error("malformed redeem body", 400);
    };
    let forward = serde_json::json!({
        "sid": sid_text,
        "secret": secret,
        "ticket": body.ticket,
    })
    .to_string();
    forward_handoff(env, route.binding, &room.name, REDEEM_PATH, forward).await
}

/// `POST /handoff/{sid}/report`: one lifecycle report from the dedicated
/// server, forwarded with the session secret.
async fn handoff_report(
    mut req: Request,
    env: &Env,
    cfg: &GatewayConfig,
    sid_text: &str,
) -> Result<Response> {
    let Some((route, room)) = handoff_route(cfg, sid_text) else {
        return Response::error("unknown handoff session", 404);
    };
    let Some(secret) = req.headers().get(SESSION_SECRET_HEADER)? else {
        return Response::error("missing X-Hearth-Session-Secret header", 403);
    };
    let Ok(report) = req.json::<Value>().await else {
        return Response::error("malformed report body", 400);
    };
    let forward = serde_json::json!({
        "sid": sid_text,
        "secret": secret,
        "report": report,
    })
    .to_string();
    forward_handoff(env, route.binding, &room.name, REPORT_PATH, forward).await
}

/// POST one handoff request to the room's Durable Object and pass its answer
/// (status and JSON body) straight back to the dedicated server.
async fn forward_handoff(
    env: &Env,
    binding: &str,
    room_name: &str,
    path: &str,
    body: String,
) -> Result<Response> {
    let (status, text) = post_room_json(env, binding, room_name, path, body).await?;
    handoff_response(status, text)
}

/// POST one JSON body to a room object and return its status with the response
/// body text. The shared half of the dedicated-server routes and the container
/// proxy.
async fn post_room_json(
    env: &Env,
    binding: &str,
    room_name: &str,
    path: &str,
    body: String,
) -> Result<(u16, String)> {
    let namespace = env.durable_object(binding)?;
    let id = namespace.id_from_name(room_name)?;
    let stub = id.get_stub()?;
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.into()));
    let request = Request::new_with_init(&format!("https://room{path}"), &init)?;
    let mut incoming = stub.fetch_with_request(request).await?;
    let status = incoming.status_code();
    let text = incoming.text().await?;
    Ok((status, text))
}

/// A room's JSON answer as a gateway response (204 stays empty).
fn handoff_response(status: u16, body: String) -> Result<Response> {
    if status == 204 {
        return Response::empty().map(|response| response.with_status(204));
    }
    let mut response = Response::ok(body)?.with_status(status);
    response
        .headers_mut()
        .set("Content-Type", "application/json")?;
    Ok(response)
}

/// `GET /handoff/{sid}/connect?ticket=<t>`: the WebSocket proxy into a session's
/// container.
///
/// The ticket is redeemed with the room *before* the upgrade is attempted, so a
/// used, expired or foreign ticket answers 403/410 without ever opening a
/// socket. Only then is the upgrade forwarded to the session's container object,
/// with identity headers built from the redeemed player; any client-supplied
/// `X-Hearth-*` header is dropped.
async fn handoff_connect(
    req: Request,
    env: &Env,
    cfg: &GatewayConfig,
    sid_text: &str,
) -> Result<Response> {
    let Some((route, room)) = handoff_route(cfg, sid_text) else {
        return Response::error("unknown handoff session", 404);
    };
    let Some(container_binding) = route.container else {
        return Response::error("container handoff is not enabled for this room type", 404);
    };
    let upgrade = req
        .headers()
        .get("Upgrade")?
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    if !upgrade {
        return Response::error("expected a websocket upgrade", 426);
    }
    let Ok(query) = req.query::<TicketQuery>() else {
        return Response::error("invalid query", 400);
    };
    let Some(ticket) = query.ticket.filter(|ticket| !ticket.is_empty()) else {
        return Response::error("missing ticket", 400);
    };

    // Redeem first: the ticket is the only credential on this path, and a used
    // or expired one must answer before any upgrade.
    let redeem_body = serde_json::json!({ "sid": sid_text, "ticket": ticket }).to_string();
    let (status, body) = post_room_json(
        env,
        route.binding,
        &room.name,
        REDEEM_TRUSTED_PATH,
        redeem_body,
    )
    .await?;
    if status != 200 {
        return handoff_response(status, body);
    }
    let redeemed: Redeemed = match serde_json::from_str(&body) {
        Ok(redeemed) => redeemed,
        Err(_) => return Response::error("room returned a malformed redeem", 502),
    };

    // Build the container's request here instead of forwarding the client's: the
    // container only ever sees this URL, and identity comes from the redeem.
    let headers = Headers::new();
    for (name, value) in req.headers().entries() {
        let lower = name.to_ascii_lowercase();
        if lower == "host" || lower == "authorization" || lower.starts_with("x-hearth-") {
            continue;
        }
        headers.set(&name, &value)?;
    }
    headers.set(PLAYER_HEADER, &redeemed.player_id.0)?;
    headers.set(PROVIDER_HEADER, &redeemed.provider)?;
    if let Some(display_name) = &redeemed.display_name {
        if !display_name.is_empty() {
            headers.set(DISPLAY_NAME_HEADER, &percent_encode(display_name))?;
        }
    }
    let claims = match serde_json::to_vec(&redeemed.claims) {
        Ok(claims) => URL_SAFE_NO_PAD.encode(claims),
        Err(_) => return Response::error("redeemed claims are not JSON", 502),
    };
    headers.set(CLAIMS_HEADER, &claims)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let outgoing = Request::new_with_init(CONTAINER_PROXY_URL, &init)?;

    let namespace = env.durable_object(container_binding)?;
    let id = namespace.id_from_name(sid_text)?;
    let stub = id.get_stub()?;
    stub.fetch_with_request(outgoing).await
}

async fn connect(
    req: Request,
    env: &Env,
    secret: &[u8],
    cfg: &GatewayConfig,
    room_type: &str,
    name: &str,
) -> Result<Response> {
    let Some(route) = cfg.rooms.iter().find(|route| route.room_type == room_type) else {
        return Response::error("unknown room type", 404);
    };
    if !valid_room_name(name) {
        return Response::error("invalid room name", 400);
    }
    let Some(token) = query_token(&req) else {
        return Response::error("missing token", 401);
    };
    let identity = match verify_token(secret, &token, now_s()) {
        Ok(identity) => identity,
        Err(_) => return Response::error("invalid or expired token", 401),
    };
    if let Some(response) = player_limit(env, cfg, Some(&identity)).await? {
        return Ok(response);
    }
    let upgrade = req
        .headers()
        .get("Upgrade")?
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    if !upgrade {
        return Response::error("expected a websocket upgrade", 426);
    }

    // Build the room's request here instead of forwarding the client's: the room
    // only ever sees `https://room/__hearth/connect`, and the player identity and
    // room name come from verified values, never from client headers.
    let headers = Headers::new();
    for (name, value) in req.headers().entries() {
        if name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        headers.set(&name, &value)?;
    }
    headers.set(PLAYER_HEADER, &identity.player_id.0)?;
    headers.set(ROOM_HEADER, &format!("{room_type}/{name}"))?;
    // Both values come from the *verified* token, so a room can trust them; the
    // name is percent-encoded, which makes header injection impossible.
    headers.set(PROVIDER_HEADER, identity.provider)?;
    if let Some(display_name) = &identity.display_name {
        headers.set(DISPLAY_NAME_HEADER, &percent_encode(display_name))?;
    }
    // A matchmaker-minted name tells the room which matchmaker pools it, so it
    // reports its lobby state there too.
    if let Some(region) = mm_room(name) {
        headers.set(MATCHMAKER_HEADER, &format!("{room_type}:{region}"))?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let outgoing = Request::new_with_init(CONNECT_URL, &init)?;

    let namespace = env.durable_object(route.binding)?;
    let id = namespace.id_from_name(name)?;
    let stub = match location_hint(req.cf().and_then(|cf| cf.continent())) {
        Some(hint) => id.get_stub_with_location_hint(hint)?,
        None => id.get_stub()?,
    };
    stub.fetch_with_request(outgoing).await
}

/// `POST /match/{type}`: seat the caller through the matchmaker.
async fn match_room(
    mut req: Request,
    env: &Env,
    secret: &[u8],
    cfg: &GatewayConfig,
    room_type: &str,
) -> Result<Response> {
    if !cfg.matchmaker {
        return not_configured("HEARTH_MATCHMAKER binding not configured");
    }
    let Some(route) = cfg.rooms.iter().find(|route| route.room_type == room_type) else {
        return Response::error("unknown room type", 404);
    };
    let Some(identity) = bearer_player(&req, secret)? else {
        return Response::error("missing or invalid bearer token", 401);
    };
    if let Some(response) = player_limit(env, cfg, Some(&identity)).await? {
        return Ok(response);
    }
    let pool = match req.json::<MatchBody>().await {
        Ok(body) => body.pool.unwrap_or_else(|| "default".to_owned()),
        Err(_) => "default".to_owned(),
    };
    if !valid_pool(&pool) {
        return Response::error("pool must be 1 to 32 characters of [A-Za-z0-9_-]", 400);
    }
    let region = region_for(&req);
    let rpc = MmRpc::QuickJoin {
        player: identity.player_id,
        pool,
        capacity: route.max_clients,
    };
    match call_matchmaker(env, room_type, region, rpc).await {
        Ok(MmReply::Room { name }) => {
            Response::from_json(&serde_json::json!({ "room": name, "region": region }))
        }
        Ok(_) => Response::error("matchmaker returned an unexpected reply", 502),
        Err(message) => Response::from_json(&serde_json::json!({ "error": message }))
            .map(|response| response.with_status(502)),
    }
}

/// `GET /lobby/{type}`: the open rooms in one pool.
async fn lobby(
    req: Request,
    env: &Env,
    secret: &[u8],
    cfg: &GatewayConfig,
    room_type: &str,
) -> Result<Response> {
    if !cfg.matchmaker {
        return not_configured("HEARTH_MATCHMAKER binding not configured");
    }
    if !cfg.rooms.iter().any(|route| route.room_type == room_type) {
        return Response::error("unknown room type", 404);
    }
    let Some(identity) = bearer_player(&req, secret)? else {
        return Response::error("missing or invalid bearer token", 401);
    };
    if let Some(response) = player_limit(env, cfg, Some(&identity)).await? {
        return Ok(response);
    }
    let Ok(query) = req.query::<LobbyQuery>() else {
        return Response::error("invalid query", 400);
    };
    let pool = query.pool.unwrap_or_else(|| "default".to_owned());
    if !valid_pool(&pool) {
        return Response::error("pool must be 1 to 32 characters of [A-Za-z0-9_-]", 400);
    }
    let rpc = MmRpc::List {
        pool,
        limit: query.limit.unwrap_or(20),
    };
    match call_matchmaker(env, room_type, region_for(&req), rpc).await {
        Ok(MmReply::Rooms(rooms)) => Response::from_json(&LobbyResponse {
            rooms: rooms
                .into_iter()
                .map(|room| OpenRoomView {
                    name: room.name,
                    players: room.players,
                    capacity: room.capacity,
                    metadata: room.metadata,
                })
                .collect(),
        }),
        Ok(_) => Response::error("matchmaker returned an unexpected reply", 502),
        Err(message) => Response::from_json(&serde_json::json!({ "error": message }))
            .map(|response| response.with_status(502)),
    }
}

/// `GET /me`: the caller's own player data, computed by the player DO.
async fn player_view(
    req: Request,
    env: &Env,
    secret: &[u8],
    cfg: &GatewayConfig,
) -> Result<Response> {
    let Some(player_route) = cfg.player.as_ref() else {
        return not_configured("player binding not configured");
    };
    let Some(identity) = bearer_player(&req, secret)? else {
        return Response::error("missing or invalid bearer token", 401);
    };
    if let Some(response) = player_limit(env, cfg, Some(&identity)).await? {
        return Ok(response);
    }
    let namespace = env.durable_object(player_route.binding)?;
    let id = namespace.id_from_name(&identity.player_id.0)?;
    let stub = id.get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    let request = Request::new_with_init(PLAYER_VIEW_URL, &init)?;
    let mut response = stub.fetch_with_request(request).await?;
    if response.status_code() != 200 {
        return Response::from_json(&serde_json::json!({
            "error": format!("player object answered {}", response.status_code()),
        }))
        .map(|response| response.with_status(502));
    }
    let body: Value = response.json().await?;
    Response::from_json(&body)
}

#[derive(serde::Serialize)]
struct LobbyResponse {
    rooms: Vec<OpenRoomView>,
}

#[derive(serde::Serialize)]
struct OpenRoomView {
    name: String,
    players: u8,
    capacity: u8,
    metadata: Value,
}

/// `Authorization: Bearer <token>` verified against the deployment secret.
fn bearer_player(req: &Request, secret: &[u8]) -> Result<Option<Identity>> {
    let Some(token) = bearer_token(req) else {
        return Ok(None);
    };
    Ok(verify_token(secret, &token, now_s()).ok())
}

/// Percent-encode every byte that is not unreserved, so the value survives a
/// header unchanged whatever the display name contains.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Call one matchmaker instance (`"{room_type}:{region}"`) and decode its reply.
async fn call_matchmaker(
    env: &Env,
    room_type: &str,
    region: &str,
    rpc: MmRpc,
) -> std::result::Result<MmReply, String> {
    let to = Target::new(MATCHMAKER_BINDING, format!("{room_type}:{region}"));
    let envelope = Envelope {
        msg_id: None,
        from: Target::new("gateway", ""),
        chain: Vec::new(),
        rpc: serde_json::to_value(rpc).map_err(|err| err.to_string())?,
    };
    let value = call_object(env, &to, envelope)
        .await
        .map_err(|err| err.to_string())?;
    serde_json::from_value(value).map_err(|err| format!("matchmaker reply did not decode: {err}"))
}

/// The region of a matchmaker-minted room name: `m_<region>_<16 hex>`.
pub fn mm_room(name: &str) -> Option<&str> {
    hearth_matchmaker::parse_room_name(name).map(|(region, _)| region)
}

fn bearer_token(req: &Request) -> Option<String> {
    req.headers()
        .get("Authorization")
        .ok()
        .flatten()?
        .strip_prefix("Bearer ")
        .map(str::to_owned)
}

fn query_token(req: &Request) -> Option<String> {
    req.query::<TokenQuery>()
        .ok()?
        .token
        .filter(|token| !token.is_empty())
}

fn not_configured(message: &str) -> Result<Response> {
    Response::from_json(&serde_json::json!({ "error": message }))
        .map(|response| response.with_status(501))
}

fn valid_pool(pool: &str) -> bool {
    (1..=32).contains(&pool.len())
        && pool
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The player's continent-level region, or `"any"` when it is unknown.
fn region_for(req: &Request) -> &'static str {
    location_hint(req.cf().and_then(|cf| cf.continent())).unwrap_or("any")
}

// shortcut: continent-level hint only, refine with region/colo when the matchmaker needs it
fn location_hint(continent: Option<String>) -> Option<&'static str> {
    match continent?.as_str() {
        "NA" => Some("enam"),
        "SA" => Some("sam"),
        "EU" => Some("weur"),
        "AS" => Some("apac"),
        "OC" => Some("oc"),
        "AF" => Some("afr"),
        _ => None,
    }
}

fn now_s() -> u64 {
    worker::Date::now().as_millis() / 1000
}
