use crate::frame::{Encoding, Frame, Opcode, WireMsg};

/// Error returned by [`decode`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame exceeds the 64 KiB maximum")]
    /// The frame exceeds [`MAX_FRAME_BYTES`](crate::MAX_FRAME_BYTES). [`decode`] no
    /// longer returns it: the cap is on frames *to* the server, which checks the
    /// size itself, while state frames from the server may be larger.
    TooLarge,
    #[error("malformed frame: {0}")]
    /// The frame did not decode: bad MessagePack/JSON, a missing field, or an empty binary frame.
    Malformed(String),
    #[error("opcode byte does not match the frame body")]
    /// A binary frame's opcode byte disagrees with its body's `op`.
    OpcodeMismatch,
    /// A frame this protocol version does not know: an unknown opcode byte
    /// (`"0x40"`) or an unknown JSON `op` (`"FUTURE"`). Clients ignore these
    /// (`spec/protocol-v1.md` section 12); servers answer `BAD_FRAME`.
    #[error("unknown opcode {0}")]
    UnknownOpcode(String),
}

/// Every JSON `op` this version knows, including the reserved `PING`.
const KNOWN_OPS: [&str; 10] = [
    "HELLO",
    "JOIN_OK",
    "FULL_STATE",
    "PATCH",
    "INTENT",
    "EVENT",
    "ACK",
    "PING",
    "ERROR",
    "LEAVE",
];

/// The opcode for a frame.
pub const fn opcode(frame: &Frame) -> Opcode {
    match frame {
        Frame::Hello { .. } => Opcode::Hello,
        Frame::JoinOk { .. } => Opcode::JoinOk,
        Frame::FullState { .. } => Opcode::FullState,
        Frame::Patch { .. } => Opcode::Patch,
        Frame::Intent { .. } => Opcode::Intent,
        Frame::Event { .. } => Opcode::Event,
        Frame::Ack { .. } => Opcode::Ack,
        Frame::Error { .. } => Opcode::Error,
        Frame::Leave { .. } => Opcode::Leave,
    }
}

/// Encode a frame. Binary is `opcode byte || MessagePack(frame)`; JSON is the
/// frame serialised as text.
pub fn encode(frame: &Frame, encoding: Encoding) -> WireMsg {
    match encoding {
        Encoding::MsgPack => {
            let mut bytes = rmp_serde::to_vec_named(frame).expect("frames always serialise");
            bytes.insert(0, opcode(frame).as_u8());
            WireMsg::Binary(bytes)
        }
        Encoding::Json => {
            WireMsg::Text(serde_json::to_string(frame).expect("frames always serialise"))
        }
    }
}

/// Decode a wire message: the opcode byte, then the body, whose own `op` tag
/// must agree with the opcode byte. It does not check the size; a server checks
/// [`MAX_FRAME_BYTES`](crate::MAX_FRAME_BYTES) before calling it.
pub fn decode(msg: &WireMsg) -> Result<Frame, ProtocolError> {
    match msg {
        WireMsg::Binary(bytes) => {
            let Some((&byte, body)) = bytes.split_first() else {
                return Err(ProtocolError::Malformed("empty frame".into()));
            };
            let expected = Opcode::from_u8(byte)
                .ok_or_else(|| ProtocolError::UnknownOpcode(format!("0x{byte:02x}")))?;
            let frame: Frame = rmp_serde::from_slice(body)
                .map_err(|err| ProtocolError::Malformed(err.to_string()))?;
            if opcode(&frame) != expected {
                return Err(ProtocolError::OpcodeMismatch);
            }
            Ok(frame)
        }
        WireMsg::Text(text) => {
            serde_json::from_str(text).map_err(|err| {
                // An object whose `op` is a string this version does not know is
                // a future frame, not a malformed one.
                let op = serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .and_then(|v| v.get("op").and_then(|op| op.as_str()).map(str::to_owned));
                match op {
                    Some(op) if !KNOWN_OPS.contains(&op.as_str()) => {
                        ProtocolError::UnknownOpcode(op)
                    }
                    _ => ProtocolError::Malformed(err.to_string()),
                }
            })
        }
    }
}
