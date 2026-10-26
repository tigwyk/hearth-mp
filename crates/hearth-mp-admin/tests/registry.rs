//! `Registry` logic against the in-memory SQLite behind `MemHost`.

use hearth_mp_admin::{Registry, RoomRow};
use hearth_mp_server::{PlayerId, RoomName, RoomReport};
use hearth_mp_test::MemHost;

fn room(room_type: &str, name: &str) -> RoomName {
    RoomName {
        room_type: room_type.to_owned(),
        name: name.to_owned(),
    }
}

/// `count` seated players, so report constructors stay short.
fn seated(count: u8) -> Vec<PlayerId> {
    (0..count).map(|n| PlayerId(format!("p{n}"))).collect()
}

fn opened(room: RoomName, created_ms: u64, players: u8) -> RoomReport {
    RoomReport::Open {
        room,
        created_ms,
        players: seated(players),
        capacity: 2,
        locked: false,
        metadata: serde_json::Value::Null,
    }
}

fn players(room: RoomName, players: u8) -> RoomReport {
    RoomReport::Players {
        room,
        players: seated(players),
        capacity: 2,
        locked: false,
        metadata: serde_json::Value::Null,
    }
}

fn disposed(room: RoomName) -> RoomReport {
    RoomReport::Disposed { room }
}

#[test]
fn open_then_players_then_disposed() {
    let host = MemHost::new();
    let registry = Registry::new(host.sql());
    let room = room("tictactoe", "r1");

    registry
        .apply(&opened(room.clone(), 1_000, 1), 2_000)
        .unwrap();
    let (rows, next) = registry.list(None, None, 100).unwrap();
    assert_eq!(next, None);
    assert_eq!(
        rows,
        vec![RoomRow {
            room_type: "tictactoe".into(),
            name: "r1".into(),
            players: 1,
            created_ms: 1_000,
            updated_ms: 2_000,
        }]
    );

    registry.apply(&players(room.clone(), 2), 3_000).unwrap();
    let (rows, _) = registry.list(None, None, 100).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].players, 2);
    // An upsert keeps the original creation time.
    assert_eq!(rows[0].created_ms, 1_000);
    assert_eq!(rows[0].updated_ms, 3_000);

    registry.apply(&disposed(room), 4_000).unwrap();
    assert!(registry.list(None, None, 100).unwrap().0.is_empty());
}

#[test]
fn players_without_open_inserts() {
    let host = MemHost::new();
    let registry = Registry::new(host.sql());

    registry
        .apply(&players(room("tictactoe", "r1"), 3), 5_000)
        .unwrap();
    let (rows, _) = registry.list(None, None, 100).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].players, 3);
    // An out-of-order report has no creation time, so `now` becomes it.
    assert_eq!(rows[0].created_ms, 5_000);
    assert_eq!(rows[0].updated_ms, 5_000);
}

#[test]
fn list_filters_by_type() {
    let host = MemHost::new();
    let registry = Registry::new(host.sql());
    for (room_type, name) in [("a", "x"), ("a", "y"), ("b", "z")] {
        registry
            .apply(&opened(room(room_type, name), 1, 1), 1)
            .unwrap();
    }

    let (rows, _) = registry.list(Some("a"), None, 100).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
        ["x", "y"]
    );
    let (rows, _) = registry.list(Some("b"), None, 100).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
        ["z"]
    );
    let (rows, _) = registry.list(None, None, 100).unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (row.room_type.as_str(), row.name.as_str()))
            .collect::<Vec<_>>(),
        [("a", "x"), ("a", "y"), ("b", "z")]
    );
}

#[test]
fn list_paginates_with_cursor() {
    let host = MemHost::new();
    let registry = Registry::new(host.sql());
    for index in 0..250u32 {
        let room = room("game", &format!("room-{index:03}"));
        registry.apply(&opened(room, 1_000, 1), 1_000).unwrap();
    }

    let (first, cursor) = registry.list(None, None, 100).unwrap();
    assert_eq!(first.len(), 100);
    assert_eq!(cursor, Some(room("game", "room-099")));

    let mut seen: Vec<String> = first.into_iter().map(|row| row.name).collect();
    let mut cursor = cursor;
    let mut pages = 1;
    while let Some(after) = cursor {
        let (rows, next) = registry.list(None, Some(&after), 100).unwrap();
        pages += 1;
        seen.extend(rows.into_iter().map(|row| row.name));
        cursor = next;
    }
    assert_eq!(pages, 3);
    assert_eq!(seen.len(), 250);
    assert_eq!(seen.first().map(String::as_str), Some("room-000"));
    assert_eq!(seen.last().map(String::as_str), Some("room-249"));
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 250);
}

#[test]
fn limit_clamped() {
    let host = MemHost::new();
    let registry = Registry::new(host.sql());
    for index in 0..3u32 {
        let room = room("game", &format!("room-{index:03}"));
        registry.apply(&opened(room, 1_000, 1), 1_000).unwrap();
    }

    let (rows, next) = registry.list(None, None, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(next.is_some());

    let (rows, next) = registry.list(None, None, 10_000).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(next, None);

    for index in 3..153u32 {
        let room = room("game", &format!("room-{index:03}"));
        registry.apply(&opened(room, 1_000, 1), 1_000).unwrap();
    }
    let (rows, next) = registry.list(None, None, 10_000).unwrap();
    assert_eq!(rows.len(), 100);
    assert!(next.is_some());
}

#[test]
fn reopen_after_dispose_is_fresh_row() {
    let host = MemHost::new();
    let registry = Registry::new(host.sql());
    let room = room("tictactoe", "r1");

    registry
        .apply(&opened(room.clone(), 1_000, 1), 1_200)
        .unwrap();
    registry.apply(&disposed(room.clone()), 1_300).unwrap();
    registry.apply(&opened(room, 9_000, 1), 9_500).unwrap();

    let (rows, _) = registry.list(None, None, 100).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].created_ms, 9_000);
    assert_eq!(rows[0].updated_ms, 9_500);
}
