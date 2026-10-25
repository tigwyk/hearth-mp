use hearth_protocol::{
    close, decode, encode, Encoding, ErrorCode, Frame, IntentMsg, PlayerId, Value, WireMsg,
    MAX_FRAME_BYTES, PING_TEXT, PONG_TEXT, PROTOCOL_VERSION,
};
use hearth_sync::{diff, HearthState};

use crate::ctx::{Ctx, Effect};
use crate::host::{Attachment, Host, SqlValue};
use crate::room::{Client, JoinOpts, Leave, Room, TimerKey};
use crate::Result;

/// Internal key for the empty-room grace timer. User timers are stored under a
/// `u:` prefix, so internal keys can never collide with user keys.
const EMPTY_TIMER: &str = "h:empty";
const USER_TIMER_PREFIX: &str = "u:";

/// Per-player intent bucket: capacity 120 intents, refill 60/s. Hibernation
/// resets the bucket, which is intended: a room only hibernates when quiet.
const INTENT_BUCKET_CAPACITY: f64 = 120.0;
const INTENT_REFILL_PER_MS: f64 = 60.0 / 1000.0;

const CREATE_STATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS hearth_state (\
    id INTEGER PRIMARY KEY CHECK (id = 0), \
    seq INTEGER NOT NULL, \
    locked INTEGER NOT NULL, \
    data BLOB NOT NULL)";
const CREATE_TIMERS_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS hearth_timers (key TEXT PRIMARY KEY, due_ms INTEGER NOT NULL)";

/// Event-mode room runtime. One instance lives behind the room's async mutex;
/// `new` after hibernation rebuilds everything from the [`Host`] (SQLite and
/// socket attachments).
pub struct RoomRuntime<R: Room, H: Host> {
    host: H,
    room: R,
    state: Option<R::State>,
    seq: u64,
    locked: bool,
    last_saved: Option<Vec<u8>>,
    last_saved_locked: bool,
    incompatible: bool,
    buckets: Vec<(PlayerId, TokenBucket)>,
}

struct TokenBucket {
    tokens: f64,
    last_ms: u64,
}

impl<R: Room, H: Host> RoomRuntime<R, H> {
    pub fn new(host: H) -> Self {
        Self {
            host,
            room: R::default(),
            state: None,
            seq: 0,
            locked: false,
            last_saved: None,
            last_saved_locked: false,
            incompatible: false,
            buckets: Vec::new(),
        }
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    pub fn state(&self) -> Option<&R::State> {
        self.state.as_ref()
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Register a newly accepted socket. A second socket for a player replaces
    /// the old one: it is closed with `REPLACED` and the seat is carried over.
    pub async fn connect(&mut self, socket: H::Socket, player: PlayerId) {
        let mut kept_seat = None;
        for existing in self.host.sockets() {
            if existing == socket {
                continue;
            }
            if let Some(attachment) = self.host.attachment(&existing) {
                if attachment.player_id == player {
                    kept_seat = attachment.seat;
                    self.host
                        .close(&existing, close::REPLACED, "replaced by a newer socket");
                }
            }
        }
        self.host.set_attachment(
            &socket,
            &Attachment {
                player_id: player,
                seat: kept_seat,
                encoding: None,
            },
        );
    }

    pub async fn message(&mut self, socket: H::Socket, msg: WireMsg) {
        if let Err(err) = self.handle_message(socket, msg).await {
            log_error(err);
        }
    }

    pub async fn close(&mut self, socket: H::Socket) {
        if let Err(err) = self.handle_close(socket).await {
            log_error(err);
        }
    }

    pub async fn alarm(&mut self) {
        if let Err(err) = self.handle_alarm().await {
            log_error(err);
        }
    }

    async fn handle_message(&mut self, socket: H::Socket, msg: WireMsg) -> Result<()> {
        if !self.host.sockets().contains(&socket) {
            return Ok(());
        }
        let Some(attachment) = self.host.attachment(&socket) else {
            return Ok(());
        };
        let wire_encoding = match &msg {
            WireMsg::Binary(_) => Encoding::MsgPack,
            WireMsg::Text(_) => Encoding::Json,
        };
        let size = match &msg {
            WireMsg::Binary(bytes) => bytes.len(),
            WireMsg::Text(text) => text.len(),
        };
        if size > MAX_FRAME_BYTES {
            self.send_error(
                &socket,
                &attachment,
                wire_encoding,
                ErrorCode::FrameTooLarge,
                "frame exceeds the 64 KiB maximum",
                None,
            );
            self.host
                .close(&socket, close::FRAME_TOO_LARGE, "frame too large");
            return Ok(());
        }
        if let WireMsg::Text(text) = &msg {
            if text == PING_TEXT || text == PONG_TEXT {
                // Keepalive text is answered by the DO auto-response and is
                // never a protocol frame.
                return Ok(());
            }
        }
        let joined = attachment.seat.is_some() && attachment.encoding.is_some();
        let frame = match decode(&msg) {
            Ok(frame) => frame,
            Err(_) => {
                self.send_error(
                    &socket,
                    &attachment,
                    wire_encoding,
                    ErrorCode::BadFrame,
                    "frame did not decode",
                    None,
                );
                self.host.close(&socket, close::BAD_FRAME, "bad frame");
                return Ok(());
            }
        };

        match frame {
            Frame::Hello {
                protocol,
                schema_hash,
                opts,
            } => {
                self.handle_hello(
                    socket,
                    attachment,
                    wire_encoding,
                    protocol,
                    schema_hash,
                    opts,
                )
                .await
            }
            _ if !joined => {
                self.send_error(
                    &socket,
                    &attachment,
                    wire_encoding,
                    ErrorCode::NotJoined,
                    "send HELLO before any other frame",
                    None,
                );
                Ok(())
            }
            Frame::Intent { intents } => self.handle_intents(socket, attachment, intents).await,
            Frame::Leave {} => {
                self.run_leave(socket.clone(), attachment, Leave::Left)
                    .await?;
                self.host.close(&socket, 1000, "left");
                Ok(())
            }
            _ => {
                self.send_error(
                    &socket,
                    &attachment,
                    wire_encoding,
                    ErrorCode::BadFrame,
                    "unexpected frame from client",
                    None,
                );
                Ok(())
            }
        }
    }

    async fn handle_hello(
        &mut self,
        socket: H::Socket,
        attachment: Attachment,
        wire_encoding: Encoding,
        protocol: u16,
        schema_hash: String,
        opts: Value,
    ) -> Result<()> {
        let mut attachment = attachment;
        if attachment.seat.is_some() && attachment.encoding.is_some() {
            self.send_error(
                &socket,
                &attachment,
                wire_encoding,
                ErrorCode::BadFrame,
                "already joined",
                None,
            );
            self.host.close(&socket, close::BAD_FRAME, "bad frame");
            return Ok(());
        }
        attachment.encoding = Some(wire_encoding);
        self.host.set_attachment(&socket, &attachment);

        if protocol != PROTOCOL_VERSION {
            self.send_error(
                &socket,
                &attachment,
                wire_encoding,
                ErrorCode::UnsupportedProtocol,
                format!("unsupported protocol {protocol}; this server speaks {PROTOCOL_VERSION}"),
                None,
            );
            self.host
                .close(&socket, close::BAD_FRAME, "unsupported protocol");
            return Ok(());
        }
        let server_hash = R::State::schema_hash_hex();
        if schema_hash != server_hash {
            self.send_error(
                &socket,
                &attachment,
                wire_encoding,
                ErrorCode::SchemaMismatch,
                format!("schema mismatch: client has {schema_hash}, server has {server_hash}"),
                None,
            );
            self.host
                .close(&socket, close::SCHEMA_MISMATCH, "schema mismatch");
            return Ok(());
        }
        if self.ensure_loaded().await? {
            self.send_error(
                &socket,
                &attachment,
                wire_encoding,
                ErrorCode::StateIncompatible,
                "persisted room state was written by an incompatible build",
                None,
            );
            self.host
                .close(&socket, close::STATE_INCOMPATIBLE, "state incompatible");
            return Ok(());
        }

        let replacement_seat = attachment.seat;
        if replacement_seat.is_none() {
            if self.locked {
                self.send_error(
                    &socket,
                    &attachment,
                    wire_encoding,
                    ErrorCode::RoomLocked,
                    "room is locked",
                    None,
                );
                self.host.close(&socket, close::ROOM_LOCKED, "room locked");
                return Ok(());
            }
            if self.joined().len() >= usize::from(R::MAX_CLIENTS) {
                self.send_error(
                    &socket,
                    &attachment,
                    wire_encoding,
                    ErrorCode::RoomFull,
                    "room is full",
                    None,
                );
                self.host.close(&socket, close::ROOM_FULL, "room full");
                return Ok(());
            }
        }
        let seat = match replacement_seat {
            Some(seat) => seat,
            None => self.lowest_free_seat(),
        };
        let who = Client {
            player_id: attachment.player_id.clone(),
            seat,
        };
        let before = self.views();
        let mut effects = Vec::new();

        if replacement_seat.is_none() {
            let roster = self.clients_roster();
            let snapshot = self.snapshot()?;
            let state = self.state.take().expect("state loaded");
            let sql = self.host.sql();
            let now = self.host.now_ms();
            let mut ctx = Ctx::new(state, roster, sql, now);
            let result = self.room.on_join(&mut ctx, &who, JoinOpts(opts)).await;
            let (state, ctx_effects) = ctx.into_parts();
            self.state = Some(state);
            if let Err(err) = result {
                self.state = Some(rmp_serde::from_slice(&snapshot)?);
                self.host.set_attachment(
                    &socket,
                    &Attachment {
                        player_id: attachment.player_id.clone(),
                        seat: None,
                        encoding: Some(wire_encoding),
                    },
                );
                self.send_error(
                    &socket,
                    &attachment,
                    wire_encoding,
                    ErrorCode::JoinRejected,
                    err.to_string(),
                    None,
                );
                self.host
                    .close(&socket, close::JOIN_REJECTED, "join rejected");
                return Ok(());
            }
            effects = ctx_effects;
        }

        let joined_attachment = Attachment {
            player_id: attachment.player_id.clone(),
            seat: Some(seat),
            encoding: Some(wire_encoding),
        };
        self.host.set_attachment(&socket, &joined_attachment);
        // A join during the empty-room grace period cancels the dispose.
        self.timer_delete(EMPTY_TIMER)?;

        let seq = self.persist()?;
        self.patch_joined(&before, seq);
        let view = self
            .state
            .as_ref()
            .expect("state loaded")
            .view(&who.player_id);
        self.send_frame(
            &socket,
            &joined_attachment,
            Frame::JoinOk {
                player_id: who.player_id.clone(),
                seat,
            },
        );
        self.send_frame(
            &socket,
            &joined_attachment,
            Frame::FullState { seq, state: view },
        );

        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        self.apply_rest_and_kicks(rest).await?;
        Ok(())
    }

    async fn handle_intents(
        &mut self,
        socket: H::Socket,
        attachment: Attachment,
        intents: Vec<IntentMsg>,
    ) -> Result<()> {
        if self.ensure_loaded().await? {
            let fallback = attachment.encoding.unwrap_or(Encoding::MsgPack);
            self.send_error(
                &socket,
                &attachment,
                fallback,
                ErrorCode::StateIncompatible,
                "persisted room state was written by an incompatible build",
                None,
            );
            self.host
                .close(&socket, close::STATE_INCOMPATIBLE, "state incompatible");
            return Ok(());
        }
        let fallback = attachment.encoding.unwrap_or(Encoding::MsgPack);
        let who = Client {
            player_id: attachment.player_id.clone(),
            seat: attachment.seat.unwrap_or(0),
        };
        let before = self.views();
        let mut effects: Vec<Effect> = Vec::new();
        let mut accepted: Vec<u32> = Vec::new();
        let mut errors: Vec<(ErrorCode, String, Option<u32>)> = Vec::new();
        let mut rate_limited = false;

        for intent_msg in intents {
            let now = self.host.now_ms();
            if !self.take_token(&attachment.player_id, now) {
                errors.push((
                    ErrorCode::RateLimited,
                    "intent rate limit exceeded".to_string(),
                    Some(intent_msg.seq),
                ));
                rate_limited = true;
                break;
            }
            let Ok(intent) = serde_json::from_value::<R::Intent>(intent_msg.intent) else {
                errors.push((
                    ErrorCode::BadIntent,
                    "intent did not decode".to_string(),
                    Some(intent_msg.seq),
                ));
                continue;
            };
            let snapshot = self.snapshot()?;
            let roster = self.clients_roster();
            let state = self.state.take().expect("state loaded");
            let sql = self.host.sql();
            let now = self.host.now_ms();
            let mut ctx = Ctx::new(state, roster, sql, now);
            let result = self.room.on_intent(&mut ctx, &who, intent).await;
            let (state, ctx_effects) = ctx.into_parts();
            self.state = Some(state);
            match result {
                Ok(()) => {
                    accepted.push(intent_msg.seq);
                    effects.extend(ctx_effects);
                }
                Err(err) => {
                    self.state = Some(rmp_serde::from_slice(&snapshot)?);
                    errors.push((
                        ErrorCode::IntentRejected,
                        err.to_string(),
                        Some(intent_msg.seq),
                    ));
                }
            }
        }

        let seq = self.persist()?;
        self.patch_joined(&before, seq);
        for (code, message, ref_seq) in errors {
            self.send_error(&socket, &attachment, fallback, code, message, ref_seq);
        }
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        if !accepted.is_empty() {
            self.send_frame(
                &socket,
                &attachment,
                Frame::Ack {
                    state_seq: seq,
                    intent_seqs: accepted,
                },
            );
        }
        if rate_limited {
            self.run_leave(socket.clone(), attachment, Leave::Kicked)
                .await?;
            self.host
                .close(&socket, close::RATE_LIMITED, "rate limited");
        }
        self.apply_rest_and_kicks(rest).await?;
        Ok(())
    }

    async fn handle_close(&mut self, socket: H::Socket) -> Result<()> {
        let Some(attachment) = self.host.attachment(&socket) else {
            return Ok(());
        };
        let joined = attachment.seat.is_some() && attachment.encoding.is_some();
        if joined {
            self.run_leave(socket, attachment, Leave::Closed).await?;
        } else {
            self.host.set_attachment(
                &socket,
                &Attachment {
                    player_id: attachment.player_id,
                    seat: None,
                    encoding: None,
                },
            );
        }
        Ok(())
    }

    async fn handle_alarm(&mut self) -> Result<()> {
        if self.ensure_loaded().await? {
            self.host.set_alarm(None).await?;
            return Ok(());
        }
        let now = self.host.now_ms();
        let rows = self.host.sql().exec(
            "SELECT key, due_ms FROM hearth_timers WHERE due_ms <= ? ORDER BY due_ms, key",
            vec![SqlValue::Int(now as i64)],
        )?;
        let mut due = Vec::new();
        for row in rows {
            let Some(key) = row.get("key").and_then(Value::as_str) else {
                continue;
            };
            let due_ms = row.get("due_ms").and_then(Value::as_u64).unwrap_or(0);
            due.push((key.to_string(), due_ms));
        }
        for (key, _) in due {
            // Delete before firing so a failing handler can never loop forever.
            self.timer_delete(&key)?;
            if key == EMPTY_TIMER {
                self.run_dispose().await?;
                break;
            }
            if let Some(user_key) = key.strip_prefix(USER_TIMER_PREFIX) {
                self.run_timer(TimerKey(user_key.to_string())).await?;
                if self.state.is_none() {
                    break;
                }
            }
        }
        if self.state.is_some() {
            self.reschedule_alarm().await?;
        } else {
            self.host.set_alarm(None).await?;
        }
        Ok(())
    }

    async fn run_timer(&mut self, key: TimerKey) -> Result<()> {
        if self.state.is_none() {
            return Ok(());
        }
        let before = self.views();
        let roster = self.clients_roster();
        let snapshot = self.snapshot()?;
        let state = self.state.take().expect("state loaded");
        let sql = self.host.sql();
        let now = self.host.now_ms();
        let mut ctx = Ctx::new(state, roster, sql, now);
        let result = self.room.on_timer(&mut ctx, key).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        let effects = if result.is_ok() {
            effects
        } else {
            // A failed timer handler is not retried: the row is already gone.
            self.state = Some(rmp_serde::from_slice(&snapshot)?);
            Vec::new()
        };
        let seq = self.persist()?;
        self.patch_joined(&before, seq);
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        self.apply_rest_and_kicks(rest).await?;
        Ok(())
    }

    async fn run_leave(
        &mut self,
        socket: H::Socket,
        attachment: Attachment,
        reason: Leave,
    ) -> Result<()> {
        if self.ensure_loaded().await? {
            return Ok(());
        }
        let Some(seat) = attachment.seat else {
            return Ok(());
        };
        if attachment.encoding.is_none() {
            self.host.set_attachment(
                &socket,
                &Attachment {
                    player_id: attachment.player_id,
                    seat: None,
                    encoding: None,
                },
            );
            return Ok(());
        }
        let who = Client {
            player_id: attachment.player_id.clone(),
            seat,
        };
        let before = self.views();
        let roster = self.clients_roster();
        let snapshot = self.snapshot()?;
        let state = self.state.take().expect("state loaded");
        let sql = self.host.sql();
        let now = self.host.now_ms();
        let mut ctx = Ctx::new(state, roster, sql, now);
        let result = self.room.on_leave(&mut ctx, &who, reason).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        let effects = if result.is_ok() {
            effects
        } else {
            self.state = Some(rmp_serde::from_slice(&snapshot)?);
            Vec::new()
        };

        // Mark the socket as left before diffing so it receives no patch.
        self.host.set_attachment(
            &socket,
            &Attachment {
                player_id: attachment.player_id,
                seat: None,
                encoding: None,
            },
        );
        let seq = self.persist()?;
        self.patch_joined(&before, seq);
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        if self.joined().is_empty() {
            let due = self
                .host
                .now_ms()
                .saturating_add(R::EMPTY_GRACE.as_millis() as u64);
            self.timer_set(EMPTY_TIMER, due)?;
        }
        self.apply_rest(rest).await?;
        Ok(())
    }

    async fn run_dispose(&mut self) -> Result<()> {
        if self.incompatible {
            // Never save or wipe once persisted state is known incompatible.
            return Ok(());
        }
        if let Some(state) = self.state.take() {
            let roster = self.clients_roster();
            let sql = self.host.sql();
            let now = self.host.now_ms();
            let mut ctx = Ctx::new(state, roster, sql, now);
            let _ = self.room.on_dispose(&mut ctx).await;
            let _ = ctx.into_parts();
        }
        self.host.wipe().await?;
        for socket in self.host.sockets() {
            self.host.close(&socket, close::DISPOSED, "disposed");
        }
        self.state = None;
        self.seq = 0;
        self.locked = false;
        self.last_saved = None;
        self.last_saved_locked = false;
        self.buckets.clear();
        self.host.set_alarm(None).await?;
        Ok(())
    }

    async fn apply_rest(&mut self, effects: Vec<Effect>) -> Result<()> {
        let mut dispose = false;
        for effect in effects {
            match effect {
                Effect::Timer { key, after } => {
                    let due = self.host.now_ms().saturating_add(after.as_millis() as u64);
                    self.timer_set(&format!("{USER_TIMER_PREFIX}{}", key.0), due)?;
                }
                Effect::CancelTimer { key } => {
                    self.timer_delete(&format!("{USER_TIMER_PREFIX}{}", key.0))?;
                }
                Effect::Lock => {
                    self.locked = true;
                    self.persist()?;
                }
                Effect::Dispose => dispose = true,
                // Events are delivered by the caller; nested kicks are not
                // supported in v0.1 and are ignored here.
                Effect::Event { .. } | Effect::Kick(_) => {}
            }
        }
        self.reschedule_alarm().await?;
        if dispose {
            self.run_dispose().await?;
        }
        Ok(())
    }

    async fn apply_rest_and_kicks(&mut self, effects: Vec<Effect>) -> Result<()> {
        let mut kicks = Vec::new();
        let mut dispose = false;
        let mut rest = Vec::new();
        for effect in effects {
            match effect {
                Effect::Kick(player) => kicks.push(player),
                Effect::Dispose => dispose = true,
                other => rest.push(other),
            }
        }
        self.apply_rest(rest).await?;
        for player in kicks {
            let target = self
                .joined()
                .into_iter()
                .find(|(_, attachment)| attachment.player_id == player);
            if let Some((socket, attachment)) = target {
                self.run_leave(socket.clone(), attachment, Leave::Kicked)
                    .await?;
                self.host.close(&socket, close::KICKED, "kicked");
            }
        }
        if dispose {
            self.run_dispose().await?;
        }
        Ok(())
    }

    async fn ensure_loaded(&mut self) -> Result<bool> {
        if self.state.is_some() {
            return Ok(self.incompatible);
        }
        if self.incompatible {
            return Ok(true);
        }
        self.host.sql().exec(CREATE_STATE_TABLE, Vec::new())?;
        self.host.sql().exec(CREATE_TIMERS_TABLE, Vec::new())?;
        let rows = self.host.sql().exec(
            "SELECT seq, locked, data FROM hearth_state WHERE id = 0",
            Vec::new(),
        )?;
        if let Some(row) = rows.into_iter().next() {
            let seq = row.get("seq").and_then(Value::as_u64).unwrap_or(0);
            let locked = row.get("locked").and_then(Value::as_i64).unwrap_or(0) != 0;
            let data = json_to_bytes(row.get("data"))?;
            match rmp_serde::from_slice::<R::State>(&data) {
                Ok(state) => {
                    self.state = Some(state);
                    self.seq = seq;
                    self.locked = locked;
                    self.last_saved = Some(data);
                    self.last_saved_locked = locked;
                }
                Err(_) => {
                    self.incompatible = true;
                }
            }
        } else {
            let state = R::State::default();
            let bytes = rmp_serde::to_vec_named(&state)?;
            self.state = Some(state);
            self.seq = 0;
            self.locked = false;
            self.last_saved = Some(bytes);
            self.last_saved_locked = false;
        }
        Ok(self.incompatible)
    }

    /// Persist the state if it (or the lock flag) changed. The sequence number
    /// increments once per changed flush, never per message.
    fn persist(&mut self) -> Result<u64> {
        if self.incompatible {
            return Ok(self.seq);
        }
        let Some(state) = &self.state else {
            return Ok(self.seq);
        };
        let bytes = rmp_serde::to_vec_named(state)?;
        let data_changed = self.last_saved.as_ref() != Some(&bytes);
        let locked_changed = self.last_saved_locked != self.locked;
        if data_changed {
            self.seq += 1;
        }
        if data_changed || locked_changed {
            self.host.sql().exec(
                "INSERT OR REPLACE INTO hearth_state (id, seq, locked, data) VALUES (0, ?, ?, ?)",
                vec![
                    SqlValue::Int(self.seq as i64),
                    SqlValue::Int(i64::from(self.locked)),
                    SqlValue::Blob(bytes.clone()),
                ],
            )?;
            self.last_saved = Some(bytes);
            self.last_saved_locked = self.locked;
        }
        Ok(self.seq)
    }

    fn snapshot(&self) -> Result<Vec<u8>> {
        Ok(rmp_serde::to_vec_named(
            self.state.as_ref().expect("state loaded"),
        )?)
    }

    fn timer_set(&self, key: &str, due_ms: u64) -> Result<()> {
        self.host.sql().exec(
            "INSERT OR REPLACE INTO hearth_timers (key, due_ms) VALUES (?, ?)",
            vec![SqlValue::Text(key.to_owned()), SqlValue::Int(due_ms as i64)],
        )?;
        Ok(())
    }

    fn timer_delete(&self, key: &str) -> Result<()> {
        self.host.sql().exec(
            "DELETE FROM hearth_timers WHERE key = ?",
            vec![SqlValue::Text(key.to_owned())],
        )?;
        Ok(())
    }

    async fn reschedule_alarm(&mut self) -> Result<()> {
        if self.state.is_none() {
            self.host.set_alarm(None).await?;
            return Ok(());
        }
        let rows = self
            .host
            .sql()
            .exec("SELECT MIN(due_ms) AS next FROM hearth_timers", Vec::new())?;
        let next = rows
            .first()
            .and_then(|row| row.get("next"))
            .and_then(Value::as_u64);
        self.host.set_alarm(next).await
    }

    fn take_token(&mut self, player: &PlayerId, now_ms: u64) -> bool {
        if let Some(index) = self.buckets.iter().position(|(id, _)| id == player) {
            let bucket = &mut self.buckets[index].1;
            let elapsed = now_ms.saturating_sub(bucket.last_ms);
            bucket.tokens =
                (bucket.tokens + elapsed as f64 * INTENT_REFILL_PER_MS).min(INTENT_BUCKET_CAPACITY);
            bucket.last_ms = now_ms;
            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                true
            } else {
                false
            }
        } else {
            self.buckets.push((
                player.clone(),
                TokenBucket {
                    tokens: INTENT_BUCKET_CAPACITY - 1.0,
                    last_ms: now_ms,
                },
            ));
            true
        }
    }

    fn joined(&self) -> Vec<(H::Socket, Attachment)> {
        let mut joined = Vec::new();
        for socket in self.host.sockets() {
            if let Some(attachment) = self.host.attachment(&socket) {
                if attachment.seat.is_some() && attachment.encoding.is_some() {
                    joined.push((socket, attachment));
                }
            }
        }
        joined
    }

    fn clients_roster(&self) -> Vec<Client> {
        self.joined()
            .into_iter()
            .map(|(_, attachment)| Client {
                player_id: attachment.player_id,
                seat: attachment.seat.unwrap_or(0),
            })
            .collect()
    }

    fn views(&self) -> Vec<(H::Socket, Value)> {
        let Some(state) = &self.state else {
            return Vec::new();
        };
        self.joined()
            .into_iter()
            .map(|(socket, attachment)| (socket, state.view(&attachment.player_id)))
            .collect()
    }

    fn patch_joined(&self, before: &[(H::Socket, Value)], seq: u64) {
        let Some(state) = &self.state else {
            return;
        };
        for (socket, attachment) in self.joined() {
            let Some((_, old)) = before.iter().find(|(candidate, _)| candidate == &socket) else {
                continue;
            };
            let new = state.view(&attachment.player_id);
            let ops = diff(old, &new);
            if !ops.is_empty() {
                self.send_frame(&socket, &attachment, Frame::Patch { seq, ops });
            }
        }
    }

    fn deliver_events(&self, events: &[Effect]) {
        for effect in events {
            if let Effect::Event { to, name, data } = effect {
                let frame = Frame::Event {
                    name: name.clone(),
                    data: data.clone(),
                };
                match to {
                    Some(player) => self.send_to_player(player, frame),
                    None => self.broadcast_frame(frame),
                }
            }
        }
    }

    fn send_to_player(&self, player: &PlayerId, frame: Frame) {
        for (socket, attachment) in self.joined() {
            if &attachment.player_id == player {
                self.send_frame(&socket, &attachment, frame.clone());
            }
        }
    }

    fn broadcast_frame(&self, frame: Frame) {
        for (socket, attachment) in self.joined() {
            self.send_frame(&socket, &attachment, frame.clone());
        }
    }

    fn send_frame(&self, socket: &H::Socket, attachment: &Attachment, frame: Frame) {
        let encoding = attachment.encoding.unwrap_or(Encoding::MsgPack);
        self.host.send(socket, encode(&frame, encoding));
    }

    fn send_error(
        &self,
        socket: &H::Socket,
        attachment: &Attachment,
        fallback: Encoding,
        code: ErrorCode,
        message: impl Into<String>,
        ref_seq: Option<u32>,
    ) {
        let frame = Frame::Error {
            code,
            message: message.into(),
            ref_seq,
        };
        self.host.send(
            socket,
            encode(&frame, attachment.encoding.unwrap_or(fallback)),
        );
    }

    fn lowest_free_seat(&self) -> u8 {
        let taken: Vec<u8> = self
            .joined()
            .iter()
            .filter_map(|(_, attachment)| attachment.seat)
            .collect();
        (0..R::MAX_CLIENTS)
            .find(|seat| !taken.contains(seat))
            .unwrap_or(0)
    }
}

fn split_events(effects: Vec<Effect>) -> (Vec<Effect>, Vec<Effect>) {
    effects
        .into_iter()
        .partition(|effect| matches!(effect, Effect::Event { .. }))
}

/// Blob columns surface as JSON arrays of byte values through the [`Sql`]
/// trait, which keeps that interface JSON-shaped while still round-tripping
/// the runtime's state snapshot.
fn json_to_bytes(value: Option<&Value>) -> Result<Vec<u8>> {
    let Some(Value::Array(items)) = value else {
        anyhow::bail!("hearth_state.data is not a byte array");
    };
    let mut bytes = Vec::with_capacity(items.len());
    for item in items {
        let Some(byte) = item.as_u64().and_then(|value| u8::try_from(value).ok()) else {
            anyhow::bail!("hearth_state.data contains a non-byte value");
        };
        bytes.push(byte);
    }
    Ok(bytes)
}

fn log_error(_err: anyhow::Error) {
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("hearth runtime error: {_err:#}");
}
