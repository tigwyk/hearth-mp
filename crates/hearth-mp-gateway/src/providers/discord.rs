//! Discord provider: an OAuth2 authorization code exchanged for an identity.
//!
//! Two calls: the code is exchanged for an access token at the token endpoint,
//! then the token reads the user. The access token is used once and dropped; it
//! is never stored and never appears in an error message.

use serde_json::Value;
use worker::Env;

use crate::auth::{namespaced, sanitize_name, Identity};

use super::{Fetcher, HttpReq, HttpResp, ProviderError};

const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
const ME_URL: &str = "https://discord.com/api/users/@me";

/// The Discord application whose codes are accepted.
#[non_exhaustive]
pub struct DiscordConfig {
    /// The Discord application's client id.
    pub client_id: String,
    /// The binding holding the OAuth2 client secret.
    pub secret_env: &'static str,
    /// Sent with the token exchange when the code came from a redirect flow.
    pub redirect_uri: Option<String>,
}

impl DiscordConfig {
    /// Accept codes for the application `client_id`, whose OAuth2 secret is in the
    /// `secret_env` binding; no redirect URI.
    pub fn new(client_id: impl Into<String>, secret_env: &'static str) -> Self {
        Self {
            client_id: client_id.into(),
            secret_env,
            redirect_uri: None,
        }
    }
}

/// `{"code": "<code>"}`.
pub async fn verify(
    cfg: &DiscordConfig,
    body: &Value,
    env: &Env,
    fetcher: &impl Fetcher,
) -> Result<Identity, ProviderError> {
    let secret = match env.secret(cfg.secret_env) {
        Ok(secret) => secret.to_string(),
        Err(_) => return Err(unavailable()),
    };
    if secret.is_empty() {
        return Err(unavailable());
    }
    verify_with_secret(cfg, body, &secret, fetcher).await
}

/// The Workers-free part, so native tests can exercise it without an `Env`.
pub async fn verify_with_secret(
    cfg: &DiscordConfig,
    body: &Value,
    client_secret: &str,
    fetcher: &impl Fetcher,
) -> Result<Identity, ProviderError> {
    let code = body.get("code").and_then(Value::as_str).ok_or_else(|| {
        ProviderError::BadRequest("body must be JSON: {\"code\": \"...\"}".to_owned())
    })?;
    if code.is_empty() {
        return Err(ProviderError::BadRequest(
            "code must not be empty".to_owned(),
        ));
    }

    let mut form = vec![
        ("client_id", cfg.client_id.as_str()),
        ("client_secret", client_secret),
        ("grant_type", "authorization_code"),
        ("code", code),
    ];
    if let Some(redirect_uri) = &cfg.redirect_uri {
        form.push(("redirect_uri", redirect_uri.as_str()));
    }
    let request = HttpReq {
        method: "POST",
        url: TOKEN_URL.to_owned(),
        headers: vec![(
            "Content-Type".to_owned(),
            "application/x-www-form-urlencoded".to_owned(),
        )],
        body: Some(encode_form(&form)),
    };
    let token_response = fetcher.send(request).await.map_err(|_| unavailable())?;
    let token_value = response_json(&token_response)?;
    let access_token = token_value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(unavailable)?;

    let request = HttpReq {
        method: "GET",
        url: ME_URL.to_owned(),
        headers: vec![("Authorization".to_owned(), format!("Bearer {access_token}"))],
        body: None,
    };
    let user_response = fetcher.send(request).await.map_err(|_| unavailable())?;
    let user = response_json(&user_response)?;

    let id = user.get("id").and_then(user_id).ok_or_else(unavailable)?;
    let player_id = namespaced("discord", &id).map_err(|_| unavailable())?;
    // `global_name` is the display name; older accounts only have `username`,
    // and either may sanitise away to nothing.
    let display_name = user
        .get("global_name")
        .and_then(Value::as_str)
        .and_then(sanitize_name)
        .or_else(|| {
            user.get("username")
                .and_then(Value::as_str)
                .and_then(sanitize_name)
        });
    Ok(Identity {
        player_id,
        provider: "discord",
        display_name,
    })
}

fn user_id(value: &Value) -> Option<String> {
    if let Some(id) = value.as_i64() {
        return Some(id.to_string());
    }
    value.as_str().map(str::to_owned)
}

fn unavailable() -> ProviderError {
    ProviderError::Upstream("discord unavailable".to_owned())
}

/// 4xx is a rejected credential (bad code or token); anything else unreadable is
/// the provider being down. Never issue an identity off anything but a 2xx JSON.
fn response_json(response: &HttpResp) -> Result<Value, ProviderError> {
    match response.status {
        200..=299 => serde_json::from_str(&response.body).map_err(|_| unavailable()),
        400..=499 => Err(ProviderError::Rejected("discord rejected".to_owned())),
        _ => Err(unavailable()),
    }
}

fn encode_form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode one form value; the code, client id and redirect URI are
/// external input, so none of them may break the body.
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
