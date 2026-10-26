//! Plan v0.4 Task 3: ticked rooms in the runtime. The `Mover` fixture is the
//! plan's: 10 Hz, persist every 5 ticks, `pos += vel` each tick.

mod support;

use std::time::Duration;

use hearth_client::ClientEvent;
use hearth_protocol::{encode, Encoding, ErrorCode, Frame, IntentMsg, PlayerId, Value};
use hearth_server::{Client, Ctx, JoinOpts, Leave, Mode, Result, Room, RoomConfig};
use hearth_sync::{HearthState, Schema, SyncMap};
use hearth_test::TestRoom;
use serde::{Deserialize, Serialize};
use serde_json::json;
use support::CounterRoom;

#[derive(HearthState, Serialize, Deserialize, Default)]
pub struct MoveState {
    #[sync]
    pub pos: SyncMap<PlayerId, i32>,
    #[sync]
    pub vel: SyncMap<PlayerId, i32>,
    /// Every `dt` an `on_tick` saw, for the fixed-timestep test. Server-only.
    pub tick_dts: Vec<u64>,
}

#[derive(Schema, Deserialize, Debug)]
pub enum MoveIntent {
    SetVel(i32),
}

#[derive(Default)]
pub struct Mover;

impl RoomConfig for Mover {
    const MODE: Mode = Mode::Ticked;
    const MAX_CLIENTS: u8 = 2;
    const TICK_HZ: u8 = 10;
    const PERSIST_EVERY: u32 = 5;
}

impl Room for Mover {
    type State = MoveState;
    type Intent = MoveIntent;

    async fn on_join(&mut self, ctx: &mut Ctx<Self>, who: &Client, _opts: JoinOpts) -> Result<()> {
        let player = who.player_id.clone();
        ctx.state().pos.insert(player.clone(), 0);
        ctx.state().vel.insert(player, 0);
        Ok(())
    }

    async fn on_intent(
        &mut self,
        ctx: &mut Ctx<Self>,
        who: &Client,
        intent: MoveIntent,
    ) -> Result<()> {
        match intent {
            MoveIntent::SetVel(vel) => {
                ctx.state()
                    .vel
                    .insert(who.player_id.clone(), vel.clamp(-1, 1));
            }
        }
        Ok(())
    }

    async fn on_leave(&mut self, ctx: &mut Ctx<Self>, who: &Client, _reason: Leave) -> Result<()> {
        ctx.state().pos.remove(&who.player_id);
        ctx.state().vel.remove(&who.player_id);
        Ok(())
    }

    async fn on_tick(&mut self, ctx: &mut Ctx<Self>, dt: Duration) -> Result<()> {
        let players: Vec<PlayerId> = ctx
            .clients()
            .iter()
            .map(|client| client.player_id.clone())
            .collect();
        let state = ctx.state();
        state.tick_dts.push(dt.as_millis() as u64);
        for player in players {
            let vel = state.vel.get(&player).copied().unwrap_or(0);
            let pos = state.pos.get(&player).copied().unwrap_or(0);
            state.pos.insert(player, pos + vel);
        }
        Ok(())
    }
}

fn new_room() -> TestRoom<Mover> {
    TestRoom::new()
}

fn leave(r: &mut TestRoom<Mover>, c: hearth_test::C) {
    r.send_raw(c, encode(&Frame::Leave {}, Encoding::MsgPack));
}

fn log_lines(r: &TestRoom<Mover>, event: &str) -> Vec<Value> {
    r.logs()
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["hearth"] == json!(event))
        .collect()
}

#[test]
fn ticks_only_while_connected() {
    let mut r = new_room();
    r.advance(Duration::from_secs(1));
    assert_eq!(r.ticks(), 0, "an empty room never ticks");

    let a = r.join("a", json!({}));
    r.advance(Duration::from_secs(1));
    assert_eq!(r.ticks(), 10, "10 Hz for one second");
    assert_eq!(
        r.server_state()["pos"],
        json!({"a": 0}),
        "on_tick ran 10 times, but the velocity is still 0"
    );

    leave(&mut r, a);
    let ticks = r.ticks();
    r.advance(Duration::from_secs(1));
    assert_eq!(r.ticks(), ticks, "stopped ticking when the room emptied");
    // The empty-room grace timer is still the next wake; what matters is that
    // no tick wake is scheduled any more (Review Focus 2). A disposed room has
    // `next_wake() == None`; see `dispose_stops_ticking`.
    let wake = r.next_wake().expect("the empty-room grace timer");
    assert!(wake >= r.clock_ms(), "the next wake is a timer, not a tick");
}

#[test]
fn held_seat_does_not_tick() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.advance(Duration::from_millis(200));
    assert_eq!(r.ticks(), 2);

    r.drop_socket(a);
    r.advance(Duration::from_secs(1));
    assert_eq!(r.ticks(), 2, "a held seat does not keep the room ticking");

    let resumed = r.resume(a);
    assert!(r.client(resumed).closed.is_none());
    r.advance(Duration::from_secs(1));
    assert_eq!(
        r.ticks(),
        12,
        "ticking resumed from the persisted tick number"
    );
}

#[test]
fn intents_apply_at_next_tick_in_order() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let start = r.client(a).events.len();

    r.intents(a, vec![json!({"SetVel": 1}), json!({"SetVel": -1})]);
    assert_eq!(
        r.server_state()["vel"],
        json!({"a": 0}),
        "intents are not applied between ticks"
    );

    r.advance(Duration::from_millis(100));
    assert_eq!(
        r.server_state()["vel"],
        json!({"a": -1}),
        "arrival order wins"
    );
    assert_eq!(
        r.server_state()["pos"],
        json!({"a": -1}),
        "intents run before on_tick, so this tick moved by -1"
    );
    let acks: Vec<Vec<u32>> = r.client(a).events[start..]
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Acked { intent_seqs, .. } => Some(intent_seqs.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(acks, vec![vec![1, 2]], "one ACK with both seqs");
    assert_eq!(
        r.client(a).core.last_tick(),
        Some((1, 100)),
        "the PATCH carries the tick and the server time"
    );
}

#[test]
fn invalid_intent_value_clamped() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"SetVel": 5}));
    r.advance(Duration::from_millis(100));
    assert_eq!(r.server_state()["vel"], json!({"a": 1}));
}

#[test]
fn undecodable_intent_immediate_bad_intent() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    let start = r.client(a).events.len();
    r.send_raw(
        a,
        encode(
            &Frame::Intent {
                intents: vec![IntentMsg {
                    seq: 1,
                    intent: json!({"Nope": 1}),
                }],
            },
            Encoding::MsgPack,
        ),
    );
    let errors: Vec<_> = r.client(a).events[start..]
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Error { code, ref_seq, .. } => Some((*code, *ref_seq)),
            _ => None,
        })
        .collect();
    assert_eq!(
        errors,
        vec![(ErrorCode::BadIntent, Some(1))],
        "decoding errors are immediate, not tick-buffered"
    );
}

#[test]
fn catch_up_capped_at_three() {
    let mut r = new_room();
    let a = r.join("a", json!({}));

    r.stall(Duration::from_secs(2));
    assert_eq!(r.ticks(), 3, "at most 3 catch-up ticks");
    let dropped = log_lines(&r, "tick_dropped");
    assert_eq!(dropped.len(), 1, "{:?}", r.logs());
    assert_eq!(dropped[0]["n"], json!(17), "20 due - 3 run");

    r.advance(Duration::from_millis(100));
    assert_eq!(
        r.ticks(),
        4,
        "the absolute schedule skipped the dropped ticks"
    );
    let _ = a;
}

#[test]
fn fixed_dt_always_period() {
    let mut r = new_room();
    let _a = r.join("a", json!({}));
    r.advance(Duration::from_millis(300));
    r.stall(Duration::from_millis(500));
    r.advance(Duration::from_millis(200));

    let dts: Vec<u64> = r.server_state()["tick_dts"]
        .as_array()
        .expect("on_tick recorded its dt")
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    assert!(!dts.is_empty());
    assert!(
        dts.iter().all(|dt| *dt == 100),
        "every step is exactly 1 s / 10 Hz: {dts:?}"
    );
}

#[test]
fn persist_every_n_ticks() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"SetVel": 1}));
    let seq_after_join = support::stored_seq(&r);

    r.advance(Duration::from_millis(400));
    assert_eq!(r.ticks(), 4);
    assert_eq!(
        support::stored_seq(&r),
        seq_after_join,
        "ticks 1..4 are not persisted"
    );

    r.advance(Duration::from_millis(100));
    assert_eq!(r.ticks(), 5);
    assert_ne!(
        support::stored_seq(&r),
        seq_after_join,
        "tick 5 persists the state and the tick number"
    );
    assert_eq!(
        r.inspect().expect("the room exists").tick,
        5,
        "the admin inspect exposes the tick"
    );
}

#[test]
fn crash_loses_at_most_n_ticks_never_corrupts() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"SetVel": 1}));
    r.advance(Duration::from_millis(700));
    assert_eq!(r.ticks(), 7);

    // Eviction: drop the runtime without a clean stop.
    r.hibernate();
    let resumed = r.resume(a);
    assert!(r.client(resumed).closed.is_none());
    assert_eq!(r.ticks(), 5, "the persisted tick number");
    assert_eq!(
        r.server_state()["pos"],
        json!({"a": 5}),
        "the state is the one persisted at tick 5, not the lost ticks 6-7"
    );
}

#[test]
fn buffered_intents_lost_on_eviction_are_unconfirmed() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"SetVel": 1}));
    r.hibernate();

    let events = r.client_mut(a).core.on_disconnected();
    assert_eq!(
        events,
        vec![ClientEvent::IntentsUnconfirmed { seqs: vec![1] }],
        "the client is told the buffered intent is unconfirmed"
    );
}

#[test]
fn patch_carries_tick_and_t() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"SetVel": 1}));
    r.advance(Duration::from_millis(100));
    assert_eq!(
        r.client(a).core.last_tick(),
        Some((1, 100)),
        "PATCH records tick 1 and the server ms at that tick"
    );
    r.advance(Duration::from_millis(100));
    assert_eq!(r.client(a).core.last_tick(), Some((2, 200)));
}

#[test]
fn dispose_stops_ticking() {
    let mut r = new_room();
    let _a = r.join("a", json!({}));
    r.advance(Duration::from_millis(200));
    assert_eq!(r.ticks(), 2);

    assert!(r.force_dispose());
    assert_eq!(r.ticks(), 0, "a disposed room has no tick number");
    assert_eq!(r.next_wake(), None, "nothing is scheduled after a dispose");
    r.advance(Duration::from_secs(1));
    assert_eq!(r.ticks(), 0);
    assert!(support::table_names(&r).is_empty());
}

#[test]
fn watchdog_alarm_restarts_a_hibernated_room() {
    let mut r = new_room();
    let a = r.join("a", json!({}));
    r.advance(Duration::from_millis(200));
    assert_eq!(r.ticks(), 2);

    // Drop the runtime as hibernation does, then advance past the tick
    // watchdog deadline and fire the alarm: the fresh instance reloads the
    // persisted state (ticks 1-2 were never persisted), resyncs the still-
    // attached clients with a FULL_STATE and restarts the schedule instead of
    // staying dead. Production relies on this after an eviction, where the loop
    // driver's in-object future is gone.
    r.hibernate();
    r.stall(Duration::from_secs(300));
    r.advance(Duration::from_millis(100));
    assert_eq!(
        r.ticks(),
        1,
        "the tick counter restarts at the persisted tick"
    );
    // A tick with no state change sends no patch, so provoke one: the resynced
    // client must apply it even though its old high-water tick was higher.
    r.intent(a, json!({"SetVel": 1}));
    r.advance(Duration::from_millis(100));
    assert_eq!(r.ticks(), 2);
    assert_eq!(
        r.client(a).core.last_tick().map(|(tick, _)| tick),
        Some(2),
        "the resynced client applies the restarted schedule"
    );
    assert_eq!(r.client(a).core.state()["pos"], json!({"a": 1}));
}

#[test]
fn event_mode_unchanged() {
    let mut r = TestRoom::<CounterRoom>::new();
    let a = r.join("a", json!({}));
    r.intent(a, json!({"Add": 2}));
    r.advance(Duration::from_secs(1));

    assert_eq!(r.ticks(), 0, "event rooms have no ticks");
    assert_eq!(
        r.inspect().expect("the room exists").tick,
        0,
        "the admin inspect reports tick 0 in event mode"
    );
    assert_eq!(r.client(a).core.state()["count"], json!(2));
    assert_eq!(
        r.client(a).core.last_tick(),
        None,
        "event-mode PATCHes carry neither tick nor t"
    );
    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        None,
        "event-mode room still runs normally"
    );
}
