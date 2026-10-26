//! The durable outbox and inbox: `ctx.notify` and the runtime's own room
//! reports.
//!
//! A notify is written to `hearth_outbox` in the same synchronous block as the
//! state save, so a committed handler can never lose it. Delivery runs after the
//! pass and on the internal `h:outbox` timer, retries with capped backoff, keeps
//! order per target, and drops rows older than 24 hours (logged) so the table
//! stays bounded.
//!
//! The receiver deduplicates `Envelope::msg_id` through `hearth_inbox`, which
//! gives a notify exactly-once application per 24-hour window even when the
//! sender retries after an ambiguous failure.

use std::collections::HashSet;

use hearth_mp_protocol::Value;

use crate::call::{CallTransport, Envelope, Target};
use crate::host::{Sql, SqlValue};
use crate::store;
use crate::Result;

pub(crate) const CREATE_OUTBOX_TABLE: &str = "CREATE TABLE IF NOT EXISTS hearth_outbox (\
    id INTEGER PRIMARY KEY AUTOINCREMENT, \
    binding TEXT NOT NULL, \
    name TEXT NOT NULL, \
    body TEXT NOT NULL, \
    attempts INTEGER NOT NULL, \
    next_ms INTEGER NOT NULL, \
    first_ms INTEGER NOT NULL)";
pub(crate) const CREATE_INBOX_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS hearth_inbox (msg_id TEXT PRIMARY KEY, at_ms INTEGER NOT NULL)";

/// The internal timer that retries pending rows.
pub(crate) const OUTBOX_TIMER: &str = "h:outbox";

/// Rows older than this are dropped and logged: a 24-hour guarantee, not
/// forever.
pub(crate) const RETENTION_MS: u64 = 86_400_000;
const MAX_BACKOFF_MS: u64 = 300_000;

/// One pending outbox row.
struct Row {
    id: i64,
    binding: String,
    name: String,
    body: Value,
    attempts: u32,
    next_ms: u64,
    first_ms: u64,
}

/// A row that expired before it could be delivered.
pub(crate) struct Dropped {
    pub binding: String,
    pub name: String,
    pub first_ms: u64,
}

pub(crate) fn create(sql: &dyn Sql) -> Result<()> {
    sql.exec(CREATE_OUTBOX_TABLE, Vec::new())?;
    sql.exec(CREATE_INBOX_TABLE, Vec::new())?;
    Ok(())
}

/// Buffer one durable call. It fires after the current handler commits.
pub(crate) fn enqueue(
    sql: &dyn Sql,
    to_binding: &str,
    to_name: &str,
    body: &Value,
    now: u64,
) -> Result<()> {
    sql.exec(
        "INSERT INTO hearth_outbox (binding, name, body, attempts, next_ms, first_ms) \
         VALUES (?, ?, ?, 0, ?, ?)",
        vec![
            SqlValue::Text(to_binding.to_owned()),
            SqlValue::Text(to_name.to_owned()),
            SqlValue::Text(serde_json::to_string(body)?),
            SqlValue::Int(now as i64),
            SqlValue::Int(now as i64),
        ],
    )?;
    Ok(())
}

/// Whether the receiver already applied this message.
pub(crate) fn seen(sql: &dyn Sql, msg_id: &str) -> Result<bool> {
    let rows = sql.exec(
        "SELECT msg_id FROM hearth_inbox WHERE msg_id = ?",
        vec![SqlValue::Text(msg_id.to_owned())],
    )?;
    Ok(!rows.is_empty())
}

/// Record one applied message and prune entries past the retention window.
pub(crate) fn remember(sql: &dyn Sql, msg_id: &str, now: u64) -> Result<()> {
    sql.exec(
        "INSERT OR REPLACE INTO hearth_inbox (msg_id, at_ms) VALUES (?, ?)",
        vec![SqlValue::Text(msg_id.to_owned()), SqlValue::Int(now as i64)],
    )?;
    prune_inbox(sql, now)
}

fn prune_inbox(sql: &dyn Sql, now: u64) -> Result<()> {
    let cutoff = now.saturating_sub(RETENTION_MS);
    sql.exec(
        "DELETE FROM hearth_inbox WHERE at_ms < ?",
        vec![SqlValue::Int(cutoff as i64)],
    )?;
    Ok(())
}

/// Deliver every pending row: in id order, stopping each target at its first
/// failure so per-target order holds. Returns the rows dropped after 24 hours.
pub(crate) async fn deliver(
    sql: &dyn Sql,
    me: &Target,
    now: u64,
    transport: &dyn CallTransport,
) -> Result<Vec<Dropped>> {
    let rows = pending(sql)?;
    let mut blocked: HashSet<(String, String)> = HashSet::new();
    let mut dropped = Vec::new();
    let mut next_due: Option<u64> = None;
    for row in rows {
        let target = (row.binding.clone(), row.name.clone());
        if now.saturating_sub(row.first_ms) > RETENTION_MS {
            delete(sql, row.id)?;
            dropped.push(Dropped {
                binding: row.binding,
                name: row.name,
                first_ms: row.first_ms,
            });
            continue;
        }
        if blocked.contains(&target) {
            // This target's head row failed (or is waiting): later rows for the
            // same target wait too, and do not schedule a retry of their own.
            continue;
        }
        if row.next_ms > now {
            blocked.insert(target);
            next_due = Some(next_due.map_or(row.next_ms, |due| due.min(row.next_ms)));
            continue;
        }
        let envelope = Envelope {
            msg_id: Some(msg_id(me, row.id, row.first_ms)),
            from: me.clone(),
            chain: Vec::new(),
            rpc: row.body,
        };
        let to = Target::new(row.binding.clone(), row.name.clone());
        match transport.call(to, envelope).await {
            Ok(_) => {
                delete(sql, row.id)?;
            }
            Err(_err) => {
                let attempts = row.attempts.saturating_add(1);
                let next_ms = now.saturating_add(backoff_ms(attempts));
                sql.exec(
                    "UPDATE hearth_outbox SET attempts = ?, next_ms = ? WHERE id = ?",
                    vec![
                        SqlValue::Int(i64::from(attempts)),
                        SqlValue::Int(next_ms as i64),
                        SqlValue::Int(row.id),
                    ],
                )?;
                blocked.insert(target);
                next_due = Some(next_due.map_or(next_ms, |due| due.min(next_ms)));
            }
        }
    }
    match next_due {
        Some(at) => store::timer_set(sql, OUTBOX_TIMER, at)?,
        None => store::timer_delete(sql, OUTBOX_TIMER)?,
    }
    Ok(dropped)
}

/// The message id a receiver deduplicates on. Unique per sender because `id`
/// is the sending object's autoincrementing row id.
fn msg_id(me: &Target, id: i64, first_ms: u64) -> String {
    format!("{first_ms:013}-{id:06}-{}/{}", me.binding, me.name)
}

fn backoff_ms(attempts: u32) -> u64 {
    // Exponential, reaching the 5 minute cap at the ninth retry.
    (1000u64 << attempts.min(20)).min(MAX_BACKOFF_MS)
}

fn delete(sql: &dyn Sql, id: i64) -> Result<()> {
    sql.exec(
        "DELETE FROM hearth_outbox WHERE id = ?",
        vec![SqlValue::Int(id)],
    )?;
    Ok(())
}

fn pending(sql: &dyn Sql) -> Result<Vec<Row>> {
    let rows = sql.exec(
        "SELECT id, binding, name, body, attempts, next_ms, first_ms \
         FROM hearth_outbox ORDER BY id",
        Vec::new(),
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let (Some(id), Some(binding), Some(name), Some(body), Some(first_ms)) = (
            row.get("id").and_then(Value::as_i64),
            row.get("binding").and_then(Value::as_str),
            row.get("name").and_then(Value::as_str),
            row.get("body").and_then(Value::as_str),
            row.get("first_ms").and_then(Value::as_u64),
        ) else {
            continue;
        };
        out.push(Row {
            id,
            binding: binding.to_owned(),
            name: name.to_owned(),
            body: serde_json::from_str(body).unwrap_or(Value::Null),
            attempts: row.get("attempts").and_then(Value::as_u64).unwrap_or(0) as u32,
            next_ms: row
                .get("next_ms")
                .and_then(Value::as_u64)
                .unwrap_or(now_hint()),
            first_ms,
        });
    }
    Ok(out)
}

/// A missing `next_ms` never happens for a row this module wrote; treating it as
/// zero would retry immediately, which is the safe direction.
fn now_hint() -> u64 {
    0
}

/// The reply a receiver sends for a duplicate delivery: success without running
/// the handler again.
pub(crate) fn duplicate_reply() -> Value {
    Value::Null
}
