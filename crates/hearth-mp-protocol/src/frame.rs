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
    /// `HELLO`: client handshake; selects the socket's encoding.
    Hello = 0x01,
    /// `JOIN_OK`: join accepted, seat assigned.
    JoinOk = 0x02,
    /// `FULL_STATE`: complete state snapshot.
    FullState = 0x03,
    /// `PATCH`: incremental state change.
    Patch = 0x04,
    /// `INTENT`: one or more player intents.
    Intent = 0x05,
    /// `EVENT`: out-of-band event.
    Event = 0x06,
    /// `ACK`: intent acknowledgement.
    Ack = 0x07,
    /// Reserved. The keepalive is the [`PING_TEXT`] text frame, not this opcode.
    Ping = 0x08,
    /// `ERROR`: an error, optionally tied to one intent.
    Error = 0x09,
    /// `LEAVE`: voluntary leave.
    Leave = 0x0A,
}

impl Opcode {
    /// The opcode byte.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// The opcode for a byte, or `None` for a byte this version does not know.
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
    /// Binary frames: an opcode byte plus named MessagePack.
    MsgPack,
    /// Text frames: the frame object as JSON.
    Json,
}

/// An encoded frame: MsgPack frames are binary, JSON frames are text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireMsg {
    /// A binary WebSocket message (MessagePack mode).
    Binary(Vec<u8>),
    /// A text WebSocket message (JSON mode, or a keepalive).
    Text(String),
}

/// Client-to-server intent with a client-assigned sequence number.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IntentMsg {
    /// Client-assigned sequence number: 1 for the first intent on a socket session, then +1 each.
    pub seq: u32,
    /// The room's intent value, in serde's external tagging.
    pub intent: Value,
}

/// Protocol error code carried by an `ERROR` frame. On the wire it is the
/// `SCREAMING_SNAKE_CASE` name; a name this version does not know decodes to
/// [`ErrorCode::Unknown`], so a newer server never breaks an older client
/// (`spec/protocol-v1.md` section 12).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCode {
    /// The frame could not be decoded.
    BadFrame,
    /// `HELLO.protocol` is not supported.
    UnsupportedProtocol,
    /// The schema hash differs from the room's.
    SchemaMismatch,
    /// No seat is left.
    RoomFull,
    /// The room refuses new players.
    RoomLocked,
    /// A frame arrived before a successful `HELLO`.
    NotJoined,
    /// The intent did not decode into the room's intent type.
    BadIntent,
    /// The room refused the intent; no state change was kept.
    IntentRejected,
    /// The room's `on_join` refused the player.
    JoinRejected,
    /// The per-player intent rate cap was exceeded.
    RateLimited,
    /// An incoming frame exceeded 64 KiB.
    FrameTooLarge,
    /// The persisted room state cannot be read by this build.
    StateIncompatible,
    /// An unexpected server error.
    Internal,
    /// A code added by a newer protocol revision; treat it as a generic error.
    Unknown(String),
}

impl ErrorCode {
    const NAMED: [(ErrorCode, &'static str); 13] = [
        (ErrorCode::BadFrame, "BAD_FRAME"),
        (ErrorCode::UnsupportedProtocol, "UNSUPPORTED_PROTOCOL"),
        (ErrorCode::SchemaMismatch, "SCHEMA_MISMATCH"),
        (ErrorCode::RoomFull, "ROOM_FULL"),
        (ErrorCode::RoomLocked, "ROOM_LOCKED"),
        (ErrorCode::NotJoined, "NOT_JOINED"),
        (ErrorCode::BadIntent, "BAD_INTENT"),
        (ErrorCode::IntentRejected, "INTENT_REJECTED"),
        (ErrorCode::JoinRejected, "JOIN_REJECTED"),
        (ErrorCode::RateLimited, "RATE_LIMITED"),
        (ErrorCode::FrameTooLarge, "FRAME_TOO_LARGE"),
        (ErrorCode::StateIncompatible, "STATE_INCOMPATIBLE"),
        (ErrorCode::Internal, "INTERNAL"),
    ];

    /// The wire name (`"BAD_FRAME"`), or the unknown code's own text.
    pub fn as_str(&self) -> &str {
        match self {
            ErrorCode::Unknown(name) => name,
            known => Self::NAMED
                .iter()
                .find(|(code, _)| code == known)
                .map(|(_, name)| *name)
                .unwrap_or("INTERNAL"),
        }
    }

    /// The code for a wire name; unknown names become [`ErrorCode::Unknown`].
    pub fn from_name(name: &str) -> Self {
        Self::NAMED
            .iter()
            .find(|(_, known)| *known == name)
            .map(|(code, _)| code.clone())
            .unwrap_or_else(|| ErrorCode::Unknown(name.to_owned()))
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(ErrorCode::from_name(&name))
    }
}

/// Every frame on the wire. The `op` tag is the variant name in
/// SCREAMING_SNAKE_CASE; binary frames additionally carry the opcode byte.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Frame {
    /// Client handshake, the first frame on a socket.
    Hello {
        /// Protocol version: [`PROTOCOL_VERSION`].
        protocol: u16,
        /// The client build's schema hash, 16 lowercase hex characters.
        schema_hash: String,
        /// Passed to the room's `on_join`; `{}` when there is nothing to pass.
        opts: Value,
    },
    /// The join was accepted.
    JoinOk {
        /// The verified player identity.
        player_id: PlayerId,
        /// This player's seat, `0..max_clients`.
        seat: u8,
    },
    /// The viewer's complete state; replaces the client's tree.
    FullState {
        /// The state sequence number of this snapshot.
        seq: u64,
        /// The state view (sync fields only, owner fields filtered to the viewer).
        state: Value,
    },
    /// Ops to apply, in order, to reach state `seq`.
    Patch {
        /// The state sequence number after the ops (repeated between persists in ticked rooms).
        seq: u64,
        /// The ops, applied in order.
        ops: Vec<PatchOp>,
        /// The tick this patch is the result of (ticked rooms only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tick: Option<u64>,
        /// Server wall-clock milliseconds at that tick (ticked rooms only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t: Option<u64>,
    },
    /// Player intents, in application order.
    Intent {
        /// At least one intent.
        intents: Vec<IntentMsg>,
    },
    /// An out-of-band event; not part of the state.
    Event {
        /// The event name; `hearth.*` names are reserved for the framework.
        name: String,
        /// The event payload.
        data: Value,
    },
    /// Intents the server accepted.
    Ack {
        /// The state sequence number after the accepted intents.
        state_seq: u64,
        /// The accepted intent sequence numbers.
        intent_seqs: Vec<u32>,
        /// The tick that applied these intents (ticked rooms only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tick: Option<u64>,
    },
    /// An error, sent before a fatal close or for a rejected intent.
    Error {
        /// What went wrong.
        code: ErrorCode,
        /// For humans; clients must not parse it.
        message: String,
        /// The rejected intent, or `None` when the error is not about one intent.
        ref_seq: Option<u32>,
    },
    /// The client leaves voluntarily.
    Leave {},
}

/// One state mutation. Path segments are strings (map key or field) or
/// integers (list index).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchOp {
    /// Replace the value at `path`. An empty path replaces the root.
    Set {
        /// Where to set; `[]` is the root.
        path: Vec<Value>,
        /// The new value.
        value: Value,
    },
    /// Remove a map key or list index at `path`.
    Del {
        /// The key or index to remove.
        path: Vec<Value>,
    },
    /// Insert or replace the string `key` in the map at `path`.
    MapInsert {
        /// The object to insert into.
        path: Vec<Value>,
        /// The key to insert or replace.
        key: String,
        /// The new value.
        value: Value,
    },
    /// Splice `insert` into the list at `path`, deleting `delete` values at `index`.
    ListSplice {
        /// The array to splice.
        path: Vec<Value>,
        /// Where to delete and insert; at most the array's length.
        index: u32,
        /// How many values to remove at `index`.
        delete: u32,
        /// The values to insert at `index`.
        insert: Vec<Value>,
    },
}

/// WebSocket close codes used by the runtime.
pub mod close {
    /// 4000: bad frame or unsupported protocol version.
    pub const BAD_FRAME: u16 = 4000;
    /// 4001: schema hash mismatch.
    pub const SCHEMA_MISMATCH: u16 = 4001;
    /// 4002: no seat left.
    pub const ROOM_FULL: u16 = 4002;
    /// 4003: the room is locked.
    pub const ROOM_LOCKED: u16 = 4003;
    /// 4004: kicked by the room.
    pub const KICKED: u16 = 4004;
    /// 4005: rate limited.
    pub const RATE_LIMITED: u16 = 4005;
    /// 4006: frame too large.
    pub const FRAME_TOO_LARGE: u16 = 4006;
    /// 4007: the persisted state is incompatible with this build.
    pub const STATE_INCOMPATIBLE: u16 = 4007;
    /// 4008: the room was disposed.
    pub const DISPOSED: u16 = 4008;
    /// 4009: replaced by a newer socket for the same player.
    pub const REPLACED: u16 = 4009;
    /// 4010: the room's `on_join` refused the player.
    pub const JOIN_REJECTED: u16 = 4010;
}
