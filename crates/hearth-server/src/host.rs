use std::rc::Rc;

use hearth_protocol::{PlayerId, WireMsg};
use serde::{Deserialize, Serialize};

use crate::Result;

/// One SQL value, mapped 1:1 onto SQLite storage types.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// The room's SQLite database. The runtime creates its own tables; dev code may
/// create and use its own through [`crate::Ctx::sql`].
pub trait Sql {
    fn exec(
        &self,
        query: &str,
        params: Vec<SqlValue>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>>;
}

/// Per-socket data that survives hibernation. Stored in the WebSocket
/// attachment, so rebuilding a view never needs a storage read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub player_id: PlayerId,
    pub seat: Option<u8>,
    pub encoding: Option<hearth_protocol::Encoding>,
    /// An admin tail socket: never a seat, never a client, never keeps the room
    /// alive. Defaulted so attachments written before v0.2c still decode.
    #[serde(default)]
    pub admin: bool,
}

/// A Durable Object room's identity: its type (`"tictactoe"`) and its name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RoomName {
    pub room_type: String,
    pub name: String,
}

/// What a room tells the registry about itself. `players` counts seated players,
/// both live and held across a reconnect.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoomReport {
    Open {
        room: RoomName,
        created_ms: u64,
        players: u8,
    },
    Players {
        room: RoomName,
        players: u8,
    },
    Disposed {
        room: RoomName,
    },
}

/// Everything the runtime needs from its environment. The production
/// implementation wraps a hibernating Durable Object (`worker` feature); the
/// test harness implements it over an in-memory SQLite database and sockets.
#[allow(async_fn_in_trait)]
pub trait Host {
    type Socket: Clone + PartialEq;

    fn sockets(&self) -> Vec<Self::Socket>;
    fn attachment(&self, socket: &Self::Socket) -> Option<Attachment>;
    fn set_attachment(&self, socket: &Self::Socket, attachment: &Attachment);
    fn send(&self, socket: &Self::Socket, msg: WireMsg);
    fn close(&self, socket: &Self::Socket, code: u16, reason: &str);
    fn now_ms(&self) -> u64;
    fn sql(&self) -> &dyn Sql;
    /// An owned SQL handle for [`crate::Ctx::sql`]. `Ctx` outlives the runtime
    /// borrow that produced it, so the handle is reference-counted.
    fn sql_handle(&self) -> Rc<dyn Sql>;

    async fn set_alarm(&self, at_ms: Option<u64>) -> Result<()>;
    async fn wipe(&self) -> Result<()>;

    /// Send one object-to-object call to `to` and return its reply.
    ///
    /// Implementations map a network failure or a non-200 answer to
    /// [`CallError::Unreachable`]; a receiver-side failure comes back decoded.
    async fn call(
        &self,
        _to: &crate::call::Target,
        _envelope: crate::call::Envelope,
    ) -> std::result::Result<hearth_protocol::Value, crate::call::CallError> {
        Err(crate::call::CallError::Unreachable(
            "this host cannot make object-to-object calls".to_owned(),
        ))
    }

    /// Whether the deployment declares this Durable Object binding. Reports to
    /// an optional binding are only enqueued when it exists.
    fn has_binding(&self, _binding: &str) -> bool {
        false
    }

    /// Append one structured JSON line to the deployment's logs.
    fn log_line(&self, _json: &str) {}

    /// Count one unfiltered full-state render done for the admin tail. Tests
    /// override it to prove that a room with no admin socket does no extra work.
    fn count_full_state(&self) {}
}
