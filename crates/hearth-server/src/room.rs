use std::time::Duration;

use hearth_protocol::{PlayerId, Value};
use hearth_sync::HearthState;
use serde::de::DeserializeOwned;

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
}

/// A joined client. Identity comes from the gateway, never from the client's
/// own headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    pub player_id: PlayerId,
    pub seat: u8,
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
    type Intent: DeserializeOwned;

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

    async fn on_timer(&mut self, _ctx: &mut crate::Ctx<Self>, _key: TimerKey) -> Result<()> {
        Ok(())
    }

    async fn on_dispose(&mut self, _ctx: &mut crate::Ctx<Self>) -> Result<()> {
        Ok(())
    }
}
