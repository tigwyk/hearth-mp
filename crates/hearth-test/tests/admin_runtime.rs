//! Plan v0.2c Task 1: room identity, registry reports, read-only inspect and
//! force-dispose.

mod support;

use std::time::Duration;

use hearth_protocol::{close, encode, Encoding, Frame};
use hearth_server::{Room as RoomTrait, RoomName, RoomReport, SqlValue};
use hearth_test::world::{RoomRef, ServiceRef, TestWorld};
use hearth_test::TestRoom;
use serde_json::json;
use support::{CounterRoom, ProbeDisposeRoom, TestRegistry};

type Room = TestRoom<CounterRoom>;

fn new_room() -> Room {
    TestRoom::new()
}

fn room_name() -> RoomName {
    RoomName {
        room_type: "counter".to_owned(),
        name: "r1".to_owned(),
    }
}

/// A one-room world with a registry service: reports travel through the durable
/// outbox to a service now, not through a direct host call.
fn world_with_registry() -> (TestWorld, RoomRef, ServiceRef) {
    let mut world = TestWorld::new();
    let room = world.add_room::<CounterRoom>("counter", "r1");
    let registry = world.add_service::<TestRegistry>("global");
    (world, room, registry)
}

/// Every report the registry service received, in delivery order.
fn reports(world: &mut TestWorld, registry: &ServiceRef) -> Vec<RoomReport> {
    let state = world.service_state(registry);
    serde_json::from_value(state.get("reports").cloned().unwrap_or_else(|| json!([])))
        .expect("registry reports decode")
}

/// The non-system tables in one world room's storage.
fn world_tables(world: &TestWorld, binding: &str, name: &str) -> Vec<String> {
    let sql = world.sql_of(binding, name);
    let rows = sql
        .sql()
        .exec(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            vec![],
        )
        .expect("sqlite_master is readable");
    rows.iter()
        .filter_map(|row| row.get("name"))
        .filter_map(|value| value.as_str())
        .map(str::to_owned)
        .collect()
}

fn log_events<R: RoomTrait>(room: &TestRoom<R>) -> Vec<String> {
    room.logs()
        .iter()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|_| panic!("log line is not JSON: {line}"));
            value
                .get("hearth")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned()
        })
        .collect()
}

#[test]
fn first_join_reports_open_then_players() {
    let (mut world, room, registry) = world_with_registry();
    let a = world.join(&room, "a", json!({}));
    let open = RoomReport::Open {
        room: room_name(),
        created_ms: 0,
        players: 1,
    };
    assert_eq!(reports(&mut world, &registry), vec![open.clone()]);

    let _b = world.join(&room, "b", json!({}));
    let players = RoomReport::Players {
        room: room_name(),
        players: 2,
    };
    assert_eq!(reports(&mut world, &registry), vec![open, players]);

    world.intent(a, json!({"Add": 1}));
    assert_eq!(
        reports(&mut world, &registry).len(),
        2,
        "an intent changes no seated count"
    );
}

#[test]
fn drop_keeps_count_held_then_expiry_reports() {
    let (mut world, room, registry) = world_with_registry();
    let a = world.join(&room, "a", json!({}));
    let _b = world.join(&room, "b", json!({}));
    assert_eq!(reports(&mut world, &registry).len(), 2);

    world.drop_socket(a);
    assert_eq!(
        reports(&mut world, &registry).len(),
        2,
        "a held seat still counts as a player"
    );

    world.advance(Duration::from_secs(30));
    assert_eq!(
        reports(&mut world, &registry).last(),
        Some(&RoomReport::Players {
            room: room_name(),
            players: 1,
        }),
        "grace expiry reports the smaller seated count: {:?}",
        reports(&mut world, &registry)
    );
}

#[test]
fn dispose_reports_disposed() {
    let (mut world, room, registry) = world_with_registry();
    let a = world.join(&room, "a", json!({}));
    world.intent(a, json!("Dispose"));

    assert_eq!(
        reports(&mut world, &registry),
        vec![
            RoomReport::Open {
                room: room_name(),
                created_ms: 0,
                players: 1,
            },
            RoomReport::Disposed { room: room_name() },
        ]
    );
    assert!(world_tables(&world, "counter", "r1").is_empty());
    assert_eq!(
        world.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::DISPOSED)
    );
}

#[test]
fn inspect_shows_full_state_including_server_only() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let _b = r.join("b", json!({}));
    r.intent(a, json!({"Timer": ["t", 60_000]}));
    r.intent(a, json!({"Secret": "s"}));
    r.drop_socket(a);

    let doc = r.inspect().expect("the room exists");

    assert_eq!(doc.room, room_name());
    assert_eq!(doc.created_ms, 0);
    assert!(doc.seq > 0, "the flushed intents persisted");
    assert!(!doc.locked);
    assert!(!doc.incompatible);
    assert_eq!(doc.state.as_ref().unwrap()["hidden"], json!(1));
    assert_eq!(
        doc.state.as_ref().unwrap()["secrets"],
        json!({"a": "s"}),
        "owner-only fields are included in the admin state"
    );
    assert!(doc.state_bytes > 0);

    assert_eq!(
        doc.clients,
        vec![
            hearth_server::InspectClient {
                player_id: "b".into(),
                seat: 1,
                connected: true,
                held_until_ms: None,
            },
            hearth_server::InspectClient {
                player_id: "a".into(),
                seat: 0,
                connected: false,
                held_until_ms: Some(30_000),
            },
        ]
    );
    assert_eq!(
        doc.timers,
        vec![("h:grace:a".to_owned(), 30_000), ("u:t".to_owned(), 60_000),],
        "every scheduled timer is visible, internal ones included"
    );
}

#[test]
fn inspect_nonexistent_room_writes_nothing() {
    let mut r = new_room();

    assert!(r.inspect().is_none());
    assert!(
        support::table_names(&r).is_empty(),
        "inspect must not create tables: {:?}",
        support::table_names(&r)
    );
}

#[test]
fn force_dispose_incompatible_room() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.sql()
        .exec(
            "UPDATE hearth_state SET data = ? WHERE id = 0",
            vec![SqlValue::Blob(vec![0xde, 0xad, 0xbe, 0xef])],
        )
        .expect("the test writes garbage state");

    assert!(
        r.force_dispose(),
        "a room with identity can be force-disposed"
    );
    assert!(
        support::table_names(&r).is_empty(),
        "force-dispose wipes storage: {:?}",
        support::table_names(&r)
    );

    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::DISPOSED)
    );
    let events = log_events(&r);
    assert!(
        events.contains(&"state_incompatible".to_owned()),
        "{events:?}"
    );
    assert!(
        !events.contains(&"handler_error".to_owned()),
        "on_dispose is skipped for an incompatible room: {events:?}"
    );
}

#[test]
fn force_dispose_runs_on_dispose_when_compatible() {
    let mut r = TestRoom::<ProbeDisposeRoom>::new();
    support::PROBE_DISPOSE_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    r.connect_raw("a");

    assert!(r.force_dispose());
    assert_eq!(
        support::PROBE_DISPOSE_CALLS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "force-dispose runs the room's on_dispose"
    );
    assert!(support::table_names(&r).is_empty());
}

#[test]
fn force_dispose_unknown_room_is_false_and_writes_nothing() {
    let mut r = new_room();
    assert!(!r.force_dispose());
    assert!(support::table_names(&r).is_empty());
}

#[test]
fn logs_join_leave_dispose_and_handler_errors() {
    let mut r = new_room();
    r.join("x", json!({"reject": true})); // rejected join -> handler_error
    let a = r.join("a", json!({})); // join
    r.intent(a, json!("Fail")); // rejected intent -> handler_error
    r.send_raw(a, encode(&Frame::Leave {}, Encoding::MsgPack)); // leave
    let b = r.join("b", json!({})); // join
    r.intent(b, json!("Dispose")); // dispose

    let events = log_events(&r);
    assert!(events.contains(&"join".to_owned()), "{events:?}");
    assert!(events.contains(&"leave".to_owned()), "{events:?}");
    assert!(events.contains(&"dispose".to_owned()), "{events:?}");
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "handler_error")
            .count(),
        2,
        "rejected join and rejected intent are logged: {events:?}"
    );
    let logs = r.logs();
    let join = logs
        .iter()
        .find(|line| line.contains("\"hearth\":\"join\""))
        .expect("a join log line");
    assert!(join.contains("\"room\":\"counter/r1\""), "{join}");
    let leave = logs
        .iter()
        .find(|line| line.contains("\"hearth\":\"leave\""))
        .expect("a leave log line");
    assert!(leave.contains("\"player\":\"a\""), "{leave}");
}
