//! Hearth v0.1 sample: tic-tac-toe.
//!
//! One Durable Object room per match. The board is ordinary `#[sync]` state, so
//! the framework pushes a delta patch after every move; the room only has to
//! enforce the rules.

use hearth::anyhow;
use hearth::sync::SyncMap;
use hearth::{
    Client, Ctx, HearthState, JoinOpts, Leave, PlayerId, PlayerService, Result, Room, Schema,
    Service, ServiceCtx,
};
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
#[derive(Schema, Deserialize)]
#[hearth(crate = "hearth")]
pub enum Move {
    Place { cell: u8 },
    Rematch,
}

/// A finished game's outcome for one player.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Win,
    Loss,
    Draw,
}

/// One player's lifetime record, kept in a Player Durable Object so it outlives
/// any match.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct PlayerStats {
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
}

/// What a room tells a player object when a game ends.
#[derive(Serialize, Deserialize, Debug)]
pub enum PlayerRpc {
    Result(Outcome),
}

#[derive(Default)]
pub struct TttPlayer;

#[hearth::service(binding = "TTT_PLAYER", player)]
impl Service for TttPlayer {
    type State = PlayerStats;
    type Rpc = PlayerRpc;
    type Reply = ();

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &hearth::Target,
        rpc: PlayerRpc,
    ) -> Result<()> {
        let PlayerRpc::Result(outcome) = rpc;
        let stats = ctx.state();
        match outcome {
            Outcome::Win => stats.wins += 1,
            Outcome::Loss => stats.losses += 1,
            Outcome::Draw => stats.draws += 1,
        }
        Ok(())
    }
}

impl PlayerService for TttPlayer {
    /// The player sees only their own record, which is the whole state here.
    fn client_view(state: &Self::State) -> serde_json::Value {
        serde_json::to_value(state).expect("PlayerStats is JSON")
    }
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

#[hearth::room(mode = "event", max_clients = 2, binding = "TICTACTOE")]
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

                let mut results: Vec<(PlayerId, Outcome)> = Vec::new();
                if let Some(winning_mark) = winning_mark(&state.cells) {
                    let winner = state
                        .marks
                        .iter()
                        .find(|(_, candidate)| **candidate == winning_mark)
                        .map(|(player, _)| player.clone())
                        .unwrap_or_else(|| who.player_id.clone());
                    state.status = Status::Won(winner.clone());
                    results = state
                        .marks
                        .iter()
                        .map(|(player, candidate)| {
                            let outcome = if *candidate == winning_mark {
                                Outcome::Win
                            } else {
                                Outcome::Loss
                            };
                            (player.clone(), outcome)
                        })
                        .collect();
                } else if state.cells.iter().all(|cell| *cell != 0) {
                    state.status = Status::Draw;
                    results = state
                        .marks
                        .iter()
                        .map(|(player, _)| (player.clone(), Outcome::Draw))
                        .collect();
                } else if let Some((next, _)) = state
                    .marks
                    .iter()
                    .find(|(_, candidate)| **candidate != mark)
                {
                    state.turn = next.clone();
                }
                // Durable fire-after-commit: the record survives a handler
                // failure after this point and a player-object outage.
                for (player, outcome) in results {
                    ctx.notify::<TttPlayer>(&player.0, PlayerRpc::Result(outcome));
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

// The room registry (`hearth inspect rooms`) and the matchmaker
// (`/match`, `/lobby`). Both are optional bindings; a missing one only disables
// its feature.
hearth::admin::registry_object!();
hearth::matchmaker::matchmaker_object!();

/// Every room type this Worker serves.
fn room_routes() -> &'static [hearth_gateway::RoomRoute] {
    static ROUTES: [hearth_gateway::RoomRoute; 1] = [hearth_gateway::RoomRoute {
        room_type: "tictactoe",
        binding: "TICTACTOE",
        container: None,
        schema_hash: hearth::server::schema_hash_hex::<TicTacToe>,
        max_clients: 2,
    }];
    &ROUTES
}

/// The gateway's optional features and login providers.
///
/// The auth providers come from the environment (`.dev.vars` locally, Worker
/// variables and secrets in production), so this deployment enables anonymous
/// logins unless `HEARTH_AUTH_ANONYMOUS=0`, plus whatever else is configured.
fn gateway_config(env: &Env) -> std::result::Result<hearth_gateway::GatewayConfig, String> {
    Ok(hearth_gateway::GatewayConfig {
        rooms: room_routes(),
        player: Some(hearth_gateway::PlayerRoute {
            binding: "TTT_PLAYER",
        }),
        matchmaker: true,
        auth: hearth_gateway::AuthConfig::from_env(env)?,
        limits: hearth_gateway::ratelimit::Limits::default(),
    })
}

/// The gateway Worker entry: auth, routing, and the tictactoe room route.
#[worker::event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    // The admin API exists only when HEARTH_ADMIN_TOKEN is set. Without it these
    // paths are ordinary 404s, as if the feature were not deployed.
    if req.path().starts_with("/admin/") && hearth::admin::admin_token(&env).is_some() {
        return hearth::admin::serve_admin(req, env, room_routes()).await;
    }
    // A provider configured wrongly fails loudly instead of silently disabling.
    let config = match gateway_config(&env) {
        Ok(config) => config,
        Err(message) => return Response::error(message, 500),
    };
    hearth_gateway::serve(req, env, &config).await
}
