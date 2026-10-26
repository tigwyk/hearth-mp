//! The WebGL exports, run under Node:
//! `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
//!  cargo test -p hearth-unity --features webgl --target wasm32-unknown-unknown --test web`
#![cfg(all(target_arch = "wasm32", feature = "webgl"))]

use hearth_protocol::{decode, encode, Encoding, Frame, PlayerId, WireMsg};
use hearth_unity::web::*;
use serde_json::{json, Value};
use wasm_bindgen_test::wasm_bindgen_test;

const HASH: &str = "0123456789abcdef";

fn feed(h: u32, frame: &Frame) -> Value {
    let WireMsg::Binary(bytes) = encode(frame, Encoding::MsgPack) else {
        unreachable!()
    };
    serde_json::from_str(&client_handle(h, &bytes, false)).unwrap()
}

#[wasm_bindgen_test]
fn hello_then_handshake_round_trip() {
    let h = client_new(HASH, 0);
    assert_ne!(h, 0);
    let hello = client_hello(h, r#"{"name":"ana"}"#);
    assert!(!last_is_text());
    let Frame::Hello { schema_hash, .. } = decode(&WireMsg::Binary(hello)).unwrap() else {
        panic!("expected HELLO");
    };
    assert_eq!(schema_hash, HASH);
    let events = feed(
        h,
        &Frame::JoinOk {
            player_id: PlayerId::from("p1"),
            seat: 0,
        },
    );
    assert_eq!(events[0]["type"], "joined");
    let events = feed(
        h,
        &Frame::FullState {
            seq: 1,
            state: json!({"x": 1}),
        },
    );
    assert_eq!(events, json!([{"type": "state_reset"}]));
    assert_eq!(client_state(h), r#"{"x":1}"#);
    client_free(h);
}

#[wasm_bindgen_test]
fn queue_and_poll_batches() {
    let h = client_new(HASH, 1);
    for i in 1..=3 {
        assert_eq!(client_queue_intent(h, "1"), i);
    }
    let frame = client_poll_send(h, 0.0);
    assert!(last_is_text());
    let Frame::Intent { intents } =
        decode(&WireMsg::Text(String::from_utf8(frame).unwrap())).unwrap()
    else {
        panic!("expected INTENT");
    };
    assert_eq!(intents.len(), 3);
    assert!(client_poll_send(h, 0.0).is_empty());
    client_free(h);
}

#[wasm_bindgen_test]
fn garbage_frame_is_error_event_not_crash() {
    let h = client_new(HASH, 0);
    let events: Value = serde_json::from_str(&client_handle(h, &[0xFF, 0, 1], false)).unwrap();
    assert_eq!(events[0]["code"], "BAD_FRAME");
    client_free(h);
}

#[wasm_bindgen_test]
fn reconnect_fixture_subset() {
    assert_eq!(reconnect_decide(1006, 1000.0, 0, 2000.0), 2500.0);
    assert_eq!(reconnect_decide(1006, 1000.0, 5, 2000.0), 10000.0);
    assert_eq!(reconnect_decide(4001, 1000.0, 0, 2000.0), -1.0);
    assert_eq!(reconnect_decide(1000, 1000.0, 0, 2000.0), -1.0);
}

#[wasm_bindgen_test]
fn freed_handle_is_safe() {
    let h = client_new(HASH, 0);
    client_free(h);
    client_free(h);
    assert!(client_hello(h, "{}").is_empty());
    assert_eq!(client_handle(h, &[1], false), "");
    assert_eq!(client_queue_intent(h, "1"), 0);
    assert!(client_poll_send(h, 0.0).is_empty());
    assert!(client_leave(h).is_empty());
    assert_eq!(client_state(h), "");
    assert!(last_error().contains("unknown client handle"));
    assert_eq!(client_queue_intent(0, "1"), 0);
}
