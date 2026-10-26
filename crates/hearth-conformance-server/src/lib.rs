//! Hearth conformance reference server.
//!
//! The real [`RoomRuntime`] hosting the conformance room ([`room::Conf`]) over
//! local tokio WebSockets, so a third-party SDK can run the scenarios in
//! `conformance/scenarios/` without Cloudflare.
//!
//! - `ws://127.0.0.1:<port>/rooms/conf/<name>?player=<id>` (or `?token=<id>`):
//!   the room socket. There is **no authentication**: the player id is taken
//!   from the query. This is a local test server only, never for production.
//! - `POST /auth/anonymous` with `{"device_id": "<id>"}` answers
//!   `{"token": "<id>", "player_id": "<id>", "display_name": null}`, so an SDK
//!   can use its normal login and join flow; the token *is* the player id.
//! - `GET /schema/conf`: `{"schema_hash": "<hex>"}`.
//!
//! Rooms are not `Send` (the runtime uses `Rc`), so everything runs on one
//! thread: call [`serve`] inside a `tokio::task::LocalSet`. Each room is one
//! task that handles its socket events in order.

pub mod host;
pub mod room;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hearth_protocol::{PlayerId, WireMsg, PING_TEXT, PONG_TEXT};
use hearth_server::{schema_hash_hex, RoomName, RoomRuntime, Session};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use host::{Out, Socket, TokioHost};
use room::Conf;

/// The room type the server hosts.
pub const ROOM_TYPE: &str = "conf";

/// The conformance room's schema hash.
pub fn schema_hash() -> String {
    schema_hash_hex::<Conf>()
}

enum Cmd {
    Connect { socket: Socket, player: String, out: UnboundedSender<Out> },
    Message(Socket, WireMsg),
    Closed(Socket),
}

type Rooms = Rc<RefCell<HashMap<String, UnboundedSender<Cmd>>>>;

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);

/// Accept connections forever. Must run inside a `tokio::task::LocalSet`.
pub async fn serve(listener: TcpListener) -> std::io::Result<()> {
    let rooms: Rooms = Rc::new(RefCell::new(HashMap::new()));
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::task::spawn_local(connection(stream, rooms.clone()));
    }
}

async fn connection(stream: TcpStream, rooms: Rooms) {
    // Plain HTTP for the schema and login routes; everything else is a
    // WebSocket upgrade.
    let mut head = [0u8; 256];
    let n = stream.peek(&mut head).await.unwrap_or(0);
    let request_line = String::from_utf8_lossy(&head[..n]).into_owned();
    if request_line.starts_with("GET /schema/") || request_line.starts_with("POST /auth/") {
        let _ = http(stream).await;
        return;
    }

    let mut target: Option<(String, String)> = None;
    let callback = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        match parse_room_path(request.uri().path(), request.uri().query().unwrap_or("")) {
            Some(found) => {
                target = Some(found);
                Ok(response)
            }
            None => {
                let mut error = ErrorResponse::new(Some("expected /rooms/conf/<name>?player=<id>".to_owned()));
                *error.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::NOT_FOUND;
                Err(error)
            }
        }
    };
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
        return;
    };
    let Some((name, player)) = target else {
        return;
    };

    let socket = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
    let (out_tx, mut out_rx) = unbounded_channel::<Out>();
    let room = room_sender(&rooms, &name);
    if room.send(Cmd::Connect { socket, player, out: out_tx.clone() }).is_err() {
        return;
    }

    let (mut ws_tx, mut ws_rx) = ws.split();
    let writer = tokio::task::spawn_local(async move {
        while let Some(out) = out_rx.recv().await {
            match out {
                Out::Frame(WireMsg::Binary(bytes)) => {
                    if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Out::Frame(WireMsg::Text(text)) => {
                    if ws_tx.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Out::Close(code, reason) => {
                    let frame = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
                    let _ = ws_tx.send(Message::Close(Some(frame))).await;
                    break;
                }
            }
        }
    });

    while let Some(Ok(message)) = ws_rx.next().await {
        let wire = match message {
            Message::Binary(bytes) => WireMsg::Binary(bytes.to_vec()),
            Message::Text(text) if text.as_str() == PING_TEXT => {
                // The Durable Object auto-response, without waking the room.
                let _ = out_tx.send(Out::Frame(WireMsg::Text(PONG_TEXT.to_owned())));
                continue;
            }
            Message::Text(text) => WireMsg::Text(text.to_string()),
            Message::Close(_) => break,
            _ => continue,
        };
        if room.send(Cmd::Message(socket, wire)).is_err() {
            break;
        }
    }
    let _ = room.send(Cmd::Closed(socket));
    drop(out_tx);
    let _ = writer.await;
}

/// `/rooms/conf/<name>` plus `player=<id>` in the query.
fn parse_room_path(path: &str, query: &str) -> Option<(String, String)> {
    let mut parts = path.trim_start_matches('/').split('/');
    let (Some("rooms"), Some(ROOM_TYPE), Some(name), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let player = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("player=").or_else(|| pair.strip_prefix("token=")))?;
    (!name.is_empty() && !player.is_empty()).then(|| (name.to_owned(), player.to_owned()))
}

/// One plain HTTP request: `GET /schema/<type>` or `POST /auth/anonymous`.
async fn http(mut stream: TcpStream) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read the head, then as much body as Content-Length says.
    let (head, body) = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        request.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&request).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            let head = text[..end].to_owned();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break (head, String::from_utf8_lossy(&request[end + 4..end + 4 + length]).into_owned());
            }
        }
    };
    let target = head.split(' ').nth(1).unwrap_or("");
    let (status, body) = if let Some(room_type) = target.strip_prefix("/schema/") {
        if room_type == ROOM_TYPE {
            ("200 OK", serde_json::json!({ "schema_hash": schema_hash() }).to_string())
        } else {
            ("404 Not Found", "unknown room type".to_owned())
        }
    } else if target == "/auth/anonymous" {
        let device = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("device_id").and_then(|d| d.as_str()).map(str::to_owned))
            .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        match device {
            Some(id) => (
                "200 OK",
                serde_json::json!({ "token": id, "player_id": id, "display_name": null }).to_string(),
            ),
            None => ("400 Bad Request", "device_id must be [A-Za-z0-9_-]+".to_owned()),
        }
    } else {
        ("404 Not Found", "not found".to_owned())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn room_sender(rooms: &Rooms, name: &str) -> UnboundedSender<Cmd> {
    if let Some(tx) = rooms.borrow().get(name) {
        if !tx.is_closed() {
            return tx.clone();
        }
    }
    let (tx, rx) = unbounded_channel();
    tokio::task::spawn_local(room_task(name.to_owned(), rx));
    rooms.borrow_mut().insert(name.to_owned(), tx.clone());
    tx
}

/// One room: socket events in order, plus the runtime's alarm.
async fn room_task(name: String, mut rx: UnboundedReceiver<Cmd>) {
    let host = TokioHost::new();
    let mut runtime = RoomRuntime::<Conf, TokioHost>::new(host.clone(), "CONF");
    let room = RoomName { room_type: ROOM_TYPE.to_owned(), name };
    loop {
        let wait = host.alarm_at().map(|at| Duration::from_millis(at.saturating_sub(hearth_server::Host::now_ms(&host))));
        let cmd = tokio::select! {
            cmd = rx.recv() => cmd,
            _ = tokio::time::sleep(wait.unwrap_or_default()), if wait.is_some() => {
                // Clear first: the runtime sets the next alarm itself.
                let _ = hearth_server::Host::set_alarm(&host, None).await;
                runtime.alarm().await;
                continue;
            }
        };
        match cmd {
            None => break,
            Some(Cmd::Connect { socket, player, out }) => {
                host.add_socket(socket, out);
                runtime.connect(socket, Session::anon(PlayerId(player)), room.clone(), None).await;
            }
            Some(Cmd::Message(socket, wire)) => runtime.message(socket, wire).await,
            Some(Cmd::Closed(socket)) => {
                // A socket the room already closed (kick, replace) is gone already.
                if host.sockets().contains(&socket) {
                    runtime.close(socket).await;
                    host.remove_socket(socket);
                }
            }
        }
    }
}

use hearth_server::Host as _;

/// Bind `127.0.0.1:<port>` (0 for any free port) and serve until the process
/// ends, on the current thread.
pub fn run(port: u16) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        eprintln!(
            "hearth-conformance-server: ws://{}/rooms/conf/<name>?player=<id> (schema {}). Local testing only.",
            listener.local_addr()?,
            schema_hash()
        );
        serve(listener).await
    })
}
