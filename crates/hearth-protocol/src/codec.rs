use crate::frame::{Encoding, Frame, Opcode, WireMsg, MAX_FRAME_BYTES};

/// Error returned by [`decode`]. `TooLarge` is checked before any parsing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame exceeds the 64 KiB maximum")]
    TooLarge,
    #[error("malformed frame: {0}")]
    Malformed(String),
    #[error("opcode byte does not match the frame body")]
    OpcodeMismatch,
}

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

/// Decode a wire message. Length is checked first, then the opcode byte, then
/// the body; the body's own `op` tag must agree with the opcode byte.
pub fn decode(msg: &WireMsg) -> Result<Frame, ProtocolError> {
    match msg {
        WireMsg::Binary(bytes) => {
            if bytes.len() > MAX_FRAME_BYTES {
                return Err(ProtocolError::TooLarge);
            }
            let Some((&byte, body)) = bytes.split_first() else {
                return Err(ProtocolError::Malformed("empty frame".into()));
            };
            let expected = Opcode::from_u8(byte)
                .ok_or_else(|| ProtocolError::Malformed(format!("unknown opcode 0x{byte:02x}")))?;
            let frame: Frame = rmp_serde::from_slice(body)
                .map_err(|err| ProtocolError::Malformed(err.to_string()))?;
            if opcode(&frame) != expected {
                return Err(ProtocolError::OpcodeMismatch);
            }
            Ok(frame)
        }
        WireMsg::Text(text) => {
            if text.len() > MAX_FRAME_BYTES {
                return Err(ProtocolError::TooLarge);
            }
            serde_json::from_str(text).map_err(|err| ProtocolError::Malformed(err.to_string()))
        }
    }
}
