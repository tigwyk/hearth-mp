//! Hearth v0.1 wire protocol: frames, codec and patch ops.
//!
//! The language-neutral specification for third-party SDK authors lives in
//! `docs/protocol.md`.

pub mod codec;
pub mod frame;
pub mod patch;

pub use codec::{decode, encode, opcode, ProtocolError};
pub use frame::{
    close, Encoding, ErrorCode, Frame, IntentMsg, Opcode, PatchOp, PlayerId, WireMsg,
    MAX_FRAME_BYTES, PING_TEXT, PONG_TEXT, PROTOCOL_VERSION,
};
pub use patch::{apply, PatchError};

/// JSON data type used on the wire for state trees and intent payloads.
pub type Value = serde_json::Value;
