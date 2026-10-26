use std::rc::Rc;

use hearth_mp_protocol::{PlayerId, Value, WireMsg};
use serde::{Deserialize, Serialize};

use crate::Result;

/// One SQL value, mapped 1:1 onto SQLite storage types.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    /// SQL `NULL`.
    Null,
    /// A 64-bit integer.
    Int(i64),
    /// A 64-bit float.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Raw bytes.
    Blob(Vec<u8>),
}

/// The room's SQLite database. The runtime creates its own tables; dev code may
/// create and use its own through [`crate::Ctx::sql`].
pub trait Sql {
    /// Run one statement with positional `?` parameters; rows come back as JSON objects by column name.
    fn exec(
        &self,
        query: &str,
        params: Vec<SqlValue>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>>;
}

/// The provider every unauthenticated client gets. The gateway namespaces
/// player ids per provider (`anon_`, `jwt_`, ...); this is the room's default.
pub const ANON_PROVIDER: &str = "anon";

/// Per-socket data that survives hibernation. Stored in the WebSocket
/// attachment, so rebuilding a view never needs a storage read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// The player this socket belongs to.
    pub player_id: PlayerId,
    /// The seat, once joined.
    pub seat: Option<u8>,
    /// The encoding chosen by the socket's `HELLO`, once received.
    pub encoding: Option<hearth_mp_protocol::Encoding>,
    /// An admin tail socket: never a seat, never a client, never keeps the room
    /// alive. Defaulted so attachments written before v0.2c still decode.
    #[serde(default)]
    pub admin: bool,
    /// The gateway-verified provider (`"anon"`, `"jwt"`, `"steam"`, ...).
    /// Defaulted to anonymous so attachments written before v0.3b still decode.
    #[serde(default = "anon_provider")]
    pub provider: String,
    /// The gateway-verified display name, if the provider supplied one.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// The serde default for [`Attachment::provider`].
fn anon_provider() -> String {
    ANON_PROVIDER.to_owned()
}

/// A player identity verified by the gateway before the connect. Rooms never
/// see raw credentials: only this reaches the runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The verified player identity.
    pub player: PlayerId,
    /// The login provider: `"anon"`, `"jwt"`, `"steam"`, `"itch"` or `"discord"`.
    pub provider: String,
    /// The provider's display name, when it gave one.
    pub display_name: Option<String>,
}

impl Session {
    /// The identity of a client that was never authenticated: provider
    /// `"anon"` and no display name.
    pub fn anon(player: PlayerId) -> Self {
        Self {
            player,
            provider: ANON_PROVIDER.to_owned(),
            display_name: None,
        }
    }
}

/// A Durable Object room's identity: its type (`"tictactoe"`) and its name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RoomName {
    /// The room type, for example `"tictactoe"`.
    pub room_type: String,
    /// The room's name within its type.
    pub name: String,
}

/// What a room tells the registry and the matchmaker about itself.
///
/// `players` is the seated roster, live and held across a reconnect: the count
/// is its length. `metadata` is the room's `ctx.set_metadata` value, which the
/// matchmaker echoes in its lobby listing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RoomReport {
    /// The room exists and accepts players.
    Open {
        /// Which room.
        room: RoomName,
        /// When it was created, Unix ms.
        created_ms: u64,
        /// The seated roster, live and held.
        players: Vec<PlayerId>,
        /// Its seat count.
        capacity: u8,
        /// Whether it refuses new players.
        locked: bool,
        /// The room's `ctx.set_metadata` value.
        metadata: Value,
    },
    /// The roster or lock changed.
    Players {
        /// Which room.
        room: RoomName,
        /// The seated roster, live and held.
        players: Vec<PlayerId>,
        /// Its seat count.
        capacity: u8,
        /// Whether it refuses new players.
        locked: bool,
        /// The room's `ctx.set_metadata` value.
        metadata: Value,
    },
    /// The room was disposed.
    Disposed {
        /// Which room.
        room: RoomName,
    },
}

/// Everything the runtime needs from its environment. The production
/// implementation wraps a hibernating Durable Object (`worker` feature); the
/// test harness implements it over an in-memory SQLite database and sockets.
#[allow(async_fn_in_trait)]
pub trait Host: crate::__private::Sealed {
    /// The host's socket handle.
    type Socket: Clone + PartialEq;

    /// Every open socket.
    fn sockets(&self) -> Vec<Self::Socket>;
    /// A socket's attachment, if it has one.
    fn attachment(&self, socket: &Self::Socket) -> Option<Attachment>;
    /// Store a socket's attachment (it survives hibernation).
    fn set_attachment(&self, socket: &Self::Socket, attachment: &Attachment);
    /// Send one frame; a socket that is gone accepts nothing.
    fn send(&self, socket: &Self::Socket, msg: WireMsg);
    /// Close a socket with a close code and reason.
    fn close(&self, socket: &Self::Socket, code: u16, reason: &str);
    /// The clock, Unix milliseconds.
    fn now_ms(&self) -> u64;
    /// The room's SQLite storage.
    fn sql(&self) -> &dyn Sql;
    /// An owned SQL handle for [`crate::Ctx::sql`]. `Ctx` outlives the runtime
    /// borrow that produced it, so the handle is reference-counted.
    fn sql_handle(&self) -> Rc<dyn Sql>;

    /// Set (or with `None` clear) the single alarm.
    async fn set_alarm(&self, at_ms: Option<u64>) -> Result<()>;
    /// Delete all of the room's storage (disposal).
    async fn wipe(&self) -> Result<()>;

    /// Send one object-to-object call to `to` and return its reply.
    ///
    /// Implementations map a network failure or a non-200 answer to
    /// [`CallError::Unreachable`]; a receiver-side failure comes back decoded.
    async fn call(
        &self,
        _to: &crate::call::Target,
        _envelope: crate::call::Envelope,
    ) -> std::result::Result<hearth_mp_protocol::Value, crate::call::CallError> {
        Err(crate::call::CallError::Unreachable(
            "this host cannot make object-to-object calls".to_owned(),
        ))
    }

    /// Whether the deployment declares this Durable Object binding. Reports to
    /// an optional binding are only enqueued when it exists.
    fn has_binding(&self, _binding: &str) -> bool {
        false
    }

    /// Whether the adapter drives ticks from an in-object loop instead of the
    /// alarm (the production Durable Object adapter does; the test harness and
    /// other hosts keep the alarm driver). A loop host leaves the tick deadline
    /// out of the alarm schedule and relies on the tick watchdog alarm instead.
    fn loops_ticks(&self) -> bool {
        false
    }

    /// Append one structured JSON line to the deployment's logs.
    fn log_line(&self, _json: &str) {}

    /// Count one unfiltered full-state render done for the admin tail. Tests
    /// override it to prove that a room with no admin socket does no extra work.
    fn count_full_state(&self) {}
}
