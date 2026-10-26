//! The bearer-token-gated `/admin/*` HTTP API: list rooms, inspect, dispose and
//! tail a live room through its Durable Object's internal `/__hearth/*` paths.
//!
//! The admin token is checked here and never forwarded; the room only ever sees
//! requests this router built itself.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hearth_gateway::auth::valid_room_name;
use hearth_gateway::RoomRoute;
use hearth_server::RoomName;
use serde::{Deserialize, Serialize};
use worker::{Env, Headers, Method, Request, RequestInit, Response, Url};

use crate::registry::RoomRow;

const ADMIN_TOKEN_BINDING: &str = "HEARTH_ADMIN_TOKEN";
/// Anything shorter is a misconfiguration; the admin API then acts as if absent.
const MIN_TOKEN_BYTES: usize = 32;

const REGISTRY_BINDING: &str = "HEARTH_REGISTRY";
const REGISTRY_NAME: &str = "global";

#[derive(Deserialize)]
struct RoomsQuery {
    #[serde(rename = "type")]
    room_type: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
}

/// The registry answers with `RoomName`s; the public API answers with opaque
/// base64url cursors, so clients never depend on the registry's shape.
#[derive(Deserialize)]
struct RegistryList {
    rooms: Vec<RoomRow>,
    next: Option<RoomName>,
}

#[derive(Serialize)]
struct AdminRooms {
    rooms: Vec<RoomRow>,
    next: Option<String>,
}

enum AdminRoute<'a> {
    Rooms,
    Inspect { room_type: &'a str, name: &'a str },
    Dispose { room_type: &'a str, name: &'a str },
    Tail { room_type: &'a str, name: &'a str },
}

/// The configured admin token, or `None` which disables the admin API (404).
pub fn admin_token(env: &Env) -> Option<Vec<u8>> {
    token_from(
        env.secret(ADMIN_TOKEN_BINDING)
            .ok()
            .map(|token| token.to_string()),
    )
}

#[doc(hidden)]
/// [`admin_token`] with the environment read split out, so it is testable.
pub fn token_from(value: Option<String>) -> Option<Vec<u8>> {
    value
        .filter(|token| token.len() >= MIN_TOKEN_BYTES)
        .map(String::into_bytes)
}

#[doc(hidden)]
/// `Authorization: Bearer <token>`, compared in constant time over the full
/// token so a wrong token leaks no prefix.
pub fn check_bearer(header: Option<&str>, token: &[u8]) -> bool {
    let Some(value) = header.and_then(|header| header.strip_prefix("Bearer ")) else {
        return false;
    };
    let value = value.as_bytes();
    if value.len() != token.len() {
        return false;
    }
    let mut diff = 0u8;
    for (given, expected) in value.iter().zip(token) {
        diff |= *given ^ *expected;
    }
    diff == 0
}

/// Dispatch one `/admin/*` request. `routes` is the Worker's room table, used
/// both for `{type}` validation and for the Durable Object binding name.
pub async fn serve_admin(req: Request, env: Env, routes: &[RoomRoute]) -> worker::Result<Response> {
    // A deployment without a token has no admin API at all, not an open one and
    // not an unauthorized one.
    let Some(token) = admin_token(&env) else {
        return Response::error("not found", 404);
    };
    let path = req.path();
    let Some(route) = parse_route(&req.method(), &path) else {
        return Response::error("not found", 404);
    };
    if !check_bearer(req.headers().get("Authorization")?.as_deref(), &token) {
        return Response::error("invalid admin token", 401);
    }
    match route {
        AdminRoute::Rooms => list_rooms(req, &env).await,
        AdminRoute::Inspect { room_type, name } => {
            proxy(
                &env,
                routes,
                room_type,
                name,
                "/__hearth/inspect",
                Method::Get,
                &req,
            )
            .await
        }
        AdminRoute::Dispose { room_type, name } => {
            proxy(
                &env,
                routes,
                room_type,
                name,
                "/__hearth/dispose",
                Method::Post,
                &req,
            )
            .await
        }
        AdminRoute::Tail { room_type, name } => {
            proxy(
                &env,
                routes,
                room_type,
                name,
                "/__hearth/tail",
                Method::Get,
                &req,
            )
            .await
        }
    }
}

fn parse_route<'a>(method: &Method, path: &'a str) -> Option<AdminRoute<'a>> {
    let mut parts = path.strip_prefix('/')?.split('/');
    let route = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    );
    match route {
        (Some("admin"), Some("rooms"), None, None, None, None) if *method == Method::Get => {
            Some(AdminRoute::Rooms)
        }
        (Some("admin"), Some("rooms"), Some(room_type), Some(name), None, None)
            if *method == Method::Get =>
        {
            Some(AdminRoute::Inspect { room_type, name })
        }
        (Some("admin"), Some("rooms"), Some(room_type), Some(name), Some("dispose"), None)
            if *method == Method::Post =>
        {
            Some(AdminRoute::Dispose { room_type, name })
        }
        (Some("admin"), Some("rooms"), Some(room_type), Some(name), Some("tail"), None)
            if *method == Method::Get =>
        {
            Some(AdminRoute::Tail { room_type, name })
        }
        _ => None,
    }
}

async fn list_rooms(req: Request, env: &Env) -> worker::Result<Response> {
    let Ok(query) = req.query::<RoomsQuery>() else {
        return Response::error("invalid query", 400);
    };
    // The registry is optional: without the binding, inspecting, disposing and
    // tailing still work because they only need the type and name.
    let Ok(registry) = env.durable_object(REGISTRY_BINDING) else {
        return Response::from_json(&serde_json::json!({
            "error": "HEARTH_REGISTRY binding not configured",
        }))
        .map(|response| response.with_status(501));
    };
    let after = match query.cursor.as_deref() {
        None => None,
        Some(cursor) => match decode_cursor(cursor) {
            Some(after) => Some(after),
            None => return Response::error("invalid cursor", 400),
        },
    };
    let mut url = Url::parse("https://registry/list")?;
    {
        let mut params = url.query_pairs_mut();
        if let Some(room_type) = query
            .room_type
            .as_deref()
            .filter(|room_type| !room_type.is_empty())
        {
            params.append_pair("type", room_type);
        }
        if let Some(after) = &after {
            params.append_pair("after_type", &after.room_type);
            params.append_pair("after_name", &after.name);
        }
        if let Some(limit) = query.limit {
            params.append_pair("limit", &limit.to_string());
        }
    }
    let stub = registry.get_by_name(REGISTRY_NAME)?;
    let mut response = stub
        .fetch_with_request(Request::new_with_init(url.as_str(), &RequestInit::new())?)
        .await?;
    if response.status_code() != 200 {
        return Ok(response);
    }
    let list: RegistryList = response.json().await?;
    Response::from_json(&AdminRooms {
        rooms: list.rooms,
        next: list.next.as_ref().map(encode_cursor),
    })
}

/// Forward one admin request to the room's Durable Object. The room only sees
/// an internal URL this router built; the admin token is dropped here.
async fn proxy(
    env: &Env,
    routes: &[RoomRoute],
    room_type: &str,
    name: &str,
    path: &str,
    method: Method,
    req: &Request,
) -> worker::Result<Response> {
    let Some(route) = routes.iter().find(|route| route.room_type == room_type) else {
        return Response::error("unknown room type", 404);
    };
    if !valid_room_name(name) {
        return Response::error("invalid room name", 400);
    }
    let stub = env.durable_object(route.binding)?.get_by_name(name)?;
    let mut init = RequestInit::new();
    init.method = method;
    init.headers = forwarded_headers(req)?;
    stub.fetch_with_request(Request::new_with_init(
        &format!("https://room{path}"),
        &init,
    )?)
    .await
}

/// Copy the incoming headers the room needs (notably the `Upgrade` handshake for
/// the tail) and drop the ones it must not receive.
fn forwarded_headers(req: &Request) -> worker::Result<Headers> {
    let headers = Headers::new();
    for (name, value) in req.headers().entries() {
        let lower = name.to_ascii_lowercase();
        // Never leak the admin token, and never forward framing headers for a
        // body this router does not forward.
        if matches!(
            lower.as_str(),
            "authorization" | "host" | "content-length" | "transfer-encoding"
        ) {
            continue;
        }
        headers.set(&name, &value)?;
    }
    Ok(headers)
}

fn encode_cursor(room: &RoomName) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(room).expect("a RoomName is JSON"))
}

fn decode_cursor(cursor: &str) -> Option<RoomName> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()
}
