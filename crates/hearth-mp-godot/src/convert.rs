//! JSON <-> Godot `Variant` conversion for the state tree.
//!
//! Godot builtins can only be constructed inside a running engine, so a native
//! unit test cannot create a `Variant`. The tree walk is therefore generic over
//! [`BuildVariant`]: production uses `Variant`, `tests/convert.rs` uses a plain
//! Rust tree. The other direction validates dictionary keys with the pure
//! [`key_is_string`] helper.

use godot::builtin::{GString, StringName, VarArray, VarDictionary, Variant, VariantType};
use godot::meta::ToGodot;
use serde_json::{Number, Value};

/// How one JSON number maps to a Godot value: an exact `i64` becomes a Variant
/// `int`, everything else (including `3.0`) becomes a Variant `float`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumberRepr {
    /// An exact integer.
    Int(i64),
    /// Any other number.
    Float(f64),
}

/// Decide the Variant representation of a JSON number. Integers stay integers
/// so ids and counts do not silently become floats.
pub fn number_repr(number: &Number) -> NumberRepr {
    match number.as_i64() {
        Some(int) => NumberRepr::Int(int),
        None => NumberRepr::Float(number.as_f64().unwrap_or(0.0)),
    }
}

/// A GDScript float as JSON. A whole number within ±2^53 goes out as an integer:
/// GDScript readily turns ints into floats (the web client's JSON parser makes
/// every number one), and the server's integer fields reject `3.0`. Float
/// fields accept `3` just as well.
pub fn float_to_json(float: f64) -> Value {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    if float.fract() == 0.0 && float.abs() <= EXACT {
        Value::from(float as i64)
    } else {
        Value::from(float)
    }
}

/// The protocol requires dictionary keys to be strings. GDScript may use either
/// `String` or `StringName`; any other key type cannot be represented as JSON.
pub fn key_is_string(ty: VariantType) -> bool {
    ty == VariantType::STRING || ty == VariantType::STRING_NAME
}

/// Target of the JSON -> variant tree walk. Implemented for `Variant` in
/// production and for a plain Rust tree in tests, so the recursion is testable
/// without an engine.
pub trait BuildVariant: Sized {
    /// `null`.
    fn nil() -> Self;
    /// A boolean.
    fn bool(value: bool) -> Self;
    /// An integer.
    fn int(value: i64) -> Self;
    /// A float.
    fn float(value: f64) -> Self;
    /// A string.
    fn string(value: String) -> Self;
    /// An array.
    fn array(values: Vec<Self>) -> Self;
    /// An object, in key order.
    fn object(entries: Vec<(String, Self)>) -> Self;
}

/// Recursively convert a JSON value into `B`. This is the whole conversion
/// logic; the `Variant` impl is only argument marshalling.
pub fn build<B: BuildVariant>(value: &Value) -> B {
    match value {
        Value::Null => B::nil(),
        Value::Bool(boolean) => B::bool(*boolean),
        Value::Number(number) => match number_repr(number) {
            NumberRepr::Int(int) => B::int(int),
            NumberRepr::Float(float) => B::float(float),
        },
        Value::String(string) => B::string(string.clone()),
        Value::Array(items) => B::array(items.iter().map(build).collect()),
        Value::Object(map) => B::object(
            map.iter()
                .map(|(key, val)| (key.clone(), build(val)))
                .collect(),
        ),
    }
}

impl BuildVariant for Variant {
    fn nil() -> Self {
        Variant::nil()
    }

    fn bool(value: bool) -> Self {
        Variant::from(value)
    }

    fn int(value: i64) -> Self {
        Variant::from(value)
    }

    fn float(value: f64) -> Self {
        Variant::from(value)
    }

    fn string(value: String) -> Self {
        Variant::from(GString::from(value.as_str()))
    }

    fn array(values: Vec<Self>) -> Self {
        let mut array = VarArray::new();
        for value in values {
            array.push(&value);
        }
        array.to_variant()
    }

    fn object(entries: Vec<(String, Self)>) -> Self {
        let mut dictionary = VarDictionary::new();
        for (key, value) in entries {
            dictionary.set(key.as_str(), &value);
        }
        dictionary.to_variant()
    }
}

/// Convert a JSON value (state tree or intent payload) into a Godot `Variant`.
pub fn json_to_variant(value: &Value) -> Variant {
    build(value)
}

/// Convert a Godot `Variant` into JSON. Errors on dictionary keys that are not
/// `String`/`StringName` and on types the wire protocol cannot carry.
pub fn variant_to_json(value: &Variant) -> Result<Value, String> {
    match value.get_type() {
        VariantType::NIL => Ok(Value::Null),
        VariantType::BOOL => Ok(Value::Bool(value.to::<bool>())),
        VariantType::INT => Ok(Value::from(value.to::<i64>())),
        VariantType::FLOAT => Ok(float_to_json(value.to::<f64>())),
        VariantType::STRING => Ok(Value::String(value.to::<GString>().to_string())),
        VariantType::STRING_NAME => Ok(Value::String(value.to::<StringName>().to_string())),
        VariantType::ARRAY => {
            let array = value.to::<VarArray>();
            let mut items = Vec::with_capacity(array.len());
            for index in 0..array.len() {
                items.push(variant_to_json(&array.at(index))?);
            }
            Ok(Value::Array(items))
        }
        VariantType::DICTIONARY => {
            let dictionary = value.to::<VarDictionary>();
            let mut map = serde_json::Map::new();
            for (key, item) in dictionary.iter_shared() {
                let ty = key.get_type();
                if !key_is_string(ty) {
                    return Err(format!(
                        "dictionary key must be String or StringName, got {ty:?}"
                    ));
                }
                let name = if ty == VariantType::STRING {
                    key.to::<GString>().to_string()
                } else {
                    key.to::<StringName>().to_string()
                };
                map.insert(name, variant_to_json(&item)?);
            }
            Ok(Value::Object(map))
        }
        other => Err(format!("unsupported Variant type in state tree: {other:?}")),
    }
}
