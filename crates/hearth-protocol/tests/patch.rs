use hearth_protocol::{apply, PatchError, PatchOp, Value};
use serde_json::json;

/// Shared conformance suite (`conformance/vectors/patch.json`): the same vectors
/// drive every SDK's patch implementation.
fn cases() -> Vec<Value> {
    hearth_protocol::vectors::load("patch.json")
}

#[test]
fn patch_fixture_conformance() {
    let cases = cases();
    assert!(cases.len() >= 10, "fixture should be meaningful");

    for case in cases {
        let name = case["name"].as_str().expect("case has a name");
        let mut state = case["before"].clone();
        let ops: Vec<PatchOp> = serde_json::from_value(case["ops"].clone())
            .unwrap_or_else(|err| panic!("{name}: bad fixture ops: {err}"));

        let mut result = Ok(());
        for op in &ops {
            result = apply(&mut state, op);
            if result.is_err() {
                break;
            }
        }

        if case.get("error").and_then(Value::as_bool).unwrap_or(false) {
            assert!(
                matches!(result, Err(PatchError::BadPath(_))),
                "{name}: expected BadPath"
            );
        } else {
            result.unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(state, case["after"].clone(), "{name}");
        }
    }
}

#[test]
fn ops_apply_in_order() {
    let mut state = json!({"n": 0});
    let ops = vec![
        PatchOp::Set {
            path: vec![json!("n")],
            value: json!(1),
        },
        PatchOp::Set {
            path: vec![json!("n")],
            value: json!(2),
        },
    ];
    for op in &ops {
        apply(&mut state, op).unwrap();
    }
    assert_eq!(state, json!({"n": 2}));
}

#[test]
fn set_root_then_del_nested() {
    let mut state = json!(null);
    apply(
        &mut state,
        &PatchOp::Set {
            path: vec![],
            value: json!({"a": {"b": 1}}),
        },
    )
    .unwrap();
    apply(
        &mut state,
        &PatchOp::Del {
            path: vec![json!("a"), json!("b")],
        },
    )
    .unwrap();
    assert_eq!(state, json!({"a": {}}));
}
