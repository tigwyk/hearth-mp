use hearth_client::{ClientConfig, ClientCore, ClientEvent};
use hearth_protocol::{
    decode, encode, Encoding, ErrorCode, Frame, IntentMsg, PatchOp, PlayerId, ProtocolError, Value,
    WireMsg, PING_TEXT, PONG_TEXT, PROTOCOL_VERSION,
};
use serde_json::json;

const HASH: &str = "0123456789abcdef";

fn config(encoding: Encoding) -> ClientConfig {
    ClientConfig::new(HASH, encoding)
}

/// Feed one hand-built server frame through `encode` and `handle`.
fn recv(core: &mut ClientCore, frame: &Frame, encoding: Encoding) -> Vec<ClientEvent> {
    core.handle(&encode(frame, encoding))
        .expect("a server frame decodes")
}

#[test]
fn hello_carries_schema_hash_and_protocol_1() {
    let core = ClientCore::new(config(Encoding::MsgPack));
    let hello = core.hello(json!({"name": "ana"}));
    let Frame::Hello {
        protocol,
        schema_hash,
        opts,
    } = decode(&hello).expect("HELLO decodes")
    else {
        panic!("expected a HELLO frame");
    };
    assert_eq!(protocol, PROTOCOL_VERSION);
    assert_eq!(protocol, 1);
    assert_eq!(schema_hash, HASH);
    assert_eq!(opts, json!({"name": "ana"}));
}

#[test]
fn full_state_then_patch_updates_state_and_seq() {
    let mut core = ClientCore::new(config(Encoding::MsgPack));

    let events = recv(
        &mut core,
        &Frame::FullState {
            seq: 4,
            state: json!({"count": 0}),
        },
        Encoding::MsgPack,
    );
    assert_eq!(events, vec![ClientEvent::StateReset]);
    assert_eq!(core.seq(), 4);
    assert_eq!(core.state(), &json!({"count": 0}));

    let ops = vec![
        PatchOp::Set {
            path: vec![json!("count")],
            value: json!(1),
        },
        PatchOp::MapInsert {
            path: vec![],
            key: "extra".into(),
            value: json!(true),
        },
    ];
    let events = recv(
        &mut core,
        &Frame::Patch {
            seq: 5,
            ops: ops.clone(),
        },
        Encoding::MsgPack,
    );
    assert_eq!(events, vec![ClientEvent::Patched { ops }]);
    assert_eq!(core.seq(), 5);
    assert_eq!(core.state(), &json!({"count": 1, "extra": true}));
}

#[test]
fn stale_patch_ignored() {
    let mut core = ClientCore::new(config(Encoding::Json));
    recv(
        &mut core,
        &Frame::FullState {
            seq: 5,
            state: json!({"x": 1}),
        },
        Encoding::Json,
    );

    let stale = recv(
        &mut core,
        &Frame::Patch {
            seq: 3,
            ops: vec![PatchOp::Set {
                path: vec![json!("x")],
                value: json!(2),
            }],
        },
        Encoding::Json,
    );
    assert!(stale.is_empty(), "a stale patch emits no event");

    // A patch at exactly the current seq is stale too.
    let duplicate = recv(
        &mut core,
        &Frame::Patch {
            seq: 5,
            ops: vec![PatchOp::Del {
                path: vec![json!("x")],
            }],
        },
        Encoding::Json,
    );
    assert!(duplicate.is_empty());

    assert_eq!(core.seq(), 5);
    assert_eq!(core.state(), &json!({"x": 1}));
}

#[test]
fn bad_patch_path_emits_desync_then_full_state_recovers() {
    let mut core = ClientCore::new(config(Encoding::MsgPack));
    recv(
        &mut core,
        &Frame::FullState {
            seq: 1,
            state: json!({"a": {"b": 1}}),
        },
        Encoding::MsgPack,
    );

    let events = recv(
        &mut core,
        &Frame::Patch {
            seq: 2,
            ops: vec![PatchOp::Del {
                path: vec![json!("missing")],
            }],
        },
        Encoding::MsgPack,
    );
    assert_eq!(events, vec![ClientEvent::Desync]);
    assert_eq!(
        core.state(),
        &json!({"a": {"b": 1}}),
        "the failed op changes nothing"
    );
    assert_eq!(core.seq(), 1);

    // Patches are ignored while desynced.
    let ignored = recv(
        &mut core,
        &Frame::Patch {
            seq: 3,
            ops: vec![PatchOp::Set {
                path: vec![json!("a")],
                value: json!(9),
            }],
        },
        Encoding::MsgPack,
    );
    assert!(ignored.is_empty());
    assert_eq!(core.state(), &json!({"a": {"b": 1}}));

    // FULL_STATE recovers, and patching works again.
    let events = recv(
        &mut core,
        &Frame::FullState {
            seq: 9,
            state: json!({"a": 0}),
        },
        Encoding::MsgPack,
    );
    assert_eq!(events, vec![ClientEvent::StateReset]);
    assert_eq!(core.seq(), 9);

    let ops = vec![PatchOp::Set {
        path: vec![json!("a")],
        value: json!(1),
    }];
    let events = recv(
        &mut core,
        &Frame::Patch {
            seq: 10,
            ops: ops.clone(),
        },
        Encoding::MsgPack,
    );
    assert_eq!(events, vec![ClientEvent::Patched { ops }]);
    assert_eq!(core.state(), &json!({"a": 1}));
}

#[test]
fn intents_batch_into_one_message() {
    let mut core = ClientCore::new(config(Encoding::MsgPack));
    assert_eq!(core.queue_intent(json!({"Add": 1})), 1);
    assert_eq!(core.queue_intent(json!({"Add": 2})), 2);
    assert_eq!(core.queue_intent(json!({"Add": 3})), 3);

    let wire = core.poll_send(0).expect("the first send is immediate");
    let Frame::Intent { intents } = decode(&wire).expect("INTENT decodes") else {
        panic!("expected an INTENT frame");
    };
    assert_eq!(
        intents,
        vec![
            IntentMsg {
                seq: 1,
                intent: json!({"Add": 1}),
            },
            IntentMsg {
                seq: 2,
                intent: json!({"Add": 2}),
            },
            IntentMsg {
                seq: 3,
                intent: json!({"Add": 3}),
            },
        ]
    );
    assert!(core.poll_send(0).is_none(), "the queue is drained");
}

#[test]
fn batch_capped_at_max_intents_per_msg() {
    // Default config: 32 intents per message, 20 messages per second (50 ms).
    let mut core = ClientCore::new(config(Encoding::MsgPack));
    for i in 0..40u32 {
        assert_eq!(core.queue_intent(json!(i)), i + 1);
    }

    let wire = core.poll_send(0).expect("first message");
    let Frame::Intent { intents } = decode(&wire).expect("INTENT decodes") else {
        panic!("expected an INTENT frame");
    };
    assert_eq!(intents.len(), 32);
    assert_eq!(
        intents.iter().map(|msg| msg.seq).collect::<Vec<_>>(),
        (1..=32).collect::<Vec<_>>()
    );

    let wire = core.poll_send(50).expect("second message");
    let Frame::Intent { intents } = decode(&wire).expect("INTENT decodes") else {
        panic!("expected an INTENT frame");
    };
    assert_eq!(
        intents.iter().map(|msg| msg.seq).collect::<Vec<_>>(),
        (33..=40).collect::<Vec<_>>()
    );
    assert!(core.poll_send(100).is_none(), "nothing is left to send");
}

#[test]
fn send_rate_cap() {
    let mut core = ClientCore::new(config(Encoding::MsgPack)); // 20/s → 50 ms
    core.queue_intent(json!(1));
    assert!(
        core.poll_send(1_000).is_some(),
        "the first send is immediate"
    );

    core.queue_intent(json!(2));
    assert!(
        core.poll_send(1_010).is_none(),
        "10 ms after the last send is too soon"
    );
    assert!(core.poll_send(1_049).is_none());
    assert!(
        core.poll_send(1_050).is_some(),
        "50 ms after the last send is allowed"
    );
}

#[test]
fn poll_send_none_when_queue_empty() {
    let mut core = ClientCore::new(config(Encoding::Json));
    assert!(core.poll_send(0).is_none());
    core.queue_intent(json!(1));
    assert!(core.poll_send(0).is_some());
    assert!(core.poll_send(10_000).is_none());
}

#[test]
fn json_and_msgpack_configs_both_work() {
    exercise(Encoding::Json);
    exercise(Encoding::MsgPack);
}

fn exercise(encoding: Encoding) {
    let binary = encoding == Encoding::MsgPack;
    let mut core = ClientCore::new(config(encoding));

    // HELLO carries the hash and protocol 1 in the configured encoding.
    let hello = core.hello(json!({"device": "test"}));
    assert_eq!(matches!(&hello, WireMsg::Binary(_)), binary);
    let Frame::Hello {
        protocol,
        schema_hash,
        opts,
    } = decode(&hello).expect("HELLO decodes")
    else {
        panic!("expected a HELLO frame");
    };
    assert_eq!((protocol, schema_hash.as_str()), (1, HASH));
    assert_eq!(opts, json!({"device": "test"}));

    // JOIN_OK stores the player.
    let player = PlayerId("p1".into());
    let events = recv(
        &mut core,
        &Frame::JoinOk {
            player_id: player.clone(),
            seat: 2,
        },
        encoding,
    );
    assert_eq!(
        events,
        vec![ClientEvent::Joined {
            player_id: player.clone(),
            seat: 2,
        }]
    );
    assert_eq!(core.player_id(), Some(&player));

    // FULL_STATE then PATCH.
    let events = recv(
        &mut core,
        &Frame::FullState {
            seq: 1,
            state: json!({"count": 0}),
        },
        encoding,
    );
    assert_eq!(events, vec![ClientEvent::StateReset]);
    let ops = vec![PatchOp::Set {
        path: vec![json!("count")],
        value: json!(3),
    }];
    let events = recv(
        &mut core,
        &Frame::Patch {
            seq: 2,
            ops: ops.clone(),
        },
        encoding,
    );
    assert_eq!(events, vec![ClientEvent::Patched { ops }]);
    assert_eq!(core.seq(), 2);
    assert_eq!(core.state(), &json!({"count": 3}));

    // ACK, EVENT and ERROR.
    let events = recv(
        &mut core,
        &Frame::Ack {
            state_seq: 2,
            intent_seqs: vec![1],
        },
        encoding,
    );
    assert_eq!(
        events,
        vec![ClientEvent::Acked {
            state_seq: 2,
            intent_seqs: vec![1],
        }]
    );
    let events = recv(
        &mut core,
        &Frame::Event {
            name: "shout".into(),
            data: json!({"text": "hi"}),
        },
        encoding,
    );
    assert_eq!(
        events,
        vec![ClientEvent::Event {
            name: "shout".into(),
            data: json!({"text": "hi"}),
        }]
    );
    let events = recv(
        &mut core,
        &Frame::Error {
            code: ErrorCode::IntentRejected,
            message: "nope".into(),
            ref_seq: Some(7),
        },
        encoding,
    );
    assert_eq!(
        events,
        vec![ClientEvent::Error {
            code: ErrorCode::IntentRejected,
            message: "nope".into(),
            ref_seq: Some(7),
        }]
    );

    // LEAVE and INTENT use the configured encoding.
    let leave = core.leave();
    assert_eq!(matches!(&leave, WireMsg::Binary(_)), binary);
    assert_eq!(decode(&leave).expect("LEAVE decodes"), Frame::Leave {});
    assert_eq!(core.queue_intent(json!({"Move": 0})), 1);
    let sent = core.poll_send(0).expect("the first send is immediate");
    assert_eq!(matches!(&sent, WireMsg::Binary(_)), binary);
    assert_eq!(
        decode(&sent).expect("INTENT decodes"),
        Frame::Intent {
            intents: vec![IntentMsg {
                seq: 1,
                intent: json!({"Move": 0}),
            }],
        }
    );
}

#[test]
fn server_frames_the_client_does_not_expect_are_ignored() {
    let mut core = ClientCore::new(config(Encoding::Json));
    let ignored = [
        Frame::Hello {
            protocol: 1,
            schema_hash: HASH.into(),
            opts: json!({}),
        },
        Frame::Intent {
            intents: vec![IntentMsg {
                seq: 1,
                intent: json!({}),
            }],
        },
        Frame::Leave {},
    ];
    for frame in ignored {
        assert!(
            recv(&mut core, &frame, Encoding::Json).is_empty(),
            "{frame:?} is ignored"
        );
    }
    // Keepalive text frames are not protocol frames.
    assert!(core
        .handle(&WireMsg::Text(PING_TEXT.into()))
        .expect("ping ignored")
        .is_empty());
    assert!(core
        .handle(&WireMsg::Text(PONG_TEXT.into()))
        .expect("pong ignored")
        .is_empty());
    assert!(core.player_id().is_none());
    assert_eq!(core.seq(), 0);
    assert_eq!(core.state(), &Value::Null);
}

#[test]
fn garbage_frame_is_a_protocol_error() {
    let mut core = ClientCore::new(config(Encoding::Json));
    assert!(matches!(
        core.handle(&WireMsg::Text("nope".into())),
        Err(ProtocolError::Malformed(_))
    ));
}

fn join_ok(core: &mut ClientCore, seat: u8, encoding: Encoding) -> Vec<ClientEvent> {
    recv(
        core,
        &Frame::JoinOk {
            player_id: PlayerId("p1".into()),
            seat,
        },
        encoding,
    )
}

#[test]
fn unacked_intents_reported_on_disconnect() {
    let mut core = ClientCore::new(config(Encoding::MsgPack));
    core.queue_intent(json!({"Add": 1}));
    core.queue_intent(json!({"Fail": 1}));
    core.queue_intent(json!({"Add": 2}));
    assert!(
        core.poll_send(0).is_some(),
        "all three intents are in flight"
    );

    // Seq 1 is ACKed and seq 2 is rejected; only seq 3 is unaccounted for.
    recv(
        &mut core,
        &Frame::Ack {
            state_seq: 1,
            intent_seqs: vec![1],
        },
        Encoding::MsgPack,
    );
    recv(
        &mut core,
        &Frame::Error {
            code: ErrorCode::IntentRejected,
            message: "nope".into(),
            ref_seq: Some(2),
        },
        Encoding::MsgPack,
    );

    assert_eq!(
        core.on_disconnected(),
        vec![ClientEvent::IntentsUnconfirmed { seqs: vec![3] }]
    );
    assert!(
        core.on_disconnected().is_empty(),
        "each unconfirmed intent is reported once"
    );
}

#[test]
fn queued_intents_survive_and_wait_for_join_ok() {
    let mut core = ClientCore::new(config(Encoding::MsgPack));
    join_ok(&mut core, 0, Encoding::MsgPack);

    // A first-session intent is in flight when the socket drops: it is
    // reported, never resent.
    core.queue_intent(json!({"Add": 1}));
    assert!(core.poll_send(0).is_some());
    assert_eq!(
        core.on_disconnected(),
        vec![ClientEvent::IntentsUnconfirmed { seqs: vec![1] }]
    );

    // Intents queued while disconnected stay queued.
    core.queue_intent(json!({"Add": 2}));
    core.queue_intent(json!({"Add": 3}));
    assert!(
        core.poll_send(1_000).is_none(),
        "queued intents wait for JOIN_OK"
    );

    // The resume joins again; the queue then flushes in one message.
    join_ok(&mut core, 0, Encoding::MsgPack);
    let wire = core.poll_send(1_000).expect("JOIN_OK releases the queue");
    let Frame::Intent { intents } = decode(&wire).expect("INTENT decodes") else {
        panic!("expected an INTENT frame");
    };
    assert_eq!(
        intents.iter().map(|msg| msg.seq).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(core.poll_send(2_000).is_none(), "the queue is drained");
}

#[test]
fn no_event_when_nothing_in_flight() {
    let mut core = ClientCore::new(config(Encoding::Json));
    join_ok(&mut core, 0, Encoding::Json);

    // An ACKed intent is no longer in flight.
    core.queue_intent(json!({"Add": 1}));
    assert!(core.poll_send(0).is_some());
    recv(
        &mut core,
        &Frame::Ack {
            state_seq: 1,
            intent_seqs: vec![1],
        },
        Encoding::Json,
    );
    assert!(core.on_disconnected().is_empty());

    // A queued but never-sent intent is not in flight either.
    core.queue_intent(json!({"Add": 2}));
    assert!(core.on_disconnected().is_empty());
}
