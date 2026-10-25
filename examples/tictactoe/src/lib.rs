//! Hearth v0.1 sample: tic-tac-toe.
//!
//! One Durable Object room per match. The board is ordinary `#[sync]` state, so
//! the framework pushes a delta patch after every move; the room only has to
//! enforce the rules.

use hearth::anyhow;
use hearth::sync::SyncMap;
use hearth::{Client, Ctx, HearthState, JoinOpts, Leave, PlayerId, Result, Room, Schema};
use serde::{Deserialize, Serialize};
use worker::{Context, Env, Request, Response};

/// Server-side game status. Included in the schema, so clients see it.
#[derive(Schema, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[hearth(crate = "hearth")]
pub enum Status {
    #[default]
    Waiting,
    Playing,
    Won(PlayerId),
    Draw,
}

/// The synced room state: only `#[sync]` fields reach clients.
#[derive(HearthState, Serialize, Deserialize, Default)]
#[hearth(crate = "hearth")]
pub struct Board {
    /// Nine cells: 0 empty, 1 X, 2 O.
    #[sync]
    pub cells: Vec<u8>,
    #[sync]
    pub marks: SyncMap<PlayerId, u8>,
    #[sync]
    pub turn: PlayerId,
    #[sync]
    pub status: Status,
}

/// Intents are the only way a client can change the room.
#[derive(Deserialize)]
pub enum Move {
    Place { cell: u8 },
    Rematch,
}

const LINES: [[usize; 3]; 8] = [
    [0, 1, 2],
    [3, 4, 5],
    [6, 7, 8],
    [0, 3, 6],
    [1, 4, 7],
    [2, 5, 8],
    [0, 4, 8],
    [2, 4, 6],
];

#[derive(Default)]
pub struct TicTacToe;

#[hearth::room(mode = "event", max_clients = 2)]
impl Room for TicTacToe {
    type State = Board;
    type Intent = Move;

    async fn on_join(&mut self, ctx: &mut Ctx<Self>, who: &Client, _opts: JoinOpts) -> Result<()> {
        let state = ctx.state();
        // First joiner is X, second is O; a player replacing a stale socket is
        // not re-joined by the runtime, so this only runs for real joins.
        let mark = if state.marks.values().any(|mark| *mark == 1) {
            2
        } else {
            1
        };
        state.marks.insert(who.player_id.clone(), mark);

        if state.marks.len() == 2 {
            state.cells = vec![0; 9];
            state.status = Status::Playing;
            if let Some((x_player, _)) = state.marks.iter().find(|(_, mark)| **mark == 1) {
                state.turn = x_player.clone();
            }
        }
        Ok(())
    }

    async fn on_intent(&mut self, ctx: &mut Ctx<Self>, who: &Client, intent: Move) -> Result<()> {
        match intent {
            Move::Place { cell } => {
                let state = ctx.state();
                if state.status != Status::Playing {
                    anyhow::bail!("the game is not playing");
                }
                if state.turn != who.player_id {
                    anyhow::bail!("it is not your turn");
                }
                let index = usize::from(cell);
                if index >= 9 {
                    anyhow::bail!("cell {cell} is out of range");
                }
                if state.cells[index] != 0 {
                    anyhow::bail!("cell {cell} is already taken");
                }
                let Some(&mark) = state.marks.get(&who.player_id) else {
                    anyhow::bail!("the player has no seat");
                };
                state.cells[index] = mark;

                if let Some(winning_mark) = winning_mark(&state.cells) {
                    let winner = state
                        .marks
                        .iter()
                        .find(|(_, candidate)| **candidate == winning_mark)
                        .map(|(player, _)| player.clone())
                        .unwrap_or_else(|| who.player_id.clone());
                    state.status = Status::Won(winner);
                } else if state.cells.iter().all(|cell| *cell != 0) {
                    state.status = Status::Draw;
                } else if let Some((next, _)) = state
                    .marks
                    .iter()
                    .find(|(_, candidate)| **candidate != mark)
                {
                    state.turn = next.clone();
                }
            }
            Move::Rematch => {
                let state = ctx.state();
                if !matches!(state.status, Status::Won(_) | Status::Draw) {
                    anyhow::bail!("a rematch is only available after a finished game");
                }
                state.cells = vec![0; 9];
                state.status = if state.marks.len() == 2 {
                    Status::Playing
                } else {
                    Status::Waiting
                };
                if let Some((x_player, _)) = state.marks.iter().find(|(_, mark)| **mark == 1) {
                    state.turn = x_player.clone();
                }
            }
        }
        Ok(())
    }

    async fn on_leave(&mut self, ctx: &mut Ctx<Self>, who: &Client, _reason: Leave) -> Result<()> {
        let state = ctx.state();
        state.marks.remove(&who.player_id);
        if !matches!(state.status, Status::Waiting) {
            // A player left: stop the game and wait for a new opponent.
            state.status = Status::Waiting;
            state.cells = vec![0; 9];
            state.turn = PlayerId::default();
        }
        Ok(())
    }
}

fn winning_mark(cells: &[u8]) -> Option<u8> {
    LINES.iter().find_map(|line| {
        let first = cells[line[0]];
        if first != 0 && line.iter().all(|cell| cells[*cell] == first) {
            Some(first)
        } else {
            None
        }
    })
}

/// The gateway Worker entry: auth, routing, and the tictactoe room route.
#[worker::event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    hearth_gateway::serve(
        req,
        env,
        &[hearth_gateway::RoomRoute {
            room_type: "tictactoe",
            binding: "TICTACTOE",
            schema_hash: <Board as HearthState>::schema_hash_hex,
        }],
    )
    .await
}
