//! Native tests for the webhook allocator (plan v0.5 Task 2 Step 1).

use futures::executor::block_on;
use hearth_server::fetcher::CannedFetcher;
use hearth_server::handoff::{AllocJob, SessionId};
use hearth_server::{
    validate_endpoint, verify_webhook, Allocator, HttpReq, PlayerId, RoomName, WebhookAllocator,
};
use serde_json::json;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const ALLOCATOR_URL: &str = "https://alloc.example.test/allocate";
const PUBLIC_URL: &str = "https://game.example.test";
const ENDPOINT: &str = "wss://dedicated.example.test/ws";

fn job() -> AllocJob {
    AllocJob {
        sid: SessionId("hs_AAAA_0123456789abcdef".to_owned()),
        secret: "a".repeat(64),
        room: RoomName {
            room_type: "arena".to_owned(),
            name: "lobby-1".to_owned(),
        },
        players: vec![
            PlayerId("jwt_alice".to_owned()),
            PlayerId("jwt_bob".to_owned()),
        ],
        claims: json!({ "map": "dust" }),
        region: Some("enam".to_owned()),
        max_duration_ms: 0,
        attempt: 1,
    }
}

fn allocator(fetcher: CannedFetcher) -> WebhookAllocator<CannedFetcher> {
    WebhookAllocator {
        url: ALLOCATOR_URL.to_owned(),
        secret: SECRET.to_vec(),
        public_url: PUBLIC_URL.to_owned(),
        allow_insecure_endpoints: false,
        fetcher,
    }
}

fn header(request: &HttpReq, name: &str) -> String {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| panic!("missing header {name}"))
}

/// Run one allocation and hand back the request it sent.
fn recorded_request(allocator: &WebhookAllocator<CannedFetcher>) -> HttpReq {
    allocator
        .fetcher
        .requests()
        .into_iter()
        .next()
        .expect("one allocation request")
}

#[test]
fn request_signed_and_verifiable() {
    let fetcher = CannedFetcher::new().route(
        ALLOCATOR_URL,
        200,
        &json!({ "endpoint": ENDPOINT }).to_string(),
    );
    let allocator = allocator(fetcher);
    let job = job();

    let endpoint = block_on(allocator.allocate(&job)).expect("allocation succeeds");
    assert_eq!(endpoint, ENDPOINT);

    let request = recorded_request(&allocator);
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, ALLOCATOR_URL);
    let body = request.body.clone().expect("a body");
    let timestamp = header(&request, "X-Hearth-Timestamp");
    let signature = header(&request, "X-Hearth-Signature");
    let now_s = timestamp.parse::<u64>().expect("unix seconds");
    assert!(verify_webhook(SECRET, &timestamp, &body, &signature, now_s));

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(parsed["sid"], job.sid.as_str());
    assert_eq!(parsed["session_secret"], job.secret);
    assert_eq!(parsed["room"]["room_type"], "arena");
    assert_eq!(parsed["room"]["name"], "lobby-1");
    assert_eq!(parsed["players"], json!(["jwt_alice", "jwt_bob"]));
    assert_eq!(parsed["claims"], json!({ "map": "dust" }));
    assert_eq!(parsed["region"], "enam");
    assert_eq!(
        parsed["report_url"],
        format!("{PUBLIC_URL}/handoff/{}", job.sid.as_str())
    );
}

#[test]
fn tampered_body_fails_verify() {
    let fetcher = CannedFetcher::new().route(
        ALLOCATOR_URL,
        200,
        &json!({ "endpoint": ENDPOINT }).to_string(),
    );
    let allocator = allocator(fetcher);
    block_on(allocator.allocate(&job())).expect("allocation succeeds");
    let request = recorded_request(&allocator);
    let body = request.body.clone().expect("a body");
    let timestamp = header(&request, "X-Hearth-Timestamp");
    let signature = header(&request, "X-Hearth-Signature");
    let now_s = timestamp.parse::<u64>().expect("unix seconds");

    let tampered = body.replace("dust", "nuke");
    assert_ne!(tampered, body);
    assert!(!verify_webhook(
        SECRET, &timestamp, &tampered, &signature, now_s
    ));
    // A different secret cannot verify either.
    assert!(!verify_webhook(
        b"fedcba9876543210fedcba9876543210",
        &timestamp,
        &body,
        &signature,
        now_s
    ));
}

#[test]
fn stale_timestamp_fails_verify() {
    let fetcher = CannedFetcher::new().route(
        ALLOCATOR_URL,
        200,
        &json!({ "endpoint": ENDPOINT }).to_string(),
    );
    let allocator = allocator(fetcher);
    block_on(allocator.allocate(&job())).expect("allocation succeeds");
    let request = recorded_request(&allocator);
    let body = request.body.clone().expect("a body");
    let timestamp = header(&request, "X-Hearth-Timestamp");
    let signature = header(&request, "X-Hearth-Signature");
    let now_s = timestamp.parse::<u64>().expect("unix seconds");

    assert!(verify_webhook(SECRET, &timestamp, &body, &signature, now_s));
    // The window is 300 s in both directions.
    assert!(verify_webhook(
        SECRET,
        &timestamp,
        &body,
        &signature,
        now_s + 300
    ));
    assert!(!verify_webhook(
        SECRET,
        &timestamp,
        &body,
        &signature,
        now_s + 301
    ));
    assert!(!verify_webhook(
        SECRET,
        &timestamp,
        &body,
        &signature,
        now_s.saturating_sub(301)
    ));
    assert!(!verify_webhook(
        SECRET,
        "not-a-timestamp",
        &body,
        &signature,
        now_s
    ));
    assert!(!verify_webhook(SECRET, &timestamp, &body, "abc", now_s));
}

#[test]
fn endpoint_validated() {
    // Review Focus 5: allocator output is a capability, so only transport
    // endpoints the clients can actually meet are accepted.
    for (endpoint, allow_insecure, expected) in [
        ("javascript:alert(1)", false, false),
        ("http://x", false, false),
        ("", false, false),
        ("ws://x", false, false),
        ("wss://", false, false),
        ("udp://1.2.3.4:7777", false, true),
        ("wss://a.b/c", false, true),
        ("ws://127.0.0.1:1", true, true),
        ("ws://x\ny", true, false),
        ("wss://a.b/€", false, false),
    ] {
        assert_eq!(
            validate_endpoint(endpoint, allow_insecure),
            expected,
            "{endpoint:?} allow_insecure={allow_insecure}"
        );
    }
    // The tail after the scheme is 1..=250 characters.
    assert!(validate_endpoint(
        &format!("wss://{}", "a".repeat(250)),
        false
    ));
    assert!(!validate_endpoint(
        &format!("wss://{}", "a".repeat(251)),
        false
    ));

    // An endpoint that fails validation fails the whole allocation.
    let fetcher = CannedFetcher::new().route(
        ALLOCATOR_URL,
        200,
        &json!({ "endpoint": "javascript:alert(1)" }).to_string(),
    );
    assert!(block_on(allocator(fetcher).allocate(&job())).is_err());
}

#[test]
fn conflict_409_same_endpoint_is_ok() {
    // The idempotent retry path: the allocator already has this sid's server.
    let fetcher = CannedFetcher::new().route(
        ALLOCATOR_URL,
        409,
        &json!({ "endpoint": ENDPOINT }).to_string(),
    );
    assert_eq!(
        block_on(allocator(fetcher).allocate(&job())).expect("409 is retryable"),
        ENDPOINT
    );
    // A 409 without an endpoint is still a failure.
    let fetcher = CannedFetcher::new().route(ALLOCATOR_URL, 409, "{}");
    assert!(block_on(allocator(fetcher).allocate(&job())).is_err());
}

#[test]
fn upstream_500_is_err() {
    for fetcher in [
        CannedFetcher::new().route(ALLOCATOR_URL, 500, "boom"),
        CannedFetcher::new().failing("timeout"),
        CannedFetcher::new().route(ALLOCATOR_URL, 200, "<html>not json</html>"),
    ] {
        assert!(block_on(allocator(fetcher).allocate(&job())).is_err());
    }
}

#[test]
fn stop_signed() {
    let fetcher = CannedFetcher::new().route(ALLOCATOR_URL, 204, "");
    let allocator = allocator(fetcher);
    let job = job();
    block_on(allocator.stop(&job.sid)).expect("stop succeeds");

    let request = recorded_request(&allocator);
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, ALLOCATOR_URL);
    let body = request.body.clone().expect("a body");
    let timestamp = header(&request, "X-Hearth-Timestamp");
    let signature = header(&request, "X-Hearth-Signature");
    let now_s = timestamp.parse::<u64>().expect("unix seconds");
    assert!(verify_webhook(SECRET, &timestamp, &body, &signature, now_s));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).expect("JSON body"),
        json!({ "sid": job.sid.as_str(), "action": "stop" })
    );
}
