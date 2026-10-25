use serde::{Deserialize, Serialize};

use crate::Value;

/// Protocol version exchanged in `HELLO`. A mismatch is a hard reject.
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximum incoming frame size, checked before any deserialisation.
pub const MAX_FRAME_BYTES: usize = 65536;

/// Keepalive request text frame, answered by the Durable Object auto-response.
pub const PING_TEXT: &str = "hearth:ping";

/// Keepalive response text frame.
pub const PONG_TEXT: &str = "hearth:pong";

/// Stable player identity. Always a string on the wire; never a number.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlayerId(pub String);

impl From<&str> for PlayerId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for PlayerId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for PlayerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PlayerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One-byte opcode that prefixes every binary frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Opcode {
    Hello = 0x01,
    JoinOk = 0x02,
    FullState = 0x03,
    Patch = 0x04,
    Intent = 0x05,
    Event = 0x06,
    Ack = 0x07,
    /// Reserved. The keepalive is the [`PING_TEXT`] text frame, not this opcode.
    Ping = 0x08,
    Error = 0x09,
    Leave = 0x0A,
}

impl Opcode {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(Self::Hello),
            0x02 => Some(Self::JoinOk),
            0x03 => Some(Self::FullState),
            0x04 => Some(Self::Patch),
            0x05 => Some(Self::Intent),
            0x06 => Some(Self::Event),
            0x07 => Some(Self::Ack),
            0x08 => Some(Self::Ping),
            0x09 => Some(Self::Error),
            0x0A => Some(Self::Leave),
            _ => None,
        }
    }
}

/// Frame encoding for one socket. Chosen by the client's `HELLO` frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoding {
    MsgPack,
    Json,
}

/// An encoded frame: MsgPack frames are binary, JSON frames are text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireMsg {
    Binary(Vec<u8>),
    Text(String),
}

/// Client-to-server intent with a client-assigned sequence number.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IntentMsg {
    pub seq: u32,
    pub intent: Value,
}

/// Protocol error code carried by an `ERROR` frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    BadFrame,
    UnsupportedProtocol,
    SchemaMismatch,
    RoomFull,
    RoomLocked,
    NotJoined,
    BadIntent,
    IntentRejected,
    JoinRejected,
    RateLimited,
    FrameTooLarge,
    StateIncompatible,
    Internal,
}

/// Every frame on the wire. The `op` tag is the variant name in
/// SCREAMING_SNAKE_CASE; binary frames additionally carry the opcode byte.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Frame {
    Hello {
        protocol: u16,
        schema_hash: String,
        opts: Value,
    },
    JoinOk {
        player_id: PlayerId,
        seat: u8,
    },
    FullState {
        seq: u64,
        state: Value,
    },
    Patch {
        seq: u64,
        ops: Vec<PatchOp>,
    },
    Intent {
        intents: Vec<IntentMsg>,
    },
    Event {
        name: String,
        data: Value,
    },
    Ack {
        state_seq: u64,
        intent_seqs: Vec<u32>,
    },
    Error {
        code: ErrorCode,
        message: String,
        ref_seq: Option<u32>,
    },
    Leave {},
}

/// One state mutation. Path segments are strings (map key or field) or
/// integers (list index).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchOp {
    /// Replace the value at `path`. An empty path replaces the root.
    Set { path: Vec<Value>, value: Value },
    /// Remove a map key or list index at `path`.
    Del { path: Vec<Value> },
    /// Insert or replace the string `key` in the map at `path`.
    MapInsert {
        path: Vec<Value>,
        key: String,
        value: Value,
    },
    /// Splice `insert` into the list at `path`, deleting `delete` values at `index`.
    ListSplice {
        path: Vec<Value>,
        index: u32,
        delete: u32,
        insert: Vec<Value>,
    },
}

/// WebSocket close codes used by the runtime.
pub mod close {
    pub const BAD_FRAME: u16 = 4000;
    pub const SCHEMA_MISMATCH: u16 = 4001;
    pub const ROOM_FULL: u16 = 4002;
    pub const ROOM_LOCKED: u16 = 4003;
    pub const KICKED: u16 = 4004;
    pub const RATE_LIMITED: u16 = 4005;
    pub const FRAME_TOO_LARGE: u16 = 4006;
    pub const STATE_INCOMPATIBLE: u16 = 4007;
    pub const DISPOSED: u16 = 4008;
    pub const REPLACED: u16 = 4009;
    pub const JOIN_REJECTED: u16 = 4010;
}
