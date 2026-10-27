//! The ladder service end to end in the native test world: queue, pairing,
//! agreed and disputed results, the widening window, the leaderboard.

use std::time::Duration;

use ffny_ladder::{Ladder, LadderReply, LadderRpc, Outcome};
use hearth_mp::PlayerId;
use hearth_mp_test::world::TestWorld;

fn p(id: &str) -> PlayerId {
    PlayerId(id.to_owned())
}

fn call(world: &mut TestWorld, rpc: LadderRpc) -> LadderReply {
    world.call::<Ladder>("global", rpc).expect("ladder call")
}

fn search(world: &mut TestWorld, id: &str) -> LadderReply {
    call(
        world,
        LadderRpc::Search {
            player: p(id),
            name: id.to_uppercase(),
        },
    )
}

fn report(world: &mut TestWorld, id: &str, pairing: &str, result: Outcome) -> LadderReply {
    call(
        world,
        LadderRpc::Report {
            player: p(id),
            pairing: pairing.to_owned(),
            result,
        },
    )
}

fn world() -> TestWorld {
    let mut world = TestWorld::new();
    world.add_service::<Ladder>("global");
    world
}

/// Two players search; returns the pairing id after both have been told.
fn pair(world: &mut TestWorld) -> String {
    assert!(matches!(
        search(world, "alice"),
        LadderReply::Searching { .. }
    ));
    let LadderReply::Matched { pairing, opponent } = search(world, "bob") else {
        panic!("bob should match alice");
    };
    assert_eq!(opponent.player, "alice");
    // Alice learns of it on her next poll, once.
    let LadderReply::Matched {
        pairing: same,
        opponent,
    } = search(world, "alice")
    else {
        panic!("alice should be told");
    };
    assert_eq!(
        (same.as_str(), opponent.player.as_str()),
        (pairing.as_str(), "bob")
    );
    pairing
}

#[test]
fn agreed_result_moves_both_ratings_and_fills_the_board() {
    let mut w = world();
    let pairing = pair(&mut w);
    assert_eq!(
        report(&mut w, "alice", &pairing, Outcome::Win),
        LadderReply::Reported {
            settled: false,
            counted: false
        }
    );
    assert_eq!(
        report(&mut w, "bob", &pairing, Outcome::Loss),
        LadderReply::Reported {
            settled: true,
            counted: true
        }
    );
    let LadderReply::Top { entries } = call(&mut w, LadderRpc::Top { limit: 10 }) else {
        panic!()
    };
    assert_eq!(entries.len(), 2);
    assert_eq!((entries[0].player.as_str(), entries[0].wins), ("alice", 1));
    assert!(entries[0].rating > 1500 && entries[1].rating < 1500);
}

#[test]
fn disagreeing_reports_count_for_nobody() {
    let mut w = world();
    let pairing = pair(&mut w);
    report(&mut w, "alice", &pairing, Outcome::Win);
    assert_eq!(
        report(&mut w, "bob", &pairing, Outcome::Win),
        LadderReply::Reported {
            settled: true,
            counted: false
        }
    );
    let LadderReply::Top { entries } = call(&mut w, LadderRpc::Top { limit: 10 }) else {
        panic!()
    };
    assert!(entries.is_empty());
}

#[test]
fn a_stranger_cannot_report_a_pairing() {
    let mut w = world();
    let pairing = pair(&mut w);
    let err = w
        .call::<Ladder>(
            "global",
            LadderRpc::Report {
                player: p("mallory"),
                pairing,
                result: Outcome::Win,
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("no open pairing"), "{err}");
}

#[test]
fn a_settled_pairing_cannot_be_reported_again() {
    let mut w = world();
    let pairing = pair(&mut w);
    report(&mut w, "alice", &pairing, Outcome::Win);
    report(&mut w, "bob", &pairing, Outcome::Loss);
    assert!(w
        .call::<Ladder>(
            "global",
            LadderRpc::Report {
                player: p("alice"),
                pairing,
                result: Outcome::Win
            },
        )
        .is_err());
}

#[test]
fn the_window_widens_with_waiting() {
    let mut w = world();
    // Make carol strong: she beats dave once, so her rating sits well above 1500.
    search(&mut w, "carol");
    let LadderReply::Matched { pairing, .. } = search(&mut w, "dave") else {
        panic!()
    };
    search(&mut w, "carol");
    report(&mut w, "carol", &pairing, Outcome::Win);
    report(&mut w, "dave", &pairing, Outcome::Loss);

    // Dave (now below 1500) and carol are ~hundreds apart; fresh erin sits at 1500.
    search(&mut w, "carol");
    // Erin is within 100 of nobody who is searching only if carol is far above;
    // after waiting, the window opens and they pair.
    let mut matched = false;
    for _ in 0..40 {
        w.advance(Duration::from_secs(1));
        search(&mut w, "carol");
        if matches!(search(&mut w, "erin"), LadderReply::Matched { .. }) {
            matched = true;
            break;
        }
    }
    assert!(matched, "a wide enough window must pair them");
}

#[test]
fn a_cancelled_search_is_not_paired() {
    let mut w = world();
    search(&mut w, "alice");
    call(&mut w, LadderRpc::Cancel { player: p("alice") });
    assert!(matches!(
        search(&mut w, "bob"),
        LadderReply::Searching { .. }
    ));
}

#[test]
fn a_silent_searcher_expires() {
    let mut w = world();
    search(&mut w, "alice");
    w.advance(Duration::from_secs(30));
    assert!(matches!(
        search(&mut w, "bob"),
        LadderReply::Searching { .. }
    ));
}
