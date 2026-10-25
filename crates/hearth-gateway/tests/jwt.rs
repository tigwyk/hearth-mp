//! v0.3b Task 2: JWT claim rules, algorithm binding and HS256 verification.
//!
//! Native only. The JWKS RS256/ES256 path needs WebCrypto, which has no host
//! implementation; it is covered by the live e2e against a real JWKS instead.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hearth_gateway::providers::jwt::{
    allowed_alg, check_claims, identity, parse, select_jwk, verify_hs256, Claims, Header, Jwk,
    JwtConfig, JwtKeys,
};
use hearth_gateway::ProviderError;
use hmac::{Hmac, Mac};
use serde_json::{json, Map, Value};
use sha2::Sha256;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const NOW: u64 = 1_700_000_000;
const ISSUER: &str = "https://auth.example.test";
const AUDIENCE: &str = "hearth-tictactoe";

fn hs256_config() -> JwtConfig {
    JwtConfig {
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
        keys: JwtKeys::Hs256 {
            secret_env: "HEARTH_JWT_SECRET",
        },
        name_claim: Some("name".to_owned()),
    }
}

fn jwks_config() -> JwtConfig {
    JwtConfig {
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
        keys: JwtKeys::Jwks {
            url: "https://auth.example.test/jwks.json".to_owned(),
        },
        name_claim: None,
    }
}

fn base_claims() -> Claims {
    Claims {
        iss: Some(ISSUER.to_owned()),
        aud: Some(json!(AUDIENCE)),
        sub: Some("u1".to_owned()),
        exp: Some(NOW + 300),
        nbf: None,
        extra: Map::new(),
    }
}

fn named_claims(name: &str) -> Claims {
    let mut claims = base_claims();
    claims
        .extra
        .insert("name".to_owned(), Value::String(name.to_owned()));
    claims
}

fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Mint a compact JWS by hand, so a test can lie about the header or the claims.
fn compact(header: Value, claims: &Claims) -> String {
    let header = encode(&serde_json::to_vec(&header).expect("the header is JSON"));
    let payload = encode(&serde_json::to_vec(claims).expect("the claims are JSON"));
    let signing_input = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).expect("HMAC accepts any key length");
    mac.update(signing_input.as_bytes());
    let signature: [u8; 32] = mac.finalize().into_bytes().into();
    format!("{signing_input}.{}", encode(&signature))
}

fn hs256_token(claims: &Claims) -> String {
    compact(json!({ "alg": "HS256", "typ": "JWT" }), claims)
}

fn rsa_jwk() -> Jwk {
    Jwk::from_value(json!({
        "kty": "RSA",
        "kid": "rsa-1",
        "n": "sXch0Xch0Xch0Xch0Xch0Xch0Xch0Xch0Xch0Xch0Xch0Xch0Xch0",
        "e": "AQAB",
    }))
    .expect("an RSA JWK")
}

fn ec_jwk() -> Jwk {
    Jwk::from_value(json!({
        "kty": "EC",
        "kid": "ec-1",
        "crv": "P-256",
        "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
        "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0",
    }))
    .expect("an EC JWK")
}

fn assert_rejected<T: std::fmt::Debug>(result: Result<T, ProviderError>) {
    match result {
        Err(ProviderError::Rejected(_)) => {}
        other => panic!("expected a 401 rejection, got {other:?}"),
    }
}

#[test]
fn valid_hs256_accepted() {
    let token = hs256_token(&named_claims("Ann"));
    let who = verify_hs256(&hs256_config(), &token, SECRET, NOW).expect("a valid HS256 token");
    assert_eq!(who.player_id.0, "jwt_u1");
    assert_eq!(who.provider, "jwt");
    assert_eq!(who.display_name.as_deref(), Some("Ann"));
}

#[test]
fn alg_none_rejected() {
    // Review Focus 1: `alg: none` is never an accepted algorithm.
    let header = encode(br#"{"alg":"none"}"#);
    let payload = encode(&serde_json::to_vec(&base_claims()).expect("the claims are JSON"));
    let token = format!("{header}.{payload}.");
    assert_rejected(verify_hs256(&hs256_config(), &token, SECRET, NOW));
    let jwks = jwks_config();
    assert!(!allowed_alg(&jwks.keys, &rsa_jwk()).contains(&"none"));
}

#[test]
fn hs256_token_rejected_by_jwks_key() {
    // Review Focus 1: an RSA public key must never be usable as an HMAC secret,
    // so a JWKS key accepts only asymmetric algorithms, bound per key.
    let jwks = jwks_config();
    assert_eq!(allowed_alg(&jwks.keys, &rsa_jwk()).to_vec(), ["RS256"]);
    assert!(
        !allowed_alg(&jwks.keys, &rsa_jwk()).contains(&"HS256"),
        "an RSA JWK must never accept HS256"
    );
    assert_eq!(allowed_alg(&jwks.keys, &ec_jwk()).to_vec(), ["ES256"]);
    // A key that declares an algorithm is bound to exactly that algorithm, and
    // an unsupported one is unusable rather than falling back to its `kty`.
    let declared_hs256 = Jwk::from_value(json!({
        "kty": "RSA",
        "kid": "rsa-2",
        "alg": "HS256",
        "n": "x",
        "e": "AQAB",
    }))
    .expect("a JWK");
    assert!(allowed_alg(&jwks.keys, &declared_hs256).is_empty());
}

#[test]
fn missing_exp_rejected() {
    let mut claims = base_claims();
    claims.exp = None;
    assert_rejected(check_claims(&claims, &hs256_config(), NOW));
}

#[test]
fn expired_beyond_leeway_rejected() {
    let mut claims = base_claims();
    claims.exp = Some(NOW - 61);
    assert_rejected(check_claims(&claims, &hs256_config(), NOW));
}

#[test]
fn within_leeway_accepted() {
    let mut claims = base_claims();
    claims.exp = Some(NOW - 59);
    assert!(check_claims(&claims, &hs256_config(), NOW).is_ok());
}

#[test]
fn nbf_future_rejected() {
    let mut claims = base_claims();
    claims.nbf = Some(NOW + 61);
    assert_rejected(check_claims(&claims, &hs256_config(), NOW));
}

#[test]
fn wrong_iss_rejected() {
    let mut claims = base_claims();
    claims.iss = Some("https://evil.example".to_owned());
    assert_rejected(check_claims(&claims, &hs256_config(), NOW));
}

#[test]
fn aud_array_containing_audience_accepted() {
    let mut claims = base_claims();
    claims.aud = Some(json!(["someone-else", AUDIENCE]));
    assert!(check_claims(&claims, &hs256_config(), NOW).is_ok());
}

#[test]
fn aud_mismatch_rejected() {
    let mut claims = base_claims();
    claims.aud = Some(json!("someone-else"));
    assert_rejected(check_claims(&claims, &hs256_config(), NOW));
}

#[test]
fn missing_sub_rejected() {
    let mut claims = base_claims();
    claims.sub = None;
    assert_rejected(check_claims(&claims, &hs256_config(), NOW));
}

#[test]
fn sub_namespaced() {
    // Review Focus 2: a `sub` that looks like another provider is still namespaced.
    let mut claims = base_claims();
    claims.sub = Some("steam_76561198000000000".to_owned());
    let who = identity(&hs256_config(), &claims).expect("a namespaced identity");
    assert_eq!(who.player_id.0, "jwt_steam_76561198000000000");
    assert_eq!(who.provider, "jwt");
}

#[test]
fn bad_sub_rejected() {
    let too_long = "a".repeat(200);
    for subject in ["a/b", "x\ny", too_long.as_str()] {
        let mut claims = base_claims();
        claims.sub = Some(subject.to_owned());
        assert_rejected(identity(&hs256_config(), &claims));
    }
}

#[test]
fn name_claim_sanitized() {
    let claims = named_claims("a\r\nX-Evil: 1");
    let who = identity(&hs256_config(), &claims).expect("an identity");
    assert_eq!(who.display_name.as_deref(), Some("aX-Evil: 1"));

    // No configured claim means no display name, even when one is present.
    let config = JwtConfig {
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
        keys: JwtKeys::Hs256 {
            secret_env: "HEARTH_JWT_SECRET",
        },
        name_claim: None,
    };
    assert_eq!(
        identity(&config, &named_claims("Ann"))
            .expect("an identity")
            .display_name,
        None
    );
}

#[test]
fn malformed_compact_rejected() {
    for jwt in ["", "only-two.parts", "!!!.???.###", "a.b.c.d"] {
        assert_rejected(parse(jwt));
    }
}

#[test]
fn jwks_kid_required() {
    let keys = vec![rsa_jwk()];
    let header = |value: Value| -> Header {
        parse(&compact(value, &base_claims()))
            .expect("a parsable token")
            .0
    };
    assert_rejected(select_jwk(&header(json!({ "alg": "RS256" })), &keys));
    assert_eq!(
        select_jwk(&header(json!({ "alg": "RS256", "kid": "rsa-1" })), &keys)
            .expect("the named key")
            .kid
            .as_deref(),
        Some("rsa-1")
    );
    assert_rejected(select_jwk(
        &header(json!({ "alg": "RS256", "kid": "nope" })),
        &keys,
    ));
}
