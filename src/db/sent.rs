//! Sent-entry markers, sent-message registry and file_id cache.

use super::*;
use sqlx::Row;

pub async fn is_entry_sent(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
) -> anyhow::Result<bool> {
    let row = sqlx::query(
        "SELECT 1 FROM sent_entries WHERE chat_id = ? AND feed_url = ? AND entry_id = ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .fetch_optional(db)
    .await?;
    Ok(row.is_some())
}

pub async fn mark_entry_sent(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO sent_entries (chat_id, feed_url, entry_id) VALUES (?, ?, ?)",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .execute(db)
    .await?;
    Ok(())
}

// ===== Cleanup =====

pub async fn cleanup_sent_entries(db: &Db, retention_days: i64) -> anyhow::Result<u64> {
    // Time is not a safe criterion for an *existing* subscription: an entry
    // can stay inside a source's rolling window (or reappear on the next full
    // fetch) long after the retention period. Deleting its sent marker then
    // makes the bot re-push an old entry — for X feeds without conditional
    // requests this can re-post the whole visible window every retention
    // cycle. Keep rows for any subscription that still exists (active,
    // paused, or auto-disabled and waiting for self-heal); only clean up
    // true orphans whose subscription was removed. `/sub` marks the current
    // window as sent on re-subscribe, so keeping these rows cannot double
    // push either.
    let result = sqlx::query(
        r#"DELETE FROM sent_entries
           WHERE sent_at < datetime('now', ?)
             AND NOT EXISTS (
                 SELECT 1 FROM subscriptions s
                 WHERE s.chat_id = sent_entries.chat_id
                   AND s.feed_url = sent_entries.feed_url
             )"#,
    )
    .bind(format!("-{} days", retention_days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

// ===== File-ID cache (upload dedup) =====

/// Cache lookup: whether a reusable Telegram file_id already exists for the
/// same content hash.
pub async fn get_cached_file_id(db: &Db, content_hash: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT file_id FROM file_id_cache WHERE content_hash = ?")
        .bind(content_hash)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("file_id")))
}

/// Cache write: record the file_id after a successful media upload
/// (refreshes the timestamp on conflict).
pub async fn put_cached_file_id(db: &Db, content_hash: &str, file_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO file_id_cache (content_hash, file_id)
           VALUES (?1, ?2)
           ON CONFLICT(content_hash) DO UPDATE SET
             file_id = excluded.file_id,
             created_at = datetime('now')"#,
    )
    .bind(content_hash)
    .bind(file_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Cache invalidation: delete the entry when sending by file_id is rejected by
/// Telegram (bot changed / file cleaned up); the next attempt automatically
/// falls back to re-uploading.
pub async fn invalidate_cached_file_id(db: &Db, content_hash: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM file_id_cache WHERE content_hash = ?")
        .bind(content_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Clean up expired cache entries (retention in days); returns the number of
/// deleted rows.
pub async fn cleanup_file_id_cache(db: &Db, retention_days: i64) -> anyhow::Result<u64> {
    let result = sqlx::query("DELETE FROM file_id_cache WHERE created_at < datetime('now', ?)")
        .bind(format!("-{} days", retention_days))
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// Clean up stale keys with the given prefix in the settings table
/// (updated_at older than the retention period). Used for keys that can linger
/// past an entry's lifecycle (e.g. media_dl_fail: when an entry rolls out of
/// the source's sliding window before reaching the retry cap, neither the
/// download-success nor the abandon deletion point will ever fire again).
/// Prefix matching uses instr instead of LIKE (feed_url may contain the % / _
/// wildcard characters).
pub async fn cleanup_stale_settings(
    db: &Db,
    prefix: &str,
    retention_days: i64,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM settings WHERE instr(key, ?) = 1 AND updated_at < datetime('now', ?)",
    )
    .bind(prefix)
    .bind(format!("-{} days", retention_days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

// ===== Sent messages registry (/find reverse lookup) =====

/// Register one sent message: TG message → RSS feed entry. Row-count
/// trimming is done by the periodic cleanup job, not on every insert: the
/// previous COUNT(*) ran once per delivered message.
pub async fn record_sent_message(
    db: &Db,
    chat_id: &str,
    message_id: i64,
    feed_url: &str,
    entry_id: &str,
    entry_link: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO sent_messages (chat_id, message_id, feed_url, entry_id, entry_link)
           VALUES (?1, ?2, ?3, ?4, ?5)
           ON CONFLICT(chat_id, message_id) DO UPDATE SET
             feed_url = excluded.feed_url,
             entry_id = excluded.entry_id,
             entry_link = excluded.entry_link,
             created_at = datetime('now')"#,
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(feed_url)
    .bind(entry_id)
    .bind(entry_link)
    .execute(db)
    .await?;
    Ok(())
}

/// Enforce the sent-messages row cap (called from the periodic cleanup job).
/// Returns the number of rows removed.
pub async fn trim_sent_messages(db: &Db, max_rows: i64) -> anyhow::Result<u64> {
    if max_rows <= 0 {
        return Ok(0);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sent_messages")
        .fetch_one(db)
        .await?;
    if count <= max_rows {
        return Ok(0);
    }
    let result = sqlx::query(
        r#"DELETE FROM sent_messages
           WHERE id NOT IN (SELECT id FROM sent_messages ORDER BY id DESC LIMIT ?)"#,
    )
    .bind(max_rows)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// /find reverse lookup: find the RSS entry for a channel + message id.
pub struct SentMessageRow {
    pub feed_url: String,
    pub entry_id: String,
    pub entry_link: Option<String>,
    pub created_at: String,
}

pub async fn find_sent_message(
    db: &Db,
    chat_id: &str,
    message_id: i64,
) -> anyhow::Result<Option<SentMessageRow>> {
    let row = sqlx::query(
        "SELECT feed_url, entry_id, entry_link, created_at FROM sent_messages WHERE chat_id = ? AND message_id = ?",
    )
    .bind(chat_id)
    .bind(message_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| SentMessageRow {
        feed_url: r.get("feed_url"),
        entry_id: r.get("entry_id"),
        entry_link: r.get("entry_link"),
        created_at: r.get("created_at"),
    }))
}

/// Bounded sent-entry snapshot for platform enrichment.
///
/// `sent_entries` is never pruned while a subscription exists, so loading the
/// full history for Weibo detail/vote suppression grows with the source's age.
/// The snapshot is only a hint: a missing old entry costs one extra detail
/// request and is still filtered by the exact window query below.
pub const SENT_ENTRY_LOOKBACK_DAYS: i64 = 180;
/// Row cap for the bounded snapshot (most recent first).
pub const SENT_ENTRY_LOOKBACK_ROWS: i64 = 5000;

/// Recent sent-entry ids for one subscription (bounded by age and row count).
pub async fn sent_entry_ids_recent(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    retention_days: i64,
    limit: i64,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let rows = sqlx::query(
        "SELECT entry_id FROM sent_entries
          WHERE chat_id = ? AND feed_url = ? AND sent_at >= datetime('now', ?)
          ORDER BY sent_at DESC LIMIT ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(format!("-{} days", retention_days))
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| r.get("entry_id")).collect())
}

/// Sent-entry ids from the current feed window (chunked `IN` query).
///
/// Uses the `UNIQUE(chat_id, feed_url, entry_id)` index so only matching rows
/// are read; the table's full history is never scanned. Chunking keeps the
/// bound count below SQLite's variable limit.
pub async fn sent_entry_ids_in_window(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    ids: &[String],
) -> anyhow::Result<std::collections::HashSet<String>> {
    const CHUNK: usize = 300;
    let mut found = std::collections::HashSet::new();
    for chunk in ids.chunks(CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT entry_id FROM sent_entries
              WHERE chat_id = ? AND feed_url = ? AND entry_id IN ({placeholders})"
        );
        let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
        query = query.bind(chat_id).bind(feed_url);
        for id in chunk {
            query = query.bind(id);
        }
        found.extend(query.fetch_all(db).await?);
    }
    Ok(found)
}
