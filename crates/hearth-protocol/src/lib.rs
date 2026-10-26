//! Hearth wire protocol v1: frames, codec and patch ops.
//!
//! The normative, language-neutral specification is `spec/protocol-v1.md`; the
//! conformance vectors are in `conformance/vectors/`.

#![warn(missing_docs)]

/// Frame encoding and decoding.
pub mod codec;
/// Frames, opcodes, error codes and close codes.
pub mod frame;
/// Applying patch ops to a state tree.
pub mod patch;
#[cfg(feature = "vectors")]
pub mod vectors;

pub use codec::{decode, encode, opcode, ProtocolError};
pub use frame::{
    close, Encoding, ErrorCode, Frame, IntentMsg, Opcode, PatchOp, PlayerId, WireMsg,
    MAX_FRAME_BYTES, PING_TEXT, PONG_TEXT, PROTOCOL_VERSION,
};
pub use patch::{apply, PatchError};

/// JSON data type used on the wire for state trees and intent payloads.
pub type Value = serde_json::Value;
