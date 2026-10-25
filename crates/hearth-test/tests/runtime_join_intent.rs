//! Plan Task 5 Step 1: join, intent, diff-sync, errors and handshake tests.

mod support;

use hearth_client::ClientEvent;
use hearth_protocol::{
    close, encode, Encoding, ErrorCode, Frame, IntentMsg, PatchOp, PROTOCOL_VERSION,
};
use hearth_server::SqlValue;
use hearth_test::TestRoom;
use serde_json::json;
use support::{schema_hash, CounterRoom};

type Room = TestRoom<CounterRoom>;

fn new_room() -> Room {
    TestRoom::<CounterRoom>::new()
}

#[test]
fn join_sends_join_ok_then_full_state_with_own_view() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let events = &r.client(a).events;
    assert!(matches!(&events[0], ClientEvent::Joined { seat: 0, .. }));
    assert!(matches!(&events[1], ClientEvent::StateReset));
    assert_eq!(r.client(a).core.state()["seats"], json!({"a": 0}));
    assert!(
        r.client(a).core.state().get("hidden").is_none(),
        "server-only fields never reach clients"
    );
}

#[test]
fn second_join_patches_first_client() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let start = r.client(a).events.len();
    let b = r.join("b", json!({}));

    let ops = support::patched_ops(&r, a, start);
    assert_eq!(
        ops,
        vec![vec![PatchOp::MapInsert {
            path: vec![json!("seats")],
            key: "b".into(),
            value: json!(1),
        }]]
    );
    assert_eq!(r.client(a).core.state()["seats"], json!({"a": 0, "b": 1}));
    assert_eq!(r.client(a).core.state(), r.client(b).core.state());
}

#[test]
fn intent_patches_everyone_and_acks_sender() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let a_start = r.client(a).events.len();
    let b_start = r.client(b).events.len();

    assert_eq!(r.intent(a, json!({"Add": 5})), 1);

    assert_eq!(r.client(a).core.state()["count"], json!(5));
    assert_eq!(r.client(b).core.state()["count"], json!(5));
    assert!(!support::patched_ops(&r, b, b_start).is_empty());
    let state_seq = r.client(a).core.seq();
    assert_eq!(support::acks(&r, a, a_start), vec![(state_seq, vec![1])]);
    assert!(
        support::acks(&r, b, b_start).is_empty(),
        "only the sender is acked"
    );
}

#[test]
fn owner_field_never_reaches_other_clients() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let b_start = r.client(b).events.len();

    r.intent(a, json!({"Secret": "s"}));

    assert_eq!(r.client(a).core.state()["secrets"], json!({"a": "s"}));
    assert_eq!(r.client(b).core.state()["secrets"], json!({}));
    assert_eq!(
        r.client(b).events.len(),
        b_start,
        "b receives no frame at all: no patch, no event, no ack"
    );
    assert_eq!(r.server_state()["secrets"], json!({"a": "s"}));
}

#[test]
fn failed_intent_rolls_back_everything() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let a_start = r.client(a).events.len();
    let b_start = r.client(b).events.len();
    let seq_before = support::stored_seq(&r);
    let client_seq_before = r.client(a).core.seq();

    assert_eq!(r.intent(a, json!("Fail")), 1);

    let new_events = support::events_since(&r, a, a_start);
    assert_eq!(new_events.len(), 1, "only the error: {new_events:?}");
    match &new_events[0] {
        ClientEvent::Error {
            code,
            message,
            ref_seq,
        } => {
            assert_eq!(*code, ErrorCode::IntentRejected);
            assert_eq!(*ref_seq, Some(1));
            assert!(message.contains("nope"), "message was {message:?}");
        }
        other => panic!("expected an ERROR event, got {other:?}"),
    }

    assert!(support::patched_ops(&r, a, a_start).is_empty());
    assert!(
        support::events_since(&r, b, b_start).is_empty(),
        "no patch or event reaches the other client"
    );
    assert_eq!(r.server_state()["count"], json!(0), "state rolled back");
    assert!(
        support::timer_keys(&r).is_empty(),
        "buffered timer was dropped"
    );
    assert_eq!(support::stored_seq(&r), seq_before, "seq did not advance");
    assert_eq!(r.client(a).core.seq(), client_seq_before);
}

#[test]
fn undecodable_intent_is_bad_intent_and_socket_stays_open() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let start = r.client(a).events.len();

    r.intent(a, json!({"Nope": 1}));

    let errs = support::errors(&r, a, start);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, ErrorCode::BadIntent);
    assert_eq!(errs[0].2, Some(1));
    assert_eq!(r.client(a).closed, None, "the socket stays open");

    let start = r.client(a).events.len();
    r.intent(a, json!({"Add": 1}));
    assert_eq!(r.client(a).core.state()["count"], json!(1));
    assert_eq!(
        support::acks(&r, a, start),
        vec![(r.client(a).core.seq(), vec![2])]
    );
}

#[test]
fn batch_of_intents_is_one_patch_one_ack() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let a_start = r.client(a).events.len();
    let b_start = r.client(b).events.len();

    r.intents(a, vec![json!({"Add": 1}), json!("Fail"), json!({"Add": 2})]);

    assert_eq!(r.client(a).core.state()["count"], json!(3));
    assert_eq!(r.client(b).core.state()["count"], json!(3));
    assert_eq!(support::patched_ops(&r, a, a_start).len(), 1);
    assert_eq!(support::patched_ops(&r, b, b_start).len(), 1);
    assert_eq!(
        support::acks(&r, a, a_start),
        vec![(r.client(a).core.seq(), vec![1, 3])]
    );
    let rejected = support::errors(&r, a, a_start);
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].0, ErrorCode::IntentRejected);
    assert_eq!(rejected[0].2, Some(2));
    assert!(support::acks(&r, b, b_start).is_empty());
}

#[test]
fn state_persists_across_hibernation() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"Add": 7}));
    let seq_before = r.client(a).core.seq();
    assert_eq!(seq_before, 2);

    r.hibernate();
    let b = r.join("b", json!({}));

    assert_eq!(r.client(b).core.state()["count"], json!(7));
    assert_eq!(r.client(b).core.state()["seats"], json!({"a": 0, "b": 1}));
    assert_eq!(r.client(a).core.seq(), seq_before + 1);
    assert_eq!(r.client(b).core.seq(), seq_before + 1);
    assert_eq!(r.client(a).core.state(), r.client(b).core.state());
}

#[test]
fn unchanged_state_is_not_rewritten() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let start = r.client(a).events.len();
    let seq_before = support::stored_seq(&r);

    r.intent(a, json!({"Shout": "hi"}));

    let new_events = support::events_since(&r, a, start);
    assert!(
        new_events.iter().any(|event| match event {
            ClientEvent::Event { name, data } => name.as_str() == "shout" && *data == json!("hi"),
            _ => false,
        }),
        "the shout event arrives: {new_events:?}"
    );
    assert!(support::patched_ops(&r, a, start).is_empty());
    assert_eq!(
        support::stored_seq(&r),
        seq_before,
        "no write when nothing changed"
    );
}

#[test]
fn events_follow_patch_in_order() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let start = r.client(a).events.len();

    r.intents(a, vec![json!({"Add": 2}), json!({"Shout": "hey"})]);

    let events = support::events_since(&r, a, start);
    let patch = events
        .iter()
        .position(|event| matches!(event, ClientEvent::Patched { .. }))
        .expect("a patch");
    let shout = events
        .iter()
        .position(|event| matches!(event, ClientEvent::Event { .. }))
        .expect("an event");
    assert!(patch < shout, "patch then event: {events:?}");
    assert_eq!(r.client(a).core.state()["count"], json!(2));
    assert_eq!(r.client(b).core.state()["count"], json!(2));
}

#[test]
fn json_mode_client_interops() {
    let mut r = new_room();
    let a = r.join_with("a", Encoding::Json, json!({}));
    let b = r.join_with("b", Encoding::MsgPack, json!({}));

    r.intent(a, json!({"Add": 1}));
    r.intent(b, json!({"Add": 2}));

    assert_eq!(r.client(a).core.state()["count"], json!(3));
    assert_eq!(r.client(b).core.state()["count"], json!(3));
    assert_eq!(r.client(a).closed, None);
    assert_eq!(r.client(b).closed, None);
}

#[test]
fn frames_before_hello_get_not_joined() {
    let mut r = new_room();
    let a = r.connect_raw("a");
    let start = r.client(a).events.len();

    let intent = encode(
        &Frame::Intent {
            intents: vec![IntentMsg {
                seq: 1,
                intent: json!({"Add": 1}),
            }],
        },
        Encoding::MsgPack,
    );
    r.send_raw(a, intent);

    let errs = support::errors(&r, a, start);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, ErrorCode::NotJoined);
    assert_eq!(r.client(a).closed, None, "the socket stays open");

    let hello = encode(
        &Frame::Hello {
            protocol: PROTOCOL_VERSION,
            schema_hash: schema_hash(),
            opts: json!({}),
        },
        Encoding::MsgPack,
    );
    r.send_raw(a, hello);
    assert!(
        r.client(a)
            .events
            .iter()
            .any(|event| matches!(event, ClientEvent::Joined { seat: 0, .. })),
        "HELLO after the rejection still joins"
    );
    assert_eq!(r.client(a).core.state()["seats"], json!({"a": 0}));
}

#[test]
fn schema_mismatch_hard_rejects() {
    let mut r = new_room();
    let a = r.connect_raw("a");
    let server_hash = schema_hash();
    let client_hash = "0000000000000000";

    r.send_raw(
        a,
        encode(
            &Frame::Hello {
                protocol: PROTOCOL_VERSION,
                schema_hash: client_hash.into(),
                opts: json!({}),
            },
            Encoding::MsgPack,
        ),
    );

    let errs = support::errors(&r, a, 0);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, ErrorCode::SchemaMismatch);
    assert!(
        errs[0].1.contains(client_hash),
        "message names the client hash: {:?}",
        errs[0].1
    );
    assert!(
        errs[0].1.contains(&server_hash),
        "message names the server hash: {:?}",
        errs[0].1
    );
    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::SCHEMA_MISMATCH)
    );
    assert_eq!(close::SCHEMA_MISMATCH, 4001);
}

#[test]
fn wrong_protocol_version_rejected() {
    let mut r = new_room();
    let a = r.connect_raw("a");

    r.send_raw(
        a,
        encode(
            &Frame::Hello {
                protocol: 2,
                schema_hash: schema_hash(),
                opts: json!({}),
            },
            Encoding::MsgPack,
        ),
    );

    let errs = support::errors(&r, a, 0);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, ErrorCode::UnsupportedProtocol);
    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::BAD_FRAME),
        "wrong protocol closes with 4000"
    );
}

#[test]
fn room_full_rejected() {
    let mut r = new_room();
    let _a = r.join("a", json!({}));
    let _b = r.join("b", json!({}));
    assert_eq!(r.server_state()["join_calls"], json!(2));

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
        "on_join is not called when the room is full"
    );
    assert_eq!(r.server_state()["seats"], json!({"a": 0, "b": 1}));
}

#[test]
fn join_rejected_by_room() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let a_start = r.client(a).events.len();

    let b = r.join("b", json!({"reject": true}));

    let errs = support::errors(&r, b, 0);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, ErrorCode::JoinRejected);
    assert_eq!(
        r.client(b).closed.as_ref().map(|(code, _)| *code),
        Some(close::JOIN_REJECTED)
    );
    assert_eq!(r.server_state()["seats"], json!({"a": 0}), "no seat taken");
    assert!(
        support::events_since(&r, a, a_start).is_empty(),
        "other clients see no patch"
    );

    // The rejected join did not consume a seat.
    let c = r.join("c", json!({}));
    assert_eq!(r.client(c).core.state()["seats"], json!({"a": 0, "c": 1}));
}

#[test]
fn dev_sql_tables_work() {
    let mut r = new_room();
    let a = r.join("a", json!({}));

    r.intent(a, json!("Sql"));

    let rows = r
        .sql()
        .exec(
            "SELECT v FROM dev WHERE k = ?",
            vec![SqlValue::Text("hello".into())],
        )
        .expect("the dev table exists and is queryable");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("v"), Some(&json!(7)));
}
