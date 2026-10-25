//! Hearth v0.1 sans-IO client core.
//!
//! [`ClientCore`] holds all session state and performs no I/O. The caller drives
//! it: send [`ClientCore::hello`], forward every received frame to
//! [`ClientCore::handle`], queue intents with [`ClientCore::queue_intent`] and
//! ask [`ClientCore::poll_send`] for the next frame to send.
//!
//! When the socket drops, call [`ClientCore::on_disconnected`] to learn which
//! intents the server may or may not have applied, then use a
//! [`ReconnectPolicy`] to decide when to try again.
//!
//! ```
//! use hearth_client::{ClientConfig, ClientCore};
//! use hearth_protocol::Encoding;
//! use serde_json::json;
//!
//! let mut client = ClientCore::new(ClientConfig::new("0123456789abcdef", Encoding::Json));
//! let hello = client.hello(json!({}));
//! client.queue_intent(json!({"Move": 1}));
//! let intent = client.poll_send(0).unwrap();
//! ```

use std::collections::{BTreeSet, VecDeque};

use hearth_protocol::{
    apply, decode, encode, Encoding, ErrorCode, Frame, IntentMsg, PatchOp, PlayerId, ProtocolError,
    Value, WireMsg, PING_TEXT, PONG_TEXT, PROTOCOL_VERSION,
};

mod interp;
mod predict;
mod reconnect;

pub use interp::{ClockSync, Interpolator};
pub use predict::{Logic, Predictor};
pub use reconnect::{Reconnect, ReconnectPolicy};

/// Default outgoing message rate limit, in messages per second.
pub const DEFAULT_MAX_MSGS_PER_SEC: u32 = 20;

/// Default maximum number of intents batched into one `INTENT` frame.
pub const DEFAULT_MAX_INTENTS_PER_MSG: usize = 32;

/// Client configuration.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// `HearthState::schema_hash_hex()` of the room's state type.
    pub schema_hash: String,
    /// Frame encoding for this socket.
    pub encoding: Encoding,
    /// Maximum outgoing messages per second.
    pub max_msgs_per_sec: u32,
    /// Maximum intents batched into one `INTENT` frame.
    pub max_intents_per_msg: usize,
}

impl ClientConfig {
    /// Config with the default limits (20 messages per second, 32 intents per
    /// message).
    pub fn new(schema_hash: impl Into<String>, encoding: Encoding) -> Self {
        Self {
            schema_hash: schema_hash.into(),
            encoding,
            max_msgs_per_sec: DEFAULT_MAX_MSGS_PER_SEC,
            max_intents_per_msg: DEFAULT_MAX_INTENTS_PER_MSG,
        }
    }
}

/// A server frame, decoded for the caller.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientEvent {
    /// `JOIN_OK`: the socket is joined as this player and seat.
    Joined { player_id: PlayerId, seat: u8 },
    /// `FULL_STATE`: the state was replaced; read it with [`ClientCore::state`].
    StateReset,
    /// `PATCH`: these ops were applied to the state.
    Patched { ops: Vec<PatchOp> },
    /// `ACK`: intents up to these sequence numbers were accepted.
    Acked {
        state_seq: u64,
        intent_seqs: Vec<u32>,
    },
    /// `EVENT`: a named game event.
    Event { name: String, data: Value },
    /// `ERROR`: the server rejected something.
    Error {
        code: ErrorCode,
        message: String,
        ref_seq: Option<u32>,
    },
    /// A patch could not be applied. The state is unusable until the next
    /// `FULL_STATE`, which emits [`ClientEvent::StateReset`].
    Desync,
    /// Intents that were sent but never ACKed or rejected before the socket
    /// dropped. The server may or may not have applied them: never resend them
    /// silently. Reported once per drop, sorted ascending.
    IntentsUnconfirmed { seqs: Vec<u32> },
}

/// Sans-IO client session state.
pub struct ClientCore {
    cfg: ClientConfig,
    state: Value,
    seq: u64,
    player_id: Option<PlayerId>,
    next_intent_seq: u32,
    pending: VecDeque<IntentMsg>,
    /// Intent sequence numbers sent but not yet ACKed or rejected.
    in_flight: BTreeSet<u32>,
    /// Set by [`ClientCore::on_disconnected`], cleared by the next `JOIN_OK`.
    /// Sending is paused while it is set.
    awaiting_join: bool,
    desynced: bool,
    /// Minimum milliseconds between two sends, from `cfg.max_msgs_per_sec`.
    send_interval_ms: u64,
    /// Clock value of the last queued-intent send; `None` before the first one.
    last_send_ms: Option<u64>,
    /// The newest `(tick, server_ms)` from a ticked-mode `PATCH`.
    last_tick: Option<(u64, u64)>,
}

impl ClientCore {
    /// A fresh, unjoined client.
    pub fn new(cfg: ClientConfig) -> Self {
        let send_interval_ms = 1000 / u64::from(cfg.max_msgs_per_sec.max(1));
        Self {
            cfg,
            state: Value::Null,
            seq: 0,
            player_id: None,
            next_intent_seq: 1,
            pending: VecDeque::new(),
            in_flight: BTreeSet::new(),
            awaiting_join: false,
            desynced: false,
            send_interval_ms,
            last_send_ms: None,
            last_tick: None,
        }
    }

    /// The `HELLO` frame that opens the session.
    pub fn hello(&self, opts: Value) -> WireMsg {
        encode(
            &Frame::Hello {
                protocol: PROTOCOL_VERSION,
                schema_hash: self.cfg.schema_hash.clone(),
                opts,
            },
            self.cfg.encoding,
        )
    }

    /// Decode one frame from the server and apply it to the session.
    ///
    /// An `ACK` clears the listed intents from the unconfirmed set, and an
    /// `ERROR` with a `ref_seq` clears that intent.
    pub fn handle(&mut self, msg: &WireMsg) -> Result<Vec<ClientEvent>, ProtocolError> {
        // Keepalive pings and pongs are text frames, not protocol frames.
        if let WireMsg::Text(text) = msg {
            if text.as_str() == PING_TEXT || text.as_str() == PONG_TEXT {
                return Ok(Vec::new());
            }
        }
        match decode(msg)? {
            Frame::JoinOk { player_id, seat } => {
                self.player_id = Some(player_id.clone());
                self.awaiting_join = false;
                Ok(vec![ClientEvent::Joined { player_id, seat }])
            }
            Frame::FullState { seq, state } => {
                self.state = state;
                self.seq = seq;
                // The tree was replaced, so any ticked patch that follows is
                // newer than it regardless of the tick it carries (a room that
                // was disposed and recreated restarts its tick count).
                self.last_tick = None;
                self.desynced = false;
                Ok(vec![ClientEvent::StateReset])
            }
            Frame::Patch { seq, ops, tick, t } => {
                // Ticked patches repeat the persisted state seq (it only moves
                // on persist), so they are ordered by their tick instead.
                let previous_tick = self.last_tick.map(|(tick, _)| tick);
                if let (Some(tick), Some(t)) = (tick, t) {
                    self.last_tick = Some((tick, t));
                }
                Ok(self.handle_patch(seq, tick, previous_tick, ops))
            }
            Frame::Ack {
                state_seq,
                intent_seqs,
                ..
            } => {
                for seq in &intent_seqs {
                    self.in_flight.remove(seq);
                }
                Ok(vec![ClientEvent::Acked {
                    state_seq,
                    intent_seqs,
                }])
            }
            Frame::Event { name, data } => Ok(vec![ClientEvent::Event { name, data }]),
            Frame::Error {
                code,
                message,
                ref_seq,
            } => {
                if let Some(seq) = ref_seq {
                    self.in_flight.remove(&seq);
                }
                Ok(vec![ClientEvent::Error {
                    code,
                    message,
                    ref_seq,
                }])
            }
            // Server-to-client frames this client never expects; ignore them.
            Frame::Hello { .. } | Frame::Intent { .. } | Frame::Leave {} => Ok(Vec::new()),
        }
    }

    /// A stale patch is dropped. A patch that fails to apply desyncs the
    /// client: the failed op changes nothing, later patches are ignored, and
    /// the next `FULL_STATE` recovers.
    ///
    /// Event-mode patches advance `seq`, so an old one is `seq <= self.seq`.
    /// Ticked patches carry a `tick` and repeat the persisted `seq` until the
    /// next persist, so they are ordered by `tick` instead.
    fn handle_patch(
        &mut self,
        seq: u64,
        tick: Option<u64>,
        previous_tick: Option<u64>,
        ops: Vec<PatchOp>,
    ) -> Vec<ClientEvent> {
        let stale = match tick {
            Some(tick) => previous_tick.is_some_and(|previous| tick <= previous),
            None => seq <= self.seq,
        };
        if self.desynced || stale {
            return Vec::new();
        }
        for op in &ops {
            if apply(&mut self.state, op).is_err() {
                self.desynced = true;
                return vec![ClientEvent::Desync];
            }
        }
        self.seq = self.seq.max(seq);
        vec![ClientEvent::Patched { ops }]
    }

    /// Queue an intent and return its client-assigned sequence number. The
    /// first intent is `1`; each one adds `1`.
    pub fn queue_intent(&mut self, intent: Value) -> u32 {
        let seq = self.next_intent_seq;
        self.next_intent_seq += 1;
        self.pending.push_back(IntentMsg { seq, intent });
        seq
    }

    /// The next `INTENT` frame to send, or `None` when nothing may be sent now.
    ///
    /// Returns `None` while the queue is empty, while waiting for the next
    /// `JOIN_OK` after a drop, or while the rate limit has not elapsed since the
    /// previous send. The first send is never rate limited.
    pub fn poll_send(&mut self, now_ms: u64) -> Option<WireMsg> {
        if self.awaiting_join || self.pending.is_empty() {
            return None;
        }
        if let Some(last) = self.last_send_ms {
            if now_ms.saturating_sub(last) < self.send_interval_ms {
                return None;
            }
        }
        let take = self.cfg.max_intents_per_msg.max(1).min(self.pending.len());
        let intents: Vec<IntentMsg> = self.pending.drain(..take).collect();
        self.in_flight
            .extend(intents.iter().map(|intent| intent.seq));
        self.last_send_ms = Some(now_ms);
        Some(encode(&Frame::Intent { intents }, self.cfg.encoding))
    }

    /// Record that the socket dropped.
    ///
    /// Returns at most one event: [`ClientEvent::IntentsUnconfirmed`] naming
    /// every intent that was sent but never ACKed or rejected. Those intents
    /// are then forgotten, so they are reported once and never resent silently.
    /// Intents queued but never sent stay queued, and sending pauses until the
    /// next `JOIN_OK`.
    pub fn on_disconnected(&mut self) -> Vec<ClientEvent> {
        self.awaiting_join = true;
        if self.in_flight.is_empty() {
            return Vec::new();
        }
        let seqs: Vec<u32> = std::mem::take(&mut self.in_flight).into_iter().collect();
        vec![ClientEvent::IntentsUnconfirmed { seqs }]
    }

    /// The `LEAVE` frame that closes the session.
    pub fn leave(&self) -> WireMsg {
        encode(&Frame::Leave {}, self.cfg.encoding)
    }

    /// The current state view; `Value::Null` until the first `FULL_STATE`.
    pub fn state(&self) -> &Value {
        &self.state
    }

    /// The last state sequence applied.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The newest `(tick, server_ms)` from a ticked-mode `PATCH`; `None` in
    /// event rooms or before the first ticked patch.
    pub fn last_tick(&self) -> Option<(u64, u64)> {
        self.last_tick
    }

    /// The joined player id; `None` until `JOIN_OK`.
    pub fn player_id(&self) -> Option<&PlayerId> {
        self.player_id.as_ref()
    }
}
