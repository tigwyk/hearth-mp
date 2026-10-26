use std::time::Duration;

use hearth_mp_client::ClientEvent;
use hearth_mp_protocol::ErrorCode;
use hearth_mp_test::{TestRoom, C};
use serde_json::{json, Value};
use tictactoe::TicTacToe;

fn new_game() -> (TestRoom<TicTacToe>, C, C) {
    let mut room = TestRoom::<TicTacToe>::new();
    let a = room.join("a", json!({}));
    let b = room.join("b", json!({}));
    (room, a, b)
}

fn place(room: &mut TestRoom<TicTacToe>, client: C, cell: u8) {
    room.intent(client, json!({ "Place": { "cell": cell } }));
}

fn last_error(events: &[ClientEvent]) -> Option<(ErrorCode, Option<u32>)> {
    events.iter().rev().find_map(|event| match event {
        ClientEvent::Error { code, ref_seq, .. } => Some((code.clone(), *ref_seq)),
        _ => None,
    })
}

fn status(room: &TestRoom<TicTacToe>, client: C) -> Value {
    room.client(client).core.state()["status"].clone()
}

fn cells(room: &TestRoom<TicTacToe>, client: C) -> Value {
    room.client(client).core.state()["cells"].clone()
}

#[test]
fn x_wins_top_row() {
    let (mut room, a, b) = new_game();
    place(&mut room, a, 0);
    place(&mut room, b, 3);
    place(&mut room, a, 1);
    place(&mut room, b, 4);
    place(&mut room, a, 2);

    let expected = json!({ "Won": "a" });
    assert_eq!(status(&room, a), expected);
    assert_eq!(status(&room, b), expected);
}

#[test]
fn out_of_turn_rejected_no_patch() {
    let (mut room, a, b) = new_game();
    let patches_before = room
        .client(a)
        .events
        .iter()
        .filter(|event| matches!(event, ClientEvent::Patched { .. }))
        .count();

    // X joins first, so O cannot move first.
    place(&mut room, b, 0);

    let (code, ref_seq) = last_error(&room.client(b).events).expect("an error was sent");
    assert_eq!(code, ErrorCode::IntentRejected);
    assert_eq!(ref_seq, Some(1));
    let patches_after = room
        .client(a)
        .events
        .iter()
        .filter(|event| matches!(event, ClientEvent::Patched { .. }))
        .count();
    assert_eq!(
        patches_before, patches_after,
        "a failed intent must not patch"
    );
    assert_eq!(cells(&room, a), json!([0, 0, 0, 0, 0, 0, 0, 0, 0]));
}

#[test]
fn occupied_cell_rejected() {
    let (mut room, a, b) = new_game();
    place(&mut room, a, 4);
    place(&mut room, b, 4);

    let (code, _) = last_error(&room.client(b).events).expect("an error was sent");
    assert_eq!(code, ErrorCode::IntentRejected);
    assert_eq!(cells(&room, a), json!([0, 0, 0, 0, 1, 0, 0, 0, 0]));
}

#[test]
fn cell_out_of_range_rejected() {
    let (mut room, a, _b) = new_game();
    place(&mut room, a, 9);

    let (code, _) = last_error(&room.client(a).events).expect("an error was sent");
    assert_eq!(code, ErrorCode::IntentRejected);
    assert_eq!(cells(&room, a), json!([0, 0, 0, 0, 0, 0, 0, 0, 0]));
}

#[test]
fn third_player_room_full() {
    let (mut room, _a, _b) = new_game();
    let c = room.join("c", json!({}));
    let closed = room.client(c).closed.clone();
    assert_eq!(closed.map(|(code, _)| code), Some(4002));
}

#[test]
fn draw_detected() {
    let (mut room, a, b) = new_game();
    // X: 0 2 3 7 8, O: 1 4 5 6 -> full board, no line.
    for (client, cell) in [
        (a, 0),
        (b, 1),
        (a, 2),
        (b, 4),
        (a, 3),
        (b, 5),
        (a, 7),
        (b, 6),
        (a, 8),
    ] {
        place(&mut room, client, cell);
    }
    assert_eq!(status(&room, a), json!("Draw"));
    assert_eq!(status(&room, b), json!("Draw"));
}

#[test]
fn leave_mid_game_resets() {
    let (mut room, a, b) = new_game();
    place(&mut room, a, 0);
    place(&mut room, b, 1);

    room.disconnect(b);
    // The seat is held for the default 30 s reconnect grace before the room
    // treats b as gone and resets to Waiting.
    room.advance(Duration::from_secs(30));

    assert_eq!(status(&room, a), json!("Waiting"));
    assert_eq!(cells(&room, a), json!([0, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(room.client(a).core.state()["marks"], json!({ "a": 1 }));
}

#[test]
fn rematch_clears_board() {
    let (mut room, a, b) = new_game();
    for (client, cell) in [(a, 0), (b, 3), (a, 1), (b, 4), (a, 2)] {
        place(&mut room, client, cell);
    }
    assert_eq!(status(&room, a), json!({ "Won": "a" }));

    room.intent(a, json!("Rematch"));

    assert_eq!(status(&room, a), json!("Playing"));
    assert_eq!(status(&room, b), json!("Playing"));
    assert_eq!(cells(&room, a), json!([0, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(room.client(a).core.state()["turn"], json!("a"));
}
