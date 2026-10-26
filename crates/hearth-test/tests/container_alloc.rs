//! Native tests for the host-independent container allocator (plan v0.5 Task 3
//! Step 1, the part that does not need the Durable Object adapter).

use std::cell::RefCell;
use std::time::Duration;

use futures::executor::block_on;
use hearth_server::handoff::{AllocJob, DEFAULT_MAX_DURATION};
use hearth_server::{
    Allocator, ContainerAllocator, ContainerStarter, PlayerId, RoomName, SessionId, StartRequest,
    DEFAULT_CONTAINER_PORT,
};

const PUBLIC_URL: &str = "https://game.example.test";

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
        claims: serde_json::json!({ "map": "dust" }),
        region: Some("enam".to_owned()),
        attempt: 1,
    }
}

/// Records what the allocator asked for; can fail on demand.
#[derive(Default)]
struct FakeStarter {
    starts: RefCell<Vec<StartRequest>>,
    stops: RefCell<Vec<SessionId>>,
    error: Option<String>,
}

impl ContainerStarter for FakeStarter {
    async fn start(&self, request: StartRequest) -> Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.starts.borrow_mut().push(request);
        Ok(())
    }

    async fn stop(&self, sid: &SessionId) -> Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.stops.borrow_mut().push(sid.clone());
        Ok(())
    }
}

#[test]
fn allocate_passes_start_request_and_returns_connect_url() {
    let allocator = ContainerAllocator::new(FakeStarter::default(), PUBLIC_URL);
    let job = job();
    let endpoint = block_on(allocator.allocate(&job)).expect("container allocation succeeds");

    let sid = job.sid.as_str();
    assert_eq!(
        endpoint,
        format!(
            "{}/handoff/{sid}/connect",
            PUBLIC_URL.replace("https://", "wss://")
        )
    );
    let starts = allocator.starter.starts.borrow();
    assert_eq!(starts.len(), 1);
    let request = &starts[0];
    assert_eq!(request.sid, job.sid);
    assert_eq!(request.secret, job.secret);
    assert_eq!(request.report_url, format!("{PUBLIC_URL}/handoff/{sid}"));
    assert_eq!(request.port, DEFAULT_CONTAINER_PORT);
    assert!(request.env.is_empty());
    assert_eq!(
        request.max_duration_ms,
        DEFAULT_MAX_DURATION.as_millis() as u64
    );
}

#[test]
fn http_public_url_yields_ws_endpoint() {
    let allocator = ContainerAllocator::new(FakeStarter::default(), "http://127.0.0.1:8787");
    let endpoint = block_on(allocator.allocate(&job())).expect("container allocation succeeds");
    assert_eq!(
        endpoint,
        format!("ws://127.0.0.1:8787/handoff/{}/connect", job().sid.as_str())
    );
}

#[test]
fn allocator_forwards_configured_port_env_and_duration() {
    let mut allocator = ContainerAllocator::new(FakeStarter::default(), PUBLIC_URL);
    allocator.port = 9100;
    allocator.env = vec![("ARENA_MAP".to_owned(), "dust".to_owned())];
    allocator.max_duration = Duration::from_secs(30);

    block_on(allocator.allocate(&job())).expect("container allocation succeeds");
    let starts = allocator.starter.starts.borrow();
    assert_eq!(starts[0].port, 9100);
    assert_eq!(starts[0].env, allocator.env);
    assert_eq!(starts[0].max_duration_ms, 30_000);
}

#[test]
fn stop_calls_the_starter() {
    let allocator = ContainerAllocator::new(FakeStarter::default(), PUBLIC_URL);
    let job = job();
    block_on(allocator.stop(&job.sid)).expect("stop succeeds");
    assert_eq!(allocator.starter.stops.borrow().as_slice(), &[job.sid]);
}

#[test]
fn starter_error_propagates() {
    let starter = FakeStarter {
        error: Some("container service unavailable".to_owned()),
        ..FakeStarter::default()
    };
    let allocator = ContainerAllocator::new(starter, PUBLIC_URL);
    let error = block_on(allocator.allocate(&job())).expect_err("a failed start fails allocation");
    assert_eq!(error, "container service unavailable");
    assert!(allocator.starter.starts.borrow().is_empty());
    assert_eq!(
        block_on(allocator.stop(&job().sid)),
        Err("container service unavailable".to_owned())
    );
}

#[test]
fn defaults() {
    let allocator = ContainerAllocator::new(FakeStarter::default(), PUBLIC_URL);
    assert_eq!(allocator.port, DEFAULT_CONTAINER_PORT);
    assert_eq!(allocator.max_duration, DEFAULT_MAX_DURATION);
    assert!(allocator.env.is_empty());
    assert_eq!(allocator.public_url, PUBLIC_URL);
}
