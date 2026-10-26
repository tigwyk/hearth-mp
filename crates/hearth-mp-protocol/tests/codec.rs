use hearth_mp_protocol::{
    decode, encode, opcode, Encoding, ErrorCode, Frame, IntentMsg, Opcode, PatchOp, PlayerId,
    ProtocolError, WireMsg, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

fn sample_frames() -> Vec<Frame> {
    vec![
        Frame::Hello {
            protocol: PROTOCOL_VERSION,
            schema_hash: "0123456789abcdef".to_string(),
            opts: json!({ "token": "t0k3n", "nested": { "n": 1 } }),
        },
        Frame::JoinOk {
            player_id: PlayerId::from("player-1"),
            seat: 2,
        },
        Frame::FullState {
            seq: 3,
            state: json!({ "turn": "a", "nested": [1, 2, { "b": null }] }),
        },
        Frame::Patch {
            seq: 4,
            ops: vec![
                PatchOp::Set {
                    path: vec![json!("turn")],
                    value: json!("b"),
                },
                PatchOp::Del {
                    path: vec![json!("players"), json!("a")],
                },
                PatchOp::MapInsert {
                    path: vec![json!("seats")],
                    key: "b".to_string(),
                    value: json!(1),
                },
                PatchOp::ListSplice {
                    path: vec![],
                    index: 1,
                    delete: 2,
                    insert: vec![json!(9), json!({"x": true})],
                },
            ],
            tick: None,
            t: None,
        },
        Frame::Intent {
            intents: vec![
                IntentMsg {
                    seq: 1,
                    intent: json!({ "Move": { "cell": 4 } }),
                },
                IntentMsg {
                    seq: 2,
                    intent: json!("Rematch"),
                },
            ],
        },
        Frame::Event {
            name: "shout".to_string(),
            data: json!({ "text": "hi" }),
        },
        Frame::Ack {
            state_seq: 9,
            intent_seqs: vec![1, 2],
            tick: None,
        },
        Frame::Error {
            code: ErrorCode::BadIntent,
            message: "that intent did not decode".to_string(),
            ref_seq: Some(3),
        },
        Frame::Leave {},
    ]
}

#[test]
fn binary_frame_is_opcode_byte_then_named_msgpack() {
    let frame = Frame::Patch {
        seq: 7,
        ops: vec![],
        tick: None,
        t: None,
    };
    let WireMsg::Binary(bytes) = encode(&frame, Encoding::MsgPack) else {
        panic!("MsgPack encoding must be binary");
    };
    assert_eq!(bytes[0], 0x04);
    let body: Value = rmp_serde::from_slice(&bytes[1..]).unwrap();
    assert_eq!(body, json!({ "op": "PATCH", "seq": 7, "ops": [] }));
}

#[test]
fn json_frame_is_text_with_op_tag() {
    let WireMsg::Text(text) = encode(&Frame::Leave {}, Encoding::Json) else {
        panic!("JSON encoding must be text");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({"op":"LEAVE"})
    );
}

#[test]
fn every_frame_round_trips_in_both_encodings() {
    for frame in sample_frames() {
        for encoding in [Encoding::MsgPack, Encoding::Json] {
            let wire = encode(&frame, encoding);
            assert_eq!(
                decode(&wire),
                Ok(frame.clone()),
                "{frame:?} in {encoding:?}"
            );
        }
    }
}

#[test]
fn schema_hash_survives_json_as_exact_string() {
    let frame = Frame::Hello {
        protocol: 1,
        schema_hash: "ffffffffffffffff".into(),
        opts: json!({}),
    };
    let WireMsg::Text(text) = encode(&frame, Encoding::Json) else {
        panic!("JSON encoding must be text");
    };
    assert!(text.contains("\"schema_hash\":\"ffffffffffffffff\""));
}

#[test]
fn state_frames_over_the_intent_cap_decode() {
    // The 64 KiB cap is on frames to the server (it checks the size itself); a
    // client must accept a larger FULL_STATE.
    let frame = Frame::FullState {
        seq: 1,
        state: json!({ "blob": "x".repeat(MAX_FRAME_BYTES * 2) }),
    };
    for encoding in [Encoding::MsgPack, Encoding::Json] {
        let msg = encode(&frame, encoding);
        assert!(
            matches!(&msg, WireMsg::Binary(b) if b.len() > MAX_FRAME_BYTES)
                || matches!(&msg, WireMsg::Text(t) if t.len() > MAX_FRAME_BYTES)
        );
        assert_eq!(decode(&msg), Ok(frame.clone()));
    }
}

#[test]
fn opcode_byte_must_match_body_tag() {
    // Valid PATCH opcode byte, but the body is a LEAVE frame.
    let body = rmp_serde::to_vec_named(&Frame::Leave {}).unwrap();
    let mut bytes = vec![0x04];
    bytes.extend(body);
    assert_eq!(
        decode(&WireMsg::Binary(bytes)),
        Err(ProtocolError::OpcodeMismatch)
    );
}

#[test]
fn unknown_opcodes_are_their_own_error() {
    assert_eq!(
        decode(&WireMsg::Binary(vec![0x40, 0x80])),
        Err(ProtocolError::UnknownOpcode("0x40".into()))
    );
    assert_eq!(
        decode(&WireMsg::Text(r#"{"op":"FUTURE","x":1}"#.into())),
        Err(ProtocolError::UnknownOpcode("FUTURE".into()))
    );
}

#[test]
fn garbage_is_malformed() {
    for wire in [
        WireMsg::Binary(vec![0x04, 0xC1]),
        WireMsg::Binary(vec![]),
        WireMsg::Text(r#"{"op":"PATCH"}"#.to_string()),
        WireMsg::Text("nope".to_string()),
        WireMsg::Text("{".to_string()),
    ] {
        assert!(
            matches!(decode(&wire), Err(ProtocolError::Malformed(_))),
            "{wire:?} should be malformed"
        );
    }
}

#[test]
fn opcode_table_is_stable() {
    assert_eq!(opcode(&Frame::Leave {}), Opcode::Leave);
    assert_eq!(Opcode::Hello.as_u8(), 0x01);
    assert_eq!(Opcode::Leave.as_u8(), 0x0A);
    assert_eq!(Opcode::Ping.as_u8(), 0x08);
}
