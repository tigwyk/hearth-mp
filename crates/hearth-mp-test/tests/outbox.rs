//! Plan v0.3a Task 2: the durable outbox (`ctx.notify`), per-target ordering,
//! retry, the 24-hour drop, and receiver-side inbox dedupe.

mod support;

use std::time::Duration;

use hearth_mp_server::call::{CallError, Callable, Target};
use hearth_mp_server::{
    Client, Ctx, JoinOpts, Leave, Mode, Result, Room, RoomConfig, Service, ServiceCtx,
};
use hearth_mp_sync::{HearthState, Schema};
use hearth_mp_test::world::{RoomRef, ServiceRef, TestWorld};
use hearth_mp_test::TestRoom;
use serde::{Deserialize, Serialize};
use serde_json::json;
use support::{CounterRoom, TestRegistry};

#[derive(Serialize, Deserialize, Default, Debug)]
struct RecorderState {
    items: Vec<i64>,
}

#[derive(Serialize, Deserialize, Debug)]
enum RecorderRpc {
    Push(i64),
    Items,
}

#[derive(Default)]
struct Recorder;

impl Callable for Recorder {
    const BINDING: &'static str = "RECORDER";
    type Rpc = RecorderRpc;
    type Reply = i64;
}

impl Service for Recorder {
    type State = RecorderState;
    type Rpc = RecorderRpc;
    type Reply = i64;

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &Target,
        rpc: RecorderRpc,
    ) -> Result<i64> {
        match rpc {
            RecorderRpc::Push(item) => {
                ctx.state().items.push(item);
                Ok(item)
            }
            RecorderRpc::Items => Ok(ctx.state().items.len() as i64),
        }
    }
}

/// The same recorder under a different binding, so one target can be partitioned
/// without taking the other down with it.
#[derive(Default)]
struct OtherRecorder;

impl Callable for OtherRecorder {
    const BINDING: &'static str = "OTHER_RECORDER";
    type Rpc = RecorderRpc;
    type Reply = i64;
}

impl Service for OtherRecorder {
    type State = RecorderState;
    type Rpc = RecorderRpc;
    type Reply = i64;

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &Target,
        rpc: RecorderRpc,
    ) -> Result<i64> {
        match rpc {
            RecorderRpc::Push(item) => {
                ctx.state().items.push(item);
                Ok(item)
            }
            RecorderRpc::Items => Ok(ctx.state().items.len() as i64),
        }
    }
}

#[derive(HearthState, Serialize, Deserialize, Default, Debug)]
struct NotifierState {
    #[sync]
    notifies: i64,
}

#[derive(Deserialize, Schema, Debug)]
enum NotifierIntent {
    /// Durable fire-after-commit to the named recorder.
    Notify(String, i64),
    /// Durable notify to the other-bound recorder.
    NotifyOther(String, i64),
    /// Durable notify, then a handler error: the notify must never be sent.
    NotifyAndFail(String, i64),
    /// Deliver immediately (not durable), to compare with notify.
    Call(String, i64),
}

#[derive(Default)]
struct Notifier;

impl RoomConfig for Notifier {
    const MODE: Mode = Mode::Event;
    const MAX_CLIENTS: u8 = 2;
    const RECONNECT_GRACE: Duration = Duration::ZERO;
}

impl Room for Notifier {
    type State = NotifierState;
    type Intent = NotifierIntent;

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
        intent: NotifierIntent,
    ) -> Result<()> {
        match intent {
            NotifierIntent::Notify(to, item) => {
                ctx.notify::<Recorder>(&to, RecorderRpc::Push(item));
                ctx.state().notifies += 1;
            }
            NotifierIntent::NotifyOther(to, item) => {
                ctx.notify::<OtherRecorder>(&to, RecorderRpc::Push(item));
                ctx.state().notifies += 1;
            }
            NotifierIntent::NotifyAndFail(to, item) => {
                ctx.notify::<Recorder>(&to, RecorderRpc::Push(item));
                ctx.state().notifies += 1;
                anyhow::bail!("handler failed after notify");
            }
            NotifierIntent::Call(to, item) => {
                let _ = ctx.call::<Recorder>(&to, RecorderRpc::Push(item)).await?;
                ctx.state().notifies += 1;
            }
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

fn world() -> (TestWorld, RoomRef, ServiceRef) {
    let mut world = TestWorld::new();
    let room = world.add_room::<Notifier>("NOTIFIER", "n1");
    let recorder = world.add_service::<Recorder>("rec");
    (world, room, recorder)
}

fn items(world: &mut TestWorld, recorder: &ServiceRef) -> Vec<i64> {
    let state = world.service_state(recorder);
    serde_json::from_value(state.get("items").cloned().unwrap_or_else(|| json!([])))
        .expect("recorder items decode")
}

fn reports(world: &mut TestWorld, registry: &ServiceRef) -> Vec<serde_json::Value> {
    let state = world.service_state(registry);
    serde_json::from_value(state.get("reports").cloned().unwrap_or_else(|| json!([])))
        .expect("registry reports decode")
}

fn logs(world: &TestWorld, kind: &str) -> Vec<String> {
    world
        .logs()
        .into_iter()
        .filter(|line| line.contains(kind))
        .collect()
}

#[test]
fn notify_delivered_after_commit() {
    let (mut world, room, recorder) = world();
    let player = world.join(&room, "a", json!({}));

    world.intent(player, json!({"Notify": ["rec", 7]}));

    assert_eq!(items(&mut world, &recorder), vec![7]);
    assert_eq!(
        world.room_state(&room)["notifies"],
        json!(1),
        "the handler's own state change committed"
    );
}

#[test]
fn failed_handler_never_notifies() {
    let (mut world, room, recorder) = world();
    let player = world.join(&room, "a", json!({}));

    world.intent(player, json!({"NotifyAndFail": ["rec", 7]}));

    assert!(
        items(&mut world, &recorder).is_empty(),
        "a failed handler drops its notifies"
    );
    assert_eq!(world.room_state(&room)["notifies"], json!(0));
}

#[test]
fn target_down_then_up_delivers_once() {
    let (mut world, room, recorder) = world();
    let player = world.join(&room, "a", json!({}));
    world.partition("RECORDER", true);

    for item in [1, 2, 3] {
        world.intent(player, json!({"Notify": ["rec", item]}));
    }
    world.advance(Duration::from_secs(10));
    assert!(items(&mut world, &recorder).is_empty());

    world.partition("RECORDER", false);
    world.advance(Duration::from_secs(400));

    assert_eq!(items(&mut world, &recorder), vec![1, 2, 3]);
}

#[test]
fn duplicate_delivery_is_deduped() {
    let (mut world, room, recorder) = world();
    let player = world.join(&room, "a", json!({}));
    world.intent(player, json!({"Notify": ["rec", 5]}));
    assert_eq!(items(&mut world, &recorder), vec![5]);

    let reply = world.redeliver_last();

    assert!(reply.is_ok(), "a duplicate answers success: {reply:?}");
    assert_eq!(
        items(&mut world, &recorder),
        vec![5],
        "the handler ran exactly once"
    );
}

#[test]
fn per_target_order_kept_across_failure() {
    let (mut world, room, _recorder) = world();
    let x = world.add_service::<Recorder>("x");
    let y = world.add_service::<OtherRecorder>("y");
    let player = world.join(&room, "a", json!({}));
    world.partition("RECORDER", true);

    world.intent(player, json!({"Notify": ["x", 1]}));
    world.intent(player, json!({"Notify": ["x", 2]}));
    world.intent(player, json!({"NotifyOther": ["y", 10]}));
    world.advance(Duration::from_secs(10));

    // The down target blocks only its own order; the live one is unaffected.
    assert!(items(&mut world, &x).is_empty());
    assert_eq!(items(&mut world, &y), vec![10]);

    world.partition("RECORDER", false);
    world.advance(Duration::from_secs(400));
    assert_eq!(items(&mut world, &x), vec![1, 2]);
}

#[test]
fn outbox_survives_hibernation() {
    let (mut world, room, recorder) = world();
    let player = world.join(&room, "a", json!({}));
    world.partition("RECORDER", true);
    world.intent(player, json!({"Notify": ["rec", 9]}));

    world.hibernate_all();
    world.partition("RECORDER", false);
    world.advance(Duration::from_secs(400));

    assert_eq!(items(&mut world, &recorder), vec![9]);
}

#[test]
fn outbox_drops_after_24h_and_logs() {
    let (mut world, room, recorder) = world();
    let player = world.join(&room, "a", json!({}));
    world.partition("RECORDER", true);
    world.intent(player, json!({"Notify": ["rec", 4]}));

    world.advance(Duration::from_secs(24 * 60 * 60 + 1));

    assert!(items(&mut world, &recorder).is_empty());
    assert!(
        !logs(&world, "outbox_dropped").is_empty(),
        "the drop is logged: {:?}",
        world.logs()
    );
}

#[test]
fn registry_reports_survive_registry_outage() {
    let mut world = TestWorld::new();
    let room = world.add_room::<CounterRoom>("counter", "r1");
    let registry = world.add_service::<TestRegistry>("global");
    world.partition("HEARTH_REGISTRY", true);

    let a = world.join(&room, "a", json!({}));
    let b = world.join(&room, "b", json!({}));
    world.intent(a, json!({"Add": 5}));

    // Gameplay is unaffected by the registry outage.
    assert_eq!(world.client(a).core.state()["count"], json!(5));
    assert_eq!(world.client(b).core.state()["count"], json!(5));
    assert!(reports(&mut world, &registry).is_empty());

    world.partition("HEARTH_REGISTRY", false);
    world.advance(Duration::from_secs(10));

    let reports = reports(&mut world, &registry);
    assert_eq!(reports.len(), 2, "Open and one Players report: {reports:?}");
    assert_eq!(reports[0]["kind"], json!("open"));
    assert_eq!(reports[1]["kind"], json!("players"));
}

#[test]
fn non_notify_call_still_uses_the_typed_path() {
    let (mut world, room, recorder) = world();
    let player = world.join(&room, "a", json!({}));

    world.intent(player, json!({"Call": ["rec", 3]}));
    // A direct call is delivered during the handler pass; a notify after it.
    assert_eq!(items(&mut world, &recorder), vec![3]);
}

/// A room report also reaches the registry through the outbox on a no-op pass.
#[test]
fn reports_are_durable_rows_not_direct_calls() {
    let mut world = TestWorld::new();
    let room = world.add_room::<CounterRoom>("counter", "r1");
    world.add_service::<TestRegistry>("global");
    world.partition("HEARTH_REGISTRY", true);
    world.join(&room, "a", json!({}));

    let room_sql = world.sql_of("counter", "r1");
    let rows = room_sql
        .sql()
        .exec("SELECT COUNT(*) AS n FROM hearth_outbox", vec![])
        .expect("the outbox is readable");
    let queued = rows
        .first()
        .and_then(|row| row.get("n"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    assert_eq!(queued, 1, "the Open report is a pending outbox row");
}

#[test]
fn plain_room_without_registry_binding_never_writes_an_outbox() {
    let mut room = TestRoom::<CounterRoom>::new();
    let _a = room.join("a", json!({}));
    let rows = room
        .sql()
        .exec("SELECT COUNT(*) AS n FROM hearth_outbox", vec![])
        .expect("the outbox table exists");
    let queued = rows
        .first()
        .and_then(|row| row.get("n"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    assert_eq!(queued, 0, "no binding, no reports, no rows");
}

#[test]
fn notify_to_a_missing_object_retries_then_stays_queued() {
    let (mut world, room, _recorder) = world();
    let player = world.join(&room, "a", json!({}));

    // A binding the deployment declares but with no instance at that name.
    world.intent(player, json!({"Notify": ["nobody", 1]}));
    world.advance(Duration::from_secs(5));

    let sql = world.sql_of("NOTIFIER", "n1");
    let rows = sql
        .sql()
        .exec("SELECT attempts FROM hearth_outbox", vec![])
        .expect("the outbox is readable");
    let attempts = rows
        .first()
        .and_then(|row| row.get("attempts"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    assert!(attempts >= 1, "the failed delivery was retried: {rows:?}");
}

/// `CallError` is part of the public surface these tests exercise.
#[allow(dead_code)]
fn _call_error_is_public(err: CallError) -> String {
    err.to_string()
}
