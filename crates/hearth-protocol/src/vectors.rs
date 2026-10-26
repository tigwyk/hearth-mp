//! Loader for the conformance vectors in `conformance/vectors/` (feature
//! `vectors`, for tests). A wrong path must never turn a conformance test into
//! a silent pass, so a missing file, invalid JSON or zero cases all panic with
//! the resolved path.

use crate::Value;

/// Every case in `conformance/vectors/<name>`.
///
/// # Panics
/// When the file is missing, is not a JSON array, or holds no cases.
pub fn load(name: &str) -> Vec<Value> {
    let path = format!("{}/../../conformance/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    let cases: Vec<Value> =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path} is not a JSON array: {e}"));
    assert!(!cases.is_empty(), "{path} has no cases");
    cases
}
