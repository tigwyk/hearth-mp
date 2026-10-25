//! Anonymous provider: a stable player id from the client's device id.

use serde_json::Value;

use crate::auth::{anonymous_player_id, Identity, ANON_PROVIDER};

use super::ProviderError;

/// The v0.1 login: `{"device_id": "..."}`, 8 to 128 characters.
pub fn verify(secret: &[u8], body: &Value) -> Result<Identity, ProviderError> {
    let device_id = body
        .get("device_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ProviderError::BadRequest("body must be JSON: {\"device_id\": \"...\"}".to_owned())
        })?;
    if !(8..=128).contains(&device_id.chars().count()) {
        return Err(ProviderError::BadRequest(
            "device_id must be 8 to 128 characters".to_owned(),
        ));
    }
    Ok(Identity {
        player_id: anonymous_player_id(secret, device_id),
        provider: ANON_PROVIDER,
        display_name: None,
    })
}
