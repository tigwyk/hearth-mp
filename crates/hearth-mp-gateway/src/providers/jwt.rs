//! JWT provider: a signed token from the developer's own backend.
//!
//! HS256 is verified in-crate with `hmac`; RS256/ES256 are verified against a
//! JWKS through the Workers WebCrypto API ([`crate::webcrypto`]). The algorithm
//! is bound to the key type, never chosen by the token: an HS256 secret accepts
//! only HS256, and a JWKS key accepts only the algorithm its JWK declares (or
//! the one its `kty`/`crv` implies). That binding is what closes algorithm
//! confusion.

use std::sync::{Mutex, OnceLock};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::Sha256;
use worker::{Date, Env};

use crate::auth::{namespaced, sanitize_name, Identity};
use crate::webcrypto;

use super::{Fetcher, HttpReq, ProviderError};

type HmacSha256 = Hmac<Sha256>;

/// Clock skew allowed on `exp` and `nbf`.
const LEEWAY_S: u64 = 60;
/// How long a fetched JWKS is trusted.
const JWKS_TTL_MS: u64 = 10 * 60 * 1_000;
/// The shortest gap between refetches triggered by an unknown `kid`, so a forged
/// `kid` cannot turn into a JWKS request flood.
const JWKS_REFETCH_MS: u64 = 60 * 1_000;
/// HS256 needs a real secret; `AuthConfig::from_env` rejects shorter ones.
const MIN_HS256_SECRET: usize = 32;

const HS256: &[&str] = &["HS256"];
const RS256: &[&str] = &["RS256"];
const ES256: &[&str] = &["ES256"];

/// How to authenticate a JWT's signature.
pub enum JwtKeys {
    /// One shared HS256 secret, read from this binding at request time.
    Hs256 {
        /// The binding that holds the secret.
        secret_env: &'static str,
    },
    /// A JWKS document, refetched on an unknown `kid` (cached for 10 minutes).
    Jwks {
        /// The JWKS document's URL.
        url: String,
    },
}

/// Which tokens this deployment accepts.
#[non_exhaustive]
pub struct JwtConfig {
    /// The `iss` every token must carry.
    pub issuer: String,
    /// The value `aud` must contain.
    pub audience: String,
    /// How signatures are checked.
    pub keys: JwtKeys,
    /// The claim to read a display name from, e.g. `"name"`.
    pub name_claim: Option<String>,
}

impl JwtConfig {
    /// Accept tokens from `issuer` for `audience`, checked with `keys`; no display
    /// name claim (set `name_claim` to read one).
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>, keys: JwtKeys) -> Self {
        Self { issuer: issuer.into(), audience: audience.into(), keys, name_claim: None }
    }
}

#[doc(hidden)]
/// The protected header of a compact JWS.
#[derive(Debug, Clone, Deserialize)]
pub struct Header {
    /// The signing algorithm (`HS256` or `ES256` are accepted).
    pub alg: String,
    #[serde(default)]
    /// The key id, for JWKS lookup.
    pub kid: Option<String>,
}

#[doc(hidden)]
/// The claims this provider reads. Everything else lands in [`Claims::extra`], so
/// a configured `name_claim` can name any non-registered claim.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
pub struct Claims {
    #[serde(default)]
    /// The issuer.
    pub iss: Option<String>,
    /// A string or an array of strings.
    #[serde(default)]
    pub aud: Option<Value>,
    #[serde(default)]
    /// The subject: becomes the player id `jwt_<sub>`.
    pub sub: Option<String>,
    #[serde(default)]
    /// Expiry, Unix seconds.
    pub exp: Option<u64>,
    #[serde(default)]
    /// Not-before, Unix seconds.
    pub nbf: Option<u64>,
    #[serde(flatten)]
    /// Every other claim.
    pub extra: Map<String, Value>,
}

#[doc(hidden)]
/// One public key from a JWKS document.
#[derive(Debug, Clone)]
pub struct Jwk {
    /// The key type (`EC` or `oct`).
    pub kty: String,
    /// The key id.
    pub kid: Option<String>,
    /// The algorithm the key declares, when it declares one.
    pub alg: Option<String>,
    /// The curve, for EC keys (`P-256`).
    pub crv: Option<String>,
    /// The original JSON object, handed to WebCrypto's `importKey`.
    pub json: Value,
}

impl Jwk {
    /// Parse one JWKS `keys` entry; entries that are not key objects are ignored.
    pub fn from_value(json: Value) -> Option<Self> {
        let kty = json.get("kty")?.as_str()?.to_owned();
        Some(Self {
            kid: json.get("kid").and_then(Value::as_str).map(str::to_owned),
            alg: json.get("alg").and_then(Value::as_str).map(str::to_owned),
            crv: json.get("crv").and_then(Value::as_str).map(str::to_owned),
            kty,
            json,
        })
    }
}

#[derive(Deserialize)]
struct JwksDocument {
    keys: Vec<Value>,
}

/// A JWKS fetched by this isolate. One entry is enough: a gateway deployment
/// serves one issuer.
struct CachedJwks {
    url: String,
    keys: Vec<Jwk>,
    fetched_at_ms: u64,
}

#[doc(hidden)]
/// Split and decode a compact JWS into its header, claims, signing input and
/// signature. Every structural problem is a rejection (401).
pub fn parse(jwt: &str) -> Result<(Header, Claims, Vec<u8>, Vec<u8>), ProviderError> {
    let mut parts = jwt.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(rejected("jwt is not a compact JWS"));
    };
    let decoded_header = decode::<Header>(header)?;
    let claims = decode::<Claims>(payload)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| rejected("jwt signature is not base64url"))?;
    Ok((
        decoded_header,
        claims,
        format!("{header}.{payload}").into_bytes(),
        signature,
    ))
}

#[doc(hidden)]
/// The claim rules. `exp` is required; everything is checked with
/// [`LEEWAY_S`] of clock skew.
pub fn check_claims(claims: &Claims, cfg: &JwtConfig, now_s: u64) -> Result<(), ProviderError> {
    let exp = claims.exp.ok_or_else(|| rejected("jwt has no exp"))?;
    if now_s > exp.saturating_add(LEEWAY_S) {
        return Err(rejected("jwt is expired"));
    }
    if let Some(nbf) = claims.nbf {
        if now_s.saturating_add(LEEWAY_S) < nbf {
            return Err(rejected("jwt is not valid yet"));
        }
    }
    if claims.iss.as_deref() != Some(cfg.issuer.as_str()) {
        return Err(rejected("jwt issuer mismatch"));
    }
    if !audience_matches(claims.aud.as_ref(), &cfg.audience) {
        return Err(rejected("jwt audience mismatch"));
    }
    if claims.sub.is_none() {
        return Err(rejected("jwt has no sub"));
    }
    Ok(())
}

/// The algorithms a header may use with this key.
///
/// HS256 keys accept only HS256. A JWKS key accepts only the `alg` its JWK
/// declares, or — when it declares none — RS256 for `kty: "RSA"` and ES256 for
/// `kty: "EC", crv: "P-256"`. Everything else, including `none`, is rejected.
pub fn allowed_alg<'a>(keys: &JwtKeys, jwk: &'a Jwk) -> &'a [&'a str] {
    match keys {
        JwtKeys::Hs256 { .. } => HS256,
        JwtKeys::Jwks { .. } => match jwk.alg.as_deref() {
            Some("RS256") => RS256,
            Some("ES256") => ES256,
            // A key that declares an unsupported algorithm is unusable, and a
            // declared algorithm never falls back to the kty default.
            Some(_) => &[],
            None => match (jwk.kty.as_str(), jwk.crv.as_deref()) {
                ("RSA", _) => RS256,
                ("EC", Some("P-256")) => ES256,
                _ => &[],
            },
        },
    }
}

#[doc(hidden)]
/// The identity a verified set of claims names.
///
/// The id is always namespaced `jwt_`, whatever the `sub` claims; a `sub` that
/// would break out of its namespace is rejected.
pub fn identity(cfg: &JwtConfig, claims: &Claims) -> Result<Identity, ProviderError> {
    let sub = claims
        .sub
        .as_deref()
        .ok_or_else(|| rejected("jwt has no sub"))?;
    let player_id =
        namespaced("jwt", sub).map_err(|_| rejected("jwt sub is not a usable subject"))?;
    let display_name = cfg
        .name_claim
        .as_deref()
        .and_then(|claim| claims.extra.get(claim))
        .and_then(Value::as_str)
        .and_then(sanitize_name);
    Ok(Identity {
        player_id,
        provider: "jwt",
        display_name,
    })
}

/// The JWKS key named by the header's `kid`. A JWKS token without a `kid` is
/// rejected: guessing which key signed a token is not a verification.
pub fn select_jwk<'a>(header: &Header, keys: &'a [Jwk]) -> Result<&'a Jwk, ProviderError> {
    if header.kid.is_none() {
        return Err(rejected("jwt header has no kid"));
    }
    key_for(header, keys).ok_or_else(|| rejected("jwt kid is not in the JWKS"))
}

#[doc(hidden)]
/// Verify an HS256 token against a secret already resolved from the Worker's
/// environment, and return its identity.
pub fn verify_hs256(
    cfg: &JwtConfig,
    jwt: &str,
    secret: &[u8],
    now_s: u64,
) -> Result<Identity, ProviderError> {
    let (header, claims, signing_input, signature) = parse(jwt)?;
    if !alg_allowed(&cfg.keys, &header.alg, None) {
        return Err(rejected("jwt algorithm is not allowed for this key"));
    }
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| ProviderError::Upstream("jwt signing secret is unusable".to_owned()))?;
    mac.update(&signing_input);
    mac.verify_slice(&signature)
        .map_err(|_| rejected("jwt signature is invalid"))?;
    check_claims(&claims, cfg, now_s)?;
    identity(cfg, &claims)
}

/// `{"jwt": "<compact JWS>"}`.
pub async fn verify(
    cfg: &JwtConfig,
    body: &Value,
    env: &Env,
    fetcher: &impl Fetcher,
) -> Result<Identity, ProviderError> {
    let jwt = body
        .get("jwt")
        .and_then(Value::as_str)
        .filter(|jwt| !jwt.is_empty())
        .ok_or_else(|| {
            ProviderError::BadRequest("body must be JSON: {\"jwt\": \"<compact JWS>\"}".to_owned())
        })?;
    let now_ms = Date::now().as_millis();
    let now_s = now_ms / 1_000;
    match &cfg.keys {
        JwtKeys::Hs256 { secret_env } => {
            let secret = env
                .secret(secret_env)
                .map(|secret| secret.to_string())
                .unwrap_or_default();
            if secret.len() < MIN_HS256_SECRET {
                return Err(ProviderError::Upstream(
                    "jwt signing secret is not configured".to_owned(),
                ));
            }
            verify_hs256(cfg, jwt, secret.as_bytes(), now_s)
        }
        JwtKeys::Jwks { url } => verify_jwks(cfg, url, jwt, now_s, now_ms, fetcher).await,
    }
}

async fn verify_jwks(
    cfg: &JwtConfig,
    url: &str,
    jwt: &str,
    now_s: u64,
    now_ms: u64,
    fetcher: &impl Fetcher,
) -> Result<Identity, ProviderError> {
    let (header, claims, signing_input, signature) = parse(jwt)?;
    let cached = cached_jwks(url, now_ms);

    // Fast path: a known kid from a cache entry that is still fresh.
    if let Some((keys, fetched_at_ms)) = &cached {
        if let Some(jwk) = key_for(&header, keys) {
            return finish_jwks(
                cfg,
                &header,
                &claims,
                &signing_input,
                &signature,
                jwk,
                now_s,
            )
            .await;
        }
        // An unknown kid may be a key that was added upstream, but refetching at
        // most once a minute keeps a forged kid from hammering the JWKS URL.
        if now_ms.saturating_sub(*fetched_at_ms) < JWKS_REFETCH_MS {
            return Err(rejected("jwt kid is not in the JWKS"));
        }
    }

    let keys = match fetch_jwks(url, fetcher).await {
        Ok(keys) => {
            store_jwks(url, &keys, now_ms);
            keys
        }
        Err(err) => {
            // A broken JWKS with no usable cache is an upstream outage; with a
            // cache, "unknown kid" is still an ordinary rejection.
            return if cached.as_ref().is_some_and(|(keys, _)| !keys.is_empty()) {
                Err(rejected("jwt kid is not in the JWKS"))
            } else {
                Err(err)
            };
        }
    };
    let jwk = key_for(&header, &keys).ok_or_else(|| rejected("jwt kid is not in the JWKS"))?;
    finish_jwks(
        cfg,
        &header,
        &claims,
        &signing_input,
        &signature,
        jwk,
        now_s,
    )
    .await
}

async fn finish_jwks(
    cfg: &JwtConfig,
    header: &Header,
    claims: &Claims,
    signing_input: &[u8],
    signature: &[u8],
    jwk: &Jwk,
    now_s: u64,
) -> Result<Identity, ProviderError> {
    if !alg_allowed(&cfg.keys, &header.alg, Some(jwk)) {
        return Err(rejected("jwt algorithm is not allowed for this key"));
    }
    match webcrypto::verify(&header.alg, &jwk.json, signing_input, signature).await {
        Ok(true) => {}
        Ok(false) => return Err(rejected("jwt signature is invalid")),
        // The token is not at fault if the runtime cannot run the check.
        Err(_) => {
            return Err(ProviderError::Upstream(
                "jwt signature check unavailable".to_owned(),
            ))
        }
    }
    check_claims(claims, cfg, now_s)?;
    identity(cfg, claims)
}

async fn fetch_jwks(url: &str, fetcher: &impl Fetcher) -> Result<Vec<Jwk>, ProviderError> {
    let request = HttpReq {
        method: "GET",
        url: url.to_owned(),
        headers: Vec::new(),
        body: None,
    };
    let response = fetcher.send(request).await.map_err(|_| upstream())?;
    if response.status != 200 {
        return Err(upstream());
    }
    let document: JwksDocument = serde_json::from_str(&response.body).map_err(|_| upstream())?;
    Ok(document
        .keys
        .into_iter()
        .filter_map(Jwk::from_value)
        .collect())
}

fn cached_jwks(url: &str, now_ms: u64) -> Option<(Vec<Jwk>, u64)> {
    let cache = jwks_cache()
        .lock()
        .expect("the JWKS cache lock is not poisoned");
    let cached = cache.as_ref()?;
    let fresh = cached.url == url && now_ms.saturating_sub(cached.fetched_at_ms) < JWKS_TTL_MS;
    fresh.then(|| (cached.keys.clone(), cached.fetched_at_ms))
}

fn store_jwks(url: &str, keys: &[Jwk], now_ms: u64) {
    let mut cache = jwks_cache()
        .lock()
        .expect("the JWKS cache lock is not poisoned");
    *cache = Some(CachedJwks {
        url: url.to_owned(),
        keys: keys.to_vec(),
        fetched_at_ms: now_ms,
    });
}

/// The per-isolate JWKS cache. A Worker isolate is single-threaded, so a `Mutex`
/// around a plain `Option` is the whole cache.
fn jwks_cache() -> &'static Mutex<Option<CachedJwks>> {
    static CACHE: OnceLock<Mutex<Option<CachedJwks>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn key_for<'a>(header: &Header, keys: &'a [Jwk]) -> Option<&'a Jwk> {
    let kid = header.kid.as_deref()?;
    keys.iter().find(|key| key.kid.as_deref() == Some(kid))
}

fn alg_allowed(keys: &JwtKeys, alg: &str, jwk: Option<&Jwk>) -> bool {
    match (keys, jwk) {
        (JwtKeys::Hs256 { .. }, _) => alg == "HS256",
        (JwtKeys::Jwks { .. }, Some(jwk)) => allowed_alg(keys, jwk).contains(&alg),
        (JwtKeys::Jwks { .. }, None) => false,
    }
}

fn audience_matches(aud: Option<&Value>, expected: &str) -> bool {
    match aud {
        Some(Value::String(audience)) => audience == expected,
        Some(Value::Array(audiences)) => audiences
            .iter()
            .any(|audience| audience.as_str() == Some(expected)),
        _ => false,
    }
}

fn decode<T: DeserializeOwned>(part: &str) -> Result<T, ProviderError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| rejected("jwt is not base64url"))?;
    serde_json::from_slice(&bytes).map_err(|_| rejected("jwt part is not JSON"))
}

fn rejected(message: &str) -> ProviderError {
    ProviderError::Rejected(message.to_owned())
}

fn upstream() -> ProviderError {
    ProviderError::Upstream("jwt keys unavailable".to_owned())
}
