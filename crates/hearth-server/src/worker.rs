//! Production [`Host`] over a hibernating Durable Object, plus the
//! [`RoomObject`] and [`ServiceObject`] every room class and service class is
//! wrapped in.
//!
//! Sockets are only ever accepted through the hibernation API; the keepalive
//! `hearth:ping` text frame is answered by the Durable Object auto-response so
//! it never wakes the room.
//!
//! Every object also serves the internal `/__hearth/*` paths used by the admin
//! API, the registry and object-to-object calls. Hearth builds those requests
//! itself, so a player can never reach them.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use futures::lock::Mutex;
use hearth_protocol::{PlayerId, Value, WireMsg, PING_TEXT, PONG_TEXT};
use worker::{
    console_log, Date, DateInit, Delay, Env, Method, Request, RequestInit, Response, ScheduledTime,
    SqlCursor, SqlStorageValue, State, WebSocket, WebSocketIncomingMessage, WebSocketPair,
    WebSocketRequestResponsePair,
};

use crate::call::{CallError, Envelope, Target};
use crate::host::{Attachment, Host, RoomName, Session, Sql, SqlValue, ANON_PROVIDER};
use crate::room::{Room, RoomRpc};
use crate::runtime::RoomRuntime;
use crate::service::{PlayerService, Service, ServiceRuntime};
use crate::Result;

/// The room's type and name. The Gateway builds this header itself.
const ROOM_HEADER: &str = "X-Hearth-Room";
/// The target object's own name. Every caller sets it, so a receiver knows its
/// address (and can check the call chain) before taking its runtime lock.
const NAME_HEADER: &str = "X-Hearth-Name";
const CALL_PATH: &str = "/__hearth/call";
const CALL_URL: &str = "https://hearth/__hearth/call";

/// [`Host`] backed by a Durable Object: hibernation sockets, SQLite storage and
/// the single alarm.
#[derive(Clone)]
pub struct WorkerHost {
    state: Rc<State>,
    env: Env,
    sql: WorkerSql,
}

impl WorkerHost {
    /// The state is shared with the object wrapper because `worker::State` is not
    /// `Clone` (and WASM is single-threaded, so `Rc` is enough).
    pub(crate) fn new(state: Rc<State>, env: Env) -> Self {
        Self {
            sql: WorkerSql::new(Rc::clone(&state)),
            state,
            env,
        }
    }

    /// POST one [`Envelope`] to another object and decode its reply.
    async fn fetch_object(
        &self,
        to: &Target,
        mut init: RequestInit,
    ) -> std::result::Result<Value, CallError> {
        let namespace = self
            .env
            .durable_object(&to.binding)
            .map_err(|err| CallError::Unreachable(format!("no binding {}: {err}", to.binding)))?;
        let id = namespace
            .id_from_name(&to.name)
            .map_err(|err| CallError::Unreachable(format!("bad object name: {err}")))?;
        let stub = id
            .get_stub()
            .map_err(|err| CallError::Unreachable(format!("no stub: {err}")))?;
        init.with_method(Method::Post);
        let request = Request::new_with_init(CALL_URL, &init)
            .map_err(|err| CallError::Unreachable(format!("bad request: {err}")))?;
        let mut response = stub
            .fetch_with_request(request)
            .await
            .map_err(|err| CallError::Unreachable(format!("fetch failed: {err}")))?;
        if response.status_code() != 200 {
            return Err(CallError::Unreachable(format!(
                "{} answered {}",
                to.binding,
                response.status_code()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| CallError::Unreachable(format!("body read failed: {err}")))?;
        rmp_serde::from_slice::<std::result::Result<Value, CallError>>(&bytes)
            .map_err(|err| CallError::Unreachable(format!("reply did not decode: {err}")))?
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

    async fn call(&self, to: &Target, envelope: Envelope) -> std::result::Result<Value, CallError> {
        let body = rmp_serde::to_vec(&envelope)
            .map_err(|err| CallError::BadRpc(format!("envelope did not encode: {err}")))?;
        let headers = worker::Headers::new();
        headers
            .set(NAME_HEADER, &to.name)
            .map_err(|err| CallError::Unreachable(format!("bad header: {err}")))?;
        headers
            .set("content-type", "application/msgpack")
            .map_err(|err| CallError::Unreachable(format!("bad header: {err}")))?;
        let mut init = RequestInit::new();
        init.with_headers(headers)
            .with_body(Some(worker::wasm_bindgen::JsValue::from(
                worker::js_sys::Uint8Array::from(body.as_slice()),
            )));
        self.fetch_object(to, init).await
    }

    fn has_binding(&self, binding: &str) -> bool {
        self.env.durable_object(binding).is_ok()
    }

    /// Ticks are driven by [`tick_loop`]; the alarm only fires timers and the
    /// tick watchdog. The deployed benchmark (ADR 0001) is why: the Alarms API
    /// delivered 50 ms ticks with a p99 error of 49-98 ms and skipped 15-58% of
    /// slots, while the in-object loop held 0-1 ms p99 and skipped none.
    fn loops_ticks(&self) -> bool {
        true
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

/// Call another Hearth object from a Worker (the gateway calls the matchmaker
/// and the player objects this way).
pub async fn call_object(
    env: &Env,
    to: &Target,
    envelope: Envelope,
) -> std::result::Result<Value, CallError> {
    let body = rmp_serde::to_vec(&envelope)
        .map_err(|err| CallError::BadRpc(format!("envelope did not encode: {err}")))?;
    let headers = worker::Headers::new();
    headers
        .set(NAME_HEADER, &to.name)
        .map_err(|err| CallError::Unreachable(format!("bad header: {err}")))?;
    headers
        .set("content-type", "application/msgpack")
        .map_err(|err| CallError::Unreachable(format!("bad header: {err}")))?;
    let mut init = RequestInit::new();
    init.with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from(
            worker::js_sys::Uint8Array::from(body.as_slice()),
        )));
    let namespace = env
        .durable_object(&to.binding)
        .map_err(|err| CallError::Unreachable(format!("no binding {}: {err}", to.binding)))?;
    let id = namespace
        .id_from_name(&to.name)
        .map_err(|err| CallError::Unreachable(format!("bad object name: {err}")))?;
    let stub = id
        .get_stub()
        .map_err(|err| CallError::Unreachable(format!("no stub: {err}")))?;
    init.with_method(Method::Post);
    let request = Request::new_with_init(CALL_URL, &init)
        .map_err(|err| CallError::Unreachable(format!("bad request: {err}")))?;
    let mut response = stub
        .fetch_with_request(request)
        .await
        .map_err(|err| CallError::Unreachable(format!("fetch failed: {err}")))?;
    if response.status_code() != 200 {
        return Err(CallError::Unreachable(format!(
            "{} answered {}",
            to.binding,
            response.status_code()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| CallError::Unreachable(format!("body read failed: {err}")))?;
    rmp_serde::from_slice::<std::result::Result<Value, CallError>>(&bytes)
        .map_err(|err| CallError::Unreachable(format!("reply did not decode: {err}")))?
}

/// The Durable Object wrapper generated by `#[hearth::room]` delegates to this.
/// Every event holds the room mutex for the whole handler, which serialises
/// events that would otherwise interleave at `await` points.
pub struct RoomObject<R: Room> {
    state: Rc<State>,
    binding: &'static str,
    rt: Rc<Mutex<RoomRuntime<R, WorkerHost>>>,
    tick_loop: Rc<TickLoop>,
}

/// Bookkeeping for the in-object tick loop. The loop itself lives in
/// `State::wait_until`, which is what keeps a ticking object in memory.
struct TickLoop {
    generation: Cell<u64>,
    running: Cell<bool>,
}

impl Default for TickLoop {
    fn default() -> Self {
        Self {
            generation: Cell::new(0),
            running: Cell::new(false),
        }
    }
}

impl<R: Room> RoomObject<R> {
    /// Must be called by the generated wrapper's `DurableObject::new`.
    pub fn new(state: State, env: worker::Env, binding: &'static str) -> Self {
        let state = Rc::new(state);
        // Keepalive never wakes the room: workerd answers `PING_TEXT` itself.
        let pair = WebSocketRequestResponsePair::new(PING_TEXT, PONG_TEXT)
            .expect("hearth: keepalive request/response pair is valid");
        state.set_websocket_auto_response(&pair);
        let host = WorkerHost::new(Rc::clone(&state), env);
        Self {
            state,
            binding,
            rt: Rc::new(Mutex::new(RoomRuntime::new(host, binding))),
            tick_loop: Rc::new(TickLoop::default()),
        }
    }

    /// Start the tick loop if the room is ticking, or renew its keepalive.
    ///
    /// Alarms call this with `renew` so each watchdog alarm starts a fresh
    /// `wait_until` operation (a pending operation prevents eviction for at most
    /// 15 minutes) and restarts the loop after an eviction. Every other event
    /// only has to start the loop when it is not running.
    async fn ensure_tick_loop(&self, renew: bool) {
        if self.rt.lock().await.tick_deadline().is_none() {
            return;
        }
        if self.tick_loop.running.get() && !renew {
            return;
        }
        let generation = self.tick_loop.generation.get().wrapping_add(1);
        self.tick_loop.generation.set(generation);
        self.tick_loop.running.set(true);
        let rt = Rc::clone(&self.rt);
        let loop_state = Rc::clone(&self.tick_loop);
        self.state
            .wait_until(async move { tick_loop(rt, loop_state, generation).await });
    }

    /// Dispatch the room's internal paths. The Gateway and the admin router
    /// build these URLs themselves; a player request never reaches this object.
    pub async fn fetch(&self, req: Request) -> worker::Result<Response> {
        let response = match req.path().as_str() {
            "/__hearth/connect" => self.connect(req).await,
            "/__hearth/inspect" => self.inspect(req).await,
            "/__hearth/dispose" => self.dispose(req).await,
            "/__hearth/tail" => self.tail(req).await,
            // A room without `RoomRpc` rejects calls here; the macro routes
            // this path to `call` only for a room that accepts them.
            CALL_PATH => msgpack_error(CallError::BadRpc(
                "room does not accept rpc (add `rpc` to #[hearth::room] and impl RoomRpc)"
                    .to_owned(),
            )),
            _ => Response::error("not found", 404),
        };
        // A join may have started ticking; a dispose may have stopped it.
        self.ensure_tick_loop(false).await;
        response
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

        let matchmaker = headers.get("X-Hearth-Matchmaker")?;
        // These headers are set by the gateway from a *verified* token only; a
        // player can never reach this internal path. The room trusts them and
        // never sees the raw credential.
        let provider = headers
            .get("X-Hearth-Provider")?
            .unwrap_or_else(|| ANON_PROVIDER.to_owned());
        let display_name = headers
            .get("X-Hearth-Display-Name")?
            .map(|value| percent_decode(&value))
            .filter(|name| !name.is_empty());
        let session = Session {
            player: PlayerId(player_id),
            provider,
            display_name,
        };
        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        self.rt
            .lock()
            .await
            .connect(pair.server.clone(), session, room, matchmaker)
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
        // A first intent or a recovered socket restarts a lost loop, and the
        // first join starts ticking.
        self.ensure_tick_loop(false).await;
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
        self.ensure_tick_loop(false).await;
        Ok(())
    }

    pub async fn websocket_error(
        &self,
        ws: WebSocket,
        _error: worker::Error,
    ) -> worker::Result<()> {
        self.rt.lock().await.close(ws).await;
        self.ensure_tick_loop(false).await;
        Ok(())
    }

    pub async fn alarm(&self) -> worker::Result<Response> {
        self.rt.lock().await.alarm().await;
        // The watchdog alarm: renew the loop's keepalive window and restart it
        // after an eviction, even when no client event woke the object.
        self.ensure_tick_loop(true).await;
        Response::empty()
    }
}

/// Drive ticks while the room is ticking.
///
/// The loop lives inside `State::wait_until`, which keeps the object in memory;
/// the runtime alarm watchdog re-enters here with a new generation so each
/// keepalive operation gets a fresh eviction window. The loop exits when ticking
/// stops or a newer loop supersedes it.
async fn tick_loop<R: Room>(
    rt: Rc<Mutex<RoomRuntime<R, WorkerHost>>>,
    loop_state: Rc<TickLoop>,
    generation: u64,
) {
    loop {
        if loop_state.generation.get() != generation {
            break;
        }
        let Some(deadline) = rt.lock().await.tick_deadline() else {
            break;
        };
        let now = Date::now().as_millis();
        if deadline > now {
            Delay::from(Duration::from_millis(deadline - now)).await;
        }
        if loop_state.generation.get() != generation {
            break;
        }
        let before = rt.lock().await.tick_deadline();
        rt.lock().await.tick_if_due().await;
        let after = rt.lock().await.tick_deadline();
        if before == after {
            // Nothing rescheduled (a failed tick): do not spin. The schedule is
            // integer milliseconds, so this guard is defensive only.
            Delay::from(Duration::from_millis(10)).await;
        }
    }
    if loop_state.generation.get() == generation {
        loop_state.running.set(false);
    }
}

/// `on_call` for a room that accepts object-to-object calls. The macro routes
/// `/__hearth/call` here only for a room that implements [`RoomRpc`].
impl<R: RoomRpc> RoomObject<R> {
    pub async fn call(&self, mut req: Request) -> worker::Result<Response> {
        if req.method() != Method::Post {
            return Response::error("not found", 404);
        }
        let Some(name) = req.headers().get(NAME_HEADER)? else {
            return Response::error("missing X-Hearth-Name header", 400);
        };
        let bytes = req.bytes().await?;
        let envelope: Envelope = match rmp_serde::from_slice(&bytes) {
            Ok(envelope) => envelope,
            Err(_) => {
                return msgpack_error(CallError::BadRpc(
                    "body is not a MessagePack Envelope".to_owned(),
                ))
            }
        };
        // The chain check happens before the mutex: a reentrant call would wait
        // for a turn this object cannot take.
        let me = Target::new(self.binding, &name);
        if envelope.chain.contains(&me) {
            return msgpack_error(CallError::ReentrantCall(me));
        }
        let reply = self.rt.lock().await.call(envelope, &name).await;
        self.ensure_tick_loop(false).await;
        msgpack_reply(&reply)
    }
}

/// The Durable Object wrapper generated by `#[hearth::service]` delegates to
/// this: a socket-less object with durable state, timers and SQL.
pub struct ServiceObject<S: Service> {
    binding: &'static str,
    rt: Mutex<ServiceRuntime<S, WorkerHost>>,
}

impl<S: Service> ServiceObject<S> {
    /// Must be called by the generated wrapper's `DurableObject::new`.
    pub fn new(state: State, env: worker::Env, binding: &'static str) -> Self {
        Self::new_shared(Rc::new(state), env, binding)
    }

    /// For a wrapper that also owns other state derived from the same DO (the
    /// registry serves its admin listing next to its service path).
    pub fn new_shared(state: Rc<State>, env: worker::Env, binding: &'static str) -> Self {
        let host = WorkerHost::new(Rc::clone(&state), env);
        Self {
            binding,
            rt: Mutex::new(ServiceRuntime::new(host, binding)),
        }
    }

    pub async fn fetch(&self, req: Request) -> worker::Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Post, CALL_PATH) => self.call(req).await,
            (Method::Get, "/__hearth/inspect") => match self.rt.lock().await.inspect().await {
                Ok(Some(state)) => Response::from_json(&state),
                Ok(None) => Response::error("no such object", 404),
                Err(err) => Response::error(format!("inspect failed: {err:#}"), 500),
            },
            _ => Response::error("not found", 404),
        }
    }

    async fn call(&self, req: Request) -> worker::Result<Response> {
        service_call(self.binding, &self.rt, req).await
    }

    pub async fn alarm(&self) -> worker::Result<()> {
        self.rt.lock().await.alarm().await;
        Ok(())
    }
}

/// One service call: decode the envelope, check the chain before taking the
/// runtime lock, run `on_call`, reply in MessagePack. Shared by [`ServiceObject`]
/// and [`PlayerObject`].
async fn service_call<S: Service>(
    binding: &'static str,
    rt: &Mutex<ServiceRuntime<S, WorkerHost>>,
    mut req: Request,
) -> worker::Result<Response> {
    let Some(name) = req.headers().get(NAME_HEADER)? else {
        return Response::error("missing X-Hearth-Name header", 400);
    };
    let bytes = req.bytes().await?;
    let envelope: Envelope = match rmp_serde::from_slice(&bytes) {
        Ok(envelope) => envelope,
        Err(_) => {
            return msgpack_error(CallError::BadRpc(
                "body is not a MessagePack Envelope".to_owned(),
            ))
        }
    };
    let me = Target::new(binding, &name);
    if envelope.chain.contains(&me) {
        return msgpack_error(CallError::ReentrantCall(me));
    }
    let reply = rt.lock().await.call(envelope, &name).await;
    msgpack_reply(&reply)
}

/// The Durable Object wrapper generated by
/// `#[hearth::service(binding = "...", player)]` delegates to this instead of a
/// [`ServiceObject`]: the same service surface plus the read-only player view.
pub struct PlayerObject<S: PlayerService> {
    binding: &'static str,
    rt: Mutex<ServiceRuntime<S, WorkerHost>>,
}

impl<S: PlayerService> PlayerObject<S> {
    /// Must be called by the generated wrapper's `DurableObject::new`.
    pub fn new(state: State, env: worker::Env, binding: &'static str) -> Self {
        let host = WorkerHost::new(Rc::new(state), env);
        Self {
            binding,
            rt: Mutex::new(ServiceRuntime::new(host, binding)),
        }
    }

    pub async fn fetch(&self, req: Request) -> worker::Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Post, CALL_PATH) => service_call(self.binding, &self.rt, req).await,
            (Method::Get, "/__hearth/view") => {
                let mut rt = self.rt.lock().await;
                if let Err(err) = rt.load().await {
                    return Response::error(format!("player load failed: {err:#}"), 500);
                }
                match rt.state() {
                    Some(state) => Response::from_json(&S::client_view(state)),
                    None => Response::error("player state unavailable", 500),
                }
            }
            (Method::Get, "/__hearth/inspect") => match self.rt.lock().await.inspect().await {
                Ok(Some(state)) => Response::from_json(&state),
                Ok(None) => Response::error("no such player", 404),
                Err(err) => Response::error(format!("inspect failed: {err:#}"), 500),
            },
            _ => Response::error("not found", 404),
        }
    }

    pub async fn alarm(&self) -> worker::Result<()> {
        self.rt.lock().await.alarm().await;
        Ok(())
    }
}

/// A MessagePack reply carrying one call's result.
fn msgpack_reply(value: &std::result::Result<Value, CallError>) -> worker::Result<Response> {
    let body = rmp_serde::to_vec(value).map_err(runtime_error)?;
    let mut response = Response::from_bytes(body)?;
    response
        .headers_mut()
        .set("content-type", "application/msgpack")?;
    Ok(response)
}

/// A MessagePack reply carrying one call failure.
fn msgpack_error(error: CallError) -> worker::Result<Response> {
    msgpack_reply(&Err(error))
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

fn runtime_error(error: impl std::fmt::Display) -> worker::Error {
    worker::Error::RustError(format!("{error}"))
}

/// Decode a percent-encoded header value (`%XX`). The gateway percent-encodes
/// the display name so CR/LF can never reach a header; a malformed escape is
/// kept literally rather than panicking the room.
fn percent_decode(value: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
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
