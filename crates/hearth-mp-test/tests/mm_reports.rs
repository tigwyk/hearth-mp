//! v0.3a Task 4: a room reports its lobby state to the matchmaker.
//!
//! The room fixture is defined here by hand: the `#[hearth_mp::room]` macros are
//! for wasm Durable Object wrappers, which a native test binary cannot export.
//! `world.set_matchmaker` is the test hook for the gateway's
//! `X-Hearth-Matchmaker` header.

use std::time::Duration;

use hearth_mp_matchmaker::{MatchmakerService, MmReply, MmRpc, OpenRoom, BINDING};
use hearth_mp_protocol::PlayerId;
use hearth_mp_server::{Client, Ctx, JoinOpts, Leave, Mode, Room, RoomConfig};
use hearth_mp_sync::{HearthState, Schema};
use hearth_mp_test::world::{RoomRef, TestWorld};
use hearth_mp_test::C;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One matchmaker Durable Object per `(room_type, region)`.
const MM: &str = "tictactoe:enam";
const POOL: &str = "default";

#[derive(HearthState, Serialize, Deserialize, Default, Debug)]
struct ReportState {
    #[sync]
    turns: i64,
}

#[derive(Deserialize, Schema, Debug)]
enum ReportIntent {
    /// `ctx.set_metadata({"mode": "ranked"})`.
    SetMetadata,
    /// `ctx.lock()`.
    Lock,
}

#[derive(Default)]
struct ReportRoom;

impl RoomConfig for ReportRoom {
    const MODE: Mode = Mode::Event;
    const MAX_CLIENTS: u8 = 2;
    const RECONNECT_GRACE: Duration = Duration::ZERO;
}

impl Room for ReportRoom {
    type State = ReportState;
    type Intent = ReportIntent;

    async fn on_join(
        &mut self,
        _ctx: &mut Ctx<Self>,
        _who: &Client,
        _opts: JoinOpts,
    ) -> hearth_mp_server::Result<()> {
        Ok(())
    }

    async fn on_intent(
        &mut self,
        ctx: &mut Ctx<Self>,
        _who: &Client,
        intent: ReportIntent,
    ) -> hearth_mp_server::Result<()> {
        match intent {
            ReportIntent::SetMetadata => ctx.set_metadata(json!({"mode": "ranked"})),
            ReportIntent::Lock => ctx.lock(),
        }
        ctx.state().turns += 1;
        Ok(())
    }

    async fn on_leave(
        &mut self,
        _ctx: &mut Ctx<Self>,
        _who: &Client,
        _reason: Leave,
    ) -> hearth_mp_server::Result<()> {
        Ok(())
    }
}

fn world() -> TestWorld {
    let mut world = TestWorld::new();
    world.add_service::<MatchmakerService>(MM);
    world
}

fn pid(player: &str) -> PlayerId {
    PlayerId(player.to_owned())
}

fn quick_join(world: &mut TestWorld, player: &str, capacity: u8) -> String {
    let reply = world
        .call::<MatchmakerService>(
            MM,
            MmRpc::QuickJoin {
                player: pid(player),
                pool: POOL.to_owned(),
                capacity,
            },
        )
        .expect("quick-join succeeds");
    let MmReply::Room { name } = reply else {
        panic!("quick-join returned {reply:?}");
    };
    name
}

fn list(world: &mut TestWorld) -> Vec<OpenRoom> {
    let reply = world
        .call::<MatchmakerService>(
            MM,
            MmRpc::List {
                pool: POOL.to_owned(),
                limit: 50,
            },
        )
        .expect("list succeeds");
    let MmReply::Rooms(rooms) = reply else {
        panic!("list returned {reply:?}");
    };
    rooms
}

fn mm_rooms(world: &TestWorld) -> Vec<serde_json::Map<String, Value>> {
    world
        .sql_of(BINDING, MM)
        .sql()
        .exec(
            "SELECT name, players, capacity, locked, metadata FROM mm_rooms",
            Vec::new(),
        )
        .expect("the matchmaker's mm_rooms table exists")
}

fn mm_row(world: &TestWorld, room: &str) -> serde_json::Map<String, Value> {
    mm_rooms(world)
        .into_iter()
        .find(|row| row.get("name").and_then(Value::as_str) == Some(room))
        .unwrap_or_else(|| panic!("no mm_rooms row for {room}"))
}

/// The world's matchmaker only accepts reports for names it minted, so mint the
/// name, then add a real room with it pointed at the matchmaker and join p1.
fn reported_room(world: &mut TestWorld, player: &str, minted_capacity: u8) -> (String, RoomRef, C) {
    let name = quick_join(world, player, minted_capacity);
    let room = world.add_room::<ReportRoom>("tictactoe", &name);
    world.set_matchmaker(&room, MM);
    let client = world.join(&room, player, json!({}));
    (name, room, client)
}

#[test]
fn join_reports_open_with_players() {
    let mut world = world();
    // Mint with a different capacity: the room's own report must overwrite it.
    let (name, _room, _p1) = reported_room(&mut world, "p1", 9);

    let rooms = list(&mut world);
    assert_eq!(rooms.len(), 1, "the room is reported to the matchmaker");
    assert_eq!(rooms[0].name, name);
    assert_eq!(rooms[0].players, 1, "the report carries the seated player");
    assert_eq!(
        rooms[0].capacity,
        ReportRoom::MAX_CLIENTS,
        "the report overwrites the minted capacity"
    );
    assert_eq!(rooms[0].metadata, json!(null));

    let row = mm_row(&world, &name);
    assert_eq!(
        row.get("locked").and_then(Value::as_i64),
        Some(0),
        "the open room is unlocked"
    );
    let players: Vec<PlayerId> = serde_json::from_str(
        row.get("players")
            .and_then(Value::as_str)
            .expect("players is JSON text"),
    )
    .expect("players is a JSON array");
    assert_eq!(players, vec![pid("p1")], "the report carries player ids");
}

#[test]
fn set_metadata_reports_players() {
    let mut world = world();
    let (_name, _room, p1) = reported_room(&mut world, "p1", 2);
    assert_eq!(list(&mut world)[0].metadata, json!(null));

    world.intent(p1, json!("SetMetadata"));

    let rooms = list(&mut world);
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0].metadata, json!({"mode": "ranked"}));
}

#[test]
fn lock_reports_and_hides_the_room() {
    let mut world = world();
    let (name, _room, p1) = reported_room(&mut world, "p1", 2);
    assert_eq!(list(&mut world).len(), 1);

    world.intent(p1, json!("Lock"));

    assert!(
        list(&mut world).is_empty(),
        "a locked room leaves the lobby listing"
    );
    let row = mm_row(&world, &name);
    assert_eq!(
        row.get("locked").and_then(Value::as_i64),
        Some(1),
        "the locked flag is reported"
    );
}

#[test]
fn dispose_reports_disposed() {
    let mut world = world();
    let (_name, room, _p1) = reported_room(&mut world, "p1", 2);
    assert_eq!(list(&mut world).len(), 1);

    assert!(world.force_dispose(&room));

    assert!(
        list(&mut world).is_empty(),
        "the matchmaker drops a disposed room"
    );
    assert!(
        mm_rooms(&world).is_empty(),
        "the disposed room's row is deleted"
    );
}

#[test]
fn room_without_matchmaker_never_notifies() {
    let mut world = world();
    let room = world.add_room::<ReportRoom>("tictactoe", "r1");
    let p1 = world.join(&room, "p1", json!({}));

    world.intent(p1, json!("SetMetadata"));
    world.intent(p1, json!("Lock"));

    assert!(
        list(&mut world).is_empty(),
        "no matchmaker hook, no lobby rows"
    );
    assert!(
        mm_rooms(&world).is_empty(),
        "the matchmaker never heard of the room"
    );
}
