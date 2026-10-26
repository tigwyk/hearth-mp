//! itch.io provider: an API key checked against `itch.io/api/1/jwt/me`.
//!
//! The API key arrives in the request body (itch's own integrations ship it in
//! `ITCHIO_API_KEY`); it is sent as a bearer credential and never stored.

use serde_json::Value;

use crate::auth::{namespaced, sanitize_name, Identity};

use super::{Fetcher, HttpReq, HttpResp, ProviderError};

const ME_URL: &str = "https://itch.io/api/1/jwt/me";

/// `{"api_key": "<key>"}`.
pub async fn verify(body: &Value, fetcher: &impl Fetcher) -> Result<Identity, ProviderError> {
    let api_key = body.get("api_key").and_then(Value::as_str).ok_or_else(|| {
        ProviderError::BadRequest("body must be JSON: {\"api_key\": \"...\"}".to_owned())
    })?;
    if api_key.is_empty() {
        return Err(ProviderError::BadRequest(
            "api_key must not be empty".to_owned(),
        ));
    }

    let request = HttpReq {
        method: "GET",
        url: ME_URL.to_owned(),
        headers: vec![("Authorization".to_owned(), format!("Bearer {api_key}"))],
        body: None,
    };
    let response = fetcher.send(request).await.map_err(|_| unavailable())?;
    let value = response_json(&response)?;

    // itch answers 200 with an `errors` array for a bad key, and 401 for a
    // missing one; both are a rejected credential.
    if let Some(errors) = value.get("errors") {
        let message = errors
            .get(0)
            .and_then(Value::as_str)
            .unwrap_or("itch rejected");
        return Err(ProviderError::Rejected(message.to_owned()));
    }
    let user = value.get("user").ok_or_else(unavailable)?;
    let id = user.get("id").and_then(user_id).ok_or_else(unavailable)?;
    let player_id = namespaced("itch", &id).map_err(|_| unavailable())?;
    // `display_name`, falling back to `username` when either is missing or
    // sanitises away to nothing.
    let display_name = user
        .get("display_name")
        .and_then(Value::as_str)
        .and_then(sanitize_name)
        .or_else(|| {
            user.get("username")
                .and_then(Value::as_str)
                .and_then(sanitize_name)
        });
    Ok(Identity {
        player_id,
        provider: "itch",
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
    ProviderError::Upstream("itch unavailable".to_owned())
}

/// 4xx is a rejected key; anything else that is not readable JSON is upstream.
fn response_json(response: &HttpResp) -> Result<Value, ProviderError> {
    match response.status {
        200..=299 => serde_json::from_str(&response.body).map_err(|_| unavailable()),
        400..=499 => Err(ProviderError::Rejected("itch rejected".to_owned())),
        _ => Err(unavailable()),
    }
}
