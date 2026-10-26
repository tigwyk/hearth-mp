//! Plan v0.2a Task 1: seat hold and resume across a reconnect grace window.

mod support;

use std::time::Duration;

use hearth_mp_client::ClientEvent;
use hearth_mp_protocol::{close, encode, Encoding, ErrorCode, Frame, PatchOp};
use hearth_mp_test::TestRoom;
use serde_json::json;
use support::{CounterRoom, GraceZeroRoom};

type Room = TestRoom<CounterRoom>;

#[test]
fn drop_holds_seat_and_notifies_room() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let b_start = r.client(b).events.len();

    r.drop_socket(a);

    assert_eq!(
        r.server_state()["hidden"],
        json!(1),
        "on_disconnect runs exactly once"
    );
    assert_eq!(
        r.server_state()["seats"],
        json!({"a": 0, "b": 1}),
        "the held seat stays in the synced roster"
    );
    assert_eq!(support::held_players(&r), vec!["a".to_string()]);
    assert!(
        support::patched_ops(&r, b, b_start).is_empty(),
        "b sees no seat removal while the seat is held"
    );

    // `ctx.clients()` reports the held seat as disconnected.
    r.intent(b, json!("Clients"));
    assert_eq!(
        r.server_state()["connected"],
        json!({"a": false, "b": true})
    );
}

#[test]
fn resume_within_grace_keeps_seat_without_join_or_leave() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    let _b = r.join("b", json!({}));
    r.drop_socket(a);
    r.advance(Duration::from_secs(29));

    let a2 = r.resume(a);

    let events = &r.client(a2).events;
    assert!(
        matches!(events[0], ClientEvent::Joined { seat: 0, .. }),
        "JOIN_OK carries the same seat: {events:?}"
    );
    assert!(
        matches!(events[1], ClientEvent::StateReset),
        "a resumed client gets a fresh FULL_STATE: {events:?}"
    );
    assert_eq!(r.server_state()["hidden"], json!(11), "on_reconnect runs");
    assert_eq!(
        r.server_state()["join_calls"],
        json!(2),
        "on_join does not run again"
    );
    assert_eq!(
        r.server_state()["leave_calls"],
        json!(0),
        "on_leave does not run for a resume"
    );
    assert_eq!(r.server_state()["seats"], json!({"a": 0, "b": 1}));
    assert_eq!(r.client(a2).core.state()["seats"], json!({"a": 0, "b": 1}));
}

#[test]
fn resume_after_hibernation_works() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    let _b = r.join("b", json!({}));
    r.drop_socket(a);
    r.hibernate();
    r.advance(Duration::from_secs(10));
    r.hibernate();

    let a2 = r.resume(a);

    let events = &r.client(a2).events;
    assert!(
        matches!(events[0], ClientEvent::Joined { seat: 0, .. }),
        "JOIN_OK carries the same seat: {events:?}"
    );
    assert!(
        matches!(events[1], ClientEvent::StateReset),
        "a resumed client gets a fresh FULL_STATE: {events:?}"
    );
    assert_eq!(r.server_state()["hidden"], json!(11));
    assert_eq!(r.server_state()["join_calls"], json!(2));
    assert_eq!(r.server_state()["leave_calls"], json!(0));
    assert_eq!(r.server_state()["seats"], json!({"a": 0, "b": 1}));
}

#[test]
fn grace_expiry_calls_on_leave_closed() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    r.drop_socket(a);
    let b_start = r.client(b).events.len();

    r.advance(Duration::from_secs(30));

    assert_eq!(
        r.server_state()["seats"],
        json!({"b": 1}),
        "the expired seat is gone"
    );
    assert_eq!(r.server_state()["leave_log"], json!(["a:Closed"]));
    assert_eq!(r.server_state()["leave_calls"], json!(1));
    assert_eq!(r.server_state()["hidden"], json!(1), "no on_reconnect");
    assert!(
        support::held_players(&r).is_empty(),
        "the hearth_held row is deleted on expiry"
    );
    assert!(
        support::timer_keys(&r).is_empty(),
        "the grace timer row is deleted when it fires"
    );
    assert_eq!(
        support::patched_ops(&r, b, b_start),
        vec![vec![PatchOp::Del {
            path: vec![json!("seats"), json!("a")],
        }]]
    );
}

#[test]
fn rejoin_after_expiry_is_fresh_join() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    let _b = r.join("b", json!({}));
    r.drop_socket(a);
    r.advance(Duration::from_secs(30));

    let a2 = r.join("a", json!({}));

    assert!(
        r.client(a2)
            .events
            .iter()
            .any(|event| matches!(event, ClientEvent::Joined { seat: 0, .. })),
        "the lowest free seat is allocated again"
    );
    assert_eq!(
        r.server_state()["join_calls"],
        json!(3),
        "on_join runs again for a fresh join"
    );
    assert_eq!(r.server_state()["leave_calls"], json!(1));
    assert_eq!(r.server_state()["hidden"], json!(1), "no on_reconnect");
    assert_eq!(r.server_state()["seats"], json!({"a": 0, "b": 1}));
}

#[test]
fn held_seat_counts_toward_room_full() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    let _b = r.join("b", json!({}));
    r.drop_socket(a);

    let c = r.join("c", json!({}));

    let errs = support::errors(&r, c, 0);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, ErrorCode::RoomFull);
    assert_eq!(
        r.client(c).closed.as_ref().map(|(code, _)| *code),
        Some(close::ROOM_FULL)
    );
    assert_eq!(
        r.server_state()["join_calls"],
        json!(2),
        "the full check runs before on_join"
    );
}

#[test]
fn locked_room_still_accepts_resume() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    let _b = r.join("b", json!({}));
    r.intent(a, json!("Lock"));
    r.drop_socket(a);
    r.advance(Duration::from_secs(5));

    let a2 = r.resume(a);

    assert!(
        r.client(a2)
            .events
            .iter()
            .any(|event| matches!(event, ClientEvent::Joined { seat: 0, .. })),
        "a held seat resumes even while the room is locked"
    );
    assert_eq!(r.server_state()["hidden"], json!(11));
    assert_eq!(support::stored_locked(&r), 1);
}

#[test]
fn leave_frame_skips_grace() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let b_start = r.client(b).events.len();

    r.send_raw(a, encode(&Frame::Leave {}, Encoding::MsgPack));

    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(1000),
        "a graceful LEAVE still closes normally"
    );
    assert_eq!(r.server_state()["leave_log"], json!(["a:Left"]));
    assert_eq!(r.server_state()["leave_calls"], json!(1));
    assert_eq!(
        r.server_state()["hidden"],
        json!(0),
        "LEAVE never runs the disconnect hook"
    );
    assert!(support::held_players(&r).is_empty(), "no row is held");
    assert!(support::timer_keys(&r).is_empty(), "no grace timer is set");
    assert_eq!(support::patched_ops(&r, b, b_start).len(), 1);
}

#[test]
fn kick_held_player() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    r.drop_socket(a);
    let b_start = r.client(b).events.len();

    r.intent(b, json!({"Kick": "a"}));

    assert_eq!(r.server_state()["leave_log"], json!(["a:Kicked"]));
    assert_eq!(r.server_state()["leave_calls"], json!(1));
    assert_eq!(r.server_state()["seats"], json!({"b": 1}));
    assert!(
        support::held_players(&r).is_empty(),
        "the kicked player's held row is deleted"
    );
    assert!(
        support::timer_keys(&r).is_empty(),
        "the kicked player's grace timer is cancelled"
    );
    assert_eq!(
        support::patched_ops(&r, b, b_start),
        vec![vec![PatchOp::Del {
            path: vec![json!("seats"), json!("a")],
        }]]
    );
}

#[test]
fn empty_timer_waits_for_held_seats() {
    let mut r = Room::new();
    let a = r.join("a", json!({}));
    r.drop_socket(a);

    r.advance(Duration::from_secs(60));

    assert!(
        support::table_names(&r).contains(&"hearth_state".to_string()),
        "no dispose at 60 s: the empty timer starts only after the seat is gone"
    );

    r.advance(Duration::from_secs(30));

    assert!(
        support::table_names(&r).is_empty(),
        "dispose fires at grace + EMPTY_GRACE"
    );
}

#[test]
fn grace_zero_is_v01_behaviour() {
    let mut r = TestRoom::<GraceZeroRoom>::new();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let b_start = r.client(b).events.len();

    r.drop_socket(a);

    assert_eq!(r.server_state()["leave_log"], json!(["a:Closed"]));
    assert_eq!(r.server_state()["leave_calls"], json!(1));
    assert_eq!(
        r.server_state()["hidden"],
        json!(0),
        "grace 0 never runs on_disconnect"
    );
    assert!(support::held_players(&r).is_empty());
    assert!(support::timer_keys(&r).is_empty());
    assert_eq!(
        support::patched_ops(&r, b, b_start),
        vec![vec![PatchOp::Del {
            path: vec![json!("seats"), json!("a")],
        }]]
    );
}
