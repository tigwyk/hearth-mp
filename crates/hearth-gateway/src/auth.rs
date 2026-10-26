//! Pure authentication helpers: identities, player id namespaces, signed tokens
//! and room names.
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

/// The provider of a token minted before v0.3b, and of anonymous logins.
pub const ANON_PROVIDER: &str = "anon";

/// Display names are clamped to this many Unicode scalar values.
const MAX_NAME_SCALARS: usize = 64;

/// Provider namespaces a token may carry. Any other value is malformed.
const PROVIDERS: [&str; 5] = ["anon", "jwt", "steam", "itch", "discord"];

/// Why a token or a subject could not be accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// The token is not `payload.signature` with base64url parts.
    Malformed,
    /// The HMAC did not match.
    BadSignature,
    /// The token's `exp` is at or before the current time.
    Expired,
    /// A provider tried to mint an id outside its own namespace.
    BadSubject,
}

/// Who a verified credential belongs to.
///
/// The raw credential never leaves the provider: only this struct reaches a room
/// (as the player id plus the `X-Hearth-Provider`/`X-Hearth-Display-Name` headers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The namespaced player id, e.g. `jwt_1234`.
    pub player_id: PlayerId,
    /// `"anon"`, `"jwt"`, `"steam"`, `"itch"` or `"discord"`.
    pub provider: &'static str,
    /// The sanitised display name, when the provider supplied one.
    pub display_name: Option<String>,
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

#[doc(hidden)]
/// `"{provider}_{subject}"`, the only way a provider may mint a player id.
///
/// The subject charset excludes separators and whitespace, so a subject cannot
/// smuggle itself into another provider's namespace (`"steam_1"` stays
/// `"jwt_steam_1"`) and cannot break a header.
pub fn namespaced(provider: &'static str, subject: &str) -> Result<PlayerId, AuthError> {
    let valid = (1..=128).contains(&subject.len())
        && subject
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'@' | b'-'));
    if !valid {
        return Err(AuthError::BadSubject);
    }
    Ok(PlayerId(format!("{provider}_{subject}")))
}

#[doc(hidden)]
/// A display name safe to echo in JSON and in a header.
///
/// Trimmed, at most 64 Unicode scalar values, with control characters (Cc) and
/// the line/paragraph separators removed, so CR/LF header injection is
/// impossible. An empty result is `None`.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{2028}' && *c != '\u{2029}')
        .take(MAX_NAME_SCALARS)
        .collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}

fn provider_name(name: &str) -> Option<&'static str> {
    PROVIDERS.into_iter().find(|known| *known == name)
}

#[doc(hidden)]
/// Issue a token: base64url-nopad of `{"pid","prv","name"?,"exp"}` plus `.` plus
/// its base64url-nopad HMAC.
pub fn issue_token(secret: &[u8], identity: &Identity, now_s: u64) -> String {
    let mut claims = serde_json::json!({
        "pid": identity.player_id.0.as_str(),
        "prv": identity.provider,
        "exp": now_s + TOKEN_TTL_SECS,
    });
    if let Some(name) = &identity.display_name {
        claims["name"] = serde_json::Value::String(name.clone());
    }
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims are JSON"));
    let signature = URL_SAFE_NO_PAD.encode(sign(secret, payload.as_bytes()));
    format!("{payload}.{signature}")
}

/// Verify a token's constant-time HMAC and expiry, and return its identity.
///
/// Every token names its provider in `prv`; one without it (minted before
/// v0.3b, so at least 24 hours stale) is malformed.
pub fn verify_token(secret: &[u8], token: &str, now_s: u64) -> Result<Identity, AuthError> {
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
    let provider = claims
        .get("prv")
        .and_then(|prv| prv.as_str())
        .and_then(provider_name)
        .ok_or(AuthError::Malformed)?;
    // An id outside its provider's namespace cannot be one we minted, and would
    // let a credential claim another provider's identity.
    if !pid.starts_with(provider)
        || !pid
            .as_bytes()
            .get(provider.len())
            .is_some_and(|b| *b == b'_')
    {
        return Err(AuthError::BadSubject);
    }
    let display_name = claims
        .get("name")
        .and_then(|name| name.as_str())
        .and_then(sanitize_name);
    Ok(Identity {
        player_id: PlayerId(pid.to_owned()),
        provider,
        display_name,
    })
}

#[doc(hidden)]
/// `^[A-Za-z0-9_-]{1,64}$`, without pulling in the regex crate.
pub fn valid_room_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[doc(hidden)]
/// 16 random hex chars. A predictable name would be guessable, so RNG failure is fatal.
pub fn mint_room_name() -> String {
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).expect("system RNG unavailable");
    hex(&buf)
}
