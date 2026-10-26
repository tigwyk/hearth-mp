//! Hearth v0.5 sample: the dedicated arena server that runs in a Cloudflare
//! Container for the handoff flow.
//!
//! The lobby hands two players to this process. It boots, reports `started`, and
//! waits for the gateway to proxy player WebSockets to it. Each accepted
//! connection carries the identity the gateway redeemed from the player's
//! single-use ticket (never anything the client supplied). Once two players are
//! in, the server runs a five-second "match" that broadcasts the server time at
//! 30 Hz, reports `result` and `ended`, and exits.
//!
//! Environment (set by the container adapter):
//! - `HEARTH_SID`, `HEARTH_SESSION_SECRET`, `HEARTH_REPORT_URL`: the handoff
//!   session. Without them the process still serves, for local `curl` checks.
//! - `HEARTH_PORT` (default 9000): the port to listen on.
//!
//! The container health check is `GET /healthz` on the same port.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hearth_handoff_server::{ServerReport, Session, DEFAULT_HEARTBEAT};
use hearth_server::PlayerId;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// The port inside the container; `HEARTH_PORT` overrides it.
const DEFAULT_PORT: u16 = 9000;
/// How long a match runs, and how often it broadcasts.
const MATCH_DURATION: Duration = Duration::from_secs(5);
const TICK_RATE_HZ: u32 = 30;
/// How many players a match waits for.
const PLAYERS_FOR_MATCH: usize = 2;

fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build the tokio runtime")
        .block_on(run());
}

async fn run() {
    let port = std::env::var("HEARTH_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let session = match Session::from_env() {
        Ok(session) => Some(session),
        Err(error) => {
            eprintln!("no handoff session ({error}); serving without lifecycle reports");
            None
        }
    };

    // The room's "server is alive" check runs on its own threads, like a normal
    // dedicated server's lifecycle calls would.
    if let Some(session) = session.clone() {
        std::thread::spawn(move || {
            if let Err(error) = session.report(&ServerReport::Started) {
                eprintln!("started report failed: {error}");
            }
            // The heartbeat thread outlives this one; `Ended` or `Gone` stops it.
            let _heartbeat = session.spawn_heartbeat(DEFAULT_HEARTBEAT);
        });
    }

    let (updates, _) = broadcast::channel(256);
    let shared = Arc::new(Shared {
        session,
        players: Mutex::new(Vec::new()),
        updates,
        match_started: AtomicBool::new(false),
    });

    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|error| {
            eprintln!("bind 0.0.0.0:{port}: {error}");
            std::process::exit(1);
        });
    println!("arena dedicated server listening on 0.0.0.0:{port}");
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                tokio::spawn(handle_connection(stream, Arc::clone(&shared)));
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
}

/// The state a connection needs: the handoff session (when configured), the
/// players in arrival order, and the match's broadcast channel.
struct Shared {
    session: Option<Session>,
    players: Mutex<Vec<String>>,
    updates: broadcast::Sender<String>,
    match_started: AtomicBool,
}

impl Shared {
    /// Report a lifecycle event, if this process was started with a session.
    /// `hearth-handoff-server` is blocking, so the call moves off the reactor.
    fn report(&self, report: ServerReport) {
        let Some(session) = self.session.clone() else {
            return;
        };
        tokio::task::spawn_blocking(move || {
            if let Err(error) = session.report(&report) {
                eprintln!("report failed: {error}");
            }
        });
    }
}

/// One TCP connection: a container health check, a forwarded WebSocket upgrade,
/// or something we do not serve.
async fn handle_connection(mut stream: TcpStream, shared: Arc<Shared>) {
    let Some(request) = peek_request(&mut stream).await else {
        return;
    };
    if request.starts_with("GET /healthz") {
        let _ = stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
            )
            .await;
        return;
    }
    if !request
        .to_ascii_lowercase()
        .contains("\r\nupgrade: websocket")
    {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
        return;
    }
    let Some(identity) = identity_from_request(&request) else {
        // Only the gateway reaches this port, and it always forwards the
        // identity it just redeemed.
        let _ = stream
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await;
        return;
    };
    match tokio_tungstenite::accept_async(stream).await {
        Ok(websocket) => serve_player(websocket, identity, shared).await,
        Err(error) => eprintln!("websocket handshake failed: {error}"),
    }
}

/// Peek the request head without consuming it: the WebSocket handshake still has
/// to read the same bytes.
async fn peek_request(stream: &mut TcpStream) -> Option<String> {
    let mut chunk = [0u8; 8192];
    for _ in 0..200 {
        match stream.peek(&mut chunk).await {
            Ok(0) => return None,
            Ok(read) => {
                let head = &chunk[..read];
                if head.windows(4).any(|window| window == b"\r\n\r\n") {
                    return Some(String::from_utf8_lossy(head).into_owned());
                }
            }
            Err(error) => {
                eprintln!("peek failed: {error}");
                return None;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// The identity the gateway redeemed for this connection.
#[derive(Clone, Debug)]
struct Identity {
    player_id: String,
    provider: String,
    display_name: Option<String>,
}

fn identity_from_request(request: &str) -> Option<Identity> {
    let mut player_id = None;
    let mut provider = "anonymous".to_owned();
    let mut display_name = None;
    for line in request.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = percent_decode(value.trim());
        match name.trim().to_ascii_lowercase().as_str() {
            "x-hearth-player-id" => player_id = Some(value),
            "x-hearth-provider" => provider = value,
            "x-hearth-display-name" => display_name = Some(value),
            _ => {}
        }
    }
    player_id.map(|player_id| Identity {
        player_id,
        provider,
        display_name,
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
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

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Greet the player (the e2e harness checks the server echoes the redeemed
/// identity), then forward match broadcasts until the socket or the match ends.
async fn serve_player(
    websocket: WebSocketStream<TcpStream>,
    identity: Identity,
    shared: Arc<Shared>,
) {
    let (mut sink, mut stream) = websocket.split();
    let hello = json!({
        "type": "hello",
        "player_id": identity.player_id,
        "provider": identity.provider,
        "display_name": identity.display_name,
    })
    .to_string();
    if sink.send(Message::text(hello)).await.is_err() {
        return;
    }
    register_player(&shared, &identity.player_id).await;
    maybe_start_match(&shared).await;

    let mut updates = shared.updates.subscribe();
    loop {
        tokio::select! {
            message = stream.next() => match message {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            update = updates.recv() => match update {
                Ok(text) => {
                    if sink.send(Message::text(text)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

/// Record the player once; a reconnecting socket does not report a second join.
async fn register_player(shared: &Arc<Shared>, player_id: &str) {
    let first = {
        let mut players = shared.players.lock().await;
        if players.iter().any(|player| player == player_id) {
            false
        } else {
            players.push(player_id.to_owned());
            true
        }
    };
    if first {
        shared.report(ServerReport::PlayerJoined {
            player: PlayerId(player_id.to_owned()),
        });
    }
}

/// Start the match once, when it has enough players. The first player in is the
/// winner of the sample's deterministic "match".
async fn maybe_start_match(shared: &Arc<Shared>) {
    let winner = {
        let players = shared.players.lock().await;
        if players.len() < PLAYERS_FOR_MATCH {
            return;
        }
        players[0].clone()
    };
    if shared.match_started.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(run_match(Arc::clone(shared), winner));
}

/// Broadcast the server time at 30 Hz for five seconds, then report the result
/// and the end of the session, and exit.
async fn run_match(shared: Arc<Shared>, winner: String) {
    let started = Instant::now();
    let ticks = (MATCH_DURATION.as_secs_f64() * f64::from(TICK_RATE_HZ)) as u32;
    let period = MATCH_DURATION / ticks;
    for _ in 0..ticks {
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let _ = shared
            .updates
            .send(json!({ "type": "tick", "t": elapsed_ms }).to_string());
        tokio::time::sleep(period).await;
    }

    if let Some(session) = shared.session.clone() {
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(error) = session.report(&ServerReport::Result {
                data: json!({ "winner": winner }),
            }) {
                eprintln!("result report failed: {error}");
            }
            if let Err(error) = session.report(&ServerReport::Ended {
                reason: "match over".to_owned(),
            }) {
                eprintln!("ended report failed: {error}");
            }
        })
        .await;
    }
    // Let the forwarded sockets flush the last tick before the process exits.
    tokio::time::sleep(Duration::from_millis(250)).await;
    std::process::exit(0);
}
