//! Plan Task 6 Step 1: timers, leave/grace/dispose, kick/lock, limits,
//! replacement, incompatible state and keepalive tests.

mod support;

use std::time::Duration;

use hearth_client::ClientEvent;
use hearth_protocol::{
    close, encode, Encoding, ErrorCode, Frame, IntentMsg, PatchOp, WireMsg, MAX_FRAME_BYTES,
    PING_TEXT, PONG_TEXT,
};
use hearth_server::SqlValue;
use hearth_test::TestRoom;
use serde_json::json;
use support::GraceZeroRoom;

type Room = TestRoom<GraceZeroRoom>;

fn new_room() -> Room {
    TestRoom::<GraceZeroRoom>::new()
}

#[test]
fn timer_fires_after_delay_and_survives_hibernation() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"Timer": ["t", 1000]}));
    r.hibernate();
    let start = r.client(a).events.len();

    r.advance(Duration::from_millis(999));
    assert_eq!(r.client(a).core.state()["count"], json!(0), "not due yet");
    assert!(support::patched_ops(&r, a, start).is_empty());

    r.advance(Duration::from_millis(1));
    assert_eq!(r.client(a).core.state()["count"], json!(1000));
    assert_eq!(support::patched_ops(&r, a, start).len(), 1);
    assert!(
        support::timer_keys(&r).is_empty(),
        "the fired timer row is deleted"
    );
}

#[test]
fn timer_reset_with_same_key_overwrites() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"Timer": ["t", 1000]})); // due at t=1000
    r.advance(Duration::from_millis(500));
    r.intent(a, json!({"Timer": ["t", 1000]})); // overwrites: due at t=1500
    r.advance(Duration::from_millis(999));
    assert_eq!(r.client(a).core.state()["count"], json!(0));
    r.advance(Duration::from_millis(1));
    assert_eq!(
        r.client(a).core.state()["count"],
        json!(1000),
        "the reset timer fires once, not twice"
    );
    r.advance(Duration::from_millis(5000));
    assert_eq!(r.client(a).core.state()["count"], json!(1000));
}

#[test]
fn failing_timer_handler_is_not_retried() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"Timer": ["boom", 1000]}));

    r.advance(Duration::from_millis(1000));

    assert_eq!(
        r.client(a).core.state()["count"],
        json!(0),
        "the failed handler's state change was rolled back"
    );
    assert!(
        support::timer_keys(&r).is_empty(),
        "the row is deleted before firing, so a failure cannot retry"
    );

    // The alarm machinery is still healthy after the failure.
    r.intent(a, json!({"Timer": ["ok", 1000]}));
    r.advance(Duration::from_millis(1000));
    assert_eq!(r.client(a).core.state()["count"], json!(1000));
}

#[test]
fn leave_frame_and_socket_close_both_call_on_leave() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let b_start = r.client(b).events.len();

    r.send_raw(a, encode(&Frame::Leave {}, Encoding::MsgPack));

    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(1000),
        "a graceful LEAVE closes normally"
    );
    assert_eq!(r.server_state()["leave_log"], json!(["a:Left"]));
    assert_eq!(r.server_state()["seats"], json!({"b": 1}));
    let ops = support::patched_ops(&r, b, b_start);
    assert_eq!(ops.len(), 1, "the remaining client sees the seat removal");
    assert_eq!(
        ops[0],
        vec![PatchOp::Del {
            path: vec![json!("seats"), json!("a")],
        }]
    );

    r.disconnect(b);
    assert_eq!(
        r.server_state()["leave_log"],
        json!(["a:Left", "b:Closed"]),
        "socket close calls on_leave with Closed"
    );
    assert_eq!(r.server_state()["leave_calls"], json!(2));
    assert_eq!(r.server_state()["seats"], json!({}));
}

#[test]
fn last_leave_starts_grace_then_disposes() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!("Sql"));
    assert!(
        support::table_names(&r).contains(&"dev".to_string()),
        "the dev table exists before dispose"
    );

    r.disconnect(a);
    r.advance(Duration::from_secs(60));

    assert!(
        support::table_names(&r).is_empty(),
        "dispose wipes every table, dev tables included"
    );

    // The room is usable again, with fresh state.
    let b = r.join("b", json!({}));
    assert_eq!(r.client(b).core.state()["count"], json!(0));
    assert_eq!(
        r.client(b).core.seq(),
        1,
        "a fresh room starts its sequence again"
    );
}

#[test]
fn join_during_grace_cancels_dispose() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.disconnect(a);

    let b = r.join("b", json!({}));
    r.advance(Duration::from_secs(120));

    assert_eq!(r.client(b).closed, None, "the room was not disposed");
    assert!(
        support::table_names(&r).contains(&"hearth_state".to_string()),
        "storage survives"
    );
    assert_eq!(r.server_state()["seats"], json!({"b": 0}));
}

#[test]
fn kick_closes_4004_and_calls_on_leave_kicked() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));

    r.intent(b, json!({"Kick": "a"}));

    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::KICKED)
    );
    assert_eq!(r.server_state()["leave_log"], json!(["a:Kicked"]));
    assert_eq!(r.server_state()["seats"], json!({"b": 1}));

    // The kicked player's seat is free again.
    let c = r.join("c", json!({}));
    assert_eq!(r.client(c).core.state()["seats"], json!({"b": 1, "c": 0}));
}

#[test]
fn lock_rejects_new_players_4003_and_persists_across_hibernation() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!("Lock"));
    assert_eq!(support::stored_locked(&r), 1);

    r.hibernate();
    let b = r.join("b", json!({}));

    let errs = support::errors(&r, b, 0);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, ErrorCode::RoomLocked);
    assert_eq!(
        r.client(b).closed.as_ref().map(|(code, _)| *code),
        Some(close::ROOM_LOCKED)
    );
    assert_eq!(
        support::stored_locked(&r),
        1,
        "the lock survives hibernation"
    );
}

#[test]
fn dispose_closes_all_4008_and_wipes() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));

    r.intent(a, json!("Dispose"));

    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::DISPOSED)
    );
    assert_eq!(
        r.client(b).closed.as_ref().map(|(code, _)| *code),
        Some(close::DISPOSED)
    );
    assert!(support::table_names(&r).is_empty(), "storage is wiped");
}

#[test]
fn oversize_frame_closes_4006_without_decoding() {
    let mut r = new_room();
    let a = r.connect_raw("a");

    r.send_raw(a, WireMsg::Binary(vec![0x05; MAX_FRAME_BYTES + 1]));

    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::FRAME_TOO_LARGE)
    );
    let errs = support::errors(&r, a, 0);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, ErrorCode::FrameTooLarge);
    assert_eq!(
        r.client(a).core.state(),
        &json!(null),
        "the oversize frame was never decoded"
    );
}

#[test]
fn rate_limit_closes_4005() {
    let mut r = new_room();
    let a = r.join("a", json!({}));

    let intents: Vec<IntentMsg> = (1..=121u32)
        .map(|seq| IntentMsg {
            seq,
            intent: json!({"Add": 1}),
        })
        .collect();
    r.send_raw(a, encode(&Frame::Intent { intents }, Encoding::MsgPack));

    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::RATE_LIMITED)
    );
    let errs = support::errors(&r, a, 0);
    assert!(errs
        .iter()
        .any(|(code, _, _)| *code == ErrorCode::RateLimited));
    assert_eq!(
        r.server_state()["leave_log"],
        json!(["a:Kicked"]),
        "a rate-limited client leaves with Kicked"
    );
    assert_eq!(
        r.server_state()["count"],
        json!(120),
        "120 intents fit the per-player bucket"
    );
}

#[test]
fn duplicate_player_replaces_old_socket() {
    let mut r = new_room();
    let a1 = r.join("a", json!({}));
    assert_eq!(r.client(a1).core.state()["seats"], json!({"a": 0}));
    assert_eq!(r.server_state()["join_calls"], json!(1));

    let a2 = r.join("a", json!({}));
    assert_eq!(
        r.client(a1).closed.as_ref().map(|(code, _)| *code),
        Some(close::REPLACED)
    );
    let joined_seat = r.client(a2).events.iter().find_map(|event| match event {
        ClientEvent::Joined { seat, .. } => Some(*seat),
        _ => None,
    });
    assert_eq!(joined_seat, Some(0), "the seat is kept");
    assert!(r
        .client(a2)
        .events
        .iter()
        .any(|event| matches!(event, ClientEvent::StateReset)));
    assert_eq!(r.server_state()["seats"], json!({"a": 0}));
    assert_eq!(
        r.server_state()["join_calls"],
        json!(1),
        "on_join is not called again for the same player"
    );
    assert_eq!(
        r.server_state()["leave_calls"],
        json!(0),
        "the replaced socket does not call on_leave"
    );

    // Replacement works while the room is locked too: only new players are
    // refused.
    r.intent(a2, json!("Lock"));
    let a3 = r.join("a", json!({}));
    assert_eq!(
        r.client(a2).closed.as_ref().map(|(code, _)| *code),
        Some(close::REPLACED)
    );
    assert!(r
        .client(a3)
        .events
        .iter()
        .any(|event| matches!(event, ClientEvent::Joined { seat: 0, .. })));
    assert_eq!(r.server_state()["seats"], json!({"a": 0}));
    assert_eq!(r.server_state()["join_calls"], json!(1));
}

#[test]
fn incompatible_persisted_state_refuses_without_wiping() {
    let mut r = new_room();
    let _a = r.join("a", json!({}));
    let garbage = vec![0xde_u8, 0xad, 0xbe, 0xef];
    r.sql()
        .exec(
            "UPDATE hearth_state SET data = ? WHERE id = 0",
            vec![SqlValue::Blob(garbage.clone())],
        )
        .expect("write an undecodable snapshot");
    r.hibernate();

    let b = r.join("b", json!({}));

    let errs = support::errors(&r, b, 0);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, ErrorCode::StateIncompatible);
    assert_eq!(
        r.client(b).closed.as_ref().map(|(code, _)| *code),
        Some(close::STATE_INCOMPATIBLE)
    );

    let rows = r
        .sql()
        .exec("SELECT data FROM hearth_state WHERE id = 0", vec![])
        .expect("the row still exists");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("data"),
        Some(&json!(garbage)),
        "the incompatible storage is left untouched"
    );
    assert!(support::table_names(&r).contains(&"hearth_state".to_string()));
}

#[test]
fn keepalive_text_is_not_a_frame() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let start = r.client(a).events.len();
    let seq_before = r.client(a).core.seq();

    r.send_raw(a, WireMsg::Text(PING_TEXT.into()));
    r.send_raw(a, WireMsg::Text(PONG_TEXT.into()));

    assert_eq!(
        r.client(a).events.len(),
        start,
        "keepalive text produces no frame and no wake effect"
    );
    assert_eq!(r.client(a).closed, None);
    assert_eq!(r.client(a).core.seq(), seq_before);
    assert_eq!(r.server_state()["count"], json!(0));
}
