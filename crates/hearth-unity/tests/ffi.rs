//! The C ABI, called from Rust exactly as C# calls it.

use std::ffi::CString;

use hearth_protocol::{decode, encode, Encoding, Frame, PlayerId, WireMsg};
use hearth_unity::*;
use serde_json::{json, Value};

const HASH: &str = "0123456789abcdef";

/// Copy a buffer out and free it.
fn take(buf: HearthBuf) -> Vec<u8> {
    let bytes = if buf.ptr.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) }.to_vec()
    };
    unsafe { hearth_buf_free(buf) };
    bytes
}

fn take_json(buf: HearthBuf) -> Value {
    serde_json::from_slice(&take(buf)).expect("the buffer is JSON")
}

fn last_error_text() -> String {
    String::from_utf8(take(hearth_last_error())).unwrap()
}

fn new_client(encoding: u8) -> *mut HearthClient {
    let hash = CString::new(HASH).unwrap();
    let c = unsafe { hearth_client_new(hash.as_ptr(), encoding) };
    assert!(!c.is_null());
    c
}

fn feed(c: *mut HearthClient, frame: &Frame) -> Value {
    let (bytes, is_text) = match encode(frame, Encoding::MsgPack) {
        WireMsg::Binary(b) => (b, 0),
        WireMsg::Text(t) => (t.into_bytes(), 1),
    };
    take_json(unsafe { hearth_client_handle(c, bytes.as_ptr(), bytes.len(), is_text) })
}

#[test]
fn hello_then_handshake_round_trip() {
    let c = new_client(0);
    let opts = CString::new(r#"{"name":"ana"}"#).unwrap();
    let mut is_text = 9u8;
    let hello = take(unsafe { hearth_client_hello(c, opts.as_ptr(), &mut is_text) });
    assert_eq!(is_text, 0, "msgpack frames are binary");
    let Frame::Hello {
        schema_hash, opts, ..
    } = decode(&WireMsg::Binary(hello)).unwrap()
    else {
        panic!("expected HELLO");
    };
    assert_eq!(schema_hash, HASH);
    assert_eq!(opts, json!({"name": "ana"}));

    let events = feed(
        c,
        &Frame::JoinOk {
            player_id: PlayerId::from("p1"),
            seat: 2,
        },
    );
    assert_eq!(
        events,
        json!([{"type": "joined", "player_id": "p1", "seat": 2}])
    );
    let events = feed(
        c,
        &Frame::FullState {
            seq: 3,
            state: json!({"cells": [0, 0]}),
        },
    );
    assert_eq!(events, json!([{"type": "state_reset"}]));
    assert_eq!(
        take_json(unsafe { hearth_client_state(c) }),
        json!({"cells": [0, 0]})
    );
    assert_eq!(unsafe { hearth_client_seq(c) }, 3);
    unsafe { hearth_client_free(c) };
}

#[test]
fn queue_and_poll_batches() {
    let c = new_client(1);
    for i in 0..3 {
        let intent = CString::new(format!(r#"{{"Place":{i}}}"#)).unwrap();
        assert_eq!(
            unsafe { hearth_client_queue_intent(c, intent.as_ptr()) },
            i + 1
        );
    }
    let mut is_text = 0u8;
    let frame = take(unsafe { hearth_client_poll_send(c, 0, &mut is_text) });
    assert_eq!(is_text, 1, "json frames are text");
    let Frame::Intent { intents } =
        decode(&WireMsg::Text(String::from_utf8(frame).unwrap())).unwrap()
    else {
        panic!("expected INTENT");
    };
    assert_eq!(intents.len(), 3);
    assert!(take(unsafe { hearth_client_poll_send(c, 0, &mut is_text) }).is_empty());
    unsafe { hearth_client_free(c) };
}

#[test]
fn invalid_intent_json_returns_zero_and_sets_error() {
    let c = new_client(0);
    let bad = CString::new("{not json").unwrap();
    assert_eq!(unsafe { hearth_client_queue_intent(c, bad.as_ptr()) }, 0);
    assert!(last_error_text().contains("bad intent JSON"));
    unsafe { hearth_client_free(c) };
}

#[test]
fn null_handle_is_safe() {
    let null = std::ptr::null_mut();
    let s = CString::new("{}").unwrap();
    let mut t = 0u8;
    unsafe {
        assert!(hearth_client_new(std::ptr::null(), 0).is_null());
        hearth_client_free(null);
        assert!(take(hearth_client_hello(null, s.as_ptr(), &mut t)).is_empty());
        assert!(take(hearth_client_handle(null, [1u8].as_ptr(), 1, 0)).is_empty());
        assert_eq!(hearth_client_queue_intent(null, s.as_ptr()), 0);
        assert!(take(hearth_client_poll_send(null, 0, &mut t)).is_empty());
        assert!(take(hearth_client_leave(null, &mut t)).is_empty());
        assert!(take(hearth_client_on_disconnected(null)).is_empty());
        assert!(take(hearth_client_state(null)).is_empty());
        assert_eq!(hearth_client_seq(null), 0);
    }
    assert!(last_error_text().contains("null"));
}

#[test]
fn garbage_frame_is_error_event_not_crash() {
    let c = new_client(0);
    let garbage = [0xFFu8, 0x00, 0x13];
    let events = take_json(unsafe { hearth_client_handle(c, garbage.as_ptr(), garbage.len(), 0) });
    assert_eq!(events[0]["type"], "error");
    assert_eq!(events[0]["code"], "BAD_FRAME");
    assert_eq!(events[0]["ref_seq"], Value::Null);
    unsafe { hearth_client_free(c) };
}

#[test]
fn error_event_and_unconfirmed_shapes() {
    let c = new_client(0);
    let intent = CString::new("1").unwrap();
    unsafe { hearth_client_queue_intent(c, intent.as_ptr()) };
    let mut t = 0u8;
    take(unsafe { hearth_client_poll_send(c, 0, &mut t) });
    let events = take_json(unsafe { hearth_client_on_disconnected(c) });
    assert_eq!(
        events,
        json!([{"type": "intents_unconfirmed", "seqs": [1]}])
    );
    let events = feed(
        c,
        &Frame::Error {
            code: hearth_protocol::ErrorCode::SchemaMismatch,
            message: "nope".into(),
            ref_seq: None,
        },
    );
    assert_eq!(
        events,
        json!([{"type": "error", "code": "SCHEMA_MISMATCH", "message": "nope", "ref_seq": null}])
    );
    unsafe { hearth_client_free(c) };
}

#[test]
fn reconnect_decide_matches_fixture() {
    let cases: Value = serde_json::from_str(include_str!(
        "../../hearth-client/tests/fixtures/reconnect_cases.json"
    ))
    .unwrap();
    for case in cases.as_array().unwrap() {
        let actual = hearth_reconnect_decide(
            case["code"].as_u64().unwrap() as u16,
            case["first_drop_ms"].as_u64().unwrap(),
            case["attempt"].as_u64().unwrap() as u32,
            case["now_ms"].as_u64().unwrap(),
        );
        let expect = match &case["expect"] {
            Value::String(_) => -1,
            n => n.as_i64().unwrap(),
        };
        assert_eq!(actual, expect, "case {}", case["name"]);
    }
}

#[test]
fn buf_free_of_empty_is_noop() {
    unsafe {
        hearth_buf_free(HearthBuf {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        })
    };
}
