//! The room registry: one Durable Object holding a SQLite table of live rooms.
//!
//! [`Registry`] is plain logic over [`Sql`], so it is tested natively against
//! `hearth_test::MemHost`; [`RegistryObject`] wraps it in the Durable Object
//! that rooms `POST /report` to and the admin API `GET /list`s.

use std::rc::Rc;

use hearth_server::worker::{ServiceObject, WorkerSql};
use hearth_server::{
    Callable, Result, RoomName, RoomReport, Service, ServiceCtx, Sql, SqlValue, Target,
};
use serde::{Deserialize, Serialize};
use worker::{Env, Method, Request, Response, State};

const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS hearth_rooms (
    room_type TEXT NOT NULL,
    name TEXT NOT NULL,
    players INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (room_type, name)
)";

const DELETE_ROOM: &str = "DELETE FROM hearth_rooms WHERE room_type = ? AND name = ?";

const UPSERT_ROOM: &str = "INSERT INTO hearth_rooms
        (room_type, name, players, created_ms, updated_ms)
    VALUES (?, ?, ?, ?, ?)
    ON CONFLICT (room_type, name) DO UPDATE SET
        players = excluded.players,
        updated_ms = excluded.updated_ms";

/// The page-size ceiling: one admin request can never read the whole table.
const MAX_PAGE: u32 = 100;

/// One live room, as the registry remembers it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RoomRow {
    pub room_type: String,
    pub name: String,
    pub players: u8,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// Room bookkeeping over any [`Sql`] (the registry Durable Object's SQLite in
/// production, `MemHost` in tests).
pub struct Registry<'a> {
    sql: &'a dyn Sql,
}

impl<'a> Registry<'a> {
    /// Open a registry, creating its table if this is the first use.
    pub fn new(sql: &'a dyn Sql) -> Self {
        // The DDL is fixed and `IF NOT EXISTS`; a failure means storage itself
        // is broken, and the next event retries after the object resets.
        sql.exec(CREATE_TABLE, Vec::new())
            .expect("the registry table is creatable");
        Self { sql }
    }

    /// Apply one room report. `Open` keeps the row's original `created_ms`;
    /// `Players` inserts a row when the `Open` never arrived (or was lost);
    /// `Disposed` deletes it.
    pub fn apply(&self, report: &RoomReport, now_ms: u64) -> Result<()> {
        match report {
            RoomReport::Open {
                room,
                created_ms,
                players,
            } => self.upsert(room, *players, *created_ms, now_ms),
            RoomReport::Players { room, players } => self.upsert(room, *players, now_ms, now_ms),
            RoomReport::Disposed { room } => {
                self.sql.exec(
                    DELETE_ROOM,
                    vec![
                        SqlValue::Text(room.room_type.clone()),
                        SqlValue::Text(room.name.clone()),
                    ],
                )?;
                Ok(())
            }
        }
    }

    /// One page of rooms ordered by `(room_type, name)`, plus the cursor for the
    /// next page. The cursor is only returned when at least one more row exists.
    pub fn list(
        &self,
        room_type: Option<&str>,
        after: Option<&RoomName>,
        limit: u32,
    ) -> Result<(Vec<RoomRow>, Option<RoomName>)> {
        let limit = limit.clamp(1, MAX_PAGE);
        let text = |value: Option<&str>| match value {
            Some(value) => SqlValue::Text(value.to_owned()),
            None => SqlValue::Null,
        };
        let after_type = text(after.map(|room| room.room_type.as_str()));
        let after_name = text(after.map(|room| room.name.as_str()));

        let mut rows = self.sql.exec(
            "SELECT room_type, name, players, created_ms, updated_ms
             FROM hearth_rooms
             WHERE (? IS NULL OR room_type = ?)
               AND (? IS NULL OR room_type > ? OR (room_type = ? AND name > ?))
             ORDER BY room_type, name
             LIMIT ?",
            vec![
                text(room_type),
                text(room_type),
                after_type.clone(),
                after_type.clone(),
                after_type,
                after_name,
                // One extra row tells us whether a next page exists.
                SqlValue::Int(i64::from(limit) + 1),
            ],
        )?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let rooms = rows
            .into_iter()
            .map(row_from_json)
            .collect::<Result<Vec<RoomRow>>>()?;
        let next = more.then(|| rooms.last()).flatten().map(|row| RoomName {
            room_type: row.room_type.clone(),
            name: row.name.clone(),
        });
        Ok((rooms, next))
    }

    fn upsert(&self, room: &RoomName, players: u8, created_ms: u64, now_ms: u64) -> Result<()> {
        self.sql.exec(
            UPSERT_ROOM,
            vec![
                SqlValue::Text(room.room_type.clone()),
                SqlValue::Text(room.name.clone()),
                SqlValue::Int(i64::from(players)),
                SqlValue::Int(created_ms as i64),
                SqlValue::Int(now_ms as i64),
            ],
        )?;
        Ok(())
    }
}

/// SQLite values surface JSON-shaped, so a row deserialises like any other JSON
/// object — including the `u8` range check on `players`.
fn row_from_json(fields: serde_json::Map<String, serde_json::Value>) -> Result<RoomRow> {
    Ok(serde_json::from_value(serde_json::Value::Object(fields))?)
}

/// The room registry as a Hearth [`Service`]: rooms `ctx.notify` their
/// [`RoomReport`]s to it through the durable outbox, and `on_call` applies them
/// to the registry table.
#[derive(Default)]
pub struct RegistryService;

/// The service's own state is empty: all bookkeeping lives in SQL, so the
/// registry table stays queryable by the admin listing.
#[derive(Default, Serialize, Deserialize)]
pub struct RegistryState {}

impl Callable for RegistryService {
    const BINDING: &'static str = "HEARTH_REGISTRY";
    type Rpc = RoomReport;
    type Reply = ();
}

impl Service for RegistryService {
    type State = RegistryState;
    type Rpc = RoomReport;
    type Reply = ();

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &Target,
        report: RoomReport,
    ) -> Result<()> {
        Registry::new(ctx.sql()).apply(&report, ctx.now_ms())?;
        Ok(())
    }
}

/// The Durable Object the rooms report to. One for every room in the
/// deployment: `POST /report` with a [`RoomReport`] body, `GET /list` with
/// `type`, `after_type`, `after_name` and `limit` query parameters.
// shortcut: single registry DO (~1k writes/s); shard by hash(room_type, name) when a game outgrows it
pub struct RegistryObject {
    service: ServiceObject<RegistryService>,
    sql: WorkerSql,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(rename = "type")]
    room_type: Option<String>,
    after_type: Option<String>,
    after_name: Option<String>,
    limit: Option<u32>,
}

#[derive(Serialize)]
struct ListResponse {
    rooms: Vec<RoomRow>,
    next: Option<RoomName>,
}

impl RegistryObject {
    /// Must be called by the generated Durable Object wrapper's `new`.
    pub fn new(state: State, env: Env) -> Self {
        let state = Rc::new(state);
        let sql = WorkerSql::new(Rc::clone(&state));
        let service = ServiceObject::<RegistryService>::new_shared(
            Rc::clone(&state),
            env,
            RegistryService::BINDING,
        );
        // Create the table now so the first read cannot fail on a missing table.
        Registry::new(&sql);
        Self { service, sql }
    }

    pub async fn fetch(&self, req: Request) -> worker::Result<Response> {
        // `/list` is the admin listing; everything else is the service path
        // (`/__hearth/call`, `/__hearth/inspect`).
        if req.path() == "/list" {
            return self.list(req).await;
        }
        self.service.fetch(req).await
    }

    async fn list(&self, req: Request) -> worker::Result<Response> {
        if req.method() != Method::Get {
            return Response::error("not found", 404);
        }
        let Ok(query) = req.query::<ListQuery>() else {
            return Response::error("invalid query", 400);
        };
        let room_type = query
            .room_type
            .as_deref()
            .filter(|room_type| !room_type.is_empty());
        let after = match (query.after_type.as_deref(), query.after_name.as_deref()) {
            (None, None) => None,
            (Some(room_type), Some(name)) => Some(RoomName {
                room_type: room_type.to_owned(),
                name: name.to_owned(),
            }),
            _ => return Response::error("after_type and after_name must be passed together", 400),
        };
        let registry = Registry::new(&self.sql);
        match registry.list(room_type, after.as_ref(), query.limit.unwrap_or(50)) {
            Ok((rooms, next)) => Response::from_json(&ListResponse { rooms, next }),
            Err(err) => Response::error(format!("registry read failed: {err:#}"), 500),
        }
    }

    pub async fn alarm(&self) -> worker::Result<()> {
        self.service.alarm().await
    }
}
