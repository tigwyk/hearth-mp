//! Production [`Host`] over a hibernating Durable Object, plus the
//! [`RoomObject`] every `#[hearth::room]` impl is wrapped in.
//!
//! Sockets are only ever accepted through the hibernation API; the keepalive
//! `hearth:ping` text frame is answered by the Durable Object auto-response so
//! it never wakes the room.
//!
//! The room also serves the internal `/__hearth/*` paths used by the admin API
//! and by the registry. The Gateway builds those requests itself, so a player
//! can never reach them.

use std::rc::Rc;

use futures::lock::Mutex;
use hearth_protocol::{PlayerId, WireMsg, PING_TEXT, PONG_TEXT};
use worker::{
    console_log, Date, DateInit, Env, Method, Request, RequestInit, Response, ScheduledTime,
    SqlCursor, SqlStorageValue, State, WebSocket, WebSocketIncomingMessage, WebSocketPair,
    WebSocketRequestResponsePair,
};

use crate::host::{Attachment, Host, RoomName, RoomReport, Sql, SqlValue};
use crate::room::Room;
use crate::runtime::RoomRuntime;
use crate::Result;

/// Binding name of the registry Durable Object. Optional: without it the room
/// never reports and `hearth inspect rooms` returns 501.
const REGISTRY_BINDING: &str = "HEARTH_REGISTRY";
/// One registry Durable Object for every room type.
// shortcut: single registry DO (~1k writes/s); shard by hash(room_type, name) when a game outgrows it
const REGISTRY_NAME: &str = "global";
/// The registry's report path.
const REGISTRY_URL: &str = "https://registry/report";
const ROOM_HEADER: &str = "X-Hearth-Room";

/// [`Host`] backed by a Durable Object: hibernation sockets, SQLite storage and
/// the single alarm.
#[derive(Clone)]
pub struct WorkerHost {
    state: Rc<State>,
    env: Env,
    sql: WorkerSql,
}

impl WorkerHost {
    /// The state is shared with [`RoomObject`] because `worker::State` is not
    /// `Clone` (and WASM is single-threaded, so `Rc` is enough).
    pub(crate) fn new(state: Rc<State>, env: Env) -> Self {
        Self {
            sql: WorkerSql::new(Rc::clone(&state)),
            state,
            env,
        }
    }
}

impl Host for WorkerHost {
    type Socket = WebSocket;

    fn sockets(&self) -> Vec<WebSocket> {
        self.state.get_websockets()
    }

    fn attachment(&self, socket: &WebSocket) -> Option<Attachment> {
        socket.deserialize_attachment::<Attachment>().ok().flatten()
    }

    fn set_attachment(&self, socket: &WebSocket, attachment: &Attachment) {
        // The socket may already be gone; there is nothing useful to do then.
        let _ = socket.serialize_attachment(attachment);
    }

    fn send(&self, socket: &WebSocket, msg: WireMsg) {
        let _ = match msg {
            WireMsg::Binary(bytes) => socket.send_with_bytes(bytes),
            WireMsg::Text(text) => socket.send_with_str(text),
        };
    }

    fn close(&self, socket: &WebSocket, code: u16, reason: &str) {
        let _ = socket.close(Some(code), Some(reason));
    }

    fn now_ms(&self) -> u64 {
        Date::now().as_millis()
    }

    fn sql(&self) -> &dyn Sql {
        self
    }

    fn sql_handle(&self) -> Rc<dyn Sql> {
        Rc::new(self.clone())
    }

    async fn set_alarm(&self, at_ms: Option<u64>) -> Result<()> {
        let storage = self.state.storage();
        match at_ms {
            None => storage.delete_alarm().await.map_err(worker_error),
            Some(ms) => {
                let at: worker::js_sys::Date = Date::from(DateInit::Millis(ms)).into();
                storage
                    .set_alarm(ScheduledTime::new(at))
                    .await
                    .map_err(worker_error)
            }
        }
    }

    async fn wipe(&self) -> Result<()> {
        self.state
            .storage()
            .delete_all()
            .await
            .map_err(worker_error)
    }

    /// POST the report to the registry Durable Object. A missing binding is not
    /// an error: the registry is optional.
    async fn report(&self, report: RoomReport) -> Result<()> {
        let Ok(namespace) = self.env.durable_object(REGISTRY_BINDING) else {
            return Ok(());
        };
        let stub = namespace.get_by_name(REGISTRY_NAME).map_err(worker_error)?;
        let body = serde_json::to_string(&report)?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));
        let mut request = Request::new_with_init(REGISTRY_URL, &init).map_err(worker_error)?;
        request
            .headers_mut()
            .map_err(worker_error)?
            .set("content-type", "application/json")
            .map_err(worker_error)?;
        let _ = stub
            .fetch_with_request(request)
            .await
            .map_err(worker_error)?;
        Ok(())
    }

    fn log_line(&self, json: &str) {
        console_log!("{json}");
    }
}

impl Sql for WorkerHost {
    fn exec(
        &self,
        query: &str,
        params: Vec<SqlValue>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
        self.sql.exec(query, params)
    }
}

/// SQL over a Durable Object's SQLite storage, without the rest of [`Host`].
/// The registry Durable Object uses it directly.
#[derive(Clone)]
pub struct WorkerSql {
    state: Rc<State>,
}

impl WorkerSql {
    pub fn new(state: Rc<State>) -> Self {
        Self { state }
    }
}

impl Sql for WorkerSql {
    fn exec(
        &self,
        query: &str,
        params: Vec<SqlValue>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
        let bindings: Vec<SqlStorageValue> = params.into_iter().map(to_binding).collect();
        let cursor: SqlCursor = self
            .state
            .storage()
            .sql()
            .exec(query, bindings)
            .map_err(worker_error)?;
        let columns = cursor.column_names();
        let mut rows = Vec::new();
        for row in cursor.raw() {
            let values = row.map_err(worker_error)?;
            let mut fields = serde_json::Map::with_capacity(values.len());
            for (name, value) in columns.iter().zip(values) {
                fields.insert(name.clone(), to_json(value));
            }
            rows.push(fields);
        }
        Ok(rows)
    }
}

/// The Durable Object wrapper generated by `#[hearth::room]` delegates to this.
/// Every event holds the room mutex for the whole handler, which serialises
/// events that would otherwise interleave at `await` points.
pub struct RoomObject<R: Room> {
    state: Rc<State>,
    rt: Mutex<RoomRuntime<R, WorkerHost>>,
}

impl<R: Room> RoomObject<R> {
    /// Must be called by the generated wrapper's `DurableObject::new`.
    pub fn new(state: State, env: worker::Env) -> Self {
        let state = Rc::new(state);
        // Keepalive never wakes the room: workerd answers `PING_TEXT` itself.
        let pair = WebSocketRequestResponsePair::new(PING_TEXT, PONG_TEXT)
            .expect("hearth: keepalive request/response pair is valid");
        state.set_websocket_auto_response(&pair);
        let host = WorkerHost::new(Rc::clone(&state), env);
        Self {
            state,
            rt: Mutex::new(RoomRuntime::new(host, "")),
        }
    }

    /// Dispatch the room's internal paths. The Gateway and the admin router
    /// build these URLs themselves; a player request never reaches this object.
    pub async fn fetch(&self, req: Request) -> worker::Result<Response> {
        match req.path().as_str() {
            "/__hearth/connect" => self.connect(req).await,
            "/__hearth/inspect" => self.inspect(req).await,
            "/__hearth/dispose" => self.dispose(req).await,
            "/__hearth/tail" => self.tail(req).await,
            _ => Response::error("not found", 404),
        }
    }

    /// The player connect endpoint. The Gateway has already verified the player
    /// and set `X-Hearth-Player-Id` and `X-Hearth-Room`, so both are trusted.
    async fn connect(&self, req: Request) -> worker::Result<Response> {
        if !is_websocket(&req)? {
            return Response::empty()?.with_status(426).cloned();
        }
        let headers = req.headers();
        let Some(player_id) = headers.get("X-Hearth-Player-Id")? else {
            return Response::error("missing X-Hearth-Player-Id header", 400);
        };
        let Some(room) = headers
            .get(ROOM_HEADER)?
            .and_then(|value| parse_room_name(&value))
        else {
            return Response::error("missing or malformed X-Hearth-Room header", 400);
        };

        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        self.rt
            .lock()
            .await
            .connect(pair.server.clone(), PlayerId(player_id), room)
            .await;
        Response::from_websocket(pair.client)
    }

    /// A hibernatable admin tail: an unfiltered, full-state patch stream.
    async fn tail(&self, req: Request) -> worker::Result<Response> {
        if !is_websocket(&req)? {
            return Response::empty()?.with_status(426).cloned();
        }
        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        self.rt.lock().await.connect_admin(pair.server.clone());
        Response::from_websocket(pair.client)
    }

    async fn inspect(&self, req: Request) -> worker::Result<Response> {
        if req.method() != Method::Get {
            return Response::error("not found", 404);
        }
        match self.rt.lock().await.inspect().map_err(runtime_error)? {
            Some(doc) => Response::from_json(&doc),
            None => Response::error("no such room", 404),
        }
    }

    async fn dispose(&self, req: Request) -> worker::Result<Response> {
        if req.method() != Method::Post {
            return Response::error("not found", 404);
        }
        match self
            .rt
            .lock()
            .await
            .force_dispose()
            .await
            .map_err(runtime_error)?
        {
            true => Response::empty()?.with_status(204).cloned(),
            false => Response::error("no such room", 404),
        }
    }

    pub async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> worker::Result<()> {
        let msg = match message {
            WebSocketIncomingMessage::String(text) => WireMsg::Text(text),
            WebSocketIncomingMessage::Binary(bytes) => WireMsg::Binary(bytes),
        };
        self.rt.lock().await.message(ws, msg).await;
        Ok(())
    }

    pub async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> worker::Result<()> {
        self.rt.lock().await.close(ws).await;
        Ok(())
    }

    pub async fn websocket_error(
        &self,
        ws: WebSocket,
        _error: worker::Error,
    ) -> worker::Result<()> {
        self.rt.lock().await.close(ws).await;
        Ok(())
    }

    pub async fn alarm(&self) -> worker::Result<Response> {
        self.rt.lock().await.alarm().await;
        Response::empty()
    }
}

fn is_websocket(req: &Request) -> worker::Result<bool> {
    Ok(req
        .headers()
        .get("Upgrade")?
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket")))
}

/// Split `"<room_type>/<name>"` into a [`RoomName`].
fn parse_room_name(value: &str) -> Option<RoomName> {
    let (room_type, name) = value.split_once('/')?;
    if room_type.is_empty() || name.is_empty() {
        return None;
    }
    Some(RoomName {
        room_type: room_type.to_owned(),
        name: name.to_owned(),
    })
}

fn runtime_error(error: anyhow::Error) -> worker::Error {
    worker::Error::RustError(format!("{error:#}"))
}

fn to_binding(value: SqlValue) -> SqlStorageValue {
    match value {
        SqlValue::Null => SqlStorageValue::Null,
        SqlValue::Int(value) => SqlStorageValue::Integer(value),
        SqlValue::Real(value) => SqlStorageValue::Float(value),
        SqlValue::Text(value) => SqlStorageValue::String(value),
        SqlValue::Blob(value) => SqlStorageValue::Blob(value),
    }
}

/// Blob columns surface as JSON arrays of byte numbers: the runtime's
/// `json_to_bytes` relies on that convention to round-trip its state snapshot.
fn to_json(value: SqlStorageValue) -> serde_json::Value {
    match value {
        SqlStorageValue::Null => serde_json::Value::Null,
        SqlStorageValue::Boolean(value) => serde_json::Value::Bool(value),
        SqlStorageValue::Integer(value) => serde_json::Value::from(value),
        SqlStorageValue::Float(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        SqlStorageValue::String(value) => serde_json::Value::String(value),
        SqlStorageValue::Blob(bytes) => {
            serde_json::Value::Array(bytes.into_iter().map(serde_json::Value::from).collect())
        }
    }
}

fn worker_error(err: worker::Error) -> anyhow::Error {
    anyhow::anyhow!("worker error: {err}")
}
