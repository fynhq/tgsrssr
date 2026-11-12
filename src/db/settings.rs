//! Settings key-value store and stale-key cleanup.

use super::*;
use sqlx::Row;


pub async fn get_setting(db: &Db, key: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
}

pub async fn set_setting(db: &Db, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
           ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')"#,
    )
    .bind(key)
    .bind(value)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_setting(db: &Db, key: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM settings WHERE key = ?")
        .bind(key)
        .execute(db)
        .await?;
    Ok(())
}

/// Atomically flip a state bit in settings: writes `to` only when the current
/// value != `to`. Returns true when this call performed the flip (the caller
/// sends an edge notification based on it). Used for platform health state
/// (ok/down): when multiple sources fail concurrently only the first flip
/// succeeds, eliminating duplicate notifications (a read-check-write sequence
/// has a race window under concurrency).
pub async fn flip_setting_once(db: &Db, key: &str, to: &str) -> anyhow::Result<bool> {
    let res = sqlx::query(
        r#"INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
           ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')
           WHERE settings.value != excluded.value"#,
    )
    .bind(key)
    .bind(to)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Get all settings whose key starts with the given prefix.
/// Returns a Result: a transient DB error must not be treated as "no data"
/// (consistent with the project's fail-closed principle); the caller decides
/// explicitly (startup cleanup / cookie-loading scenarios log a warn and skip).
pub async fn get_settings_by_prefix(db: &Db, prefix: &str) -> anyhow::Result<Vec<(String, String)>> {
    // `instr` instead of LIKE: key prefixes such as `platform_health:` contain
    // `_`, which LIKE treats as a single-character wildcard.
    let rows = sqlx::query("SELECT key, value FROM settings WHERE instr(key, ?) = 1")
        .bind(prefix)
        .fetch_all(db)
        .await?;
    Ok(rows.iter()
        .map(|r| (r.get::<String, _>("key"), r.get::<String, _>("value")))
        .collect())
}

/// Get the owner (added_by) of a chat.
/// Ok(None) = chat does not exist or has no owner; Err = DB query failed
/// (callers must log it and must not silently treat it as "no owner" —
/// otherwise owner notifications would be silently lost).
pub async fn get_chat_owner(db: &Db, chat_id: &str) -> anyhow::Result<Option<i64>> {
    Ok(get_chat(db, chat_id)
        .await?
        .and_then(|c| c.added_by)
        .and_then(|s| s.parse::<i64>().ok()))
}

