//! Pure authentication helpers: anonymous player ids, signed tokens and room names.
//!
//! Nothing here touches a Workers API, so it compiles and tests natively.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hearth_protocol::PlayerId;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Token lifetime: 24 hours.
const TOKEN_TTL_SECS: u64 = 86_400;

/// Why a token could not be verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// The token is not `payload.signature` with base64url parts.
    Malformed,
    /// The HMAC did not match.
    BadSignature,
    /// The token's `exp` is at or before the current time.
    Expired,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sign(secret: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// `"anon_"` plus the first 32 hex chars of `HMAC-SHA256(secret, "anon:" + device_id)`.
pub fn anonymous_player_id(secret: &[u8], device_id: &str) -> PlayerId {
    let digest = sign(secret, format!("anon:{device_id}").as_bytes());
    PlayerId(format!("anon_{}", hex(&digest[..16])))
}

/// Issue a token: base64url-nopad of `{"pid","exp"}` plus `.` plus its base64url-nopad HMAC.
pub fn issue_token(secret: &[u8], player: &PlayerId, now_s: u64) -> String {
    let claims = serde_json::json!({ "pid": player.0.as_str(), "exp": now_s + TOKEN_TTL_SECS });
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims are JSON"));
    let signature = URL_SAFE_NO_PAD.encode(sign(secret, payload.as_bytes()));
    format!("{payload}.{signature}")
}

/// Verify a token's constant-time HMAC and expiry, and return the player it was issued to.
pub fn verify_token(secret: &[u8], token: &str, now_s: u64) -> Result<PlayerId, AuthError> {
    let (payload, signature) = token.split_once('.').ok_or(AuthError::Malformed)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| AuthError::Malformed)?;
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| AuthError::BadSignature)?;
    let claims: serde_json::Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| AuthError::Malformed)?,
    )
    .map_err(|_| AuthError::Malformed)?;
    let pid = claims
        .get("pid")
        .and_then(|pid| pid.as_str())
        .ok_or(AuthError::Malformed)?;
    let exp = claims
        .get("exp")
        .and_then(|exp| exp.as_u64())
        .ok_or(AuthError::Malformed)?;
    if now_s >= exp {
        return Err(AuthError::Expired);
    }
    Ok(PlayerId(pid.to_owned()))
}

/// `^[A-Za-z0-9_-]{1,64}$`, without pulling in the regex crate.
pub fn valid_room_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 16 random hex chars. A predictable name would be guessable, so RNG failure is fatal.
pub fn mint_room_name() -> String {
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).expect("system RNG unavailable");
    hex(&buf)
}
