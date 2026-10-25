//! Pure-function tests for `hearth_gateway::auth`. No Workers runtime needed.

use hearth_gateway::{
    anonymous_player_id, issue_token, mint_room_name, valid_room_name, verify_token, AuthError,
};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const OTHER_SECRET: &[u8] = b"fedcba9876543210fedcba9876543210";
const NOW: u64 = 1_700_000_000;

#[test]
fn token_round_trips() {
    let player = anonymous_player_id(SECRET, "device-one");
    let token = issue_token(SECRET, &player, NOW);
    assert_eq!(verify_token(SECRET, &token, NOW), Ok(player));
}

#[test]
fn expired_token_rejected() {
    let player = anonymous_player_id(SECRET, "device-one");
    let token = issue_token(SECRET, &player, NOW);
    assert_eq!(
        verify_token(SECRET, &token, NOW + 86_401),
        Err(AuthError::Expired)
    );
}

#[test]
fn tampered_payload_rejected() {
    let player = anonymous_player_id(SECRET, "device-one");
    let token = issue_token(SECRET, &player, NOW);
    let (payload, signature) = token.split_once('.').expect("token has a dot");
    let mut chars: Vec<char> = payload.chars().collect();
    chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
    let tampered = format!("{}.{signature}", chars.into_iter().collect::<String>());
    assert_eq!(
        verify_token(SECRET, &tampered, NOW),
        Err(AuthError::BadSignature)
    );
}

#[test]
fn token_from_other_secret_rejected() {
    let player = anonymous_player_id(SECRET, "device-one");
    let token = issue_token(SECRET, &player, NOW);
    assert_eq!(
        verify_token(OTHER_SECRET, &token, NOW),
        Err(AuthError::BadSignature)
    );
}

#[test]
fn garbage_is_malformed() {
    for token in ["", "a.b.c", "no-dot"] {
        assert_eq!(
            verify_token(SECRET, token, NOW),
            Err(AuthError::Malformed),
            "{token:?}"
        );
    }
}

#[test]
fn anonymous_id_is_stable_and_secret_dependent() {
    let player = anonymous_player_id(SECRET, "device-one");
    assert_eq!(player, anonymous_player_id(SECRET, "device-one"));
    assert_ne!(player, anonymous_player_id(SECRET, "device-two"));
    assert_ne!(player, anonymous_player_id(OTHER_SECRET, "device-one"));
    assert!(player.0.starts_with("anon_"), "{player:?}");
    assert_eq!(player.0.len(), "anon_".len() + 32);
}

#[test]
fn room_names() {
    assert!(valid_room_name("abc-DEF_09"));
    assert!(valid_room_name(&"a".repeat(64)));
    let too_long = "a".repeat(65);
    for name in ["", too_long.as_str(), "a/b", "a b"] {
        assert!(!valid_room_name(name), "{name:?}");
    }
    let name = mint_room_name();
    assert!(valid_room_name(&name), "{name:?}");
    assert_eq!(name.len(), 16);
    assert_ne!(name, mint_room_name());
}
