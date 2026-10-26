//! v0.3a Task 4: `hearth-mp-matchmaker` quick-join, seat reservations, pools and
//! the lobby listing.
//!
//! These tests own the matchmaker only, so the room reports are driven by hand
//! with `MmRpc::Report`: the caller sends the `RoomReport` the room runtime
//! would send after a real join.

use std::time::Duration;

use hearth_mp_matchmaker::{
    parse_room_name, MatchmakerService, MmReply, MmRpc, OpenRoom, RESERVATION_MS,
};
use hearth_mp_protocol::PlayerId;
use hearth_mp_server::{RoomName, RoomReport};
use hearth_mp_test::world::TestWorld;
use serde_json::{json, Value};

/// One matchmaker Durable Object per `(room_type, region)`.
const MM: &str = "tictactoe:enam";
const POOL: &str = "default";

fn pid(player: &str) -> PlayerId {
    PlayerId(player.to_owned())
}

fn world() -> TestWorld {
    let mut world = TestWorld::new();
    world.add_service::<MatchmakerService>(MM);
    world
}

fn quick_join(world: &mut TestWorld, player: &str, pool: &str, capacity: u8) -> String {
    let reply = world
        .call::<MatchmakerService>(
            MM,
            MmRpc::QuickJoin {
                player: pid(player),
                pool: pool.to_owned(),
                capacity,
            },
        )
        .expect("quick-join succeeds");
    let MmReply::Room { name } = reply else {
        panic!("quick-join returned {reply:?}");
    };
    name
}

fn list(world: &mut TestWorld, pool: &str, limit: u32) -> Vec<OpenRoom> {
    let reply = world
        .call::<MatchmakerService>(
            MM,
            MmRpc::List {
                pool: pool.to_owned(),
                limit,
            },
        )
        .expect("list succeeds");
    let MmReply::Rooms(rooms) = reply else {
        panic!("list returned {reply:?}");
    };
    rooms
}

fn names(rooms: &[OpenRoom]) -> Vec<String> {
    rooms.iter().map(|room| room.name.clone()).collect()
}

fn room_name(name: &str) -> RoomName {
    RoomName {
        room_type: "tictactoe".to_owned(),
        name: name.to_owned(),
    }
}

fn report(world: &mut TestWorld, report: RoomReport) {
    let reply = world
        .call::<MatchmakerService>(MM, MmRpc::Report(report))
        .expect("report is accepted");
    assert_eq!(reply, MmReply::Ok);
}

fn report_players(
    world: &mut TestWorld,
    room: &str,
    players: &[&str],
    capacity: u8,
    locked: bool,
    metadata: Value,
) {
    report(
        world,
        RoomReport::Players {
            room: room_name(room),
            players: players.iter().map(|player| pid(player)).collect(),
            capacity,
            locked,
            metadata,
        },
    );
}

fn report_disposed(world: &mut TestWorld, room: &str) {
    report(
        world,
        RoomReport::Disposed {
            room: room_name(room),
        },
    );
}

#[test]
fn first_quick_join_mints_room_in_region() {
    let mut world = world();

    let name = quick_join(&mut world, "p1", POOL, 2);

    assert!(name.starts_with("m_enam_"), "minted name is {name}");
    let (region, hex) = parse_room_name(&name).expect("a minted name parses");
    assert_eq!(region, "enam");
    assert_eq!(hex.len(), 16);
}

#[test]
fn second_player_gets_same_room_until_full() {
    let mut world = world();

    let first = quick_join(&mut world, "p1", POOL, 2);
    report_players(&mut world, &first, &["p1"], 2, false, json!(null));

    let second = quick_join(&mut world, "p2", POOL, 2);
    assert_eq!(second, first, "the second player fills the open room");

    report_players(&mut world, &first, &["p1", "p2"], 2, false, json!(null));
    let third = quick_join(&mut world, "p3", POOL, 2);
    assert_ne!(third, first, "a full room is not offered");
    assert_eq!(names(&list(&mut world, POOL, 50)), vec![third]);
}

#[test]
fn simultaneous_first_joins_share_new_room() {
    let mut world = world();

    // No report arrives between the two calls: the minted row's capacity and
    // the first player's reservation still seat the second player there.
    let first = quick_join(&mut world, "p1", POOL, 2);
    let second = quick_join(&mut world, "p2", POOL, 2);

    assert_eq!(second, first, "both first joins share the minted room");
}

#[test]
fn last_seat_race_never_overbooks() {
    let mut world = world();
    let room = quick_join(&mut world, "p1", POOL, 2);
    report_players(&mut world, &room, &["p1"], 2, false, json!(null));

    // Both race for the one free seat before either report reaches the
    // matchmaker; the first reservation has to hold the seat for the second.
    let p2 = quick_join(&mut world, "p2", POOL, 2);
    let p3 = quick_join(&mut world, "p3", POOL, 2);

    assert_ne!(p2, p3, "the two racers never share one seat");
    let seated = usize::from(p2 == room) + usize::from(p3 == room);
    assert_eq!(
        seated, 1,
        "exactly one racer takes the last seat: {p2} / {p3}"
    );
}

#[test]
fn same_player_twice_same_room() {
    let mut world = world();
    let room = quick_join(&mut world, "p1", POOL, 2);
    report_players(&mut world, &room, &["p1"], 2, false, json!(null));

    assert_eq!(quick_join(&mut world, "p1", POOL, 2), room);
    // A seated player keeps their room even when asked with other limits.
    assert_eq!(quick_join(&mut world, "p1", "other-pool", 9), room);
}

#[test]
fn stale_reservation_is_dropped_when_the_room_filled() {
    let mut world = world();
    // p1 holds the reservation on a fresh room, then the room fills without
    // them: the promised seat is gone, so a retried match must move on instead
    // of handing the same full room back (the SDK retries `POST /match`).
    let full = quick_join(&mut world, "p1", POOL, 2);
    report_players(&mut world, &full, &["p2", "p3"], 2, false, json!(null));

    let retried = quick_join(&mut world, "p1", POOL, 2);
    assert_ne!(
        retried, full,
        "a full reserved room is not handed out again"
    );
}

#[test]
fn seated_player_gets_their_room() {
    let mut world = world();
    let seated = quick_join(&mut world, "p1", POOL, 2);
    report_players(&mut world, &seated, &["p1"], 2, false, json!(null));

    world.advance(Duration::from_secs(31));

    // The reservation is long gone, but the report says p1 is seated there.
    assert_eq!(quick_join(&mut world, "p1", POOL, 2), seated);
}

#[test]
fn reservation_expires_after_30s() {
    assert_eq!(RESERVATION_MS, 30_000);
    let mut world = world();

    let first = quick_join(&mut world, "p1", POOL, 1);
    world.advance(Duration::from_millis(100));
    let second = quick_join(&mut world, "p2", POOL, 1);
    assert_ne!(second, first, "p1's reservation holds the only seat");

    // Past the window the seat is free again and the oldest room comes back.
    world.advance(Duration::from_millis(RESERVATION_MS + 100));
    let third = quick_join(&mut world, "p3", POOL, 1);
    assert_eq!(third, first, "the expired reservation no longer counts");
}

#[test]
fn locked_room_not_offered() {
    let mut world = world();
    let filled = quick_join(&mut world, "p1", POOL, 2);
    report_players(&mut world, &filled, &["p1"], 2, false, json!(null));
    report_players(&mut world, &filled, &["p1"], 2, true, json!(null));

    assert!(
        list(&mut world, POOL, 50).is_empty(),
        "locked rooms are never listed"
    );

    // Currently fails: `quick_join` does not skip locked rooms when it picks a
    // candidate, so it seats p2 in the locked room instead of minting a new one.
    let next = quick_join(&mut world, "p2", POOL, 2);
    assert_ne!(next, filled, "a locked room is not offered to quick-join");
}

#[test]
fn pools_are_separate() {
    let mut world = world();

    let ranked = quick_join(&mut world, "p1", "ranked", 2);
    let casual = quick_join(&mut world, "p2", "casual", 2);
    assert_ne!(ranked, casual, "each pool mints its own room");

    assert_eq!(names(&list(&mut world, "ranked", 50)), vec![ranked]);
    assert_eq!(names(&list(&mut world, "casual", 50)), vec![casual]);
}

#[test]
fn unknown_room_reports_ignored() {
    let mut world = world();
    let forged = "m_enam_0000000000000000";

    report_players(
        &mut world,
        forged,
        &["p1"],
        2,
        false,
        json!({"mode": "ranked"}),
    );
    report_disposed(&mut world, forged);

    assert!(
        list(&mut world, POOL, 50).is_empty(),
        "a report for a room this matchmaker never minted is ignored"
    );
    assert_ne!(quick_join(&mut world, "p2", POOL, 2), forged);
}

#[test]
fn disposed_room_removed() {
    let mut world = world();
    let room = quick_join(&mut world, "p1", POOL, 2);
    report_players(&mut world, &room, &["p1"], 2, false, json!(null));
    assert_eq!(names(&list(&mut world, POOL, 50)), vec![room.clone()]);

    report_disposed(&mut world, &room);

    assert!(
        list(&mut world, POOL, 50).is_empty(),
        "a disposed room leaves the lobby"
    );
    // Disposing also drops the room's reservation, so p1's stored seat cannot
    // resurrect the dead room.
    assert_ne!(quick_join(&mut world, "p1", POOL, 2), room);
}

#[test]
fn list_sorted_and_clamped() {
    let mut world = world();
    let mut minted: Vec<(String, String)> = Vec::new();
    for index in 0..52 {
        if index > 0 {
            // Distinct mint times make "oldest" observable.
            world.advance(Duration::from_millis(100));
        }
        let player = format!("p{index}");
        let room = quick_join(&mut world, &player, POOL, 1);
        minted.push((player, room));
    }

    // Every minted room holds its own reservation, so no join can reach it.
    // Seat the mints instead: the newest room keeps one free seat, the rest
    // keep two. Fewest free seats first, then oldest.
    for (index, (player, room)) in minted.iter().enumerate() {
        let capacity = if index == 51 { 2 } else { 3 };
        report_players(
            &mut world,
            room,
            &[player.as_str()],
            capacity,
            false,
            json!(null),
        );
    }
    let mut expected: Vec<String> = vec![minted[51].1.clone()];
    expected.extend(minted[..51].iter().map(|(_, room)| room.clone()));

    assert_eq!(
        names(&list(&mut world, POOL, 1)),
        vec![expected[0].clone()],
        "limit 1 clamps and the fewest free seats come first"
    );
    assert_eq!(
        names(&list(&mut world, POOL, 0)),
        vec![expected[0].clone()],
        "limit 0 clamps to 1"
    );
    let clamped = names(&list(&mut world, POOL, 100));
    assert_eq!(clamped.len(), 50, "limit 100 clamps to 50");
    assert_eq!(
        clamped,
        expected[..50].to_vec(),
        "fewest free seats first, then oldest"
    );
}

#[test]
fn metadata_reported_and_listed() {
    let mut world = world();
    let room = quick_join(&mut world, "p1", POOL, 2);
    report_players(
        &mut world,
        &room,
        &["p1"],
        2,
        false,
        json!({"mode": "ranked"}),
    );

    let rooms = list(&mut world, POOL, 50);
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0].metadata, json!({"mode": "ranked"}));
    assert_eq!(rooms[0].players, 1);
    assert_eq!(rooms[0].capacity, 2);

    // A changed metadata value is re-reported and listed.
    report_players(
        &mut world,
        &room,
        &["p1"],
        2,
        false,
        json!({"mode": "casual", "turn": 3}),
    );
    assert_eq!(
        list(&mut world, POOL, 50)[0].metadata,
        json!({"mode": "casual", "turn": 3})
    );
}
