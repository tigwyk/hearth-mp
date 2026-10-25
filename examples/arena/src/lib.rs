//! Hearth v0.4 sample: a ticked 20 Hz arena.
//!
//! The room is a tiny top-down arena. Every avatar moves at the same speed and
//! two avatars that touch push each other apart — the textbook case where a
//! client that predicts must occasionally be corrected. The simulation is one
//! call to [`arena_logic::step`], which the client predictor calls too, so the
//! server and a predicting client agree unless the player's input was late.
//!
//! Intents are buffered: the runtime hands them to `on_intent` at the start of
//! the next tick, so every client steps the same inputs with the same `dt`.

use std::time::Duration;

use arena_logic::{ArenaState, Move};
use hearth::sync::SyncMap;
use hearth::{Client, Ctx, HearthState, JoinOpts, Leave, PlayerId, Result, Room, Schema};
use serde::{Deserialize, Serialize};
use worker::{Context, Env, Request, Response};

/// An avatar position as synced state. Same serde shape as the shared
/// `arena_logic::Avatar`, which keeps the logic crate free of Hearth types (it
/// also compiles into the client GDExtension); the room converts between the two
/// around each tick.
#[derive(Schema, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[hearth(crate = "hearth")]
pub struct AvatarPos {
    pub pos: [f32; 2],
}

impl From<&AvatarPos> for arena_logic::Avatar {
    fn from(avatar: &AvatarPos) -> Self {
        Self { pos: avatar.pos }
    }
}

impl From<arena_logic::Avatar> for AvatarPos {
    fn from(avatar: arena_logic::Avatar) -> Self {
        Self { pos: avatar.pos }
    }
}

/// The synced state: what every client sees.
#[derive(HearthState, Serialize, Deserialize)]
#[hearth(crate = "hearth")]
pub struct Arena {
    #[sync]
    pub avatars: SyncMap<PlayerId, AvatarPos>,
    /// Display-only pickups; moving them is left to a real game.
    #[sync]
    pub coins: Vec<[f32; 2]>,
}

impl Default for Arena {
    fn default() -> Self {
        Self {
            avatars: SyncMap::default(),
            coins: vec![[5.0, 5.0], [15.0, 5.0], [10.0, 15.0]],
        }
    }
}

/// The one intent: a direction for the next tick.
#[derive(Schema, Deserialize)]
#[hearth(crate = "hearth")]
pub enum ArenaIntent {
    Move { dir: [i8; 2] },
}

/// The room struct holds the tick's buffered moves. It resets on a wake, which is
/// correct: a hibernated room is not ticking, so there is nothing to carry over.
#[derive(Default)]
pub struct ArenaRoom {
    moves: Vec<(PlayerId, Move)>,
}

#[hearth::room(mode = "ticked", tick_hz = 20, max_clients = 16, binding = "ARENA")]
impl Room for ArenaRoom {
    type State = Arena;
    type Intent = ArenaIntent;

    async fn on_join(&mut self, ctx: &mut Ctx<Self>, who: &Client, _opts: JoinOpts) -> Result<()> {
        // A deterministic spawn per seat: the sample's tests can predict it, and
        // two players never start on top of each other.
        let seat = usize::from(who.seat);
        let pos = [2.0 + (seat % 4) as f32 * 5.0, 2.0 + (seat / 4) as f32 * 5.0];
        ctx.state()
            .avatars
            .insert(who.player_id.clone(), AvatarPos { pos });
        Ok(())
    }

    async fn on_intent(
        &mut self,
        _ctx: &mut Ctx<Self>,
        who: &Client,
        intent: Self::Intent,
    ) -> Result<()> {
        let ArenaIntent::Move { dir } = intent;
        // One move per player per tick: the newest wins, like a held key.
        self.moves.retain(|(player, _)| player != &who.player_id);
        self.moves.push((who.player_id.clone(), Move { dir }));
        Ok(())
    }

    async fn on_leave(&mut self, ctx: &mut Ctx<Self>, who: &Client, _reason: Leave) -> Result<()> {
        ctx.state().avatars.remove(&who.player_id);
        self.moves.retain(|(player, _)| player != &who.player_id);
        Ok(())
    }

    async fn on_tick(&mut self, ctx: &mut Ctx<Self>, dt: Duration) -> Result<()> {
        let state = ctx.state();
        // The sim type is the client-visible shape, so prediction and the server
        // run the exact same function on the exact same data.
        let mut sim = ArenaState {
            avatars: state
                .avatars
                .iter()
                .map(|(player, avatar)| (player.0.clone(), avatar.into()))
                .collect(),
        };
        let moves = std::mem::take(&mut self.moves);
        arena_logic::step(&mut sim, &moves, dt.as_secs_f32());
        for (player, avatar) in sim.avatars {
            state.avatars.insert(PlayerId(player), avatar.into());
        }
        Ok(())
    }
}

hearth::admin::registry_object!();

fn room_routes() -> &'static [hearth_gateway::RoomRoute] {
    static ROUTES: [hearth_gateway::RoomRoute; 1] = [hearth_gateway::RoomRoute {
        room_type: "arena",
        binding: "ARENA",
        schema_hash: hearth::server::schema_hash_hex::<ArenaRoom>,
        max_clients: 16,
    }];
    &ROUTES
}

/// The gateway Worker entry: anonymous logins, room routing and the admin API.
#[worker::event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    if req.path().starts_with("/admin/") && hearth::admin::admin_token(&env).is_some() {
        return hearth::admin::serve_admin(req, env, room_routes()).await;
    }
    let config = match gateway_config(&env) {
        Ok(config) => config,
        Err(message) => return Response::error(message, 500),
    };
    hearth_gateway::serve(req, env, &config).await
}

fn gateway_config(env: &Env) -> std::result::Result<hearth_gateway::GatewayConfig, String> {
    Ok(hearth_gateway::GatewayConfig {
        rooms: room_routes(),
        player: None,
        matchmaker: false,
        auth: hearth_gateway::AuthConfig::from_env(env)?,
        limits: hearth_gateway::ratelimit::Limits::default(),
    })
}
