//! HTTP surface: anonymous auth, room minting, schema lookup, WebSocket routing,
//! quick-join, the lobby listing and the caller's own player view.

use hearth_matchmaker::{MmReply, MmRpc};
use hearth_protocol::{PlayerId, Value};
use hearth_server::call::{Envelope, Target};
use hearth_server::worker::call_object;
use serde::Deserialize;
use worker::{Env, Headers, Method, Request, RequestInit, Response, Result};

use crate::auth::{
    anonymous_player_id, issue_token, mint_room_name, valid_room_name, verify_token,
};

/// One room type this Worker serves.
pub struct RoomRoute {
    /// URL segment, e.g. `"tictactoe"`.
    pub room_type: &'static str,
    /// Durable Object binding name, e.g. `"TICTACTOE"`.
    pub binding: &'static str,
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
/// The Durable Object's internal connect path. The gateway builds the URL, so no
/// client path or query reaches the room.
const CONNECT_URL: &str = "https://room/__hearth/connect";
/// The player DO's read-only view path.
const PLAYER_VIEW_URL: &str = "https://player/__hearth/view";
const MATCHMAKER_BINDING: &str = "HEARTH_MATCHMAKER";

#[derive(Deserialize)]
struct AnonymousBody {
    device_id: String,
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
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
    Anonymous,
    MintRoom(&'a str),
    Schema(&'a str),
    Connect(&'a str, &'a str),
    Match(&'a str),
    Lobby(&'a str),
    Me,
}

/// Dispatch one request. `cfg` is the Worker's static gateway configuration.
pub async fn serve(mut req: Request, env: Env, cfg: &GatewayConfig) -> Result<Response> {
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
    match route {
        Route::Anonymous => anonymous(&mut req, secret).await,
        Route::MintRoom(room_type) => mint_room(&req, secret, cfg.rooms, room_type),
        Route::Schema(room_type) => schema(cfg.rooms, room_type),
        Route::Connect(room_type, name) => {
            connect(req, &env, secret, cfg.rooms, room_type, name).await
        }
        Route::Match(room_type) => match_room(req, &env, secret, cfg, room_type).await,
        Route::Lobby(room_type) => lobby(req, &env, secret, cfg, room_type).await,
        Route::Me => player_view(req, &env, secret, cfg).await,
    }
}

fn parse_route<'a>(method: &Method, path: &'a str) -> Option<Route<'a>> {
    let mut parts = path.trim_start_matches('/').split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("auth"), Some("anonymous"), None, None) if *method == Method::Post => {
            Some(Route::Anonymous)
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
        (Some("me"), None, None, None) if *method == Method::Get => Some(Route::Me),
        _ => None,
    }
}

async fn anonymous(req: &mut Request, secret: &[u8]) -> Result<Response> {
    let Ok(body) = req.json::<AnonymousBody>().await else {
        return Response::error("body must be JSON: {\"device_id\": \"...\"}", 400);
    };
    if !(8..=128).contains(&body.device_id.chars().count()) {
        return Response::error("device_id must be 8 to 128 characters", 400);
    }
    let player = anonymous_player_id(secret, &body.device_id);
    let token = issue_token(secret, &player, now_s());
    Response::from_json(&serde_json::json!({ "token": token, "player_id": player }))
}

/// Mints a room name without touching any Durable Object.
fn mint_room(
    req: &Request,
    secret: &[u8],
    routes: &[RoomRoute],
    room_type: &str,
) -> Result<Response> {
    if !routes.iter().any(|route| route.room_type == room_type) {
        return Response::error("unknown room type", 404);
    }
    let Some(token) = bearer_token(req) else {
        return Response::error("missing bearer token", 401);
    };
    if verify_token(secret, &token, now_s()).is_err() {
        return Response::error("invalid or expired token", 401);
    }
    Response::from_json(&serde_json::json!({ "room": mint_room_name() }))
}

fn schema(routes: &[RoomRoute], room_type: &str) -> Result<Response> {
    let Some(route) = routes.iter().find(|route| route.room_type == room_type) else {
        return Response::error("unknown room type", 404);
    };
    Response::from_json(&serde_json::json!({ "schema_hash": (route.schema_hash)() }))
}

async fn connect(
    req: Request,
    env: &Env,
    secret: &[u8],
    routes: &[RoomRoute],
    room_type: &str,
    name: &str,
) -> Result<Response> {
    let Some(route) = routes.iter().find(|route| route.room_type == room_type) else {
        return Response::error("unknown room type", 404);
    };
    if !valid_room_name(name) {
        return Response::error("invalid room name", 400);
    }
    let Some(token) = query_token(&req) else {
        return Response::error("missing token", 401);
    };
    let player = match verify_token(secret, &token, now_s()) {
        Ok(player) => player,
        Err(_) => return Response::error("invalid or expired token", 401),
    };
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
    headers.set(PLAYER_HEADER, &player.0)?;
    headers.set(ROOM_HEADER, &format!("{room_type}/{name}"))?;
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
    let Some(player) = bearer_player(&req, secret)? else {
        return Response::error("missing or invalid bearer token", 401);
    };
    let pool = match req.json::<MatchBody>().await {
        Ok(body) => body.pool.unwrap_or_else(|| "default".to_owned()),
        Err(_) => "default".to_owned(),
    };
    if !valid_pool(&pool) {
        return Response::error("pool must be 1 to 32 characters of [A-Za-z0-9_-]", 400);
    }
    let region = region_for(&req);
    let rpc = MmRpc::QuickJoin {
        player,
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
    let Some(_player) = bearer_player(&req, secret)? else {
        return Response::error("missing or invalid bearer token", 401);
    };
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
    let Some(player) = bearer_player(&req, secret)? else {
        return Response::error("missing or invalid bearer token", 401);
    };
    let namespace = env.durable_object(player_route.binding)?;
    let id = namespace.id_from_name(&player.0)?;
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
fn bearer_player(req: &Request, secret: &[u8]) -> Result<Option<PlayerId>> {
    let Some(token) = bearer_token(req) else {
        return Ok(None);
    };
    Ok(verify_token(secret, &token, now_s()).ok())
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
