//! v0.3b Task 1: identity plumbing, namespaces, token v2 and the provider
//! framework. Pure functions only — no Workers runtime is available natively.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hearth_gateway::{
    anonymous_player_id, check_enabled, issue_token, namespaced, sanitize_name, verify_token,
    AuthConfig, AuthError, Identity, ProviderError,
};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const NOW: u64 = 1_700_000_000;

/// Mint a token by hand, so a test can leave claims out or lie about them.
fn signed(secret: &[u8], claims: serde_json::Value) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims are JSON"));
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{payload}.{signature}")
}

#[test]
fn sanitize_strips_controls_and_crlf() {
    // The CR/LF are gone, which is what makes header injection impossible.
    assert_eq!(
        sanitize_name("a\r\nX-Evil: 1"),
        Some("aX-Evil: 1".to_owned())
    );
    assert_eq!(
        sanitize_name("Ann\u{0} \u{2028}Smith\u{2029}"),
        Some("Ann Smith".to_owned())
    );
}

#[test]
fn sanitize_truncates_to_64_scalars() {
    let long = "🎮".repeat(200);
    let name = sanitize_name(&long).expect("a name");
    assert_eq!(
        name.chars().count(),
        64,
        "measured in Unicode scalar values"
    );
}

#[test]
fn sanitize_empty_is_none() {
    for raw in ["", "   ", "\r\n", "\u{0}\u{1}"] {
        assert_eq!(sanitize_name(raw), None, "{raw:?}");
    }
}

#[test]
fn namespaced_prefixes_and_validates() {
    assert_eq!(namespaced("jwt", "steam_123").unwrap().0, "jwt_steam_123");
    assert_eq!(namespaced("itch", "user.name").unwrap().0, "itch_user.name");
    let too_long = "a".repeat(129);
    for subject in ["a/b", "", too_long.as_str(), "x\n", "x y"] {
        assert_eq!(
            namespaced("jwt", subject),
            Err(AuthError::BadSubject),
            "{subject:?}"
        );
    }
}

#[test]
fn token_v2_round_trips_identity_with_name() {
    let identity = Identity {
        player_id: namespaced("jwt", "u1").unwrap(),
        provider: "jwt",
        display_name: Some("Ann".to_owned()),
    };
    let token = issue_token(SECRET, &identity, NOW);
    assert_eq!(verify_token(SECRET, &token, NOW), Ok(identity));
}

#[test]
fn token_without_prv_rejected() {
    // The pre-v0.3b compatibility shim is gone (v1.0a): every token names its provider.
    let player = anonymous_player_id(SECRET, "device-one");
    let token = signed(SECRET, json!({ "pid": player.0, "exp": NOW + 60 }));
    assert_eq!(verify_token(SECRET, &token, NOW), Err(AuthError::Malformed));
}

#[test]
fn unknown_provider_in_token_is_malformed() {
    let token = signed(
        SECRET,
        json!({ "pid": "root_1", "prv": "root", "exp": NOW + 60 }),
    );
    assert_eq!(verify_token(SECRET, &token, NOW), Err(AuthError::Malformed));
}

#[test]
fn token_id_outside_its_namespace_is_rejected() {
    // A token that claims the jwt provider with a steam id would be a way to
    // borrow another identity; only the provider namespace may prefix the id.
    let token = signed(
        SECRET,
        json!({ "pid": "steam_76561198000000000", "prv": "jwt", "exp": NOW + 60 }),
    );
    assert_eq!(
        verify_token(SECRET, &token, NOW),
        Err(AuthError::BadSubject)
    );
}

#[test]
fn disabled_provider_is_404() {
    // `verify` itself needs a Workers `Env`, which native tests cannot build, so
    // the enabled/disabled decision is its own pure function.
    let err = check_enabled(&AuthConfig::default(), "steam").expect_err("steam is off");
    assert_eq!(err, ProviderError::Disabled);
    assert_eq!(err.status(), 404);
    assert!(check_enabled(&AuthConfig::default(), "anonymous").is_ok());
    assert!(check_enabled(&AuthConfig::default(), "nope").is_err());
}
