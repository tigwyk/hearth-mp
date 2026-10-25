//! HTTP surface: anonymous auth, room minting, schema lookup and WebSocket routing.

use serde::Deserialize;
use worker::{Env, Method, Request, Response, Result};

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
}

const SECRET_BINDING: &str = "HEARTH_SECRET";
const MIN_SECRET_BYTES: usize = 32;
const SECRET_HELP: &str =
    "HEARTH_SECRET is not set (min 32 bytes); run: wrangler secret put HEARTH_SECRET";
const PLAYER_HEADER: &str = "X-Hearth-Player-Id";

#[derive(Deserialize)]
struct AnonymousBody {
    device_id: String,
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

enum Route<'a> {
    Anonymous,
    MintRoom(&'a str),
    Schema(&'a str),
    Connect(&'a str, &'a str),
}

/// Dispatch one request. `routes` is the Worker's static room table.
pub async fn serve(mut req: Request, env: Env, routes: &[RoomRoute]) -> Result<Response> {
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
        Route::MintRoom(room_type) => mint_room(&req, secret, routes, room_type),
        Route::Schema(room_type) => schema(routes, room_type),
        Route::Connect(room_type, name) => {
            connect(req, &env, secret, routes, room_type, name).await
        }
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

    let namespace = env.durable_object(route.binding)?;
    let id = namespace.id_from_name(name)?;
    let stub = match location_hint(req.cf().and_then(|cf| cf.continent())) {
        Some(hint) => id.get_stub_with_location_hint(hint)?,
        None => id.get_stub()?,
    };

    // Review focus 1: replace any client-supplied player id with the verified one.
    let mut outgoing = req.clone_mut()?;
    outgoing.headers_mut()?.set(PLAYER_HEADER, &player.0)?;
    stub.fetch_with_request(outgoing).await
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

// shortcut: continent-level hint only, refine with region/colo when Matchmaker lands (v0.3)
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
