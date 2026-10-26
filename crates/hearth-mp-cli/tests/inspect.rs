//! Checks for `hearth-mp inspect` that need no server: the pure helpers, plus the
//! missing-token exit path of the built binary.

use std::fs;
use std::io::Cursor;
use std::process::Command;

use hearth_mp_cli::inspect::{confirm, parse_room, summarize};
use serde_json::json;

#[test]
fn parse_room_ok() {
    let (room_type, name) = parse_room("tictactoe/abc").unwrap();
    assert_eq!((room_type.as_str(), name.as_str()), ("tictactoe", "abc"));
}

#[test]
fn parse_room_rejects() {
    for arg in ["a", "a/", "/b", "a/b/c"] {
        assert!(parse_room(arg).is_err(), "accepted {arg:?}");
    }
}

#[test]
fn summarize_intent_cause() {
    let msg = json!({
        "seq": 4,
        "cause": [{"cause": "intent", "player": "a", "intent_seq": 1}],
        "ops": [{"op": "set"}, {"op": "set"}, {"op": "delete"}],
        "rejected": [],
    });
    assert_eq!(summarize(&msg), "#4 intent a#1 3 ops");
}

#[test]
fn summarize_multiple_causes() {
    let msg = json!({
        "seq": 5,
        "cause": [
            {"cause": "intent", "player": "a", "intent_seq": 1},
            {"cause": "timer", "key": "u:t"},
        ],
        "ops": [{"op": "set"}, {"op": "set"}],
    });
    assert_eq!(summarize(&msg), "#5 intent a#1, timer u:t 2 ops");
}

#[test]
fn summarize_dispose_cause() {
    let msg = json!({"seq": 6, "cause": [{"cause": "dispose"}], "ops": [{"op": "set"}]});
    assert_eq!(summarize(&msg), "#6 dispose 1 ops");
}

#[test]
fn dispose_prompt_requires_yes() {
    assert!(!confirm(Cursor::new("n\n")), "n accepted");
    assert!(!confirm(Cursor::new("\n")), "empty accepted");
    assert!(
        !confirm(Cursor::new("yes please\n")),
        "trailing text accepted"
    );
    assert!(confirm(Cursor::new("y\n")), "y rejected");
    assert!(confirm(Cursor::new("YES\n")), "YES rejected");
    assert!(confirm(Cursor::new(" yes \n")), "padded yes rejected");
}

#[test]
fn missing_token_exit_2() {
    // No `.dev.vars` in the temp dir and no token in the environment: the admin
    // API is unreachable, so the CLI must say how to set a token and exit 2.
    let dir = std::env::temp_dir().join(format!("hearth-inspect-no-token-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    assert!(!dir.join(".dev.vars").exists(), "temp dir has .dev.vars");

    let output = Command::new(env!("CARGO_BIN_EXE_hearth-mp"))
        .args(["inspect", "rooms"])
        .current_dir(&dir)
        .env_remove("HEARTH_ADMIN_TOKEN")
        .env_remove("HEARTH_URL")
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HEARTH_ADMIN_TOKEN"),
        "stderr did not mention the token: {stderr}"
    );
}
