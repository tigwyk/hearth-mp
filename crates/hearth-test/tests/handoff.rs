//! Plan v0.5 Task 1: handoff sessions, tickets and lifecycle in the runtime.
//!
//! The `Lobby` fixture is the plan's: event mode, `max_clients = 4`, the `Start`
//! intent hands every seated player to a dedicated server, and `on_handoff`
//! records the lifecycle in the synced `log`.

mod support;

use std::time::Duration;

use hearth_client::ClientEvent;
use hearth_protocol::{encode, Encoding, Frame, PlayerId, Value};
use hearth_server::handoff::{HandoffError, HandoffRequest, ServerReport, SessionId, TICKET_TTL};
use hearth_server::{Client, Ctx, HandoffEvent, JoinOpts, Leave, Mode, Result, Room, RoomConfig};
use hearth_sync::{HearthState, Schema};
use hearth_test::TestRoom;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(HearthState, Serialize, Deserialize, Default)]
pub struct LobbyState {
    #[sync]
    pub log: Vec<String>,
    #[sync]
    pub result: String,
}

#[derive(Schema, Deserialize, Debug)]
pub enum LobbyIntent {
    Start,
    /// `Start` with explicit timeouts, in milliseconds.
    StartWith {
        start_timeout_ms: u64,
        heartbeat_timeout_ms: u64,
        max_duration_ms: u64,
    },
    /// Requests a handoff and then fails: the effect must not commit.
    StartThenFail,
    /// Tries to send a framework-reserved event name.
    SendReserved,
    SendNormal,
}

#[derive(Default)]
pub struct Lobby;

impl RoomConfig for Lobby {
    const MODE: Mode = Mode::Event;
    const MAX_CLIENTS: u8 = 4;
}

impl Lobby {
    fn request(ctx: &mut Ctx<Self>, start_ms: u64, heartbeat_ms: u64, max_ms: u64) -> SessionId {
        let players: Vec<PlayerId> = ctx
            .clients()
            .iter()
            .filter(|client| client.connected)
            .map(|client| client.player_id.clone())
            .collect();
        let request = HandoffRequest::new(players, json!({"map": "dust"}))
            .start_timeout(Duration::from_millis(start_ms))
            .heartbeat_timeout(Duration::from_millis(heartbeat_ms))
            .max_duration(Duration::from_millis(max_ms));
        ctx.handoff(request)
    }
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
        who: &Client,
        intent: LobbyIntent,
    ) -> Result<()> {
        match intent {
            LobbyIntent::Start => {
                Self::request(ctx, 60_000, 60_000, 7_200_000);
            }
            LobbyIntent::StartWith {
                start_timeout_ms,
                heartbeat_timeout_ms,
                max_duration_ms,
            } => {
                Self::request(ctx, start_timeout_ms, heartbeat_timeout_ms, max_duration_ms);
            }
            LobbyIntent::StartThenFail => {
                Self::request(ctx, 60_000, 60_000, 7_200_000);
                return Err(anyhow::anyhow!(
                    "the handler failed after asking for a handoff"
                ));
            }
            LobbyIntent::SendReserved => {
                ctx.send(&who.player_id, "hearth.handoff", json!({"spoof": true}));
                ctx.broadcast("hearth.handoff", json!({"spoof": true}));
            }
            LobbyIntent::SendNormal => {
                ctx.send(&who.player_id, "lobby.notice", json!({"ok": true}));
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
        let name = match &event {
            HandoffEvent::Ready { .. } => "ready",
            HandoffEvent::Started => "started",
            HandoffEvent::PlayerJoined(_) => "player_joined",
            HandoffEvent::PlayerLeft(_) => "player_left",
            HandoffEvent::Result(_) => "result",
            HandoffEvent::Ended { .. } => "ended",
            HandoffEvent::Failed { .. } => "failed",
        };
        ctx.state().log.push(name.to_owned());
        if let HandoffEvent::Result(data) = event {
            ctx.state().result = serde_json::to_string(&data).expect("a handoff result is JSON");
        }
        Ok(())
    }
}

fn new_room() -> TestRoom<Lobby> {
    TestRoom::new()
}

fn start(r: &mut TestRoom<Lobby>, c: hearth_test::C) {
    r.intent(c, json!("Start"));
}

fn start_with(
    r: &mut TestRoom<Lobby>,
    c: hearth_test::C,
    start_ms: u64,
    heartbeat_ms: u64,
    max_ms: u64,
) {
    r.intent(
        c,
        json!({"StartWith": {
            "start_timeout_ms": start_ms,
            "heartbeat_timeout_ms": heartbeat_ms,
            "max_duration_ms": max_ms,
        }}),
    );
}

/// Start a handoff and complete its first allocation job; returns the job.
fn allocate(r: &mut TestRoom<Lobby>, c: hearth_test::C) -> hearth_server::AllocJob {
    start(r, c);
    let jobs = r.jobs();
    assert_eq!(jobs.len(), 1, "one allocation job");
    let job = jobs.into_iter().next().expect("a job");
    r.complete_job(&job.sid, Ok("wss://dedicated.example/ws".to_owned()));
    job
}

fn ticket_events(r: &TestRoom<Lobby>, c: hearth_test::C) -> Vec<Value> {
    r.client(c)
        .events
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Event { name, data } if name == "hearth.handoff" => Some(data.clone()),
            _ => None,
        })
        .collect()
}

fn ended_events(r: &TestRoom<Lobby>, c: hearth_test::C) -> Vec<Value> {
    r.client(c)
        .events
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Event { name, data } if name == "hearth.handoff_ended" => {
                Some(data.clone())
            }
            _ => None,
        })
        .collect()
}

fn tickets(r: &TestRoom<Lobby>, c: hearth_test::C) -> Vec<String> {
    ticket_events(r, c)
        .iter()
        .map(|data| data["ticket"].as_str().expect("a ticket").to_owned())
        .collect()
}

fn table_rows_json(r: &TestRoom<Lobby>, table: &str) -> String {
    let rows = r
        .sql()
        .exec(&format!("SELECT * FROM {table}"), Vec::new())
        .expect("handoff tables are readable");
    format!("{rows:?}")
}

#[test]
fn handoff_creates_job_after_commit_only() {
    let mut r = new_room();
    let a = r.join("a", json!({}));

    r.intent(a, json!("StartThenFail"));

    assert!(r.jobs().is_empty(), "a failed handler created no session");
    assert_eq!(
        table_rows_json(&r, "hearth_sessions"),
        "[]",
        "no session row"
    );
}

#[test]
fn job_success_sends_private_ticket_events() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let job = allocate(&mut r, a);

    let a_tickets = tickets(&r, a);
    let b_tickets = tickets(&r, b);
    assert_eq!(a_tickets.len(), 1);
    assert_eq!(b_tickets.len(), 1);
    assert_ne!(a_tickets[0], b_tickets[0], "one ticket per player");
    assert_eq!(ticket_events(&r, a)[0]["sid"], json!(job.sid.0));
    assert_eq!(
        ticket_events(&r, a)[0]["endpoint"],
        json!("wss://dedicated.example/ws")
    );
    assert_ne!(
        ticket_events(&r, a)[0]["ticket"],
        ticket_events(&r, b)[0]["ticket"]
    );
}

#[test]
fn ticket_single_use() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let job = allocate(&mut r, a);
    let ticket = tickets(&r, a).remove(0);

    let redeemed = r
        .redeem(&job.sid, &job.secret, &ticket)
        .expect("first redeem");
    assert_eq!(redeemed.player_id, PlayerId("a".to_owned()));
    assert_eq!(redeemed.claims, json!({"map": "dust"}));
    assert_eq!(redeemed.sid, job.sid);
    assert_eq!(
        r.redeem(&job.sid, &job.secret, &ticket),
        Err(HandoffError::Forbidden),
        "the second redeem is refused"
    );
}

#[test]
fn ticket_expires_after_60s() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    start_with(&mut r, a, 300_000, 60_000, 7_200_000);
    let job = r.jobs().into_iter().next().expect("a job");
    r.complete_job(&job.sid, Ok("wss://x".to_owned()));
    let ticket = tickets(&r, a).remove(0);

    r.advance(TICKET_TTL + Duration::from_secs(1));

    assert_eq!(
        r.redeem(&job.sid, &job.secret, &ticket),
        Err(HandoffError::Forbidden),
        "the ticket expired"
    );
}

#[test]
fn ticket_wrong_secret_forbidden() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let job = allocate(&mut r, a);
    let ticket = tickets(&r, a).remove(0);

    assert_eq!(
        r.redeem(&job.sid, "00", &ticket),
        Err(HandoffError::Forbidden)
    );
}

#[test]
fn ticket_from_other_session_forbidden() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let first = allocate(&mut r, a);
    let first_ticket = tickets(&r, a).remove(0);
    let second = allocate(&mut r, a);
    assert_ne!(first.sid, second.sid, "a second session");
    let second_ticket = tickets(&r, a).last().expect("second ticket").clone();
    assert_ne!(first_ticket, second_ticket);

    assert_eq!(
        r.redeem(&first.sid, &first.secret, &second_ticket),
        Err(HandoffError::Forbidden),
        "a ticket for another session is refused"
    );
}

#[test]
fn redeem_after_session_ended_is_gone() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let job = allocate(&mut r, a);
    let ticket = tickets(&r, a).remove(0);
    r.report(
        &job.sid,
        &job.secret,
        ServerReport::Ended {
            reason: "done".into(),
        },
    )
    .expect("the end report is accepted");

    assert_eq!(
        r.redeem(&job.sid, &job.secret, &ticket),
        Err(HandoffError::Gone)
    );
}

#[test]
fn report_requires_session_secret() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let job = allocate(&mut r, a);
    let log_before = r.server_state()["log"].clone();

    assert_eq!(
        r.report(&job.sid, "wrong-secret", ServerReport::Started),
        Err(HandoffError::Forbidden)
    );
    assert_eq!(
        r.server_state()["log"],
        log_before,
        "a refused report changes nothing"
    );
}

#[test]
fn reports_after_ended_ignored() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let job = allocate(&mut r, a);
    r.report(
        &job.sid,
        &job.secret,
        ServerReport::Ended {
            reason: "done".into(),
        },
    )
    .expect("ended");
    let log_after_end = r.server_state()["log"].clone();

    assert_eq!(
        r.report(
            &job.sid,
            &job.secret,
            ServerReport::Result {
                data: json!({"late": true})
            }
        ),
        Ok(()),
        "reports after the end are accepted and ignored"
    );
    assert_eq!(r.server_state()["log"], log_after_end);
}

#[test]
fn lifecycle_events_reach_on_handoff_in_order() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let job = allocate(&mut r, a);

    r.report(&job.sid, &job.secret, ServerReport::Started)
        .expect("started");
    r.report(
        &job.sid,
        &job.secret,
        ServerReport::PlayerJoined {
            player: PlayerId("a".to_owned()),
        },
    )
    .expect("joined");
    r.report(
        &job.sid,
        &job.secret,
        ServerReport::Result {
            data: json!({"winner": "a"}),
        },
    )
    .expect("result");
    r.report(
        &job.sid,
        &job.secret,
        ServerReport::Ended {
            reason: "done".into(),
        },
    )
    .expect("ended");
    let _ = b;

    assert_eq!(
        r.server_state()["log"],
        json!(["ready", "started", "player_joined", "result", "ended"])
    );
    assert_eq!(r.server_state()["result"], json!(r#"{"winner":"a"}"#));
}

#[test]
fn allocation_retries_then_fails() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    start(&mut r, a);

    for attempt in 1..=3u32 {
        let jobs = r.jobs();
        assert_eq!(jobs.len(), 1, "attempt {attempt} is due");
        assert_eq!(jobs[0].attempt, attempt);
        if attempt < 3 {
            r.complete_job(&jobs[0].sid, Err("allocator exploded".to_owned()));
            r.advance(Duration::from_secs(1 << attempt));
        } else {
            r.complete_job(&jobs[0].sid, Err("allocator exploded".to_owned()));
        }
    }

    assert_eq!(r.server_state()["log"], json!(["failed"]));
    assert!(tickets(&r, a).is_empty(), "no ticket was minted");
    assert!(r.jobs().is_empty(), "a failed session is not retried");
}

#[test]
fn job_in_flight_reissued_after_30s_same_sid() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    start(&mut r, a);

    let first = r.jobs().into_iter().next().expect("first job");
    r.advance(Duration::from_secs(31));
    let again = r.jobs().into_iter().next().expect("reissued job");
    assert_eq!(again.sid, first.sid);
    assert_eq!(again.attempt, 2);
}

#[test]
fn start_timeout_fails_session() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let job = allocate(&mut r, a);

    r.advance(Duration::from_secs(61));

    assert_eq!(r.server_state()["log"], json!(["ready", "failed"]));
    assert!(
        !ended_events(&r, a).is_empty(),
        "a got hearth.handoff_ended"
    );
    assert!(
        !ended_events(&r, b).is_empty(),
        "b got hearth.handoff_ended"
    );
    assert_eq!(
        ended_events(&r, a)[0]["reason"],
        json!("start timeout"),
        "sid {:?}",
        job.sid
    );
}

#[test]
fn heartbeat_timeout_fails_lost() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let job = allocate(&mut r, a);
    r.report(&job.sid, &job.secret, ServerReport::Started)
        .expect("started");

    r.advance(Duration::from_secs(61));

    assert_eq!(
        r.server_state()["log"],
        json!(["ready", "started", "failed"])
    );
    assert_eq!(ended_events(&r, a)[0]["reason"], json!("lost"));
}

#[test]
fn max_duration_fails() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    start_with(&mut r, a, 300_000, 300_000, 90_000);
    let job = r.jobs().into_iter().next().expect("a job");
    r.complete_job(&job.sid, Ok("wss://x".to_owned()));
    r.report(&job.sid, &job.secret, ServerReport::Started)
        .expect("started");

    r.advance(Duration::from_secs(91));

    assert_eq!(
        r.server_state()["log"],
        json!(["ready", "started", "failed"])
    );
    assert_eq!(ended_events(&r, a)[0]["reason"], json!("max duration"));
}

#[test]
fn active_session_keeps_room_alive() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    start_with(&mut r, a, 300_000, 300_000, 7_200_000);
    let job = r.jobs().into_iter().next().expect("a job");
    r.complete_job(&job.sid, Ok("wss://x".to_owned()));

    r.send_raw(a, encode(&Frame::Leave {}, Encoding::MsgPack));
    r.send_raw(b, encode(&Frame::Leave {}, Encoding::MsgPack));
    r.advance(Duration::from_secs(70));
    assert!(
        !support::table_names(&r).is_empty(),
        "the room is not disposed while a session is active"
    );

    r.report(
        &job.sid,
        &job.secret,
        ServerReport::Ended {
            reason: "done".into(),
        },
    )
    .expect("ended");
    r.advance(Duration::from_secs(61));
    assert!(
        support::table_names(&r).is_empty(),
        "after the session ends the empty room disposes"
    );
}

#[test]
fn reconnect_during_session_reissues_ticket() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let job = allocate(&mut r, a);
    let first = tickets(&r, a).remove(0);

    r.drop_socket(a);
    let resumed = r.resume(a);
    let reissued = tickets(&r, resumed);
    assert_eq!(reissued.len(), 1, "one fresh ticket after the reconnect");
    assert_ne!(reissued[0], first, "the player got a new ticket");
    assert!(
        r.redeem(&job.sid, &job.secret, &reissued[0]).is_ok(),
        "the fresh ticket redeems"
    );
}

#[test]
fn secrets_and_tickets_stored_hashed_only() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let job = allocate(&mut r, a);
    let ticket = tickets(&r, a).remove(0);

    for table in ["hearth_sessions", "hearth_tickets"] {
        let rows = table_rows_json(&r, table);
        assert!(
            !rows.contains(&job.secret),
            "{table} must not store the plaintext secret"
        );
        assert!(
            !rows.contains(&ticket),
            "{table} must not store the plaintext ticket"
        );
    }
}

#[test]
fn reserved_event_name_dropped() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let start = r.client(a).events.len();

    r.intent(a, json!("SendReserved"));

    let reserved = r.client(a).events[start..]
        .iter()
        .filter(
            |event| matches!(event, ClientEvent::Event { name, .. } if name == "hearth.handoff"),
        )
        .count();
    assert_eq!(reserved, 0, "the reserved event was dropped");
    assert!(
        r.logs()
            .iter()
            .any(|line| line.contains("reserved_event_name")),
        "the drop is logged: {:?}",
        r.logs()
    );

    r.intent(a, json!("SendNormal"));
    assert!(
        r.client(a).events.iter().any(
            |event| matches!(event, ClientEvent::Event { name, .. } if name == "lobby.notice")
        ),
        "normal events still arrive"
    );
}

#[test]
fn survives_hibernation_at_every_step() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    start(&mut r, a);
    r.hibernate();

    let job = r.jobs().into_iter().next().expect("a job");
    r.hibernate();
    r.complete_job(&job.sid, Ok("wss://x".to_owned()));
    r.hibernate();

    r.report(&job.sid, &job.secret, ServerReport::Started)
        .expect("started after hibernation");
    r.hibernate();
    r.report(
        &job.sid,
        &job.secret,
        ServerReport::PlayerJoined {
            player: PlayerId("a".to_owned()),
        },
    )
    .expect("joined");
    r.hibernate();
    r.report(
        &job.sid,
        &job.secret,
        ServerReport::Result {
            data: json!({"winner": "b"}),
        },
    )
    .expect("result");
    r.hibernate();
    r.report(
        &job.sid,
        &job.secret,
        ServerReport::Ended {
            reason: "done".into(),
        },
    )
    .expect("ended");
    let _ = b;

    assert_eq!(
        r.server_state()["log"],
        json!(["ready", "started", "player_joined", "result", "ended"])
    );
    assert_eq!(r.server_state()["result"], json!(r#"{"winner":"b"}"#));
}
