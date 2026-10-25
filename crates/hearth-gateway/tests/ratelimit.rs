//! v0.3b Task 4: rate-limit keys and the 429 response. Pure functions only; the
//! live binding behaviour is covered by the e2e test against `wrangler dev`.

use hearth_gateway::ratelimit::{key, response_for, Limit, Limits, Scope};
use hearth_protocol::PlayerId;

#[test]
fn keys_are_scoped_and_prefixed() {
    let player = PlayerId("jwt_u1".to_owned());
    assert_eq!(
        key(Scope::Ip, "1.2.3.4", None),
        Some("ip:1.2.3.4".to_owned())
    );
    assert_eq!(
        key(Scope::Player, "1.2.3.4", Some(&player)),
        Some("p:jwt_u1".to_owned())
    );
    assert_eq!(
        key(Scope::Ip, "", None),
        None,
        "an unknown IP cannot be limited"
    );
    assert_eq!(
        key(Scope::Player, "1.2.3.4", None),
        None,
        "a player limit without a verified player is skipped"
    );
}

#[test]
fn limited_response_is_429_with_retry_after() {
    let (status, headers, body) = response_for(Scope::Player, 60);
    assert_eq!(status, 429);
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "Retry-After" && value == "60"),
        "{headers:?}"
    );
    assert!(body.contains("\"rate_limited\""), "{body}");
    assert!(body.contains("\"player\""), "{body}");
}

#[test]
fn default_limits_are_named_and_generous_for_ips() {
    let limits = Limits::default();
    assert_eq!(limits.ip_binding, "HEARTH_IP_LIMIT");
    assert_eq!(limits.player_binding, "HEARTH_PLAYER_LIMIT");
    assert!(limits.ip_period_s >= 60, "shared IPs need a long window");
    assert!(limits.player_period_s >= 60);
    assert_eq!(Limit::NoBinding, Limit::NoBinding);
}
