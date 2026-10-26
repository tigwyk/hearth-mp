use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use hearth_mp_protocol::{
    close, decode, encode, Encoding, ErrorCode, Frame, IntentMsg, PlayerId, Value, WireMsg,
    MAX_FRAME_BYTES, PING_TEXT, PONG_TEXT, PROTOCOL_VERSION,
};
use hearth_mp_sync::{diff, HearthState};

use crate::admin::{Cause, InspectClient, InspectDoc, TailMsg};
use crate::call::{rpc_from_value, rpc_to_value, CallError, Caller, Envelope, HostCaller, Target};
use crate::ctx::{Ctx, Effect};
use crate::handoff::{
    self, AllocJob, HandoffError, HandoffEvent, Redeemed, ServerReport, SessionId, SessionStatus,
    HANDOFF_ENDED_EVENT, HANDOFF_EVENT, JOB_IN_FLIGHT, MAX_ALLOC_ATTEMPTS, TICKET_TTL,
};
use crate::host::{Attachment, Host, RoomName, RoomReport, Session, SqlValue, ANON_PROVIDER};
use crate::outbox;
use crate::room::{Client, JoinOpts, Leave, Mode, Room, RoomRpc, TimerKey};
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
/// Binding of the optional matchmaker. A room whose gateway told it which
/// matchmaker pools it also reports there.
const MATCHMAKER_BINDING: &str = "HEARTH_MATCHMAKER";
/// One handoff session's timeout/retry deadline (`h:handoff:<sid>`).
const HANDOFF_TIMER_PREFIX: &str = "h:handoff:";
/// Persisted stop jobs: sessions whose dedicated server must be stopped. The
/// adapter takes them after every event and on every wake.
const CREATE_STOPS_TABLE: &str = "CREATE TABLE IF NOT EXISTS hearth_stops (\
    sid TEXT PRIMARY KEY, \
    endpoint TEXT NOT NULL, \
    created_ms INTEGER NOT NULL)";

/// A dedicated server that must be stopped, as the adapter's job runner sees it.
#[derive(Clone, Debug)]
pub struct StopJob {
    /// The handoff session.
    pub sid: SessionId,
    /// Its dedicated server's endpoint.
    pub endpoint: String,
}

/// One persisted handoff session, decoded row by row.
#[derive(Clone, Debug)]
struct SessionRow {
    sid: SessionId,
    status: SessionStatus,
    secret_hash: Vec<u8>,
    players: Vec<PlayerId>,
    claims: Value,
    region: Option<String>,
    endpoint: Option<String>,
    attempts: u32,
    created_ms: u64,
    start_timeout_ms: u64,
    heartbeat_timeout_ms: u64,
    max_duration_ms: u64,
}
/// How often a ticking room schedules a watchdog alarm. The alarm renews the
/// loop driver's `wait_until` keepalive (whose eviction window is 15 minutes)
/// and restarts the loop after an eviction. One alarm per five minutes is
/// negligible next to one per tick, and a missed recovery window is bounded.
const TICK_WATCHDOG_MS: u64 = 300_000;

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
    /// lobby snapshot.
    open_reported: bool,
    last_lobby: Option<Lobby>,
    /// `ctx.set_metadata`'s value, persisted in `hearth_meta`.
    metadata: Option<Value>,
    /// The matchmaker instance (`"type:region"`) that pools this room, if any.
    mm: Option<String>,
    /// The call chain of the pass in progress, empty for every non-call pass.
    chain: Vec<Target>,
    /// The name a call's header gave this room, used before identity is known.
    call_name: String,
    /// Ticked-mode tick counter, persisted in `hearth_meta.tick`.
    tick: u64,
    /// The tick number last written to `hearth_meta`.
    last_saved_tick: u64,
    /// Absolute time of the next tick; `None` when the room is not ticking.
    next_tick_ms: Option<u64>,
    /// Absolute time of the watchdog alarm that renews a loop driver's
    /// keepalive and restarts ticking after an eviction. Only set while ticking.
    tick_watchdog_ms: Option<u64>,
    /// Intents received between ticks, applied at the start of the next tick in
    /// arrival order. Lost on eviction; the client then sees them unconfirmed.
    pending: Vec<(PlayerId, u32, R::Intent)>,
    /// Plaintext handoff session secrets that have not reached a dedicated
    /// server yet. A fresh isolate regenerates a missing one (and updates its
    /// hash) because the server has never seen it.
    secrets: HashMap<SessionId, String>,
    /// Allocation jobs currently running in the adapter, with the moment the
    /// in-flight window ends.
    in_flight: HashMap<SessionId, u64>,
}

/// The fields a lobby report carries: seated players, capacity, lock and the
/// room's own metadata.
type Lobby = (Vec<PlayerId>, u8, bool, Value);

struct TokenBucket {
    tokens: f64,
    last_ms: u64,
}

/// One held seat's `hearth_held` row, including the verified identity written
/// when the socket dropped.
struct HeldSeat {
    seat: u8,
    until_ms: u64,
    provider: String,
    display_name: Option<String>,
}

/// The views captured before one handler pass: the public view computed once,
/// plus each client's owner-scoped view when the state has owner fields. The
/// public view is what makes one diff serve every client.
struct CapturedViews<Socket> {
    public: Option<Value>,
    /// The joined sockets at capture time: a socket that joins during the pass
    /// already got `FULL_STATE` and must not also receive a patch.
    sockets: Vec<Socket>,
    /// Empty when `R::State::HAS_OWNER` is false, so a room without owner fields
    /// does no per-client view work at all.
    owner: Vec<(Socket, Value)>,
}

impl<R: Room, H: Host + 'static> RoomRuntime<R, H> {
    /// A runtime for one room over `host`; `binding` is its Durable Object binding name.
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
            last_lobby: None,
            metadata: None,
            mm: None,
            chain: Vec::new(),
            call_name: String::new(),
            tick: 0,
            last_saved_tick: 0,
            next_tick_ms: None,
            tick_watchdog_ms: None,
            pending: Vec::new(),
            secrets: HashMap::new(),
            in_flight: HashMap::new(),
        }
    }

    /// The host.
    pub fn host(&self) -> &H {
        self.host.as_ref()
    }

    /// The loaded state, if any.
    pub fn state(&self) -> Option<&R::State> {
        self.state.as_ref()
    }

    /// The current state sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The current ticked-mode tick number, `0` for an event room or before the
    /// room's first tick.
    pub fn ticks(&self) -> u64 {
        self.tick
    }

    /// The next moment this room needs to wake: its next timer (the outbox retry
    /// is one of those rows) or its next tick, whichever comes first.
    pub fn next_wake(&self) -> Option<u64> {
        let timer = store::next_timer(self.host.sql()).ok().flatten();
        // A loop host wakes for the tick in the object itself, so its alarm is
        // only ever the watchdog (plus timers). The alarm driver (tests, other
        // hosts) must still be armed for the next tick.
        let tick = if self.host.loops_ticks() {
            None
        } else {
            self.next_tick_ms
        };
        let watchdog = self.tick_watchdog_ms;
        [timer, tick, watchdog].into_iter().flatten().min()
    }

    /// The deadline the loop driver should wait for, or `None` when the room is
    /// not ticking. Event mode always answers `None`.
    pub fn tick_deadline(&self) -> Option<u64> {
        if Self::ticked_mode() {
            self.next_tick_ms
        } else {
            None
        }
    }

    /// Whether this room type is a fixed-step simulation. `TICK_HZ = 0` keeps a
    /// `mode = "ticked"` room dormant, which the macro never emits.
    fn ticked_mode() -> bool {
        R::MODE == Mode::Ticked && R::TICK_HZ > 0
    }

    /// The scheduling period, `1 s / TICK_HZ` rounded down to whole ms.
    fn tick_period_ms() -> u64 {
        (1000 / u64::from(R::TICK_HZ)).max(1)
    }

    /// The fixed timestep handed to [`Room::on_tick`]: exactly `1 s / TICK_HZ`.
    fn tick_dt() -> Duration {
        Duration::from_micros(1_000_000 / u64::from(R::TICK_HZ))
    }

    fn tick_due(&self) -> bool {
        Self::ticked_mode()
            && self
                .next_tick_ms
                .is_some_and(|next| self.host.now_ms() >= next)
    }

    /// Run the ticked steps that are due. A loop driver (the DO adapter's
    /// held-open driver) calls this; `alarm()` calls the same path.
    pub async fn tick_if_due(&mut self) {
        if let Err(err) = self.handle_tick_if_due().await {
            log_error(err);
        }
    }

    async fn handle_tick_if_due(&mut self) -> Result<()> {
        if self.ensure_loaded().await? {
            self.host.set_alarm(None).await?;
            return Ok(());
        }
        if self.tick_due() {
            self.tick().await?;
        }
        self.reschedule_alarm().await
    }

    /// The joined socket for `player`, if they are live right now.
    fn socket_of_player(&self, player: &PlayerId) -> Option<(H::Socket, Attachment)> {
        self.joined()
            .into_iter()
            .find(|(_, attachment)| &attachment.player_id == player)
    }

    fn connected_client(&self, player: &PlayerId) -> Option<Client> {
        self.socket_of_player(player).map(|(_, attachment)| Client {
            player_id: attachment.player_id,
            seat: attachment.seat.unwrap_or(0),
            connected: true,
            provider: attachment.provider,
            display_name: attachment.display_name,
        })
    }

    // ---- Handoff sessions -------------------------------------------------

    fn create_handoff_tables(&self) -> Result<()> {
        self.host
            .sql()
            .exec(handoff::CREATE_SESSIONS_TABLE, Vec::new())?;
        self.host
            .sql()
            .exec(handoff::CREATE_TICKETS_TABLE, Vec::new())?;
        self.host.sql().exec(CREATE_STOPS_TABLE, Vec::new())?;
        Ok(())
    }

    /// Delete ended/failed sessions (and their tickets) older than the
    /// retention window. Runs on load; the test suite never advances 24 h.
    fn prune_sessions(&self) -> Result<()> {
        let now = self.host.now_ms();
        let cutoff = now.saturating_sub(handoff::SESSION_RETENTION.as_millis() as u64);
        self.host.sql().exec(
            "DELETE FROM hearth_tickets WHERE sid IN \
             (SELECT sid FROM hearth_sessions WHERE status IN ('ended','failed') AND ended_ms < ?)",
            vec![SqlValue::Int(cutoff as i64)],
        )?;
        self.host.sql().exec(
            "DELETE FROM hearth_sessions WHERE status IN ('ended','failed') AND ended_ms < ?",
            vec![SqlValue::Int(cutoff as i64)],
        )?;
        Ok(())
    }

    fn load_session(&self, sid: &SessionId) -> Result<Option<SessionRow>> {
        let rows = self.host.sql().exec(
            "SELECT sid, status, secret_hash, players, claims, region, endpoint, attempts, \
             created_ms, ended_ms, start_timeout_ms, heartbeat_timeout_ms, max_duration_ms \
             FROM hearth_sessions WHERE sid = ?",
            vec![SqlValue::Text(sid.0.clone())],
        )?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let Some(status) = row
            .get("status")
            .and_then(Value::as_str)
            .and_then(SessionStatus::parse)
        else {
            anyhow::bail!("hearth_sessions.status is not a Hearth status");
        };
        // `players` and `claims` are JSON text columns (the plan's table shape),
        // so decode the string; tolerate an already-decoded array too.
        let players = match row.get("players") {
            Some(Value::String(raw)) => {
                serde_json::from_str::<Vec<PlayerId>>(raw).unwrap_or_default()
            }
            Some(value) => {
                serde_json::from_value::<Vec<PlayerId>>(value.clone()).unwrap_or_default()
            }
            None => Vec::new(),
        };
        let claims = match row.get("claims") {
            Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or(Value::Null),
            Some(value) => value.clone(),
            None => Value::Null,
        };
        let region = row.get("region").and_then(Value::as_str).map(str::to_owned);
        let endpoint = row
            .get("endpoint")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(Some(SessionRow {
            sid: SessionId(
                row.get("sid")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_default(),
            ),
            status,
            secret_hash: store::json_to_bytes(row.get("secret_hash"))?,
            players,
            claims,
            region,
            endpoint,
            attempts: row.get("attempts").and_then(Value::as_u64).unwrap_or(0) as u32,
            created_ms: row.get("created_ms").and_then(Value::as_u64).unwrap_or(0),
            start_timeout_ms: row
                .get("start_timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(handoff::DEFAULT_START_TIMEOUT.as_millis() as u64),
            heartbeat_timeout_ms: row
                .get("heartbeat_timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(handoff::DEFAULT_HEARTBEAT_TIMEOUT.as_millis() as u64),
            max_duration_ms: row
                .get("max_duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or(handoff::DEFAULT_MAX_DURATION.as_millis() as u64),
        }))
    }

    fn handoff_timer_key(sid: &SessionId) -> String {
        format!("{HANDOFF_TIMER_PREFIX}{}", sid.0)
    }

    fn handoff_timer_due(&self, sid: &SessionId) -> Result<Option<u64>> {
        let rows = self.host.sql().exec(
            "SELECT due_ms FROM hearth_timers WHERE key = ?",
            vec![SqlValue::Text(Self::handoff_timer_key(sid))],
        )?;
        Ok(rows
            .first()
            .and_then(|row| row.get("due_ms"))
            .and_then(Value::as_u64))
    }

    fn set_handoff_timer(&self, sid: &SessionId, due_ms: u64) -> Result<()> {
        self.timer_set(&Self::handoff_timer_key(sid), due_ms)
    }

    fn clear_handoff_timer(&self, sid: &SessionId) -> Result<()> {
        self.timer_delete(&Self::handoff_timer_key(sid))
    }

    /// The absolute deadline for the next timeout: the requested gap, capped by
    /// the session's hard maximum duration.
    fn session_deadline(row: &SessionRow, now_ms: u64, gap_ms: u64) -> u64 {
        let hard = row.created_ms.saturating_add(row.max_duration_ms);
        now_ms.saturating_add(gap_ms).min(hard)
    }

    /// Whether any session still keeps the room alive (allocating/ready/started).
    fn has_active_sessions(&self) -> Result<bool> {
        let rows = self.host.sql().exec(
            "SELECT 1 AS active FROM hearth_sessions \
             WHERE status IN ('allocating','ready','started') LIMIT 1",
            Vec::new(),
        )?;
        Ok(!rows.is_empty())
    }

    /// How many handoff sessions are live.
    pub fn active_sessions(&self) -> Result<u64> {
        let rows = self.host.sql().exec(
            "SELECT COUNT(*) AS n FROM hearth_sessions \
             WHERE status IN ('allocating','ready','started')",
            Vec::new(),
        )?;
        Ok(rows
            .first()
            .and_then(|row| row.get("n"))
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }

    /// A player's verified identity, from their live socket or their held seat,
    /// defaulting to the anonymous provider (the `Redeemed` shape needs one).
    fn identity_for(&self, player: &PlayerId) -> Result<(String, Option<String>)> {
        if let Some((_, attachment)) = self.socket_of_player(player) {
            return Ok((attachment.provider, attachment.display_name));
        }
        let rows = self.host.sql().exec(
            "SELECT provider, display_name FROM hearth_held WHERE player_id = ?",
            vec![SqlValue::Text(player.0.clone())],
        )?;
        if let Some(row) = rows.first() {
            return Ok((
                row.get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or(ANON_PROVIDER)
                    .to_owned(),
                row.get("display_name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ));
        }
        Ok((ANON_PROVIDER.to_owned(), None))
    }

    /// Send one player their private `hearth.handoff` event with a fresh ticket.
    fn mint_and_send_ticket(&mut self, row: &SessionRow, player: &PlayerId) -> Result<()> {
        let Some(endpoint) = row.endpoint.clone() else {
            return Ok(());
        };
        let (ticket, hash) = handoff::new_ticket();
        let now = self.host.now_ms();
        let expires_ms = now.saturating_add(TICKET_TTL.as_millis() as u64);
        self.host.sql().exec(
            "INSERT INTO hearth_tickets (hash, sid, player_id, exp_ms, used) \
             VALUES (?, ?, ?, ?, 0)",
            vec![
                SqlValue::Blob(hash),
                SqlValue::Text(row.sid.0.clone()),
                SqlValue::Text(player.0.clone()),
                SqlValue::Int(expires_ms as i64),
            ],
        )?;
        let data = serde_json::json!({
            "sid": row.sid.0,
            "endpoint": endpoint,
            "ticket": ticket,
            "expires_ms": expires_ms,
        });
        self.send_to_player(
            player,
            Frame::Event {
                name: HANDOFF_EVENT.to_owned(),
                data,
            },
        );
        Ok(())
    }

    fn notify_players_ended(&self, row: &SessionRow, reason: &str) {
        let data = serde_json::json!({"sid": row.sid.0, "reason": reason});
        for player in &row.players {
            self.send_to_player(
                player,
                Frame::Event {
                    name: HANDOFF_ENDED_EVENT.to_owned(),
                    data: data.clone(),
                },
            );
        }
    }

    fn enqueue_stop(&self, sid: &SessionId, endpoint: &str) -> Result<()> {
        self.host.sql().exec(
            "INSERT OR REPLACE INTO hearth_stops (sid, endpoint, created_ms) VALUES (?, ?, ?)",
            vec![
                SqlValue::Text(sid.0.clone()),
                SqlValue::Text(endpoint.to_owned()),
                SqlValue::Int(self.host.now_ms() as i64),
            ],
        )?;
        Ok(())
    }

    /// Take the sessions whose dedicated server must be stopped. The adapter
    /// runs each `stop` outside the runtime lock.
    pub fn take_stop_jobs(&mut self) -> Vec<StopJob> {
        if self.ensure_loaded_now().is_err() {
            return Vec::new();
        }
        let rows = match self.host.sql().exec(
            "SELECT sid, endpoint FROM hearth_stops ORDER BY sid",
            Vec::new(),
        ) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        let mut jobs = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(sid) = row.get("sid").and_then(Value::as_str) else {
                continue;
            };
            let Some(endpoint) = row.get("endpoint").and_then(Value::as_str) else {
                continue;
            };
            jobs.push(StopJob {
                sid: SessionId(sid.to_owned()),
                endpoint: endpoint.to_owned(),
            });
        }
        let _ = self.host.sql().exec("DELETE FROM hearth_stops", Vec::new());
        jobs
    }

    /// Sessions that need an allocation attempt now. A job stays in flight for
    /// [`JOB_IN_FLIGHT`]; after that it is re-issued with `attempt + 1`. A
    /// session waiting for its retry backoff is skipped until the gate passes.
    pub fn take_jobs(&mut self) -> Vec<AllocJob> {
        if self.ensure_loaded_now().is_err() || self.incompatible {
            return Vec::new();
        }
        let now = self.host.now_ms();
        let sessions = match self.sessions_with_status(SessionStatus::Allocating) {
            Ok(sessions) => sessions,
            Err(err) => {
                log_error(err);
                return Vec::new();
            }
        };
        let mut jobs = Vec::new();
        for row in sessions {
            if let Some(deadline) = self.in_flight.get(&row.sid).copied() {
                if now < deadline {
                    continue;
                }
                self.in_flight.remove(&row.sid);
            }
            match self.handoff_timer_due(&row.sid) {
                Ok(Some(due)) if due > now => continue,
                Ok(Some(_)) => {
                    let _ = self.clear_handoff_timer(&row.sid);
                }
                Ok(None) => {}
                Err(err) => {
                    log_error(err);
                    continue;
                }
            }
            let attempt = row.attempts.saturating_add(1);
            if let Err(err) = self.host.sql().exec(
                "UPDATE hearth_sessions SET attempts = ? WHERE sid = ?",
                vec![
                    SqlValue::Int(i64::from(attempt)),
                    SqlValue::Text(row.sid.0.clone()),
                ],
            ) {
                log_error(err);
                continue;
            }
            let secret = match self.secrets.get(&row.sid).cloned() {
                Some(secret) => secret,
                None => {
                    // The isolate restarted before the allocator saw the
                    // secret: regenerate it and store the new hash.
                    let (secret, hash) = handoff::new_secret();
                    if let Err(err) = self.host.sql().exec(
                        "UPDATE hearth_sessions SET secret_hash = ? WHERE sid = ?",
                        vec![SqlValue::Blob(hash), SqlValue::Text(row.sid.0.clone())],
                    ) {
                        log_error(err);
                        continue;
                    }
                    self.secrets.insert(row.sid.clone(), secret.clone());
                    secret
                }
            };
            self.in_flight.insert(
                row.sid.clone(),
                now.saturating_add(JOB_IN_FLIGHT.as_millis() as u64),
            );
            jobs.push(AllocJob {
                sid: row.sid,
                secret,
                room: self.room_or_unknown(),
                players: row.players,
                claims: row.claims,
                region: row.region,
                max_duration_ms: row.max_duration_ms,
                attempt,
            });
        }
        jobs
    }

    fn sessions_with_status(&self, status: SessionStatus) -> Result<Vec<SessionRow>> {
        let rows = self.host.sql().exec(
            "SELECT sid FROM hearth_sessions WHERE status = ? ORDER BY created_ms, sid",
            vec![SqlValue::Text(status.as_str().to_owned())],
        )?;
        let mut sessions = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(sid) = row.get("sid").and_then(Value::as_str) else {
                continue;
            };
            if let Some(session) = self.load_session(&SessionId(sid.to_owned()))? {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    /// The adapter reports the allocation result here. `Ok` completes the
    /// session (`ready`, tickets, `on_handoff(Ready)`); `Err` schedules a retry
    /// or fails the session after [`MAX_ALLOC_ATTEMPTS`].
    pub async fn job_done(&mut self, sid: &SessionId, result: std::result::Result<String, String>) {
        if let Err(err) = self.handle_job_done(sid, result).await {
            log_error(err);
        }
    }

    async fn handle_job_done(
        &mut self,
        sid: &SessionId,
        result: std::result::Result<String, String>,
    ) -> Result<()> {
        if self.ensure_loaded().await? {
            return Ok(());
        }
        self.in_flight.remove(sid);
        let Some(row) = self.load_session(sid)? else {
            return Ok(());
        };
        match (row.status, result) {
            (SessionStatus::Allocating, Ok(endpoint)) => {
                let now = self.host.now_ms();
                let deadline = Self::session_deadline(&row, now, row.start_timeout_ms);
                self.host.sql().exec(
                    "UPDATE hearth_sessions SET status = 'ready', endpoint = ?, \
                     deadline_ms = ? WHERE sid = ?",
                    vec![
                        SqlValue::Text(endpoint.clone()),
                        SqlValue::Int(deadline as i64),
                        SqlValue::Text(sid.0.clone()),
                    ],
                )?;
                self.secrets.remove(sid);
                self.set_handoff_timer(sid, deadline)?;
                let mut ready = row.clone();
                ready.status = SessionStatus::Ready;
                ready.endpoint = Some(endpoint.clone());
                for player in ready.players.clone() {
                    self.mint_and_send_ticket(&ready, &player)?;
                }
                self.run_handoff_event(sid, HandoffEvent::Ready { endpoint })
                    .await
            }
            // A container reports `Started` as soon as its process boots, and
            // that outbound report can beat the allocator's RPC reply. The
            // session is genuinely up, but its endpoint and the players'
            // tickets are still owed, so this success is not stale: record the
            // endpoint and deliver the tickets now (the session keeps the
            // `started` status and its heartbeat deadline).
            (SessionStatus::Started, Ok(endpoint)) => {
                self.host.sql().exec(
                    "UPDATE hearth_sessions SET endpoint = ? WHERE sid = ?",
                    vec![
                        SqlValue::Text(endpoint.clone()),
                        SqlValue::Text(sid.0.clone()),
                    ],
                )?;
                self.secrets.remove(sid);
                let mut ready = row.clone();
                ready.status = SessionStatus::Ready;
                ready.endpoint = Some(endpoint.clone());
                for player in ready.players.clone() {
                    self.mint_and_send_ticket(&ready, &player)?;
                }
                self.run_handoff_event(sid, HandoffEvent::Ready { endpoint })
                    .await
            }
            (SessionStatus::Allocating, Err(error)) => {
                if row.attempts < MAX_ALLOC_ATTEMPTS {
                    // Retry after 2^attempts seconds.
                    let backoff_ms = 1000u64 << row.attempts.min(20);
                    let due = self.host.now_ms().saturating_add(backoff_ms);
                    self.set_handoff_timer(sid, due)?;
                    self.log_json(serde_json::json!({
                        "hearth": "handoff_retry",
                        "room": self.room_label(),
                        "session": sid.0,
                        "attempt": row.attempts,
                        "msg": error,
                    }));
                    Ok(())
                } else {
                    self.end_session(sid, SessionStatus::Failed, &error, true)
                        .await
                }
            }
            // A result for a finished session, or a failure that arrived after
            // the server had already reported `Started`, is stale.
            _ => Ok(()),
        }
    }

    /// Mark a session ended or failed: no more reports/redeems, players get
    /// `hearth.handoff_ended`, `on_handoff` runs, and a stop job is queued.
    async fn end_session(
        &mut self,
        sid: &SessionId,
        status: SessionStatus,
        reason: &str,
        notify_players: bool,
    ) -> Result<()> {
        let Some(row) = self.load_session(sid)? else {
            return Ok(());
        };
        if !row.status.is_active() {
            return Ok(());
        }
        let now = self.host.now_ms();
        self.host.sql().exec(
            "UPDATE hearth_sessions SET status = ?, ended_ms = ?, deadline_ms = 0 WHERE sid = ?",
            vec![
                SqlValue::Text(status.as_str().to_owned()),
                SqlValue::Int(now as i64),
                SqlValue::Text(sid.0.clone()),
            ],
        )?;
        self.clear_handoff_timer(sid)?;
        self.secrets.remove(sid);
        self.in_flight.remove(sid);
        if notify_players {
            self.notify_players_ended(&row, reason);
        }
        if let Some(endpoint) = &row.endpoint {
            self.enqueue_stop(sid, endpoint)?;
        }
        let event = match status {
            SessionStatus::Ended => HandoffEvent::Ended {
                reason: reason.to_owned(),
            },
            _ => HandoffEvent::Failed {
                reason: reason.to_owned(),
            },
        };
        self.run_handoff_event(sid, event).await?;
        self.sync_empty_timer().await
    }

    /// Schedule the empty-room dispose when nothing (client, held seat or active
    /// session) keeps the room alive any more.
    async fn sync_empty_timer(&mut self) -> Result<()> {
        if !self.any_live_or_held()? {
            self.start_empty_timer()?;
        }
        self.reschedule_alarm().await
    }

    async fn run_handoff_event(&mut self, sid: &SessionId, event: HandoffEvent) -> Result<()> {
        if self.state.is_none() {
            return Ok(());
        }
        let before = self.capture_views();
        let admin_before = self.admin_before();
        let roster = self.clients_roster()?;
        let snapshot = self.snapshot()?;
        let state = self.state.take().expect("state loaded");
        let mut ctx = self.make_ctx(state, roster);
        let result = self.room.on_handoff(&mut ctx, sid, event).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        let effects = if result.is_ok() {
            effects
        } else {
            self.state = Some(rmp_serde::from_slice(&snapshot)?);
            self.log_json(serde_json::json!({
                "hearth": "handler_error",
                "room": self.room_label(),
                "session": sid.0,
                "msg": result.expect_err("handoff handler failed").to_string(),
            }));
            Vec::new()
        };
        let seq = self.persist()?;
        self.patch_joined(&before, seq, None, None);
        self.admin_after(
            admin_before,
            vec![Cause::Timer {
                key: format!("handoff:{}", sid.0),
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

    /// A handoff timeout fired: the retry gate passed, the server missed its
    /// start window, its heartbeat went quiet, or the session hit max duration.
    async fn handoff_timeout(&mut self, sid: &SessionId) -> Result<()> {
        let Some(row) = self.load_session(sid)? else {
            return Ok(());
        };
        let now = self.host.now_ms();
        match row.status {
            SessionStatus::Allocating => Ok(()),
            SessionStatus::Ready => {
                self.end_session(sid, SessionStatus::Failed, "start timeout", true)
                    .await
            }
            SessionStatus::Started => {
                let reason = if now >= row.created_ms.saturating_add(row.max_duration_ms) {
                    "max duration"
                } else {
                    "lost"
                };
                self.end_session(sid, SessionStatus::Failed, reason, true)
                    .await
            }
            SessionStatus::Ended | SessionStatus::Failed => Ok(()),
        }
    }

    /// Redeem a ticket with the session secret. `Forbidden` deliberately hides
    /// which check failed.
    pub async fn redeem(
        &mut self,
        sid: &SessionId,
        secret: &str,
        ticket: &str,
    ) -> std::result::Result<Redeemed, HandoffError> {
        if self.ensure_loaded().await.is_err() {
            return Err(HandoffError::NotFound);
        }
        let row = self.session_or_error(sid)?;
        if !row.status.is_active() {
            return Err(HandoffError::Gone);
        }
        if !handoff::constant_time_eq(&handoff::hash_secret(secret), &row.secret_hash) {
            return Err(HandoffError::Forbidden);
        }
        self.consume_ticket(sid, ticket)
    }

    /// The gateway's container-proxy variant: the ticket is the only credential.
    pub async fn redeem_trusted(
        &mut self,
        sid: &SessionId,
        ticket: &str,
    ) -> std::result::Result<Redeemed, HandoffError> {
        if self.ensure_loaded().await.is_err() {
            return Err(HandoffError::NotFound);
        }
        let row = self.session_or_error(sid)?;
        if !row.status.is_active() {
            return Err(HandoffError::Gone);
        }
        self.consume_ticket(sid, ticket)
    }

    fn session_or_error(&self, sid: &SessionId) -> std::result::Result<SessionRow, HandoffError> {
        match self.load_session(sid) {
            Ok(Some(row)) => Ok(row),
            Ok(None) => Err(HandoffError::NotFound),
            Err(_) => Err(HandoffError::NotFound),
        }
    }

    fn consume_ticket(
        &mut self,
        sid: &SessionId,
        ticket: &str,
    ) -> std::result::Result<Redeemed, HandoffError> {
        let hash = handoff::hash_ticket(ticket);
        let rows = self
            .host
            .sql()
            .exec(
                "SELECT sid, player_id, exp_ms, used FROM hearth_tickets WHERE hash = ?",
                vec![SqlValue::Blob(hash)],
            )
            .map_err(|_| HandoffError::NotFound)?;
        let Some(row) = rows.into_iter().next() else {
            return Err(HandoffError::Forbidden);
        };
        let Some(ticket_sid) = row.get("sid").and_then(Value::as_str) else {
            return Err(HandoffError::Forbidden);
        };
        let Some(player) = row.get("player_id").and_then(Value::as_str) else {
            return Err(HandoffError::Forbidden);
        };
        let used = row.get("used").and_then(Value::as_i64).unwrap_or(1) != 0;
        let exp_ms = row.get("exp_ms").and_then(Value::as_u64).unwrap_or(0);
        if ticket_sid != sid.0 || used || exp_ms < self.host.now_ms() {
            return Err(HandoffError::Forbidden);
        }
        self.host
            .sql()
            .exec(
                "UPDATE hearth_tickets SET used = 1 WHERE hash = ?",
                vec![SqlValue::Blob(handoff::hash_ticket(ticket))],
            )
            .map_err(|_| HandoffError::NotFound)?;
        let player_id = PlayerId(player.to_owned());
        let (provider, display_name) = self
            .identity_for(&player_id)
            .map_err(|_| HandoffError::NotFound)?;
        let row = self.session_or_error(sid)?;
        Ok(Redeemed {
            player_id,
            provider,
            display_name,
            claims: row.claims,
            sid: sid.clone(),
        })
    }

    /// A server lifecycle report. `Heartbeat` only moves the deadline and runs
    /// no handler pass; every other report is one pass. Reports for a finished
    /// session are accepted and ignored.
    pub async fn report(
        &mut self,
        sid: &SessionId,
        secret: &str,
        event: ServerReport,
    ) -> std::result::Result<(), HandoffError> {
        if self.ensure_loaded().await.is_err() {
            return Err(HandoffError::NotFound);
        }
        let row = self.session_or_error(sid)?;
        if !handoff::constant_time_eq(&handoff::hash_secret(secret), &row.secret_hash) {
            return Err(HandoffError::Forbidden);
        }
        if !row.status.is_active() {
            return Ok(());
        }
        match event {
            ServerReport::Heartbeat => {
                self.touch_deadline(&row)
                    .map_err(|_| HandoffError::NotFound)?;
                Ok(())
            }
            ServerReport::Started => {
                // A server may retry `started` (for example after its own
                // restart). Treat a repeat as a heartbeat: the room already saw
                // the transition and must not run a second handler pass for it.
                if row.status == SessionStatus::Started {
                    self.touch_deadline(&row)
                        .map_err(|_| HandoffError::NotFound)?;
                    return Ok(());
                }
                self.start_session(&row)
                    .map_err(|_| HandoffError::NotFound)?;
                self.run_handoff_event(sid, HandoffEvent::Started)
                    .await
                    .map_err(|_| HandoffError::NotFound)
            }
            ServerReport::PlayerJoined { player } => {
                self.touch_deadline(&row)
                    .map_err(|_| HandoffError::NotFound)?;
                self.run_handoff_event(sid, HandoffEvent::PlayerJoined(player))
                    .await
                    .map_err(|_| HandoffError::NotFound)
            }
            ServerReport::PlayerLeft { player } => {
                self.touch_deadline(&row)
                    .map_err(|_| HandoffError::NotFound)?;
                self.run_handoff_event(sid, HandoffEvent::PlayerLeft(player))
                    .await
                    .map_err(|_| HandoffError::NotFound)
            }
            ServerReport::Result { data } => {
                self.touch_deadline(&row)
                    .map_err(|_| HandoffError::NotFound)?;
                self.run_handoff_event(sid, HandoffEvent::Result(data))
                    .await
                    .map_err(|_| HandoffError::NotFound)
            }
            ServerReport::Ended { reason } => self
                .end_session(sid, SessionStatus::Ended, &reason, true)
                .await
                .map_err(|_| HandoffError::NotFound),
        }
    }

    fn touch_deadline(&self, row: &SessionRow) -> Result<()> {
        if row.status != SessionStatus::Started {
            return Ok(());
        }
        let now = self.host.now_ms();
        let deadline = Self::session_deadline(row, now, row.heartbeat_timeout_ms);
        self.host.sql().exec(
            "UPDATE hearth_sessions SET deadline_ms = ? WHERE sid = ?",
            vec![
                SqlValue::Int(deadline as i64),
                SqlValue::Text(row.sid.0.clone()),
            ],
        )?;
        self.set_handoff_timer(&row.sid, deadline)
    }

    fn start_session(&self, row: &SessionRow) -> Result<()> {
        let now = self.host.now_ms();
        let deadline = Self::session_deadline(row, now, row.heartbeat_timeout_ms);
        self.host.sql().exec(
            "UPDATE hearth_sessions SET status = 'started', deadline_ms = ? WHERE sid = ?",
            vec![
                SqlValue::Int(deadline as i64),
                SqlValue::Text(row.sid.0.clone()),
            ],
        )?;
        self.set_handoff_timer(&row.sid, deadline)
    }

    /// Send a fresh ticket for `sid` to a player who reconnected while the
    /// session is ready or started.
    pub fn reissue_ticket(&mut self, sid: &SessionId, player: &PlayerId) {
        if let Err(err) = self.reissue_ticket_inner(sid, player) {
            log_error(err);
        }
    }

    fn reissue_ticket_inner(&mut self, sid: &SessionId, player: &PlayerId) -> Result<()> {
        let Some(row) = self.load_session(sid)? else {
            return Ok(());
        };
        if !matches!(row.status, SessionStatus::Ready | SessionStatus::Started) {
            return Ok(());
        }
        if !row.players.contains(player) {
            return Ok(());
        }
        if self.socket_of_player(player).is_none() {
            return Ok(());
        }
        self.mint_and_send_ticket(&row, player)
    }

    /// A player just joined or resumed: reissue a ticket for every active
    /// session they belong to, so a reconnect during a handoff can still enter.
    fn reissue_tickets_for(&mut self, player: &PlayerId) {
        let sessions = ["ready", "started"];
        for status in sessions {
            let Ok(rows) = self.host.sql().exec(
                "SELECT sid FROM hearth_sessions WHERE status = ? AND players LIKE ?",
                vec![
                    SqlValue::Text(status.to_owned()),
                    SqlValue::Text(format!("%\"{}\"%", player.0)),
                ],
            ) else {
                continue;
            };
            for row in rows {
                if let Some(sid) = row.get("sid").and_then(Value::as_str) {
                    self.reissue_ticket(&SessionId(sid.to_owned()), player);
                }
            }
        }
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
        Ctx::new(
            state,
            roster,
            sql,
            now,
            self.tick,
            self.room_or_unknown(),
            self.caller(),
        )
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
    pub async fn connect(
        &mut self,
        socket: H::Socket,
        session: Session,
        room: RoomName,
        matchmaker: Option<String>,
    ) {
        if let Err(err) = self.remember_room(&room) {
            self.log_json(serde_json::json!({
                "hearth": "meta_error",
                "room": self.room_label(),
                "msg": err.to_string(),
            }));
        }
        if let Some(mm) = matchmaker {
            if let Err(err) = self.set_matchmaker(&mm) {
                self.log_json(serde_json::json!({
                    "hearth": "meta_error",
                    "room": self.room_label(),
                    "msg": err.to_string(),
                }));
            }
        }
        let Session {
            player,
            provider,
            display_name,
        } = session;
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
                            provider: provider.clone(),
                            display_name: display_name.clone(),
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
                provider,
                display_name,
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
                provider: ANON_PROVIDER.to_owned(),
                display_name: None,
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
        // Inspect is read-only, but a hot ticked room persists first so the
        // snapshot includes every applied tick (and writes no uninitialised
        // storage: the state is already loaded).
        if Self::ticked_mode() && self.state.is_some() {
            self.persist()?;
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
        let tick = store::meta_get(self.host.sql(), "tick")?
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(0);
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
            tick,
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

    /// The room's current lobby snapshot: seated players, capacity, lock and
    /// metadata. A report goes out only when this changes.
    fn lobby_snapshot(&self) -> Option<Lobby> {
        let players = self.occupied_players().ok()?;
        Some((
            players,
            R::MAX_CLIENTS,
            self.locked,
            self.metadata.clone().unwrap_or(Value::Null),
        ))
    }

    /// Live and held seats, sorted by player id so a drop (live -> held) does
    /// not reorder the roster, and the registry sees only real changes.
    fn occupied_players(&self) -> Result<Vec<PlayerId>> {
        let mut players: Vec<PlayerId> = self
            .joined()
            .into_iter()
            .map(|(_, attachment)| attachment.player_id)
            .collect();
        players.extend(
            self.held_clients()?
                .into_iter()
                .map(|client| client.player_id),
        );
        players.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(players)
    }

    fn report_players(&mut self) {
        if !self.open_reported {
            return;
        }
        let Some(lobby) = self.lobby_snapshot() else {
            return;
        };
        if self.last_lobby.as_ref() == Some(&lobby) {
            return;
        }
        self.last_lobby = Some(lobby.clone());
        let (players, capacity, locked, metadata) = lobby;
        self.queue_report(RoomReport::Players {
            room: self.room_or_unknown(),
            players,
            capacity,
            locked,
            metadata,
        });
    }

    fn report_open(&mut self) {
        if self.open_reported {
            return;
        }
        let Some(lobby) = self.lobby_snapshot() else {
            return;
        };
        self.open_reported = true;
        self.last_lobby = Some(lobby.clone());
        let (players, capacity, locked, metadata) = lobby;
        self.queue_report(RoomReport::Open {
            room: self.room_or_unknown(),
            created_ms: self.created_ms,
            players,
            capacity,
            locked,
            metadata,
        });
    }

    /// Queue one report to the registry and, when this room is pooled by a
    /// matchmaker, to that matchmaker too. Reports are durable outbox rows, so
    /// an outage delays them instead of losing them.
    fn queue_report(&self, report: RoomReport) {
        let Ok(body) = serde_json::to_value(&report) else {
            return;
        };
        self.enqueue_report(REGISTRY_BINDING, REGISTRY_NAME, &body);
        if let Some(mm) = &self.mm {
            // The matchmaker is a Service, so the report rides in its `Report`
            // variant; the registry service takes the bare report.
            self.enqueue_report(MATCHMAKER_BINDING, mm, &serde_json::json!({"Report": body}));
        }
    }

    fn enqueue_report(&self, binding: &str, name: &str, body: &Value) {
        if !self.host.has_binding(binding) {
            return;
        }
        if let Err(err) = outbox::enqueue(self.host.sql(), binding, name, body, self.host.now_ms())
        {
            self.log_json(serde_json::json!({
                "hearth": "report_error",
                "room": self.room_label(),
                "binding": binding,
                "msg": err.to_string(),
            }));
        }
    }
    /// The end of every handler pass: report the seated count, then deliver
    /// whatever committed notifies are pending.
    async fn finish_pass(&mut self) {
        self.report_players();
        self.flush_outbox().await;
        // Start ticking on the first connected join, stop it when the last
        // connected client goes (held seats and admin tails never tick).
        if let Err(err) = self.sync_ticking().await {
            log_error(err);
        }
    }

    /// Keep the tick schedule in sync with the connected clients.
    ///
    /// This runs at the end of every handler pass and on an alarm, never on a
    /// tick, so the watchdog deadline below does not slide while the loop driver
    /// ticks. The watchdog alarm is what renews the loop's keepalive and
    /// restarts ticking after an eviction.
    async fn sync_ticking(&mut self) -> Result<()> {
        if !Self::ticked_mode() {
            return Ok(());
        }
        let now = self.host.now_ms();
        if !self.joined().is_empty() {
            if self.next_tick_ms.is_none() {
                self.next_tick_ms = Some(now.saturating_add(Self::tick_period_ms()));
            }
            if self.tick_watchdog_ms.is_none_or(|at| at <= now) {
                self.tick_watchdog_ms = Some(now.saturating_add(TICK_WATCHDOG_MS));
            }
        } else if self.next_tick_ms.take().is_some() {
            self.tick_watchdog_ms = None;
            // Stopping: accepted intents must not be lost, and the tick number
            // is persisted with the state.
            self.drain_pending().await?;
            self.persist()?;
        }
        self.reschedule_alarm().await
    }

    /// Run the buffered intents when ticking stops. There is no ACK: the room
    /// is no longer ticking and the last connected client is already leaving.
    async fn drain_pending(&mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.pending);
        let mut effects = Vec::new();
        for (player, _intent_seq, intent) in pending {
            // The sender may already have left (a clean LEAVE clears the
            // attachment before this runs): the plan still runs its accepted
            // intents on stop, so hand the handler a disconnected client.
            let who = self.connected_client(&player).unwrap_or(Client {
                player_id: player,
                seat: 0,
                connected: false,
                provider: ANON_PROVIDER.to_owned(),
                display_name: None,
            });
            let snapshot = self.snapshot()?;
            let roster = self.clients_roster()?;
            let state = self.state.take().expect("state loaded");
            let mut ctx = self.make_ctx(state, roster);
            let result = self.room.on_intent(&mut ctx, &who, intent).await;
            let (state, ctx_effects) = ctx.into_parts();
            self.state = Some(state);
            match result {
                Ok(()) => effects.extend(ctx_effects),
                Err(err) => {
                    self.state = Some(rmp_serde::from_slice(&snapshot)?);
                    self.log_json(serde_json::json!({
                        "hearth": "handler_error",
                        "room": self.room_label(),
                        "player": who.player_id.0,
                        "msg": err.to_string(),
                    }));
                }
            }
        }
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        self.apply_rest(rest).await?;
        Ok(())
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
        self.last_lobby = None;
        self.metadata = None;
        self.mm = None;
        self.chain.clear();
        self.call_name.clear();
        self.tick = 0;
        self.last_saved_tick = 0;
        self.next_tick_ms = None;
        self.tick_watchdog_ms = None;
        self.pending.clear();
        self.secrets.clear();
        self.in_flight.clear();
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
                provider: attachment.provider,
                display_name: attachment.display_name,
            })
            .collect();
        if self.has_table("hearth_held")? {
            roster.extend(self.held_clients()?);
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

    /// Handle one frame from `socket`.
    pub async fn message(&mut self, socket: H::Socket, msg: WireMsg) {
        if let Err(err) = self.handle_message(socket, msg).await {
            log_error(err);
        }
    }

    /// `socket` closed or dropped: hold its seat for the grace period, or run `on_leave`.
    pub async fn close(&mut self, socket: H::Socket) {
        if let Err(err) = self.handle_close(socket).await {
            log_error(err);
        }
    }

    /// The host's alarm fired: timers, ticks, grace expiries, disposal.
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
            provider: attachment.provider.clone(),
            display_name: attachment.display_name.clone(),
        };
        let before = self.capture_views();
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
                        provider: attachment.provider.clone(),
                        display_name: attachment.display_name.clone(),
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
            provider: attachment.provider.clone(),
            display_name: attachment.display_name.clone(),
        };
        self.host.set_attachment(&socket, &joined_attachment);
        // A join during the empty-room grace period cancels the dispose.
        self.timer_delete(EMPTY_TIMER)?;

        let seq = self.persist()?;
        self.patch_joined(&before, seq, None, None);
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
        // A player joining during a ready/started handoff gets a fresh ticket
        // for the dedicated server, exactly like a reconnect.
        self.reissue_tickets_for(&who.player_id);
        self.apply_rest_and_kicks(rest).await?;
        self.finish_pass().await;
        Ok(())
    }

    /// Complete a resumed `HELLO`: delete the held row and its grace timer, send
    /// `JOIN_OK` + `FULL_STATE`, then run `on_reconnect` as a handler pass.
    ///
    /// The identity comes from the held row, not from the connect's session: a
    /// resume keeps the values the player originally joined with.
    async fn resume(
        &mut self,
        socket: H::Socket,
        attachment: Attachment,
        wire_encoding: Encoding,
        seat: u8,
    ) -> Result<()> {
        let identity = self.held_identity(&attachment.player_id)?;
        self.expire_held(&attachment.player_id)?;
        let (provider, display_name) = identity
            .unwrap_or_else(|| (attachment.provider.clone(), attachment.display_name.clone()));
        let who = Client {
            player_id: attachment.player_id.clone(),
            seat,
            connected: true,
            provider: provider.clone(),
            display_name: display_name.clone(),
        };
        let joined_attachment = Attachment {
            player_id: attachment.player_id.clone(),
            seat: Some(seat),
            encoding: Some(wire_encoding),
            admin: false,
            provider,
            display_name,
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

        let before = self.capture_views();
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
        self.patch_joined(&before, seq, None, None);
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
        self.reissue_tickets_for(&who.player_id);
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
        if Self::ticked_mode() {
            return self.buffer_intents(socket, attachment, intents).await;
        }
        let fallback = attachment.encoding.unwrap_or(Encoding::MsgPack);
        let who = Client {
            player_id: attachment.player_id.clone(),
            seat: attachment.seat.unwrap_or(0),
            connected: true,
            provider: attachment.provider.clone(),
            display_name: attachment.display_name.clone(),
        };
        let before = self.capture_views();
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
        self.patch_joined(&before, seq, None, None);
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
                    tick: None,
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
                    provider: attachment.provider.clone(),
                    display_name: attachment.display_name.clone(),
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
            "INSERT OR REPLACE INTO hearth_held \
             (player_id, seat, until_ms, provider, display_name) VALUES (?, ?, ?, ?, ?)",
            vec![
                SqlValue::Text(player.0.clone()),
                SqlValue::Int(i64::from(seat)),
                SqlValue::Int(due as i64),
                SqlValue::Text(attachment.provider.clone()),
                optional_text(&attachment.display_name),
            ],
        )?;
        self.timer_set(&grace_timer_key(&player), due)?;

        // The socket is gone: exclude it from the roster's live clients and from
        // every patch.
        self.host.set_attachment(
            &socket,
            &Attachment {
                provider: attachment.provider.clone(),
                display_name: attachment.display_name.clone(),
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
            provider: attachment.provider,
            display_name: attachment.display_name,
        };
        let before = self.capture_views();
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
        self.patch_joined(&before, seq, None, None);
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

    /// Ticked mode: buffer decoded intents until the next tick. Decode and rate
    /// limit errors are reported immediately, as in event mode; the ACK is sent
    /// by the tick that applies them, carrying its tick number.
    async fn buffer_intents(
        &mut self,
        socket: H::Socket,
        attachment: Attachment,
        intents: Vec<IntentMsg>,
    ) -> Result<()> {
        let fallback = attachment.encoding.unwrap_or(Encoding::MsgPack);
        let mut rate_limited = false;
        for intent_msg in intents {
            let now = self.host.now_ms();
            if !self.take_token(&attachment.player_id, now) {
                self.send_error(
                    &socket,
                    &attachment,
                    fallback,
                    ErrorCode::RateLimited,
                    "intent rate limit exceeded",
                    Some(intent_msg.seq),
                );
                rate_limited = true;
                break;
            }
            let Ok(intent) = serde_json::from_value::<R::Intent>(intent_msg.intent) else {
                self.send_error(
                    &socket,
                    &attachment,
                    fallback,
                    ErrorCode::BadIntent,
                    "intent did not decode",
                    Some(intent_msg.seq),
                );
                continue;
            };
            self.pending
                .push((attachment.player_id.clone(), intent_msg.seq, intent));
        }
        if rate_limited {
            self.run_leave(socket.clone(), attachment, Leave::Kicked)
                .await?;
            self.host
                .close(&socket, close::RATE_LIMITED, "rate limited");
        }
        self.finish_pass().await;
        Ok(())
    }

    /// Run the fixed steps that are due: at most three catch-up steps, then one
    /// PATCH per client and one ACK per intent sender. The schedule is absolute,
    /// so ticks skipped after a stall stay skipped.
    async fn tick(&mut self) -> Result<()> {
        let Some(next) = self.next_tick_ms else {
            return Ok(());
        };
        let period = Self::tick_period_ms();
        let now = self.host.now_ms();
        if now < next {
            return Ok(());
        }
        let due = (now - next) / period + 1;
        let steps = due.min(3);
        if due > steps {
            self.log_json(serde_json::json!({
                "hearth": "tick_dropped",
                "room": self.room_label(),
                "n": due - steps,
            }));
        }
        let before = self.capture_views();
        let admin_before = self.admin_before();
        let started = self.host.now_ms();
        let mut effects: Vec<Effect> = Vec::new();
        let mut applied: Vec<(PlayerId, Vec<u32>)> = Vec::new();
        let mut rejected: Vec<(PlayerId, u32, String)> = Vec::new();

        for _ in 0..steps {
            self.tick += 1;
            // 1. The intents that arrived since the last tick run first, in
            //    arrival order, so the handlers and on_tick agree on the input.
            let pending = std::mem::take(&mut self.pending);
            for (player, intent_seq, intent) in pending {
                let Some(who) = self.connected_client(&player) else {
                    // The socket went away before its input ran: drop it.
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
                        match applied.iter_mut().find(|(id, _)| id == &player) {
                            Some((_, seqs)) => seqs.push(intent_seq),
                            None => applied.push((player, vec![intent_seq])),
                        }
                        effects.extend(ctx_effects);
                    }
                    Err(err) => {
                        self.state = Some(rmp_serde::from_slice(&snapshot)?);
                        self.log_json(serde_json::json!({
                            "hearth": "handler_error",
                            "room": self.room_label(),
                            "player": who.player_id.0,
                            "msg": err.to_string(),
                        }));
                        rejected.push((player, intent_seq, err.to_string()));
                    }
                }
            }
            // 2. The tick itself. A failed step is rolled back and not retried.
            let snapshot = self.snapshot()?;
            let roster = self.clients_roster()?;
            let state = self.state.take().expect("state loaded");
            let mut ctx = self.make_ctx(state, roster);
            let result = self.room.on_tick(&mut ctx, Self::tick_dt()).await;
            let (state, step_effects) = ctx.into_parts();
            self.state = Some(state);
            match result {
                Ok(()) => effects.extend(step_effects),
                Err(err) => {
                    self.state = Some(rmp_serde::from_slice(&snapshot)?);
                    self.log_json(serde_json::json!({
                        "hearth": "handler_error",
                        "room": self.room_label(),
                        "msg": err.to_string(),
                    }));
                }
            }
            // 3. Persist every N ticks, with the tick number.
            if R::PERSIST_EVERY > 0 && self.tick % u64::from(R::PERSIST_EVERY) == 0 {
                self.persist()?;
            }
        }

        // Absolute schedule: consume every elapsed period, dropped ones too.
        self.next_tick_ms = Some(next.saturating_add(due.saturating_mul(period)));
        let seq = self.seq;
        self.patch_joined(&before, seq, Some(self.tick), Some(now));
        self.admin_after(
            admin_before,
            vec![Cause::Timer {
                key: "tick".to_owned(),
            }],
            Vec::new(),
            seq,
        );
        for (player, intent_seqs) in applied {
            if let Some((socket, attachment)) = self.socket_of_player(&player) {
                self.send_frame(
                    &socket,
                    &attachment,
                    Frame::Ack {
                        state_seq: seq,
                        intent_seqs,
                        tick: Some(self.tick),
                    },
                );
            }
        }
        for (player, intent_seq, message) in rejected {
            if let Some((socket, attachment)) = self.socket_of_player(&player) {
                let encoding = attachment.encoding.unwrap_or(Encoding::MsgPack);
                self.send_error(
                    &socket,
                    &attachment,
                    encoding,
                    ErrorCode::IntentRejected,
                    message,
                    Some(intent_seq),
                );
            }
        }
        let (events, rest) = split_events(effects);
        self.deliver_events(&events);
        self.apply_rest_and_kicks(rest).await?;
        let elapsed = self.host.now_ms().saturating_sub(started);
        if elapsed > period {
            self.log_json(serde_json::json!({
                "hearth": "tick_overrun",
                "room": self.room_label(),
                "ms": elapsed,
            }));
        }
        self.report_players();
        self.flush_outbox().await;
        self.reschedule_alarm().await?;
        Ok(())
    }

    async fn handle_alarm(&mut self) -> Result<()> {
        if self.ensure_loaded().await? {
            self.host.set_alarm(None).await?;
            return Ok(());
        }
        if self.tick_due() {
            self.tick().await?;
            if self.state.is_none() {
                self.host.set_alarm(None).await?;
                return Ok(());
            }
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
            if let Some(sid) = key.strip_prefix(HANDOFF_TIMER_PREFIX) {
                self.handoff_timeout(&SessionId(sid.to_string())).await?;
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
            if Self::ticked_mode() {
                // An alarm is also the recovery path for a ticked room whose
                // loop driver was lost to an eviction: it restarts the schedule
                // (and renews the watchdog) without a client event.
                self.sync_ticking().await?;
            } else {
                self.reschedule_alarm().await?;
            }
        } else {
            self.host.set_alarm(None).await?;
        }
        Ok(())
    }

    async fn run_timer(&mut self, key: TimerKey) -> Result<()> {
        if self.state.is_none() {
            return Ok(());
        }
        let before = self.capture_views();
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
        self.patch_joined(&before, seq, None, None);
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
                    provider: attachment.provider.clone(),
                    display_name: attachment.display_name.clone(),
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
            provider: attachment.provider.clone(),
            display_name: attachment.display_name.clone(),
        };
        let before = self.capture_views();
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
                provider: attachment.provider.clone(),
                display_name: attachment.display_name.clone(),
                player_id: attachment.player_id,
                seat: None,
                encoding: None,
                admin: false,
            },
        );
        let seq = self.persist()?;
        self.patch_joined(&before, seq, None, None);
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
        held: &HeldSeat,
        reason: Leave,
        leave_ms: Option<u64>,
    ) -> Result<()> {
        if self.ensure_loaded().await? {
            return Ok(());
        }
        let who = Client {
            player_id: player.clone(),
            seat: held.seat,
            connected: false,
            provider: held.provider.clone(),
            display_name: held.display_name.clone(),
        };
        let before = self.capture_views();
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
        self.patch_joined(&before, seq, None, None);
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
        let Some(held) = self.expire_held(player)? else {
            return Ok(());
        };
        let until_ms = held.until_ms;
        self.run_held_leave(player, &held, Leave::Closed, Some(until_ms))
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
                Effect::SetMetadata(metadata) => {
                    store::meta_put(
                        self.host.sql(),
                        "metadata",
                        &serde_json::to_string(&metadata)?,
                    )?;
                    self.metadata = Some(metadata);
                }
                Effect::Dispose => dispose = true,
                Effect::Handoff { sid, request } => {
                    let seated: Vec<PlayerId> = self
                        .clients_roster()?
                        .into_iter()
                        .map(|client| client.player_id)
                        .collect();
                    let known = !request.players.is_empty()
                        && request.players.iter().all(|player| seated.contains(player));
                    if !known {
                        self.log_json(serde_json::json!({
                            "hearth": "handoff_unknown_player",
                            "room": self.room_label(),
                            "session": sid.0,
                        }));
                    } else {
                        let now = self.host.now_ms();
                        let (secret, hash) = handoff::new_secret();
                        self.host.sql().exec(
                            "INSERT OR REPLACE INTO hearth_sessions (\
                             sid, status, secret_hash, players, claims, region, endpoint, \
                             attempts, created_ms, deadline_ms, ended_ms, \
                             start_timeout_ms, heartbeat_timeout_ms, max_duration_ms) \
                             VALUES (?, 'allocating', ?, ?, ?, ?, NULL, 0, ?, 0, NULL, ?, ?, ?)",
                            vec![
                                SqlValue::Text(sid.0.clone()),
                                SqlValue::Blob(hash),
                                SqlValue::Text(serde_json::to_string(&request.players)?),
                                SqlValue::Text(serde_json::to_string(&request.claims)?),
                                match &request.region {
                                    Some(region) => SqlValue::Text(region.clone()),
                                    None => SqlValue::Null,
                                },
                                SqlValue::Int(now as i64),
                                SqlValue::Int(request.start_timeout.as_millis() as i64),
                                SqlValue::Int(request.heartbeat_timeout.as_millis() as i64),
                                SqlValue::Int(request.max_duration.as_millis() as i64),
                            ],
                        )?;
                        self.secrets.insert(sid, secret);
                        // An active session keeps the room alive: no dispose.
                        self.timer_delete(EMPTY_TIMER)?;
                    }
                }
                Effect::ReservedEventName { name } => {
                    self.log_json(serde_json::json!({
                        "hearth": "reserved_event_name",
                        "room": self.room_label(),
                        "name": name,
                    }));
                }
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
            } else if let Some(held) = self.expire_held(&player)? {
                // Kicking a held player deletes the row and calls on_leave.
                self.run_held_leave(&player, &held, Leave::Kicked, None)
                    .await?;
            }
        }
        if dispose {
            self.run_dispose().await?;
        }
        Ok(())
    }

    /// Remember which matchmaker pools this room. The gateway sets it on the
    /// first connect; tests use it directly.
    pub fn set_matchmaker(&mut self, mm: &str) -> Result<()> {
        store::meta_put_if_absent(self.host.sql(), "mm", mm)?;
        self.mm = Some(mm.to_owned());
        Ok(())
    }

    /// Load the metadata and matchmaker values persisted in `hearth_meta`.
    fn load_lobby_meta(&mut self) {
        if self.metadata.is_none() {
            if let Ok(Some(text)) = store::meta_get(self.host.sql(), "metadata") {
                self.metadata = serde_json::from_str(&text).ok();
            }
        }
        if self.mm.is_none() {
            if let Ok(Some(mm)) = store::meta_get(self.host.sql(), "mm") {
                self.mm = Some(mm);
            }
        }
    }

    async fn ensure_loaded(&mut self) -> Result<bool> {
        self.ensure_loaded_now()
    }

    /// The synchronous body of [`Self::ensure_loaded`]: every storage call is
    /// local, so the sync jobs API (`take_jobs`, `take_stop_jobs`) can load too.
    fn ensure_loaded_now(&mut self) -> Result<bool> {
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
        store::ensure_held_columns(self.host.sql())?;
        self.host.sql().exec(store::CREATE_META_TABLE, Vec::new())?;
        outbox::create(self.host.sql())?;
        self.create_handoff_tables()?;
        self.prune_sessions()?;
        if self.identity.is_none() {
            if let Some((room, created_ms)) = self.read_meta()? {
                self.identity = Some(room);
                self.created_ms = created_ms;
                // A runtime that did not create the meta row is waking up over
                // an existing room, whose `Open` already reached the registry.
                if had_meta && !self.meta_created_here && !self.open_reported {
                    self.open_reported = true;
                    self.last_lobby = self.lobby_snapshot();
                }
            }
        }
        self.load_lobby_meta();
        if let Some(tick) =
            store::meta_get(self.host.sql(), "tick")?.and_then(|raw| raw.parse::<u64>().ok())
        {
            self.tick = tick;
            self.last_saved_tick = tick;
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
        // A ticked room can wake from an eviction with a persisted state and
        // tick number older than what its still-connected clients saw (the
        // unpersisted ticks were lost). Clients order ticked patches strictly by
        // tick, so without a fresh FULL_STATE they would drop every patch until
        // the counter passed their old high-water mark, and the diffs in between
        // would not match their state. Event rooms never need this.
        if Self::ticked_mode() && self.state.is_some() && !self.joined().is_empty() {
            let seq = self.seq;
            for (socket, attachment) in self.joined() {
                let view = self
                    .state
                    .as_ref()
                    .expect("state loaded")
                    .view(&attachment.player_id);
                self.send_frame(&socket, &attachment, Frame::FullState { seq, state: view });
            }
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
        // A ticked room keeps its tick number with its state, so an eviction
        // loses at most the unpersisted ticks.
        if self.last_saved_tick != self.tick {
            store::meta_put(self.host.sql(), "tick", &self.tick.to_string())?;
            self.last_saved_tick = self.tick;
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
        self.host.set_alarm(self.next_wake()).await
    }

    fn take_token(&mut self, player: &PlayerId, now_ms: u64) -> bool {
        // A room can tune both numbers with its `#[hearth_mp::room]` attributes.
        let burst = f64::from(R::INTENT_BURST);
        let refill_per_ms = f64::from(R::INTENT_RATE) / 1000.0;
        if let Some(index) = self.buckets.iter().position(|(id, _)| id == player) {
            let bucket = &mut self.buckets[index].1;
            let elapsed = now_ms.saturating_sub(bucket.last_ms);
            bucket.tokens = (bucket.tokens + elapsed as f64 * refill_per_ms).min(burst);
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
                    tokens: burst - 1.0,
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
                provider: attachment.provider,
                display_name: attachment.display_name,
            })
            .collect();
        roster.extend(self.held_clients()?);
        Ok(roster)
    }

    /// Every live or held seat, in player order.
    fn held_clients(&self) -> Result<Vec<Client>> {
        let rows = self.host.sql().exec(
            "SELECT player_id, seat, provider, display_name FROM hearth_held ORDER BY player_id",
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
            held.push(Client {
                player_id: PlayerId(player.to_owned()),
                seat: seat as u8,
                connected: false,
                // Rows written before v0.3b have NULL identity columns.
                provider: row
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or(ANON_PROVIDER)
                    .to_owned(),
                display_name: row
                    .get("display_name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
        Ok(held)
    }

    /// A held seat's stored identity, if it has a row.
    fn held_identity(&self, player: &PlayerId) -> Result<Option<(String, Option<String>)>> {
        let rows = self.host.sql().exec(
            "SELECT provider, display_name FROM hearth_held WHERE player_id = ?",
            vec![SqlValue::Text(player.0.clone())],
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        Ok(Some((
            row.get("provider")
                .and_then(Value::as_str)
                .unwrap_or(ANON_PROVIDER)
                .to_owned(),
            row.get("display_name")
                .and_then(Value::as_str)
                .map(str::to_owned),
        )))
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

    /// Delete a held seat's row and grace timer. Returns everything the row
    /// carried, including the identity written when the socket dropped.
    fn expire_held(&self, player: &PlayerId) -> Result<Option<HeldSeat>> {
        let rows = self.host.sql().exec(
            "SELECT seat, until_ms, provider, display_name FROM hearth_held WHERE player_id = ?",
            vec![SqlValue::Text(player.0.clone())],
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let Some(seat) = row.get("seat").and_then(Value::as_u64) else {
            return Ok(None);
        };
        let held = HeldSeat {
            seat: seat as u8,
            until_ms: row
                .get("until_ms")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| self.host.now_ms()),
            provider: row
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or(ANON_PROVIDER)
                .to_owned(),
            display_name: row
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        self.host.sql().exec(
            "DELETE FROM hearth_held WHERE player_id = ?",
            vec![SqlValue::Text(player.0.clone())],
        )?;
        self.timer_delete(&grace_timer_key(player))?;
        Ok(Some(held))
    }

    /// Live joined sockets plus held seats.
    fn occupied(&self) -> Result<usize> {
        Ok(self.joined().len() + self.held_clients()?.len())
    }

    fn any_live_or_held(&self) -> Result<bool> {
        Ok(!self.joined().is_empty()
            || !self.held_clients()?.is_empty()
            || self.has_active_sessions()?)
    }

    fn capture_views(&self) -> CapturedViews<H::Socket> {
        let Some(state) = &self.state else {
            return CapturedViews {
                public: None,
                sockets: Vec::new(),
                owner: Vec::new(),
            };
        };
        let joined = self.joined();
        let sockets = joined.iter().map(|(socket, _)| socket.clone()).collect();
        let public = Some(state.public_view());
        let owner = if R::State::HAS_OWNER {
            joined
                .into_iter()
                .map(|(socket, attachment)| (socket, state.owner_view(&attachment.player_id)))
                .collect()
        } else {
            Vec::new()
        };
        CapturedViews {
            public,
            sockets,
            owner,
        }
    }

    fn patch_joined(
        &self,
        before: &CapturedViews<H::Socket>,
        seq: u64,
        tick: Option<u64>,
        t: Option<u64>,
    ) {
        let (Some(state), Some(public_before)) = (&self.state, &before.public) else {
            return;
        };
        // One public diff serves every client; only owner fields are per client.
        let public_ops = diff(public_before, &state.public_view());
        for (socket, attachment) in self.joined() {
            if !before.sockets.contains(&socket) {
                continue;
            }
            let mut ops = public_ops.clone();
            if R::State::HAS_OWNER {
                let Some((_, owner_before)) = before
                    .owner
                    .iter()
                    .find(|(candidate, _)| candidate == &socket)
                else {
                    continue;
                };
                ops.extend(diff(owner_before, &state.owner_view(&attachment.player_id)));
            }
            if ops.is_empty() {
                continue;
            }
            self.send_frame(&socket, &attachment, Frame::Patch { seq, ops, tick, t });
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
        taken.extend(self.held_clients()?.into_iter().map(|client| client.seat));
        Ok((0..R::MAX_CLIENTS)
            .find(|seat| !taken.contains(seat))
            .unwrap_or(0))
    }
}

/// `on_call` for a room that accepts object-to-object calls. The handler runs
/// as a normal pass, so its state change is patched to the room's clients.
impl<R: RoomRpc, H: Host + 'static> RoomRuntime<R, H> {
    /// Handle one call from another Hearth object.
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
        let before = self.capture_views();
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
                self.patch_joined(&before, seq, None, None);
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
    /// Drop in-memory state as a hibernating Durable Object would; the next event reloads it.
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

/// A nullable TEXT column value.
fn optional_text(value: &Option<String>) -> SqlValue {
    match value {
        Some(text) => SqlValue::Text(text.clone()),
        None => SqlValue::Null,
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
