//! Webhook allocation for handoff sessions.
//!
//! The room adapter POSTs a signed allocation request to the operator's fleet
//! allocator and hands the answered endpoint to clients. The contract:
//!
//! 1. `POST <HEARTH_ALLOCATOR_URL>` with a JSON body and two headers:
//!    `X-Hearth-Timestamp` (unix seconds) and `X-Hearth-Signature`
//!    (`hex(HMAC-SHA256(HEARTH_ALLOCATOR_SECRET, "<timestamp>.<body>"))`).
//! 2. `200 {"endpoint": "..."}` for a fresh allocation, or
//!    `409 {"endpoint": "..."}` when this `sid` was already allocated by an
//!    earlier attempt (the idempotent path). The endpoint is validated before a
//!    client ever sees it; a bad endpoint fails the allocation.
//! 3. `POST` with `{"sid": "...", "action": "stop"}` to release a session's
//!    server, signed the same way.
//!
//! [`verify_webhook`] is exported so an allocator written in Rust can reuse the
//! exact check; `docs/handoff.md` has the JavaScript equivalent.

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::fetcher::{Fetcher, HttpReq};
use crate::handoff::{constant_time_eq, AllocJob, SessionId};

type HmacSha256 = Hmac<Sha256>;

/// The header carrying the unix-second timestamp the signature covers.
pub const TIMESTAMP_HEADER: &str = "X-Hearth-Timestamp";
/// The header carrying `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`.
pub const SIGNATURE_HEADER: &str = "X-Hearth-Signature";
/// How far a webhook timestamp may be from the receiver's clock.
pub const MAX_SKEW_S: u64 = 300;
/// The longest endpoint tail an allocator may return.
pub const MAX_ENDPOINT_LEN: usize = 250;

/// Why a session's server could not be obtained or released. A webhook
/// allocator has no richer information to give the runtime.
#[allow(async_fn_in_trait)]
pub trait Allocator {
    /// Ask for a server, returning its client-facing endpoint.
    async fn allocate(&self, job: &AllocJob) -> Result<String, String>;
    /// Release the server for a finished session. Best effort.
    async fn stop(&self, sid: &SessionId) -> Result<(), String>;
}

/// The config-driven allocator: one signed webhook.
pub struct WebhookAllocator<F: Fetcher> {
    /// `HEARTH_ALLOCATOR_URL`.
    pub url: String,
    /// `HEARTH_ALLOCATOR_SECRET`, at least 32 bytes.
    pub secret: Vec<u8>,
    /// `HEARTH_PUBLIC_URL`: the gateway's public base, used for `report_url`.
    pub public_url: String,
    /// Allows `ws://` endpoints for local development. Never set in production.
    pub allow_insecure_endpoints: bool,
    pub fetcher: F,
}

impl<F: Fetcher> Allocator for WebhookAllocator<F> {
    async fn allocate(&self, job: &AllocJob) -> Result<String, String> {
        let body = allocation_body(job, &self.public_url);
        let answer = self.call(&body).await?;
        // 409 is the idempotent retry: the allocator already has a server for
        // this sid, so it answers with the same endpoint the first attempt got.
        if answer.status != 200 && answer.status != 409 {
            return Err(format!("allocator answered HTTP {}", answer.status));
        }
        let endpoint = endpoint_from(&answer.body)?;
        if validate_endpoint(&endpoint, self.allow_insecure_endpoints) {
            Ok(endpoint)
        } else {
            Err("allocator returned an invalid endpoint".to_owned())
        }
    }

    async fn stop(&self, sid: &SessionId) -> Result<(), String> {
        let body = serde_json::json!({ "sid": sid.as_str(), "action": "stop" }).to_string();
        let answer = self.call(&body).await?;
        if (200..300).contains(&answer.status) {
            Ok(())
        } else {
            Err(format!("allocator stop answered HTTP {}", answer.status))
        }
    }
}

impl<F: Fetcher> WebhookAllocator<F> {
    /// POST one signed body and hand back the raw answer. A transport failure is
    /// an `Err`; an HTTP status is not.
    async fn call(&self, body: &str) -> Result<crate::fetcher::HttpResp, String> {
        let timestamp = now_s().to_string();
        let signature = sign(&self.secret, &timestamp, body);
        let request = HttpReq {
            method: "POST",
            url: self.url.clone(),
            headers: vec![
                ("Content-Type".to_owned(), "application/json".to_owned()),
                (TIMESTAMP_HEADER.to_owned(), timestamp),
                (SIGNATURE_HEADER.to_owned(), signature),
            ],
            body: Some(body.to_owned()),
        };
        self.fetcher.send(request).await
    }
}

/// The JSON body of an allocation request.
fn allocation_body(job: &AllocJob, public_url: &str) -> String {
    let report_url = format!(
        "{}/handoff/{}",
        public_url.trim_end_matches('/'),
        job.sid.as_str()
    );
    serde_json::json!({
        "sid": job.sid.as_str(),
        "session_secret": job.secret,
        "room": {
            "room_type": job.room.room_type.as_str(),
            "name": job.room.name.as_str(),
        },
        "players": &job.players,
        "claims": &job.claims,
        "region": &job.region,
        "report_url": report_url,
    })
    .to_string()
}

#[derive(Deserialize)]
struct AllocationAnswer {
    endpoint: String,
}

fn endpoint_from(body: &str) -> Result<String, String> {
    serde_json::from_str::<AllocationAnswer>(body)
        .map(|answer| answer.endpoint)
        .map_err(|_| "allocator answer must be JSON: {\"endpoint\": \"...\"}".to_owned())
}

/// Verify an allocator's webhook signature and freshness.
///
/// `timestamp` is the `X-Hearth-Timestamp` header, `body` the raw request body
/// and `signature_hex` the `X-Hearth-Signature` header. The comparison is
/// constant time, and a timestamp further than [`MAX_SKEW_S`] from `now_s` is
/// rejected so a captured request cannot be replayed later.
pub fn verify_webhook(
    secret: &[u8],
    timestamp: &str,
    body: &str,
    signature_hex: &str,
    now_s: u64,
) -> bool {
    let Ok(parsed) = timestamp.parse::<u64>() else {
        return false;
    };
    if now_s.abs_diff(parsed) > MAX_SKEW_S {
        return false;
    }
    let expected = sign(secret, timestamp, body);
    if expected.len() != signature_hex.len() {
        return false;
    }
    // Hex is case-insensitive; fold before the constant-time compare.
    constant_time_eq(
        expected.as_bytes(),
        signature_hex.to_ascii_lowercase().as_bytes(),
    )
}

/// Whether an allocator-provided endpoint is safe to hand to a client.
///
/// Accepted schemes are `udp`, `wss` and `https`; `ws` is accepted only when
/// `allow_insecure` is set for local development. The tail after the scheme must
/// be 1 to [`MAX_ENDPOINT_LEN`] ASCII characters with no whitespace or control
/// characters, so an allocator cannot inject a `javascript:` URL, a header break
/// or an absurdly long string.
pub fn validate_endpoint(endpoint: &str, allow_insecure: bool) -> bool {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return false;
    };
    let scheme_allowed = match scheme {
        "udp" | "wss" | "https" => true,
        "ws" => allow_insecure,
        _ => false,
    };
    if !scheme_allowed || rest.is_empty() || rest.len() > MAX_ENDPOINT_LEN {
        return false;
    }
    endpoint.is_ascii()
        && !endpoint
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
}

/// `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`, lower-case.
fn sign(secret: &[u8], timestamp: &str, body: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Unix seconds.
///
/// On the Workers target the runtime clock is the only one that exists; natively
/// `SystemTime` works even when feature unification enabled the `worker` feature
/// for another crate in the same test run (the Worker's `Date` panics off-wasm).
#[cfg(all(target_arch = "wasm32", feature = "worker"))]
fn now_s() -> u64 {
    worker::Date::now().as_millis() / 1000
}

#[cfg(not(all(target_arch = "wasm32", feature = "worker")))]
fn now_s() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
