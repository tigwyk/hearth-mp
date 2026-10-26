//! Plan v0.3a Task 2: the registry is a [`Service`] now, and rooms reach it
//! through the durable outbox (`ctx.notify`) instead of a direct host call.

use hearth_mp_admin::registry::RegistryService;
use hearth_mp_admin::RoomRow;
use hearth_mp_server::{PlayerId, RoomName, RoomReport};
use hearth_mp_test::world::TestWorld;
use serde_json::json;

fn room(name: &str) -> RoomName {
    RoomName {
        room_type: "tictactoe".to_owned(),
        name: name.to_owned(),
    }
}

fn seated(players: &[&str]) -> Vec<PlayerId> {
    players
        .iter()
        .map(|id| PlayerId((*id).to_owned()))
        .collect()
}

fn open(room: RoomName, players: &[&str]) -> RoomReport {
    RoomReport::Open {
        room,
        created_ms: 0,
        players: seated(players),
        capacity: 2,
        locked: false,
        metadata: serde_json::Value::Null,
    }
}

fn players(room: RoomName, players: &[&str]) -> RoomReport {
    RoomReport::Players {
        room,
        players: seated(players),
        capacity: 2,
        locked: false,
        metadata: serde_json::Value::Null,
    }
}

fn list_rows(world: &TestWorld) -> Vec<RoomRow> {
    let sql = world.sql_of("HEARTH_REGISTRY", "global");
    let rows = sql
        .sql()
        .exec(
            "SELECT room_type, name, players, created_ms, updated_ms FROM hearth_rooms \
             ORDER BY room_type, name",
            vec![],
        )
        .expect("the registry table is readable");
    rows.into_iter()
        .map(|row| serde_json::from_value(serde_json::Value::Object(row)).expect("a RoomRow"))
        .collect()
}

#[test]
fn registry_service_applies_reports() {
    let mut world = TestWorld::new();
    world.add_service::<RegistryService>("global");

    world
        .call::<RegistryService>("global", open(room("r1"), &["a"]))
        .expect("open is applied");
    world
        .call::<RegistryService>("global", players(room("r1"), &["a", "b"]))
        .expect("players is applied");

    let rows = list_rows(&world);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "r1");
    assert_eq!(rows[0].players, 2);

    world
        .call::<RegistryService>("global", RoomReport::Disposed { room: room("r1") })
        .expect("dispose is applied");
    assert!(list_rows(&world).is_empty());
}

#[test]
fn registry_service_is_deduplicated_by_the_inbox() {
    let mut world = TestWorld::new();
    world.add_service::<RegistryService>("global");
    world
        .call::<RegistryService>("global", open(room("r1"), &["a"]))
        .expect("open is applied");

    // Re-sending the same envelope is a no-op: the service deduplicates it.
    world.redeliver_last().expect("a duplicate answers success");

    assert_eq!(list_rows(&world).len(), 1);
    assert_eq!(list_rows(&world)[0].created_ms, 0);
    assert_eq!(json!(list_rows(&world)[0].players), json!(1));
}
