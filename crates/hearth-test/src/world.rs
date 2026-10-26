//! A many-object test world: rooms and services with separate storage, a shared
//! virtual clock, and synchronous object-to-object call routing.
//!
//! [`TestRoom`](crate::TestRoom) is the one-room harness for plain rooms. A test
//! that needs several objects, calls between them, or a partition uses
//! [`TestWorld`]: every object still runs the real runtime against an in-memory
//! `Host`, so the code under test is the code production runs.

use std::any::Any;
use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use futures::executor::block_on;
use hearth_client::{ClientConfig, ClientCore};
use hearth_protocol::{Encoding, PlayerId, Value, WireMsg};
use hearth_server::call::{CallError, Callable, Envelope, Target};
use hearth_server::{
    schema_hash_hex, Attachment, Host, InspectDoc, Result, Room, RoomName, RoomRpc, RoomRuntime,
    Service, ServiceRuntime, Session, Sql, SqlValue,
};

use crate::mem_host::MemHost;
use crate::{pump_into, AdminTail, TestClient, C};

/// A boxed future a test object's runtime hands back to the world.
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// One object's address inside the world.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    binding: String,
    name: String,
}

impl Key {
    fn label(&self) -> String {
        format!("{}/{}", self.binding, self.name)
    }
}

/// A room in a [`TestWorld`].
#[derive(Clone, Debug)]
pub struct RoomRef {
    key: Key,
}

/// A service in a [`TestWorld`].
#[derive(Clone, Debug)]
pub struct ServiceRef {
    key: Key,
}

/// A cloneable handle to one object's SQLite database.
#[derive(Clone)]
pub struct SqlHandle(MemHost);

impl SqlHandle {
    /// The object's SQLite.
    pub fn sql(&self) -> &dyn Sql {
        self.0.sql()
    }
}

/// The call entry point for a room that implements [`RoomRpc`].
type RoomCall = for<'a> fn(
    &'a mut dyn Any,
    Envelope,
    String,
) -> LocalBoxFuture<'a, std::result::Result<Value, CallError>>;

fn room_call<R: RoomRpc>(
    any: &mut dyn Any,
    envelope: Envelope,
    name: String,
) -> LocalBoxFuture<'_, std::result::Result<Value, CallError>> {
    Box::pin(async move {
        let slot = any
            .downcast_mut::<RoomSlot<R>>()
            .expect("the call pointer matches the slot's room type");
        slot.rt.call(envelope, &name).await
    })
}

/// Everything the world needs from one object. Rooms override the socket
/// methods; services leave them as no-ops.
trait AnyObject {
    fn connect<'a>(
        &'a mut self,
        socket: usize,
        session: Session,
        room: RoomName,
    ) -> LocalBoxFuture<'a, ()>;
    fn connect_admin(&mut self, socket: usize) -> LocalBoxFuture<'_, ()>;
    fn message(&mut self, socket: usize, msg: WireMsg) -> LocalBoxFuture<'_, ()>;
    /// Point this room at a matchmaker instance, as the gateway's header does.
    fn set_matchmaker(&mut self, mm: &str);
    fn close(&mut self, socket: usize) -> LocalBoxFuture<'_, ()>;
    fn alarm(&mut self) -> LocalBoxFuture<'_, ()>;
    fn force_dispose(&mut self) -> LocalBoxFuture<'_, bool>;
    /// Services and rpc-capable rooms implement this; a plain room answers
    /// `BadRpc` instead.
    fn call(
        &mut self,
        envelope: Envelope,
    ) -> LocalBoxFuture<'_, std::result::Result<Value, CallError>>;
    /// The read-only admin view: an [`InspectDoc`] for a room, the state for a
    /// service.
    fn inspect(&mut self) -> Option<Value>;
    fn hibernate(&mut self);
    fn alarm_at(&self) -> Option<u64>;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// One room's runtime inside the world.
struct RoomSlot<R: Room> {
    rt: RoomRuntime<R, WorldHost>,
}

impl<R: Room> AnyObject for RoomSlot<R> {
    fn connect<'a>(
        &'a mut self,
        socket: usize,
        session: Session,
        room: RoomName,
    ) -> LocalBoxFuture<'a, ()> {
        Box::pin(async move { self.rt.connect(socket, session, room, None).await })
    }

    fn connect_admin(&mut self, socket: usize) -> LocalBoxFuture<'_, ()> {
        Box::pin(async move { self.rt.connect_admin(socket) })
    }

    fn message(&mut self, socket: usize, msg: WireMsg) -> LocalBoxFuture<'_, ()> {
        Box::pin(async move { self.rt.message(socket, msg).await })
    }

    fn set_matchmaker(&mut self, mm: &str) {
        let _ = self.rt.set_matchmaker(mm);
    }

    fn close(&mut self, socket: usize) -> LocalBoxFuture<'_, ()> {
        Box::pin(async move { self.rt.close(socket).await })
    }

    fn alarm(&mut self) -> LocalBoxFuture<'_, ()> {
        Box::pin(async move { self.rt.alarm().await })
    }

    fn force_dispose(&mut self) -> LocalBoxFuture<'_, bool> {
        Box::pin(async move { self.rt.force_dispose().await.unwrap_or(false) })
    }

    fn call(
        &mut self,
        _envelope: Envelope,
    ) -> LocalBoxFuture<'_, std::result::Result<Value, CallError>> {
        // A room without `RoomRpc` gets this method; a room with it is reached
        // through the `RoomCall` pointer instead.
        Box::pin(async {
            Err(CallError::BadRpc(
                "room does not accept rpc (add `rpc` to #[hearth::room] and impl RoomRpc)"
                    .to_owned(),
            ))
        })
    }

    fn inspect(&mut self) -> Option<Value> {
        let doc = self.rt.inspect().ok()??;
        serde_json::to_value(doc).ok()
    }

    fn hibernate(&mut self) {
        self.rt.hibernate();
    }

    fn alarm_at(&self) -> Option<u64> {
        self.rt.host().alarm_at()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// One service's runtime inside the world.
struct ServiceSlot<S: Service> {
    rt: ServiceRuntime<S, WorldHost>,
    alarm: Rc<Cell<Option<u64>>>,
    label: String,
}

impl<S: Service> AnyObject for ServiceSlot<S> {
    fn connect<'a>(
        &'a mut self,
        _socket: usize,
        _session: Session,
        _room: RoomName,
    ) -> LocalBoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn connect_admin(&mut self, _socket: usize) -> LocalBoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn message(&mut self, _socket: usize, _msg: WireMsg) -> LocalBoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn set_matchmaker(&mut self, _mm: &str) {}

    fn close(&mut self, _socket: usize) -> LocalBoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn alarm(&mut self) -> LocalBoxFuture<'_, ()> {
        Box::pin(async move { self.rt.alarm().await })
    }

    fn force_dispose(&mut self) -> LocalBoxFuture<'_, bool> {
        Box::pin(async { false })
    }

    fn call(
        &mut self,
        envelope: Envelope,
    ) -> LocalBoxFuture<'_, std::result::Result<Value, CallError>> {
        Box::pin(async move {
            // The world's slot label is the service's own name, which is what
            // the receiver would learn from the call header in production.
            let label = self.label.clone();
            self.rt.call(envelope, &label).await
        })
    }

    fn inspect(&mut self) -> Option<Value> {
        block_on(self.rt.inspect()).ok().flatten()
    }

    fn hibernate(&mut self) {
        self.rt.hibernate();
    }

    fn alarm_at(&self) -> Option<u64> {
        self.alarm.get()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// One object's slot in the world's table.
struct Slot {
    obj: Box<dyn AnyObject>,
    /// Present only for rooms that accept rpc.
    call: Option<RoomCall>,
    /// The room's schema-hash function, so a test client can send `HELLO`.
    schema: Option<fn() -> String>,
    host: WorldHost,
}

struct Inner {
    clock: Cell<u64>,
    next_socket: Cell<usize>,
    sockets: RefCell<Vec<usize>>,
    attachments: RefCell<HashMap<usize, Attachment>>,
    journal: RefCell<Vec<crate::mem_host::MemEvent>>,
    logs: RefCell<Vec<String>>,
    full_state_count: Cell<usize>,
    objects: RefCell<HashMap<Key, Slot>>,
    /// Every binding the deployment declares, even before its first object.
    bindings: RefCell<HashSet<String>>,
    partitions: RefCell<HashSet<String>>,
    clients: RefCell<Vec<TestClient>>,
    client_sockets: RefCell<Vec<usize>>,
    client_players: RefCell<Vec<(Session, Encoding)>>,
    client_rooms: RefCell<Vec<Key>>,
    tails: RefCell<Vec<AdminTail>>,
    tail_rooms: RefCell<Vec<Key>>,
    /// The most recent dispatch, so a test can force a duplicate delivery.
    last_delivery: RefCell<Option<(Target, Envelope)>>,
}

/// A set of Hearth objects that route calls to each other in one process.
pub struct TestWorld {
    inner: Rc<Inner>,
}

impl TestWorld {
    /// An empty world at clock 0.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(Inner {
                clock: Cell::new(0),
                next_socket: Cell::new(0),
                sockets: RefCell::new(Vec::new()),
                attachments: RefCell::new(HashMap::new()),
                journal: RefCell::new(Vec::new()),
                logs: RefCell::new(Vec::new()),
                full_state_count: Cell::new(0),
                objects: RefCell::new(HashMap::new()),
                bindings: RefCell::new(HashSet::new()),
                partitions: RefCell::new(HashSet::new()),
                clients: RefCell::new(Vec::new()),
                client_sockets: RefCell::new(Vec::new()),
                client_players: RefCell::new(Vec::new()),
                client_rooms: RefCell::new(Vec::new()),
                tails: RefCell::new(Vec::new()),
                tail_rooms: RefCell::new(Vec::new()),
                last_delivery: RefCell::new(None),
            }),
        }
    }

    /// Add a room that only speaks the socket protocol.
    pub fn add_room<R: Room>(&mut self, binding: &str, name: &str) -> RoomRef {
        self.add_room_inner::<R>(binding, name, None)
    }

    /// Add a room that also accepts object-to-object calls.
    pub fn add_rpc_room<R: RoomRpc>(&mut self, binding: &str, name: &str) -> RoomRef {
        self.add_room_inner::<R>(binding, name, Some(room_call::<R> as RoomCall))
    }

    fn add_room_inner<R: Room>(
        &mut self,
        binding: &str,
        name: &str,
        call: Option<RoomCall>,
    ) -> RoomRef {
        // Test-only: the runtime wants a `'static` binding, and a test world's
        // binding is fixed for the life of the test.
        let static_binding: &'static str = Box::leak(binding.to_owned().into_boxed_str());
        let key = Key {
            binding: binding.to_owned(),
            name: name.to_owned(),
        };
        let host = self.new_host(&key);
        let mut rt = RoomRuntime::<R, WorldHost>::new(host.clone(), static_binding);
        let room = RoomName {
            room_type: binding.to_owned(),
            name: name.to_owned(),
        };
        rt.remember_identity(&room)
            .expect("a test world room identity is writable");
        self.inner.bindings.borrow_mut().insert(binding.to_owned());
        self.inner.objects.borrow_mut().insert(
            key.clone(),
            Slot {
                obj: Box::new(RoomSlot::<R> { rt }),
                call,
                schema: Some(schema_hash_hex::<R>),
                host,
            },
        );
        RoomRef { key }
    }

    /// Add a service. Its binding comes from its [`Callable`] impl.
    pub fn add_service<S: Service + Callable>(&mut self, name: &str) -> ServiceRef {
        let key = Key {
            binding: S::BINDING.to_owned(),
            name: name.to_owned(),
        };
        let host = self.new_host(&key);
        let rt = ServiceRuntime::<S, WorldHost>::new(host.clone(), S::BINDING);
        self.inner
            .bindings
            .borrow_mut()
            .insert(S::BINDING.to_owned());
        self.inner.objects.borrow_mut().insert(
            key.clone(),
            Slot {
                obj: Box::new(ServiceSlot::<S> {
                    rt,
                    alarm: host.alarm.clone(),
                    label: name.to_owned(),
                }),
                call: None,
                schema: None,
                host,
            },
        );
        ServiceRef { key }
    }

    fn new_host(&self, key: &Key) -> WorldHost {
        WorldHost {
            inner: Rc::clone(&self.inner),
            db: MemHost::new(),
            target: Target::new(key.binding.clone(), key.name.clone()),
            alarm: Rc::new(Cell::new(None)),
        }
    }

    /// Take calls to `binding` out of service (`true`) or back in (`false`).
    pub fn partition(&mut self, binding: &str, down: bool) {
        if down {
            self.inner
                .partitions
                .borrow_mut()
                .insert(binding.to_owned());
        } else {
            self.inner.partitions.borrow_mut().remove(binding);
        }
    }

    /// Call an object from outside Hearth, as the gateway would.
    pub fn call<T: Callable>(
        &mut self,
        name: &str,
        rpc: T::Rpc,
    ) -> std::result::Result<T::Reply, CallError> {
        let to = Target::new(T::BINDING, name);
        let rpc = serde_json::to_value(rpc)
            .map_err(|err| CallError::BadRpc(format!("rpc did not encode: {err}")))?;
        let envelope = Envelope {
            msg_id: None,
            from: Target::new("test", ""),
            chain: Vec::new(),
            rpc,
        };
        let value = block_on(self.inner.dispatch(to, envelope))?;
        self.pump();
        serde_json::from_value(value)
            .map_err(|err| CallError::BadRpc(format!("reply did not decode: {err}")))
    }

    /// Call an object with a raw JSON payload, so a test can send something the
    /// receiver cannot decode.
    pub fn call_raw(
        &mut self,
        binding: &str,
        name: &str,
        rpc: Value,
    ) -> std::result::Result<Value, CallError> {
        let envelope = Envelope {
            msg_id: None,
            from: Target::new("test", ""),
            chain: Vec::new(),
            rpc,
        };
        let value = block_on(self.inner.dispatch(Target::new(binding, name), envelope));
        self.pump();
        value
    }

    /// Re-send the most recently delivered envelope, to prove inbox dedupe.
    pub fn redeliver_last(&mut self) -> std::result::Result<Value, CallError> {
        let (to, envelope) = self
            .inner
            .last_delivery
            .borrow()
            .clone()
            .expect("a call was delivered");
        block_on(self.inner.dispatch(to, envelope))
    }

    /// Join a player with a fresh `HELLO`, MessagePack encoded.
    pub fn join(&mut self, room: &RoomRef, player: &str, opts: Value) -> C {
        self.join_with(room, player, Encoding::MsgPack, opts)
    }

    /// Join `room` as `player` with this session identity and options.
    pub fn join_with(
        &mut self,
        room: &RoomRef,
        player: &str,
        encoding: Encoding,
        opts: Value,
    ) -> C {
        let c = self.connect_raw(room, player);
        self.hello(c, encoding, opts)
    }

    /// Join a player with the identity the gateway verified, MessagePack
    /// encoded and with no `HELLO` options.
    pub fn join_as(
        &mut self,
        room: &RoomRef,
        player: &str,
        provider: &str,
        display_name: Option<&str>,
    ) -> C {
        let session = Session {
            player: PlayerId(player.to_owned()),
            provider: provider.to_owned(),
            display_name: display_name.map(str::to_owned),
        };
        let c = self.connect_raw_as(room, session);
        self.hello(c, Encoding::MsgPack, Value::Null)
    }

    /// Register a socket and a client core without sending `HELLO`.
    pub fn connect_raw(&mut self, room: &RoomRef, player: &str) -> C {
        self.connect_raw_as(room, Session::anon(PlayerId(player.to_owned())))
    }

    fn connect_raw_as(&mut self, room: &RoomRef, session: Session) -> C {
        let socket = self.open_socket();
        let c = self.push_client(room, session.clone(), Encoding::MsgPack, socket);
        let room_name = RoomName {
            room_type: room.key.binding.clone(),
            name: room.key.name.clone(),
        };
        self.with_object(&room.key, |obj| {
            Box::pin(async move { obj.connect(socket, session, room_name).await })
        });
        self.pump();
        c
    }

    /// Send `HELLO` for a client that has not joined yet.
    pub fn hello(&mut self, c: C, encoding: Encoding, opts: Value) -> C {
        let key = self.inner.client_rooms.borrow()[c.0].clone();
        let mut config = ClientConfig::new(self.schema_hash(&key), encoding);
        config.max_msgs_per_sec = u32::MAX;
        self.inner.client_players.borrow_mut()[c.0].1 = encoding;
        self.inner.clients.borrow_mut()[c.0] = TestClient {
            core: ClientCore::new(config),
            events: Vec::new(),
            closed: None,
        };
        let socket = self.inner.client_sockets.borrow()[c.0];
        let hello = self.inner.clients.borrow_mut()[c.0].core.hello(opts);
        self.deliver(socket, hello);
        self.pump();
        c
    }

    /// Reconnect a dropped player on a new socket with a fresh `HELLO`.
    pub fn resume(&mut self, room: &RoomRef, c: C) -> C {
        let (session, encoding) = self.inner.client_players.borrow()[c.0].clone();
        let socket = self.open_socket();
        let room_name = RoomName {
            room_type: room.key.binding.clone(),
            name: room.key.name.clone(),
        };
        let reconnect = session.clone();
        self.with_object(&room.key, |obj| {
            Box::pin(async move { obj.connect(socket, reconnect, room_name).await })
        });
        let c = self.push_client(room, session, encoding, socket);
        let hello = self.inner.clients.borrow_mut()[c.0].core.hello(Value::Null);
        self.deliver(socket, hello);
        self.pump();
        c
    }

    /// Send one intent; returns its seq.
    pub fn intent(&mut self, c: C, intent: impl serde::Serialize) -> u32 {
        let value = serde_json::to_value(intent).expect("intent serialises as JSON");
        let seq = self.inner.clients.borrow_mut()[c.0]
            .core
            .queue_intent(value);
        self.flush(c);
        seq
    }

    /// Send several intents in one frame.
    pub fn intents(&mut self, c: C, intents: Vec<Value>) {
        for intent in intents {
            self.inner.clients.borrow_mut()[c.0]
                .core
                .queue_intent(intent);
        }
        self.flush(c);
    }

    /// Send one already-encoded frame.
    pub fn send_raw(&mut self, c: C, msg: WireMsg) {
        let socket = self.inner.client_sockets.borrow()[c.0];
        self.deliver(socket, msg);
        self.pump();
    }

    /// The client's socket closes.
    pub fn disconnect(&mut self, c: C) {
        let socket = self.inner.client_sockets.borrow()[c.0];
        self.with_room_of(socket, |obj| {
            Box::pin(async move { obj.close(socket).await })
        });
        self.inner.close_socket(socket, 1000, "client closed");
        self.pump();
    }

    /// Simulate an abnormal socket drop: the server sees close code 1006.
    pub fn drop_socket(&mut self, c: C) {
        let socket = self.inner.client_sockets.borrow()[c.0];
        self.with_room_of(socket, |obj| {
            Box::pin(async move { obj.close(socket).await })
        });
        self.inner.close_socket(socket, 1006, "abnormal closure");
        self.pump();
    }

    /// Point a test room at a matchmaker instance, as the gateway does.
    pub fn set_matchmaker(&mut self, room: &RoomRef, mm: &str) {
        self.with_object(&room.key, |obj| {
            obj.set_matchmaker(mm);
            Box::pin(async {})
        });
    }

    /// Move the virtual clock and fire every due alarm.
    pub fn advance(&mut self, delta: Duration) {
        let target = self
            .inner
            .clock
            .get()
            .saturating_add(u64::try_from(delta.as_millis()).unwrap_or(u64::MAX));
        self.inner.clock.set(target);
        let mut fired = 0;
        loop {
            let Some(key) = self.due_object(target) else {
                break;
            };
            self.with_object(&key, |obj| Box::pin(async move { obj.alarm().await }));
            self.pump();
            fired += 1;
            assert!(
                fired < 10_000,
                "the runtime kept rescheduling a due alarm; is a handler looping?"
            );
        }
    }

    /// Drop every in-memory runtime, as hibernation does.
    pub fn hibernate_all(&mut self) {
        let keys: Vec<Key> = self.inner.objects.borrow().keys().cloned().collect();
        for key in keys {
            self.with_object(&key, |obj| {
                obj.hibernate();
                Box::pin(async {})
            });
        }
    }

    /// One client's socket, core and events.
    pub fn client(&self, c: C) -> Ref<'_, TestClient> {
        Ref::map(self.inner.clients.borrow(), |clients| &clients[c.0])
    }

    /// Open an admin tail on a room.
    pub fn tail(&mut self, room: &RoomRef) -> C {
        let socket = self.open_socket();
        self.with_object(&room.key, |obj| {
            Box::pin(async move { obj.connect_admin(socket).await })
        });
        self.pump();
        let mut tails = self.inner.tails.borrow_mut();
        let index = tails.len();
        tails.push(AdminTail {
            socket,
            messages: Vec::new(),
            closed: None,
        });
        self.inner.tail_rooms.borrow_mut().push(room.key.clone());
        C(index)
    }

    /// Every frame an admin tail socket received, as JSON.
    pub fn tail_msgs(&self, c: C) -> Vec<Value> {
        self.inner.tails.borrow()[c.0].messages.clone()
    }

    /// The close code and reason of an admin tail socket, once closed.
    pub fn tail_closed(&self, c: C) -> Option<(u16, String)> {
        self.inner.tails.borrow()[c.0].closed.clone()
    }

    /// The room's persisted state (server-only fields included) as JSON.
    pub fn room_state(&mut self, room: &RoomRef) -> Value {
        self.inspect(room)
            .and_then(|doc| doc.state)
            .unwrap_or(Value::Null)
    }

    /// A service's persisted state as JSON.
    pub fn service_state(&mut self, service: &ServiceRef) -> Value {
        self.inspected(&service.key).unwrap_or(Value::Null)
    }

    /// The room's inspect document, as the admin route builds it.
    pub fn inspect(&mut self, room: &RoomRef) -> Option<InspectDoc> {
        let json = self.inspected(&room.key)?;
        serde_json::from_value(json).ok()
    }

    fn inspected(&mut self, key: &Key) -> Option<Value> {
        self.inner
            .objects
            .borrow_mut()
            .get_mut(key)
            .and_then(|slot| slot.obj.inspect())
    }

    /// Dispose the room as the admin route does; `true` when it existed.
    pub fn force_dispose(&mut self, room: &RoomRef) -> bool {
        let disposed = self.with_object(&room.key, |obj| {
            Box::pin(async move { obj.force_dispose().await })
        });
        self.pump();
        disposed
    }

    /// A SQLite handle for one object's storage.
    pub fn sql_of(&self, binding: &str, name: &str) -> SqlHandle {
        let objects = self.inner.objects.borrow();
        let (_, slot) = objects
            .iter()
            .find(|(key, _)| key.binding == binding && key.name == name)
            .unwrap_or_else(|| panic!("no test object at {binding}/{name}"));
        SqlHandle(slot.host.db.clone())
    }

    /// Every JSON log line every object emitted, in order.
    pub fn logs(&self) -> Vec<String> {
        self.inner.logs.borrow().clone()
    }

    /// How many unfiltered full-state renders the admin tails asked for.
    pub fn full_state_count(&self) -> usize {
        self.inner.full_state_count.get()
    }

    fn open_socket(&mut self) -> usize {
        let socket = self.inner.next_socket.get();
        self.inner.next_socket.set(socket + 1);
        self.inner.sockets.borrow_mut().push(socket);
        socket
    }

    fn push_client(
        &mut self,
        room: &RoomRef,
        session: Session,
        encoding: Encoding,
        socket: usize,
    ) -> C {
        let mut config = ClientConfig::new(self.schema_hash(&room.key), encoding);
        // The harness drives `poll_send` by hand at one virtual instant, so the
        // production 20 msgs/s pacing would stall every send.
        config.max_msgs_per_sec = u32::MAX;
        let index = self.inner.clients.borrow().len();
        self.inner.clients.borrow_mut().push(TestClient {
            core: ClientCore::new(config),
            events: Vec::new(),
            closed: None,
        });
        self.inner.client_sockets.borrow_mut().push(socket);
        self.inner
            .client_players
            .borrow_mut()
            .push((session, encoding));
        self.inner.client_rooms.borrow_mut().push(room.key.clone());
        C(index)
    }

    fn schema_hash(&self, key: &Key) -> String {
        self.inner
            .objects
            .borrow()
            .get(key)
            .and_then(|slot| slot.schema)
            .map(|schema| schema())
            .expect("a room has a schema hash")
    }

    fn deliver(&mut self, socket: usize, msg: WireMsg) {
        self.with_room_of(socket, |obj| {
            Box::pin(async move { obj.message(socket, msg).await })
        });
    }

    /// Send everything a client core has queued.
    fn flush(&mut self, c: C) {
        let now = self.inner.clock.get();
        loop {
            let Some(msg) = self.inner.clients.borrow_mut()[c.0].core.poll_send(now) else {
                break;
            };
            let socket = self.inner.client_sockets.borrow()[c.0];
            self.deliver(socket, msg);
        }
        self.pump();
    }

    fn pump(&mut self) {
        let events = self.inner.take_journal();
        pump_into(
            events,
            &mut self.inner.clients.borrow_mut(),
            &self.inner.client_sockets.borrow(),
            &mut self.inner.tails.borrow_mut(),
        );
    }

    fn due_object(&self, target: u64) -> Option<Key> {
        self.inner
            .objects
            .borrow()
            .iter()
            .find(|(_, slot)| slot.obj.alarm_at().is_some_and(|at| at <= target))
            .map(|(key, _)| key.clone())
    }

    fn with_room_of<T>(
        &mut self,
        socket: usize,
        f: impl for<'a> FnOnce(&'a mut dyn AnyObject) -> LocalBoxFuture<'a, T>,
    ) -> T {
        let key = {
            let sockets = self.inner.client_sockets.borrow();
            let index = sockets.iter().position(|candidate| *candidate == socket);
            match index {
                Some(index) => self.inner.client_rooms.borrow()[index].clone(),
                None => {
                    let tails = self.inner.tails.borrow();
                    let index = tails
                        .iter()
                        .position(|tail| tail.socket == socket)
                        .expect("the socket belongs to a test client or an admin tail");
                    self.inner.tail_rooms.borrow()[index].clone()
                }
            }
        };
        self.with_object(&key, f)
    }

    fn with_object<T>(
        &mut self,
        key: &Key,
        f: impl for<'a> FnOnce(&'a mut dyn AnyObject) -> LocalBoxFuture<'a, T>,
    ) -> T {
        let mut slot = self
            .inner
            .objects
            .borrow_mut()
            .remove(key)
            .unwrap_or_else(|| panic!("no test object at {}", key.label()));
        let out = block_on(f(slot.obj.as_mut()));
        self.inner.objects.borrow_mut().insert(key.clone(), slot);
        out
    }
}

impl Default for TestWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    /// Route one call: the partition and reentrancy checks first, then run the
    /// target with its slot temporarily out of the table, so a nested call can
    /// borrow the table again.
    async fn dispatch(
        &self,
        to: Target,
        envelope: Envelope,
    ) -> std::result::Result<Value, CallError> {
        if self.partitions.borrow().contains(&to.binding) {
            return Err(CallError::Unreachable(format!(
                "{} is partitioned",
                to.binding
            )));
        }
        if !self.bindings.borrow().contains(&to.binding) {
            return Err(CallError::Unreachable(format!(
                "no such binding {}",
                to.binding
            )));
        }
        if envelope.chain.contains(&to) {
            return Err(CallError::ReentrantCall(to));
        }
        let key = Key {
            binding: to.binding.clone(),
            name: to.name.clone(),
        };
        let Some(mut slot) = self.objects.borrow_mut().remove(&key) else {
            return Err(CallError::Unreachable(format!(
                "no object at {}",
                key.label()
            )));
        };
        self.last_delivery
            .borrow_mut()
            .replace((to.clone(), envelope.clone()));
        let call = slot.call;
        let name = to.name;
        let result = {
            let obj = slot.obj.as_mut();
            match call {
                Some(call) => call(obj.as_any_mut(), envelope, name).await,
                None => obj.call(envelope).await,
            }
        };
        self.objects.borrow_mut().insert(key, slot);
        result
    }

    fn take_journal(&self) -> Vec<crate::mem_host::MemEvent> {
        std::mem::take(&mut *self.journal.borrow_mut())
    }

    fn close_socket(&self, socket: usize, code: u16, reason: &str) {
        let present = {
            let mut sockets = self.sockets.borrow_mut();
            match sockets.iter().position(|candidate| *candidate == socket) {
                Some(index) => {
                    sockets.remove(index);
                    true
                }
                None => false,
            }
        };
        if present {
            self.attachments.borrow_mut().remove(&socket);
            self.journal
                .borrow_mut()
                .push(crate::mem_host::MemEvent::Closed {
                    socket,
                    code,
                    reason: reason.to_owned(),
                });
        }
    }
}

/// The [`Host`] one test object's runtime sees: its own storage, the world's
/// shared clock/sockets and the world's call routing.
#[derive(Clone)]
pub struct WorldHost {
    inner: Rc<Inner>,
    db: MemHost,
    target: Target,
    alarm: Rc<Cell<Option<u64>>>,
}

impl WorldHost {
    /// When the object's alarm is set for.
    pub fn alarm_at(&self) -> Option<u64> {
        self.alarm.get()
    }

    /// The object's address.
    pub fn target(&self) -> &Target {
        &self.target
    }
}

impl hearth_server::__private::Sealed for WorldHost {}

impl Host for WorldHost {
    type Socket = usize;

    fn sockets(&self) -> Vec<usize> {
        self.inner.sockets.borrow().clone()
    }

    fn attachment(&self, socket: &usize) -> Option<Attachment> {
        self.inner.attachments.borrow().get(socket).cloned()
    }

    fn set_attachment(&self, socket: &usize, attachment: &Attachment) {
        if self.inner.sockets.borrow().contains(socket) {
            self.inner
                .attachments
                .borrow_mut()
                .insert(*socket, attachment.clone());
        }
    }

    fn send(&self, socket: &usize, msg: WireMsg) {
        if self.inner.sockets.borrow().contains(socket) {
            self.inner
                .journal
                .borrow_mut()
                .push(crate::mem_host::MemEvent::Sent {
                    socket: *socket,
                    msg,
                });
        }
    }

    fn close(&self, socket: &usize, code: u16, reason: &str) {
        self.inner.close_socket(*socket, code, reason);
    }

    fn now_ms(&self) -> u64 {
        self.inner.clock.get()
    }

    fn sql(&self) -> &dyn Sql {
        self.db.sql()
    }

    fn sql_handle(&self) -> Rc<dyn Sql> {
        Rc::new(self.db.clone())
    }

    async fn set_alarm(&self, at_ms: Option<u64>) -> Result<()> {
        self.alarm.set(at_ms);
        Ok(())
    }

    async fn wipe(&self) -> Result<()> {
        self.db.wipe().await
    }

    async fn call(&self, to: &Target, envelope: Envelope) -> std::result::Result<Value, CallError> {
        self.inner.dispatch(to.clone(), envelope).await
    }

    fn has_binding(&self, binding: &str) -> bool {
        self.inner.bindings.borrow().contains(binding)
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

impl Sql for WorldHost {
    fn exec(
        &self,
        query: &str,
        params: Vec<SqlValue>,
    ) -> Result<Vec<serde_json::Map<String, Value>>> {
        <MemHost as Sql>::exec(&self.db, query, params)
    }
}
