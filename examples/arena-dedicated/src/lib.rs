//! Hearth v0.5 sample: a dedicated-server lobby.
//!
//! `ArenaLobby` is an ordinary event-mode room. When at least two players are
//! ready it calls `ctx.handoff`; the adapter asks the configured allocator for a
//! dedicated server and each player receives a private `hearth.handoff` event
//! with its endpoint and single-use ticket. The dedicated server redeems the
//! tickets and reports the match lifecycle back, which the lobby records and
//! forwards to the player stats service.
//!
//! Two allocators are wired for the same room:
//!
//! - **Cloudflare Containers** (default): the session's `ArenaServer` object owns
//!   a container built from `server/Dockerfile`, and players connect through the
//!   gateway's `handoff/{sid}/connect` proxy.
//! - **Webhook fleet**: set `HEARTH_ALLOCATOR_MODE = "webhook"` (and the
//!   `HEARTH_ALLOCATOR_*` vars below) to use `fleet/allocator.mjs` and a
//!   dedicated server on `udp://`/`wss://`, as `e2e-fleet.mjs` does.

use hearth::sync::SyncMap;
use hearth::{
    Client, Ctx, HearthState, JoinOpts, Leave, PlayerId, PlayerService, Result, Room, Schema,
    Service, ServiceCtx,
};
use serde::{Deserialize, Serialize};
use worker::{Context, Env, Request, Response};

// The container Durable Object class: one instance per handoff session, each
// owning one Cloudflare Container. `[[containers]]` in `wrangler.toml` must name
// this class, and the `ARENA_SERVER` binding must point at it.
hearth::container_object!(ArenaServer);

/// The synced lobby state: who is ready, the lifecycle log and the last result.
#[derive(HearthState, Serialize, Deserialize, Default)]
#[hearth(crate = "hearth")]
pub struct LobbyState {
    #[sync]
    pub ready: SyncMap<PlayerId, bool>,
    /// `ready`, `started`, `player_joined`, `result`, `ended`, `failed`: one
    /// entry per handoff event, so clients and e2e can follow the lifecycle.
    #[sync]
    pub log: Vec<String>,
    /// The last `Result` data as compact JSON (empty before the first result).
    #[sync]
    pub result: String,
    /// Whether a handoff session is currently running; blocks a second start.
    #[sync]
    pub in_session: bool,
}

/// The only intents: mark yourself ready or not.
#[derive(Schema, Deserialize)]
#[hearth(crate = "hearth")]
pub enum LobbyIntent {
    Ready,
    Unready,
}

#[derive(Default)]
pub struct ArenaLobby;

impl ArenaLobby {
    /// Start a handoff when every connected player is ready and at least two
    /// are seated. The room passes the whole lobby; the dedicated server is
    /// expected to be the match host.
    fn maybe_start(ctx: &mut Ctx<Self>) {
        if ctx.state().in_session {
            return;
        }
        let players: Vec<PlayerId> = ctx
            .clients()
            .iter()
            .filter(|client| client.connected)
            .map(|client| client.player_id.clone())
            .collect();
        if players.len() < 2 {
            return;
        }
        let ready = &ctx.state().ready;
        if !players
            .iter()
            .all(|player| ready.get(player).copied().unwrap_or(false))
        {
            return;
        }
        ctx.state().in_session = true;
        ctx.handoff(hearth::server::handoff::HandoffRequest::new(
            players,
            serde_json::json!({"mode": "ffa", "map": "arena"}),
        ));
    }
}

#[hearth::room(
    mode = "event",
    max_clients = 8,
    binding = "ARENA_LOBBY",
    allocator = "container",
    container = "ARENA_SERVER"
)]
impl Room for ArenaLobby {
    type State = LobbyState;
    type Intent = LobbyIntent;

    async fn on_join(&mut self, ctx: &mut Ctx<Self>, who: &Client, _opts: JoinOpts) -> Result<()> {
        ctx.state().ready.insert(who.player_id.clone(), false);
        Ok(())
    }

    async fn on_intent(
        &mut self,
        ctx: &mut Ctx<Self>,
        who: &Client,
        intent: LobbyIntent,
    ) -> Result<()> {
        match intent {
            LobbyIntent::Ready => {
                ctx.state().ready.insert(who.player_id.clone(), true);
            }
            LobbyIntent::Unready => {
                ctx.state().ready.insert(who.player_id.clone(), false);
            }
        }
        Self::maybe_start(ctx);
        Ok(())
    }

    async fn on_leave(&mut self, ctx: &mut Ctx<Self>, who: &Client, _reason: Leave) -> Result<()> {
        ctx.state().ready.remove(&who.player_id);
        Ok(())
    }

    async fn on_handoff(
        &mut self,
        ctx: &mut Ctx<Self>,
        _sid: &hearth::server::handoff::SessionId,
        event: hearth::server::handoff::HandoffEvent,
    ) -> Result<()> {
        let name = match &event {
            hearth::server::handoff::HandoffEvent::Ready { .. } => "ready",
            hearth::server::handoff::HandoffEvent::Started => "started",
            hearth::server::handoff::HandoffEvent::PlayerJoined(_) => "player_joined",
            hearth::server::handoff::HandoffEvent::PlayerLeft(_) => "player_left",
            hearth::server::handoff::HandoffEvent::Result(_) => "result",
            hearth::server::handoff::HandoffEvent::Ended { .. } => "ended",
            hearth::server::handoff::HandoffEvent::Failed { .. } => "failed",
            _ => "other",
        };
        ctx.state().log.push(name.to_owned());
        match event {
            hearth::server::handoff::HandoffEvent::Result(data) => {
                ctx.state().result = serde_json::to_string(&data)?;
                // Forward the outcome to the winner's player object (v0.3a),
                // durably: the outbox survives a handler failure after this.
                if let Some(winner) = data.get("winner").and_then(|value| value.as_str()) {
                    ctx.notify::<ArenaPlayer>(winner, PlayerRpc::Result { won: true });
                }
            }
            hearth::server::handoff::HandoffEvent::Ended { .. }
            | hearth::server::handoff::HandoffEvent::Failed { .. } => {
                let state = ctx.state();
                state.in_session = false;
                state.ready.clear();
            }
            _ => {}
        }
        Ok(())
    }
}

/// One player's lifetime stats, kept in the player Durable Object.
#[derive(Serialize, Deserialize, Default)]
pub struct PlayerStats {
    pub wins: u32,
}

/// What a room reports to the player object.
#[derive(Serialize, Deserialize)]
pub enum PlayerRpc {
    Result { won: bool },
}

#[derive(Default)]
pub struct ArenaPlayer;

#[hearth::service(binding = "ARENA_PLAYER", player)]
impl Service for ArenaPlayer {
    type State = PlayerStats;
    type Rpc = PlayerRpc;
    type Reply = ();

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &hearth::Target,
        rpc: PlayerRpc,
    ) -> Result<()> {
        let PlayerRpc::Result { won } = rpc;
        if won {
            ctx.state().wins += 1;
        }
        Ok(())
    }
}

impl PlayerService for ArenaPlayer {
    /// The player sees only their own record.
    fn client_view(state: &Self::State) -> serde_json::Value {
        serde_json::to_value(state).expect("PlayerStats is JSON")
    }
}

/// Every room type this Worker serves.
fn room_routes() -> &'static [hearth_gateway::RoomRoute] {
    static ROUTES: [hearth_gateway::RoomRoute; 1] = [hearth_gateway::RoomRoute::new(
        "arena",
        "ARENA_LOBBY",
        hearth::server::schema_hash_hex::<ArenaLobby>,
        8,
    )
    .with_container("ARENA_SERVER")];
    &ROUTES
}

fn gateway_config(env: &Env) -> std::result::Result<hearth_gateway::GatewayConfig, String> {
    Ok(hearth_gateway::GatewayConfig::new(room_routes())
        .player(hearth_gateway::PlayerRoute::new("ARENA_PLAYER"))
        .auth(hearth_gateway::AuthConfig::from_env(env)?))
}

/// The gateway Worker entry: auth, routing and the lobby room route.
#[worker::event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    let config = match gateway_config(&env) {
        Ok(config) => config,
        Err(message) => return Response::error(message, 500),
    };
    hearth_gateway::serve(req, env, &config).await
}
