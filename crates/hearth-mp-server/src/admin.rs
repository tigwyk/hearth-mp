//! Admin and observability types: the read-only inspect document and the
//! unfiltered admin-tail message.
//!
//! These types cross two boundaries: the Durable Object's internal
//! `/__hearth/*` paths (`worker` feature) and `hearth-mp inspect` in the CLI.

use hearth_mp_protocol::{PatchOp, PlayerId, Value};
use serde::{Deserialize, Serialize};

use crate::host::RoomName;

/// One client seat as `hearth-mp inspect room` sees it. A held seat appears with
/// `connected: false` and its reconnect deadline.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct InspectClient {
    /// The seated player.
    pub player_id: PlayerId,
    /// Their seat.
    pub seat: u8,
    /// Whether a socket is open for them now.
    pub connected: bool,
    /// When their held seat expires, for a dropped player within the reconnect grace.
    pub held_until_ms: Option<u64>,
}

/// The read-only view of a room behind `GET /__hearth/inspect`.
///
/// `state` is the **full** server state, server-only and owner-scoped fields
/// included, and is `None` when the room's persisted state is incompatible with
/// this build.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InspectDoc {
    /// Which room.
    pub room: RoomName,
    /// When it was created, Unix ms.
    pub created_ms: u64,
    /// The current state sequence number.
    pub seq: u64,
    /// The ticked-mode tick number, `0` in event mode (`hearth_meta.tick`).
    pub tick: u64,
    /// Whether the room refuses new players.
    pub locked: bool,
    /// Whether the persisted state could not be read by this build.
    pub incompatible: bool,
    /// Every seated player.
    pub clients: Vec<InspectClient>,
    /// Every scheduled timer as `(key, due_ms)`, ordered by key.
    pub timers: Vec<(String, u64)>,
    /// The unfiltered state, when it could be read.
    pub state: Option<Value>,
    /// Length in bytes of the persisted state snapshot.
    pub state_bytes: usize,
}

/// Why one handler pass changed the unfiltered full state.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum Cause {
    /// A player joined.
    Join {
        /// Who.
        player: PlayerId,
    },
    /// A dropped player came back within the grace period.
    Reconnect {
        /// Who.
        player: PlayerId,
    },
    /// A player's intent.
    Intent {
        /// Who sent it.
        player: PlayerId,
        /// Its sequence number.
        intent_seq: u32,
    },
    /// A player left.
    Leave {
        /// Who.
        player: PlayerId,
    },
    /// A player's socket dropped; the seat is held.
    Disconnect {
        /// Who.
        player: PlayerId,
    },
    /// A timer fired.
    Timer {
        /// Its key.
        key: String,
    },
    /// Another object called this room (`ctx.call` / `ctx.notify`).
    Call {
        /// The calling object.
        from: crate::call::Target,
    },
    /// The room is being disposed.
    Dispose,
}

/// One admin-tail frame: the unfiltered full-state diff one pass produced,
/// labelled with what caused it. `rejected` lists the intent seqs that failed in
/// this pass.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TailMsg {
    /// The state sequence number after the pass.
    pub seq: u64,
    /// What caused the pass.
    pub cause: Vec<Cause>,
    /// The unfiltered diff.
    pub ops: Vec<PatchOp>,
    /// Intent seqs that failed in this pass.
    pub rejected: Vec<u32>,
}
