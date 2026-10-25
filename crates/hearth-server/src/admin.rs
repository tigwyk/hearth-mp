//! Admin and observability types: the read-only inspect document and the
//! unfiltered admin-tail message.
//!
//! These types cross two boundaries: the Durable Object's internal
//! `/__hearth/*` paths (`worker` feature) and `hearth inspect` in the CLI.

use hearth_protocol::{PatchOp, PlayerId, Value};
use serde::{Deserialize, Serialize};

use crate::host::RoomName;

/// One client seat as `hearth inspect room` sees it. A held seat appears with
/// `connected: false` and its reconnect deadline.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct InspectClient {
    pub player_id: PlayerId,
    pub seat: u8,
    pub connected: bool,
    pub held_until_ms: Option<u64>,
}

/// The read-only view of a room behind `GET /__hearth/inspect`.
///
/// `state` is the **full** server state, server-only and owner-scoped fields
/// included, and is `None` when the room's persisted state is incompatible with
/// this build.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InspectDoc {
    pub room: RoomName,
    pub created_ms: u64,
    pub seq: u64,
    pub locked: bool,
    pub incompatible: bool,
    pub clients: Vec<InspectClient>,
    /// Every scheduled timer as `(key, due_ms)`, ordered by key.
    pub timers: Vec<(String, u64)>,
    pub state: Option<Value>,
    /// Length in bytes of the persisted state snapshot.
    pub state_bytes: usize,
}

/// Why one handler pass changed the unfiltered full state.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum Cause {
    Join { player: PlayerId },
    Reconnect { player: PlayerId },
    Intent { player: PlayerId, intent_seq: u32 },
    Leave { player: PlayerId },
    Disconnect { player: PlayerId },
    Timer { key: String },
    Dispose,
}

/// One admin-tail frame: the unfiltered full-state diff one pass produced,
/// labelled with what caused it. `rejected` lists the intent seqs that failed in
/// this pass.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TailMsg {
    pub seq: u64,
    pub cause: Vec<Cause>,
    pub ops: Vec<PatchOp>,
    pub rejected: Vec<u32>,
}
