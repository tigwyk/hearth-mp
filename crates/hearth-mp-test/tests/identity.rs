//! Plan v0.3b Task 1: the gateway-verified provider and display name reach a
//! room's `Client`, and survive a drop, a hibernation and a resume.

mod support;

use hearth_mp_server::SqlValue;
use hearth_mp_test::TestRoom;
use serde_json::json;
use support::CounterRoom;

fn new_room() -> TestRoom<CounterRoom> {
    TestRoom::new()
}

/// A `hearth_held` table as v0.3a created it, before the identity columns.
const OLD_HELD_TABLE: &str = "CREATE TABLE hearth_held (\
    player_id TEXT PRIMARY KEY, seat INTEGER NOT NULL, until_ms INTEGER NOT NULL)";

#[test]
fn join_carries_verified_identity_into_on_join() {
    let mut r = new_room();
    let a = r.join_as("jwt_u1", "jwt", Some("Ann"));

    assert!(
        r.client(a).closed.is_none(),
        "the identified join succeeded: {:?}",
        r.client(a).closed
    );
    assert_eq!(
        r.server_state()["join_identity"],
        json!(["jwt_u1:jwt:Ann"]),
        "on_join sees the verified provider and display name"
    );
}

#[test]
fn identity_survives_drop_hibernate_and_resume() {
    let mut r = new_room();
    let a = r.join_as("jwt_u1", "jwt", Some("Ann"));
    r.drop_socket(a);

    let rows = r
        .sql()
        .exec(
            "SELECT provider, display_name FROM hearth_held ORDER BY player_id",
            vec![],
        )
        .expect("hearth_held is readable");
    assert_eq!(rows[0]["provider"], json!("jwt"));
    assert_eq!(rows[0]["display_name"], json!("Ann"));

    r.hibernate();
    let b = r.resume(a);
    assert!(
        r.client(b).closed.is_none(),
        "the resume succeeded: {:?}",
        r.client(b).closed
    );
    assert_eq!(
        r.server_state()["reconnect_identity"],
        json!(["jwt_u1:jwt:Ann"]),
        "on_reconnect sees the identity restored from the held row"
    );
}

#[test]
fn old_held_rows_read_as_anonymous_and_gain_the_columns() {
    let mut r = new_room();
    r.sql()
        .exec(OLD_HELD_TABLE, vec![])
        .expect("the old schema is creatable");
    r.sql()
        .exec(
            "INSERT INTO hearth_held (player_id, seat, until_ms) VALUES (?, ?, ?)",
            vec![
                SqlValue::Text("jwt_u1".to_owned()),
                SqlValue::Int(0),
                SqlValue::Int(30_000),
            ],
        )
        .expect("the old held row is writable");

    // The connect carries a fresh verified identity; the held row predates
    // identity columns, so the resume must keep reading it as anonymous.
    let a = r.join_as("jwt_u1", "jwt", Some("Ann"));
    assert!(r.client(a).closed.is_none());
    assert_eq!(
        r.server_state()["reconnect_identity"],
        json!(["jwt_u1:anon:"]),
        "a legacy held row reads as provider anon and no name"
    );

    let columns = r
        .sql()
        .exec("SELECT name FROM pragma_table_info('hearth_held')", vec![])
        .expect("the held table is migratable");
    let names: Vec<String> = columns
        .iter()
        .filter_map(|row| row.get("name"))
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect();
    assert!(names.contains(&"provider".to_owned()), "{names:?}");
    assert!(
        names.contains(&"display_name".to_owned()),
        "the migration added both identity columns: {names:?}"
    );
}

#[test]
fn world_join_as_carries_identity_too() {
    let mut world = hearth_mp_test::world::TestWorld::new();
    let room = world.add_room::<CounterRoom>("counter", "r1");
    world.join_as(&room, "jwt_u1", "jwt", Some("Ann"));

    assert_eq!(
        world.room_state(&room)["join_identity"],
        json!(["jwt_u1:jwt:Ann"]),
        "the multi-object world harness passes the identity through as well"
    );
}
