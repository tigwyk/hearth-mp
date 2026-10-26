//! A newer server talking to this client (v1.0a Review Focus 1): every
//! `conformance/vectors/forward.json` case, in each encoding it exists in, must
//! be survived without a desync or an error.

use hearth_mp_client::{ClientConfig, ClientCore};
use hearth_mp_protocol::{encode, Encoding, Frame, WireMsg};
use serde_json::{json, Value};

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

/// `pattern` is a subset of `value`: every field it gives is equal.
fn matches(pattern: &Value, value: &Value) -> bool {
    match (pattern, value) {
        (Value::Object(p), Value::Object(v)) => p.iter().all(|(k, pv)| v.get(k).is_some_and(|vv| matches(pv, vv))),
        _ => pattern == value,
    }
}

/// A joined client holding `{"n": 0}` at seq 1.
fn joined(encoding: Encoding) -> ClientCore {
    let mut core = ClientCore::new(ClientConfig::new("0123456789abcdef", encoding));
    core.handle(&encode(&Frame::FullState { seq: 1, state: json!({"n": 0}) }, encoding)).unwrap();
    core
}

#[test]
fn every_forward_case_is_survived() {
    let mut ran = 0;
    for case in hearth_mp_protocol::vectors::load("forward.json") {
        let name = case["name"].as_str().unwrap();
        let mut wires = Vec::new();
        if let Some(text) = case["json"].as_object() {
            wires.push((Encoding::Json, WireMsg::Text(Value::Object(text.clone()).to_string())));
        }
        if let Some(hex) = case["msgpack_hex"].as_str() {
            wires.push((Encoding::MsgPack, WireMsg::Binary(unhex(hex))));
        }
        assert!(!wires.is_empty(), "{name}: no encoding");
        for (encoding, wire) in wires {
            let mut core = joined(encoding);
            let events = core
                .handle(&wire)
                .unwrap_or_else(|e| panic!("{name} ({encoding:?}): must not be an error: {e}"));
            let events = serde_json::to_value(&events).unwrap();
            let events = events.as_array().unwrap();
            let expect = &case["expect"];
            match expect.as_str() {
                Some("ignored") => {
                    assert!(events.is_empty(), "{name} ({encoding:?}): ignored, got {events:?}");
                    assert_eq!(core.state(), &json!({"n": 0}), "{name}: state untouched");
                }
                Some("event_or_ignored") => assert!(events.len() <= 1, "{name}: {events:?}"),
                _ => {
                    assert!(
                        events.first().is_some_and(|e| matches(&expect["event"], e)),
                        "{name} ({encoding:?}): expected {}, got {events:?}",
                        expect["event"]
                    );
                    if let Some(after) = expect.get("state_after") {
                        assert_eq!(core.state(), after, "{name}: state after");
                    }
                }
            }
            // Still usable afterwards: a normal patch applies.
            let next = encode(
                &Frame::Patch { seq: 10, tick: None, t: None, ops: vec![hearth_mp_protocol::PatchOp::Set { path: vec![json!("n")], value: json!(9) }] },
                encoding,
            );
            assert!(!core.handle(&next).unwrap().is_empty(), "{name}: the client still applies patches");
            ran += 1;
        }
    }
    assert!(ran > 0);
}

#[test]
fn unknown_fatal_close_code_gives_up() {
    use hearth_mp_client::{Reconnect, ReconnectPolicy};
    assert_eq!(ReconnectPolicy::default().decide(4050, 0, 0, 0), Reconnect::GiveUp);
}
