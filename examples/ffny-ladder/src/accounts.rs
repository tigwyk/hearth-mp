//! A drop-in for the FFNY account service (`netplay/relay/accounts`), ranked play only:
//! sign-in, session, one-time ranked tickets, the relay's redeem/pairing calls, match
//! reports that settle when both paired games agree, Glicko-2, and the leaderboard.
//!
//! One Durable Object (`"global"`), like the Go service's single process. Every reply is
//! the final HTTP answer (`status`, `kind`, `body`), so the Worker only forwards it.
//!
//! Sign-in keeps the game's handshake: the game opens `/auth/discord/start?login=<sha256 of
//! its secret>` and polls `/auth/poll {secret}`. Here the start page is a dev sign-in
//! (a name, no password), on only when the Worker has `DEV_LOGIN=1`. Discord OAuth would
//! replace that one page.

use hearth_mp::server::{Sql, SqlValue as V};
use hearth_mp::{anyhow, Result, Service, ServiceCtx, Target};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::rating::{self, Rating};

const SESSION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const TICKET_MS: i64 = 60 * 1000;
const LOGIN_MS: i64 = 10 * 60 * 1000;
const PAIRING_MS: i64 = 3 * 60 * 60 * 1000;
/// Matches that put an account on the leaderboard, and its page size (the game's constants).
const BOARD_MIN: i64 = 3;
const BOARD_PAGE: usize = 10;

#[derive(Serialize, Deserialize, Debug)]
pub enum AccRpc {
    /// `GET /auth/discord/start?login=`: the dev sign-in page.
    LoginPage { login: String, dev: bool },
    /// The dev sign-in form: bind `login` to a new account called `name`.
    DevComplete { login: String, name: String, dev: bool },
    /// `POST /auth/poll`.
    Poll { secret: String },
    Logout { token: String },
    Me { token: String },
    DeleteMe { token: String },
    Ticket { token: String, text: bool },
    Stats { token: String, page: usize },
    Board { discord: bool },
    /// `POST /api/matches`: the body as the game sent it.
    Report { token: String, body: String },
    /// The relay (internal): a ticket's account.
    Redeem { ticket: String },
    /// The relay (internal): a ranked match it started.
    Pair { body: String },
}

/// The HTTP answer: `kind` is `json`, `text` or `html`.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct AccReply {
    pub status: u16,
    pub kind: String,
    pub body: String,
}

fn json_reply(status: u16, v: Value) -> AccReply {
    AccReply { status, kind: "json".into(), body: v.to_string() }
}
fn err(status: u16, message: &str) -> AccReply {
    json_reply(status, json!({ "error": message }))
}
fn empty(status: u16) -> AccReply {
    AccReply { status, kind: "text".into(), body: String::new() }
}

#[derive(Default)]
pub struct Accounts;

type Row = serde_json::Map<String, Value>;

fn s(row: &Row, k: &str) -> String {
    row.get(k).and_then(Value::as_str).unwrap_or("").to_owned()
}
fn i(row: &Row, k: &str) -> i64 {
    row.get(k).and_then(Value::as_i64).unwrap_or(0)
}
fn f(row: &Row, k: &str) -> f64 {
    row.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

const SCHEMA: [&str; 7] = [
    "CREATE TABLE IF NOT EXISTS accounts (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
     rating REAL NOT NULL, dev REAL NOT NULL, vol REAL NOT NULL, games INTEGER NOT NULL DEFAULT 0, \
     wins INTEGER NOT NULL DEFAULT 0, losses INTEGER NOT NULL DEFAULT 0, draws INTEGER NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS sessions (token TEXT PRIMARY KEY, account INTEGER NOT NULL, exp INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS tickets (ticket TEXT PRIMARY KEY, account INTEGER NOT NULL, exp INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS logins (hash TEXT PRIMARY KEY, token TEXT, exp INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS pairings (key TEXT NOT NULL, slot INTEGER NOT NULL, account INTEGER NOT NULL, \
     at INTEGER NOT NULL, PRIMARY KEY (key, slot))",
    "CREATE TABLE IF NOT EXISTS reports (key TEXT NOT NULL, account INTEGER NOT NULL, you INTEGER NOT NULL, \
     lineup TEXT NOT NULL, winners TEXT NOT NULL, draw INTEGER NOT NULL, at INTEGER NOT NULL, \
     PRIMARY KEY (key, account))",
    "CREATE TABLE IF NOT EXISTS settled (key TEXT PRIMARY KEY, at INTEGER NOT NULL)",
];

#[hearth_mp::service(binding = "ACCOUNTS")]
impl Service for Accounts {
    type State = ();
    type Rpc = AccRpc;
    type Reply = AccReply;

    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        _from: &Target,
        rpc: AccRpc,
    ) -> Result<AccReply> {
        let now = ctx.now_ms() as i64;
        let sql = ctx.sql();
        for ddl in SCHEMA {
            sql.exec(ddl, vec![])?;
        }
        handle(sql, now, rpc)
    }
}

fn handle(sql: &dyn Sql, now: i64, rpc: AccRpc) -> Result<AccReply> {
    let signed_in = |token: &str| session(sql, token, now);
    Ok(match rpc {
        AccRpc::LoginPage { login, dev } => {
            if !dev || !is_hex(&login, 64) {
                return Ok(err(404, "sign-in is not available here"));
            }
            sql.exec(
                "INSERT OR IGNORE INTO logins (hash, exp) VALUES (?, ?)",
                vec![V::Text(login.clone()), V::Int(now + LOGIN_MS)],
            )?;
            AccReply { status: 200, kind: "html".into(), body: login_page(&login) }
        }
        AccRpc::DevComplete { login, name, dev } => {
            let name: String = name.chars().filter(|c| !c.is_control()).take(24).collect();
            if !dev || !is_hex(&login, 64) || name.trim().is_empty() {
                return Ok(err(400, "a name is required"));
            }
            let rows = sql.exec(
                "SELECT hash FROM logins WHERE hash = ? AND token IS NULL AND exp > ?",
                vec![V::Text(login.clone()), V::Int(now)],
            )?;
            if rows.is_empty() {
                return Ok(err(410, "the sign-in ran out; try again"));
            }
            sql.exec(
                "INSERT INTO accounts (name, rating, dev, vol) VALUES (?, ?, ?, ?)",
                vec![
                    V::Text(name.trim().to_owned()),
                    V::Real(rating::START),
                    V::Real(rating::DEVIATION_START),
                    V::Real(rating::VOLATILITY_START),
                ],
            )?;
            let id = i(&sql.exec("SELECT last_insert_rowid() AS id", vec![])?[0], "id");
            let token = random_hex(32)?;
            sql.exec(
                "INSERT INTO sessions (token, account, exp) VALUES (?, ?, ?)",
                vec![V::Text(token.clone()), V::Int(id), V::Int(now + SESSION_MS)],
            )?;
            sql.exec("UPDATE logins SET token = ? WHERE hash = ?", vec![V::Text(token), V::Text(login)])?;
            AccReply { status: 200, kind: "html".into(), body: done_page() }
        }
        AccRpc::Poll { secret } => {
            let hash = hex(&Sha256::digest(secret.as_bytes()));
            sql.exec("DELETE FROM logins WHERE exp < ?", vec![V::Int(now)])?;
            let rows = sql.exec("SELECT token FROM logins WHERE hash = ?", vec![V::Text(hash.clone())])?;
            let Some(row) = rows.first() else {
                return Ok(err(404, "no such sign-in"));
            };
            let token = s(row, "token");
            if token.is_empty() {
                return Ok(json_reply(202, json!({ "status": "waiting" })));
            }
            sql.exec("DELETE FROM logins WHERE hash = ?", vec![V::Text(hash)])?;
            let a = account_of_token(sql, &token)?;
            json_reply(200, json!({ "token": token, "name": s(&a, "name"), "username": s(&a, "name") }))
        }
        AccRpc::Logout { token } => {
            sql.exec("DELETE FROM sessions WHERE token = ?", vec![V::Text(token)])?;
            empty(204)
        }
        AccRpc::Me { token } => match signed_in(&token)? {
            Some(a) => json_reply(200, json!({ "id": i(&a, "id"), "name": s(&a, "name"), "username": s(&a, "name") })),
            None => err(401, "not signed in"),
        },
        AccRpc::DeleteMe { token } => match signed_in(&token)? {
            Some(a) => {
                let id = V::Int(i(&a, "id"));
                for table in ["sessions", "tickets", "reports"] {
                    sql.exec(&format!("DELETE FROM {table} WHERE account = ?"), vec![id.clone()])?;
                }
                sql.exec("DELETE FROM accounts WHERE id = ?", vec![id])?;
                empty(204)
            }
            None => err(401, "not signed in"),
        },
        AccRpc::Ticket { token, text } => match signed_in(&token)? {
            Some(a) => {
                let ticket = random_hex(16)?;
                sql.exec("DELETE FROM tickets WHERE exp < ?", vec![V::Int(now)])?;
                sql.exec(
                    "INSERT INTO tickets (ticket, account, exp) VALUES (?, ?, ?)",
                    vec![V::Text(ticket.clone()), V::Int(i(&a, "id")), V::Int(now + TICKET_MS)],
                )?;
                if text {
                    AccReply { status: 200, kind: "text".into(), body: format!("{ticket}\n") }
                } else {
                    json_reply(200, json!({ "ticket": ticket, "expires_in": TICKET_MS / 1000 }))
                }
            }
            None => err(401, "not signed in"),
        },
        AccRpc::Redeem { ticket } => {
            let rows = sql.exec(
                "SELECT a.* FROM tickets t JOIN accounts a ON a.id = t.account WHERE t.ticket = ? AND t.exp > ?",
                vec![V::Text(ticket.clone()), V::Int(now)],
            )?;
            // Used at once, redeemable only once.
            sql.exec("DELETE FROM tickets WHERE ticket = ?", vec![V::Text(ticket)])?;
            match rows.first() {
                Some(a) => json_reply(
                    200,
                    json!({ "account": i(a, "id"), "name": s(a, "name"), "rating": f(a, "rating"), "deviation": f(a, "dev") }),
                ),
                None => err(404, "no such ticket"),
            }
        }
        AccRpc::Pair { body } => pair(sql, now, &body)?,
        AccRpc::Board { discord: _ } => {
            let (board, _) = board_page(sql, 0, BOARD_PAGE)?;
            json_reply(200, json!({ "entries": board, "min_matches": BOARD_MIN }))
        }
        AccRpc::Stats { token, page } => match signed_in(&token)? {
            Some(a) => stats_text(sql, &a, page)?,
            None => err(401, "not signed in"),
        },
        AccRpc::Report { token, body } => match signed_in(&token)? {
            Some(a) => report(sql, now, i(&a, "id"), &body)?,
            None => err(401, "not signed in"),
        },
    })
}

fn session(sql: &dyn Sql, token: &str, now: i64) -> Result<Option<Row>> {
    Ok(sql
        .exec(
            "SELECT a.* FROM sessions s JOIN accounts a ON a.id = s.account WHERE s.token = ? AND s.exp > ?",
            vec![V::Text(token.to_owned()), V::Int(now)],
        )?
        .into_iter()
        .next())
}

fn account_of_token(sql: &dyn Sql, token: &str) -> Result<Row> {
    sql.exec(
        "SELECT a.* FROM sessions s JOIN accounts a ON a.id = s.account WHERE s.token = ?",
        vec![V::Text(token.to_owned())],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| anyhow::anyhow!("session without an account"))
}

// ---- leaderboard and stats (the game's text form) ----

/// The leaderboard's order: most wins, then the better win rate, then fewer matches.
fn ranked(sql: &dyn Sql) -> Result<Vec<Row>> {
    sql.exec(
        "SELECT * FROM accounts WHERE games >= ? \
         ORDER BY wins DESC, (wins * 1000 / MAX(1, wins + losses)) DESC, games ASC, id ASC",
        vec![V::Int(BOARD_MIN)],
    )
}

fn win_rate(r: &Row) -> i64 {
    let d = i(r, "wins") + i(r, "losses");
    if d > 0 {
        (i(r, "wins") * 100 + d / 2) / d
    } else {
        0
    }
}

fn board_page(sql: &dyn Sql, offset: usize, n: usize) -> Result<(Vec<Value>, usize)> {
    let list = ranked(sql)?;
    let page = list
        .iter()
        .enumerate()
        .skip(offset)
        .take(n)
        .map(|(idx, r)| {
            json!({ "rank": idx + 1, "name": s(r, "name"), "wins": i(r, "wins"), "losses": i(r, "losses"),
                    "matches": i(r, "games"), "win_rate": win_rate(r) })
        })
        .collect();
    Ok((page, list.len()))
}

fn clean(text: &str) -> String {
    text.chars().map(|c| if matches!(c, '|' | '\n' | '\r') { ' ' } else { c }).collect()
}

/// One `key=value` per line, as `StatsText` in the Go service writes it. No match history
/// here (`recent`), and no favorite fighter: this service sees only ranked games.
fn stats_text(sql: &dyn Sql, a: &Row, page: usize) -> Result<AccReply> {
    let list = ranked(sql)?;
    let rank = list.iter().position(|r| i(r, "id") == i(a, "id")).map_or(0, |p| p + 1);
    let (board, total) = board_page(sql, page * BOARD_PAGE, BOARD_PAGE)?;
    let pages = total.div_ceil(BOARD_PAGE).max(1);
    let mut out = String::new();
    let mut line = |k: &str, v: String| out.push_str(&format!("{k}={v}\n"));
    line("name", clean(&s(a, "name")));
    line("matches", i(a, "games").to_string());
    line("wins", i(a, "wins").to_string());
    line("losses", i(a, "losses").to_string());
    line("draws", i(a, "draws").to_string());
    line("win_rate", win_rate(a).to_string());
    line("streak", "0".into());
    line("best_streak", "0".into());
    line("rank", rank.to_string());
    line("min", BOARD_MIN.to_string());
    line("favorite", "-1".into());
    line("favorite_name", String::new());
    if i(a, "games") > 0 {
        line(
            "ranked",
            format!(
                "{}|{}|{}|{}|{}",
                f(a, "rating").round() as i64,
                i(a, "games"),
                i(a, "wins"),
                i(a, "losses"),
                i(a, "draws")
            ),
        );
    }
    for e in &board {
        line(
            "board",
            format!("{}|{}|{}|{}|{}", e["rank"], clean(e["name"].as_str().unwrap_or("")), e["wins"], e["losses"], e["win_rate"]),
        );
    }
    let page = page.min(pages - 1);
    out.push_str(&format!("board_page={page}\nboard_pages={pages}\nboard_total={total}\n"));
    Ok(AccReply { status: 200, kind: "text".into(), body: out })
}

// ---- ranked pairing and match reports ----

#[derive(Deserialize)]
struct PairBody {
    key: String,
    seats: Vec<Seat>,
}
#[derive(Deserialize)]
struct Seat {
    slot: i64,
    account: i64,
}

fn pair(sql: &dyn Sql, now: i64, body: &str) -> Result<AccReply> {
    let Ok(p) = serde_json::from_str::<PairBody>(body) else {
        return Ok(err(400, "bad request"));
    };
    let key = p.key.as_bytes();
    let mut slots: Vec<i64> = p.seats.iter().map(|x| x.slot).collect();
    let mut accounts: Vec<i64> = p.seats.iter().map(|x| x.account).collect();
    slots.sort_unstable();
    slots.dedup();
    accounts.sort_unstable();
    accounts.dedup();
    if key.len() != 15 || key[6] != b'-' || p.seats.len() != 2 || slots.len() != 2 || accounts.len() != 2
        || slots.iter().any(|x| !(0..=3).contains(x))
    {
        return Ok(err(400, "pairing not taken"));
    }
    for id in &accounts {
        if sql.exec("SELECT id FROM accounts WHERE id = ?", vec![V::Int(*id)])?.is_empty() {
            return Ok(err(400, "pairing not taken"));
        }
    }
    sql.exec("DELETE FROM pairings WHERE at < ?", vec![V::Int(now - PAIRING_MS)])?;
    sql.exec("DELETE FROM reports WHERE at < ?", vec![V::Int(now - PAIRING_MS)])?;
    sql.exec("DELETE FROM settled WHERE at < ?", vec![V::Int(now - PAIRING_MS)])?;
    for seat in &p.seats {
        sql.exec(
            "INSERT OR REPLACE INTO pairings (key, slot, account, at) VALUES (?, ?, ?, ?)",
            vec![V::Text(p.key.clone()), V::Int(seat.slot), V::Int(seat.account), V::Int(now)],
        )?;
    }
    Ok(empty(204))
}

#[derive(Deserialize, Default)]
struct Player {
    slot: i64,
    #[serde(default)]
    fighter: i64,
    #[serde(default)]
    cpu: bool,
}

/// What a game sends after a match (the fields ranked play needs).
#[derive(Deserialize)]
struct MatchReport {
    key: String,
    #[serde(default)]
    mode: i64,
    #[serde(default)]
    venue: i64,
    you: i64,
    players: Vec<Player>,
    #[serde(default)]
    winners: Vec<i64>,
    #[serde(default)]
    draw: bool,
    #[serde(default)]
    event: String,
}

impl MatchReport {
    fn valid(&self) -> bool {
        let slots: std::collections::BTreeSet<i64> = self.players.iter().map(|p| p.slot).collect();
        (8..=80).contains(&self.key.len())
            && (2..=4).contains(&self.players.len())
            && slots.len() == self.players.len()
            && slots.iter().all(|x| (0..=3).contains(x))
            && slots.contains(&self.you)
            && self.winners.iter().all(|w| slots.contains(w))
            && match self.event.as_str() {
                "start" | "crashed" => self.winners.is_empty() && !self.draw,
                "left" => self.winners.is_empty() && !self.draw,
                "" | "result" => self.draw || !self.winners.is_empty(),
                _ => false,
            }
    }

    /// What both games must agree on: mode, venue, who fought as what, and who won.
    fn outcome(&self) -> String {
        let mut players: Vec<&Player> = self.players.iter().collect();
        players.sort_by_key(|p| p.slot);
        let mut winners = self.winners.clone();
        winners.sort_unstable();
        let lineup: String = players.iter().map(|p| format!("{}:{}:{},", p.slot, p.fighter, p.cpu)).collect();
        let won: String = winners.iter().map(|w| format!("w{w}")).collect();
        format!("{}/{}/{}/{lineup}{won}", self.mode, self.venue, self.draw)
    }
}

/// The pairing a report key belongs to: the key without its match number.
fn pairing_key_of(key: &str) -> Option<&str> {
    let b = key.as_bytes();
    (b.len() >= 15 && b[6] == b'-' && (b.len() == 15 || b[15] == b'-')).then(|| &key[..15])
}

fn report(sql: &dyn Sql, now: i64, account: i64, body: &str) -> Result<AccReply> {
    let Ok(rep) = serde_json::from_str::<MatchReport>(body) else {
        return Ok(err(400, "report not taken"));
    };
    if !rep.valid() {
        return Ok(err(400, "report not taken"));
    }
    let accepted = json_reply(202, json!({ "status": "counted once the other players' games agree" }));
    // Only ranked results matter here: starts, leaves and crashes, and games nobody paired, are taken and ignored.
    let Some(pk) = pairing_key_of(&rep.key) else { return Ok(accepted) };
    if !matches!(rep.event.as_str(), "" | "result") {
        return Ok(accepted);
    }
    let seats = sql.exec("SELECT slot, account FROM pairings WHERE key = ?", vec![V::Text(pk.to_owned())])?;
    let seat_of = |slot: i64| seats.iter().find(|r| i(r, "slot") == slot).map(|r| i(r, "account"));
    if seats.len() != 2 || seat_of(rep.you) != Some(account) {
        return Ok(accepted);
    }
    if rep.players.len() != 2 || rep.players.iter().any(|p| p.cpu || seat_of(p.slot).is_none()) {
        return Ok(accepted);
    }
    sql.exec(
        "INSERT OR REPLACE INTO reports (key, account, you, lineup, winners, draw, at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        vec![
            V::Text(rep.key.clone()),
            V::Int(account),
            V::Int(rep.you),
            V::Text(rep.outcome()),
            V::Text(serde_json::to_string(&rep.winners)?),
            V::Int(rep.draw as i64),
            V::Int(now),
        ],
    )?;
    let both = sql.exec("SELECT * FROM reports WHERE key = ?", vec![V::Text(rep.key.clone())])?;
    let done = !sql.exec("SELECT key FROM settled WHERE key = ?", vec![V::Text(rep.key.clone())])?.is_empty();
    if both.len() == 2 && !done {
        sql.exec("INSERT INTO settled (key, at) VALUES (?, ?)", vec![V::Text(rep.key.clone()), V::Int(now)])?;
        if s(&both[0], "lineup") == s(&both[1], "lineup") {
            // Score for the first reporter: a draw, or a win when its own slot won.
            let a = &both[0];
            let winners: Vec<i64> = serde_json::from_str(&s(a, "winners"))?;
            let score_a = if i(a, "draw") == 1 {
                0.5
            } else if winners.contains(&i(a, "you")) {
                1.0
            } else {
                0.0
            };
            settle(sql, i(&both[0], "account"), i(&both[1], "account"), score_a)?;
        }
    }
    Ok(accepted)
}

fn rating_of(r: &Row) -> Rating {
    Rating { rating: f(r, "rating"), deviation: f(r, "dev"), volatility: f(r, "vol") }
}

/// Move both ratings after one agreed game (each game its own rating period).
fn settle(sql: &dyn Sql, a: i64, b: i64, score_a: f64) -> Result<()> {
    let get = |id: i64| -> Result<Row> {
        sql.exec("SELECT * FROM accounts WHERE id = ?", vec![V::Int(id)])?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("unknown account {id}"))
    };
    let (ra, rb) = (get(a)?, get(b)?);
    let (na, nb) = Rating::game(rating_of(&ra), rating_of(&rb), score_a);
    for (id, r, score) in [(a, na, score_a), (b, nb, 1.0 - score_a)] {
        sql.exec(
            "UPDATE accounts SET rating = ?, dev = ?, vol = ?, games = games + 1, \
             wins = wins + ?, losses = losses + ?, draws = draws + ? WHERE id = ?",
            vec![
                V::Real(r.rating),
                V::Real(r.deviation),
                V::Real(r.volatility),
                V::Int((score == 1.0) as i64),
                V::Int((score == 0.0) as i64),
                V::Int((score == 0.5) as i64),
                V::Int(id),
            ],
        )?;
    }
    Ok(())
}

// ---- small helpers ----

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(hex(&buf))
}

fn login_page(login: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'>\
         <title>FFNY dev sign-in</title><body style='font:16px sans-serif;max-width:28em;margin:3em auto'>\
         <h1>FFNY dev sign-in</h1><p>Test server: pick a name, no password. This is not Discord.</p>\
         <form method=post action=/auth/dev/complete>\
         <input type=hidden name=login value={login}>\
         <input name=name maxlength=24 required autofocus placeholder='Your name'> <button>Sign in</button></form>"
    )
}

fn done_page() -> String {
    "<!doctype html><meta charset=utf-8><title>Signed in</title>\
     <body style='font:16px sans-serif;max-width:28em;margin:3em auto'><h1>Signed in</h1>\
     <p>Go back to the game.</p>"
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_key_strips_the_match_number() {
        assert_eq!(pairing_key_of("ABC123-1A2B3C4D"), Some("ABC123-1A2B3C4D"));
        assert_eq!(pairing_key_of("ABC123-1A2B3C4D-2"), Some("ABC123-1A2B3C4D"));
        assert_eq!(pairing_key_of("ABC123-1A2B3C4DX"), None);
        assert_eq!(pairing_key_of("short"), None);
    }
}
