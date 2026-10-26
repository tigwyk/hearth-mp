//! The conformance room `Conf`: small, but it exercises every server behaviour
//! the scenarios check (seats, owner-only fields, events, rejections, the
//! forward-compatibility injection).

use std::time::Duration;

use hearth_protocol::PlayerId;
use hearth_server::{Client, Ctx, JoinOpts, Leave, Mode, Result, Room, RoomConfig};
use hearth_sync::{HearthState, Schema, SyncMap};
use serde::{Deserialize, Serialize};

/// The marker event the host replaces with the `forward.json` frames.
pub const FUTURE_EVENT: &str = "conf.future";

#[derive(HearthState, Serialize, Deserialize, Default, Debug)]
pub struct ConfState {
    #[sync]
    pub n: i64,
    #[sync]
    pub seats: SyncMap<PlayerId, u8>,
    #[sync(owner)]
    pub secret: SyncMap<PlayerId, String>,
}

#[derive(Deserialize, Schema, Debug)]
pub enum ConfIntent {
    /// Add to `n`.
    Add(i64),
    /// Set this player's owner-only secret.
    SetSecret(String),
    /// Send `EVENT "echo"` with the text back to the sender.
    Echo(String),
    /// Always rejected (`INTENT_REJECTED`).
    Fail,
    /// The host sends the `forward.json` frames marked `inject` to the sender.
    Future,
}

#[derive(Default)]
pub struct Conf;

impl RoomConfig for Conf {
    const MODE: Mode = Mode::Event;
    const MAX_CLIENTS: u8 = 2;
    // Short, so the reconnect scenario and stale seats stay fast.
    const RECONNECT_GRACE: Duration = Duration::from_secs(5);
}

impl Room for Conf {
    type State = ConfState;
    type Intent = ConfIntent;

    async fn on_join(&mut self, ctx: &mut Ctx<Self>, who: &Client, _opts: JoinOpts) -> Result<()> {
        ctx.state().seats.insert(who.player_id.clone(), who.seat);
        Ok(())
    }

    async fn on_intent(&mut self, ctx: &mut Ctx<Self>, who: &Client, intent: ConfIntent) -> Result<()> {
        match intent {
            ConfIntent::Add(amount) => ctx.state().n += amount,
            ConfIntent::SetSecret(text) => {
                ctx.state().secret.insert(who.player_id.clone(), text);
            }
            ConfIntent::Echo(text) => ctx.send(&who.player_id, "echo", text),
            ConfIntent::Fail => anyhow::bail!("Fail is always rejected"),
            ConfIntent::Future => ctx.send(&who.player_id, FUTURE_EVENT, ()),
        }
        Ok(())
    }

    async fn on_leave(&mut self, ctx: &mut Ctx<Self>, who: &Client, _reason: Leave) -> Result<()> {
        ctx.state().seats.remove(&who.player_id);
        ctx.state().secret.remove(&who.player_id);
        Ok(())
    }
}
