//! Shared test rooms for the runtime tests (plan Tasks 5 and 6 of v0.1, plus
//! the reconnect tests of v0.2a Task 1).
#![allow(dead_code)]

use std::time::Duration;

use hearth_client::ClientEvent;
use hearth_protocol::{ErrorCode, PatchOp, PlayerId};
use hearth_server::{
    Client, Ctx, JoinOpts, Leave, Mode, Result, Room, RoomConfig, SqlValue, TimerKey,
};
use hearth_sync::{HearthState, Schema, SyncMap};
use hearth_test::{TestRoom, C};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The plan's state shape plus a few server-only fields so tests can observe
/// joins and leave reasons without leaking them to clients.
///
/// `hidden`, `join_calls`, `leave_calls` and `leave_log` are unmarked, so the
/// `HearthState` derive keeps them out of the schema hash and every client view.
#[derive(HearthState, Serialize, Deserialize, Default, Debug)]
pub struct CState {
    #[sync]
    pub count: i64,
    #[sync]
    pub seats: SyncMap<PlayerId, u8>,
    #[sync]
    pub connected: SyncMap<PlayerId, bool>,
    #[sync(owner)]
    pub secrets: SyncMap<PlayerId, String>,
    pub hidden: i64,
    pub join_calls: i64,
    pub leave_calls: i64,
    pub leave_log: Vec<String>,
}

#[derive(Deserialize, Schema, Debug)]
pub enum CIntent {
    Add(i64),
    Fail,
    Secret(String),
    Shout(String),
    Timer(String, u64),
    Kick(String),
    Lock,
    Dispose,
    Sql,
    /// Copy `ctx.clients()`'s `connected` flags into the synced `connected` map.
    Clients,
}

/// Generates a counter test room with the given reconnect grace. The room
/// behaviour matches the v0.1 plan; only [`RoomConfig::RECONNECT_GRACE`] varies.
macro_rules! counter_room {
    ($room:ident, $grace:expr) => {
        #[derive(Default)]
        pub struct $room;

        impl RoomConfig for $room {
            const MODE: Mode = Mode::Event;
            const MAX_CLIENTS: u8 = 2;
            const RECONNECT_GRACE: Duration = $grace;
        }

        impl Room for $room {
            type State = CState;
            type Intent = CIntent;

            async fn on_join(
                &mut self,
                ctx: &mut Ctx<Self>,
                who: &Client,
                opts: JoinOpts,
            ) -> Result<()> {
                if opts.0 == json!({"reject": true}) {
                    anyhow::bail!("join rejected by test room");
                }
                let state = ctx.state();
                state.join_calls += 1;
                state.seats.insert(who.player_id.clone(), who.seat);
                Ok(())
            }

            async fn on_intent(
                &mut self,
                ctx: &mut Ctx<Self>,
                who: &Client,
                intent: CIntent,
            ) -> Result<()> {
                match intent {
                    CIntent::Add(amount) => ctx.state().count += amount,
                    CIntent::Fail => {
                        ctx.state().count += 100;
                        ctx.broadcast("x", "should never be delivered");
                        ctx.timer("never", Duration::from_secs(1));
                        anyhow::bail!("nope");
                    }
                    CIntent::Secret(secret) => {
                        ctx.state().secrets.insert(who.player_id.clone(), secret);
                    }
                    CIntent::Shout(text) => ctx.broadcast("shout", text),
                    CIntent::Timer(key, after_ms) => {
                        ctx.timer(key, Duration::from_millis(after_ms))
                    }
                    CIntent::Kick(player) => ctx.kick(&PlayerId(player)),
                    CIntent::Lock => ctx.lock(),
                    CIntent::Dispose => ctx.dispose(),
                    CIntent::Sql => {
                        let sql = ctx.sql();
                        sql.exec(
                            "CREATE TABLE IF NOT EXISTS dev (k TEXT PRIMARY KEY, v INTEGER)",
                            vec![],
                        )?;
                        sql.exec(
                            "INSERT OR REPLACE INTO dev (k, v) VALUES (?, ?)",
                            vec![SqlValue::Text("hello".into()), SqlValue::Int(7)],
                        )?;
                    }
                    CIntent::Clients => {
                        let flags: Vec<(PlayerId, bool)> = ctx
                            .clients()
                            .iter()
                            .map(|client| (client.player_id.clone(), client.connected))
                            .collect();
                        let state = ctx.state();
                        for (player, connected) in flags {
                            state.connected.insert(player, connected);
                        }
                    }
                }
                Ok(())
            }

            async fn on_leave(
                &mut self,
                ctx: &mut Ctx<Self>,
                who: &Client,
                reason: Leave,
            ) -> Result<()> {
                let state = ctx.state();
                state.leave_calls += 1;
                state
                    .leave_log
                    .push(format!("{}:{reason:?}", who.player_id));
                state.seats.remove(&who.player_id);
                Ok(())
            }

            async fn on_disconnect(&mut self, ctx: &mut Ctx<Self>, _who: &Client) -> Result<()> {
                ctx.state().hidden += 1;
                Ok(())
            }

            async fn on_reconnect(&mut self, ctx: &mut Ctx<Self>, _who: &Client) -> Result<()> {
                ctx.state().hidden += 10;
                Ok(())
            }

            async fn on_timer(&mut self, ctx: &mut Ctx<Self>, key: TimerKey) -> Result<()> {
                // The "room variant whose on_timer errors" of v0.1 plan Task 6:
                // a `boom` timer fails so the runtime's no-retry behaviour is
                // testable.
                if key.0 == "boom" {
                    anyhow::bail!("timer handler failed");
                }
                ctx.state().count += 1000;
                Ok(())
            }
        }
    };
}

// The default v0.2a room: a 30 s reconnect grace.
counter_room!(CounterRoom, Duration::from_secs(30));

// The v0.1 room: no reconnect grace, so a socket close calls `on_leave` at
// once. Used by the v0.1 tests that assumed immediate leave on close.
counter_room!(GraceZeroRoom, Duration::ZERO);

/// The schema hash a client must send in `HELLO` to join a [`CounterRoom`].
pub fn schema_hash() -> String {
    hearth_server::schema_hash_hex::<CounterRoom>()
}

// Shared assertion helpers. Every test binary includes this module; helpers a
// particular binary does not use are expected (the module allows dead code).

pub fn events_since<R: Room>(room: &TestRoom<R>, c: C, from: usize) -> &[ClientEvent] {
    &room.client(c).events[from..]
}

pub fn patched_ops<R: Room>(room: &TestRoom<R>, c: C, from: usize) -> Vec<Vec<PatchOp>> {
    events_since(room, c, from)
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Patched { ops } => Some(ops.clone()),
            _ => None,
        })
        .collect()
}

pub fn acks<R: Room>(room: &TestRoom<R>, c: C, from: usize) -> Vec<(u64, Vec<u32>)> {
    events_since(room, c, from)
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Acked {
                state_seq,
                intent_seqs,
            } => Some((*state_seq, intent_seqs.clone())),
            _ => None,
        })
        .collect()
}

pub fn errors<R: Room>(
    room: &TestRoom<R>,
    c: C,
    from: usize,
) -> Vec<(ErrorCode, String, Option<u32>)> {
    events_since(room, c, from)
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Error {
                code,
                message,
                ref_seq,
            } => Some((*code, message.clone(), *ref_seq)),
            _ => None,
        })
        .collect()
}

/// The `hearth_state.seq` value persisted right now.
pub fn stored_seq<R: Room>(room: &TestRoom<R>) -> u64 {
    let rows = room
        .sql()
        .exec("SELECT seq FROM hearth_state WHERE id = 0", vec![])
        .expect("hearth_state is readable");
    rows.first()
        .and_then(|row| row.get("seq"))
        .and_then(|value| value.as_u64())
        .expect("a persisted seq row")
}

/// The `hearth_state.locked` flag persisted right now.
pub fn stored_locked<R: Room>(room: &TestRoom<R>) -> i64 {
    let rows = room
        .sql()
        .exec("SELECT locked FROM hearth_state WHERE id = 0", vec![])
        .expect("hearth_state is readable");
    rows.first()
        .and_then(|row| row.get("locked"))
        .and_then(|value| value.as_i64())
        .expect("a persisted locked row")
}

/// Every non-system table currently in the room's database.
pub fn table_names<R: Room>(room: &TestRoom<R>) -> Vec<String> {
    let rows = room
        .sql()
        .exec(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            vec![],
        )
        .expect("sqlite_master is readable");
    rows.iter()
        .filter_map(|row| row.get("name"))
        .filter_map(|value| value.as_str())
        .map(str::to_owned)
        .collect()
}

/// Every persisted timer key, in key order.
pub fn timer_keys<R: Room>(room: &TestRoom<R>) -> Vec<String> {
    let rows = room
        .sql()
        .exec("SELECT key FROM hearth_timers ORDER BY key", vec![])
        .expect("hearth_timers is readable");
    rows.iter()
        .filter_map(|row| row.get("key"))
        .filter_map(|value| value.as_str())
        .map(str::to_owned)
        .collect()
}

/// The held-seat player ids, in player-id order.
pub fn held_players<R: Room>(room: &TestRoom<R>) -> Vec<String> {
    let rows = room
        .sql()
        .exec(
            "SELECT player_id FROM hearth_held ORDER BY player_id",
            vec![],
        )
        .expect("hearth_held is readable");
    rows.iter()
        .filter_map(|row| row.get("player_id"))
        .filter_map(|value| value.as_str())
        .map(str::to_owned)
        .collect()
}
