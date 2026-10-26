//! Plan v0.3a Task 5: the sample's per-player stats, recorded by a Player DO
//! through durable `ctx.notify` when a game ends.

use std::time::Duration;

use hearth_mp_test::world::{RoomRef, ServiceRef, TestWorld};
use serde_json::{json, Value};
use tictactoe::{TicTacToe, TttPlayer};

struct World {
    world: TestWorld,
    room: RoomRef,
    alice: ServiceRef,
    bob: ServiceRef,
}

fn world() -> World {
    let mut world = TestWorld::new();
    let room = world.add_room::<TicTacToe>("TICTACTOE", "r1");
    let alice = world.add_service::<TttPlayer>("a");
    let bob = world.add_service::<TttPlayer>("b");
    World {
        world,
        room,
        alice,
        bob,
    }
}

impl World {
    /// X is the first player ("a"), O the second ("b").
    fn new_game(&mut self) -> (hearth_mp_test::C, hearth_mp_test::C) {
        (
            self.world.join(&self.room, "a", json!({})),
            self.world.join(&self.room, "b", json!({})),
        )
    }

    fn place(&mut self, client: hearth_mp_test::C, cell: u8) {
        self.world
            .intent(client, json!({ "Place": { "cell": cell } }));
    }
}

/// The player's persisted stats.
fn stats(world: &mut TestWorld, player: &ServiceRef) -> Value {
    world.service_state(player)
}

#[test]
fn win_records_win_and_loss() {
    let mut game = world();
    let (a, b) = game.new_game();
    game.place(a, 0);
    game.place(b, 3);
    game.place(a, 1);
    game.place(b, 4);
    game.place(a, 2);

    assert_eq!(
        stats(&mut game.world, &game.alice),
        json!({"wins": 1, "losses": 0, "draws": 0})
    );
    assert_eq!(
        stats(&mut game.world, &game.bob),
        json!({"wins": 0, "losses": 1, "draws": 0})
    );
}

#[test]
fn draw_records_draws() {
    let mut game = world();
    let (a, b) = game.new_game();
    for cell in [0u8, 1, 2, 4, 3, 5, 7, 6] {
        let player = if [0u8, 2, 3, 7].contains(&cell) { a } else { b };
        game.place(player, cell);
    }
    game.place(a, 8);

    assert_eq!(
        stats(&mut game.world, &game.alice),
        json!({"wins": 0, "losses": 0, "draws": 1})
    );
    assert_eq!(
        stats(&mut game.world, &game.bob),
        json!({"wins": 0, "losses": 0, "draws": 1})
    );
}

#[test]
fn failed_move_after_end_does_not_double_count() {
    let mut game = world();
    let (a, b) = game.new_game();
    game.place(a, 0);
    game.place(b, 3);
    game.place(a, 1);
    game.place(b, 4);
    game.place(a, 2);

    // The game is over: this move is rejected and must not record a second
    // result for either player.
    game.place(b, 5);

    assert_eq!(
        stats(&mut game.world, &game.alice),
        json!({"wins": 1, "losses": 0, "draws": 0})
    );
    assert_eq!(
        stats(&mut game.world, &game.bob),
        json!({"wins": 0, "losses": 1, "draws": 0})
    );
}

#[test]
fn stats_survive_player_service_outage() {
    let mut game = world();
    let (a, b) = game.new_game();
    // The player objects are briefly unreachable: the game still finishes and
    // the notify rows wait in the outbox.
    game.world.partition("TTT_PLAYER", true);
    game.place(a, 0);
    game.place(b, 3);
    game.place(a, 1);
    game.place(b, 4);
    game.place(a, 2);

    game.world.partition("TTT_PLAYER", false);
    game.world.advance(Duration::from_secs(10));

    assert_eq!(
        stats(&mut game.world, &game.alice),
        json!({"wins": 1, "losses": 0, "draws": 0})
    );
    assert_eq!(
        stats(&mut game.world, &game.bob),
        json!({"wins": 0, "losses": 1, "draws": 0})
    );
}
