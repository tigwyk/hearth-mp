//! Plan v0.2c Task 2: the admin tail (unfiltered full-state diffs with causes).

mod support;

use std::time::Duration;

use hearth_mp_client::ClientEvent;
use hearth_mp_protocol::{close, encode, Encoding, Frame};
use hearth_mp_test::TestRoom;
use serde_json::{json, Value};
use support::CounterRoom;

type Room = TestRoom<CounterRoom>;

fn new_room() -> Room {
    TestRoom::new()
}

fn messages(msgs: &[Value]) -> String {
    format!("{msgs:?}")
}

#[test]
fn tail_sees_full_unfiltered_diffs_with_cause() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let t = r.tail();
    assert!(r.tail_msgs(t).is_empty(), "a tail starts with no frames");

    r.intent(a, json!({"Secret": "s"}));
    let msgs = r.tail_msgs(t);
    assert_eq!(
        msgs.len(),
        1,
        "one pass, one tail frame: {}",
        messages(&msgs)
    );
    let msg = &msgs[0];
    assert_eq!(
        msg["cause"],
        json!([{"cause": "intent", "player": "a", "intent_seq": 1}])
    );
    let ops = msg["ops"].as_array().expect("ops is an array");
    assert!(
        ops.iter().any(|op| {
            op["op"] == json!("map_insert")
                && op["path"] == json!(["secrets"])
                && op["key"] == json!("a")
                && op["value"] == json!("s")
        }),
        "the tail sees owner-only state: {ops:?}"
    );
    assert_eq!(msg["rejected"], json!([]));
}

#[test]
fn tail_sees_rejected_intents() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let t = r.tail();

    r.intent(a, json!("Fail"));
    let msgs = r.tail_msgs(t);
    assert_eq!(msgs.len(), 1, "{}", messages(&msgs));
    assert_eq!(msgs[0]["rejected"], json!([1]));
    assert_eq!(msgs[0]["ops"], json!([]), "the rejected intent rolled back");
    assert_eq!(msgs[0]["cause"], json!([]));
}

#[test]
fn tail_survives_hibernation() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let t = r.tail();
    r.hibernate();

    r.intent(a, json!({"Add": 7}));
    let msgs = r.tail_msgs(t);
    assert_eq!(msgs.len(), 1, "{}", messages(&msgs));
    assert_eq!(
        msgs[0]["cause"],
        json!([{"cause": "intent", "player": "a", "intent_seq": 1}])
    );
    let ops = msgs[0]["ops"].as_array().expect("ops is an array");
    assert!(
        ops.iter().any(|op| {
            op["op"] == json!("set") && op["path"] == json!(["count"]) && op["value"] == json!(7)
        }),
        "{ops:?}"
    );
}

#[test]
fn tail_never_takes_a_seat() {
    let mut r = new_room();
    let t = r.tail();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));

    assert!(matches!(
        r.client(a).events[0],
        ClientEvent::Joined { seat: 0, .. }
    ));
    assert!(
        r.client(b)
            .events
            .iter()
            .any(|event| matches!(event, ClientEvent::Joined { seat: 1, .. })),
        "the tail did not take the second seat"
    );
    assert!(r.tail_closed(t).is_none(), "the tail is still open");

    let c = r.join("c", json!({}));
    assert_eq!(
        r.client(c).closed.as_ref().map(|(code, _)| *code),
        Some(close::ROOM_FULL),
        "the room is full with two players, tail excluded"
    );
}

#[test]
fn tail_gets_no_player_frames() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let t = r.tail();
    r.intent(a, json!({"Shout": "hi"}));
    r.intent(a, json!({"Add": 1}));

    let msgs = r.tail_msgs(t);
    assert_eq!(
        msgs.len(),
        1,
        "an event-only pass changes no state, so only the Add pass tails: {}",
        messages(&msgs)
    );
    for msg in &msgs {
        let object = msg.as_object().expect("a tail frame is a JSON object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["cause", "ops", "rejected", "seq"],
            "the tail only receives TailMsg frames: {msg}"
        );
    }
    assert!(
        r.client(a)
            .events
            .iter()
            .any(|event| matches!(event, ClientEvent::Event { name, .. } if name == "shout")),
        "players still receive their own frames"
    );
}

#[test]
fn tail_does_not_keep_empty_room_alive() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let t = r.tail();
    r.send_raw(a, encode(&Frame::Leave {}, Encoding::MsgPack));
    r.advance(Duration::from_secs(60));

    assert!(
        support::table_names(&r).is_empty(),
        "the empty room was disposed"
    );
    assert_eq!(
        r.tail_closed(t),
        Some((close::DISPOSED, "disposed".to_owned())),
        "the tail is closed with the dispose code"
    );
}

#[test]
fn no_tail_no_extra_work() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    r.intent(a, json!({"Add": 1}));
    r.intent(a, json!({"Secret": "s"}));
    r.drop_socket(a);
    r.advance(Duration::from_secs(30));
    assert_eq!(
        r.full_state_count(),
        0,
        "no admin tail, no full-state renders"
    );

    let t = r.tail();
    r.intent(b, json!({"Add": 1}));
    assert!(
        r.full_state_count() > 0,
        "a tail forces the unfiltered renders"
    );
    assert!(!r.tail_msgs(t).is_empty());
}
