//! Hearth v0.3a matchmaker: quick-join, seat reservations, pools and the lobby
//! listing.
//!
//! One matchmaker Durable Object exists per `(room_type, region)`: `QuickJoin`
//! mints `m_<region>_<16 hex>` names with a location hint from the gateway, and
//! rooms report their lobby state back through the durable outbox. The room is
//! still the authority for its match; the matchmaker only remembers which rooms
//! exist, who is seated, and which seats are reserved.
//!
//! Shortcuts, both with an upgrade path:
//! - one matchmaker DO per `(room_type, region)`: shard by pool when a pool
//!   outgrows a single DO.
//! - quick-join only, no skill or rating matching: add queues when a game needs
//!   them.

#![warn(missing_docs)]

use hearth_protocol::{PlayerId, Value};
use hearth_server::{Callable, Result, RoomReport, Service, ServiceCtx, Sql, SqlValue, Target};
use serde::{Deserialize, Serialize};

/// The wrangler binding every matchmaker class is exported as.
pub const BINDING: &str = "HEARTH_MATCHMAKER";

/// How long a quick-join seat is held for the player to connect.
pub const RESERVATION_MS: u64 = 30_000;

const CREATE_ROOMS: &str = "CREATE TABLE IF NOT EXISTS mm_rooms (\
    name TEXT PRIMARY KEY, \
    pool TEXT NOT NULL, \
    capacity INTEGER NOT NULL, \
    players TEXT NOT NULL, \
    locked INTEGER NOT NULL, \
    metadata TEXT NOT NULL, \
    minted_ms INTEGER NOT NULL, \
    updated_ms INTEGER NOT NULL)";
const CREATE_RESERVATIONS: &str = "CREATE TABLE IF NOT EXISTS mm_reservations (\
    player_id TEXT PRIMARY KEY, \
    room TEXT NOT NULL, \
    until_ms INTEGER NOT NULL)";

/// One call into the matchmaker.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MmRpc {
    /// Seat the player in the fullest room with a free seat, or mint one.
    QuickJoin {
        /// The player to seat.
        player: PlayerId,
        /// The pool to match in.
        pool: String,
        /// Seats per room.
        capacity: u8,
    },
    /// The lobby listing of open rooms in one pool.
    List {
        /// The pool to list.
        pool: String,
        /// At most this many rooms.
        limit: u32,
    },
    /// A room's lifecycle report; reports for names this matchmaker never
    /// minted are ignored, so a self-made `m_...` name cannot pollute the lobby.
    Report(RoomReport),
}

/// One matchmaker reply.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum MmReply {
    /// The room the player was seated in.
    Room {
        /// The room name.
        name: String,
    },
    /// The lobby listing.
    Rooms(Vec<OpenRoom>),
    /// The report was recorded.
    Ok,
}

/// One lobby row: what `GET /lobby` returns.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct OpenRoom {
    /// The room name.
    pub name: String,
    /// Seated players.
    pub players: u8,
    /// Seats in total.
    pub capacity: u8,
    /// The room's `ctx.set_metadata` value.
    pub metadata: Value,
}

/// The matchmaker service. All data lives in SQL, so the struct itself is
/// stateless and cheap to rebuild after hibernation.
#[derive(Default)]
pub struct MatchmakerService;

#[derive(Default, Serialize, Deserialize)]
/// The matchmaker's (empty) object state: everything lives in its SQL tables.
pub struct MatchmakerState {}

impl Callable for MatchmakerService {
    const BINDING: &'static str = BINDING;
    type Rpc = MmRpc;
    type Reply = MmReply;
}

impl Service for MatchmakerService {
    type State = MatchmakerState;
    type Rpc = MmRpc;
    type Reply = MmReply;

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &Target,
        rpc: MmRpc,
    ) -> Result<MmReply> {
        let instance = ctx.me().name.clone();
        let sql = ctx.sql();
        let now = ctx.now_ms();
        create_tables(sql)?;
        match rpc {
            MmRpc::QuickJoin {
                player,
                pool,
                capacity,
            } => Ok(MmReply::Room {
                name: quick_join(sql, &instance, &player, &pool, capacity, now)?,
            }),
            MmRpc::List { pool, limit } => Ok(MmReply::Rooms(list(sql, &pool, limit, now)?)),
            MmRpc::Report(report) => {
                apply_report(sql, &report, now)?;
                Ok(MmReply::Ok)
            }
        }
    }
}

/// Declare the `HearthMatchmaker` Durable Object class in the calling crate.
///
/// ```ignore
/// hearth::matchmaker::matchmaker_object!();
/// ```
///
/// Then bind it in `wrangler.toml`:
///
/// ```toml
/// [[durable_objects.bindings]]
/// name = "HEARTH_MATCHMAKER"
/// class_name = "HearthMatchmaker"
/// ```
#[macro_export]
macro_rules! matchmaker_object {
    () => {
        // `#[worker::durable_object]`'s wasm-bindgen struct-marker expansion
        // spells the crate as bare `wasm_bindgen`, so the wrapper lives in a
        // hidden module that puts that name in scope.
        #[doc(hidden)]
        pub mod __hearth_matchmaker_object {
            #[allow(unused_imports)]
            use ::worker::wasm_bindgen;

            #[::worker::durable_object]
            pub struct HearthMatchmaker(
                ::hearth::server::worker::ServiceObject<$crate::MatchmakerService>,
            );

            impl ::worker::DurableObject for HearthMatchmaker {
                fn new(state: ::worker::State, env: ::worker::Env) -> Self {
                    Self(::hearth::server::worker::ServiceObject::<
                        $crate::MatchmakerService,
                    >::new(state, env, $crate::BINDING))
                }

                async fn fetch(
                    &self,
                    req: ::worker::Request,
                ) -> ::worker::Result<::worker::Response> {
                    self.0.fetch(req).await
                }

                async fn alarm(&self) -> ::worker::Result<::worker::Response> {
                    self.0.alarm().await?;
                    ::worker::Response::empty()
                }
            }
        }

        pub use self::__hearth_matchmaker_object::HearthMatchmaker;
    };
}

/// One room the matchmaker knows about.
#[derive(Clone, Debug)]
struct Room {
    name: String,
    capacity: u8,
    players: Vec<PlayerId>,
    locked: bool,
    metadata: Value,
    minted_ms: u64,
}

impl Room {
    /// Seats taken, including unexpired reservations for players who are not
    /// seated yet.
    fn used(&self, reservations: u32) -> u32 {
        self.players.len() as u32 + reservations
    }

    fn free(&self, reservations: u32) -> u32 {
        u32::from(self.capacity).saturating_sub(self.used(reservations))
    }
}

fn create_tables(sql: &dyn Sql) -> Result<()> {
    sql.exec(CREATE_ROOMS, Vec::new())?;
    sql.exec(CREATE_RESERVATIONS, Vec::new())?;
    Ok(())
}

/// The room instance name is `"{room_type}:{region}"`.
fn parse_instance(instance: &str) -> (&str, &str) {
    instance.split_once(':').unwrap_or((instance, "any"))
}

/// Seat `player` in the fullest open room of `pool`, minting one when none has
/// a free seat. The reservation is what keeps two players from taking the last
/// seat: it counts as used before any report arrives.
fn quick_join(
    sql: &dyn Sql,
    instance: &str,
    player: &PlayerId,
    pool: &str,
    capacity: u8,
    now: u64,
) -> Result<String> {
    let (_, region) = parse_instance(instance);
    // Already seated anywhere (the report may lag the actual join).
    if let Some(room) = room_with_player(sql, player)? {
        return Ok(room);
    }
    // An unexpired reservation wins, so a player asking twice gets one room —
    // unless that room filled up without them, because then the promised seat
    // is gone and a retried `/match` must not be handed the same full room.
    if let Some(room) = reservation(sql, player, now)? {
        if still_has_seat(sql, &room, now)? {
            return Ok(room);
        }
        sql.exec(
            "DELETE FROM mm_reservations WHERE player_id = ?",
            vec![SqlValue::Text(player.0.clone())],
        )?;
    }

    let rooms = open_rooms(sql, pool, now)?;
    // Fill rooms first: the fewest free seats wins, then the oldest room.
    let mut candidates: Vec<(u32, u64, String)> = Vec::new();
    for room in rooms {
        let reserved = reservations_for(sql, &room.name, &room.players, now)?;
        if room.free(reserved) > 0 {
            candidates.push((room.free(reserved), room.minted_ms, room.name));
        }
    }
    candidates.sort();

    let name = match candidates.first() {
        Some((_, _, name)) => name.clone(),
        None => {
            let name = mint_name(region);
            sql.exec(
                "INSERT INTO mm_rooms (name, pool, capacity, players, locked, metadata, minted_ms, updated_ms) \
                 VALUES (?, ?, ?, ?, 0, ?, ?, ?)",
                vec![
                    SqlValue::Text(name.clone()),
                    SqlValue::Text(pool.to_owned()),
                    SqlValue::Int(i64::from(capacity)),
                    SqlValue::Text("[]".to_owned()),
                    SqlValue::Text("null".to_owned()),
                    SqlValue::Int(now as i64),
                    SqlValue::Int(now as i64),
                ],
            )?;
            name
        }
    };
    reserve(sql, player, &name, now)?;
    Ok(name)
}

/// The lobby: open rooms in `pool` with a free seat, ordered the way
/// `QuickJoin` fills them.
fn list(sql: &dyn Sql, pool: &str, limit: u32, now: u64) -> Result<Vec<OpenRoom>> {
    let limit = limit.clamp(1, 50);
    let rooms = open_rooms(sql, pool, now)?;
    let mut rows: Vec<(u32, u64, OpenRoom)> = Vec::new();
    for room in rooms {
        let reserved = reservations_for(sql, &room.name, &room.players, now)?;
        if room.locked || room.free(reserved) == 0 {
            continue;
        }
        rows.push((
            room.free(reserved),
            room.minted_ms,
            OpenRoom {
                name: room.name.clone(),
                players: room.players.len() as u8,
                capacity: room.capacity,
                metadata: room.metadata.clone(),
            },
        ));
    }
    rows.sort_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
    Ok(rows
        .into_iter()
        .take(limit as usize)
        .map(|(_, _, row)| row)
        .collect())
}

/// Apply one room report.
///
/// A report for a room this matchmaker never minted is ignored: `m_` names are
/// guessable, so a player could otherwise invent a fake lobby row.
fn apply_report(sql: &dyn Sql, report: &RoomReport, now: u64) -> Result<()> {
    match report {
        RoomReport::Open {
            room,
            players,
            capacity,
            locked,
            metadata,
            ..
        }
        | RoomReport::Players {
            room,
            players,
            capacity,
            locked,
            metadata,
        } => {
            let known = sql.exec(
                "SELECT name FROM mm_rooms WHERE name = ?",
                vec![SqlValue::Text(room.name.clone())],
            )?;
            if known.is_empty() {
                return Ok(());
            }
            sql.exec(
                "UPDATE mm_rooms SET capacity = ?, players = ?, locked = ?, metadata = ?, updated_ms = ? \
                 WHERE name = ?",
                vec![
                    SqlValue::Int(i64::from(*capacity)),
                    SqlValue::Text(serde_json::to_string(players)?),
                    SqlValue::Int(i64::from(*locked)),
                    SqlValue::Text(serde_json::to_string(metadata)?),
                    SqlValue::Int(now as i64),
                    SqlValue::Text(room.name.clone()),
                ],
            )?;
        }
        RoomReport::Disposed { room } => {
            sql.exec(
                "DELETE FROM mm_rooms WHERE name = ?",
                vec![SqlValue::Text(room.name.clone())],
            )?;
            sql.exec(
                "DELETE FROM mm_reservations WHERE room = ?",
                vec![SqlValue::Text(room.name.clone())],
            )?;
        }
        // Report kinds added by a later hearth-server release.
        _ => {}
    }
    Ok(())
}

fn room_with_player(sql: &dyn Sql, player: &PlayerId) -> Result<Option<String>> {
    let rows = sql.exec("SELECT name, players FROM mm_rooms", Vec::new())?;
    for row in rows {
        let (Some(name), Some(players)) = (
            row.get("name").and_then(Value::as_str),
            row.get("players").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Ok(seated) = serde_json::from_str::<Vec<PlayerId>>(players) else {
            continue;
        };
        if seated.iter().any(|seated| seated == player) {
            return Ok(Some(name.to_owned()));
        }
    }
    Ok(None)
}

/// The player's unexpired reservation, dropping stale ones.
fn reservation(sql: &dyn Sql, player: &PlayerId, now: u64) -> Result<Option<String>> {
    let rows = sql.exec(
        "SELECT room, until_ms FROM mm_reservations WHERE player_id = ?",
        vec![SqlValue::Text(player.0.clone())],
    )?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let until_ms = row.get("until_ms").and_then(Value::as_u64).unwrap_or(0);
    let Some(room) = row.get("room").and_then(Value::as_str) else {
        return Ok(None);
    };
    if until_ms > now {
        return Ok(Some(room.to_owned()));
    }
    sql.exec(
        "DELETE FROM mm_reservations WHERE player_id = ?",
        vec![SqlValue::Text(player.0.clone())],
    )?;
    Ok(None)
}

/// Whether a reserved room can still seat its reservee. A vanished or now
/// locked row counts as no seat, so the player is matched again instead.
fn still_has_seat(sql: &dyn Sql, room: &str, now: u64) -> Result<bool> {
    let rows = sql.exec(
        "SELECT capacity, players, locked FROM mm_rooms WHERE name = ?",
        vec![SqlValue::Text(room.to_owned())],
    )?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(false);
    };
    let (Some(capacity), Some(players), Some(locked)) = (
        row.get("capacity").and_then(Value::as_u64),
        row.get("players").and_then(Value::as_str),
        row.get("locked").and_then(Value::as_i64),
    ) else {
        return Ok(false);
    };
    let seated: Vec<PlayerId> = serde_json::from_str(players).unwrap_or_default();
    let used = seated.len() as u32 + reservations_for(sql, room, &seated, now)?;
    Ok(locked == 0 && used < capacity as u32)
}

fn reserve(sql: &dyn Sql, player: &PlayerId, room: &str, now: u64) -> Result<()> {
    sql.exec(
        "INSERT OR REPLACE INTO mm_reservations (player_id, room, until_ms) VALUES (?, ?, ?)",
        vec![
            SqlValue::Text(player.0.clone()),
            SqlValue::Text(room.to_owned()),
            SqlValue::Int(now.saturating_add(RESERVATION_MS) as i64),
        ],
    )?;
    Ok(())
}

/// Unexpired reservations for a room, minus players already seated in it.
fn reservations_for(sql: &dyn Sql, room: &str, seated: &[PlayerId], now: u64) -> Result<u32> {
    let rows = sql.exec(
        "SELECT player_id FROM mm_reservations WHERE room = ? AND until_ms > ?",
        vec![SqlValue::Text(room.to_owned()), SqlValue::Int(now as i64)],
    )?;
    let mut count = 0;
    for row in rows {
        let Some(player) = row.get("player_id").and_then(Value::as_str) else {
            continue;
        };
        if !seated.iter().any(|seated| seated.0 == player) {
            count += 1;
        }
    }
    Ok(count)
}

/// Rooms a player can still be seated in: open, unlocked, in `pool`.
fn open_rooms(sql: &dyn Sql, pool: &str, _now: u64) -> Result<Vec<Room>> {
    let rows = sql.exec(
        "SELECT name, pool, capacity, players, locked, metadata, minted_ms FROM mm_rooms \
         WHERE pool = ? AND locked = 0 ORDER BY minted_ms, name",
        vec![SqlValue::Text(pool.to_owned())],
    )?;
    let mut rooms = Vec::with_capacity(rows.len());
    for row in rows {
        let (
            Some(name),
            Some(_pool),
            Some(capacity),
            Some(players),
            Some(locked),
            Some(metadata),
            Some(minted_ms),
        ) = (
            row.get("name").and_then(Value::as_str),
            row.get("pool").and_then(Value::as_str),
            row.get("capacity").and_then(Value::as_u64),
            row.get("players").and_then(Value::as_str),
            row.get("locked").and_then(Value::as_i64),
            row.get("metadata").and_then(Value::as_str),
            row.get("minted_ms").and_then(Value::as_u64),
        )
        else {
            continue;
        };
        rooms.push(Room {
            name: name.to_owned(),
            capacity: capacity as u8,
            players: serde_json::from_str(players).unwrap_or_default(),
            locked: locked != 0,
            metadata: serde_json::from_str(metadata).unwrap_or(Value::Null),
            minted_ms,
        });
    }
    Ok(rooms)
}

/// `m_<region>_<16 hex>`, the name form every matchmaker room uses.
fn mint_name(region: &str) -> String {
    let mut bytes = [0u8; 8];
    // `getrandom` on wasm uses the JS crypto source configured in `worker`.
    let _ = getrandom::getrandom(&mut bytes);
    let mut hex = String::with_capacity(16);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("m_{region}_{hex}")
}

#[doc(hidden)]
/// The name form `mm_room` recognises; used by the gateway.
pub fn parse_room_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("m_")?;
    let (region, hex) = rest.rsplit_once('_')?;
    if hex.len() != 16
        || !hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !matches!(
            region,
            "enam" | "sam" | "weur" | "apac" | "oc" | "afr" | "any"
        )
    {
        return None;
    }
    Some((region, hex))
}
