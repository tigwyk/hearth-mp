//! Native tests for the engine-free parts of the JSON <-> `Variant` bridge.
//!
//! `godot::builtin` types can only be constructed inside a running Godot engine,
//! so these tests drive the same recursive [`build`] logic through a plain Rust
//! tree (`Shape`) instead of `Variant`. The production `Variant` implementation
//! is argument marshalling only.

use std::collections::BTreeMap;

use godot::builtin::VariantType;
use hearth_godot::convert::{build, key_is_string, BuildVariant};
use serde_json::json;

#[derive(Clone, Debug, PartialEq)]
enum Shape {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Shape>),
    Object(BTreeMap<String, Shape>),
}

impl BuildVariant for Shape {
    fn nil() -> Self {
        Shape::Nil
    }

    fn bool(value: bool) -> Self {
        Shape::Bool(value)
    }

    fn int(value: i64) -> Self {
        Shape::Int(value)
    }

    fn float(value: f64) -> Self {
        Shape::Float(value)
    }

    fn string(value: String) -> Self {
        Shape::Str(value)
    }

    fn array(values: Vec<Self>) -> Self {
        Shape::Array(values)
    }

    fn object(entries: Vec<(String, Self)>) -> Self {
        Shape::Object(entries.into_iter().collect())
    }
}

fn object(entries: &[(&str, Shape)]) -> Shape {
    Shape::Object(
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    )
}

#[test]
fn int_stays_int() {
    assert_eq!(build::<Shape>(&json!(42)), Shape::Int(42));
    assert_eq!(build::<Shape>(&json!(-7)), Shape::Int(-7));
    assert_eq!(build::<Shape>(&json!(0)), Shape::Int(0));
    // Beyond 2^53 but inside i64: still a Variant int, no float round-trip.
    assert_eq!(
        build::<Shape>(&json!(9_007_199_254_740_993_i64)),
        Shape::Int(9_007_199_254_740_993)
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>("9007199254740993")
            .map(|value| build::<Shape>(&value))
            .unwrap(),
        Shape::Int(9_007_199_254_740_993)
    );
}

#[test]
fn float_stays_float() {
    assert_eq!(build::<Shape>(&json!(2.5)), Shape::Float(2.5));
    // A JSON number written with a decimal point is a float even when integral.
    let three_point_zero: serde_json::Value = serde_json::from_str("3.0").unwrap();
    assert_eq!(build::<Shape>(&three_point_zero), Shape::Float(3.0));
    assert_eq!(build::<Shape>(&json!(1e300)), Shape::Float(1e300));
}

#[test]
fn nested_map_and_array() {
    let value = json!({
        "players": {"a": 1, "b": 2},
        "rows": [[0], [1.5, {"mark": 2}]],
        "turn": "a",
        "ok": true,
        "none": null,
    });
    assert_eq!(
        build::<Shape>(&value),
        object(&[
            ("none", Shape::Nil),
            ("ok", Shape::Bool(true)),
            (
                "players",
                object(&[("a", Shape::Int(1)), ("b", Shape::Int(2))])
            ),
            (
                "rows",
                Shape::Array(vec![
                    Shape::Array(vec![Shape::Int(0)]),
                    Shape::Array(vec![Shape::Float(1.5), object(&[("mark", Shape::Int(2))]),]),
                ])
            ),
            ("turn", Shape::Str("a".to_owned())),
        ])
    );
}

#[test]
fn non_string_key_rejected() {
    assert!(key_is_string(VariantType::STRING));
    assert!(key_is_string(VariantType::STRING_NAME));
    assert!(!key_is_string(VariantType::INT));
    assert!(!key_is_string(VariantType::FLOAT));
    assert!(!key_is_string(VariantType::BOOL));
    assert!(!key_is_string(VariantType::NIL));
    assert!(!key_is_string(VariantType::ARRAY));
    assert!(!key_is_string(VariantType::DICTIONARY));
}
