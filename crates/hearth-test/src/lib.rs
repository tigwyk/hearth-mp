//! Hearth test harness.
//!
//! [`TestRoom`] wires a [`Room`] to a [`MemHost`] and to real [`ClientCore`]
//! instances, so every server frame a test triggers is decoded by the client
//! core the same way a browser or Godot client would decode it.
//!
//! All methods are synchronous: each runtime call is driven to completion with
//! [`futures::executor::block_on`], because the runtime is sans-IO and the
//! [`MemHost`] futures are always immediately ready.

pub mod mem_host;
pub mod world;

use std::time::Duration;

use futures::executor::block_on;
use hearth_client::{ClientConfig, ClientCore, ClientEvent};
use hearth_protocol::{Encoding, PatchOp, PlayerId, Value, WireMsg};
use hearth_server::{schema_hash_hex, Host, InspectDoc, Room, RoomName, RoomRuntime, Sql};
use mem_host::MemEvent;
pub use mem_host::{MemHost, Socket};

/// Handle to one client in a [`TestRoom`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct C(pub usize);

/// One test client: its sans-IO core, the events decoded so far, and the close
/// code the server sent (if any).
pub struct TestClient {
    pub core: ClientCore,
    pub events: Vec<ClientEvent>,
    pub closed: Option<(u16, String)>,
}

/// One client seat as an admin tail socket sees itself: no client core.
pub(crate) struct AdminTail {
    socket: Socket,
    messages: Vec<Value>,
    closed: Option<(u16, String)>,
}

/// A room plus its clients and environment.
pub struct TestRoom<R: Room> {
    host: MemHost,
    runtime: Option<RoomRuntime<R, MemHost>>,
    clients: Vec<TestClient>,
    sockets: Vec<Socket>,
    /// The player and encoding behind each client, so `resume` can reconnect.
    players: Vec<(PlayerId, Encoding)>,
    /// Admin tail sockets, addressed by the `C` values `tail` returns.
    tails: Vec<AdminTail>,
}

impl<R: Room> TestRoom<R> {
    pub fn new() -> Self {
        Self {
            host: MemHost::new(),
            runtime: None,
            clients: Vec::new(),
            sockets: Vec::new(),
            players: Vec::new(),
            tails: Vec::new(),
        }
    }

    /// This harness room's identity, reported to the registry and stored in
    /// `hearth_meta` on the first connect.
    pub fn room_name(&self) -> RoomName {
        RoomName {
            room_type: "counter".to_owned(),
            name: "r1".to_owned(),
        }
    }

    /// Join a client with a fresh `HELLO`, MessagePack encoded.
    pub fn join(&mut self, player: &str, opts: Value) -> C {
        self.join_with(player, Encoding::MsgPack, opts)
    }

    /// Join a client with a fresh `HELLO` in `encoding`.
    pub fn join_with(&mut self, player: &str, encoding: Encoding, opts: Value) -> C {
        let player = PlayerId(player.to_owned());
        let room = self.room_name();
        let (index, socket) = self.spawn_client(player.clone(), encoding);
        block_on(self.runtime().connect(socket, player, room));
        let hello = self.clients[index].core.hello(opts);
        block_on(self.runtime().message(socket, hello));
        self.pump();
        C(index)
    }

    /// Register a socket and a client core without sending `HELLO`.
    pub fn connect_raw(&mut self, player: &str) -> C {
        let player = PlayerId(player.to_owned());
        let room = self.room_name();
        let (index, socket) = self.spawn_client(player.clone(), Encoding::MsgPack);
        block_on(self.runtime().connect(socket, player, room));
        self.pump();
        C(index)
    }

    /// Queue one intent, send it immediately, and return its sequence number.
    pub fn intent(&mut self, c: C, intent: impl serde::Serialize) -> u32 {
        let value = serde_json::to_value(intent).expect("intent serialises as JSON");
        let seq = self.clients[c.0].core.queue_intent(value);
        self.flush(c);
        seq
    }

    /// Queue several intents and send them (batched per the client core).
    pub fn intents(&mut self, c: C, intents: Vec<Value>) {
        for intent in intents {
            self.clients[c.0].core.queue_intent(intent);
        }
        self.flush(c);
    }

    /// Send one already-encoded frame.
    pub fn send_raw(&mut self, c: C, msg: WireMsg) {
        let socket = self.sockets[c.0];
        block_on(self.runtime().message(socket, msg));
        self.pump();
    }

    /// Simulate the socket disappearing.
    pub fn disconnect(&mut self, c: C) {
        let socket = self.sockets[c.0];
        block_on(self.runtime().close(socket));
        // The transport close arrives after the runtime handled the event.
        self.host.close(&socket, 1000, "client closed");
        self.pump();
    }

    /// Simulate an abnormal socket drop: the server sees close code 1006.
    pub fn drop_socket(&mut self, c: C) {
        let socket = self.sockets[c.0];
        block_on(self.runtime().close(socket));
        self.host.close(&socket, 1006, "abnormal closure");
        self.pump();
    }

    /// Reconnect `c`'s player on a new socket with a fresh `HELLO`.
    pub fn resume(&mut self, c: C) -> C {
        let (player, encoding) = self.players[c.0].clone();
        let room = self.room_name();
        let (index, socket) = self.spawn_client(player.clone(), encoding);
        block_on(self.runtime().connect(socket, player, room));
        let hello = self.clients[index].core.hello(serde_json::json!({}));
        block_on(self.runtime().message(socket, hello));
        self.pump();
        C(index)
    }

    /// Move the virtual clock and fire the alarm until nothing else is due.
    pub fn advance(&mut self, delta: Duration) {
        let target = self
            .host
            .now_ms()
            .saturating_add(u64::try_from(delta.as_millis()).unwrap_or(u64::MAX));
        self.host.set_clock(target);
        let mut fired = 0;
        loop {
            let Some(at) = self.host.alarm_at() else {
                break;
            };
            if at > target {
                break;
            }
            block_on(self.runtime().alarm());
            self.pump();
            fired += 1;
            assert!(
                fired < 10_000,
                "the runtime kept rescheduling a due alarm; is a timer handler looping?"
            );
        }
    }

    /// Drop the runtime (as hibernation does) while keeping storage and sockets.
    pub fn hibernate(&mut self) {
        self.runtime = None;
    }

    pub fn client(&self, c: C) -> &TestClient {
        &self.clients[c.0]
    }

    /// The persisted full state (including server-only fields) as JSON.
    ///
    /// `alarm()` lazily loads persisted state after a hibernation; `advance`
    /// has already fired every due alarm, so nothing is fired here.
    pub fn server_state(&mut self) -> Value {
        let runtime = self.runtime();
        block_on(runtime.alarm());
        match runtime.state() {
            Some(state) => serde_json::to_value(state).expect("state serialises as JSON"),
            None => Value::Null,
        }
    }

    pub fn sql(&self) -> &dyn Sql {
        self.host.sql()
    }

    /// Every JSON log line the room emitted, in order.
    pub fn logs(&self) -> Vec<String> {
        self.host.logs()
    }

    /// How many unfiltered full-state renders the admin tail asked for.
    pub fn full_state_count(&self) -> usize {
        self.host.full_state_count()
    }

    /// The read-only admin view, or `None` when the room never existed.
    pub fn inspect(&mut self) -> Option<InspectDoc> {
        self.runtime()
            .inspect()
            .expect("inspect reads the room's SQLite")
    }

    /// Force-dispose the room; `false` when it never existed.
    pub fn force_dispose(&mut self) -> bool {
        let disposed = block_on(self.runtime().force_dispose()).expect("force-dispose succeeds");
        self.pump();
        disposed
    }

    /// Open an admin tail socket. Its `C` indexes the tail list, not the clients.
    pub fn tail(&mut self) -> C {
        let socket = self.host.open_socket();
        self.runtime().connect_admin(socket);
        self.pump();
        let index = self.tails.len();
        self.tails.push(AdminTail {
            socket,
            messages: Vec::new(),
            closed: None,
        });
        C(index)
    }

    /// The decoded JSON text frames an admin tail received so far.
    pub fn tail_msgs(&self, c: C) -> Vec<Value> {
        self.tails[c.0].messages.clone()
    }

    /// The close code the server sent an admin tail, if it was closed.
    pub fn tail_closed(&self, c: C) -> Option<(u16, String)> {
        self.tails[c.0].closed.clone()
    }

    fn spawn_client(&mut self, player: PlayerId, encoding: Encoding) -> (usize, Socket) {
        let socket = self.host.open_socket();
        let mut config = ClientConfig::new(schema_hash_hex::<R>(), encoding);
        // The harness drives `poll_send` by hand at one virtual instant, so the
        // production 20 msgs/s pacing would stall every send. Effectively
        // unlimit the client; rate limiting is a server-side concern.
        config.max_msgs_per_sec = u32::MAX;
        let index = self.clients.len();
        self.clients.push(TestClient {
            core: ClientCore::new(config),
            events: Vec::new(),
            closed: None,
        });
        self.sockets.push(socket);
        self.players.push((player, encoding));
        (index, socket)
    }

    fn runtime(&mut self) -> &mut RoomRuntime<R, MemHost> {
        if self.runtime.is_none() {
            self.runtime = Some(RoomRuntime::new(self.host.clone(), "counter"));
        }
        self.runtime.as_mut().expect("runtime is set")
    }

    /// Send everything the client core has queued, then decode server replies.
    fn flush(&mut self, c: C) {
        let now = self.host.now_ms();
        loop {
            let Some(msg) = self.clients[c.0].core.poll_send(now) else {
                break;
            };
            let socket = self.sockets[c.0];
            block_on(self.runtime().message(socket, msg));
        }
        self.pump();
    }

    /// Feed every frame and close the runtime produced into the matching client
    /// or admin tail.
    fn pump(&mut self) {
        pump_into(
            self.host.take_journal(),
            &mut self.clients,
            &self.sockets,
            &mut self.tails,
        );
    }
}

/// Route one host journal into the matching client cores and admin tails.
/// Shared by [`TestRoom`] and [`world::TestWorld`].
pub(crate) fn pump_into(
    events: Vec<MemEvent>,
    clients: &mut [TestClient],
    sockets: &[Socket],
    tails: &mut [AdminTail],
) {
    for event in events {
        match event {
            MemEvent::Sent { socket, msg } => {
                if let Some(index) = sockets.iter().position(|candidate| *candidate == socket) {
                    match clients[index].core.handle(&msg) {
                        Ok(events) => clients[index].events.extend(events),
                        Err(err) => {
                            panic!("client {index} could not decode a server frame: {err:?}")
                        }
                    }
                    continue;
                }
                let Some(index) = tails.iter().position(|tail| tail.socket == socket) else {
                    continue;
                };
                match msg {
                    WireMsg::Text(text) => match serde_json::from_str::<Value>(&text) {
                        Ok(value) => tails[index].messages.push(value),
                        Err(err) => panic!("admin tail {index} got non-JSON text: {err}"),
                    },
                    WireMsg::Binary(_) => panic!("admin tail {index} got a binary frame"),
                }
            }
            MemEvent::Closed {
                socket,
                code,
                reason,
            } => {
                if let Some(index) = sockets.iter().position(|candidate| *candidate == socket) {
                    clients[index].closed = Some((code, reason));
                    continue;
                }
                if let Some(index) = tails.iter().position(|tail| tail.socket == socket) {
                    tails[index].closed = Some((code, reason));
                }
            }
        }
    }
}

impl<R: Room> Default for TestRoom<R> {
    fn default() -> Self {
        Self::new()
    }
}

/// One script step for [`fuzz_hibernation`].
pub enum Step {
    Join(&'static str),
    Intent(&'static str, Value),
    Leave(&'static str),
    /// The socket drops with close code 1006.
    Drop(&'static str),
    /// A new socket for the same player sends a fresh `HELLO`.
    Resume(&'static str),
    Advance(Duration),
}

/// Run `script` twice - once straight, once with seeded random hibernations -
/// and compare every client's final state and ordered `Patched` op lists.
///
/// Returns `Err` on the first difference, naming the seed's client.
pub fn fuzz_hibernation<R: Room>(seed: u64, script: &[Step]) -> Result<(), String> {
    let plain = run_script::<R>(seed, script, false);
    let hibernated = run_script::<R>(seed, script, true);
    if plain.len() != hibernated.len() {
        return Err(format!(
            "seed {seed}: client count differs ({} vs {})",
            plain.len(),
            hibernated.len()
        ));
    }
    for (straight, hibernated) in plain.iter().zip(&hibernated) {
        if straight.state != hibernated.state {
            return Err(format!(
                "seed {seed}: client {} final state differs: {} vs {}",
                straight.name, straight.state, hibernated.state
            ));
        }
        if straight.patched != hibernated.patched {
            return Err(format!(
                "seed {seed}: client {} patched op lists differ: {:?} vs {:?}",
                straight.name, straight.patched, hibernated.patched
            ));
        }
    }
    Ok(())
}

struct RunClient {
    name: String,
    state: Value,
    patched: Vec<Vec<PatchOp>>,
}

fn run_script<R: Room>(seed: u64, script: &[Step], hibernate: bool) -> Vec<RunClient> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut room = TestRoom::<R>::new();
    let mut clients: Vec<(&'static str, C)> = Vec::new();
    for step in script {
        if hibernate && rng.u8(0..10) < 3 {
            room.hibernate();
        }
        match step {
            Step::Join(name) => clients.push((name, room.join(name, serde_json::json!({})))),
            Step::Intent(name, intent) => {
                if let Some((_, c)) = clients.iter().find(|(who, _)| who == name) {
                    room.intent(*c, intent.clone());
                }
            }
            Step::Leave(name) => {
                if let Some((_, c)) = clients.iter().find(|(who, _)| who == name) {
                    room.disconnect(*c);
                }
            }
            Step::Drop(name) => {
                if let Some((_, c)) = clients.iter().find(|(who, _)| who == name) {
                    room.drop_socket(*c);
                }
            }
            Step::Resume(name) => {
                if let Some(entry) = clients.iter_mut().find(|(who, _)| who == name) {
                    entry.1 = room.resume(entry.1);
                }
            }
            Step::Advance(delta) => room.advance(*delta),
        }
    }
    clients
        .iter()
        .map(|(name, c)| {
            let client = room.client(*c);
            RunClient {
                name: (*name).to_owned(),
                state: client.core.state().clone(),
                patched: client
                    .events
                    .iter()
                    .filter_map(|event| match event {
                        ClientEvent::Patched { ops } => Some(ops.clone()),
                        _ => None,
                    })
                    .collect(),
            }
        })
        .collect()
}
