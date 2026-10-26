//! `spec/protocol-v1.md` and the code must agree: the opcode, error-code and
//! close-code tables are parsed out of the spec and compared with the Rust
//! definitions.

use std::collections::BTreeMap;
use std::path::Path;

use hearth_protocol::{close, ErrorCode, Opcode};

fn spec() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/protocol-v1.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The cells of every data row of the first table under `## <heading>`.
fn table(spec: &str, heading: &str) -> Vec<Vec<String>> {
    let start = spec
        .find(&format!("## {heading}"))
        .unwrap_or_else(|| panic!("spec has no section `{heading}`"));
    let rest = &spec[start..];
    let end = rest[3..].find("\n## ").map_or(rest.len(), |i| i + 3);
    let rows: Vec<Vec<String>> = rest[..end]
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .skip(2) // header and separator
        .map(|line| {
            line.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().trim_matches('`').to_owned())
                .collect()
        })
        .collect();
    assert!(!rows.is_empty(), "section `{heading}` has no table rows");
    rows
}

#[test]
fn opcode_table_matches_code() {
    let spec: BTreeMap<String, u8> = table(&spec(), "3. Opcode table")
        .into_iter()
        .map(|row| {
            let byte = u8::from_str_radix(row[1].trim_start_matches("0x"), 16)
                .unwrap_or_else(|_| panic!("bad byte `{}`", row[1]));
            (row[0].clone(), byte)
        })
        .collect();
    let code: BTreeMap<String, u8> = (0..=u8::MAX)
        .filter_map(Opcode::from_u8)
        .map(|op| (screaming(&format!("{op:?}")), op.as_u8()))
        .collect();
    assert_eq!(spec, code);
}

#[test]
fn error_code_table_matches_code() {
    let spec: Vec<String> = table(&spec(), "8. Error codes").into_iter().map(|row| row[0].clone()).collect();
    for name in &spec {
        let parsed: ErrorCode = serde_json::from_value(serde_json::json!(name))
            .unwrap_or_else(|e| panic!("spec code `{name}` is not an ErrorCode: {e}"));
        assert_eq!(serde_json::to_value(parsed).unwrap(), serde_json::json!(name));
    }
    // Every known variant, so a new one cannot be added without the spec row.
    let known = [
        ErrorCode::BadFrame,
        ErrorCode::UnsupportedProtocol,
        ErrorCode::SchemaMismatch,
        ErrorCode::RoomFull,
        ErrorCode::RoomLocked,
        ErrorCode::NotJoined,
        ErrorCode::BadIntent,
        ErrorCode::IntentRejected,
        ErrorCode::JoinRejected,
        ErrorCode::RateLimited,
        ErrorCode::FrameTooLarge,
        ErrorCode::StateIncompatible,
        ErrorCode::Internal,
    ];
    let code: Vec<String> = known
        .iter()
        .map(|c| serde_json::to_value(c).unwrap().as_str().unwrap().to_owned())
        .collect();
    assert_eq!(spec, code);
}

#[test]
fn close_code_table_matches_code() {
    let spec: BTreeMap<String, u16> = table(&spec(), "9. Close codes")
        .into_iter()
        .map(|row| (row[1].clone(), row[0].parse().unwrap()))
        .collect();
    let code: BTreeMap<String, u16> = [
        ("BAD_FRAME", close::BAD_FRAME),
        ("SCHEMA_MISMATCH", close::SCHEMA_MISMATCH),
        ("ROOM_FULL", close::ROOM_FULL),
        ("ROOM_LOCKED", close::ROOM_LOCKED),
        ("KICKED", close::KICKED),
        ("RATE_LIMITED", close::RATE_LIMITED),
        ("FRAME_TOO_LARGE", close::FRAME_TOO_LARGE),
        ("STATE_INCOMPATIBLE", close::STATE_INCOMPATIBLE),
        ("DISPOSED", close::DISPOSED),
        ("REPLACED", close::REPLACED),
        ("JOIN_REJECTED", close::JOIN_REJECTED),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect();
    assert_eq!(spec, code);
    assert!(code.values().all(|c| (4000..=4010).contains(c)), "assigned codes stay in 4000-4010");
}

/// `JoinOk` -> `JOIN_OK`.
fn screaming(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}
