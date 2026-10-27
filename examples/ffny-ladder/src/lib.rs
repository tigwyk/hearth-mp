//! Hearth sample: a ranked ladder for a peer-to-peer game mod.
//!
//! The fights themselves run on the players' machines (or any relay); this Worker
//! only does what a mod needs a server for: a ranked queue, pairing, agreed-result
//! settlement with Glicko-2, and a leaderboard. All of it is one `Ladder` service
//! (a single Durable Object, SQLite inside), so it costs nothing while nobody plays.
//!
//! Routes (bearer token from `POST /auth/anonymous` or any other login):
//! `POST /ladder/search` (call every few seconds until `matched`), `DELETE /ladder/search`,
//! `POST /ladder/report {pairing, result}`, `GET /ladder/me`, `GET /ladder/top?limit=`
//! (the last one needs no token).

pub mod rating;

use hearth_mp::server::{Sql, SqlValue as V};
use hearth_mp::{anyhow, Callable, Envelope, PlayerId, Result, Service, ServiceCtx, Target};
use rating::Rating;
use serde::{Deserialize, Serialize};
use worker::{Context, Env, Request, Response};

/// A search that has not been polled for this long has left.
const QUEUE_STALE_MS: i64 = 15_000;
/// A pairing waits this long for both reports.
const PAIRING_MS: i64 = 3 * 60 * 60 * 1000;
/// The rating window opens at `WINDOW_START` points and widens by `WINDOW_PER_S` per second.
const WINDOW_START: f64 = 100.0;
const WINDOW_PER_S: f64 = 25.0;
const WINDOW_MAX: f64 = 1000.0;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Win,
    Loss,
    Draw,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Win => "win",
            Outcome::Loss => "loss",
            Outcome::Draw => "draw",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub player: String,
    pub name: String,
    pub rating: i32,
    pub deviation: i32,
    pub games: i64,
    pub wins: i64,
    pub losses: i64,
    pub draws: i64,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum LadderRpc {
    Search { player: PlayerId, name: String },
    Cancel { player: PlayerId },
    Report { player: PlayerId, pairing: String, result: Outcome },
    Me { player: PlayerId },
    Top { limit: u32 },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum LadderReply {
    Searching { waited_s: u64, window: u32 },
    Matched { pairing: String, opponent: Entry },
    Cancelled,
    /// `settled`: both reports are in. `counted`: they agreed and the ratings moved.
    Reported { settled: bool, counted: bool },
    Me { entry: Option<Entry> },
    Top { entries: Vec<Entry> },
}

#[derive(Default)]
pub struct Ladder;

type Row = serde_json::Map<String, serde_json::Value>;

fn s(row: &Row, k: &str) -> String {
    row.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned()
}
fn i(row: &Row, k: &str) -> i64 {
    row.get(k).and_then(|v| v.as_i64()).unwrap_or(0)
}
fn f(row: &Row, k: &str) -> f64 {
    row.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

fn entry(row: &Row) -> Entry {
    Entry {
        player: s(row, "id"),
        name: s(row, "name"),
        rating: f(row, "rating").round() as i32,
        deviation: f(row, "dev").round() as i32,
        games: i(row, "games"),
        wins: i(row, "wins"),
        losses: i(row, "losses"),
        draws: i(row, "draws"),
    }
}

fn rating_of(row: &Row) -> Rating {
    Rating { rating: f(row, "rating"), deviation: f(row, "dev"), volatility: f(row, "vol") }
}

const SCHEMA: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS players (id TEXT PRIMARY KEY, name TEXT NOT NULL, rating REAL NOT NULL, \
     dev REAL NOT NULL, vol REAL NOT NULL, games INTEGER NOT NULL DEFAULT 0, wins INTEGER NOT NULL DEFAULT 0, \
     losses INTEGER NOT NULL DEFAULT 0, draws INTEGER NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS queue (player TEXT PRIMARY KEY, since INTEGER NOT NULL, seen INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS pairings (id TEXT PRIMARY KEY, a TEXT NOT NULL, b TEXT NOT NULL, \
     a_got INTEGER NOT NULL DEFAULT 0, b_got INTEGER NOT NULL DEFAULT 0, ra TEXT, rb TEXT, \
     at INTEGER NOT NULL, done INTEGER NOT NULL DEFAULT 0)",
];

#[hearth_mp::service(binding = "LADDER")]
impl Service for Ladder {
    type State = ();
    type Rpc = LadderRpc;
    type Reply = LadderReply;

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &Target,
        rpc: LadderRpc,
    ) -> Result<LadderReply> {
        let now = ctx.now_ms() as i64;
        let sql = ctx.sql();
        for ddl in SCHEMA {
            sql.exec(ddl, vec![])?;
        }
        match rpc {
            LadderRpc::Top { limit } => {
                let rows = sql.exec(
                    "SELECT * FROM players WHERE games > 0 ORDER BY rating DESC LIMIT ?",
                    vec![V::Int(limit.clamp(1, 100) as i64)],
                )?;
                Ok(LadderReply::Top { entries: rows.iter().map(entry).collect() })
            }
            LadderRpc::Me { player } => {
                let rows = sql.exec("SELECT * FROM players WHERE id = ?", vec![V::Text(player.0)])?;
                Ok(LadderReply::Me { entry: rows.first().map(entry) })
            }
            LadderRpc::Cancel { player } => {
                sql.exec("DELETE FROM queue WHERE player = ?", vec![V::Text(player.0)])?;
                Ok(LadderReply::Cancelled)
            }
            LadderRpc::Search { player, name } => search(sql, now, player.0, name),
            LadderRpc::Report { player, pairing, result } => report(sql, player.0, pairing, result),
        }
    }
}

fn search(sql: &dyn Sql, now: i64, me: String, name: String) -> Result<LadderReply> {
    sql.exec("DELETE FROM queue WHERE seen < ?", vec![V::Int(now - QUEUE_STALE_MS)])?;
    sql.exec("DELETE FROM pairings WHERE at < ?", vec![V::Int(now - PAIRING_MS)])?;
    sql.exec(
        "INSERT INTO players (id, name, rating, dev, vol) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET name = excluded.name",
        vec![
            V::Text(me.clone()),
            V::Text(name),
            V::Real(rating::START),
            V::Real(rating::DEVIATION_START),
            V::Real(rating::VOLATILITY_START),
        ],
    )?;
    // A pairing made by the other side's poll is handed over once.
    let pending = sql.exec(
        "SELECT id, CASE WHEN a = ?1 THEN b ELSE a END AS other FROM pairings \
         WHERE done = 0 AND ((a = ?1 AND a_got = 0) OR (b = ?1 AND b_got = 0))",
        vec![V::Text(me.clone())],
    )?;
    if let Some(p) = pending.first() {
        let (id, other) = (s(p, "id"), s(p, "other"));
        sql.exec(
            "UPDATE pairings SET a_got = a_got OR a = ?2, b_got = b_got OR b = ?2 WHERE id = ?1",
            vec![V::Text(id.clone()), V::Text(me)],
        )?;
        return Ok(LadderReply::Matched { pairing: id, opponent: entry(&player_row(sql, &other)?) });
    }
    sql.exec(
        "INSERT INTO queue (player, since, seen) VALUES (?1, ?2, ?2) \
         ON CONFLICT(player) DO UPDATE SET seen = excluded.seen",
        vec![V::Text(me.clone()), V::Int(now)],
    )?;
    let since = i(&sql.exec("SELECT since FROM queue WHERE player = ?", vec![V::Text(me.clone())])?[0], "since");
    let waited_s = ((now - since).max(0) / 1000) as u64;
    let window = (WINDOW_START + WINDOW_PER_S * waited_s as f64).min(WINDOW_MAX);
    let mine = f(&player_row(sql, &me)?, "rating");
    let found = sql.exec(
        "SELECT q.player AS id FROM queue q JOIN players p ON p.id = q.player \
         WHERE q.player != ?1 AND ABS(p.rating - ?2) <= ?3 \
         ORDER BY ABS(p.rating - ?2), q.since LIMIT 1",
        vec![V::Text(me.clone()), V::Real(mine), V::Real(window)],
    )?;
    let Some(other) = found.first().map(|r| s(r, "id")) else {
        return Ok(LadderReply::Searching { waited_s, window: window as u32 });
    };
    let id = format!("{now:x}-{}", &me[me.len().saturating_sub(6)..]);
    sql.exec(
        "INSERT INTO pairings (id, a, b, a_got, at) VALUES (?, ?, ?, 1, ?)",
        vec![V::Text(id.clone()), V::Text(me.clone()), V::Text(other.clone()), V::Int(now)],
    )?;
    sql.exec("DELETE FROM queue WHERE player IN (?, ?)", vec![V::Text(me), V::Text(other.clone())])?;
    Ok(LadderReply::Matched { pairing: id, opponent: entry(&player_row(sql, &other)?) })
}

/// A ranked game counts only when both paired players report it and the reports agree.
fn report(sql: &dyn Sql, me: String, pairing: String, result: Outcome) -> Result<LadderReply> {
    let rows = sql.exec(
        "SELECT * FROM pairings WHERE id = ? AND done = 0 AND (a = ? OR b = ?)",
        vec![V::Text(pairing.clone()), V::Text(me.clone()), V::Text(me.clone())],
    )?;
    let Some(p) = rows.first() else {
        anyhow::bail!("no open pairing {pairing} for this player");
    };
    let col = if s(p, "a") == me { "ra" } else { "rb" };
    if !s(p, col).is_empty() {
        return Ok(LadderReply::Reported { settled: false, counted: false });
    }
    sql.exec(
        &format!("UPDATE pairings SET {col} = ? WHERE id = ?"),
        vec![V::Text(result.as_str().to_owned()), V::Text(pairing.clone())],
    )?;
    let p = sql.exec("SELECT * FROM pairings WHERE id = ?", vec![V::Text(pairing.clone())])?.remove(0);
    let (ra, rb) = (s(&p, "ra"), s(&p, "rb"));
    if ra.is_empty() || rb.is_empty() {
        return Ok(LadderReply::Reported { settled: false, counted: false });
    }
    let score_a = match (ra.as_str(), rb.as_str()) {
        ("win", "loss") => Some(1.0),
        ("loss", "win") => Some(0.0),
        ("draw", "draw") => Some(0.5),
        _ => None, // the games disagree: it counts for nobody
    };
    sql.exec("UPDATE pairings SET done = 1 WHERE id = ?", vec![V::Text(pairing)])?;
    if let Some(score_a) = score_a {
        settle(sql, &s(&p, "a"), &s(&p, "b"), score_a)?;
    }
    Ok(LadderReply::Reported { settled: true, counted: score_a.is_some() })
}

fn player_row(sql: &dyn Sql, id: &str) -> Result<Row> {
    sql.exec("SELECT * FROM players WHERE id = ?", vec![V::Text(id.to_owned())])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("unknown player {id}"))
}

/// Move both ratings after one agreed game (each game its own rating period).
fn settle(sql: &dyn Sql, a: &str, b: &str, score_a: f64) -> Result<()> {
    let (ra, rb) = (player_row(sql, a)?, player_row(sql, b)?);
    let (na, nb) = Rating::game(rating_of(&ra), rating_of(&rb), score_a);
    for (id, r, score) in [(a, na, score_a), (b, nb, 1.0 - score_a)] {
        sql.exec(
            "UPDATE players SET rating = ?, dev = ?, vol = ?, games = games + 1, \
             wins = wins + ?, losses = losses + ?, draws = draws + ? WHERE id = ?",
            vec![
                V::Real(r.rating),
                V::Real(r.deviation),
                V::Real(r.volatility),
                V::Int((score == 1.0) as i64),
                V::Int((score == 0.0) as i64),
                V::Int((score == 0.5) as i64),
                V::Text(id.to_owned()),
            ],
        )?;
    }
    Ok(())
}

// ---- the Worker: auth, routing, and the calls into the Ladder object ----

async fn call(env: &Env, rpc: LadderRpc) -> std::result::Result<LadderReply, String> {
    let envelope = Envelope {
        msg_id: None,
        from: Target::new("gateway", ""),
        chain: Vec::new(),
        rpc: serde_json::to_value(rpc).map_err(|e| e.to_string())?,
    };
    let to = Target::new(Ladder::BINDING, "global");
    let value = hearth_mp::server::worker::call_object(env, &to, envelope)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn json_error(message: &str, status: u16) -> worker::Result<Response> {
    Response::from_json(&serde_json::json!({ "error": message })).map(|r| r.with_status(status))
}

fn reply(result: std::result::Result<LadderReply, String>) -> worker::Result<Response> {
    match result {
        Ok(r) => Response::from_json(&r),
        Err(e) => json_error(&e, 400),
    }
}

async fn ladder_route(mut req: Request, env: Env, path: &str) -> worker::Result<Response> {
    use worker::Method::{Delete, Get, Post};
    if (req.method(), path) == (Get, "top") {
        let limit = req
            .url()?
            .query_pairs()
            .find(|(k, _)| k == "limit")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(20);
        return reply(call(&env, LadderRpc::Top { limit }).await);
    }
    let secret = env.secret("HEARTH_SECRET")?.to_string();
    let token = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(str::to_owned));
    let now_s = worker::Date::now().as_millis() / 1000;
    let who = token
        .and_then(|t| hearth_mp_gateway::auth::verify_token(secret.as_bytes(), &t, now_s).ok());
    let Some(who) = who else {
        return json_error("missing or invalid bearer token", 401);
    };
    let player = who.player_id;
    let rpc = match (req.method(), path) {
        (Post, "search") => LadderRpc::Search {
            name: who.display_name.unwrap_or_else(|| "Player".into()),
            player,
        },
        (Delete, "search") => LadderRpc::Cancel { player },
        (Get, "me") => LadderRpc::Me { player },
        (Post, "report") => {
            #[derive(Deserialize)]
            struct Body {
                pairing: String,
                result: Outcome,
            }
            let Ok(body) = req.json::<Body>().await else {
                return json_error("body must be {pairing, result: win|loss|draw}", 400);
            };
            LadderRpc::Report { player, pairing: body.pairing, result: body.result }
        }
        _ => return json_error("not found", 404),
    };
    reply(call(&env, rpc).await)
}

// The one gateway feature the ladder needs is its login (`POST /auth/anonymous`).
fn gateway_config(env: &Env) -> std::result::Result<hearth_mp_gateway::GatewayConfig, String> {
    static NO_ROOMS: [hearth_mp_gateway::RoomRoute; 0] = [];
    Ok(hearth_mp_gateway::GatewayConfig::new(&NO_ROOMS)
        .auth(hearth_mp_gateway::AuthConfig::from_env(env)?))
}

#[worker::event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    if let Some(rest) = req.path().strip_prefix("/ladder/") {
        let rest = rest.to_owned();
        return ladder_route(req, env, &rest).await;
    }
    match gateway_config(&env) {
        Ok(config) => hearth_mp_gateway::serve(req, env, &config).await,
        Err(message) => Response::error(message, 500),
    }
}
