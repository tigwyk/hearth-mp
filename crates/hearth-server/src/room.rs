use std::time::Duration;

use hearth_protocol::{PlayerId, Value};
use hearth_sync::{HearthState, Schema};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::call::Target;
use crate::Result;

/// How a room runs. v0.1 ships event mode only; ticked mode arrives in v0.4.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Wakes per message or alarm and hibernates between.
    Event,
}

/// Compile-time room configuration, emitted by `#[hearth::room]`.
pub trait RoomConfig {
    const MODE: Mode;
    const MAX_CLIENTS: u8;
    const EMPTY_GRACE: Duration = Duration::from_secs(60);
    /// How long a dropped player's seat stays held. Set it with the
    /// `#[hearth::room(reconnect_grace_secs = N)]` attribute; `0` restores the
    /// v0.1 behaviour of calling `on_leave(Closed)` at once.
    const RECONNECT_GRACE: Duration = Duration::from_secs(30);
    /// Intent tokens refilled per second, per player. Set it with the
    /// `#[hearth::room(intent_rate = N)]` attribute; at least 1.
    const INTENT_RATE: u32 = 60;
    /// The per-player intent bucket's capacity: how many intents can arrive in
    /// one burst. Set it with the `#[hearth::room(intent_burst = N)]`
    /// attribute; at least 1.
    const INTENT_BURST: u32 = 120;
}

/// One client. Identity comes from the gateway, never from the client's own
/// headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    pub player_id: PlayerId,
    pub seat: u8,
    /// Whether the client's socket is live. A held seat appears in
    /// [`crate::Ctx::clients`] with `connected: false`.
    pub connected: bool,
    /// What the gateway verified: the provider (`"anon"`, `"jwt"`, ...). Rooms
    /// never see raw credentials, only this mirror.
    pub provider: String,
    /// What the gateway verified: the provider's display name, if any.
    pub display_name: Option<String>,
}

/// Opaque options handed to `on_join`, forwarded from `HELLO.opts`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JoinOpts(pub Value);

/// Why a client left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leave {
    /// The socket disappeared without a `LEAVE` frame.
    Closed,
    /// The client sent `LEAVE`.
    Left,
    /// The room, or a limit, removed the client.
    Kicked,
}

/// Key for a durable timer. Timers are multiplexed onto the single DO alarm.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TimerKey(pub String);

impl From<&str> for TimerKey {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for TimerKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// One room type. Implement this and add `#[hearth::room(...)]` to the impl.
///
/// `self` is rebuilt from [`Default`] after every hibernation, so keep all
/// durable data in `State`, not in the struct's fields.
#[allow(async_fn_in_trait)]
pub trait Room: RoomConfig + Default + 'static {
    type State: HearthState;
    type Intent: Schema + DeserializeOwned;

    async fn on_join(
        &mut self,
        ctx: &mut crate::Ctx<Self>,
        who: &Client,
        opts: JoinOpts,
    ) -> Result<()>;

    async fn on_intent(
        &mut self,
        ctx: &mut crate::Ctx<Self>,
        who: &Client,
        intent: Self::Intent,
    ) -> Result<()>;

    async fn on_leave(
        &mut self,
        ctx: &mut crate::Ctx<Self>,
        who: &Client,
        reason: Leave,
    ) -> Result<()>;

    /// The socket dropped and the seat is now held for
    /// [`RoomConfig::RECONNECT_GRACE`]. `on_leave` has not run.
    async fn on_disconnect(&mut self, _ctx: &mut crate::Ctx<Self>, _who: &Client) -> Result<()> {
        Ok(())
    }

    /// A held seat resumed: the client got a fresh `FULL_STATE` and `on_join`
    /// did not run.
    async fn on_reconnect(&mut self, _ctx: &mut crate::Ctx<Self>, _who: &Client) -> Result<()> {
        Ok(())
    }

    async fn on_timer(&mut self, _ctx: &mut crate::Ctx<Self>, _key: TimerKey) -> Result<()> {
        Ok(())
    }

    async fn on_dispose(&mut self, _ctx: &mut crate::Ctx<Self>) -> Result<()> {
        Ok(())
    }
}

/// A room that also accepts object-to-object calls.
///
/// Write it as its own impl block and add the `rpc` flag to the room attribute:
///
/// ```ignore
/// #[hearth::room(mode = "event", max_clients = 2, binding = "TICTACTOE", rpc)]
/// impl Room for TicTacToe { /* ... */ }
///
/// impl RoomRpc for TicTacToe {
///     type Rpc = TttRpc;
///     type Reply = Value;
///     async fn on_call(&mut self, ctx: &mut Ctx<Self>, from: &Target, rpc: TttRpc) -> Result<Value> { /* ... */ }
/// }
/// ```
///
/// `on_call` runs as a normal handler pass, so whatever it changes is patched
/// to the room's clients.
#[allow(async_fn_in_trait)]
pub trait RoomRpc: Room {
    type Rpc: Serialize + DeserializeOwned;
    type Reply: Serialize + DeserializeOwned;

    async fn on_call(
        &mut self,
        ctx: &mut crate::Ctx<Self>,
        from: &Target,
        rpc: Self::Rpc,
    ) -> Result<Self::Reply>;
}
