//! Pure tests for the admin bearer token. No Workers runtime needed.

use hearth_mp_admin::{check_bearer, token_from};

const TOKEN: &[u8] = b"0123456789abcdef0123456789abcdef";

#[test]
fn bearer_accepts_exact_token() {
    assert!(check_bearer(
        Some("Bearer 0123456789abcdef0123456789abcdef"),
        TOKEN
    ));
}

#[test]
fn bearer_rejects_wrong_same_length() {
    assert!(!check_bearer(
        Some("Bearer 0123456789abcdef0123456789abcdee"),
        TOKEN
    ));
}

#[test]
fn bearer_rejects_prefix_and_longer() {
    assert!(!check_bearer(
        Some("Bearer 0123456789abcdef0123456789abcde"),
        TOKEN
    ));
    assert!(!check_bearer(
        Some("Bearer 0123456789abcdef0123456789abcdef0"),
        TOKEN
    ));
}

#[test]
fn bearer_rejects_missing_and_non_bearer_scheme() {
    for header in [
        None,
        Some(""),
        Some("0123456789abcdef0123456789abcdef"),
        Some("Basic 0123456789abcdef0123456789abcdef"),
        Some("bearer 0123456789abcdef0123456789abcdef"),
        Some("Bearer"),
        Some("Bearer "),
    ] {
        assert!(!check_bearer(header, TOKEN), "{header:?}");
    }
}

#[test]
fn short_token_disables() {
    assert_eq!(token_from(None), None);
    assert_eq!(token_from(Some(String::new())), None);
    assert_eq!(token_from(Some("short".to_owned())), None);
    let almost = "a".repeat(31);
    assert_eq!(token_from(Some(almost)), None);
    let exactly = "a".repeat(32);
    assert_eq!(
        token_from(Some(exactly.clone())),
        Some(exactly.into_bytes())
    );
}
