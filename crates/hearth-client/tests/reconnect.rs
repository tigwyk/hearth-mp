//! The fixture is the contract shared with the Godot client: the GDScript
//! `HearthReconnect.decide` (Task 3) must match every case below.

use hearth_client::{Reconnect, ReconnectPolicy};
use serde_json::Value;

#[test]
fn default_policy_matches_every_reconnect_case() {
    let raw = include_str!("fixtures/reconnect_cases.json");
    let cases: Value = serde_json::from_str(raw).expect("the fixture is JSON");
    let cases = cases.as_array().expect("the fixture is an array");
    assert!(!cases.is_empty(), "the fixture has cases");

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
