//! `conformance/vectors/frames.json` is normative: every case must encode to
//! exactly its MessagePack bytes and decode back to exactly its JSON.
//! `UPDATE_VECTORS=1` regenerates the file from the frames below (only for a
//! deliberate, additive protocol change).

use hearth_protocol::{decode, encode, Encoding, ErrorCode, Frame, IntentMsg, PatchOp, PlayerId, WireMsg};
use serde_json::{json, Value};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

/// One case per frame variant and per optional-field combination.
fn frames() -> Vec<(&'static str, &'static str, Frame)> {
    vec![
        ("hello_empty_opts", "c2s", Frame::Hello { protocol: 1, schema_hash: "0123456789abcdef".into(), opts: json!({}) }),
        ("hello_with_opts", "c2s", Frame::Hello { protocol: 1, schema_hash: "0123456789abcdef".into(), opts: json!({"name": "ana", "team": 2}) }),
        ("join_ok", "s2c", Frame::JoinOk { player_id: PlayerId::from("anon_a"), seat: 1 }),
        ("full_state", "s2c", Frame::FullState { seq: 7, state: json!({"cells": [0, 1, 2], "turn": "anon_a", "status": {"Won": "anon_a"}}) }),
        (
            "patch_event_all_ops",
            "s2c",
            Frame::Patch {
                seq: 8,
                tick: None,
                t: None,
                ops: vec![
                    PatchOp::Set { path: vec![json!("turn")], value: json!("anon_b") },
                    PatchOp::Del { path: vec![json!("marks"), json!("anon_c")] },
                    PatchOp::MapInsert { path: vec![json!("marks")], key: "anon_b".into(), value: json!(2) },
                    PatchOp::ListSplice { path: vec![json!("cells")], index: 1, delete: 1, insert: vec![json!(2), json!(0)] },
                ],
            },
        ),
        (
            "patch_ticked",
            "s2c",
            Frame::Patch {
                seq: 20,
                tick: Some(41),
                t: Some(1_760_000_000_050),
                ops: vec![PatchOp::Set { path: vec![json!("avatars"), json!("anon_a"), json!("pos")], value: json!([1.5, 2.0]) }],
            },
        ),
        ("intent_batch", "c2s", Frame::Intent { intents: vec![IntentMsg { seq: 1, intent: json!({"Place": {"cell": 4}}) }, IntentMsg { seq: 2, intent: json!("Rematch") }] }),
        ("event", "s2c", Frame::Event { name: "shout".into(), data: json!({"text": "hi"}) }),
        ("ack_event", "s2c", Frame::Ack { state_seq: 9, intent_seqs: vec![1, 2], tick: None }),
        ("ack_ticked", "s2c", Frame::Ack { state_seq: 20, intent_seqs: vec![3], tick: Some(42) }),
        ("error_with_ref_seq", "s2c", Frame::Error { code: ErrorCode::BadIntent, message: "intent did not decode".into(), ref_seq: Some(3) }),
        ("error_without_ref_seq", "s2c", Frame::Error { code: ErrorCode::SchemaMismatch, message: "schema mismatch".into(), ref_seq: None }),
        ("leave", "c2s", Frame::Leave {}),
    ]
}

#[test]
fn frames_match_vectors() {
    if std::env::var_os("UPDATE_VECTORS").is_some() {
        let cases: Vec<Value> = frames()
            .into_iter()
            .map(|(name, direction, frame)| {
                let WireMsg::Binary(bytes) = encode(&frame, Encoding::MsgPack) else { unreachable!() };
                json!({ "name": name, "direction": direction, "json": serde_json::to_value(&frame).unwrap(), "msgpack_hex": hex(&bytes) })
            })
            .collect();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../conformance/vectors/frames.json");
        std::fs::write(path, serde_json::to_string_pretty(&cases).unwrap() + "\n").unwrap();
        return;
    }

    let cases = hearth_protocol::vectors::load("frames.json");
    assert_eq!(cases.len(), frames().len(), "one vector per frame shape");
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let bytes = unhex(case["msgpack_hex"].as_str().unwrap());
        let frame: Frame = serde_json::from_value(case["json"].clone()).unwrap_or_else(|e| panic!("{name}: {e}"));

        let WireMsg::Binary(encoded) = encode(&frame, Encoding::MsgPack) else { unreachable!() };
        assert_eq!(hex(&encoded), case["msgpack_hex"], "{name}: encode(json) != hex");

        let decoded = decode(&WireMsg::Binary(bytes)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(serde_json::to_value(&decoded).unwrap(), case["json"], "{name}: decode(hex) != json");

        let WireMsg::Text(text) = encode(&frame, Encoding::Json) else { unreachable!() };
        let from_text: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(from_text, case["json"], "{name}: JSON mode is the frame object");
    }
}

/// `forward.json`: frames from a newer protocol revision that every client must
/// survive (`spec/protocol-v1.md` section 12). `inject` marks the cases the
/// conformance server sends in its `forward_compat` scenario; `json`/`msgpack_hex`
/// is null where a case exists in one encoding only.
fn forward_cases() -> Vec<Value> {
    let case = |name: &str, opcode: Option<u8>, frame: Value, json_too: bool, expect: Value, inject: bool| {
        let msgpack_hex = opcode.map(|op| {
            let mut bytes = vec![op];
            bytes.extend(rmp_serde::to_vec_named(&frame).unwrap());
            hex(&bytes)
        });
        json!({
            "name": name,
            "direction": "s2c",
            "json": if json_too { frame.clone() } else { Value::Null },
            "msgpack_hex": msgpack_hex,
            "expect": expect,
            "inject": inject,
        })
    };
    vec![
        case(
            "patch_extra_field",
            Some(0x04),
            json!({"op": "PATCH", "seq": 2, "ops": [{"op": "set", "path": ["n"], "value": 5}], "z": 1}),
            true,
            json!({"event": {"type": "patched"}, "state_after": {"n": 5}}),
            false,
        ),
        case(
            "join_ok_extra_field",
            Some(0x02),
            json!({"op": "JOIN_OK", "player_id": "p1", "seat": 0, "z": {"a": 1}}),
            true,
            json!({"event": {"type": "joined", "player_id": "p1", "seat": 0}}),
            false,
        ),
        case(
            "event_extra_field",
            Some(0x06),
            json!({"op": "EVENT", "name": "echo", "data": "x", "z": 1}),
            true,
            json!({"event": {"type": "event", "name": "echo", "data": "x"}}),
            true,
        ),
        case("unknown_binary_opcode", Some(0x40), json!({"op": "FUTURE", "x": 1}), false, json!("ignored"), true),
        case("unknown_json_op", None, json!({"op": "FUTURE", "x": 1}), true, json!("ignored"), true),
        case(
            "future_hearth_event",
            Some(0x06),
            json!({"op": "EVENT", "name": "hearth.future", "data": {"v": 1}}),
            true,
            json!("event_or_ignored"),
            true,
        ),
        case(
            "error_unknown_code",
            Some(0x09),
            json!({"op": "ERROR", "code": "FUTURE_CODE", "message": "from the future", "ref_seq": null}),
            true,
            json!({"event": {"type": "error", "code": "FUTURE_CODE"}}),
            true,
        ),
    ]
}

#[test]
fn forward_vectors_are_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../conformance/vectors/forward.json");
    if std::env::var_os("UPDATE_VECTORS").is_some() {
        std::fs::write(path, serde_json::to_string_pretty(&forward_cases()).unwrap() + "\n").unwrap();
        return;
    }
    assert_eq!(Value::Array(hearth_protocol::vectors::load("forward.json")), Value::Array(forward_cases()));
}

#[test]
#[should_panic(expected = "conformance/vectors/nope.json")]
fn loader_fails_on_missing_file() {
    hearth_protocol::vectors::load("nope.json");
}
