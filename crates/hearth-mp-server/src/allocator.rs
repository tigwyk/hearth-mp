//! Handoff allocation: an operator's webhook fleet or Cloudflare Containers.
//!
//! ## Webhook allocators
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
//!
//! ## Container allocators
//!
//! [`ContainerAllocator`] asks a [`ContainerStarter`] to start one Cloudflare
//! Container per session and returns the gateway's `.../handoff/{sid}/connect`
//! WebSocket URL. The starter is a seam: the Worker adapter implements it over
//! the `hearth_mp_server::container::ContainerObject` call path, and native tests
//! use a fake. Nothing here needs the `worker` feature.

use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::fetcher::{Fetcher, HttpReq};
use crate::handoff::{constant_time_eq, AllocJob, SessionId, DEFAULT_MAX_DURATION};

type HmacSha256 = Hmac<Sha256>;

/// The header carrying the unix-second timestamp the signature covers.
pub const TIMESTAMP_HEADER: &str = "X-Hearth-Timestamp";
/// The header carrying `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`.
pub const SIGNATURE_HEADER: &str = "X-Hearth-Signature";
#[doc(hidden)]
/// How far a webhook timestamp may be from the receiver's clock.
pub const MAX_SKEW_S: u64 = 300;
#[doc(hidden)]
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
    /// How the allocator makes HTTP requests (`WorkerFetcher` in production).
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

#[doc(hidden)]
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

#[doc(hidden)]
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

/// The port a session container serves HTTP/WebSocket on unless told otherwise.
pub const DEFAULT_CONTAINER_PORT: u16 = 8080;

/// Everything a container needs to start one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartRequest {
    /// The session to start.
    pub sid: SessionId,
    /// The plaintext session secret. It is delivered only to the container's
    /// start message, never to a client.
    pub secret: String,
    /// `<public_url>/handoff/<sid>`: where the container reports lifecycle
    /// events (the SDK appends `/report`).
    pub report_url: String,
    /// The port the container serves HTTP/WebSocket on.
    pub port: u16,
    /// The developer's extra environment. `HEARTH_*` names are ignored by the
    /// adapter, so a session's identity cannot be spoofed.
    pub env: Vec<(String, String)>,
    /// The session's maximum duration, the container's self-destruct backstop.
    pub max_duration_ms: u64,
}

/// Starts and stops the server for one session.
///
/// The Worker adapter implements this by calling a session's container object;
/// native tests use a fake. It is host-independent on purpose, so the allocator
/// logic is testable without Workers.
#[allow(async_fn_in_trait)]
pub trait ContainerStarter {
    /// Start this session's server and return only once it is healthy.
    async fn start(&self, request: StartRequest) -> Result<(), String>;
    /// Destroy this session's server. Best effort: it is only called for
    /// sessions that already started.
    async fn stop(&self, sid: &SessionId) -> Result<(), String>;
}

/// One Cloudflare Container per session, reached through the gateway's
/// `/handoff/{sid}/connect` WebSocket proxy.
pub struct ContainerAllocator<S: ContainerStarter> {
    /// Starts and stops each session's container.
    pub starter: S,
    /// The gateway's public base URL (`https://...` in production, `http://...`
    /// for local dev, which yields a `ws://` connect URL).
    pub public_url: String,
    /// The port the container serves HTTP/WebSocket on.
    pub port: u16,
    /// Extra environment for the container (cannot override `HEARTH_*`).
    pub env: Vec<(String, String)>,
    /// The container's backstop: it is destroyed this long after it starts.
    pub max_duration: Duration,
}

impl<S: ContainerStarter> ContainerAllocator<S> {
    /// Defaults: port [`DEFAULT_CONTAINER_PORT`], no extra env, and the room's
    /// default maximum session duration.
    pub fn new(starter: S, public_url: impl Into<String>) -> Self {
        Self {
            starter,
            public_url: public_url.into(),
            port: DEFAULT_CONTAINER_PORT,
            env: Vec::new(),
            max_duration: DEFAULT_MAX_DURATION,
        }
    }
}

impl<S: ContainerStarter> Allocator for ContainerAllocator<S> {
    async fn allocate(&self, job: &AllocJob) -> Result<String, String> {
        let report_url = format!(
            "{}/handoff/{}",
            self.public_url.trim_end_matches('/'),
            job.sid.as_str()
        );
        // The room knows each session's own deadline; the allocator's default is
        // only a fallback for callers that hand in a bare job.
        let max_duration_ms = if job.max_duration_ms == 0 {
            self.max_duration.as_millis().min(u64::MAX as u128) as u64
        } else {
            job.max_duration_ms
        };
        self.starter
            .start(StartRequest {
                sid: job.sid.clone(),
                secret: job.secret.clone(),
                report_url,
                port: self.port,
                env: self.env.clone(),
                max_duration_ms,
            })
            .await?;
        Ok(connect_url(&self.public_url, &job.sid))
    }

    async fn stop(&self, sid: &SessionId) -> Result<(), String> {
        self.starter.stop(sid).await
    }
}

/// The gateway's WebSocket route for a session: `wss://` for an `https` public
/// URL, `ws://` for `http`, and `wss` when the URL has no scheme.
fn connect_url(public_url: &str, sid: &SessionId) -> String {
    let (scheme, host) = if let Some(host) = public_url.strip_prefix("https://") {
        ("wss", host)
    } else if let Some(host) = public_url.strip_prefix("http://") {
        ("ws", host)
    } else {
        ("wss", public_url)
    };
    format!(
        "{scheme}://{}/handoff/{}/connect",
        host.trim_end_matches('/'),
        sid.as_str()
    )
}
