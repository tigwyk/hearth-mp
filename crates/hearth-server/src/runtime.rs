use std::rc::Rc;

use hearth_protocol::{
    close, decode, encode, Encoding, ErrorCode, Frame, IntentMsg, PlayerId, Value, WireMsg,
    MAX_FRAME_BYTES, PING_TEXT, PONG_TEXT, PROTOCOL_VERSION,
};
use hearth_sync::{diff, HearthState};

use crate::admin::{Cause, InspectClient, InspectDoc, TailMsg};
use crate::call::{rpc_from_value, rpc_to_value, CallError, Caller, Envelope, HostCaller, Target};
use crate::ctx::{Ctx, Effect};
use crate::host::{Attachment, Host, RoomName, RoomReport, SqlValue};
use crate::outbox;
use crate::room::{Client, JoinOpts, Leave, Room, RoomRpc, TimerKey};
use crate::store;
use crate::Result;

/// Internal key for the empty-room grace timer. User timers are stored under a
/// `u:` prefix, so internal keys can never collide with user keys.
const EMPTY_TIMER: &str = "h:empty";
/// Internal key prefix for one held seat's reconnect grace timer.
const GRACE_TIMER_PREFIX: &str = "h:grace:";
const USER_TIMER_PREFIX: &str = "u:";
/// Binding of the optional room registry. Reports go through the outbox when
/// the deployment declares it, and a room without it never reports.
const REGISTRY_BINDING: &str = "HEARTH_REGISTRY";
/// The single registry instance (the v0.2c shortcut, unchanged).
const REGISTRY_NAME: &str = "global";

/// Per-player intent bucket: capacity 120 intents, refill 60/s. Hibernation
/// resets the bucket, which is intended: a room only hibernates when quiet.
const INTENT_BUCKET_CAPACITY: f64 = 120.0;
const INTENT_REFILL_PER_MS: f64 = 60.0 / 1000.0;

/// Event-mode room runtime. One instance lives behind the room's async mutex;
/// `new` after hibernation rebuilds everything from the [`Host`] (SQLite and
/// socket attachments).
pub struct RoomRuntime<R: Room, H: Host> {
    host: Rc<H>,
    /// The Durable Object binding this room class is deployed as.
    binding: &'static str,
    room: R,
    state: Option<R::State>,
    seq: u64,
    locked: bool,
    last_saved: Option<Vec<u8>>,
    last_saved_locked: bool,
    incompatible: bool,
    buckets: Vec<(PlayerId, TokenBucket)>,
    /// The room's identity and creation time, persisted in `hearth_meta`.
    identity: Option<RoomName>,
    created_ms: u64,
    /// Whether this runtime instance created `hearth_meta` (so it has never
    /// reported `Open`).
    meta_created_here: bool,
    /// Registry report bookkeeping: `Open` was sent, and the last reported
    /// seated count (live + held).
    open_reported: bool,
    last_players: Option<u8>,
    /// The call chain of the pass in progress, empty for every non-call pass.
    chain: Vec<Target>,
    /// The name a call's header gave this room, used before identity is known.
    call_name: String,
}

struct TokenBucket {
    tokens: f64,
    last_ms: u64,
}

impl<R: Room, H: Host + 'static> RoomRuntime<R, H> {
    pub fn new(host: H, binding: &'static str) -> Self {
        Self {
            host: Rc::new(host),
            binding,
            room: R::default(),
            state: None,
            seq: 0,
            locked: false,
            last_saved: None,
            last_saved_locked: false,
            incompatible: false,
            buckets: Vec::new(),
            identity: None,
            created_ms: 0,
            meta_created_here: false,
            open_reported: false,
            last_players: None,
            chain: Vec::new(),
            call_name: String::new(),
        }
    }

    pub fn host(&self) -> &H {
        self.host.as_ref()
    }

    pub fn state(&self) -> Option<&R::State> {
        self.state.as_ref()
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The room's own address: its class binding plus its persisted name.
    fn me(&self) -> Target {
        let name = match &self.identity {
            Some(room) => room.name.clone(),
            None => self.call_name.clone(),
        };
        Target::new(self.binding, name)
    }

    /// A fresh call handle for `Ctx`: this room's address, the chain of the
    /// pass in progress (empty except inside an `on_call`) and the host.
    fn caller(&self) -> Caller {
        Caller::new(
            self.me(),
            self.chain.clone(),
            Rc::new(HostCaller {
                host: Rc::clone(&self.host),
            }),
        )
    }

    fn make_ctx(&self, state: R::State, roster: Vec<Client>) -> Ctx<R> {
        let sql = self.host.sql_handle();
        let now = self.host.now_ms();
        Ctx::new(state, roster, sql, now, self.caller())
    }

    /// Persist the room's identity without a connect. Test worlds and the
    /// matchmaker need a named room before any socket exists.
    pub fn remember_identity(&mut self, room: &RoomName) -> Result<()> {
        self.remember_room(room)
    }

    /// Register a newly accepted socket. A second socket for a player replaces
    /// the old one: it is closed with `REPLACED` and the seat is carried over.
    ///
    /// The first connect ever also persists the room's identity and creation
    /// time in `hearth_meta`, so reports and inspect work after a hibernation
    /// without another connect.
    pub async fn connect(&mut self, socket: H::Socket, player: PlayerId, room: RoomName) {
        if let Err(err) = self.remember_room(&room) {
            self.log_json(serde_json::json!({
                "hearth": "meta_error",
                "room": self.room_label(),
                "msg": err.to_string(),
            }));
        }
        let mut kept_seat = None;
        for existing in self.host.sockets() {
            if existing == socket {
                continue;
            }
            if let Some(attachment) = self.host.attachment(&existing) {
                if attachment.player_id == player {
                    kept_seat = attachment.seat;
                    // Detach the old socket before closing it. Production
                    // delivers a late websocket_close/websocket_error for this
                    // socket *after* this call, and an attachment that still
                    // looks joined would make that event hold a phantom seat for
                    // a player who is currently live (duplicate roster entry,
                    // wrong seat count, late on_leave). The mem harness cannot
                    // replay that late event, so there is no test for it here.
                    self.host.set_attachment(
                        &existing,
                        &Attachment {
                            player_id: player.clone(),
                            seat: None,
                            encoding: None,
                            admin: false,
                        },
                    );
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
                admin: false,
            },
        );
    }

    /// Persist the room's identity on the first connect ever and adopt the
    /// stored (stable) values on every later connect.
    fn remember_room(&mut self, room: &RoomName) -> Result<()> {
        let existed = self.has_table("hearth_meta")?;
        self.host.sql().exec(store::CREATE_META_TABLE, Vec::new())?;
        outbox::create(self.host.sql())?;
        let created_ms = self.host.now_ms().to_string();
        for (key, value) in [
            ("room_type", room.room_type.as_str()),
            ("name", room.name.as_str()),
            ("created_ms", created_ms.as_str()),
        ] {
            self.host.sql().exec(
                "INSERT OR IGNORE INTO hearth_meta (key, value) VALUES (?, ?)",
                vec![
                    SqlValue::Text(key.to_owned()),
                    SqlValue::Text(value.to_owned()),
                ],
            )?;
        }
        if !existed {
            self.meta_created_here = true;
        }
        if self.identity.is_none() {
            if let Some((stored, created_ms)) = self.read_meta()? {
                self.identity = Some(stored);
                self.created_ms = created_ms;
            }
        }
        Ok(())
    }

    /// Register an admin tail socket: it gets a snapshot of every full-state
    /// change and never becomes a player, a seat or a reason to stay awake.
    pub fn connect_admin(&mut self, socket: H::Socket) {
        self.host.set_attachment(
            &socket,
            &Attachment {
                player_id: PlayerId("admin".to_owned()),
                seat: None,
                encoding: None,
                admin: true,
            },
        );
    }

    /// The read-only admin view of this room, or `None` when the room never
    /// existed. Checks `sqlite_master` first and never creates or writes
    /// anything.
    pub fn inspect(&mut self) -> Result<Option<InspectDoc>> {
        if !self.has_table("hearth_meta")? {
            return Ok(None);
        }
        let Some((room, created_ms)) = self.read_meta()? else {
            return Ok(None);
        };
        let mut seq = 0;
        let mut locked = false;
        let mut state_bytes = 0;
        let mut incompatible = self.incompatible;
        let mut state = Some(serde_json::to_value(R::State::default())?);
        if self.has_table("hearth_state")? {
            let rows = self.host.sql().exec(
                "SELECT seq, locked, data FROM hearth_state WHERE id = 0",
                Vec::new(),
            )?;
            if let Some(row) = rows.into_iter().next() {
                seq = row.get("seq").and_then(Value::as_u64).unwrap_or(0);
                locked = row.get("locked").and_then(Value::as_i64).unwrap_or(0) != 0;
                let data = store::json_to_bytes(row.get("data"))?;
                state_bytes = data.len();
                match rmp_serde::from_slice::<R::State>(&data) {
                    Ok(decoded) => state = Some(serde_json::to_value(&decoded)?),
                    Err(_) => {
                        incompatible = true;
                        state = None;
                    }
                }
            }
        }
        let clients = self.inspect_clients()?;
        let timers = if self.has_table("hearth_timers")? {
            self.host
                .sql()
                .exec(
                    "SELECT key, due_ms FROM hearth_timers ORDER BY key",
                    Vec::new(),
                )?
                .into_iter()
                .filter_map(|row| {
                    let key = row.get("key").and_then(Value::as_str)?.to_owned();
                    let due = row.get("due_ms").and_then(Value::as_u64)?;
                    Some((key, due))
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Some(InspectDoc {
            room,
            created_ms,
            seq,
            locked,
            incompatible,
            clients,
            timers,
            state,
            state_bytes,
        }))
    }

    /// Admin force-dispose: works for compatible and incompatible rooms, and is
    /// a no-op (`false`) for a room that never existed. Runs `on_dispose` only
    /// when the persisted state decodes; errors are logged and ignored.
    pub async fn force_dispose(&mut self) -> Result<bool> {
        if !self.has_table("hearth_meta")? {
            return Ok(false);
        }
        if self.identity.is_none() {
            if let Some((room, created_ms)) = self.read_meta()? {
                self.identity = Some(room);
                self.created_ms = created_ms;
            }
        }
        let room = self.room_or_unknown();
        let label = self.room_label();

        let mut persisted: Option<R::State> = Some(R::State::default());
        let mut incompatible = self.incompatible;
        if self.has_table("hearth_state")? {
            let rows = self.host.sql().exec(
                "SELECT seq, locked, data FROM hearth_state WHERE id = 0",
                Vec::new(),
            )?;
            if let Some(row) = rows.into_iter().next() {
                let data = store::json_to_bytes(row.get("data"))?;
                match rmp_serde::from_slice::<R::State>(&data) {
                    Ok(state) => persisted = Some(state),
                    Err(_) => {
                        persisted = None;
                        incompatible = true;
                    }
                }
            }
        }
        if incompatible && !self.incompatible {
            self.incompatible = true;
            self.log_json(serde_json::json!({
                "hearth": "state_incompatible",
                "room": label,
            }));
        }

        let admin_before = self.admin_before_state(persisted.as_ref());
        if !incompatible {
            if let Some(state) = persisted {
                let roster = self.admin_roster()?;
                let mut ctx = self.make_ctx(state, roster);
                if let Err(err) = self.room.on_dispose(&mut ctx).await {
                    self.log_json(serde_json::json!({
                        "hearth": "handler_error",
                        "room": label,
                        "msg": err.to_string(),
                    }));
                }
                let _ = ctx.into_parts();
            }
        }

        // The room is gone from here on, so the tail's "after" state is absent.
        self.state = None;
        self.admin_after(admin_before, vec![Cause::Dispose], Vec::new(), self.seq);
        for socket in self.host.sockets() {
            self.host.close(&socket, close::DISPOSED, "disposed");
        }
        // The Disposed report is the last row this room ever writes: deliver it
        // before the wipe, because a disposed room cannot retry afterwards.
        self.queue_report(RoomReport::Disposed { room });
        self.flush_outbox().await;
        self.host.wipe().await?;
        self.reset_after_dispose();
        self.host.set_alarm(None).await?;
        self.log_json(serde_json::json!({
            "hearth": "dispose",
            "room": label,
        }));
        Ok(true)
    }

    fn report_players(&mut self) {
        if !self.open_reported {
            return;
        }
        let Ok(players) = self.occupied() else {
            return;
        };
        let players = players as u8;
        if self.last_players == Some(players) {
            return;
        }
        self.last_players = Some(players);
        self.queue_report(RoomReport::Players {
            room: self.room_or_unknown(),
            players,
        });
    }

    fn report_open(&mut self) {
        if self.open_reported {
            return;
        }
        let Ok(players) = self.occupied() else {
            return;
        };
        self.open_reported = true;
        self.last_players = Some(players as u8);
        self.queue_report(RoomReport::Open {
            room: self.room_or_unknown(),
            created_ms: self.created_ms,
            players: players as u8,
        });
    }

    /// Queue a report as a durable notify to the registry. The registry is
    /// optional: without its binding a room never reports, and a registry
    /// outage only shows up in the logs and the outbox.
    fn queue_report(&self, report: RoomReport) {
        if !self.host.has_binding(REGISTRY_BINDING) {
            return;
        }
        let Ok(body) = serde_json::to_value(&report) else {
            return;
        };
        if let Err(err) = outbox::enqueue(
            self.host.sql(),
            REGISTRY_BINDING,
            REGISTRY_NAME,
            &body,
            self.host.now_ms(),
        ) {
            self.log_json(serde_json::json!({
                "hearth": "report_error",
                "room": self.room_label(),
                "msg": err.to_string(),
            }));
        }
    }

    /// The end of every handler pass: report the seated count, then deliver
    /// whatever committed notifies are pending.
    async fn finish_pass(&mut self) {
        self.report_players();
        self.flush_outbox().await;
    }

    /// Send every pending outbox row, retrying the ones that fail. Delivery
    /// errors never reach play: they are logged and retried by the alarm.
    async fn flush_outbox(&mut self) {
        let transport = HostCaller {
            host: Rc::clone(&self.host),
        };
        let now = self.host.now_ms();
        let me = self.me();
        let delivered = outbox::deliver(self.host.sql(), &me, now, &transport).await;
        match delivered {
            Ok(dropped) => {
                for row in dropped {
                    self.log_json(serde_json::json!({
                        "hearth": "outbox_dropped",
                        "room": self.room_label(),
                        "to": format!("{}/{}", row.binding, row.name),
                        "first_ms": row.first_ms,
                    }));
                }
            }
            Err(err) => self.log_json(serde_json::json!({
                "hearth": "outbox_error",
                "room": self.room_label(),
                "msg": err.to_string(),
            })),
        }
        let _ = self.reschedule_alarm().await;
    }

    fn log_json(&self, line: serde_json::Value) {
        self.host
            .log_line(&serde_json::to_string(&line).unwrap_or_default());
    }

    fn room_or_unknown(&self) -> RoomName {
        self.identity.clone().unwrap_or_else(|| RoomName {
            room_type: String::new(),
            name: String::new(),
        })
    }

    fn room_label(&self) -> String {
        match &self.identity {
            Some(room) => format!("{}/{}", room.room_type, room.name),
            None => String::new(),
        }
    }

    fn reset_after_dispose(&mut self) {
        self.state = None;
        self.seq = 0;
        self.locked = false;
        self.last_saved = None;
        self.last_saved_locked = false;
        self.incompatible = false;
        self.buckets.clear();
        self.identity = None;
        self.created_ms = 0;
        self.meta_created_here = false;
        self.open_reported = false;
        self.last_players = None;
        self.chain.clear();
        self.call_name.clear();
    }

    /// The admin tail's view of a state value, counting the render for the
    /// no-tail no-extra-work test.
    fn full_state_json(&self) -> Result<Value> {
        let Some(state) = &self.state else {
            return Ok(Value::Null);
        };
        self.host.count_full_state();
        Ok(serde_json::to_value(state)?)
    }

    fn admin_sockets(&self) -> Vec<H::Socket> {
        self.host
            .sockets()
            .into_iter()
            .filter(|socket| {
                self.host
                    .attachment(socket)
                    .is_some_and(|attachment| attachment.admin)
            })
            .collect()
    }

    /// Full unfiltered state before a handler pass, or `None` when no admin
    /// tail is attached (so a room without a tail does no extra work).
    fn admin_before(&self) -> Option<Value> {
        if self.admin_sockets().is_empty() {
            return None;
        }
        self.full_state_json().ok()
    }

    fn admin_before_state(&self, state: Option<&R::State>) -> Option<Value> {
        if self.admin_sockets().is_empty() {
            return None;
        }
        let Some(state) = state else {
            return Some(Value::Null);
        };
        self.host.count_full_state();
        serde_json::to_value(state).ok()
    }

    /// Send one admin-tail frame when the pass changed the full server state or
    /// rejected intents.
    fn admin_after(&self, before: Option<Value>, cause: Vec<Cause>, rejected: Vec<u32>, seq: u64) {
        let Some(before) = before else {
            return;
        };
        let sockets = self.admin_sockets();
        if sockets.is_empty() {
            return;
        }
        let Ok(after) = self.full_state_json() else {
            return;
        };
        let ops = diff(&before, &after);
        if ops.is_empty() && rejected.is_empty() {
            return;
        }
        let msg = TailMsg {
            seq,
            cause,
            ops,
            rejected,
        };
        let Ok(text) = serde_json::to_string(&msg) else {
            return;
        };
        for socket in sockets {
            self.host.send(&socket, WireMsg::Text(text.clone()));
        }
    }

    /// Whether a table exists, without creating it.
    fn has_table(&self, name: &str) -> Result<bool> {
        let rows = self.host.sql().exec(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
            vec![SqlValue::Text(name.to_owned())],
        )?;
        Ok(!rows.is_empty())
    }

    /// The persisted room identity, without creating anything.
    fn read_meta(&self) -> Result<Option<(RoomName, u64)>> {
        let rows = self
            .host
            .sql()
            .exec("SELECT key, value FROM hearth_meta", Vec::new())?;
        let mut room_type = None;
        let mut name = None;
        let mut created_ms = None;
        for row in rows {
            let Some(key) = row.get("key").and_then(Value::as_str) else {
                continue;
            };
            let Some(value) = row.get("value").and_then(Value::as_str) else {
                continue;
            };
            match key {
                "room_type" => room_type = Some(value.to_owned()),
                "name" => name = Some(value.to_owned()),
                "created_ms" => created_ms = value.parse::<u64>().ok(),
                _ => {}
            }
        }
        match (room_type, name, created_ms) {
            (Some(room_type), Some(name), Some(created_ms)) => {
                Ok(Some((RoomName { room_type, name }, created_ms)))
            }
            _ => Ok(None),
        }
    }

    /// Live seats plus held seats, tolerating a room whose tables do not exist
    /// yet (a connect-only room can be force-disposed).
    fn admin_roster(&self) -> Result<Vec<Client>> {
        let mut roster: Vec<Client> = self
            .joined()
            .into_iter()
            .map(|(_, attachment)| Client {
                player_id: attachment.player_id,
                seat: attachment.seat.unwrap_or(0),
                connected: true,
            })
            .collect();
        if self.has_table("hearth_held")? {
            roster.extend(
                self.held_rows()?
                    .into_iter()
                    .map(|(player_id, seat)| Client {
                        player_id,
                        seat,
                        connected: false,
                    }),
            );
        }
        Ok(roster)
    }

    /// The inspect document's seat list: live sockets then held seats.
    fn inspect_clients(&self) -> Result<Vec<InspectClient>> {
        let mut clients: Vec<InspectClient> = self
            .joined()
            .into_iter()
            .map(|(_, attachment)| InspectClient {
                player_id: attachment.player_id,
                seat: attachment.seat.unwrap_or(0),
                connected: true,
                held_until_ms: None,
            })
            .collect();
        if self.has_table("hearth_held")? {
            let rows = self.host.sql().exec(
                "SELECT player_id, seat, until_ms FROM hearth_held ORDER BY player_id",
                Vec::new(),
            )?;
            for row in rows {
                let Some(player) = row.get("player_id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(seat) = row.get("seat").and_then(Value::as_u64) else {
                    continue;
                };
                let until = row.get("until_ms").and_then(Value::as_u64);
                clients.push(InspectClient {
                    player_id: PlayerId(player.to_owned()),
                    seat: seat as u8,
                    connected: false,
                    held_until_ms: until,
                });
            }
        }
        Ok(clients)
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
        if attachment.admin {
            // A tail socket never speaks; its close is handled separately.
            return Ok(());
        }
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
        let server_hash = crate::schema_hash_hex::<R>();
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

        // A held seat resumes without going through on_join, and works while
        // the room is locked or full.
        if attachment.seat.is_none() {
            if let Some(seat) = self.held_seat(&attachment.player_id)? {
                return self.resume(socket, attachment, wire_encoding, seat).await;
            }
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
            if self.occupied()? >= usize::from(R::MAX_CLIENTS) {
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
            None => self.lowest_free_seat()?,
        };
        let who = Client {
            player_id: attachment.player_id.clone(),
            seat,
            connected: true,
        };
        let before = self.views();
        let mut effects = Vec::new();
        let mut admin_before = None;

        if replacement_seat.is_none() {
            admin_before = self.admin_before();
            let roster = self.clients_roster()?;
            let snapshot = self.snapshot()?;
            let state = self.state.take().expect("state loaded");
            let mut ctx = self.make_ctx(state, roster);
            let result = self.room.on_join(&mut ctx, &who, JoinOpts(opts)).await;
            let (state, ctx_effects) = ctx.into_parts();
            self.state = Some(state);
            if let Err(err) = result {
                let message = err.to_string();
                self.state = Some(rmp_serde::from_slice(&snapshot)?);
                self.log_json(serde_json::json!({
                    "hearth": "handler_error",
                    "room": self.room_label(),
                    "player": attachment.player_id.0,
                    "msg": message,
                }));
                self.host.set_attachment(
                    &socket,
                    &Attachment {
                        player_id: attachment.player_id.clone(),
                        seat: None,
                        encoding: Some(wire_encoding),
                        admin: false,
                    },
                );
                self.send_error(
                    &socket,
                    &attachment,
                    wire_encoding,
                    ErrorCode::JoinRejected,
                    message,
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
            admin: false,
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

        if replacement_seat.is_none() {
            self.admin_after(
                admin_before,
                vec![Cause::Join {
                    player: who.player_id.clone(),
                }],
                Vec::new(),
                seq,
            );
            self.report_open();
            self.log_json(serde_json::json!({
                "hearth": "join",
                "room": self.room_label(),
                "player": who.player_id.0,
            }));
        }

        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        self.apply_rest_and_kicks(rest).await?;
        self.finish_pass().await;
        Ok(())
    }

    /// Complete a resumed `HELLO`: delete the held row and its grace timer, send
    /// `JOIN_OK` + `FULL_STATE`, then run `on_reconnect` as a handler pass.
    async fn resume(
        &mut self,
        socket: H::Socket,
        attachment: Attachment,
        wire_encoding: Encoding,
        seat: u8,
    ) -> Result<()> {
        self.expire_held(&attachment.player_id)?;
        let who = Client {
            player_id: attachment.player_id.clone(),
            seat,
            connected: true,
        };
        let joined_attachment = Attachment {
            player_id: attachment.player_id.clone(),
            seat: Some(seat),
            encoding: Some(wire_encoding),
            admin: false,
        };
        self.host.set_attachment(&socket, &joined_attachment);
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
            Frame::FullState {
                seq: self.seq,
                state: view,
            },
        );

        let before = self.views();
        let admin_before = self.admin_before();
        let roster = self.clients_roster()?;
        let snapshot = self.snapshot()?;
        let state = self.state.take().expect("state loaded");
        let mut ctx = self.make_ctx(state, roster);
        let result = self.room.on_reconnect(&mut ctx, &who).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        let effects = if result.is_ok() {
            effects
        } else {
            self.state = Some(rmp_serde::from_slice(&snapshot)?);
            Vec::new()
        };
        let seq = self.persist()?;
        self.patch_joined(&before, seq);
        self.admin_after(
            admin_before,
            vec![Cause::Reconnect {
                player: who.player_id.clone(),
            }],
            Vec::new(),
            seq,
        );
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        self.apply_rest_and_kicks(rest).await?;
        self.finish_pass().await;
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
            connected: true,
        };
        let before = self.views();
        let admin_before = self.admin_before();
        let mut effects: Vec<Effect> = Vec::new();
        let mut causes: Vec<Cause> = Vec::new();
        let mut rejected: Vec<u32> = Vec::new();
        let mut accepted: Vec<u32> = Vec::new();
        let mut errors: Vec<(ErrorCode, String, Option<u32>)> = Vec::new();
        let mut rate_limited = false;

        for intent_msg in intents {
            let now = self.host.now_ms();
            if !self.take_token(&attachment.player_id, now) {
                rejected.push(intent_msg.seq);
                errors.push((
                    ErrorCode::RateLimited,
                    "intent rate limit exceeded".to_string(),
                    Some(intent_msg.seq),
                ));
                rate_limited = true;
                break;
            }
            let Ok(intent) = serde_json::from_value::<R::Intent>(intent_msg.intent) else {
                rejected.push(intent_msg.seq);
                errors.push((
                    ErrorCode::BadIntent,
                    "intent did not decode".to_string(),
                    Some(intent_msg.seq),
                ));
                continue;
            };
            let snapshot = self.snapshot()?;
            let roster = self.clients_roster()?;
            let state = self.state.take().expect("state loaded");
            let mut ctx = self.make_ctx(state, roster);
            let result = self.room.on_intent(&mut ctx, &who, intent).await;
            let (state, ctx_effects) = ctx.into_parts();
            self.state = Some(state);
            match result {
                Ok(()) => {
                    accepted.push(intent_msg.seq);
                    causes.push(Cause::Intent {
                        player: who.player_id.clone(),
                        intent_seq: intent_msg.seq,
                    });
                    effects.extend(ctx_effects);
                }
                Err(err) => {
                    rejected.push(intent_msg.seq);
                    self.state = Some(rmp_serde::from_slice(&snapshot)?);
                    self.log_json(serde_json::json!({
                        "hearth": "handler_error",
                        "room": self.room_label(),
                        "player": who.player_id.0,
                        "msg": err.to_string(),
                    }));
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
        self.admin_after(admin_before, causes, rejected, seq);
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
        self.finish_pass().await;
        Ok(())
    }

    async fn handle_close(&mut self, socket: H::Socket) -> Result<()> {
        let Some(attachment) = self.host.attachment(&socket) else {
            return Ok(());
        };
        if attachment.admin {
            // An admin tail just detaches: no leave, no hold, no report.
            return Ok(());
        }
        let joined = attachment.seat.is_some() && attachment.encoding.is_some();
        if joined {
            if R::RECONNECT_GRACE.is_zero() {
                self.run_leave(socket, attachment, Leave::Closed).await?;
            } else {
                self.run_hold(socket, attachment).await?;
            }
        } else {
            self.host.set_attachment(
                &socket,
                &Attachment {
                    player_id: attachment.player_id,
                    seat: None,
                    encoding: None,
                    admin: false,
                },
            );
        }
        Ok(())
    }

    /// Hold a dropped client's seat in SQLite, start its grace timer and run
    /// `on_disconnect` as a handler pass. `on_leave` does not run.
    async fn run_hold(&mut self, socket: H::Socket, attachment: Attachment) -> Result<()> {
        if self.ensure_loaded().await? {
            return Ok(());
        }
        let Some(seat) = attachment.seat else {
            return Ok(());
        };
        let player = attachment.player_id.clone();
        let due = self
            .host
            .now_ms()
            .saturating_add(R::RECONNECT_GRACE.as_millis() as u64);
        self.host.sql().exec(
            "INSERT OR REPLACE INTO hearth_held (player_id, seat, until_ms) VALUES (?, ?, ?)",
            vec![
                SqlValue::Text(player.0.clone()),
                SqlValue::Int(i64::from(seat)),
                SqlValue::Int(due as i64),
            ],
        )?;
        self.timer_set(&grace_timer_key(&player), due)?;

        // The socket is gone: exclude it from the roster's live clients and from
        // every patch.
        self.host.set_attachment(
            &socket,
            &Attachment {
                player_id: player.clone(),
                seat: None,
                encoding: None,
                admin: false,
            },
        );
        let who = Client {
            player_id: player,
            seat,
            connected: false,
        };
        let before = self.views();
        let admin_before = self.admin_before();
        let roster = self.clients_roster()?;
        let snapshot = self.snapshot()?;
        let state = self.state.take().expect("state loaded");
        let mut ctx = self.make_ctx(state, roster);
        let result = self.room.on_disconnect(&mut ctx, &who).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        let effects = if result.is_ok() {
            effects
        } else {
            self.state = Some(rmp_serde::from_slice(&snapshot)?);
            Vec::new()
        };
        let seq = self.persist()?;
        self.patch_joined(&before, seq);
        self.admin_after(
            admin_before,
            vec![Cause::Disconnect {
                player: who.player_id.clone(),
            }],
            Vec::new(),
            seq,
        );
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        self.apply_rest_and_kicks(rest).await?;
        self.finish_pass().await;
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
            if key == outbox::OUTBOX_TIMER {
                self.flush_outbox().await;
                continue;
            }
            if let Some(player) = key.strip_prefix(GRACE_TIMER_PREFIX) {
                self.run_grace_expired(&PlayerId(player.to_string()))
                    .await?;
                if self.state.is_none() {
                    break;
                }
                continue;
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
        let admin_before = self.admin_before();
        let roster = self.clients_roster()?;
        let snapshot = self.snapshot()?;
        let state = self.state.take().expect("state loaded");
        let mut ctx = self.make_ctx(state, roster);
        let result = self.room.on_timer(&mut ctx, key.clone()).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        let effects = if result.is_ok() {
            effects
        } else {
            // A failed timer handler is not retried: the row is already gone.
            self.state = Some(rmp_serde::from_slice(&snapshot)?);
            self.log_json(serde_json::json!({
                "hearth": "handler_error",
                "room": self.room_label(),
                "msg": result.expect_err("timer handler failed").to_string(),
            }));
            Vec::new()
        };
        let seq = self.persist()?;
        self.patch_joined(&before, seq);
        self.admin_after(
            admin_before,
            vec![Cause::Timer { key: key.0.clone() }],
            Vec::new(),
            seq,
        );
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        self.apply_rest_and_kicks(rest).await?;
        self.finish_pass().await;
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
                    admin: false,
                },
            );
            return Ok(());
        }
        let who = Client {
            player_id: attachment.player_id.clone(),
            seat,
            connected: false,
        };
        let before = self.views();
        let admin_before = self.admin_before();
        let roster = self.clients_roster()?;
        let snapshot = self.snapshot()?;
        let state = self.state.take().expect("state loaded");
        let mut ctx = self.make_ctx(state, roster);
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
                admin: false,
            },
        );
        let seq = self.persist()?;
        self.patch_joined(&before, seq);
        self.admin_after(
            admin_before,
            vec![Cause::Leave {
                player: who.player_id.clone(),
            }],
            Vec::new(),
            seq,
        );
        self.log_json(serde_json::json!({
            "hearth": "leave",
            "room": self.room_label(),
            "player": who.player_id.0,
            "reason": leave_reason(reason),
        }));
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        if !self.any_live_or_held()? {
            self.start_empty_timer()?;
        }
        self.apply_rest(rest).await?;
        self.finish_pass().await;
        Ok(())
    }

    /// Run `on_leave` for a held seat whose `hearth_held` row is already
    /// deleted. Used by the grace timer and by kicks. `leave_ms` is when the
    /// player actually left (the grace expiry), so the empty-room timer is
    /// anchored to it rather than to the moment this handler happened to run.
    async fn run_held_leave(
        &mut self,
        player: &PlayerId,
        seat: u8,
        reason: Leave,
        leave_ms: Option<u64>,
    ) -> Result<()> {
        if self.ensure_loaded().await? {
            return Ok(());
        }
        let who = Client {
            player_id: player.clone(),
            seat,
            connected: false,
        };
        let before = self.views();
        let admin_before = self.admin_before();
        let roster = self.clients_roster()?;
        let snapshot = self.snapshot()?;
        let state = self.state.take().expect("state loaded");
        let mut ctx = self.make_ctx(state, roster);
        let result = self.room.on_leave(&mut ctx, &who, reason).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        let effects = if result.is_ok() {
            effects
        } else {
            self.state = Some(rmp_serde::from_slice(&snapshot)?);
            Vec::new()
        };
        let seq = self.persist()?;
        self.patch_joined(&before, seq);
        self.admin_after(
            admin_before,
            vec![Cause::Leave {
                player: who.player_id.clone(),
            }],
            Vec::new(),
            seq,
        );
        self.log_json(serde_json::json!({
            "hearth": "leave",
            "room": self.room_label(),
            "player": who.player_id.0,
            "reason": leave_reason(reason),
        }));
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        if !self.any_live_or_held()? {
            let since = leave_ms.unwrap_or(self.host.now_ms());
            let due = since.saturating_add(R::EMPTY_GRACE.as_millis() as u64);
            self.timer_set(EMPTY_TIMER, due)?;
        }
        self.apply_rest(rest).await?;
        self.finish_pass().await;
        Ok(())
    }

    /// Fired by a held seat's grace timer: delete the row, then `on_leave(Closed)`.
    async fn run_grace_expired(&mut self, player: &PlayerId) -> Result<()> {
        let Some((seat, until_ms)) = self.expire_held(player)? else {
            return Ok(());
        };
        self.run_held_leave(player, seat, Leave::Closed, Some(until_ms))
            .await
    }

    fn start_empty_timer(&self) -> Result<()> {
        let due = self
            .host
            .now_ms()
            .saturating_add(R::EMPTY_GRACE.as_millis() as u64);
        self.timer_set(EMPTY_TIMER, due)
    }

    async fn run_dispose(&mut self) -> Result<()> {
        if self.incompatible {
            // Never save or wipe once persisted state is known incompatible.
            return Ok(());
        }
        let room = self.room_or_unknown();
        let label = self.room_label();
        let admin_before = self.admin_before();
        if let Some(state) = self.state.take() {
            let roster = self.clients_roster()?;
            let mut ctx = self.make_ctx(state, roster);
            if let Err(err) = self.room.on_dispose(&mut ctx).await {
                self.log_json(serde_json::json!({
                    "hearth": "handler_error",
                    "room": label,
                    "msg": err.to_string(),
                }));
            }
            let _ = ctx.into_parts();
        }
        let seq = self.seq;
        // The last write of a disposed room: deliver the Disposed report
        // before the wipe, because a disposed room cannot retry afterwards.
        self.queue_report(RoomReport::Disposed { room });
        self.flush_outbox().await;
        self.host.wipe().await?;
        self.admin_after(admin_before, vec![Cause::Dispose], Vec::new(), seq);
        for socket in self.host.sockets() {
            self.host.close(&socket, close::DISPOSED, "disposed");
        }
        self.reset_after_dispose();
        self.host.set_alarm(None).await?;
        self.log_json(serde_json::json!({
            "hearth": "dispose",
            "room": label,
        }));
        Ok(())
    }

    async fn apply_rest(&mut self, effects: Vec<Effect>) -> Result<()> {
        // Notify rows are written here, in the same synchronous block as the
        // state save that precedes every pass: a committed handler's notifies
        // are durable and a failed handler never reaches this point.
        for effect in &effects {
            if let Effect::Notify {
                binding,
                name,
                body,
            } = effect
            {
                outbox::enqueue(self.host.sql(), binding, name, body, self.host.now_ms())?;
            }
        }
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
                // Events are delivered by the caller, notify rows were written
                // above; nested kicks are not supported in v0.1.
                Effect::Event { .. } | Effect::Kick(_) | Effect::Notify { .. } => {}
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
            } else if let Some((seat, _)) = self.expire_held(&player)? {
                // Kicking a held player deletes the row and calls on_leave.
                self.run_held_leave(&player, seat, Leave::Kicked, None)
                    .await?;
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
        let had_meta = self.has_table("hearth_meta")?;
        self.host
            .sql()
            .exec(store::CREATE_STATE_TABLE, Vec::new())?;
        self.host
            .sql()
            .exec(store::CREATE_TIMERS_TABLE, Vec::new())?;
        self.host.sql().exec(store::CREATE_HELD_TABLE, Vec::new())?;
        self.host.sql().exec(store::CREATE_META_TABLE, Vec::new())?;
        outbox::create(self.host.sql())?;
        if self.identity.is_none() {
            if let Some((room, created_ms)) = self.read_meta()? {
                self.identity = Some(room);
                self.created_ms = created_ms;
                // A runtime that did not create the meta row is waking up over
                // an existing room, whose `Open` already reached the registry.
                if had_meta && !self.meta_created_here && !self.open_reported {
                    self.open_reported = true;
                    self.last_players = self.occupied().ok().map(|players| players as u8);
                }
            }
        }
        let rows = self.host.sql().exec(
            "SELECT seq, locked, data FROM hearth_state WHERE id = 0",
            Vec::new(),
        )?;
        if let Some(row) = rows.into_iter().next() {
            let seq = row.get("seq").and_then(Value::as_u64).unwrap_or(0);
            let locked = row.get("locked").and_then(Value::as_i64).unwrap_or(0) != 0;
            let data = store::json_to_bytes(row.get("data"))?;
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
                    self.log_json(serde_json::json!({
                        "hearth": "state_incompatible",
                        "room": self.room_label(),
                    }));
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

    fn clients_roster(&self) -> Result<Vec<Client>> {
        let mut roster: Vec<Client> = self
            .joined()
            .into_iter()
            .map(|(_, attachment)| Client {
                player_id: attachment.player_id,
                seat: attachment.seat.unwrap_or(0),
                connected: true,
            })
            .collect();
        roster.extend(
            self.held_rows()?
                .into_iter()
                .map(|(player_id, seat)| Client {
                    player_id,
                    seat,
                    connected: false,
                }),
        );
        Ok(roster)
    }

    /// Every live or held seat, in player order.
    fn held_rows(&self) -> Result<Vec<(PlayerId, u8)>> {
        let rows = self.host.sql().exec(
            "SELECT player_id, seat FROM hearth_held ORDER BY player_id",
            Vec::new(),
        )?;
        let mut held = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(player) = row.get("player_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(seat) = row.get("seat").and_then(Value::as_u64) else {
                continue;
            };
            held.push((PlayerId(player.to_owned()), seat as u8));
        }
        Ok(held)
    }

    fn held_seat(&self, player: &PlayerId) -> Result<Option<u8>> {
        let rows = self.host.sql().exec(
            "SELECT seat FROM hearth_held WHERE player_id = ?",
            vec![SqlValue::Text(player.0.clone())],
        )?;
        Ok(rows
            .first()
            .and_then(|row| row.get("seat"))
            .and_then(Value::as_u64)
            .map(|seat| seat as u8))
    }

    /// Delete a held seat's row and grace timer. Returns the seat and the
    /// scheduled expiry if it was held.
    fn expire_held(&self, player: &PlayerId) -> Result<Option<(u8, u64)>> {
        let Some(seat) = self.held_seat(player)? else {
            return Ok(None);
        };
        let until_ms = self
            .held_until(player)?
            .unwrap_or_else(|| self.host.now_ms());
        self.host.sql().exec(
            "DELETE FROM hearth_held WHERE player_id = ?",
            vec![SqlValue::Text(player.0.clone())],
        )?;
        self.timer_delete(&grace_timer_key(player))?;
        Ok(Some((seat, until_ms)))
    }

    fn held_until(&self, player: &PlayerId) -> Result<Option<u64>> {
        let rows = self.host.sql().exec(
            "SELECT until_ms FROM hearth_held WHERE player_id = ?",
            vec![SqlValue::Text(player.0.clone())],
        )?;
        Ok(rows
            .first()
            .and_then(|row| row.get("until_ms"))
            .and_then(Value::as_u64))
    }

    /// Live joined sockets plus held seats.
    fn occupied(&self) -> Result<usize> {
        Ok(self.joined().len() + self.held_rows()?.len())
    }

    fn any_live_or_held(&self) -> Result<bool> {
        Ok(!self.joined().is_empty() || !self.held_rows()?.is_empty())
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

    fn lowest_free_seat(&self) -> Result<u8> {
        let mut taken: Vec<u8> = self
            .joined()
            .iter()
            .filter_map(|(_, attachment)| attachment.seat)
            .collect();
        taken.extend(self.held_rows()?.into_iter().map(|(_, seat)| seat));
        Ok((0..R::MAX_CLIENTS)
            .find(|seat| !taken.contains(seat))
            .unwrap_or(0))
    }
}

/// `on_call` for a room that accepts object-to-object calls. The handler runs
/// as a normal pass, so its state change is patched to the room's clients.
impl<R: RoomRpc, H: Host + 'static> RoomRuntime<R, H> {
    pub async fn call(
        &mut self,
        envelope: Envelope,
        name: &str,
    ) -> std::result::Result<Value, CallError> {
        self.call_name = name.to_owned();
        let me = self.me();
        // A reentrant call would wait for a turn this object cannot take.
        if envelope.chain.contains(&me) {
            return Err(CallError::ReentrantCall(me));
        }
        if let Some(id) = envelope.msg_id.as_deref() {
            if outbox::seen(self.host.sql(), id).map_err(call_internal)? {
                // Already applied: answer success without running the handler.
                return Ok(outbox::duplicate_reply());
            }
        }
        let rpc = rpc_from_value::<R::Rpc>(envelope.rpc.clone())?;
        if self.ensure_loaded().await.map_err(call_internal)? {
            return Err(CallError::Handler(
                "persisted state was written by an incompatible build".to_owned(),
            ));
        }
        // The patch base is the loaded state, so an rpc that changes it reaches
        // the room's clients exactly like an intent does.
        let before = self.views();
        let admin_before = self.admin_before();
        self.chain = envelope.chain.clone();
        let roster = self.clients_roster().map_err(call_internal)?;
        let snapshot = self.snapshot().map_err(call_internal)?;
        let state = self.state.take().expect("state loaded");
        let mut ctx = self.make_ctx(state, roster);
        let result = self.room.on_call(&mut ctx, &envelope.from, rpc).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        self.chain.clear();
        match result {
            Ok(reply) => {
                let seq = self.persist().map_err(call_internal)?;
                // The applied-message marker shares the state commit, so a
                // retried delivery can never run the handler twice.
                if let Some(id) = envelope.msg_id.as_deref() {
                    outbox::remember(self.host.sql(), id, self.host.now_ms())
                        .map_err(call_internal)?;
                }
                self.patch_joined(&before, seq);
                self.admin_after(
                    admin_before,
                    vec![Cause::Call {
                        from: envelope.from.clone(),
                    }],
                    Vec::new(),
                    seq,
                );
                let (events, rest) = split_events(effects);
                self.deliver_events(&events);
                self.apply_rest_and_kicks(rest)
                    .await
                    .map_err(call_internal)?;
                self.finish_pass().await;
                rpc_to_value(reply)
            }
            Err(err) => {
                self.state = Some(rmp_serde::from_slice(&snapshot).map_err(call_internal)?);
                self.log_json(serde_json::json!({
                    "hearth": "handler_error",
                    "room": self.room_label(),
                    "msg": format!("{err:#}"),
                }));
                Err(CallError::Handler(format!("{err:#}")))
            }
        }
    }
}

/// Drop the in-memory runtime as hibernation does; storage and sockets stay.
impl<R: Room, H: Host + Clone + 'static> RoomRuntime<R, H> {
    pub fn hibernate(&mut self) {
        let host = H::clone(&self.host);
        *self = Self::new(host, self.binding);
    }
}

fn call_internal(err: impl std::fmt::Display) -> CallError {
    CallError::Handler(err.to_string())
}

fn grace_timer_key(player: &PlayerId) -> String {
    format!("{GRACE_TIMER_PREFIX}{}", player.0)
}

fn leave_reason(reason: Leave) -> &'static str {
    match reason {
        Leave::Closed => "closed",
        Leave::Left => "left",
        Leave::Kicked => "kicked",
    }
}

fn split_events(effects: Vec<Effect>) -> (Vec<Effect>, Vec<Effect>) {
    effects
        .into_iter()
        .partition(|effect| matches!(effect, Effect::Event { .. }))
}

pub(crate) fn log_error(_err: anyhow::Error) {
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("hearth runtime error: {_err:#}");
}
