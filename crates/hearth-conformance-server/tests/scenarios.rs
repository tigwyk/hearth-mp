//! The Rust runner for `conformance/scenarios/*.json`: the reference server
//! in-process on a free port, and `ClientCore` over `tokio-tungstenite`.
//!
//! Every runner (this one, the GDScript `scenario_runner.gd`, the Unity
//! `ScenarioTests`) turns what its SDK reports into the same *observed frames*:
//! `JOIN_OK {player_id, seat}`, `FULL_STATE {state}`, `PATCH {ops, state}` (the
//! state after the ops), `ACK {intent_seqs}`, `EVENT {name, data}`,
//! `ERROR {code, message, ref_seq}` and `CLOSE {code}`. An `expect` step scans
//! that client's observed frames forward for the first one matching the pattern:
//! every given field must be equal, `"*"` matches anything, `"$absent"` means
//! the key must not exist, and arrays match element-wise with equal length.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use hearth_client::{ClientConfig, ClientCore, ClientEvent};
use hearth_protocol::{Encoding, WireMsg};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const EXPECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Start the server on its own thread; returns `ws://127.0.0.1:<port>`.
fn start_server() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            hearth_conformance_server::serve(listener).await.unwrap();
        });
    });
    format!("ws://127.0.0.1:{}", rx.recv().unwrap())
}

/// `pattern` matches `value` (see the module docs).
fn matches(pattern: &Value, value: &Value) -> bool {
    match (pattern, value) {
        (Value::String(s), _) if s == "*" => true,
        (Value::Object(p), Value::Object(v)) => p.iter().all(|(k, pv)| match v.get(k) {
            None => pv == "$absent",
            Some(vv) => pv != "$absent" && matches(pv, vv),
        }),
        (Value::Array(p), Value::Array(v)) => p.len() == v.len() && p.iter().zip(v).all(|(a, b)| matches(a, b)),
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => pattern == value,
    }
}

struct Client {
    player: String,
    core: ClientCore,
    ws: Option<Ws>,
    observed: Vec<Value>,
    cursor: usize,
    start: Instant,
    opts: Value,
}

impl Client {
    fn new(player: &str) -> Self {
        Self {
            player: player.to_owned(),
            core: ClientCore::new(ClientConfig::new(hearth_conformance_server::schema_hash(), Encoding::Json)),
            ws: None,
            observed: Vec::new(),
            cursor: 0,
            start: Instant::now(),
            opts: json!({}),
        }
    }

    async fn open(&mut self, base: &str, room: &str) {
        let url = format!("{base}/rooms/conf/{room}?player={}", self.player);
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("connect");
        let WireMsg::Text(hello) = self.core.hello(self.opts.clone()) else { unreachable!() };
        ws.send(Message::Text(hello.into())).await.unwrap();
        self.ws = Some(ws);
    }

    fn observe(&mut self, events: Vec<ClientEvent>) {
        for event in events {
            let frame = match event {
                ClientEvent::Joined { player_id, seat } => json!({"op": "JOIN_OK", "player_id": player_id, "seat": seat}),
                ClientEvent::StateReset => json!({"op": "FULL_STATE", "state": self.core.state()}),
                ClientEvent::Patched { ops } => json!({"op": "PATCH", "ops": ops, "state": self.core.state()}),
                ClientEvent::Acked { intent_seqs, .. } => json!({"op": "ACK", "intent_seqs": intent_seqs}),
                ClientEvent::Event { name, data } => json!({"op": "EVENT", "name": name, "data": data}),
                ClientEvent::Error { code, message, ref_seq } => {
                    json!({"op": "ERROR", "code": code, "message": message, "ref_seq": ref_seq})
                }
                _ => continue,
            };
            self.observed.push(frame);
        }
    }

    /// Read what arrives within `budget`, and send what may be sent.
    async fn pump(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            self.flush().await;
            let Some(ws) = self.ws.as_mut() else { return };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match tokio::time::timeout(left, ws.next()).await {
                Err(_) => return,
                Ok(Some(Ok(Message::Text(text)))) => {
                    let events = self.core.handle(&WireMsg::Text(text.to_string())).expect("a server frame decodes");
                    self.observe(events);
                }
                Ok(Some(Ok(Message::Binary(bytes)))) => {
                    let events = self.core.handle(&WireMsg::Binary(bytes.to_vec())).expect("a server frame decodes");
                    self.observe(events);
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    let code = frame.map_or(1005, |f| u16::from(f.code));
                    self.observed.push(json!({"op": "CLOSE", "code": code}));
                    self.ws = None;
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_))) | Ok(None) => {
                    self.observed.push(json!({"op": "CLOSE", "code": 1006}));
                    self.ws = None;
                }
            }
        }
    }

    async fn flush(&mut self) {
        let now = self.start.elapsed().as_millis() as u64;
        while let Some(WireMsg::Text(frame)) = self.core.poll_send(now) {
            if let Some(ws) = self.ws.as_mut() {
                let _ = ws.send(Message::Text(frame.into())).await;
            }
        }
    }

    async fn expect(&mut self, pattern: &Value, scenario: &str) {
        let deadline = Instant::now() + EXPECT_TIMEOUT;
        loop {
            if let Some(i) = self.observed[self.cursor..].iter().position(|f| matches(pattern, f)) {
                self.cursor += i + 1;
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "{scenario}: client {} never observed {pattern}\nobserved after cursor: {:#?}",
                    self.player,
                    &self.observed[self.cursor..]
                );
            }
            self.pump(Duration::from_millis(50)).await;
        }
    }
}

async fn run_scenario(base: &str, path: &Path) {
    let scenario: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let name = scenario["name"].as_str().unwrap().to_owned();
    // A fresh room per run, so scenarios never share state; player ids are the
    // scenario's client names.
    let room = format!("{name}-{}", std::process::id());
    let mut clients: HashMap<String, Client> = scenario["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let player = c.as_str().unwrap();
            (player.to_owned(), Client::new(player))
        })
        .collect();
    let steps = scenario["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "{name}: no steps");
    for step in steps {
        if let Some(ms) = step.get("wait_ms").and_then(Value::as_u64) {
            for client in clients.values_mut() {
                client.pump(Duration::from_millis(ms)).await;
            }
            continue;
        }
        let who = step["client"].as_str().unwrap();
        let client = clients.get_mut(who).unwrap_or_else(|| panic!("{name}: unknown client {who}"));
        if let Some(pattern) = step.get("expect") {
            client.expect(pattern, &name).await;
            continue;
        }
        let send = &step["send"];
        match (send.as_str(), send["op"].as_str()) {
            (Some("connect"), _) => client.open(base, &room).await,
            (Some("drop"), _) => {
                // An abrupt drop, then the reconnect an SDK does by itself.
                client.ws = None;
                client.core.on_disconnected();
                tokio::time::sleep(Duration::from_millis(100)).await;
                client.open(base, &room).await;
            }
            (_, Some("HELLO")) => {
                let hash = send["schema_hash"].as_str().unwrap();
                client.core = ClientCore::new(ClientConfig::new(hash, Encoding::Json));
                client.opts = send["opts"].clone();
                client.open(base, &room).await;
            }
            (_, Some("INTENT")) => {
                for intent in send["intents"].as_array().unwrap() {
                    client.core.queue_intent(intent["intent"].clone());
                }
                client.flush().await;
            }
            (_, Some("LEAVE")) => {
                let WireMsg::Text(leave) = client.core.leave() else { unreachable!() };
                if let Some(ws) = client.ws.as_mut() {
                    ws.send(Message::Text(leave.into())).await.unwrap();
                }
            }
            _ => panic!("{name}: unknown step {step}"),
        }
    }
}

#[tokio::test]
async fn every_scenario_passes() {
    let base = start_server();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/scenarios");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "{} has no scenarios", dir.display());
    for path in &paths {
        run_scenario(&base, path).await;
        eprintln!("ok - {}", path.file_name().unwrap().to_string_lossy());
    }
}

#[tokio::test]
async fn schema_route_answers() {
    let base = start_server().replace("ws://", "");
    let mut stream = TcpStream::connect(&base).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream.write_all(b"GET /schema/conf HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).await.unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains(&hearth_conformance_server::schema_hash()), "{text}");
}

#[tokio::test]
async fn anonymous_login_returns_the_device_id_as_token() {
    let base = start_server().replace("ws://", "");
    let mut stream = TcpStream::connect(&base).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = r#"{"device_id":"alice"}"#;
    let request = format!(
        "POST /auth/anonymous HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).await.unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains(r#""token":"alice""#), "{text}");
}
