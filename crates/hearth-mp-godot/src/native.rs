//! `HearthNative`: GDExtension node that drives [`hearth_mp_client::ClientCore`]
//! over Godot's built-in `WebSocketPeer` (MsgPack encoding).
//!
//! A dropped socket is retried with [`hearth_mp_client::ReconnectPolicy`] until the
//! grace window is spent. Intents that were in flight at the drop are reported
//! (`intents_unconfirmed`) and never resent; queued intents wait for the next
//! `JOIN_OK`.
//!
//! Every call in this module needs a running Godot engine. The JSON <-> Variant
//! helpers live in [`crate::convert`] and stay engine-free for tests.

use std::collections::VecDeque;

use godot::classes::web_socket_peer::State;
use godot::classes::{INode, Node, Os, Time, WebSocketPeer};
use godot::global::Error as GodotError;
use godot::prelude::*;
use hearth_mp_client::{ClientConfig, ClientCore, ClientEvent, Reconnect, ReconnectPolicy};
use hearth_mp_protocol::{Encoding, WireMsg};
use serde_json::Value;

use crate::convert::{json_to_variant, variant_to_json};
use crate::HearthInterp;

/// Godot-facing Hearth client. See `godot/addons/hearth/hearth_client.gd` for the
/// autoload that owns this node on desktop builds.
#[derive(GodotClass)]
#[class(base = Node)]
pub struct HearthNative {
    base: Base<Node>,
    core: ClientCore,
    peer: Option<Gd<WebSocketPeer>>,
    /// Room URL (the auth token is already in its query) for reconnects.
    url: String,
    opts: Value,
    hello_sent: bool,
    /// Retry policy shared with the GDScript client.
    policy: ReconnectPolicy,
    /// Clock value of the first drop in the current outage.
    first_drop_ms: Option<u64>,
    /// Retries already scheduled in the current outage.
    attempt: u32,
    /// Clock value when the next retry is due; `None` when not retrying.
    retry_at_ms: Option<u64>,
    /// The next `JOIN_OK` is a resume, so it emits `reconnected`.
    resuming: bool,
    /// Whether this socket delivered an `ERROR` frame (see `close_error`).
    saw_error: bool,
    /// Interpolators created by `Hearth.interpolator()`, fed from every ticked
    /// `PATCH`.
    interpolators: Vec<Gd<HearthInterp>>,
    /// Debug-only latency injection: delays outgoing sends and incoming frames
    /// by this many milliseconds (the prediction smoke test needs it).
    #[var]
    debug_latency_ms: i64,
    /// Outgoing frames waiting for their (local ms) send time.
    outgoing: VecDeque<(u64, WireMsg)>,
    /// Incoming frames waiting for their (local ms) delivery time.
    incoming: VecDeque<(u64, WireMsg)>,
}

#[godot_api]
impl INode for HearthNative {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            core: ClientCore::new(ClientConfig::new(String::new(), Encoding::MsgPack)),
            peer: None,
            url: String::new(),
            opts: Value::Null,
            hello_sent: false,
            policy: ReconnectPolicy::default(),
            first_drop_ms: None,
            attempt: 0,
            retry_at_ms: None,
            resuming: false,
            saw_error: false,
            interpolators: Vec::new(),
            debug_latency_ms: 0,
            outgoing: VecDeque::new(),
            incoming: VecDeque::new(),
        }
    }

    fn process(&mut self, _delta: f64) {
        self.pump();
    }

    fn ready(&mut self) {
        // Engine classes must opt in to per-frame processing explicitly; there
        // is no GDScript `_process` auto-enable for GDExtension nodes.
        self.base_mut().set_process(true);
    }
}

#[godot_api]
impl HearthNative {
    #[signal]
    fn joined(player_id: GString, seat: i32);

    #[signal]
    fn state_reset();

    #[signal]
    fn patched(ops: VarArray);

    #[signal]
    fn acked(state_seq: i64, intent_seqs: VarArray);

    #[signal]
    fn event(name: GString, data: Variant);

    #[signal]
    fn error(code: GString, message: GString, ref_seq: i64);

    /// The socket gave up: the close was deliberate or fatal, or the grace
    /// window is spent. Recoverable drops emit `reconnecting` instead.
    #[signal]
    fn disconnected(code: i64, reason: GString);

    /// A retry is scheduled. `attempt` is 0-based; `retry_in_ms` is from now.
    #[signal]
    fn reconnecting(attempt: i32, retry_in_ms: i64);

    /// A retry joined the room again.
    #[signal]
    fn reconnected();

    /// Intents sent before the drop whose fate the server may or may not have
    /// decided. Sorted ascending; reported once per drop, never resent.
    #[signal]
    fn intents_unconfirmed(seqs: VarArray);

    /// Open the room WebSocket (`ws://` or `wss://`, token already in the URL)
    /// and start the MsgPack handshake.
    #[func]
    fn connect_to_room(&mut self, url: GString, schema_hash: GString, opts: VarDictionary) {
        self.opts = variant_to_json(&opts.to_variant())
            .unwrap_or_else(|_| Value::Object(Default::default()));
        self.core = ClientCore::new(ClientConfig::new(
            schema_hash.to_string(),
            Encoding::MsgPack,
        ));
        self.url = url.to_string();
        self.cancel_reconnect();
        self.incoming.clear();
        self.outgoing.clear();
        self.open_socket();
    }

    /// Create a snapshot interpolator fed from every ticked `PATCH` from now
    /// on. `paths` holds paths (arrays of keys or list indices; a `"*"` segment
    /// matches every key at its level).
    #[func]
    fn create_interpolator(&mut self, paths: VarArray) -> Gd<HearthInterp> {
        let mut interpolator = HearthInterp::new_gd();
        for index in 0..paths.len() {
            if let Ok(Value::Array(path)) = variant_to_json(&paths.at(index)) {
                interpolator.bind_mut().track(path);
            }
        }
        self.interpolators.push(interpolator.clone());
        interpolator
    }

    /// Queue one intent, returning its client-assigned sequence number. `0` means
    /// the intent could not be converted (an `error` signal was emitted).
    #[func]
    fn send_intent(&mut self, intent: Variant) -> i64 {
        match variant_to_json(&intent) {
            Ok(value) => i64::from(self.core.queue_intent(value)),
            Err(message) => {
                self.emit_error("BAD_INTENT", &message, -1);
                0
            }
        }
    }

    /// Send `LEAVE` and close the socket. Retries stop; `disconnected` does not
    /// fire for a deliberate leave.
    #[func]
    fn leave(&mut self) {
        self.cancel_reconnect();
        self.incoming.clear();
        self.outgoing.clear();
        if let Some(mut peer) = self.peer.clone() {
            // A socket still connecting (a join that timed out) has nobody to tell.
            if peer.get_ready_state() == State::OPEN {
                let leave = self.core.leave();
                send_frame(&mut peer, &leave);
            }
            peer.close();
        }
        self.peer = None;
    }

    /// Test seam: drop the socket locally and handle it like an abnormal (1006)
    /// close. Debug builds only.
    #[func]
    fn _debug_drop(&mut self) {
        if !Os::singleton().is_debug_build() {
            return;
        }
        let Some(mut peer) = self.peer.take() else {
            return;
        };
        peer.close();
        let now_ms = Time::singleton().get_ticks_msec();
        self.handle_drop(1006, String::new(), now_ms);
    }

    /// The current local state view (sync fields only); `{}` before join.
    #[func]
    fn get_state(&self) -> VarDictionary {
        json_to_variant(self.core.state())
            .try_to::<VarDictionary>()
            .unwrap_or_else(|_| VarDictionary::new())
    }

    /// The joined player id, or `""` before `JOIN_OK`.
    #[func]
    fn get_player_id(&self) -> GString {
        self.core
            .player_id()
            .map(|player_id| GString::from(player_id.0.as_str()))
            .unwrap_or_else(|| GString::from(""))
    }
}

impl HearthNative {
    /// Poll the socket once per frame: read packets, send `HELLO`/queued
    /// intents, and run the reconnect timer.
    fn pump(&mut self) {
        let now_ms = Time::singleton().get_ticks_msec();
        if self.peer.is_none() && !self.maybe_retry(now_ms) {
            return;
        }
        let Some(mut peer) = self.peer.clone() else {
            return;
        };
        peer.poll();

        match peer.get_ready_state() {
            State::OPEN => {
                let latency = self.latency_ms();
                if !self.hello_sent {
                    let hello = self.core.hello(self.opts.clone());
                    self.queue_or_send(&mut peer, hello, latency);
                    self.hello_sent = true;
                }

                while peer.get_available_packet_count() > 0 {
                    let packet = peer.get_packet();
                    let message = if peer.was_string_packet() {
                        WireMsg::Text(String::from_utf8_lossy(packet.as_slice()).into_owned())
                    } else {
                        WireMsg::Binary(packet.to_vec())
                    };
                    if latency == 0 {
                        self.deliver(message);
                    } else {
                        self.incoming.push_back((now_ms + latency, message));
                    }
                }
                self.deliver_due_incoming(now_ms);

                while let Some(message) = self.core.poll_send(now_ms) {
                    self.queue_or_send(&mut peer, message, latency);
                }
                self.deliver_due_outgoing(&mut peer, now_ms);
            }
            State::CLOSED => {
                // Frames the latency injection still holds arrived before the
                // close, so they are delivered first.
                self.deliver_due_incoming(u64::MAX);
                let code = socket_close_code(&peer);
                let reason = peer.get_close_reason().to_string();
                self.peer = None;
                self.incoming.clear();
                self.outgoing.clear();
                self.handle_drop(code, reason, now_ms);
            }
            State::CONNECTING | State::CLOSING => {}
            _ => {}
        }
    }

    /// The injected one-way latency in milliseconds (debug builds only).
    fn latency_ms(&self) -> u64 {
        if Os::singleton().is_debug_build() && self.debug_latency_ms > 0 {
            self.debug_latency_ms as u64
        } else {
            0
        }
    }

    /// Decode one server frame and emit its events.
    fn deliver(&mut self, message: WireMsg) {
        match self.core.handle(&message) {
            Ok(events) => self.emit_events(events),
            Err(error) => {
                let message = format!("bad server frame: {error:?}");
                self.emit_error("BAD_FRAME", &message, -1);
            }
        }
    }

    /// Send a frame now, or hold it for `latency` ms.
    fn queue_or_send(&mut self, peer: &mut Gd<WebSocketPeer>, message: WireMsg, latency: u64) {
        if latency == 0 {
            send_frame(peer, &message);
            return;
        }
        let due = Time::singleton().get_ticks_msec() + latency;
        self.outgoing.push_back((due, message));
    }

    /// Deliver incoming frames whose injected latency has elapsed.
    fn deliver_due_incoming(&mut self, now_ms: u64) {
        while let Some(front) = self.incoming.front() {
            if front.0 > now_ms {
                break;
            }
            let Some((_, message)) = self.incoming.pop_front() else {
                break;
            };
            self.deliver(message);
        }
    }

    /// Send outgoing frames whose injected latency has elapsed.
    fn deliver_due_outgoing(&mut self, peer: &mut Gd<WebSocketPeer>, now_ms: u64) {
        while let Some(front) = self.outgoing.front() {
            if front.0 > now_ms {
                break;
            }
            let Some((_, message)) = self.outgoing.pop_front() else {
                break;
            };
            send_frame(peer, &message);
        }
    }

    /// Open a socket to [`Self::url`] and start the handshake. On failure an
    /// `error` signal is emitted and no socket is kept.
    fn open_socket(&mut self) -> bool {
        self.hello_sent = false;
        self.saw_error = false;
        let mut peer = WebSocketPeer::new_gd();
        let error = peer.connect_to_url(&self.url);
        if error != GodotError::OK {
            let message = format!("WebSocketPeer.connect_to_url failed ({error:?})");
            self.emit_error("BAD_FRAME", &message, -1);
            self.peer = None;
            return false;
        }
        self.peer = Some(peer);
        true
    }

    /// Open the socket when a scheduled retry is due. Returns whether a socket
    /// now exists.
    fn maybe_retry(&mut self, now_ms: u64) -> bool {
        let Some(at) = self.retry_at_ms else {
            return false;
        };
        if now_ms < at {
            return false;
        }
        self.retry_at_ms = None;
        if self.open_socket() {
            return true;
        }
        // The retry could not even open a socket: treat that as another drop so
        // the backoff keeps growing.
        self.handle_drop(1006, String::new(), now_ms);
        false
    }

    /// Handle a dropped socket: report intents whose fate is unknown, then
    /// schedule a retry (`reconnecting`) or give up (`disconnected`).
    fn handle_drop(&mut self, code: u16, reason: String, now_ms: u64) {
        // Godot cannot read a frame that arrives in the same read as the close,
        // so the ERROR before a fatal close may be lost: report it from the code.
        if !self.saw_error {
            if let Some(name) = close_error(code) {
                self.emit_error(name, &reason, -1);
            }
        }
        let events = self.core.on_disconnected();
        self.emit_events(events);

        let first_drop_ms = *self.first_drop_ms.get_or_insert(now_ms);
        match self
            .policy
            .decide(code, first_drop_ms, self.attempt, now_ms)
        {
            Reconnect::GiveUp => {
                self.cancel_reconnect();
                self.signals()
                    .disconnected()
                    .emit(i64::from(code), &GString::from(reason.as_str()));
            }
            Reconnect::RetryAt(at) => {
                self.resuming = self.core.player_id().is_some();
                self.retry_at_ms = Some(at);
                let attempt = self.attempt as i32;
                let retry_in_ms = at.saturating_sub(now_ms) as i64;
                self.signals().reconnecting().emit(attempt, retry_in_ms);
                self.attempt += 1;
            }
        }
    }

    /// Forget the current outage (a join succeeded, left, or gave up).
    fn cancel_reconnect(&mut self) {
        self.first_drop_ms = None;
        self.attempt = 0;
        self.retry_at_ms = None;
        self.resuming = false;
    }

    /// Feed every registered interpolator from the newest ticked `PATCH`.
    fn feed_interpolators(&mut self) {
        if self.interpolators.is_empty() {
            return;
        }
        let Some((_, server_t)) = self.core.last_tick() else {
            return;
        };
        let state = self.core.state();
        for interpolator in &mut self.interpolators {
            interpolator.bind_mut().on_patch(state, server_t);
        }
    }

    /// Drop interpolator snapshots after a `FULL_STATE` replaced the state.
    fn reset_interpolators(&mut self) {
        for interpolator in &mut self.interpolators {
            interpolator.bind_mut().reset();
        }
    }

    fn emit_events(&mut self, events: Vec<ClientEvent>) {
        for event in events {
            match event {
                ClientEvent::Joined { player_id, seat } => {
                    let resuming = self.resuming;
                    self.cancel_reconnect();
                    let player_id = GString::from(player_id.0.as_str());
                    self.signals().joined().emit(&player_id, i32::from(seat));
                    if resuming {
                        self.signals().reconnected().emit();
                    }
                }
                ClientEvent::StateReset => {
                    self.reset_interpolators();
                    self.signals().state_reset().emit();
                }
                ClientEvent::Patched { ops } => {
                    // Feed the interpolators before the signal so game code can
                    // sample immediately in its `patched` handler.
                    self.feed_interpolators();
                    let mut array = VarArray::new();
                    for op in &ops {
                        if let Ok(value) = serde_json::to_value(op) {
                            array.push(&json_to_variant(&value));
                        }
                    }
                    self.signals().patched().emit(&array);
                }
                ClientEvent::Acked {
                    state_seq,
                    intent_seqs,
                } => {
                    let mut array = VarArray::new();
                    for seq in intent_seqs {
                        array.push(i64::from(seq));
                    }
                    self.signals().acked().emit(state_seq as i64, &array);
                }
                ClientEvent::Event { name, data } => {
                    let name = GString::from(name.as_str());
                    let data = json_to_variant(&data);
                    self.signals().event().emit(&name, &data);
                }
                ClientEvent::Error {
                    code,
                    message,
                    ref_seq,
                } => {
                    self.saw_error = true;
                    let code = GString::from(code.as_str());
                    let message = GString::from(message.as_str());
                    let ref_seq = ref_seq.map(i64::from).unwrap_or(-1);
                    self.signals().error().emit(&code, &message, ref_seq);
                }
                ClientEvent::Desync => self.emit_error(
                    "DESYNC",
                    "patch failed to apply; waiting for FULL_STATE",
                    -1,
                ),
                ClientEvent::IntentsUnconfirmed { seqs } => {
                    let mut array = VarArray::new();
                    for seq in seqs {
                        array.push(i64::from(seq));
                    }
                    self.signals().intents_unconfirmed().emit(&array);
                }
                // Events added by a later hearth-mp-client release.
                _ => {}
            }
        }
    }

    fn emit_error(&mut self, code: &str, message: &str, ref_seq: i64) {
        let code = GString::from(code);
        let message = GString::from(message);
        self.signals().error().emit(&code, &message, ref_seq);
    }
}

/// The socket's close code, mapped to 1006 (abnormal) when the engine reports no
/// clean close frame.
fn socket_close_code(peer: &Gd<WebSocketPeer>) -> u16 {
    let code = peer.get_close_code();
    if (1000..=4999).contains(&code) {
        code as u16
    } else {
        1006
    }
}

fn send_frame(peer: &mut Gd<WebSocketPeer>, message: &WireMsg) {
    let error = match message {
        WireMsg::Binary(bytes) => peer.send(&PackedByteArray::from(bytes.as_slice())),
        WireMsg::Text(text) => peer.send_text(text.as_str()),
    };
    if error != GodotError::OK {
        godot_error!("hearth: WebSocketPeer send failed: {error:?}");
    }
}

/// The error code an assigned close code stands for, when its `ERROR` frame was
/// lost (spec/protocol-v1.md section 9).
fn close_error(code: u16) -> Option<&'static str> {
    use hearth_mp_protocol::close;
    Some(match code {
        close::BAD_FRAME => "BAD_FRAME",
        close::SCHEMA_MISMATCH => "SCHEMA_MISMATCH",
        close::ROOM_FULL => "ROOM_FULL",
        close::ROOM_LOCKED => "ROOM_LOCKED",
        close::RATE_LIMITED => "RATE_LIMITED",
        close::FRAME_TOO_LARGE => "FRAME_TOO_LARGE",
        close::STATE_INCOMPATIBLE => "STATE_INCOMPATIBLE",
        close::JOIN_REJECTED => "JOIN_REJECTED",
        _ => return None,
    })
}
