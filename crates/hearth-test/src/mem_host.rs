//! In-memory [`Host`] for the test harness: a virtual clock, fake sockets and
//! an in-memory SQLite database.
//!
//! Sockets, attachments and the send journal all live behind `Rc`, so
//! hibernating the room (dropping the runtime) keeps the same environment.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use hearth_protocol::WireMsg;
use hearth_server::{Attachment, Host, Result, RoomReport, Sql, SqlValue};
use serde_json::Value;

/// Socket identity handed out by [`MemHost`].
pub type Socket = usize;

/// One observable host event, in the order the runtime produced it.
#[derive(Clone, Debug)]
pub(crate) enum MemEvent {
    Sent {
        socket: Socket,
        msg: WireMsg,
    },
    Closed {
        socket: Socket,
        code: u16,
        reason: String,
    },
}

struct Inner {
    clock: Cell<u64>,
    next_socket: Cell<Socket>,
    sockets: RefCell<Vec<Socket>>,
    attachments: RefCell<HashMap<Socket, Attachment>>,
    journal: RefCell<Vec<MemEvent>>,
    alarm: Cell<Option<u64>>,
    db: RefCell<rusqlite::Connection>,
    reports: RefCell<Vec<RoomReport>>,
    logs: RefCell<Vec<String>>,
    fail_reports: Cell<bool>,
    full_state_count: Cell<usize>,
}

/// A [`Host`] backed by an in-memory SQLite database and fake sockets.
///
/// Cloning a `MemHost` shares the same environment, which lets the harness keep
/// one handle while the runtime owns another.
#[derive(Clone)]
pub struct MemHost {
    inner: Rc<Inner>,
}

impl MemHost {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(Inner {
                clock: Cell::new(0),
                next_socket: Cell::new(0),
                sockets: RefCell::new(Vec::new()),
                attachments: RefCell::new(HashMap::new()),
                journal: RefCell::new(Vec::new()),
                alarm: Cell::new(None),
                db: RefCell::new(
                    rusqlite::Connection::open_in_memory().expect("open an in-memory SQLite db"),
                ),
                reports: RefCell::new(Vec::new()),
                logs: RefCell::new(Vec::new()),
                fail_reports: Cell::new(false),
                full_state_count: Cell::new(0),
            }),
        }
    }

    /// Register a new (unattached) socket and return its identity.
    pub fn open_socket(&self) -> Socket {
        let socket = self.inner.next_socket.get();
        self.inner.next_socket.set(socket + 1);
        self.inner.sockets.borrow_mut().push(socket);
        socket
    }

    /// The room's SQLite, without importing [`Host`]. `Registry` tests use this.
    pub fn sql(&self) -> &dyn Sql {
        self
    }

    /// Every report the runtime sent, in order.
    pub fn reports(&self) -> Vec<RoomReport> {
        self.inner.reports.borrow().clone()
    }

    /// Every JSON log line the runtime emitted, in order.
    pub fn logs(&self) -> Vec<String> {
        self.inner.logs.borrow().clone()
    }

    /// Make `Host::report` fail, so tests can prove a registry outage never
    /// affects gameplay.
    pub fn fail_reports(&self, on: bool) {
        self.inner.fail_reports.set(on);
    }

    /// How many unfiltered full-state renders the admin tail asked for.
    pub fn full_state_count(&self) -> usize {
        self.inner.full_state_count.get()
    }

    /// Move the virtual clock, without firing alarms.
    pub fn set_clock(&self, now_ms: u64) {
        self.inner.clock.set(now_ms);
    }

    /// The alarm time the runtime last set, if any.
    pub fn alarm_at(&self) -> Option<u64> {
        self.inner.alarm.get()
    }

    /// Drain the journal of sent frames and closes, in production order.
    pub(crate) fn take_journal(&self) -> Vec<MemEvent> {
        std::mem::take(&mut *self.inner.journal.borrow_mut())
    }
}

impl Default for MemHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for MemHost {
    type Socket = Socket;

    fn sockets(&self) -> Vec<Socket> {
        self.inner.sockets.borrow().clone()
    }

    fn attachment(&self, socket: &Socket) -> Option<Attachment> {
        self.inner.attachments.borrow().get(socket).cloned()
    }

    fn set_attachment(&self, socket: &Socket, attachment: &Attachment) {
        if self.inner.sockets.borrow().contains(socket) {
            self.inner
                .attachments
                .borrow_mut()
                .insert(*socket, attachment.clone());
        }
    }

    fn send(&self, socket: &Socket, msg: WireMsg) {
        // Like a closed WebSocket, a socket that is gone accepts no more frames.
        if self.inner.sockets.borrow().contains(socket) {
            self.inner.journal.borrow_mut().push(MemEvent::Sent {
                socket: *socket,
                msg,
            });
        }
    }

    fn close(&self, socket: &Socket, code: u16, reason: &str) {
        let present = {
            let mut sockets = self.inner.sockets.borrow_mut();
            match sockets.iter().position(|candidate| candidate == socket) {
                Some(index) => {
                    sockets.remove(index);
                    true
                }
                None => false,
            }
        };
        if present {
            self.inner.attachments.borrow_mut().remove(socket);
            self.inner.journal.borrow_mut().push(MemEvent::Closed {
                socket: *socket,
                code,
                reason: reason.to_owned(),
            });
        }
    }

    fn now_ms(&self) -> u64 {
        self.inner.clock.get()
    }

    fn sql(&self) -> &dyn Sql {
        self
    }

    fn sql_handle(&self) -> Rc<dyn Sql> {
        Rc::new(self.clone())
    }

    async fn set_alarm(&self, at_ms: Option<u64>) -> Result<()> {
        self.inner.alarm.set(at_ms);
        Ok(())
    }

    async fn wipe(&self) -> Result<()> {
        let db = self.inner.db.borrow();
        let tables: Vec<String> = {
            let mut statement = db.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )?;
            let rows = statement.query_map([], |row| row.get::<usize, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()?
        };
        for table in tables {
            db.execute_batch(&format!("DROP TABLE IF EXISTS \"{table}\""))?;
        }
        Ok(())
    }

    async fn report(&self, report: RoomReport) -> Result<()> {
        if self.inner.fail_reports.get() {
            anyhow::bail!("the registry is unavailable (injected failure)");
        }
        self.inner.reports.borrow_mut().push(report);
        Ok(())
    }

    fn log_line(&self, json: &str) {
        self.inner.logs.borrow_mut().push(json.to_owned());
    }

    fn count_full_state(&self) {
        self.inner
            .full_state_count
            .set(self.inner.full_state_count.get() + 1);
    }
}

impl Sql for MemHost {
    fn exec(
        &self,
        query: &str,
        params: Vec<SqlValue>,
    ) -> Result<Vec<serde_json::Map<String, Value>>> {
        let db = self.inner.db.borrow();
        let mut statement = db.prepare(query)?;
        let params: Vec<rusqlite::types::Value> = params.into_iter().map(to_rusqlite).collect();
        if statement.column_count() == 0 {
            statement.execute(rusqlite::params_from_iter(params))?;
            return Ok(Vec::new());
        }
        let columns: Vec<String> = statement.column_names().map(str::to_owned).collect();
        let mut rows = statement.query(rusqlite::params_from_iter(params))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut map = serde_json::Map::new();
            for (index, name) in columns.iter().enumerate() {
                let value = row.get::<usize, rusqlite::types::Value>(index)?;
                map.insert(name.clone(), from_rusqlite(value));
            }
            out.push(map);
        }
        Ok(out)
    }
}

/// The [`SqlValue`] to SQLite mapping.
pub(crate) fn to_rusqlite(value: SqlValue) -> rusqlite::types::Value {
    match value {
        SqlValue::Null => rusqlite::types::Value::Null,
        SqlValue::Int(number) => rusqlite::types::Value::Integer(number),
        SqlValue::Real(number) => rusqlite::types::Value::Real(number),
        SqlValue::Text(text) => rusqlite::types::Value::Text(text),
        SqlValue::Blob(bytes) => rusqlite::types::Value::Blob(bytes),
    }
}

/// SQLite values surface JSON-shaped. Byte blobs become JSON arrays of byte
/// values; this is the convention the runtime's state snapshot round-trip uses.
pub(crate) fn from_rusqlite(value: rusqlite::types::Value) -> Value {
    match value {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(number) => Value::from(number),
        rusqlite::types::Value::Real(number) => serde_json::Number::from_f64(number)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        rusqlite::types::Value::Text(text) => Value::String(text),
        rusqlite::types::Value::Blob(bytes) => {
            Value::Array(bytes.into_iter().map(Value::from).collect())
        }
    }
}
