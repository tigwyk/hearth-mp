//! Native tests for the host-independent container allocator (plan v0.5 Task 3
//! Step 1, the part that does not need the Durable Object adapter).

use std::cell::RefCell;
use std::time::Duration;

use futures::executor::block_on;
use hearth_mp_client::ClientEvent;
use hearth_mp_server::handoff::{AllocJob, HandoffRequest, ServerReport, DEFAULT_MAX_DURATION};
use hearth_mp_server::{
    Allocator, Client, ContainerAllocator, ContainerStarter, Ctx, HandoffEvent, JoinOpts, Leave,
    Mode, PlayerId, Result, Room, RoomConfig, RoomName, SessionId, StartRequest,
    DEFAULT_CONTAINER_PORT,
};
use hearth_mp_sync::{HearthState, Schema};
use hearth_mp_test::TestRoom;
use serde::{Deserialize, Serialize};
use serde_json::json;

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
        // Zero means "use the allocator's default"; the room always sets it.
        max_duration_ms: 0,
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
    async fn start(&self, request: StartRequest) -> std::result::Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.starts.borrow_mut().push(request);
        Ok(())
    }

    async fn stop(&self, sid: &SessionId) -> std::result::Result<(), String> {
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
fn job_max_duration_wins_over_the_allocator_default() {
    let allocator = ContainerAllocator::new(FakeStarter::default(), PUBLIC_URL);
    let mut job = job();
    // The room knows the session's own deadline; the container backstop must not
    // fire before it.
    job.max_duration_ms = 90_000;
    block_on(allocator.allocate(&job)).expect("container allocation succeeds");
    let starts = allocator.starter.starts.borrow();
    assert_eq!(starts[0].max_duration_ms, 90_000);
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

// ------------------------------------------------------------- session wiring

/// The smallest handoff room: one intent starts a session for every connected
/// player, and `on_handoff` records the lifecycle in the synced log.
#[derive(HearthState, Serialize, Deserialize, Default)]
pub struct LobbyState {
    #[sync]
    pub log: Vec<String>,
}

#[derive(Schema, Deserialize, Debug)]
pub enum LobbyIntent {
    Start,
}

#[derive(Default)]
pub struct Lobby;

impl RoomConfig for Lobby {
    const MODE: Mode = Mode::Event;
    const MAX_CLIENTS: u8 = 4;
    const ALLOCATOR: Option<&'static str> = Some("container");
    const CONTAINER: Option<&'static str> = Some("ARENA_SERVER");
}

impl Room for Lobby {
    type State = LobbyState;
    type Intent = LobbyIntent;

    async fn on_join(
        &mut self,
        _ctx: &mut Ctx<Self>,
        _who: &Client,
        _opts: JoinOpts,
    ) -> Result<()> {
        Ok(())
    }

    async fn on_intent(
        &mut self,
        ctx: &mut Ctx<Self>,
        _who: &Client,
        intent: LobbyIntent,
    ) -> Result<()> {
        match intent {
            LobbyIntent::Start => {
                let players: Vec<PlayerId> = ctx
                    .clients()
                    .iter()
                    .filter(|client| client.connected)
                    .map(|client| client.player_id.clone())
                    .collect();
                ctx.handoff(HandoffRequest::new(players, json!({ "map": "dust" })));
            }
        }
        Ok(())
    }

    async fn on_leave(
        &mut self,
        _ctx: &mut Ctx<Self>,
        _who: &Client,
        _reason: Leave,
    ) -> Result<()> {
        Ok(())
    }

    async fn on_handoff(
        &mut self,
        ctx: &mut Ctx<Self>,
        _sid: &SessionId,
        event: HandoffEvent,
    ) -> Result<()> {
        ctx.state().log.push(format!("{event:?}"));
        Ok(())
    }
}

/// Start a session and run its first allocation through the same
/// `ContainerAllocator` seam the Worker adapter wraps. Returns the room, both
/// players, the allocator, the job and the endpoint.
fn allocated_session() -> (
    TestRoom<Lobby>,
    hearth_mp_test::C,
    hearth_mp_test::C,
    ContainerAllocator<FakeStarter>,
    AllocJob,
    String,
) {
    let mut room = TestRoom::<Lobby>::new();
    let a = room.join("a", json!({}));
    let b = room.join("b", json!({}));
    room.intent(a, json!("Start"));
    let job = room.jobs().into_iter().next().expect("an allocation job");
    let allocator = ContainerAllocator::new(FakeStarter::default(), PUBLIC_URL);
    let endpoint = block_on(allocator.allocate(&job)).expect("container allocation");
    room.complete_job(&job.sid, Ok(endpoint.clone()));
    (room, a, b, allocator, job, endpoint)
}

fn handoff_data(room: &TestRoom<Lobby>, who: hearth_mp_test::C) -> Vec<serde_json::Value> {
    room.client(who)
        .events
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Event { name, data } if name == "hearth.handoff" => Some(data.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn failed_session_sends_stop() {
    // Review Focus 4: a dead server (no heartbeat) fails the session, and the
    // adapter then destroys the container.
    let (mut room, a, _b, allocator, job, _endpoint) = allocated_session();
    room.report(&job.sid, &job.secret, ServerReport::Started)
        .expect("started report");

    // The heartbeat timeout is 60 s (the fixture's default request).
    room.advance(Duration::from_secs(61));

    let stops = room.stop_jobs();
    assert_eq!(stops.len(), 1, "a failed session queues one stop");
    assert_eq!(stops[0].sid, job.sid);
    block_on(allocator.stop(&stops[0].sid)).expect("container stop");
    assert_eq!(allocator.starter.stops.borrow().as_slice(), &[job.sid]);
    assert!(room.client(a).events.iter().any(
        |event| matches!(event, ClientEvent::Event { name, .. } if name == "hearth.handoff_ended")
    ));
}

#[test]
fn ended_session_sends_stop() {
    let (mut room, _a, _b, allocator, job, _endpoint) = allocated_session();
    room.report(&job.sid, &job.secret, ServerReport::Started)
        .expect("started report");
    room.report(
        &job.sid,
        &job.secret,
        ServerReport::Ended {
            reason: "match over".to_owned(),
        },
    )
    .expect("ended report");

    let stops = room.stop_jobs();
    assert_eq!(stops.len(), 1, "an ended session queues one stop");
    assert_eq!(stops[0].sid, job.sid);
    block_on(allocator.stop(&stops[0].sid)).expect("container stop");
    assert_eq!(allocator.starter.stops.borrow().as_slice(), &[job.sid]);
}

#[test]
fn start_retry_is_idempotent_same_sid() {
    // Review Focus 3: a failed or interrupted start is retried with the same
    // sid, which the container allocator uses as the idempotency key.
    let mut room = TestRoom::<Lobby>::new();
    let a = room.join("a", json!({}));
    let _b = room.join("b", json!({}));
    room.intent(a, json!("Start"));
    let job = room.jobs().into_iter().next().expect("an allocation job");
    room.complete_job(&job.sid, Err("container service unavailable".to_owned()));
    assert!(room.jobs().is_empty(), "the failed attempt backs off");

    // JOB_IN_FLIGHT is 30 s; past it the adapter re-issues the job.
    room.advance(Duration::from_secs(31));

    let retry = room
        .jobs()
        .into_iter()
        .next()
        .expect("the allocation is retried");
    assert_eq!(retry.sid, job.sid, "the retry keeps the idempotency key");
    assert_eq!(retry.attempt, 2);
}

#[test]
fn session_env_only_in_start_rpc() {
    // The plan's fake-container-service case: the session secret is delivered
    // only in the Start RPC, never to clients, and the gateway's trusted redeem
    // needs only the single-use ticket.
    let (mut room, a, _b, allocator, job, _endpoint) = allocated_session();
    let starts = allocator.starter.starts.borrow();
    let request = starts.first().expect("one start request");
    assert_eq!(request.sid, job.sid);
    assert_eq!(
        request.secret, job.secret,
        "the Start RPC carries the secret"
    );
    assert_eq!(
        request.report_url,
        format!("{PUBLIC_URL}/handoff/{}", job.sid.as_str())
    );
    drop(starts);

    let events = handoff_data(&room, a);
    assert_eq!(events.len(), 1);
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains(&job.secret),
        "the session secret never reaches a client"
    );
    let ticket = events[0]["ticket"].as_str().expect("a ticket").to_owned();
    let redeemed = room
        .redeem_trusted(&job.sid, &ticket)
        .expect("the gateway's ticket-only redeem");
    assert_eq!(redeemed.player_id, PlayerId("a".to_owned()));
    assert_eq!(redeemed.sid, job.sid);
}

#[test]
fn started_report_before_allocator_result_still_sends_tickets() {
    // A container reports `Started` as soon as its process boots; that report
    // can beat the allocator's RPC reply. The session must still get the
    // endpoint and the players their tickets instead of stranding `started`
    // with no ticket (observed against the deployed Worker as `lost`).
    let mut room = TestRoom::<Lobby>::new();
    let a = room.join("a", json!({}));
    let _b = room.join("b", json!({}));
    room.intent(a, json!("Start"));
    let job = room.jobs().into_iter().next().expect("an allocation job");

    room.report(&job.sid, &job.secret, ServerReport::Started)
        .expect("the container's early Started report");
    room.complete_job(&job.sid, Ok(format!("{PUBLIC_URL}/ws")));

    let events = handoff_data(&room, a);
    assert_eq!(
        events.len(),
        1,
        "the late allocator success still sends a ticket"
    );
    let ticket = events[0]["ticket"].as_str().expect("a ticket").to_owned();
    let redeemed = room
        .redeem_trusted(&job.sid, &ticket)
        .expect("the session is still active");
    assert_eq!(redeemed.player_id, PlayerId("a".to_owned()));
    assert!(room.jobs().is_empty(), "no retry was scheduled");
}
