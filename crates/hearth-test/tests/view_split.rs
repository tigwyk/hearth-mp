//! Plan v0.4 Task 2: the runtime diffs the public view once per pass and only
//! the owner-scoped fields per client. The bytes each client receives must be
//! exactly the diff of its full view.

mod support;

use hearth_client::ClientEvent;
use hearth_protocol::PatchOp;
use hearth_sync::diff;
use hearth_test::{TestRoom, C};
use serde_json::{json, Value};
use support::CounterRoom;

fn new_room() -> TestRoom<CounterRoom> {
    TestRoom::new()
}

/// Every patch op one client received since `from`.
fn received_ops(room: &TestRoom<CounterRoom>, c: C, from: usize) -> Vec<PatchOp> {
    room.client(c).events[from..]
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Patched { ops } => Some(ops.clone()),
            _ => None,
        })
        .flatten()
        .collect()
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
fn patches_identical_to_full_view_diff() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let b = r.join("b", json!({}));
    let clients = [a, b];
    let mut rng = fastrand::Rng::with_seed(0x5eed_5eed);
    let mut starts = [r.client(a).events.len(), r.client(b).events.len()];

    for step in 0..120 {
        let actor = if rng.bool() { a } else { b };
        let intent: Value = match rng.u8(0..5) {
            0 => json!({"Add": rng.i64(-3..=3)}),
            1 => json!({"Secret": format!("s{}", rng.u8(0..4))}),
            2 => json!("Clients"),
            3 => json!({"Shout": "hi"}),
            _ => json!("Fail"),
        };
        let before = [
            r.client(a).core.state().clone(),
            r.client(b).core.state().clone(),
        ];

        r.intent(actor, intent.clone());

        for (index, c) in clients.iter().enumerate() {
            let after = r.client(*c).core.state().clone();
            let expected = diff(&before[index], &after);
            let received = received_ops(&r, *c, starts[index]);
            assert_eq!(
                received, expected,
                "step {step} intent {intent}: client {} received ops that differ \
                 from diff(view_before, view_after)",
                c.0
            );
            assert_eq!(
                expected,
                diff(&before[index], &after),
                "the diff is deterministic"
            );
            starts[index] = r.client(*c).events.len();
        }
    }
}
