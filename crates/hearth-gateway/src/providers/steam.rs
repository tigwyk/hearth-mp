//! Steam provider: a session ticket from `GetAuthTicketForWebApi`.
//!
//! One HTTP call to the Steam Web API turns the ticket into a SteamID. The
//! publisher key only ever appears in the outbound URL: every error is built
//! from the upstream response, so the key can never leak through a message.

use serde_json::Value;
use worker::Env;

use crate::auth::{namespaced, Identity};

use super::{Fetcher, HttpReq, HttpResp, ProviderError};

const AUTH_URL: &str = "https://partner.steam-api.com/ISteamUserAuth/AuthenticateUserTicket/v1/";
/// `GetAuthTicketForWebApi` returns hex; the Web API rejects anything much longer.
const MAX_TICKET_CHARS: usize = 4096;

/// Which Steam app's tickets are accepted.
pub struct SteamConfig {
    /// The Steam app id whose tickets are valid.
    pub app_id: u32,
    /// The binding holding the publisher Web API key.
    pub key_env: &'static str,
    /// The identity string the client asked the ticket for, default `"hearth"`.
    pub identity: String,
    /// Reject tickets whose owner is publisher- or VAC-banned.
    pub reject_banned: bool,
}

/// `{"ticket": "<hex>"}`.
pub async fn verify(
    cfg: &SteamConfig,
    body: &Value,
    env: &Env,
    fetcher: &impl Fetcher,
) -> Result<Identity, ProviderError> {
    let key = match env.secret(cfg.key_env) {
        Ok(secret) => secret.to_string(),
        Err(_) => return Err(unavailable()),
    };
    if key.is_empty() {
        return Err(unavailable());
    }
    verify_with_key(cfg, body, &key, fetcher).await
}

/// The Workers-free part, so native tests can exercise it without an `Env`.
pub async fn verify_with_key(
    cfg: &SteamConfig,
    body: &Value,
    key: &str,
    fetcher: &impl Fetcher,
) -> Result<Identity, ProviderError> {
    let ticket = body.get("ticket").and_then(Value::as_str).ok_or_else(|| {
        ProviderError::BadRequest("body must be JSON: {\"ticket\": \"...\"}".to_owned())
    })?;
    if ticket.is_empty() || ticket.chars().count() > MAX_TICKET_CHARS {
        return Err(ProviderError::BadRequest(format!(
            "ticket must be 1 to {MAX_TICKET_CHARS} characters"
        )));
    }

    let url = format!(
        "{AUTH_URL}?key={}&appid={}&ticket={}&identity={}",
        encode(key),
        cfg.app_id,
        encode(ticket),
        encode(&cfg.identity),
    );
    let request = HttpReq {
        method: "GET",
        url,
        headers: Vec::new(),
        body: None,
    };
    let response = fetcher.send(request).await.map_err(|_| unavailable())?;
    let value = response_json(&response)?;

    // The documented failure shape: `response.error.{errorcode,errordesc}`.
    if let Some(error) = value.pointer("/response/error") {
        let description = error
            .get("errordesc")
            .and_then(Value::as_str)
            .unwrap_or("ticket rejected");
        return Err(ProviderError::Rejected(description.to_owned()));
    }
    let params = value.pointer("/response/params").ok_or_else(unavailable)?;
    if params.get("result").and_then(Value::as_str) != Some("OK") {
        return Err(ProviderError::Rejected("ticket rejected".to_owned()));
    }
    if cfg.reject_banned && is_banned(params) {
        return Err(ProviderError::Rejected("banned".to_owned()));
    }
    let steam_id = params
        .get("steamid")
        .and_then(Value::as_str)
        .ok_or_else(unavailable)?;
    // A missing or unnameable id is an upstream anomaly, not a credential problem.
    let player_id = namespaced("steam", steam_id).map_err(|_| unavailable())?;
    Ok(Identity {
        player_id,
        provider: "steam",
        display_name: None,
    })
}

fn is_banned(params: &Value) -> bool {
    ["publisherbanned", "vacbanned"]
        .into_iter()
        .any(|field| params.get(field).and_then(Value::as_bool) == Some(true))
}

fn unavailable() -> ProviderError {
    ProviderError::Upstream("steam unavailable".to_owned())
}

/// Map an upstream answer onto the shared rules: 4xx is a bad credential, a 5xx
/// or an unreadable body is the provider being down.
fn response_json(response: &HttpResp) -> Result<Value, ProviderError> {
    match response.status {
        200..=299 => serde_json::from_str(&response.body).map_err(|_| unavailable()),
        400..=499 => Err(ProviderError::Rejected("steam rejected".to_owned())),
        _ => Err(unavailable()),
    }
}

/// Percent-encode one query value; the ticket, key and identity are client- or
/// config-supplied, so none of them may break the URL.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
