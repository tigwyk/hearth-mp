//! Prediction conformance (plan v0.4 Task 8 Step 1).

use std::time::Duration;

use hearth_mp_client::{Logic, Predictor};
use hearth_mp_protocol::PlayerId;
use serde::Deserialize;
use serde_json::json;

fn me() -> PlayerId {
    PlayerId("a".into())
}

/// A toy 1-D logic: each input adds its value to one number.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, serde::Serialize)]
struct ToyState {
    value: i32,
}

#[derive(Clone, Debug, serde::Serialize)]
struct Add {
    add: i32,
}

struct Toy;

impl Logic for Toy {
    type State = ToyState;
    type Input = Add;

    fn step(state: &mut ToyState, inputs: &[(PlayerId, Add)], _dt: Duration) {
        for (_, input) in inputs {
            state.value += input.add;
        }
    }
}

#[test]
fn input_steps_locally_immediately() {
    let mut predictor = Predictor::<Toy>::new(me(), 20);
    predictor.input(1, Add { add: 3 });
    assert_eq!(predictor.predicted().value, 3);
}

#[test]
fn ack_drops_and_replays_remaining() {
    let mut predictor = Predictor::<Toy>::new(me(), 20);
    for seq in 1..=5 {
        predictor.input(seq, Add { add: 1 });
    }
    assert_eq!(predictor.predicted().value, 5);

    // The server applied inputs 1..=3 and reports value 100; inputs 4 and 5 are
    // replayed on top of it.
    predictor.on_authoritative(&json!({ "value": 100 }), &[3]);
    assert_eq!(predictor.predicted().value, 102);
}

#[test]
fn unconfirmed_dropped() {
    let mut predictor = Predictor::<Toy>::new(me(), 20);
    predictor.input(1, Add { add: 1 });
    predictor.input(2, Add { add: 1 });
    assert_eq!(predictor.predicted().value, 2);

    // Seq 1 was never applied by the server: it must not come back.
    predictor.on_unconfirmed(&[1]);
    predictor.on_authoritative(&json!({ "value": 0 }), &[]);
    assert_eq!(predictor.predicted().value, 1);
}

#[test]
fn matches_server_when_no_interference() {
    use arena_logic::{step, Arena, ArenaState, Avatar, Move};

    let dt = 1.0 / 20.0_f32;
    let player = me();
    let mut server = ArenaState::default();
    server
        .avatars
        .insert("a".into(), Avatar { pos: [5.0, 5.0] });
    let mut predictor = Predictor::<Arena>::new(player.clone(), 20);

    for tick in 1..=100u32 {
        let dir = match tick % 7 {
            0..=2 => [1, 0],
            3 | 4 => [-1, 1],
            _ => [0, -1],
        };
        let movement = Move { dir };
        predictor.input(tick, movement);
        // The room's on_tick is one call to the shared `step`.
        step(&mut server, &[(player.clone(), movement)], dt);
        let server_json = serde_json::to_value(&server).expect("state serialises");
        predictor.on_authoritative(&server_json, &[tick]);
        assert_eq!(predictor.predicted(), &server, "tick {tick}");
    }
}

#[test]
fn diverges_then_reconciles() {
    let mut predictor = Predictor::<Toy>::new(me(), 20);
    predictor.input(1, Add { add: 1 });
    predictor.input(2, Add { add: 2 });
    predictor.input(3, Add { add: 3 });

    // The server applied 1 and 2, but a contact push also changed the state:
    // after the correction the prediction is the server state plus input 3.
    predictor.on_authoritative(&json!({ "value": 50 }), &[2]);
    assert_eq!(predictor.predicted().value, 53);
}

// ---- v0.4b: the object-safe predictor the Unity plugin uses ---------------

#[test]
fn dyn_predictor_matches_generic() {
    use hearth_mp_client::{boxed, Echo1d};
    let mut generic = Predictor::<Echo1d>::new(me(), 20);
    let mut dynamic = boxed::<Echo1d>(me(), 20);
    for (seq, input) in [(1, 1), (2, 5), (3, -1)] {
        generic.input(seq, input);
        dynamic.input(seq, &json!(input)).unwrap();
    }
    generic.on_authoritative(&json!({"x": 10}), &[1]);
    dynamic.on_authoritative(&json!({"x": 10}), &[1]).unwrap();
    assert_eq!(
        serde_json::to_value(generic.predicted()).unwrap(),
        dynamic.predicted().unwrap()
    );
    assert_eq!(dynamic.predicted().unwrap(), json!({"x": 10}));
}

#[test]
fn bad_state_shape_is_err() {
    let mut dynamic = hearth_mp_client::boxed::<hearth_mp_client::Echo1d>(me(), 20);
    let error = dynamic.on_authoritative(&json!({"y": 1}), &[]).unwrap_err();
    assert!(error.contains("does not match"), "{error}");
}

#[test]
fn bad_input_is_err() {
    let mut dynamic = hearth_mp_client::boxed::<hearth_mp_client::Echo1d>(me(), 20);
    assert!(dynamic.input(1, &json!("left")).is_err());
}
