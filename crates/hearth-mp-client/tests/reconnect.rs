//! The fixture is the contract shared with the Godot client: the GDScript
//! `HearthReconnect.decide` (Task 3) must match every case below.

use hearth_mp_client::{Reconnect, ReconnectPolicy};
use serde_json::Value;

#[test]
fn default_policy_matches_every_reconnect_case() {
    let cases = hearth_mp_protocol::vectors::load("reconnect.json");

    let policy = ReconnectPolicy::default();
    for case in cases {
        let name = case["name"].as_str().expect("every case is named");
        let code = case["code"].as_u64().expect("code") as u16;
        let first_drop_ms = case["first_drop_ms"].as_u64().expect("first_drop_ms");
        let attempt = case["attempt"].as_u64().expect("attempt") as u32;
        let now_ms = case["now_ms"].as_u64().expect("now_ms");

        let actual = policy.decide(code, first_drop_ms, attempt, now_ms);
        match &case["expect"] {
            Value::String(expect) => {
                assert_eq!(expect, "give_up", "case {name}: bad fixture expectation");
                assert_eq!(actual, Reconnect::GiveUp, "case {name}");
            }
            Value::Number(expect) => {
                let expect = expect.as_u64().expect("retry time is an integer");
                assert_eq!(actual, Reconnect::RetryAt(expect), "case {name}");
            }
            other => panic!("case {name}: unexpected expectation {other}"),
        }
    }
}
