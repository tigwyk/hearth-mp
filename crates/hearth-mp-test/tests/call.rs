//! Plan v0.3a Task 1: `Service` objects, typed `ctx.call`, the call chain and
//! the reentrancy guard.
//!
//! Native tests define their objects by hand: the `#[hearth_mp::service]` and
//! `#[hearth_mp::room]` macros generate Durable Object wrappers for wasm, which a
//! native test binary cannot export. The three-line `Callable` impl is all the
//! typed call needs.

use std::time::Duration;

use anyhow::bail;
use hearth_mp_client::ClientEvent;
use hearth_mp_server::call::{CallError, Callable, Target};
use hearth_mp_server::{
    Client, Ctx, JoinOpts, Leave, Mode, Result, Room, RoomConfig, RoomRpc, Service, ServiceCtx,
};
use hearth_mp_sync::{HearthState, Schema};
use hearth_mp_test::world::TestWorld;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize, Default, Debug)]
struct CounterState {
    value: i64,
    handler_calls: i64,
}

#[derive(Serialize, Deserialize, Debug)]
enum CounterRpc {
    Add(i64),
    Get,
    /// Call the given room back, to build a reentrant cycle.
    CallBack(String),
    Fail,
}

#[derive(Default)]
struct Counter;

impl Callable for Counter {
    const BINDING: &'static str = "COUNTER";
    type Rpc = CounterRpc;
    type Reply = i64;
}

impl Service for Counter {
    type State = CounterState;
    type Rpc = CounterRpc;
    type Reply = i64;

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &Target,
        rpc: CounterRpc,
    ) -> Result<i64> {
        ctx.state().handler_calls += 1;
        match rpc {
            CounterRpc::Add(amount) => {
                ctx.state().value += amount;
                Ok(ctx.state().value)
            }
            CounterRpc::Get => Ok(ctx.state().value),
            CounterRpc::CallBack(room) => {
                let reply = ctx.call::<CounterRoom>(&room, CounterRoomRpc::Bump).await;
                match reply {
                    Ok(value) => Ok(value),
                    Err(err) => bail!("callback failed: {err}"),
                }
            }
            CounterRpc::Fail => {
                ctx.state().value += 100;
                bail!("nope")
            }
        }
    }
}

#[derive(HearthState, Serialize, Deserialize, Default, Debug)]
struct RoomState {
    #[sync]
    count: i64,
    #[sync]
    last_reply: i64,
}

#[derive(Deserialize, Schema, Debug)]
enum RoomIntent {
    Bump,
    /// Call the counter service, store its reply and broadcast it.
    CallCounter(String),
    /// Call the service, then fail: the call is not rolled back.
    CallAndFail(String),
    /// Call the service and surface its failure as the handler's failure.
    CallServiceFail(String),
    /// Call a service that calls this room back.
    CallBacker(String),
    Get,
}

#[derive(Default)]
struct CounterRoom;

impl RoomConfig for CounterRoom {
    const MODE: Mode = Mode::Event;
    const MAX_CLIENTS: u8 = 2;
    const RECONNECT_GRACE: Duration = Duration::ZERO;
}

impl Room for CounterRoom {
    type State = RoomState;
    type Intent = RoomIntent;

    async fn on_join(
        &mut self,
        _ctx: &mut Ctx<Self>,
        _who: &Client,
        _opts: JoinOpts,
    ) -> Result<()> {
        Ok(())
    }

    async fn on_intent(
        &mut self,
        ctx: &mut Ctx<Self>,
        _who: &Client,
        intent: RoomIntent,
    ) -> Result<()> {
        match intent {
            RoomIntent::Bump => ctx.state().count += 1,
            RoomIntent::CallCounter(service) => {
                let reply = ctx.call::<Counter>(&service, CounterRpc::Add(1)).await?;
                ctx.state().last_reply = reply;
            }
            RoomIntent::CallAndFail(service) => {
                let reply = ctx.call::<Counter>(&service, CounterRpc::Add(1)).await?;
                ctx.state().last_reply = reply;
                bail!("handler failed after the call")
            }
            RoomIntent::CallServiceFail(service) => {
                let reply = ctx.call::<Counter>(&service, CounterRpc::Fail).await?;
                ctx.state().last_reply = reply;
            }
            RoomIntent::CallBacker(service) => {
                let reply = ctx
                    .call::<Counter>(&service, CounterRpc::CallBack("r1".to_owned()))
                    .await?;
                ctx.state().last_reply = reply;
            }
            RoomIntent::Get => {}
        }
        Ok(())
    }

    async fn on_leave(
        &mut self,
        _ctx: &mut Ctx<Self>,
        _who: &Client,
        _reason: Leave,
    ) -> Result<()> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum CounterRoomRpc {
    Bump,
}

impl Callable for CounterRoom {
    const BINDING: &'static str = "COUNTER_ROOM";
    type Rpc = CounterRoomRpc;
    type Reply = i64;
}

impl RoomRpc for CounterRoom {
    type Rpc = CounterRoomRpc;
    type Reply = i64;

    async fn on_call(
        &mut self,
        ctx: &mut Ctx<Self>,
        _from: &Target,
        rpc: CounterRoomRpc,
    ) -> Result<i64> {
        match rpc {
            CounterRoomRpc::Bump => {
                ctx.state().count += 1;
                Ok(ctx.state().count)
            }
        }
    }
}

fn world() -> (
    TestWorld,
    hearth_mp_test::world::RoomRef,
    hearth_mp_test::world::ServiceRef,
) {
    let mut world = TestWorld::new();
    let room = world.add_rpc_room::<CounterRoom>("COUNTER_ROOM", "r1");
    let counter = world.add_service::<Counter>("c1");
    (world, room, counter)
}

fn room_state(world: &mut TestWorld, room: &hearth_mp_test::world::RoomRef) -> serde_json::Value {
    world.room_state(room)
}

fn errors_since(world: &TestWorld, c: hearth_mp_test::C, from: usize) -> Vec<(String, String)> {
    world.client(c).events[from..]
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Error { code, message, .. } => {
                Some((format!("{code:?}"), message.clone()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn room_calls_service_and_uses_reply() {
    let (mut world, room, counter) = world();
    let player = world.join(&room, "a", json!({}));

    world.intent(player, json!({"CallCounter": "c1"}));
    world.intent(player, json!({"CallCounter": "c1"}));

    assert_eq!(room_state(&mut world, &room)["count"], json!(0));
    assert_eq!(room_state(&mut world, &room)["last_reply"], json!(2));
    assert_eq!(world.service_state(&counter)["value"], json!(2));
}

#[test]
fn service_state_persists_across_hibernation() {
    let (mut world, _room, counter) = world();
    assert_eq!(world.call::<Counter>("c1", CounterRpc::Add(5)).unwrap(), 5);

    world.hibernate_all();

    assert_eq!(world.call::<Counter>("c1", CounterRpc::Get).unwrap(), 5);
    assert_eq!(world.service_state(&counter)["value"], json!(5));
}

#[test]
fn service_handler_error_rolls_back() {
    let (mut world, _room, counter) = world();
    let result = world.call::<Counter>("c1", CounterRpc::Fail);
    assert!(
        matches!(&result, Err(CallError::Handler(message)) if message.contains("nope")),
        "expected a handler failure, got {result:?}"
    );
    assert_eq!(world.service_state(&counter)["value"], json!(0));
}

#[test]
fn undecodable_rpc_is_bad_rpc() {
    let (mut world, _room, _counter) = world();
    let result = world.call_raw("COUNTER", "c1", json!({"NotACounterRpc": 1}));
    assert!(
        matches!(&result, Err(CallError::BadRpc(message)) if message.contains("CounterRpc")),
        "expected BadRpc, got {result:?}"
    );
}

#[test]
fn room_rpc_patches_clients() {
    let (mut world, room, _counter) = world();
    let player = world.join(&room, "a", json!({}));
    let seen = world.client(player).events.len();

    let reply = world.call::<CounterRoom>("r1", CounterRoomRpc::Bump);
    assert_eq!(reply.unwrap(), 1);

    let patched = world.client(player).events[seen..]
        .iter()
        .any(|event| matches!(event, ClientEvent::Patched { ops } if !ops.is_empty()));
    assert!(
        patched,
        "an rpc that changes state patches the room's clients"
    );
    assert_eq!(world.client(player).core.state()["count"], json!(1));
}

#[test]
fn reentrant_cycle_fails_fast() {
    let (mut world, room, _counter) = world();
    let player = world.join(&room, "a", json!({}));
    let seen = world.client(player).events.len();

    // r1 -> c1 (CallBack) -> r1 (Bump), while r1 is still awaiting: the inner
    // call must fail with ReentrantCall instead of hanging.
    world.intent(player, json!({"CallBacker": "c1"}));

    let errors = errors_since(&world, player, seen);
    assert_eq!(errors.len(), 1, "one rejected intent: {errors:?}");
    assert!(
        errors[0].1.contains("reentrant"),
        "the failure names the reentrant call: {errors:?}"
    );
    // The room's failed handler was rolled back, and the service's own call
    // never applied either.
    assert_eq!(room_state(&mut world, &room)["count"], json!(0));
}

#[test]
fn call_not_rolled_back_on_later_handler_error() {
    let (mut world, room, counter) = world();
    let player = world.join(&room, "a", json!({}));

    world.intent(player, json!({"CallAndFail": "c1"}));

    // The room's state change was rolled back; the service kept the call.
    assert_eq!(room_state(&mut world, &room)["last_reply"], json!(0));
    assert_eq!(world.service_state(&counter)["value"], json!(1));
}

#[test]
fn partitioned_target_is_unreachable() {
    let (mut world, _room, counter) = world();
    world.partition("COUNTER", true);

    let result = world.call::<Counter>("c1", CounterRpc::Add(3));
    assert!(
        matches!(&result, Err(CallError::Unreachable(message)) if message.contains("partitioned")),
        "expected Unreachable, got {result:?}"
    );
    // The service never ran, so it has no state at all.
    assert_eq!(world.service_state(&counter), json!(null));
}

#[test]
fn successful_call_to_a_partitioned_target_fails_the_handler() {
    let (mut world, room, _counter) = world();
    let player = world.join(&room, "a", json!({}));
    let seen = world.client(player).events.len();
    world.partition("COUNTER", true);

    world.intent(player, json!({"CallCounter": "c1"}));

    let errors = errors_since(&world, player, seen);
    assert_eq!(errors.len(), 1, "the intent was rejected: {errors:?}");
    assert!(errors[0].1.contains("partitioned"), "{errors:?}");
}

#[test]
fn unknown_binding_is_unreachable() {
    let mut world = TestWorld::new();
    let result = world.call_raw("NOPE", "x", json!(null));
    assert!(
        matches!(&result, Err(CallError::Unreachable(message)) if message.contains("NOPE")),
        "expected Unreachable, got {result:?}"
    );
}
