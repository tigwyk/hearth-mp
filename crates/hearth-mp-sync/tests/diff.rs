use hearth_mp_protocol::{apply, PatchOp, Value};
use hearth_mp_sync::diff;
use proptest::prelude::*;
use serde_json::json;

#[test]
fn equal_trees_produce_no_ops() {
    assert!(diff(&json!({"a":[1,2]}), &json!({"a":[1,2]})).is_empty());
}

#[test]
fn changed_scalar_is_set_at_path() {
    assert_eq!(
        diff(&json!({"turn":"a"}), &json!({"turn":"b"})),
        vec![PatchOp::Set {
            path: vec![json!("turn")],
            value: json!("b"),
        }]
    );
}

#[test]
fn new_key_is_map_insert_removed_key_is_del() {
    assert_eq!(
        diff(&json!({"p":{"x":1}}), &json!({"p":{"y":2}})),
        vec![
            PatchOp::Del {
                path: vec![json!("p"), json!("x")],
            },
            PatchOp::MapInsert {
                path: vec![json!("p")],
                key: "y".into(),
                value: json!(2),
            },
        ]
    );
}

#[test]
fn list_change_is_one_minimal_splice() {
    assert_eq!(
        diff(&json!([0, 1, 2, 3]), &json!([0, 9, 3])),
        vec![PatchOp::ListSplice {
            path: vec![],
            index: 1,
            delete: 2,
            insert: vec![json!(9)],
        }]
    );
}

#[test]
fn type_change_is_set() {
    assert_eq!(
        diff(&json!({"a":1}), &json!({"a":[1]})),
        vec![PatchOp::Set {
            path: vec![json!("a")],
            value: json!([1]),
        }]
    );
}

/// Arbitrary JSON with bounded depth, used to check `apply(diff(old,new)) == new`.
fn json_tree() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        "[a-z]{0,4}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            prop::collection::btree_map("[a-z]{0,3}", inner, 0..5)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
}

proptest! {
    #[test]
    fn apply_of_diff_reproduces_new(old in json_tree(), new in json_tree()) {
        let mut state = old.clone();
        for op in diff(&old, &new) {
            apply(&mut state, &op).unwrap();
        }
        prop_assert_eq!(state, new);
    }
}
