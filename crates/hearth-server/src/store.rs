//! Shared SQLite plumbing for the room and service runtimes: the fixed table
//! definitions, small timer helpers and the state-snapshot blob convention.
//!
//! Every runtime owns one Durable Object's storage and creates only its own
//! tables, so the DDL here is the whole fixed schema.

use hearth_protocol::Value;

use crate::host::{Sql, SqlValue};
use crate::Result;

pub(crate) const CREATE_STATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS hearth_state (\
    id INTEGER PRIMARY KEY CHECK (id = 0), \
    seq INTEGER NOT NULL, \
    locked INTEGER NOT NULL, \
    data BLOB NOT NULL)";
pub(crate) const CREATE_TIMERS_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS hearth_timers (key TEXT PRIMARY KEY, due_ms INTEGER NOT NULL)";
pub(crate) const CREATE_HELD_TABLE: &str = "CREATE TABLE IF NOT EXISTS hearth_held (\
    player_id TEXT PRIMARY KEY, seat INTEGER NOT NULL, until_ms INTEGER NOT NULL, \
    provider TEXT, display_name TEXT)";
pub(crate) const CREATE_META_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS hearth_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)";

/// Add the v0.3b identity columns to a `hearth_held` table an older build
/// created. SQLite has no `ADD COLUMN IF NOT EXISTS`, so read the current
/// columns from `pragma_table_info` and run only the `ALTER TABLE`s that are
/// actually missing. Old rows keep `NULL` in both columns, which reads as the
/// anonymous provider.
pub(crate) fn ensure_held_columns(sql: &dyn Sql) -> Result<()> {
    let rows = sql.exec(
        "SELECT name FROM pragma_table_info('hearth_held')",
        Vec::new(),
    )?;
    let has = |column: &str| {
        rows.iter()
            .any(|row| row.get("name").and_then(Value::as_str) == Some(column))
    };
    if !has("provider") {
        sql.exec(
            "ALTER TABLE hearth_held ADD COLUMN provider TEXT",
            Vec::new(),
        )?;
    }
    if !has("display_name") {
        sql.exec(
            "ALTER TABLE hearth_held ADD COLUMN display_name TEXT",
            Vec::new(),
        )?;
    }
    Ok(())
}

/// Whether a table exists, without creating it.
pub(crate) fn has_table(sql: &dyn Sql, name: &str) -> Result<bool> {
    let rows = sql.exec(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
        vec![SqlValue::Text(name.to_owned())],
    )?;
    Ok(!rows.is_empty())
}

pub(crate) fn timer_set(sql: &dyn Sql, key: &str, due_ms: u64) -> Result<()> {
    sql.exec(
        "INSERT OR REPLACE INTO hearth_timers (key, due_ms) VALUES (?, ?)",
        vec![SqlValue::Text(key.to_owned()), SqlValue::Int(due_ms as i64)],
    )?;
    Ok(())
}

pub(crate) fn timer_delete(sql: &dyn Sql, key: &str) -> Result<()> {
    sql.exec(
        "DELETE FROM hearth_timers WHERE key = ?",
        vec![SqlValue::Text(key.to_owned())],
    )?;
    Ok(())
}

/// The earliest scheduled timer, if any.
pub(crate) fn next_timer(sql: &dyn Sql) -> Result<Option<u64>> {
    let rows = sql.exec("SELECT MIN(due_ms) AS next FROM hearth_timers", Vec::new())?;
    Ok(rows
        .first()
        .and_then(|row| row.get("next"))
        .and_then(Value::as_u64))
}

/// Read one `hearth_meta` value.
pub(crate) fn meta_get(sql: &dyn Sql, key: &str) -> Result<Option<String>> {
    let rows = sql.exec(
        "SELECT value FROM hearth_meta WHERE key = ?",
        vec![SqlValue::Text(key.to_owned())],
    )?;
    Ok(rows
        .first()
        .and_then(|row| row.get("value"))
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// Overwrite one `hearth_meta` value.
pub(crate) fn meta_put(sql: &dyn Sql, key: &str, value: &str) -> Result<()> {
    sql.exec(
        "INSERT OR REPLACE INTO hearth_meta (key, value) VALUES (?, ?)",
        vec![
            SqlValue::Text(key.to_owned()),
            SqlValue::Text(value.to_owned()),
        ],
    )?;
    Ok(())
}

/// Write one `hearth_meta` value without overwriting a stored one.
pub(crate) fn meta_put_if_absent(sql: &dyn Sql, key: &str, value: &str) -> Result<()> {
    sql.exec(
        "INSERT OR IGNORE INTO hearth_meta (key, value) VALUES (?, ?)",
        vec![
            SqlValue::Text(key.to_owned()),
            SqlValue::Text(value.to_owned()),
        ],
    )?;
    Ok(())
}

/// Blob columns surface as JSON arrays of byte values through the [`Sql`]
/// trait, which keeps that interface JSON-shaped while still round-tripping
/// the runtime's state snapshot.
pub(crate) fn json_to_bytes(value: Option<&Value>) -> Result<Vec<u8>> {
    let Some(Value::Array(items)) = value else {
        anyhow::bail!("hearth_state.data is not a byte array");
    };
    let mut bytes = Vec::with_capacity(items.len());
    for item in items {
        let Some(byte) = item.as_u64().and_then(|value| u8::try_from(value).ok()) else {
            anyhow::bail!("hearth_state.data contains a non-byte value");
        };
        bytes.push(byte);
    }
    Ok(bytes)
}
