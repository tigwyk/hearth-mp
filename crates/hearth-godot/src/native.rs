//! `HearthNative`: GDExtension node that drives [`hearth_client::ClientCore`]
//! over Godot's built-in `WebSocketPeer` (MsgPack encoding).
//!
//! Every call in this module needs a running Godot engine. The JSON <-> Variant
//! helpers live in [`crate::convert`] and stay engine-free for tests.

use godot::classes::web_socket_peer::State;
use godot::classes::{INode, Node, Time, WebSocketPeer};
use godot::global::Error as GodotError;
use godot::prelude::*;
use hearth_client::{ClientConfig, ClientCore, ClientEvent};
use hearth_protocol::{Encoding, ErrorCode, WireMsg};
use serde_json::Value;

use crate::convert::{json_to_variant, variant_to_json};

/// Godot-facing Hearth client. See `godot/addons/hearth/hearth_client.gd` for the
/// autoload that owns this node on desktop builds.
#[derive(GodotClass)]
#[class(base = Node)]
pub struct HearthNative {
    base: Base<Node>,
    core: ClientCore,
    peer: Option<Gd<WebSocketPeer>>,
    opts: Value,
    hello_sent: bool,
    close_reported: bool,
}

#[godot_api]
impl INode for HearthNative {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            core: ClientCore::new(ClientConfig::new(String::new(), Encoding::MsgPack)),
            peer: None,
            opts: Value::Null,
            hello_sent: false,
            close_reported: false,
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

    #[signal]
    fn disconnected(code: i64, reason: GString);

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
        self.hello_sent = false;
        self.close_reported = false;

        let mut peer = WebSocketPeer::new_gd();
        let error = peer.connect_to_url(&url);
        if error != GodotError::OK {
            let message = format!("WebSocketPeer.connect_to_url failed ({error:?})");
            self.emit_error("BAD_FRAME", &message, -1);
            self.peer = None;
            return;
        }
        self.peer = Some(peer);
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

    /// Send `LEAVE` and close the socket. The `disconnected` signal follows the
    /// normal close path.
    #[func]
    fn leave(&mut self) {
        if let Some(mut peer) = self.peer.clone() {
            let leave = self.core.leave();
            send_frame(&mut peer, &leave);
            peer.close();
        }
        self.peer = None;
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
    /// Poll the socket once per frame: read packets, send `HELLO`/queued intents.
    fn pump(&mut self) {
        let now_ms = Time::singleton().get_ticks_msec();
        let Some(mut peer) = self.peer.clone() else {
            return;
        };
        peer.poll();

        match peer.get_ready_state() {
            State::OPEN => {
                if !self.hello_sent {
                    let hello = self.core.hello(self.opts.clone());
                    send_frame(&mut peer, &hello);
                    self.hello_sent = true;
                }

                while peer.get_available_packet_count() > 0 {
                    let packet = peer.get_packet();
                    let message = if peer.was_string_packet() {
                        WireMsg::Text(String::from_utf8_lossy(packet.as_slice()).into_owned())
                    } else {
                        WireMsg::Binary(packet.to_vec())
                    };
                    let result = self.core.handle(&message);
                    match result {
                        Ok(events) => self.emit_events(events),
                        Err(error) => {
                            let message = format!("bad server frame: {error:?}");
                            self.emit_error("BAD_FRAME", &message, -1);
                        }
                    }
                }

                while let Some(message) = self.core.poll_send(now_ms) {
                    send_frame(&mut peer, &message);
                }
            }
            State::CLOSED => {
                self.peer = None;
                if !self.close_reported {
                    self.close_reported = true;
                    let code = i64::from(peer.get_close_code());
                    let reason = peer.get_close_reason();
                    self.signals().disconnected().emit(code, &reason);
                }
            }
            State::CONNECTING | State::CLOSING => {}
            _ => {}
        }
    }

    fn emit_events(&mut self, events: Vec<ClientEvent>) {
        for event in events {
            match event {
                ClientEvent::Joined { player_id, seat } => {
                    let player_id = GString::from(player_id.0.as_str());
                    self.signals().joined().emit(&player_id, i32::from(seat));
                }
                ClientEvent::StateReset => self.signals().state_reset().emit(),
                ClientEvent::Patched { ops } => {
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
                    let code = GString::from(error_code_name(code));
                    let message = GString::from(message.as_str());
                    let ref_seq = ref_seq.map(i64::from).unwrap_or(-1);
                    self.signals().error().emit(&code, &message, ref_seq);
                }
                ClientEvent::Desync => self.emit_error(
                    "DESYNC",
                    "patch failed to apply; waiting for FULL_STATE",
                    -1,
                ),
            }
        }
    }

    fn emit_error(&mut self, code: &str, message: &str, ref_seq: i64) {
        let code = GString::from(code);
        let message = GString::from(message);
        self.signals().error().emit(&code, &message, ref_seq);
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

/// The wire name of an error code (`SCREAMING_SNAKE_CASE`), as GDScript sees it.
fn error_code_name(code: ErrorCode) -> &'static str {
    use ErrorCode::*;
    match code {
        BadFrame => "BAD_FRAME",
        UnsupportedProtocol => "UNSUPPORTED_PROTOCOL",
        SchemaMismatch => "SCHEMA_MISMATCH",
        RoomFull => "ROOM_FULL",
        RoomLocked => "ROOM_LOCKED",
        NotJoined => "NOT_JOINED",
        BadIntent => "BAD_INTENT",
        IntentRejected => "INTENT_REJECTED",
        JoinRejected => "JOIN_REJECTED",
        RateLimited => "RATE_LIMITED",
        FrameTooLarge => "FRAME_TOO_LARGE",
        StateIncompatible => "STATE_INCOMPATIBLE",
        Internal => "INTERNAL",
    }
}
